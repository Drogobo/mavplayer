//! Codec opening (with optional hardware acceleration) and the generic
//! packet → frame decode loop shared by the audio and video threads.

use super::av::{self, CodecCtx, Frame, Packet, ff};
use super::sync::PacketQueue;
use super::types::HwDecode;
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
}

/// Device types to try, best first, for the current platform.
fn hw_device_types() -> &'static [ff::AVHWDeviceType] {
    use ff::AVHWDeviceType::*;
    #[cfg(target_os = "linux")]
    {
        &[AV_HWDEVICE_TYPE_VAAPI, AV_HWDEVICE_TYPE_CUDA, AV_HWDEVICE_TYPE_VDPAU]
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
            let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
            let r = unsafe { ff::av_hwdevice_ctx_create(&mut dev, ty, ptr::null(), ptr::null_mut(), 0) };
            if r < 0 {
                log::debug!("hw device {:?} unavailable: {}", ty, av::err_str(r));
                break;
            }
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

/// Opens a decoder for `stream`.
pub fn open_codec(
    stream: *mut ff::AVStream,
    threads: u32,
    hw_mode: Option<HwDecode>,
    sub_charset: Option<&str>,
) -> anyhow::Result<OpenedCodec> {
    unsafe {
        let par = (*stream).codecpar;
        let codec = ff::avcodec_find_decoder((*par).codec_id);
        if codec.is_null() {
            anyhow::bail!("no decoder for codec '{}'", av::codec_name((*par).codec_id));
        }
        let ctx = CodecCtx(ff::avcodec_alloc_context3(codec));
        if ctx.0.is_null() {
            anyhow::bail!("out of memory");
        }
        av::check(ff::avcodec_parameters_to_context(ctx.0, par), "avcodec_parameters_to_context")?;
        (*ctx.0).pkt_timebase = (*stream).time_base;
        let mut opts = av::Dict::new();
        let threads_s = if threads == 0 { "auto".to_string() } else { threads.to_string() };
        opts.set("threads", &threads_s);
        if (*par).codec_type == ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE
            && let Some(cs) = sub_charset
        {
            opts.set("sub_charenc", cs);
        }
        let hw = match hw_mode {
            Some(m) if hw_worth_it(&*par, m) => try_setup_hw(ctx.0, codec),
            _ => None,
        };
        let r = ff::avcodec_open2(ctx.0, codec, &mut opts.0);
        if r < 0 {
            if hw.is_some() {
                log::warn!("hardware decoder failed to open ({}), retrying in software", av::err_str(r));
                return open_codec(stream, threads, None, sub_charset);
            }
            anyhow::bail!("could not open decoder: {}", av::err_str(r));
        }
        let decoder_name = av::from_cstr((*codec).name).unwrap_or_default();
        Ok(OpenedCodec { ctx, hw, decoder_name })
    }
}

pub enum DecodeResult {
    Frame,
    /// The decoder was fully drained (end of stream for this serial).
    Eof,
    Aborted,
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
    stop: Arc<AtomicBool>,
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
        }
    }

    /// Decodes the next frame into `frame`. Hardware frames are downloaded
    /// to system memory transparently.
    pub fn decode(&mut self, frame: &mut Frame) -> DecodeResult {
        loop {
            if self.queue.serial() == self.pkt_serial {
                loop {
                    if self.queue.is_aborted() || self.stop.load(Ordering::Acquire) {
                        return DecodeResult::Aborted;
                    }
                    let r = unsafe { ff::avcodec_receive_frame(self.ctx.0, frame.0) };
                    if r >= 0 {
                        if let Some(hw) = self.hw_pix_fmt
                            && frame.format() == hw
                            && !self.download(frame)
                        {
                            frame.unref();
                            continue;
                        }
                        return DecodeResult::Frame;
                    }
                    if r == ff::AVERROR_EOF {
                        self.finished = self.pkt_serial;
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
                    match self.queue.get(&self.stop) {
                        Some(p) => p,
                        None => return DecodeResult::Aborted,
                    }
                };
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

    fn download(&mut self, frame: &mut Frame) -> bool {
        unsafe {
            self.sw.unref();
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
