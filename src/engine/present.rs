//! The presenter thread: shows each decoded frame at the right moment
//! relative to the master clock, converting it to BGRA just in time.

use super::SessionCtx;
use super::av::{self, Frame, ff};
use super::sync::{VideoFrame, now};
use super::types::*;
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SYNC_THRESHOLD_MIN: f64 = 0.04;
const SYNC_THRESHOLD_MAX: f64 = 0.1;
const FRAMEDUP_THRESHOLD: f64 = 0.1;
const NOSYNC_THRESHOLD: f64 = 10.0;
const MAX_FRAME_DURATION: f64 = 10.0;
/// Border around presented pictures (see [`extend_edges`]). One pixel is
/// all bilinear sampling reaches.
const PRESENT_PAD: u32 = 1;

/// Cached swscale context for YUV → BGRA (+ scale) conversion.
pub struct Converter {
    sws: *mut ff::SwsContext,
    key: (i32, i32, c_int, u32, u32, c_int, c_int, i32),
    /// The output's alpha channel needs forcing to opaque.
    fix_alpha: bool,
}
unsafe impl Send for Converter {}

impl Converter {
    pub fn new() -> Self {
        Self { sws: ptr::null_mut(), key: (0, 0, -1, 0, 0, -1, -1, 0), fix_alpha: true }
    }

