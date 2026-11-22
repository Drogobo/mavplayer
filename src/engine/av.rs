//! Thin, safe-ish RAII wrappers around the FFmpeg C API.
//!
//! The engine talks to FFmpeg through `ffmpeg-sys-next` directly; this module
//! keeps ownership rules (who frees what) in one place.

#![allow(dead_code)]

pub use ffmpeg_sys_next as ff;

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;

pub const NOPTS: i64 = ff::AV_NOPTS_VALUE;
pub const EAGAIN: c_int = ff::AVERROR(libc_eagain());

const fn libc_eagain() -> c_int {
    // EAGAIN is 11 on Linux/Windows(MSVC CRT), 35 on macOS / BSDs.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        35
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        11
    }
}

/// Human readable FFmpeg error.
pub fn err_str(code: c_int) -> String {
    let mut buf = [0 as c_char; 256];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

pub fn check(code: c_int, what: &str) -> anyhow::Result<c_int> {
    if code < 0 {
        anyhow::bail!("{what}: {}", err_str(code))
    }
    Ok(code)
}

#[inline]
pub fn q2d(r: ff::AVRational) -> f64 {
    if r.den == 0 { 0.0 } else { r.num as f64 / r.den as f64 }
}

#[inline]
pub fn ts_to_secs(ts: i64, tb: ff::AVRational) -> f64 {
    if ts == NOPTS { f64::NAN } else { ts as f64 * q2d(tb) }
}

pub fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Converts a possibly-null C string to an owned Rust string.
pub unsafe fn from_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() { None } else { Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()) }
}

pub unsafe fn dict_get(dict: *const ff::AVDictionary, key: &str) -> Option<String> {
    if dict.is_null() {
        return None;
    }
    let k = cstr(key);
    let e = unsafe { ff::av_dict_get(dict, k.as_ptr(), ptr::null(), 0) };
    if e.is_null() { None } else { unsafe { from_cstr((*e).value) }.filter(|s| !s.trim().is_empty()) }
}

pub unsafe fn dict_all(dict: *const ff::AVDictionary) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if dict.is_null() {
        return out;
    }
    let empty = cstr("");
    let mut e: *const ff::AVDictionaryEntry = ptr::null();
    loop {
        e = unsafe { ff::av_dict_get(dict, empty.as_ptr(), e, ff::AV_DICT_IGNORE_SUFFIX as c_int) };
        if e.is_null() {
            break;
        }
        unsafe {
            if let (Some(k), Some(v)) = (from_cstr((*e).key), from_cstr((*e).value)) {
                out.push((k, v));
            }
        }
    }
    out
}

/// Owned `AVDictionary` used to pass options into FFmpeg.
pub struct Dict(pub *mut ff::AVDictionary);

impl Dict {
    pub fn new() -> Self {
        Dict(ptr::null_mut())
    }
    pub fn set(&mut self, k: &str, v: &str) {
        let (k, v) = (cstr(k), cstr(v));
        unsafe {
            ff::av_dict_set(&mut self.0, k.as_ptr(), v.as_ptr(), 0);
        }
    }
}

impl Drop for Dict {
    fn drop(&mut self) {
        unsafe { ff::av_dict_free(&mut self.0) }
    }
}

/// Owned `AVPacket`.
pub struct Packet(pub *mut ff::AVPacket);
unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> Self {
        let p = unsafe { ff::av_packet_alloc() };
        assert!(!p.is_null(), "av_packet_alloc failed");
        Packet(p)
    }
    /// An empty packet signals "drain" to a decoder.
    pub fn is_empty(&self) -> bool {
        unsafe { (*self.0).data.is_null() && (*self.0).size == 0 }
    }
    pub fn stream_index(&self) -> i32 {
        unsafe { (*self.0).stream_index }
    }
    pub fn size(&self) -> usize {
        unsafe { (*self.0).size.max(0) as usize }
    }
    pub fn duration(&self) -> i64 {
        unsafe { (*self.0).duration }
    }
    pub fn pts(&self) -> i64 {
        unsafe { (*self.0).pts }
    }
    pub fn dts(&self) -> i64 {
        unsafe { (*self.0).dts }
    }
    pub fn unref(&mut self) {
        unsafe { ff::av_packet_unref(self.0) }
    }
    /// Moves the contents of `self` into a freshly allocated packet.
    pub fn take(&mut self) -> Packet {
        let p = Packet::new();
        unsafe { ff::av_packet_move_ref(p.0, self.0) };
        p
    }
    pub fn clone_ref(&self) -> Option<Packet> {
        let p = unsafe { ff::av_packet_clone(self.0) };
        if p.is_null() { None } else { Some(Packet(p)) }
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        unsafe { ff::av_packet_free(&mut self.0) }
    }
}

