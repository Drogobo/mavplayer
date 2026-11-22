//! Codec opening (with optional hardware acceleration) and the generic
//! packet → frame decode loop shared by the audio and video threads.

use super::av::{self, CodecCtx, Frame, Packet, ff};
use super::filters::Graph;
use super::sync::PacketQueue;
use super::types::{HwDecode, ScaleQuality};
use std::os::raw::c_int;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct HwInfo {
    pub pix_fmt: c_int,
    pub name: String,
}

pub struct OpenedCodec {
    pub ctx: CodecCtx,
    pub hw: Option<HwInfo>,
    pub decoder_name: String,
    reopen: Option<Reopen>,
    /// Opened with one thread for the hardware decoder (see `auto_video_threads`).
    lean: bool,
}

/// What it takes to open a decoder again after `Decoder::park`.
struct Reopen {
    par: *mut ff::AVCodecParameters,
    time_base: ff::AVRational,
    threads: u32,
    hw_mode: Option<HwDecode>,
}

// SAFETY: a private copy of the stream's parameters, owned by this value.
unsafe impl Send for Reopen {}

impl Drop for Reopen {
    fn drop(&mut self) {
        unsafe { ff::avcodec_parameters_free(&mut self.par) };
    }
}

/// How long a decoder that has reached the end of the file waits for a seek
/// before it lets go of its codec (and with it the GPU device and picture
/// pools: about 100 MB with CUDA).
const PARK_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

/// Device types to try, best first, for the current platform.
fn hw_device_types() -> &'static [ff::AVHWDeviceType] {
    use ff::AVHWDeviceType::*;
    #[cfg(target_os = "linux")]
    {
        // On NVIDIA, VAAPI only reaches the GPU through the NVDEC wrapper
        // driver, which takes twice as long as CUDA to open (about 450 ms
        // against 200 ms on an RTX 2070) and decodes the same way.
        static TYPES: std::sync::OnceLock<&'static [ff::AVHWDeviceType]> = std::sync::OnceLock::new();
        TYPES.get_or_init(|| {
            if vaapi_is_nvdec() {
                &[AV_HWDEVICE_TYPE_CUDA, AV_HWDEVICE_TYPE_VAAPI, AV_HWDEVICE_TYPE_VDPAU]
            } else {
                &[AV_HWDEVICE_TYPE_VAAPI, AV_HWDEVICE_TYPE_CUDA, AV_HWDEVICE_TYPE_VDPAU]
            }
        })
    }
    #[cfg(target_os = "windows")]
    {
        &[AV_HWDEVICE_TYPE_D3D11VA, AV_HWDEVICE_TYPE_DXVA2, AV_HWDEVICE_TYPE_CUDA]
    }
    #[cfg(target_os = "macos")]
    {
        &[AV_HWDEVICE_TYPE_VIDEOTOOLBOX]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        &[]
    }
}

/// Whether FFmpeg's default VAAPI device would be NVIDIA's NVDEC wrapper:
/// libva is told to use it, or the first render node (the one FFmpeg opens)
/// is an NVIDIA GPU. A laptop whose first node is its Intel or AMD GPU keeps
/// VAAPI first, so the discrete GPU isn't woken up.
#[cfg(target_os = "linux")]
fn vaapi_is_nvdec() -> bool {
    if let Some(name) = std::env::var_os("LIBVA_DRIVER_NAME") {
        return name == "nvidia";
    }
    std::fs::read_to_string("/sys/class/drm/renderD128/device/vendor").is_ok_and(|v| v.trim() == "0x10de")
}

/// A hardware device context, shared by every decoder that uses it.
struct HwDevice(*mut ff::AVBufferRef);
// SAFETY: FFmpeg device contexts are reference counted and may be used from
// any thread once created; decoders only take new references to them.
unsafe impl Send for HwDevice {}

impl Drop for HwDevice {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_unref(&mut self.0) };
    }
}

enum HwSlot {
    Untried,
    Unavailable,
    /// The device, and when a decoder last took it.
    Ready(HwDevice, std::time::Instant),
}

/// Hardware devices, created once per process instead of once per file:
/// opening one takes 200–500 ms on some drivers, which every file (and
/// every playlist step) would otherwise wait for. A device that fails to
/// open isn't tried again.
static HW_SLOTS: parking_lot::Mutex<Vec<(ff::AVHWDeviceType, Arc<parking_lot::Mutex<HwSlot>>)>> =
    parking_lot::Mutex::new(Vec::new());

fn hw_slot(ty: ff::AVHWDeviceType) -> Arc<parking_lot::Mutex<HwSlot>> {
    let mut slots = HW_SLOTS.lock();
    if let Some((_, s)) = slots.iter().find(|(t, _)| *t == ty) {
        return s.clone();
    }
    let s = Arc::new(parking_lot::Mutex::new(HwSlot::Untried));
    slots.push((ty, s.clone()));
    s
}

