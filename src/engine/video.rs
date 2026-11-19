//! Video decode thread: decode → (optional) libavfilter chain → frame queue.

use super::SessionCtx;
use super::av::{self, FormatCtx, Frame, ff};
use super::decoder::{DecodeResult, Decoder};
use super::filters::Graph;
use super::sync::VideoFrame;
use super::types::*;
use std::os::raw::c_int;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct StreamInfo {
    pub width: i32,
    pub height: i32,
    pub sar: f64,
    pub fps: f64,
    pub time_base: ff::AVRational,
    pub frame_rate: ff::AVRational,
    pub attached_pic: bool,
    /// Clockwise rotation from the container's display matrix, in degrees.
    pub rotation: i32,
}

impl StreamInfo {
    pub fn from_stream(ic: &FormatCtx, idx: usize) -> Self {
        unsafe {
            let st = ic.stream(idx);
            let par = &*(*st).codecpar;
            let sar = ff::av_guess_sample_aspect_ratio(ic.0, st, ptr::null_mut());
            let fr = ff::av_guess_frame_rate(ic.0, st, ptr::null_mut());
            let mut rotation = 0;
            let sd = ff::av_packet_side_data_get(
                par.coded_side_data,
                par.nb_coded_side_data,
                ff::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
            );
            if !sd.is_null() && (*sd).size >= 36 {
                let theta = -ff::av_display_rotation_get((*sd).data as *const i32);
                if theta.is_finite() {
                    rotation = ((theta.round() as i32 % 360) + 360) % 360;
                    rotation = ((rotation + 45) / 90 * 90) % 360;
                }
            }
            StreamInfo {
                width: par.width,
                height: par.height,
                sar: if sar.num > 0 && sar.den > 0 { av::q2d(sar) } else { 1.0 },
                fps: av::q2d(fr),
                time_base: (*st).time_base,
                frame_rate: fr,
                attached_pic: (*st).disposition & ff::AV_DISPOSITION_ATTACHED_PIC as c_int != 0,
                rotation,
            }
        }
    }

    pub fn display_aspect(&self) -> f64 {
        if self.height <= 0 {
            return 16.0 / 9.0;
        }
        let a = self.width as f64 * self.sar / self.height as f64;
        if self.rotation == 90 || self.rotation == 270 { 1.0 / a } else { a }
    }
}

fn is_hdr(f: &Frame) -> bool {
    let trc = f.raw().color_trc as c_int;
    trc == ff::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084 as c_int
        || trc == ff::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67 as c_int
}

/// The libavfilter chain for the current settings ("" = none needed).
fn build_chain(
    adj: &VideoAdjust,
    cfg: &EngineConfig,
    info: &StreamInfo,
    frame: &Frame,
    interlaced: bool,
    target: (u32, u32),
) -> String {
    let mut f: Vec<String> = Vec::new();
    match cfg.deinterlace {
        Deinterlace::Off => {}
        Deinterlace::Auto if !interlaced => {}
        mode => {
            let deint = if mode == Deinterlace::On { "all" } else { "interlaced" };
            if av::filter_available("bwdif") {
                f.push(format!("bwdif=mode=send_frame:parity=auto:deint={deint}"));
            } else if av::filter_available("yadif") {
                f.push(format!("yadif=mode=send_frame:parity=auto:deint={deint}"));
            }
        }
    }
    if cfg.tone_map_hdr && is_hdr(frame) && av::filter_available("zscale") && av::filter_available("tonemap") {
        // Tone mapping in linear float is expensive: do it at display size.
        let sar = {
            let s = frame.raw().sample_aspect_ratio;
            if s.num > 0 && s.den > 0 { av::q2d(s) } else { info.sar }
        };
        let (dw, dh) = super::present::output_size(frame.width(), frame.height(), sar, target);
        let (dw, dh) = (dw & !1, dh & !1);
        if dw < frame.width() as u32 || dh < frame.height() as u32 {
            f.push(format!("zscale=w={dw}:h={dh}:f=bilinear,setsar=1"));
        }
        f.push(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
                .into(),
        );
    }
    if let Some([l, t, r, b]) = adj.crop {
        let (l, t, r, b) = (l.clamp(0.0, 0.45), t.clamp(0.0, 0.45), r.clamp(0.0, 0.45), b.clamp(0.0, 0.45));
        f.push(format!(
            "crop=w=trunc(iw*{w}/2)*2:h=trunc(ih*{h}/2)*2:x=iw*{l}:y=ih*{t}",
            w = 1.0 - l - r,
            h = 1.0 - t - b
        ));
    }
    let user_rot = match adj.rotation {
        Rotation::None => 0,
        Rotation::R90 => 90,
        Rotation::R180 => 180,
        Rotation::R270 => 270,
    };
    match (info.rotation + user_rot) % 360 {
        90 => f.push("transpose=clock".into()),
        180 => f.push("hflip,vflip".into()),
        270 => f.push("transpose=cclock".into()),
        _ => {}
    }
    if adj.hflip {
        f.push("hflip".into());
    }
    if adj.vflip {
        f.push("vflip".into());
    }
    if adj.denoise {
        f.push("hqdn3d".into());
    }
    let eq_needed = adj.brightness.abs() > 0.001
        || (adj.contrast - 1.0).abs() > 0.001
        || (adj.saturation - 1.0).abs() > 0.001
        || (adj.gamma - 1.0).abs() > 0.001;
    if eq_needed {
        f.push(format!(
            "eq=brightness={:.3}:contrast={:.3}:saturation={:.3}:gamma={:.3}",
            adj.brightness, adj.contrast, adj.saturation, adj.gamma
        ));
    }
    if adj.hue.abs() > 0.5 || adj.grayscale {
        let s = if adj.grayscale { 0.0 } else { 1.0 };
        f.push(format!("hue=h={:.1}:s={s}", adj.hue));
    }
    if adj.sharpen > 0.01 {
        f.push(format!("unsharp=5:5:{:.2}:5:5:0", adj.sharpen.min(3.0)));
    }
    if adj.negate {
        f.push("negate".into());
    }
    f.join(",")
}