    /// Converts `f` to tightly-packed BGRA of `dw`×`dh`, surrounded by a
    /// border of `pad` pixels that repeat the picture's outermost pixels (so
    /// the image is `dw + 2*pad` × `dh + 2*pad`).
    pub fn convert(&mut self, f: &Frame, dw: u32, dh: u32, pad: u32, quality: ScaleQuality) -> Option<Vec<u8>> {
        let r = f.raw();
        let flags = match quality {
            ScaleQuality::Nearest => ff::SwsFlags::SWS_POINT,
            ScaleQuality::Linear => ff::SwsFlags::SWS_BILINEAR,
            ScaleQuality::Good => ff::SwsFlags::SWS_BICUBIC,
            ScaleQuality::Best => ff::SwsFlags::SWS_LANCZOS,
        } as c_int;
        // "Linear" is the speed option: let swscale use its fast SIMD paths.
        // The others trade some speed for exact rounding and full chroma.
        let flags = if quality == ScaleQuality::Linear {
            flags
        } else {
            flags | ff::SwsFlags::SWS_ACCURATE_RND as c_int | ff::SwsFlags::SWS_FULL_CHR_H_INT as c_int
        };
        let key = (r.width, r.height, r.format, dw, dh, r.colorspace as c_int, r.color_range as c_int, flags);
        unsafe {
            if self.sws.is_null() || self.key != key {
                ff::sws_freeContext(self.sws);
                self.sws = ptr::null_mut();
                let src_fmt: ff::AVPixelFormat = std::mem::transmute(r.format);
                let ctx = ff::sws_alloc_context();
                if ctx.is_null() {
                    return None;
                }
                let set = |k: &str, v: i64| {
                    let k = av::cstr(k);
                    ff::av_opt_set_int(ctx as *mut c_void, k.as_ptr(), v, 0);
                };
                set("srcw", r.width as i64);
                set("srch", r.height as i64);
                set("src_format", r.format as i64);
                set("dstw", dw as i64);
                set("dsth", dh as i64);
                set("dst_format", ff::AVPixelFormat::AV_PIX_FMT_BGRA as i64);
                set("sws_flags", flags as i64);
                let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4);
                set("threads", threads as i64);
                if ff::sws_init_context(ctx, ptr::null_mut(), ptr::null_mut()) < 0 {
                    ff::sws_freeContext(ctx);
                    log::warn!("swscale cannot convert from {}", av::pix_fmt_name(r.format));
                    return None;
                }
                // Correct YUV matrix: BT.709 for HD unless tagged otherwise.
                let mut cs = r.colorspace as c_int;
                if cs == ff::AVColorSpace::AVCOL_SPC_UNSPECIFIED as c_int
                    || cs == ff::AVColorSpace::AVCOL_SPC_RESERVED as c_int
                {
                    cs = if r.height > 576 { ff::SWS_CS_ITU709 as c_int } else { ff::SWS_CS_ITU601 as c_int };
                }
                let full_src = r.color_range == ff::AVColorRange::AVCOL_RANGE_JPEG || is_rgb_or_jpeg(src_fmt);
                ff::sws_setColorspaceDetails(
                    ctx,
                    ff::sws_getCoefficients(cs),
                    full_src as c_int,
                    ff::sws_getCoefficients(ff::SWS_CS_DEFAULT as c_int),
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                );
                self.sws = ctx;
                self.key = key;
                self.fix_alpha = !opaque_yuv(r.format);
            }
            let (pad, w, h) = (pad as usize, dw as usize, dh as usize);
            let stride = (w + 2 * pad) * 4;
            let len = stride * (h + 2 * pad);
            let first = pad * stride + pad * 4;
            // Not zeroed first: swscale writes every byte of the picture and
            // the border is copied from it, and clearing a 4K frame's 33 MB
            // each time costs as much as a fast conversion.
            let mut out: Vec<u8> = Vec::with_capacity(len);
            let dst_data = [out.as_mut_ptr().add(first), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let dst_stride = [stride as c_int, 0, 0, 0];
            let done = ff::sws_scale(
                self.sws,
                r.data.as_ptr() as *const *const u8,
                r.linesize.as_ptr(),
                0,
                r.height,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if done <= 0 {
                return None;
            }
            // Rows swscale didn't produce (never expected for a whole frame).
            let done = (done as usize).min(h);
            if done < h {
                ptr::write_bytes(out.as_mut_ptr().add((pad + done) * stride), 0, (h - done) * stride);
            }
            out.set_len(len);
            // Force opaque alpha. swscale already writes 255 when converting
            // from YUV without alpha (nearly all video), so only RGB(X) and
            // alpha sources need this full-frame pass.
            if self.fix_alpha {
                for px in out.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
            if pad > 0 {
                extend_edges(&mut out, w, h, pad);
            }
            Some(out)
        }
    }
}

/// Fills the `pad`-pixel border around a `w`×`h` BGRA picture (stored with
/// that border, row stride `(w + 2*pad) * 4`) with copies of its edge pixels.
///
/// The GPU samples pictures with bilinear filtering, which near an edge
/// blends in the texel beyond it. GPUI packs images into shared atlas
/// textures without any gap, so without this border the picture's outermost
/// rows and columns picked up the neighbouring video frame or uninitialised
/// texture memory, and flickered as frames alternated between atlas slots.
fn extend_edges(buf: &mut [u8], w: usize, h: usize, pad: usize) {
    let stride = (w + 2 * pad) * 4;
    for row in buf.chunks_exact_mut(stride).skip(pad).take(h) {
        let (l, r) = (pad * 4, (pad + w - 1) * 4);
        let (left, right): ([u8; 4], [u8; 4]) = (row[l..l + 4].try_into().unwrap(), row[r..r + 4].try_into().unwrap());
        for px in row[..l].as_chunks_mut::<4>().0 {
            *px = left;
        }
        for px in row[r + 4..].as_chunks_mut::<4>().0 {
            *px = right;
        }
    }
    let (top, bottom) = (pad * stride, (pad + h - 1) * stride);
    for y in 0..pad {
        buf.copy_within(top..top + stride, y * stride);
        buf.copy_within(bottom..bottom + stride, (pad + h + y) * stride);
    }
}

/// A YUV (or gray) format without alpha: swscale's conversion to BGRA
/// fills the alpha channel with 255 itself.
fn opaque_yuv(fmt: c_int) -> bool {
    unsafe {
        let d = ff::av_pix_fmt_desc_get(std::mem::transmute::<c_int, ff::AVPixelFormat>(fmt));
        if d.is_null() {
            return false;
        }
        let flags = (*d).flags;
        let bad =
            ff::AV_PIX_FMT_FLAG_RGB | ff::AV_PIX_FMT_FLAG_ALPHA | ff::AV_PIX_FMT_FLAG_PAL | ff::AV_PIX_FMT_FLAG_HWACCEL;
        flags & bad as u64 == 0
    }
}

fn is_rgb_or_jpeg(f: ff::AVPixelFormat) -> bool {
    use ff::AVPixelFormat::*;
    matches!(
        f,
        AV_PIX_FMT_YUVJ420P
            | AV_PIX_FMT_YUVJ422P
            | AV_PIX_FMT_YUVJ444P
            | AV_PIX_FMT_RGB24
            | AV_PIX_FMT_BGR24
            | AV_PIX_FMT_RGBA
            | AV_PIX_FMT_BGRA
            | AV_PIX_FMT_ARGB
            | AV_PIX_FMT_ABGR
            | AV_PIX_FMT_GBRP
            | AV_PIX_FMT_PAL8
    )
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { ff::sws_freeContext(self.sws) }
    }
}

/// Output size: square pixels, fitted inside the target. Never larger than
/// the source unless `enlarge` is set (otherwise the GPU does the enlarging).
pub fn output_size(w: i32, h: i32, sar: f64, target: (u32, u32), enlarge: bool) -> (u32, u32) {
    let sar = if sar.is_finite() && sar > 0.0 { sar } else { 1.0 };
    let (mut dw, mut dh) =
        if sar >= 1.0 { ((w as f64 * sar).round(), h as f64) } else { (w as f64, (h as f64 / sar).round()) };
    let (tw, th) = (target.0.max(16) as f64, target.1.max(16) as f64);
    let fit = (tw / dw).min(th / dh);
    let scale = if enlarge { fit } else { fit.min(1.0) };
    dw = (dw * scale).round().max(2.0);
    dh = (dh * scale).round().max(2.0);
    (dw as u32, dh as u32)
}

/// Once-a-second presenter statistics for `-v`, to see where video stalls.
struct Stats {
    since: std::time::Instant,
    shown: u32,
    dropped: u32,
    converted: u32,
    convert_time: Duration,
    size: (u32, u32),
}

impl Stats {
    fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            shown: 0,
            dropped: 0,
            converted: 0,
            convert_time: Duration::ZERO,
            size: (0, 0),
        }
    }

    fn report(&mut self, ctx: &SessionCtx) {
        if self.since.elapsed() < Duration::from_secs(1) || !log::log_enabled!(log::Level::Debug) {
            return;
        }
        if self.shown + self.dropped + self.converted > 0 || !ctx.paused() {
            log::debug!(
                "video: {} shown, {} dropped, {} converted at {}x{} (avg {:.1} ms); {} queued{}{}",
                self.shown,
                self.dropped,
                self.converted,
                self.size.0,
                self.size.1,
                self.convert_time.as_secs_f64() * 1000.0 / self.converted.max(1) as f64,
                ctx.frames.len(),
                if ctx.paused() { ", paused" } else { "" },
                if ctx.audio_pending() { ", waiting for audio" } else { "" },
            );
        }
        *self = Self::new();
    }
}