/// A new reference to the shared device of type `ty`, creating it on first
/// use. Callers that arrive while it is being created wait for it.
fn hw_device(ty: ff::AVHWDeviceType) -> Option<*mut ff::AVBufferRef> {
    let slot = hw_slot(ty);
    let mut slot = slot.lock();
    if let HwSlot::Untried = *slot {
        let t0 = std::time::Instant::now();
        let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
        let r = unsafe { ff::av_hwdevice_ctx_create(&mut dev, ty, ptr::null(), ptr::null_mut(), 0) };
        *slot = if r < 0 || dev.is_null() {
            log::debug!("hw device {:?} unavailable: {}", ty, av::err_str(r));
            HwSlot::Unavailable
        } else {
            #[cfg(target_os = "linux")]
            if ty == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA {
                unsafe { trim_cuda_context(dev) };
            }
            log::debug!("hw device {:?} ready in {:.0} ms", ty, t0.elapsed().as_secs_f64() * 1000.0);
            HwSlot::Ready(HwDevice(dev), std::time::Instant::now())
        };
    }
    match &mut *slot {
        HwSlot::Ready(d, used) => {
            *used = std::time::Instant::now();
            let r = unsafe { ff::av_buffer_ref(d.0) };
            (!r.is_null()).then_some(r)
        }
        _ => None,
    }
}

/// Shrinks the per-thread stack of a new CUDA context from 1 KB to 256
/// bytes. CUDA reserves local memory for that stack for every thread the GPU
/// can run at once, 30 of a context's 102 MB of VRAM on an RTX 2070; NVDEC
/// is fixed-function hardware and FFmpeg's scaling kernels need no stack.
/// (A kernel that needs more gets it: the driver grows local memory to fit
/// at launch.)
#[cfg(target_os = "linux")]
unsafe fn trim_cuda_context(dev: *mut ff::AVBufferRef) {
    type Push = unsafe extern "C" fn(ctx: ff::CUcontext) -> c_int;
    type Pop = unsafe extern "C" fn(ctx: *mut ff::CUcontext) -> c_int;
    type SetLimit = unsafe extern "C" fn(limit: c_int, value: usize) -> c_int;
    const CU_LIMIT_STACK_SIZE: c_int = 0;
    unsafe {
        // FFmpeg has loaded the driver library already.
        let lib = libc::dlopen(c"libcuda.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        if lib.is_null() {
            return;
        }
        let push = libc::dlsym(lib, c"cuCtxPushCurrent_v2".as_ptr());
        let pop = libc::dlsym(lib, c"cuCtxPopCurrent_v2".as_ptr());
        let set = libc::dlsym(lib, c"cuCtxSetLimit".as_ptr());
        if !push.is_null() && !pop.is_null() && !set.is_null() {
            let push = std::mem::transmute::<*mut libc::c_void, Push>(push);
            let pop = std::mem::transmute::<*mut libc::c_void, Pop>(pop);
            let set = std::mem::transmute::<*mut libc::c_void, SetLimit>(set);
            let hw = (*dev).data as *const ff::AVHWDeviceContext;
            let cu = (*hw).hwctx as *const ff::AVCUDADeviceContext;
            if push((*cu).cuda_ctx) == 0 {
                let r = set(CU_LIMIT_STACK_SIZE, 256);
                let mut popped = ptr::null_mut();
                pop(&mut popped);
                if r != 0 {
                    log::debug!("cuCtxSetLimit failed: {r}");
                }
            }
        }
        libc::dlclose(lib);
    }
}

/// Closes hardware devices that no decoder has held for `idle` (call it
/// every second or so: that is how often use is noticed). A CUDA
/// device costs about 100 MB, which a window left open after the video
/// ended shouldn't keep; the next video opens it again (100–200 ms).
pub fn release_idle_hw(idle: std::time::Duration) {
    let slots: Vec<_> = HW_SLOTS.lock().iter().map(|(_, s)| s.clone()).collect();
    for slot in slots {
        let Some(mut slot) = slot.try_lock() else { continue };
        let HwSlot::Ready(d, used) = &mut *slot else { continue };
        // Only the slot's own reference left: no decoder has it.
        if unsafe { ff::av_buffer_get_ref_count(d.0) } > 1 {
            *used = std::time::Instant::now();
        } else if used.elapsed() >= idle {
            log::debug!("closing the idle hardware decoding device");
            *slot = HwSlot::Untried;
        }
    }
}

/// Starts opening the hardware decoding device in the background, so the
/// video about to play doesn't wait as long for it (only when every video
/// decodes in hardware).
pub fn prewarm_hw(mode: HwDecode) {
    if mode != HwDecode::Always {
        return;
    }
    let spawned = std::thread::Builder::new().name("hwdec-init".into()).spawn(|| {
        for &ty in hw_device_types() {
            if let Some(mut dev) = hw_device(ty) {
                unsafe { ff::av_buffer_unref(&mut dev) };
                break;
            }
        }
    });
    if let Err(e) = spawned {
        log::debug!("could not start hwdec-init: {e}");
    }
}