/// Owned `AVFrame`.
pub struct Frame(pub *mut ff::AVFrame);
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    pub fn new() -> Self {
        let f = unsafe { ff::av_frame_alloc() };
        assert!(!f.is_null(), "av_frame_alloc failed");
        Frame(f)
    }
    pub fn take(&mut self) -> Frame {
        let f = Frame::new();
        unsafe { ff::av_frame_move_ref(f.0, self.0) };
        f
    }
    pub fn clone_ref(&self) -> Option<Frame> {
        let f = unsafe { ff::av_frame_clone(self.0) };
        if f.is_null() { None } else { Some(Frame(f)) }
    }
    pub fn unref(&mut self) {
        unsafe { ff::av_frame_unref(self.0) }
    }
    #[inline]
    pub fn raw(&self) -> &ff::AVFrame {
        unsafe { &*self.0 }
    }
    pub fn width(&self) -> i32 {
        self.raw().width
    }
    pub fn height(&self) -> i32 {
        self.raw().height
    }
    pub fn format(&self) -> c_int {
        self.raw().format
    }
    pub fn nb_samples(&self) -> i32 {
        self.raw().nb_samples
    }
    /// The pixel format of the picture's data: for a hardware frame, the
    /// format it has in GPU memory (what a download produces).
    pub fn sw_format(&self) -> c_int {
        let r = self.raw();
        if r.hw_frames_ctx.is_null() {
            return r.format;
        }
        unsafe { (*((*r.hw_frames_ctx).data as *const ff::AVHWFramesContext)).sw_format as c_int }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { ff::av_frame_free(&mut self.0) }
    }
}

/// Owned `AVCodecContext`.
pub struct CodecCtx(pub *mut ff::AVCodecContext);
unsafe impl Send for CodecCtx {}

impl Drop for CodecCtx {
    fn drop(&mut self) {
        unsafe { ff::avcodec_free_context(&mut self.0) }
    }
}

impl CodecCtx {
    pub fn get(&self) -> &ff::AVCodecContext {
        unsafe { &*self.0 }
    }
}

/// Owned input `AVFormatContext`.
pub struct FormatCtx(pub *mut ff::AVFormatContext);
unsafe impl Send for FormatCtx {}

impl FormatCtx {
    pub fn get(&self) -> &ff::AVFormatContext {
        unsafe { &*self.0 }
    }
    pub fn nb_streams(&self) -> usize {
        self.get().nb_streams as usize
    }
    pub fn stream(&self, i: usize) -> *mut ff::AVStream {
        unsafe { *self.get().streams.add(i) }
    }
}

impl Drop for FormatCtx {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        unsafe {
            // A context reading through our own I/O (`CustomIo`) owns it via
            // `opaque`; FFmpeg leaves custom I/O to the caller.
            let io = (*self.0).opaque as *mut CustomIo;
            (*self.0).opaque = ptr::null_mut();
            if (*self.0).iformat.is_null() {
                ff::avformat_free_context(self.0);
                self.0 = ptr::null_mut();
            } else {
                ff::avformat_close_input(&mut self.0);
            }
            if !io.is_null() {
                drop(Box::from_raw(io));
            }
        }
    }
}

/// Reads for an `AVIOContext` that isn't a file or URL (discs read through
/// libdvdnav or as audio CD sectors).
pub trait IoSource: Send {
    /// Fills `buf`; returns the bytes read, 0 at the end, or an AVERROR.
    fn read(&mut self, buf: &mut [u8]) -> c_int;
    /// `whence` as in `avio_alloc_context` (including `AVSEEK_SIZE`).
    /// Return a negative AVERROR when seeking isn't possible.
    fn seek(&mut self, _offset: i64, _whence: c_int) -> i64 {
        -1
    }
    fn seekable(&self) -> bool {
        false
    }
}

/// An `AVIOContext` reading from an [`IoSource`], owned by the format
/// context it was attached to (see `FormatCtx`'s `Drop`).
pub struct CustomIo {
    pb: *mut ff::AVIOContext,
    source: *mut Box<dyn IoSource>,
}

impl Drop for CustomIo {
    fn drop(&mut self) {
        unsafe {
            if !self.pb.is_null() {
                ff::av_freep(&mut (*self.pb).buffer as *mut *mut u8 as *mut std::ffi::c_void);
                ff::avio_context_free(&mut self.pb);
            }
            drop(Box::from_raw(self.source));
        }
    }
}

unsafe extern "C" fn custom_read(opaque: *mut std::ffi::c_void, buf: *mut u8, size: c_int) -> c_int {
    let src = unsafe { &mut *(opaque as *mut Box<dyn IoSource>) };
    if size <= 0 {
        return 0;
    }
    let n = src.read(unsafe { std::slice::from_raw_parts_mut(buf, size as usize) });
    if n == 0 { ff::AVERROR_EOF } else { n }
}