struct Prepared {
    serial: i32,
    pts: f64,
    /// What it was converted for: a resize or a quality change redoes it.
    look: Look,
    image: PresentedFrame,
}

/// Output size request, scaling filters (downscaling, and whether enlarging
/// is done here with nearest neighbour) a picture was converted with, and
/// whether the window was out of sight (nothing converted at all).
type Look = ((u32, u32), (ScaleQuality, bool), bool);

fn current_look(ctx: &SessionCtx) -> Look {
    let hidden = ctx.controls.video_hidden.load(Ordering::Relaxed);
    let cfg = ctx.controls.config.lock();
    (ctx.controls.target_size(), (cfg.scale_quality, cfg.upscaler == Upscaler::Nearest), hidden)
}

/// How often a paused picture is redrawn while the window is being resized.
const RESHOW_INTERVAL: Duration = Duration::from_millis(50);

pub fn run(ctx: Arc<SessionCtx>, stop: Arc<AtomicBool>) {
    let mut conv = Converter::new();
    let mut frame_timer = now();
    let mut last: Option<(f64, f64, i32)> = None; // (pts, duration, serial)
    let mut prepared: Option<Prepared> = None;
    let mut dropped: u64 = 0;
    let mut stats = Stats::new();
    // The look of the picture on screen, and when it was last redone.
    let mut shown_look: Option<Look> = None;
    let mut reshown = std::time::Instant::now();

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        stats.report(&ctx);
        let paused = ctx.paused();
        let step = ctx.step.load(Ordering::Acquire);
        let show_one = ctx.show_one.load(Ordering::Acquire);
        let serial = ctx.vq.serial();

        // Discard frames from before the latest seek.
        let head = ctx.frames.with_front(|a, b| a.map(|a| (a.pts, a.duration, a.serial, b.map(|b| (b.pts, b.serial)))));
        let Some((pts, dur, fserial, next)) = head else {
            ctx.frames.wait(Duration::from_millis(10));
            continue;
        };
        if fserial != serial {
            ctx.frames.pop();
            continue;
        }

        // Preloaded next item: have the first picture ready for the switch.
        if ctx.is_standby() {
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            std::thread::sleep(Duration::from_millis(5));
            frame_timer = now();
            continue;
        }

        // While paused, or while the audio device is still opening, show the
        // first picture straight away but hold the rest (so a file opened
        // paused isn't black, and audio and video start together).
        let shown_this_serial = last.map(|l| l.2) == Some(fserial);
        let audio_pending = ctx.audio_pending();
        let hold = (paused || audio_pending) && shown_this_serial;
        if hold && !step && !show_one {
            // The window was resized (or the scaling changed) while paused:
            // redo the picture on screen rather than leave it stretched.
            let look = current_look(&ctx);
            if shown_look.is_some_and(|l| l != look) && !look.2 && reshown.elapsed() >= RESHOW_INTERVAL {
                reshown = std::time::Instant::now();
                shown_look = Some(look);
                let last = ctx.last_frame.lock().as_ref().and_then(|(f, sar)| f.clone_ref().map(|f| (f, *sar)));
                if let Some((frame, sar)) = last {
                    let vf = VideoFrame { frame, pts: 0.0, duration: 0.0, serial, sar };
                    if let Some(image) = prepare(&ctx, &mut conv, &vf, &mut stats)
                        && ctx.is_current()
                    {
                        *ctx.shared.frame_slot.lock() = Some(image);
                        ctx.wake();
                    }
                }
            }
            // Keep the next picture converted so un-pausing is instant.
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            std::thread::sleep(Duration::from_millis(10));
            frame_timer = now();
            continue;
        }

        if last.map(|l| l.2) != Some(fserial) {
            frame_timer = now();
        }

        let speed = ctx.controls.speed().max(0.01);
        let last_duration = match last {
            Some((lp, ld, ls)) if ls == fserial => {
                let d = pts - lp;
                if !d.is_finite() || d <= 0.0 || d > MAX_FRAME_DURATION { ld } else { d }
            }
            _ => 0.0,
        } / speed;

        let instant = step || show_one || ((paused || audio_pending) && !shown_this_serial);
        let delay = if instant { 0.0 } else { target_delay(&ctx, last_duration) };
        let t = now();
        if !instant && t < frame_timer + delay {
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
            let wait = (frame_timer + delay - now()).clamp(0.0, 0.01);
            if wait > 0.0005 {
                std::thread::sleep(Duration::from_secs_f64(wait));
            }
            continue;
        }
        frame_timer += delay;
        if delay > 0.0 && t - frame_timer > SYNC_THRESHOLD_MAX {
            frame_timer = t;
        }

        // Drop the frame if we're already late for the next one.
        if !instant
            && ctx.audio_is_master()
            && let Some((npts, nserial)) = next
            && nserial == fserial
        {
            let d = npts - pts;
            let dd = if d.is_finite() && d > 0.0 && d < MAX_FRAME_DURATION { d } else { dur } / speed;
            if t > frame_timer + dd {
                if let Some(vf) = ctx.frames.pop() {
                    last = Some((vf.pts, vf.duration, vf.serial));
                }
                prepared = None;
                dropped += 1;
                stats.dropped += 1;
                if let Some(mut st) = ctx.shared.state.try_lock()
                    && ctx.is_current()
                {
                    st.dropped_frames = dropped;
                }
                continue;
            }
        }

        ensure_prepared(&ctx, &mut conv, &mut prepared, serial, &mut stats);
        let Some(vf) = ctx.frames.pop() else { continue };
        let look = current_look(&ctx);
        let image = match prepared.take() {
            // Out of sight: keep time, skip the conversion.
            _ if look.2 => None,
            Some(p) if p.serial == vf.serial && p.pts.to_bits() == vf.pts.to_bits() && p.look == look => Some(p.image),
            _ => prepare(&ctx, &mut conv, &vf, &mut stats),
        };
        if vf.pts.is_finite() {
            ctx.vclk.set(vf.pts, vf.serial);
        }
        last = Some((vf.pts, vf.duration, vf.serial));
        if image.is_some() || look.2 {
            stats.shown += 1;
            shown_look = Some(look);
            if let Some(image) = image
                && ctx.is_current()
            {
                *ctx.shared.frame_slot.lock() = Some(image);
            }
            *ctx.last_frame.lock() = Some((vf.frame, vf.sar));
            if ctx.seeking.swap(false, Ordering::AcqRel) {
                ctx.update_state(|st| st.seeking = false);
            }
            ctx.wake();
        }
        if step {
            ctx.step.store(false, Ordering::Release);
        }
        if show_one {
            ctx.show_one.store(false, Ordering::Release);
        }
    }
}