/// `get_format` callback: picks the hardware format stashed in `opaque`, or
/// gracefully falls back to software decoding when it is not offered.
unsafe extern "C" fn get_hw_format(s: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    unsafe {
        let want = (*s).opaque as isize as c_int;
        let mut p = fmts;
        while *p as c_int != ff::AVPixelFormat::AV_PIX_FMT_NONE as c_int {
            if *p as c_int == want {
                return *p;
            }
            p = p.add(1);
        }
        log::info!("hardware format not offered for this stream, using software decoding");
        ff::avcodec_default_get_format(s, fmts)
    }
}

fn try_setup_hw(ctx: *mut ff::AVCodecContext, codec: *const ff::AVCodec) -> Option<HwInfo> {
    for &ty in hw_device_types() {
        let mut i = 0;
        loop {
            let cfg = unsafe { ff::avcodec_get_hw_config(codec, i) };
            if cfg.is_null() {
                break;
            }
            i += 1;
            let cfg = unsafe { &*cfg };
            if cfg.device_type != ty || cfg.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int == 0 {
                continue;
            }
            let Some(dev) = hw_device(ty) else { break };
            unsafe {
                (*ctx).hw_device_ctx = dev; // ownership moves to the codec context
                (*ctx).opaque = cfg.pix_fmt as c_int as isize as *mut _;
                (*ctx).get_format = Some(get_hw_format);
            }
            let name = unsafe { av::from_cstr(ff::av_hwdevice_get_type_name(ty)) }.unwrap_or_default();
            return Some(HwInfo { pix_fmt: cfg.pix_fmt as c_int, name });
        }
    }
    None
}

fn hw_worth_it(par: &ff::AVCodecParameters, mode: HwDecode) -> bool {
    use ff::AVCodecID::*;
    match mode {
        HwDecode::Off => false,
        HwDecode::Always => true,
        HwDecode::Auto => {
            let heavy = matches!(par.codec_id, AV_CODEC_ID_HEVC | AV_CODEC_ID_AV1 | AV_CODEC_ID_VP9 | AV_CODEC_ID_H264);
            heavy && (par.width as i64 * par.height as i64) > 1920 * 1088
        }
    }
}

/// Decoder threads for "auto". FFmpeg's own choice is one per CPU plus one,
/// and with frame threading every thread holds its own decoder state and
/// pictures in flight: 17 threads cost about 90 MB more than 4 for 1080p
/// H.264, for no gain at that size.
///
/// A hardware decoder gets one: the threads only parse headers there, and
/// each one keeps its own pool of per-picture tables that H.264 allocates
/// even when the GPU decodes (16 MB of RAM and 8-20 MB of VRAM for 1080p;
/// 4K HEVC decodes at 413 fps either way). If the GPU then turns the stream
/// down, `Decoder::decode` reopens it with software threads.
fn auto_video_threads(par: &ff::AVCodecParameters, hw: bool) -> usize {
    if hw {
        return 1;
    }
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    let big = par.width as i64 * par.height as i64 > 1920 * 1088;
    cpus.min(if big { 8 } else { 4 })
}

/// Opens a decoder for `stream`.
pub fn open_codec(
    stream: *mut ff::AVStream,
    threads: u32,
    hw_mode: Option<HwDecode>,
    sub_charset: Option<&str>,
) -> anyhow::Result<OpenedCodec> {
    unsafe {
        let mut opened = open_par((*stream).codecpar, (*stream).time_base, threads, hw_mode, sub_charset)?;
        let par = ff::avcodec_parameters_alloc();
        if !par.is_null() {
            let reopen = Reopen { par, time_base: (*stream).time_base, threads, hw_mode };
            if ff::avcodec_parameters_copy(par, (*stream).codecpar) >= 0 {
                opened.reopen = Some(reopen);
            }
        }
        Ok(opened)
    }
}