#[derive(PartialEq, Clone)]
struct GraphKey {
    w: i32,
    h: i32,
    fmt: c_int,
    chain: String,
    sar: (i32, i32),
}

pub fn run_decoder(ctx: Arc<SessionCtx>, mut dec: Decoder, info: StreamInfo, stop: Arc<AtomicBool>) {
    let mut frame = Frame::new();
    let mut filtered = Frame::new();
    let mut graph: Option<(GraphKey, Graph)> = None;
    let mut adj_gen = u64::MAX;
    let mut adj = VideoAdjust::default();
    let frame_dur =
        if info.frame_rate.num > 0 && info.frame_rate.den > 0 { 1.0 / av::q2d(info.frame_rate) } else { 0.0 };
    let mut reported_fmt = false;
    let mut saw_interlaced = false;
    let mut cfg = ctx.controls.config.lock().clone();
    let mut cfg_check = 0u32;

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        match dec.decode(&mut frame) {
            DecodeResult::Aborted => break,
            DecodeResult::Eof => {
                ctx.video_finished.store(dec.finished, Ordering::Release);
                ctx.wake_demuxer();
                continue;
            }
            DecodeResult::Frame => {}
        }
        let serial = dec.pkt_serial;
        let tb = info.time_base;
        let ts = frame.raw().best_effort_timestamp;
        let pts = av::ts_to_secs(ts, tb);
        unsafe { (*frame.0).pts = ts };

        // Exact seek: skip frames before the target.
        if let Some((target, s)) = *ctx.seek_target.lock()
            && s == serial
            && pts.is_finite()
            && pts + frame_dur.max(0.001) * 0.5 < target
        {
            frame.unref();
            continue;
        }

        if !reported_fmt {
            reported_fmt = true;
            let (pf, cs, hdr) = (av::pix_fmt_name(frame.format()), color_desc(&frame), is_hdr(&frame));
            ctx.update_state(|st| {
                if let Some(v) = st.video.as_mut() {
                    v.pix_fmt = pf;
                    v.color = cs;
                    v.hdr = hdr;
                }
            });
        }

        {
            let g = ctx.controls.video_adjust.lock();
            if g.0 != adj_gen {
                adj_gen = g.0;
                adj = g.1.clone();
            }
        }
        cfg_check = cfg_check.wrapping_add(1);
        if cfg_check.is_multiple_of(30) {
            cfg = ctx.controls.config.lock().clone();
        }
        saw_interlaced |= frame.raw().flags & ff::AV_FRAME_FLAG_INTERLACED as c_int != 0;
        let chain = build_chain(&adj, &cfg, &info, &frame, saw_interlaced, ctx.controls.target_size());

        if chain.is_empty() {
            graph = None;
            let sar = frame_sar(&frame, &info);
            let dur = frame_duration(&frame, tb, frame_dur);
            let vf = VideoFrame { frame: frame.take(), pts, duration: dur, serial, sar };
            if !push(&ctx, &dec, vf) {
                continue;
            }
            continue;
        }

        let key = GraphKey {
            w: frame.width(),
            h: frame.height(),
            fmt: frame.format(),
            chain: chain.clone(),
            sar: unsafe { ((*frame.0).sample_aspect_ratio.num, (*frame.0).sample_aspect_ratio.den) },
        };
        if graph.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            graph = match make_graph(&frame, &info, &chain) {
                Ok(g) => Some((key, g)),
                Err(e) => {
                    log::warn!("video filters disabled: {e}");
                    ctx.emit(PlayerEvent::Warning(format!("Video filter error: {e}")));
                    None
                }
            };
        }
        let Some((_, g)) = graph.as_mut() else {
            let sar = frame_sar(&frame, &info);
            let dur = frame_duration(&frame, tb, frame_dur);
            let vf = VideoFrame { frame: frame.take(), pts, duration: dur, serial, sar };
            push(&ctx, &dec, vf);
            continue;
        };
        if let Err(e) = g.push(&mut frame) {
            log::debug!("{e}");
            graph = None;
            frame.unref();
            continue;
        }
        frame.unref();
        let otb = g.out_time_base();
        while g.pull(&mut filtered) {
            let p = av::ts_to_secs(filtered.raw().pts, otb);
            let sar = frame_sar(&filtered, &info);
            let dur = frame_duration(&filtered, otb, frame_dur);
            let vf = VideoFrame {
                frame: filtered.take(),
                pts: if p.is_finite() { p } else { pts },
                duration: dur,
                serial,
                sar,
            };
            if !push(&ctx, &dec, vf) {
                break;
            }
        }
    }
}