fn ensure_prepared(
    ctx: &SessionCtx,
    conv: &mut Converter,
    prepared: &mut Option<Prepared>,
    serial: i32,
    stats: &mut Stats,
) {
    let head = ctx.frames.with_front(|a, _| a.map(|a| (a.pts, a.serial)));
    let Some((pts, s)) = head else { return };
    if s != serial {
        return;
    }
    let look = current_look(ctx);
    if look.2 {
        return;
    }
    if prepared.as_ref().is_some_and(|p| p.serial == s && p.pts.to_bits() == pts.to_bits() && p.look == look) {
        return;
    }
    // Convert outside the queue lock: clone a reference to the head frame.
    let fr = ctx.frames.with_front(|a, _| a.and_then(|a| a.frame.clone_ref().map(|f| (f, a.sar, a.pts, a.serial))));
    if let Some((f, sar, pts, serial)) = fr {
        let vf = VideoFrame { frame: f, pts, duration: 0.0, serial, sar };
        if let Some(image) = prepare(ctx, conv, &vf, stats) {
            *prepared = Some(Prepared { serial, pts, look, image });
        }
    }
}

fn prepare(ctx: &SessionCtx, conv: &mut Converter, vf: &VideoFrame, stats: &mut Stats) -> Option<PresentedFrame> {
    let (w, h) = (vf.frame.width(), vf.frame.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let target = ctx.controls.target_size();
    let (quality, upscaler) = {
        let c = ctx.controls.config.lock();
        (c.scale_quality, c.upscaler)
    };
    // The GPU enlarges with bilinear filtering, which would blur away the
    // hard edges nearest-neighbour is chosen for; enlarge here instead.
    // (Shader upscalers already made the picture display-sized.)
    let nearest = upscaler == Upscaler::Nearest;
    let (dw, dh) = output_size(w, h, vf.sar, target, nearest);
    let enlarging = dw > w as u32 || dh > h as u32;
    let quality = if nearest && enlarging { ScaleQuality::Nearest } else { quality };
    let t0 = std::time::Instant::now();
    let bgra = conv.convert(&vf.frame, dw, dh, PRESENT_PAD, quality);
    stats.converted += 1;
    stats.convert_time += t0.elapsed();
    stats.size = (dw, dh);
    let Some(bgra) = bgra else {
        log::debug!("video: converting a {w}x{h} frame to {dw}x{dh} failed");
        return None;
    };
    ctx.sub_canvas.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    Some(PresentedFrame { width: dw, height: dh, pad: PRESENT_PAD, bgra })
}

/// ffplay's `compute_target_delay`: stretch or shrink the wait so the video
/// clock converges on the master (audio) clock.
fn target_delay(ctx: &SessionCtx, delay: f64) -> f64 {
    if !ctx.audio_is_master() {
        return delay;
    }
    let master = ctx.master_clock() + ctx.controls.audio_delay.get();
    let diff = ctx.vclk.get(ctx.vq.serial()) - master;
    let thr = delay.clamp(SYNC_THRESHOLD_MIN, SYNC_THRESHOLD_MAX);
    if !diff.is_finite() || diff.abs() >= NOSYNC_THRESHOLD {
        return delay;
    }
    if diff <= -thr {
        (delay + diff).max(0.0)
    } else if diff >= thr && delay > FRAMEDUP_THRESHOLD {
        delay + diff
    } else if diff >= thr {
        2.0 * delay
    } else {
        delay
    }
}

/// Saves the last shown frame at native resolution as PNG.
pub fn screenshot_async(ctx: Arc<SessionCtx>, path: PathBuf) {
    let frame = ctx.last_frame.lock().as_ref().and_then(|(f, sar)| f.clone_ref().map(|f| (f, *sar)));
    let Some((frame, sar)) = frame else {
        ctx.emit(PlayerEvent::Warning("No picture to capture".into()));
        return;
    };
    std::thread::spawn(move || {
        let mut conv = Converter::new();
        let (w, h) = output_size(frame.width(), frame.height(), sar, (u32::MAX / 2, u32::MAX / 2), false);
        let Some(mut bgra) = conv.convert(&frame, w, h, 0, ScaleQuality::Best) else {
            ctx.emit(PlayerEvent::Warning("Screenshot failed".into()));
            return;
        };
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match image::RgbaImage::from_raw(w, h, bgra).map(|img| img.save(&path)) {
            Some(Ok(())) => ctx.emit(PlayerEvent::ScreenshotSaved(path)),
            Some(Err(e)) => ctx.emit(PlayerEvent::Warning(format!("Screenshot failed: {e}"))),
            None => ctx.emit(PlayerEvent::Warning("Screenshot failed".into())),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_shrinks_but_only_enlarges_when_asked() {
        // 1080p into a 960x540 area: halved either way.
        assert_eq!(output_size(1920, 1080, 1.0, (960, 540), false), (960, 540));
        assert_eq!(output_size(1920, 1080, 1.0, (960, 540), true), (960, 540));
        // 320x240 into 1280x960: left to the GPU, or enlarged 4x here.
        assert_eq!(output_size(320, 240, 1.0, (1280, 960), false), (320, 240));
        assert_eq!(output_size(320, 240, 1.0, (1280, 960), true), (1280, 960));
        // Anamorphic 720x480 (32:27 SAR) keeps its shape when enlarged.
        assert_eq!(output_size(720, 480, 32.0 / 27.0, (1706, 960), true), (1706, 960));
    }

    fn test_frame(fmt: ff::AVPixelFormat, w: i32, h: i32) -> Frame {
        let f = Frame::new();
        unsafe {
            (*f.0).width = w;
            (*f.0).height = h;
            (*f.0).format = fmt as c_int;
            assert!(ff::av_frame_get_buffer(f.0, 0) >= 0);
            // Pixel values don't matter for these tests; the luma plane gets
            // a mid grey so the conversion has something defined to read.
            std::ptr::write_bytes((*f.0).data[0], 120, (*f.0).linesize[0] as usize * h as usize);
        }
        f
    }

    #[test]
    fn conversion_output_is_opaque() {
        use ff::AVPixelFormat::*;
        for quality in [ScaleQuality::Nearest, ScaleQuality::Linear, ScaleQuality::Good, ScaleQuality::Best] {
            // YUV skips the alpha pass, packed RGB with padding doesn't.
            for fmt in [AV_PIX_FMT_YUV420P, AV_PIX_FMT_NV12, AV_PIX_FMT_YUV420P10LE, AV_PIX_FMT_GRAY8, AV_PIX_FMT_BGR0]
            {
                let f = test_frame(fmt, 64, 36);
                let mut conv = Converter::new();
                for (dw, dh) in [(64, 36), (32, 18), (128, 72)] {
                    let out = conv.convert(&f, dw, dh, 0, quality).unwrap_or_else(|| panic!("{fmt:?} {quality:?}"));
                    assert_eq!(out.len(), (dw * dh * 4) as usize);
                    assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{fmt:?} {quality:?} {dw}x{dh}");
                }
            }
        }
    }

    #[test]
    fn padded_conversion_repeats_the_edges() {
        let f = test_frame(ff::AVPixelFormat::AV_PIX_FMT_YUV420P, 64, 36);
        let mut conv = Converter::new();
        let (w, h, pad) = (48usize, 27usize, 2usize);
        let out = conv.convert(&f, w as u32, h as u32, pad as u32, ScaleQuality::Good).unwrap();
        let stride = w + 2 * pad;
        assert_eq!(out.len(), stride * (h + 2 * pad) * 4);
        let px = |x: usize, y: usize| &out[(y * stride + x) * 4..(y * stride + x) * 4 + 4];
        for y in 0..h + 2 * pad {
            for x in 0..stride {
                let (cx, cy) = (x.clamp(pad, pad + w - 1), y.clamp(pad, pad + h - 1));
                assert_eq!(px(x, y), px(cx, cy), "({x},{y})");
                assert_eq!(px(x, y)[3], 255);
            }
        }
    }

    #[test]
    fn extend_edges_copies_outermost_pixels() {
        // 2x2 picture with distinct pixels, 1-pixel border.
        let (w, h, pad) = (2, 2, 1);
        let stride = w + 2 * pad;
        let mut buf = vec![0u8; stride * (h + 2 * pad) * 4];
        let ids = [[1u8, 2], [3, 4]];
        for (y, row) in ids.iter().enumerate() {
            for (x, &v) in row.iter().enumerate() {
                buf[((y + pad) * stride + x + pad) * 4..][..4].copy_from_slice(&[v; 4]);
            }
        }
        extend_edges(&mut buf, w, h, pad);
        let got: Vec<u8> = buf.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(got, vec![1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn old_fast_scaling_setting_still_loads() {
        let q: ScaleQuality = serde_json::from_str("\"Fast\"").unwrap();
        assert_eq!(q, ScaleQuality::Linear);
        let q: ScaleQuality = serde_json::from_str("\"Nearest\"").unwrap();
        assert_eq!(q, ScaleQuality::Nearest);
    }
}