unsafe fn open_par(
    par: *const ff::AVCodecParameters,
    time_base: ff::AVRational,
    threads: u32,
    hw_mode: Option<HwDecode>,
    sub_charset: Option<&str>,
) -> anyhow::Result<OpenedCodec> {
    unsafe {
        let codec = ff::avcodec_find_decoder((*par).codec_id);
        if codec.is_null() {
            anyhow::bail!("no decoder for codec '{}'", av::codec_name((*par).codec_id));
        }
        let ctx = CodecCtx(ff::avcodec_alloc_context3(codec));
        if ctx.0.is_null() {
            anyhow::bail!("out of memory");
        }
        av::check(ff::avcodec_parameters_to_context(ctx.0, par), "avcodec_parameters_to_context")?;
        (*ctx.0).pkt_timebase = time_base;
        let mut opts = av::Dict::new();
        if (*par).codec_type == ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE
            && let Some(cs) = sub_charset
        {
            opts.set("sub_charenc", cs);
        }
        let hw = match hw_mode {
            Some(m) if hw_worth_it(&*par, m) => try_setup_hw(ctx.0, codec),
            _ => None,
        };
        let video = (*par).codec_type == ff::AVMediaType::AVMEDIA_TYPE_VIDEO;
        let lean = video && threads == 0 && hw.is_some();
        let threads_s = match threads {
            0 if video => auto_video_threads(&*par, hw.is_some()).to_string(),
            0 => "auto".to_string(),
            n => n.to_string(),
        };
        opts.set("threads", &threads_s);
        let r = ff::avcodec_open2(ctx.0, codec, &mut opts.0);
        if r < 0 {
            if hw.is_some() {
                log::warn!("hardware decoder failed to open ({}), retrying in software", av::err_str(r));
                return open_par(par, time_base, threads, None, sub_charset);
            }
            anyhow::bail!("could not open decoder: {}", av::err_str(r));
        }
        let decoder_name = av::from_cstr((*codec).name).unwrap_or_default();
        Ok(OpenedCodec { ctx, hw, decoder_name, reopen: None, lean })
    }
}

pub enum DecodeResult {
    Frame,
    /// The decoder was fully drained (end of stream for this serial).
    Eof,
    Aborted,
    /// The GPU turned the stream down and the decoder was reopened for
    /// software decoding: it needs a keyframe (seek to where playback is).
    Restart,
}

/// A picture size for [`Decoder::resize_on_gpu`] to resize hardware frames
/// to on the GPU, and the scaling filter to use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuResize {
    pub w: i32,
    pub h: i32,
    pub quality: ScaleQuality,
}

/// The GPU filter graph [`GpuResize`] runs: `scale_cuda` to NV12 at display
/// size.
///
/// Downloading the full picture and resizing it with swscale cost most of a
/// CPU core for 1080p60 in a window (bicubic 1920×1080 → 1326×746 takes
/// 6 ms a frame); on the GPU it costs nothing measurable. Its bicubic and
/// Lanczos adapt to the scale like swscale's (checked against swscale's
/// Lanczos: 42–44 dB for both bicubics, ~50 dB for its Lanczos). Output
/// stays NV12, a third of BGRA's size, for the frames waiting in the queue:
/// they stay on the GPU, and the presenter converts each to BGRA there as it
/// downloads it (`present::GpuConvert`). 10-bit pictures become 8-bit,
/// which the BGRA output is anyway. An NV12 picture that keeps its size
/// passes through untouched: NVDEC hands out copies of its surfaces, from a
/// pool that grows as needed, so the queue may hold them.
struct GpuScaler {
    graph: Option<(GpuKey, Graph)>,
    out: Frame,
    /// The graph failed for these pictures: download them whole instead.
    broken: Option<GpuKey>,
}

/// What a [`GpuScaler`] graph is built for: the frames context (device,
/// format, size) pictures come from, and the resize.
#[derive(Clone, Copy, PartialEq)]
struct GpuKey {
    frames: usize,
    resize: GpuResize,
    colorspace: c_int,
    range: c_int,
}

impl GpuScaler {
    fn new() -> Self {
        Self { graph: None, out: Frame::new(), broken: None }
    }

    /// Whether `frame` (a hardware frame) is on a device this can resize on.
    fn supports(frame: &Frame) -> bool {
        let r = frame.raw();
        if r.hw_frames_ctx.is_null() || !av::filter_available("scale_cuda") {
            return false;
        }
        unsafe {
            let fc = (*r.hw_frames_ctx).data as *const ff::AVHWFramesContext;
            let dev = (*(*fc).device_ref).data as *const ff::AVHWDeviceContext;
            (*dev).type_ == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
        }
    }