fn push(ctx: &SessionCtx, dec: &Decoder, vf: VideoFrame) -> bool {
    let q = dec.queue.clone();
    let ok = ctx.frames.push(vf, |s| q.serial() != s);
    if let Some(mut st) = ctx.shared.state.try_lock() {
        st.decoded_frames += 1;
    }
    ok
}

fn frame_sar(f: &Frame, info: &StreamInfo) -> f64 {
    let s = f.raw().sample_aspect_ratio;
    if s.num > 0 && s.den > 0 { av::q2d(s) } else { info.sar }
}

fn frame_duration(f: &Frame, tb: ff::AVRational, fallback: f64) -> f64 {
    let d = f.raw().duration;
    if d > 0 { d as f64 * av::q2d(tb) } else { fallback }
}

fn make_graph(frame: &Frame, info: &StreamInfo, chain: &str) -> anyhow::Result<Graph> {
    let f = frame.raw();
    let sar = if f.sample_aspect_ratio.num > 0 { f.sample_aspect_ratio } else { ff::AVRational { num: 1, den: 1 } };
    let tb = info.time_base;
    let fr = if info.frame_rate.num > 0 { info.frame_rate } else { ff::AVRational { num: 25, den: 1 } };
    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}:frame_rate={}/{}:colorspace={}:range={}",
        f.width,
        f.height,
        f.format,
        tb.num,
        tb.den,
        sar.num,
        sar.den,
        fr.num,
        fr.den,
        f.colorspace as c_int,
        f.color_range as c_int
    );
    let threads = std::thread::available_parallelism().map(|n| n.get().min(8) as i32).unwrap_or(4);
    Graph::new("buffer", &args, "buffersink", chain, threads)
}

fn color_desc(f: &Frame) -> String {
    unsafe {
        let r = f.raw();
        let cs = av::from_cstr(ff::av_color_space_name(r.colorspace)).unwrap_or_default();
        let pr = av::from_cstr(ff::av_color_primaries_name(r.color_primaries)).unwrap_or_default();
        let trc = av::from_cstr(ff::av_color_transfer_name(r.color_trc)).unwrap_or_default();
        let range = av::from_cstr(ff::av_color_range_name(r.color_range)).unwrap_or_default();
        let known = |v: &String| !v.is_empty() && v != "unknown" && v != "reserved" && v != "unspecified";
        let mut parts: Vec<String> = [cs, pr, trc].into_iter().filter(known).collect();
        parts.dedup();
        let range = match range.as_str() {
            "tv" => "limited range",
            "pc" => "full range",
            _ => "",
        };
        match (parts.is_empty(), range.is_empty()) {
            (true, true) => String::new(),
            (true, false) => range.to_string(),
            (false, true) => parts.join("/"),
            (false, false) => format!("{} ({range})", parts.join("/")),
        }
    }
}