unsafe extern "C" fn custom_seek(opaque: *mut std::ffi::c_void, offset: i64, whence: c_int) -> i64 {
    let src = unsafe { &mut *(opaque as *mut Box<dyn IoSource>) };
    src.seek(offset, whence)
}

/// A format context that reads through `source`. Open it with
/// `avformat_open_input` (the context pointer may change; keep the result).
pub fn custom_io_context(source: Box<dyn IoSource>, buffer_size: usize) -> anyhow::Result<*mut ff::AVFormatContext> {
    unsafe {
        let seekable = source.seekable();
        let source = Box::into_raw(Box::new(source));
        let buf = ff::av_malloc(buffer_size) as *mut u8;
        if buf.is_null() {
            drop(Box::from_raw(source));
            anyhow::bail!("out of memory");
        }
        let pb = ff::avio_alloc_context(
            buf,
            buffer_size as c_int,
            0,
            source as *mut std::ffi::c_void,
            Some(custom_read),
            None,
            if seekable { Some(custom_seek) } else { None },
        );
        if pb.is_null() {
            ff::av_free(buf as *mut std::ffi::c_void);
            drop(Box::from_raw(source));
            anyhow::bail!("out of memory");
        }
        if !seekable {
            (*pb).seekable = 0;
        }
        let io = Box::into_raw(Box::new(CustomIo { pb, source }));
        let ic = ff::avformat_alloc_context();
        if ic.is_null() {
            drop(Box::from_raw(io));
            anyhow::bail!("out of memory");
        }
        (*ic).pb = pb;
        (*ic).flags |= ff::AVFMT_FLAG_CUSTOM_IO as c_int;
        (*ic).opaque = io as *mut std::ffi::c_void;
        Ok(ic)
    }
}

/// Frees a context from [`custom_io_context`] that never got opened (or
/// whose `avformat_open_input` failed and freed only the context: pass the
/// `opaque` taken before the call then).
pub unsafe fn free_custom_io(io: *mut std::ffi::c_void) {
    if !io.is_null() {
        drop(unsafe { Box::from_raw(io as *mut CustomIo) });
    }
}

/// Owned `AVFilterGraph`.
pub struct FilterGraph(pub *mut ff::AVFilterGraph);
unsafe impl Send for FilterGraph {}

impl Drop for FilterGraph {
    fn drop(&mut self) {
        unsafe { ff::avfilter_graph_free(&mut self.0) }
    }
}

/// Whether this FFmpeg build has filter `name`. Cached: the video thread
/// asks for every frame, and FFmpeg's lookup is a linear search.
pub fn filter_available(name: &'static str) -> bool {
    use parking_lot::Mutex;
    use std::collections::HashMap;
    static CACHE: Mutex<Option<HashMap<&'static str, bool>>> = Mutex::new(None);
    *CACHE.lock().get_or_insert_with(HashMap::new).entry(name).or_insert_with(|| {
        let n = cstr(name);
        unsafe { !ff::avfilter_get_by_name(n.as_ptr()).is_null() }
    })
}

pub fn codec_name(id: ff::AVCodecID) -> String {
    unsafe { from_cstr(ff::avcodec_get_name(id)).unwrap_or_else(|| "unknown".into()) }
}

pub fn pix_fmt_name(fmt: c_int) -> String {
    if fmt < 0 {
        return "none".into();
    }
    unsafe {
        let f: ff::AVPixelFormat = std::mem::transmute(fmt);
        from_cstr(ff::av_get_pix_fmt_name(f)).unwrap_or_else(|| "unknown".into())
    }
}

pub fn sample_fmt_name(fmt: c_int) -> String {
    if fmt < 0 {
        return "none".into();
    }
    unsafe {
        let f: ff::AVSampleFormat = std::mem::transmute(fmt);
        from_cstr(ff::av_get_sample_fmt_name(f)).unwrap_or_else(|| "unknown".into())
    }
}

pub fn channel_layout_name(l: &ff::AVChannelLayout) -> String {
    let mut buf = [0 as c_char; 128];
    unsafe {
        if ff::av_channel_layout_describe(l, buf.as_mut_ptr(), buf.len()) < 0 {
            return format!("{} ch", l.nb_channels);
        }
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Lowers FFmpeg's own log noise; the engine reports errors itself.
pub fn init_logging(verbose: bool) {
    unsafe {
        ff::av_log_set_level(if verbose { ff::AV_LOG_INFO } else { ff::AV_LOG_ERROR } as c_int);
    }
}