    /// Resizes `frame` in place, still on the GPU.
    fn run(&mut self, frame: &mut Frame, resize: GpuResize) -> anyhow::Result<()> {
        tag_matrix(frame);
        let r = frame.raw();
        // The YUV matrix travels with the frame for the presenter; it is
        // only part of the key because the graph's source declares it.
        let key = GpuKey {
            // Each frame has its own reference; the context is shared.
            frames: unsafe { (*r.hw_frames_ctx).data } as usize,
            resize,
            colorspace: r.colorspace as c_int,
            range: r.color_range as c_int,
        };
        if self.broken == Some(key) {
            anyhow::bail!("failed before");
        }
        if self.graph.as_ref().is_none_or(|(k, _)| *k != key) {
            self.graph = None;
            let t0 = std::time::Instant::now();
            match Self::build(frame, resize) {
                Ok(g) => {
                    log::debug!(
                        "gpu resize: {}x{} → {}x{} graph built in {:.1} ms",
                        r.width,
                        r.height,
                        resize.w,
                        resize.h,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    self.graph = Some((key, g));
                }
                Err(e) => {
                    self.broken = Some(key);
                    return Err(e);
                }
            }
        }
        let Some((_, g)) = self.graph.as_mut() else { unreachable!() };
        let pulled = g.push(frame).map(|()| g.pull(&mut self.out));
        if !matches!(pulled, Ok(true)) {
            self.graph = None;
            self.broken = Some(key);
            anyhow::bail!("no picture came out");
        }
        unsafe {
            // Square pixels at display size (the size accounts for the
            // pixel aspect already).
            (*self.out.0).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
        }
        frame.unref();
        unsafe { ff::av_frame_move_ref(frame.0, self.out.0) };
        Ok(())
    }

    fn build(frame: &Frame, resize: GpuResize) -> anyhow::Result<Graph> {
        let r = frame.raw();
        let sar = if r.sample_aspect_ratio.num > 0 { r.sample_aspect_ratio } else { ff::AVRational { num: 1, den: 1 } };
        let args = format!(
            "video_size={}x{}:pix_fmt={}:time_base=1/1000:pixel_aspect={}/{}:colorspace={}:range={}",
            r.width, r.height, r.format, sar.num, sar.den, r.colorspace as c_int, r.color_range as c_int
        );
        let chain =
            format!("scale_cuda=w={}:h={}:interp_algo={}:format=nv12", resize.w, resize.h, cuda_algo(resize.quality));
        Graph::hw(&args, r.hw_frames_ctx, &chain)
    }
}

/// A reference to the shared CUDA device if it is open already. Never opens
/// it: a CUDA device costs about 100 MB, which only a video that decodes on
/// the GPU (or the prewarm for one, `prewarm_hw`) should start paying.
fn cuda_device_if_open() -> Option<*mut ff::AVBufferRef> {
    let slot = HW_SLOTS
        .lock()
        .iter()
        .find(|(t, _)| *t == ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA)
        .map(|(_, s)| s.clone())?;
    let mut slot = slot.try_lock()?;
    let HwSlot::Ready(d, used) = &mut *slot else { return None };
    *used = std::time::Instant::now();
    let r = unsafe { ff::av_buffer_ref(d.0) };
    (!r.is_null()).then_some(r)
}

/// Resizes pictures decoded in software on the GPU: `hwupload` to the shared
/// CUDA device, then `scale_cuda` to NV12 at display size, the same picture
/// a hardware decoder's [`GpuScaler`] makes (and the presenter converts it
/// on the GPU too).
///
/// swscale took 7 ms a frame to bring 1080p AV1 (which GPUs before NVIDIA's
/// RTX 30 series can't decode) down to a 1446×814 window, 40% of a core at
/// 60 fps; uploading a picture costs a copy into the driver's staging
/// buffer. Only for pictures that are resized, or not 8-bit 4:2:0 (swscale's
/// fast paths), and only while a CUDA device is open anyway.
pub struct GpuUpload {
    graph: Option<(UploadKey, Graph)>,
    out: Frame,
    /// Pictures the graph failed for: they stay on the CPU.
    broken: Vec<UploadKey>,
}

#[derive(Clone, Copy, PartialEq)]
struct UploadKey {
    size: (c_int, c_int),
    format: c_int,
    sar: (c_int, c_int),
    resize: GpuResize,
    colorspace: c_int,
    range: c_int,
}

impl GpuUpload {
    pub fn new() -> Self {
        Self { graph: None, out: Frame::new(), broken: Vec::new() }
    }

    /// Whether [`GpuUpload::run`] is worth trying for a `frame` (in system
    /// memory) shown at `resize`.
    pub fn worth_it(frame: &Frame, resize: GpuResize) -> bool {
        use ff::AVPixelFormat::*;
        let r = frame.raw();
        let fast = r.format == AV_PIX_FMT_YUV420P as c_int
            || r.format == AV_PIX_FMT_YUVJ420P as c_int
            || r.format == AV_PIX_FMT_NV12 as c_int;
        r.hw_frames_ctx.is_null()
            && r.width as i64 * r.height as i64 >= 640 * 360
            && ((resize.w, resize.h) != (r.width, r.height) || !fast)
    }

