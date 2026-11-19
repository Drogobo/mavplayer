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

/// Cached swscale context for YUV → BGRA (+ scale) conversion.
pub struct Converter {
    sws: *mut ff::SwsContext,
    key: (i32, i32, c_int, u32, u32, c_int, c_int, i32),
}
unsafe impl Send for Converter {}

impl Converter {
    pub fn new() -> Self {
        Self { sws: ptr::null_mut(), key: (0, 0, -1, 0, 0, -1, -1, 0) }
    }

    /// Converts `f` to tightly-packed BGRA of `dw`×`dh`.
    pub fn convert(&mut self, f: &Frame, dw: u32, dh: u32, quality: ScaleQuality) -> Option<Vec<u8>> {
        let r = f.raw();
        let flags = match quality {
            ScaleQuality::Fast => ff::SwsFlags::SWS_BILINEAR,
            ScaleQuality::Good => ff::SwsFlags::SWS_BICUBIC,
            ScaleQuality::Best => ff::SwsFlags::SWS_LANCZOS,
        } as c_int
            | ff::SwsFlags::SWS_ACCURATE_RND as c_int
            | ff::SwsFlags::SWS_FULL_CHR_H_INT as c_int;
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
            }
            let stride = dw as usize * 4;
            let mut out = vec![0u8; stride * dh as usize];
            let dst_data = [out.as_mut_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
            let dst_stride = [stride as c_int, 0, 0, 0];
            let h = ff::sws_scale(
                self.sws,
                r.data.as_ptr() as *const *const u8,
                r.linesize.as_ptr(),
                0,
                r.height,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if h <= 0 {
                return None;
            }
            // Force opaque alpha (some sources leave it undefined).
            for px in out.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
            Some(out)
        }
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

/// Output size: square pixels, never larger than the target (or the source).
pub fn output_size(w: i32, h: i32, sar: f64, target: (u32, u32)) -> (u32, u32) {
    let sar = if sar.is_finite() && sar > 0.0 { sar } else { 1.0 };
    let (mut dw, mut dh) =
        if sar >= 1.0 { ((w as f64 * sar).round(), h as f64) } else { (w as f64, (h as f64 / sar).round()) };
    let (tw, th) = (target.0.max(16) as f64, target.1.max(16) as f64);
    let scale = (tw / dw).min(th / dh).min(1.0);
    dw = (dw * scale).round().max(2.0);
    dh = (dh * scale).round().max(2.0);
    (dw as u32, dh as u32)
}

struct Prepared {
    serial: i32,
    pts: f64,
    image: PresentedFrame,
}

pub fn run(ctx: Arc<SessionCtx>, stop: Arc<AtomicBool>) {
    let mut conv = Converter::new();
    let mut frame_timer = now();
    let mut last: Option<(f64, f64, i32)> = None; // (pts, duration, serial)
    let mut prepared: Option<Prepared> = None;
    let mut dropped: u64 = 0;

    while ctx.alive() && !stop.load(Ordering::Acquire) {
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

        // While the audio device is still opening, show the first picture
        // straight away but hold the rest so audio and video start together.
        let shown_this_serial = last.map(|l| l.2) == Some(fserial);
        let audio_pending = ctx.audio_pending();
        let hold = paused || (audio_pending && shown_this_serial);
        if hold && !step && !show_one {
            // Keep the next picture converted so un-pausing is instant.
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial);
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

        let instant = step || show_one || (audio_pending && !shown_this_serial);
        let delay = if instant { 0.0 } else { target_delay(&ctx, last_duration) };
        let t = now();
        if !instant && t < frame_timer + delay {
            ensure_prepared(&ctx, &mut conv, &mut prepared, serial);
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
                if let Some(mut st) = ctx.shared.state.try_lock()
                    && ctx.is_current()
                {
                    st.dropped_frames = dropped;
                }
                continue;
            }
        }

        ensure_prepared(&ctx, &mut conv, &mut prepared, serial);
        let Some(vf) = ctx.frames.pop() else { continue };
        let image = match prepared.take() {
            Some(p) if p.serial == vf.serial && p.pts.to_bits() == vf.pts.to_bits() => Some(p.image),
            _ => prepare(&ctx, &mut conv, &vf),
        };
        if vf.pts.is_finite() {
            ctx.vclk.set(vf.pts, vf.serial);
        }
        last = Some((vf.pts, vf.duration, vf.serial));
        if let Some(image) = image {
            if ctx.is_current() {
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

fn ensure_prepared(ctx: &SessionCtx, conv: &mut Converter, prepared: &mut Option<Prepared>, serial: i32) {
    let head = ctx.frames.with_front(|a, _| a.map(|a| (a.pts, a.serial)));
    let Some((pts, s)) = head else { return };
    if s != serial {
        return;
    }
    if prepared.as_ref().is_some_and(|p| p.serial == s && p.pts.to_bits() == pts.to_bits()) {
        return;
    }
    // Convert outside the queue lock: clone a reference to the head frame.
    let fr = ctx.frames.with_front(|a, _| a.and_then(|a| a.frame.clone_ref().map(|f| (f, a.sar, a.pts, a.serial))));
    if let Some((f, sar, pts, serial)) = fr {
        let vf = VideoFrame { frame: f, pts, duration: 0.0, serial, sar };
        if let Some(image) = prepare(ctx, conv, &vf) {
            *prepared = Some(Prepared { serial, pts, image });
        }
    }
}

fn prepare(ctx: &SessionCtx, conv: &mut Converter, vf: &VideoFrame) -> Option<PresentedFrame> {
    let (w, h) = (vf.frame.width(), vf.frame.height());
    if w <= 0 || h <= 0 {
        return None;
    }
    let target = ctx.controls.target_size();
    let (dw, dh) = output_size(w, h, vf.sar, target);
    let quality = ctx.controls.config.lock().scale_quality;
    let bgra = conv.convert(&vf.frame, dw, dh, quality)?;
    ctx.sub_canvas.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    Some(PresentedFrame { width: dw, height: dh, bgra })
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
        let (w, h) = output_size(frame.width(), frame.height(), sar, (u32::MAX / 2, u32::MAX / 2));
        let Some(mut bgra) = conv.convert(&frame, w, h, ScaleQuality::Best) else {
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