    /// Uploads `frame` and resizes it to `resize` on the GPU, in place.
    /// False if it can't: the frame is left as it was.
    pub fn run(&mut self, frame: &mut Frame, resize: GpuResize) -> bool {
        // yuvj420p is yuv420p in full range, which CUDA doesn't know by name.
        if frame.raw().format == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as c_int {
            unsafe {
                (*frame.0).format = ff::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int;
                (*frame.0).color_range = ff::AVColorRange::AVCOL_RANGE_JPEG;
            }
        }
        tag_matrix(frame);
        let r = frame.raw();
        let sar = if r.sample_aspect_ratio.num > 0 { r.sample_aspect_ratio } else { ff::AVRational { num: 1, den: 1 } };
        let key = UploadKey {
            size: (r.width, r.height),
            format: r.format,
            sar: (sar.num, sar.den),
            resize,
            colorspace: r.colorspace as c_int,
            range: r.color_range as c_int,
        };
        if self.broken.contains(&key) {
            return false;
        }
        if self.graph.as_ref().is_none_or(|(k, _)| *k != key) {
            self.graph = None;
            let Some(mut dev) = cuda_device_if_open() else { return false };
            let args = format!(
                "video_size={}x{}:pix_fmt={}:time_base=1/1000:pixel_aspect={}/{}:colorspace={}:range={}",
                r.width, r.height, r.format, sar.num, sar.den, key.colorspace, key.range
            );
            let chain = format!(
                "hwupload,scale_cuda=w={}:h={}:interp_algo={}:format=nv12",
                resize.w,
                resize.h,
                cuda_algo(resize.quality)
            );
            let t0 = std::time::Instant::now();
            let g = Graph::with_setup("buffer", &args, "buffersink", &chain, 1, &mut |f| {
                unsafe {
                    if av::from_cstr((*(*f).filter).name).as_deref() == Some("hwupload") {
                        (*f).hw_device_ctx = ff::av_buffer_ref(dev);
                    }
                }
                Ok(())
            });
            unsafe { ff::av_buffer_unref(&mut dev) };
            match g {
                Ok(g) => {
                    log::debug!(
                        "gpu resize: {}x{} {} uploaded → {}x{}, graph built in {:.1} ms",
                        r.width,
                        r.height,
                        av::pix_fmt_name(r.format),
                        resize.w,
                        resize.h,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    self.graph = Some((key, g));
                }
                Err(e) => {
                    log::debug!("gpu resize: can't upload {}: {e:#}", av::pix_fmt_name(r.format));
                    self.broken.push(key);
                    return false;
                }
            }
        }
        let Some((_, g)) = self.graph.as_mut() else { return false };
        if g.push(frame).is_err() || !g.pull(&mut self.out) {
            self.graph = None;
            self.broken.push(key);
            return false;
        }
        unsafe {
            (*self.out.0).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
        }
        frame.unref();
        unsafe { ff::av_frame_move_ref(frame.0, self.out.0) };
        true
    }
}

/// `scale_cuda`'s name for a scaling quality.
fn cuda_algo(q: ScaleQuality) -> &'static str {
    match q {
        ScaleQuality::Nearest => "nearest",
        ScaleQuality::Linear => "bilinear",
        ScaleQuality::Good => "bicubic",
        ScaleQuality::Best => "lanczos",
    }
}

/// Gives an untagged picture the YUV matrix the presenter's swscale path
/// assumes for it (BT.709 for HD, BT.601 below), judged by the size it was
/// decoded at. The GPU's conversion (and swscale, once the picture is
/// window-sized) would otherwise take BT.601 for HD as well, or judge it by
/// the smaller size.
fn tag_matrix(frame: &mut Frame) {
    use ff::AVColorSpace::*;
    let r = frame.raw();
    if r.colorspace == AVCOL_SPC_UNSPECIFIED || r.colorspace == AVCOL_SPC_RESERVED {
        let cs = if r.height > 576 { AVCOL_SPC_BT709 } else { AVCOL_SPC_SMPTE170M };
        unsafe { (*frame.0).colorspace = cs };
    }
}

/// Feeds packets from a queue into a decoder and pulls frames out.
pub struct Decoder {
    pub ctx: CodecCtx,
    pub queue: Arc<PacketQueue>,
    pub pkt_serial: i32,
    pub finished: i32,
    pending: Option<Packet>,
    pub hw_pix_fmt: Option<c_int>,
    sw: Frame,
    /// Buffers for frames downloaded from the GPU, reused frame after frame.
    pool: DownloadPool,
    stop: Arc<AtomicBool>,
    /// Decoded to the end of the stream, nothing sent since.
    drained: bool,
    reopen: Option<Reopen>,
    /// Hand hardware frames out as they are (video: they may be resized on
    /// the GPU and presented from there). Otherwise they are downloaded.
    pub keep_hw: bool,
    gpu: GpuScaler,
    /// See `OpenedCodec::lean`.
    lean: bool,
}

/// Picture buffers for downloads from a hardware decoder. Without it every
/// frame got a new buffer (3 MB at 1080p, 12 MB at 4K) that glibc maps and
/// the kernel zeroes, since big blocks are mapped on their own (`mem.rs`).
struct DownloadPool {
    pool: *mut ff::AVBufferPool,
    /// Pixel format, width and height the buffers are sized for.
    key: (c_int, c_int, c_int),
}

// SAFETY: an AVBufferPool is thread-safe; buffers return to it from any thread.
unsafe impl Send for DownloadPool {}

const DOWNLOAD_ALIGN: c_int = 64;

impl DownloadPool {
    /// Gives `dst` pooled buffers for a `w`×`h` picture in `fmt`.
    unsafe fn fill(&mut self, dst: *mut ff::AVFrame, fmt: ff::AVPixelFormat, w: c_int, h: c_int) -> bool {
        unsafe {
            let key = (fmt as c_int, w, h);
            if self.pool.is_null() || self.key != key {
                ff::av_buffer_pool_uninit(&mut self.pool); // frees once its buffers are back
                let size = ff::av_image_get_buffer_size(fmt, w, h, DOWNLOAD_ALIGN);
                if size <= 0 {
                    return false;
                }
                self.pool = ff::av_buffer_pool_init(size as usize, None);
                self.key = key;
            }
            let buf = ff::av_buffer_pool_get(self.pool);
            if buf.is_null() {
                return false;
            }
            (*dst).buf[0] = buf;
            (*dst).format = fmt as c_int;
            (*dst).width = w;
            (*dst).height = h;
            ff::av_image_fill_arrays(
                (*dst).data.as_mut_ptr(),
                (*dst).linesize.as_mut_ptr(),
                (*buf).data,
                fmt,
                w,
                h,
                DOWNLOAD_ALIGN,
            ) >= 0
        }
    }
}

impl Drop for DownloadPool {
    fn drop(&mut self) {
        unsafe { ff::av_buffer_pool_uninit(&mut self.pool) };
    }
}

impl Decoder {
    pub fn new(opened: OpenedCodec, queue: Arc<PacketQueue>, stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            hw_pix_fmt: opened.hw.as_ref().map(|h| h.pix_fmt),
            ctx: opened.ctx,
            queue,
            pkt_serial: -1,
            finished: 0,
            pending: None,
            sw: Frame::new(),
            pool: DownloadPool { pool: ptr::null_mut(), key: (0, 0, 0) },
            drained: false,
            reopen: opened.reopen,
            keep_hw: false,
            gpu: GpuScaler::new(),
            lean: opened.lean,
        }
    }

    /// Whether `frame` is still in GPU memory (see `keep_hw`).
    pub fn is_hw(&self, frame: &Frame) -> bool {
        self.hw_pix_fmt == Some(frame.format())
    }

    /// Whether [`Decoder::resize_on_gpu`] can resize `frame`.
    pub fn can_resize(&self, frame: &Frame) -> bool {
        self.is_hw(frame) && GpuScaler::supports(frame)
    }

    /// Frees the buffers kept for downloads (`DownloadPool`), while frames
    /// stay on the GPU: the first pictures, decoded before the window had a
    /// size, are downloaded whole, and their buffers (30 MB for 10-bit
    /// 1080p) would otherwise be kept for good.
    pub fn release_downloads(&mut self) {
        if !self.pool.pool.is_null() {
            self.pool = DownloadPool { pool: ptr::null_mut(), key: (0, 0, 0) };
        }
    }

    /// Resizes a hardware frame from [`Decoder::decode`] to `resize` on the
    /// GPU, in place (it stays there). False if the GPU can't: the frame is
    /// left as it was.
    pub fn resize_on_gpu(&mut self, frame: &mut Frame, resize: GpuResize) -> bool {
        if !self.is_hw(frame) || !GpuScaler::supports(frame) {
            return false;
        }
        match self.gpu.run(frame, resize) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("gpu resize unavailable, resizing on the CPU instead: {e:#}");
                false
            }
        }
    }

    /// Decodes the next frame into `frame`. Hardware frames are downloaded
    /// to system memory transparently.
    pub fn decode(&mut self, frame: &mut Frame) -> DecodeResult {
        loop {
            if self.queue.serial() == self.pkt_serial && !self.ctx.0.is_null() {
                loop {
                    if self.queue.is_aborted() || self.stop.load(Ordering::Acquire) {
                        return DecodeResult::Aborted;
                    }
                    let r = unsafe { ff::avcodec_receive_frame(self.ctx.0, frame.0) };
                    if r >= 0 {
                        if self.lean && self.hw_pix_fmt.is_some_and(|hw| frame.format() != hw) && self.reopen_software()
                        {
                            frame.unref();
                            return DecodeResult::Restart;
                        }
                        if let Some(hw) = self.hw_pix_fmt
                            && frame.format() == hw
                            && !self.keep_hw
                            && !self.download(frame)
                        {
                            frame.unref();
                            continue;
                        }
                        return DecodeResult::Frame;
                    }
                    if r == ff::AVERROR_EOF {
                        self.finished = self.pkt_serial;
                        self.drained = true;
                        unsafe { ff::avcodec_flush_buffers(self.ctx.0) };
                        return DecodeResult::Eof;
                    }
                    if r == av::EAGAIN {
                        break;
                    }
                    // Corrupt data: log and keep going rather than stopping playback.
                    log::debug!("decode error: {}", av::err_str(r));
                    break;
                }
            }

            let pkt = loop {
                let qp = if let Some(p) = self.pending.take() {
                    super::sync::QueuedPacket { pkt: p, serial: self.pkt_serial }
                } else {
                    // At the end only a seek brings more packets: let go of
                    // the codec if none comes for a while.
                    let park_at = (self.drained && self.reopen.is_some() && !self.ctx.0.is_null())
                        .then(|| std::time::Instant::now() + PARK_AFTER);
                    match self.queue.get_until(&self.stop, park_at) {
                        Some(p) => p,
                        None if park_at.is_some() && !self.queue.is_aborted() && !self.stop.load(Ordering::Acquire) => {
                            self.park();
                            continue;
                        }
                        None => return DecodeResult::Aborted,
                    }
                };
                self.drained = false;
                if self.ctx.0.is_null() && !self.unpark() {
                    return DecodeResult::Aborted;
                }
                if qp.serial != self.pkt_serial {
                    unsafe { ff::avcodec_flush_buffers(self.ctx.0) };
                    self.finished = 0;
                    self.pkt_serial = qp.serial;
                }
                if self.queue.serial() == qp.serial {
                    break qp.pkt;
                }
            };

            let r = unsafe {
                if pkt.is_empty() {
                    ff::avcodec_send_packet(self.ctx.0, ptr::null())
                } else {
                    ff::avcodec_send_packet(self.ctx.0, pkt.0)
                }
            };
            if r == av::EAGAIN {
                // Decoder is full: keep the packet and drain frames first.
                self.pending = Some(pkt);
            } else if r < 0 && r != ff::AVERROR_EOF {
                log::debug!("send_packet: {}", av::err_str(r));
            }
        }
    }

    /// The GPU turned the stream down (FFmpeg decodes it in software now,
    /// with the single thread a hardware decoder gets): opens the codec again
    /// for software decoding with its usual threads.
    fn reopen_software(&mut self) -> bool {
        self.lean = false;
        let Some(r) = self.reopen.as_mut() else { return false };
        r.hw_mode = None; // and when it is reopened after `park`
        log::info!("the GPU can't decode this video; decoding it in software");
        match unsafe { open_par(r.par, r.time_base, r.threads, None, None) } {
            Ok(opened) => {
                self.ctx = opened.ctx;
                self.hw_pix_fmt = None;
                self.pending = None;
                self.gpu = GpuScaler::new();
                true
            }
            Err(e) => {
                log::warn!("could not reopen the decoder: {e:#}");
                false
            }
        }
    }

    /// Frees the codec, its GPU device reference and the picture pools.
    fn park(&mut self) {
        log::debug!("decoder idle at the end of the file: closing it until a seek");
        self.ctx = CodecCtx(ptr::null_mut());
        self.sw.unref();
        self.pool = DownloadPool { pool: ptr::null_mut(), key: (0, 0, 0) };
        self.gpu = GpuScaler::new(); // its graph holds the device too
    }

    /// Opens the codec again after `park` (a seek arrived). The packet that
    /// woke it starts at a keyframe, as every packet after a seek does.
    fn unpark(&mut self) -> bool {
        let Some(r) = &self.reopen else { return false };
        match unsafe { open_par(r.par, r.time_base, r.threads, r.hw_mode, None) } {
            Ok(opened) => {
                log::debug!("decoder reopened");
                self.ctx = opened.ctx;
                self.hw_pix_fmt = opened.hw.as_ref().map(|h| h.pix_fmt);
                self.lean = opened.lean;
                true
            }
            Err(e) => {
                log::warn!("could not reopen the decoder: {e:#}");
                false
            }
        }
    }

    /// Downloads a hardware frame whole, in place.
    pub fn download(&mut self, frame: &mut Frame) -> bool {
        unsafe {
            self.sw.unref();
            let src = frame.0;
            if !(*src).hw_frames_ctx.is_null() {
                let fc = (*(*src).hw_frames_ctx).data as *const ff::AVHWFramesContext;
                if !self.pool.fill(self.sw.0, (*fc).sw_format, (*src).width, (*src).height) {
                    self.sw.unref(); // let FFmpeg allocate instead
                }
            }
            let r = ff::av_hwframe_transfer_data(self.sw.0, frame.0, 0);
            if r < 0 {
                log::warn!("hwframe download failed: {}", av::err_str(r));
                return false;
            }
            ff::av_frame_copy_props(self.sw.0, frame.0);
            frame.unref();
            ff::av_frame_move_ref(frame.0, self.sw.0);
        }
        true
    }
}
