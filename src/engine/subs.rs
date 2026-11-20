//! Subtitles: decoding embedded streams, loading external files, and
//! answering "what is on screen at time t".

use super::SessionCtx;
use super::av::{self, Packet, ff};
use super::decoder::OpenedCodec;
use super::demux;
use super::sync::PacketQueue;
use super::types::*;
use parking_lot::Mutex;
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static BITMAP_IDS: AtomicU64 = AtomicU64::new(1);

/// Largest subtitle bitmap accepted (a full 8K frame).
const MAX_SUB_BITMAP_PIXELS: u64 = 7680 * 4320;

struct ExternalTrack {
    /// Sorted by start time.
    events: Arc<Vec<SubEvent>>,
    /// The longest event, which bounds how far back a lookup must search.
    max_len: f64,
}

pub struct SubStore {
    embedded: Mutex<Vec<SubEvent>>,
    external: Mutex<Vec<ExternalTrack>>,
    active_external: Mutex<Option<usize>>,
}

impl SubStore {
    pub fn new() -> Self {
        Self { embedded: Mutex::new(Vec::new()), external: Mutex::new(Vec::new()), active_external: Mutex::new(None) }
    }

    pub fn clear_embedded(&self) {
        self.embedded.lock().clear();
    }

    pub fn select_external(&self, i: Option<usize>) {
        *self.active_external.lock() = i;
    }

    fn add_event(&self, ev: SubEvent) {
        let mut g = self.embedded.lock();
        // Bitmap formats (PGS, DVD, DVB) replace the previous picture.
        if matches!(ev.content, SubContent::Bitmap(_)) {
            for old in g.iter_mut().rev().take(8) {
                if matches!(old.content, SubContent::Bitmap(_)) && old.end > ev.start && old.start < ev.start {
                    old.end = ev.start;
                }
            }
        }
        g.push(ev);
        if g.len() > 4000 {
            g.drain(..1000);
        }
    }

    /// A "clear screen" packet (bitmap subtitle with no rectangles).
    fn end_bitmaps_at(&self, t: f64) {
        let mut g = self.embedded.lock();
        for old in g.iter_mut().rev().take(8) {
            if old.end > t && old.start <= t {
                old.end = t;
            }
        }
    }

    /// Events visible at media time `t` (seconds from the start of the media).
    /// Embedded events use absolute stream timestamps (offset by
    /// `start_time`); external files are already relative to the start.
    pub fn active_at(&self, t: f64, start_time: f64, out: &mut Vec<SubEvent>) {
        let ext = *self.active_external.lock();
        if let Some(i) = ext {
            let tracks = self.external.lock();
            if let Some(tr) = tracks.get(i) {
                // Sorted by start time: only events that already began, and
                // no earlier than the longest one could still be showing.
                let evs = &tr.events;
                let idx = evs.partition_point(|e| e.start <= t);
                for e in evs[..idx].iter().rev().take_while(|e| e.start >= t - tr.max_len) {
                    if t < e.end {
                        out.push(e.clone());
                    }
                }
                out.reverse();
            }
        } else {
            let abs = t + start_time;
            for e in self.embedded.lock().iter() {
                if e.start <= abs && abs < e.end {
                    let mut e = e.clone();
                    e.start -= start_time;
                    e.end -= start_time;
                    out.push(e);
                }
            }
        }
    }
}

/// Removes ASS override blocks and converts ASS line breaks.
pub fn ass_to_plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            '\\' => match chars.peek() {
                Some('N') | Some('n') => {
                    chars.next();
                    out.push('\n');
                }
                Some('h') => {
                    chars.next();
                    out.push('\u{a0}');
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// ASS event line → text: skip the first 8 comma-separated fields.
fn ass_event_text(line: &str) -> String {
    let line = line.strip_prefix("Dialogue:").unwrap_or(line);
    let mut it = line.splitn(10, ',');
    // Decoder output: ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
    let parts: Vec<&str> = it.by_ref().collect();
    let text = if parts.len() >= 9 { parts[8..].join(",") } else { line.to_string() };
    ass_to_plain(&text)
}

unsafe fn convert_subtitle(sub: &ff::AVSubtitle, start: f64, end: f64, canvas: (u32, u32)) -> Option<SubEvent> {
    let mut texts: Vec<String> = Vec::new();
    let mut bitmaps: Vec<SubBitmap> = Vec::new();
    for i in 0..sub.num_rects as usize {
        let r = unsafe { &**sub.rects.add(i) };
        match r.type_ {
            ff::AVSubtitleType::SUBTITLE_BITMAP => {
                // Guard against corrupt streams: the reads below trust these.
                if r.w <= 0
                    || r.h <= 0
                    || r.data[0].is_null()
                    || r.data[1].is_null()
                    || r.linesize[0] < r.w
                    || r.w as u64 * r.h as u64 > MAX_SUB_BITMAP_PIXELS
                {
                    continue;
                }
                let (w, h) = (r.w as usize, r.h as usize);
                let pal = r.data[1] as *const u32;
                // The palette is AVPALETTE_COUNT (256) entries.
                let nb_colors = r.nb_colors.clamp(0, 256) as usize;
                let mut lut = [0u32; 256];
                for (i, c) in lut.iter_mut().enumerate().take(nb_colors) {
                    *c = unsafe { *pal.add(i) };
                }
                let mut bgra = vec![0u8; w * h * 4];
                for (y, out_row) in bgra.chunks_exact_mut(w * 4).enumerate() {
                    let row = unsafe { std::slice::from_raw_parts(r.data[0].add(y * r.linesize[0] as usize), w) };
                    for (px, &idx) in out_row.as_chunks_mut::<4>().0.iter_mut().zip(row) {
                        *px = lut[idx as usize].to_le_bytes();
                    }
                }
                bitmaps.push(SubBitmap {
                    id: BITMAP_IDS.fetch_add(1, Ordering::Relaxed),
                    x: r.x,
                    y: r.y,
                    w: w as u32,
                    h: h as u32,
                    canvas_w: canvas.0,
                    canvas_h: canvas.1,
                    bgra,
                });
            }
            ff::AVSubtitleType::SUBTITLE_ASS => {
                if let Some(s) = unsafe { av::from_cstr(r.ass) } {
                    let t = ass_event_text(&s);
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
            }
            ff::AVSubtitleType::SUBTITLE_TEXT => {
                if let Some(s) = unsafe { av::from_cstr(r.text) } {
                    let t = s.trim().to_string();
                    if !t.is_empty() {
                        texts.push(t);
                    }
                }
            }
            _ => {}
        }
    }
    if !bitmaps.is_empty() {
        Some(SubEvent { start, end, content: SubContent::Bitmap(Arc::new(bitmaps)) })
    } else if !texts.is_empty() {
        Some(SubEvent { start, end, content: SubContent::Text(texts.join("\n")) })
    } else {
        None
    }
}

/// Decodes one packet; returns the event and whether it was a bitmap "clear".
unsafe fn decode_packet(
    ctx: *mut ff::AVCodecContext,
    pkt: &Packet,
    tb: ff::AVRational,
    canvas: (u32, u32),
) -> (Option<SubEvent>, Option<f64>) {
    unsafe {
        let mut sub: ff::AVSubtitle = std::mem::zeroed();
        let mut got: c_int = 0;
        let r = ff::avcodec_decode_subtitle2(ctx, &mut sub, &mut got, pkt.0);
        if r < 0 || got == 0 {
            return (None, None);
        }
        let base =
            if sub.pts != av::NOPTS { sub.pts as f64 / ff::AV_TIME_BASE as f64 } else { av::ts_to_secs(pkt.pts(), tb) };
        let base = if base.is_finite() { base } else { 0.0 };
        let start = base + sub.start_display_time as f64 / 1000.0;
        let pkt_dur = if pkt.duration() > 0 { pkt.duration() as f64 * av::q2d(tb) } else { 0.0 };
        let end = if sub.end_display_time > 0 && sub.end_display_time != u32::MAX {
            base + sub.end_display_time as f64 / 1000.0
        } else if pkt_dur > 0.0 {
            base + pkt_dur
        } else {
            f64::INFINITY
        };
        let is_bitmap_codec = (*(*ctx).codec_descriptor).props & ff::AV_CODEC_PROP_BITMAP_SUB as c_int != 0;
        let res = if sub.num_rects == 0 && is_bitmap_codec {
            (None, Some(start))
        } else {
            (convert_subtitle(&sub, start, end, canvas), None)
        };
        ff::avsubtitle_free(&mut sub);
        res
    }
}

fn canvas_of(ctx: *mut ff::AVCodecContext, sess: &SessionCtx) -> (u32, u32) {
    let (w, h) = unsafe { ((*ctx).width, (*ctx).height) };
    if w > 0 && h > 0 {
        return (w as u32, h as u32);
    }
    let v = sess.sub_canvas.load(Ordering::Relaxed);
    let (vw, vh) = ((v >> 32) as u32, v as u32);
    if vw > 0 { (vw, vh) } else { (720, 576) }
}

pub fn run_decoder(
    ctx: Arc<SessionCtx>,
    codec: OpenedCodec,
    q: Arc<PacketQueue>,
    tb: ff::AVRational,
    stop: Arc<AtomicBool>,
) {
    let mut serial = -1;
    while ctx.alive() && !stop.load(Ordering::Acquire) {
        let Some(qp) = q.get(&stop) else { break };
        if qp.serial != serial {
            serial = qp.serial;
            ctx.subs.clear_embedded();
            unsafe { ff::avcodec_flush_buffers(codec.ctx.0) };
        }
        if qp.pkt.is_empty() {
            continue;
        }
        let canvas = canvas_of(codec.ctx.0, &ctx);
        let (ev, clear) = unsafe { decode_packet(codec.ctx.0, &qp.pkt, tb, canvas) };
        if let Some(t) = clear {
            ctx.subs.end_bitmaps_at(t);
        }
        if let Some(ev) = ev {
            ctx.subs.add_event(ev);
        }
    }
}

/// Loads a whole subtitle file into memory.
pub fn load_file(path: &Path, cfg: &EngineConfig) -> anyhow::Result<Vec<SubEvent>> {
    let ic = demux::open_input(&MediaSource::File(path.to_path_buf()), cfg, None)?;
    let idx =
        unsafe { ff::av_find_best_stream(ic.0, ff::AVMediaType::AVMEDIA_TYPE_SUBTITLE, -1, -1, ptr::null_mut(), 0) };
    if idx < 0 {
        anyhow::bail!("no subtitles in {}", path.display());
    }
    let st = ic.stream(idx as usize);
    let charset = cfg.sub_charset.clone().or_else(|| detect_charset(path));
    let codec = super::decoder::open_codec(st, 1, None, charset.as_deref())?;
    let tb = unsafe { (*st).time_base };
    let canvas = {
        let (w, h) = unsafe { ((*codec.ctx.0).width, (*codec.ctx.0).height) };
        if w > 0 { (w as u32, h as u32) } else { (720, 576) }
    };
    let mut events: Vec<SubEvent> = Vec::new();
    let pkt = Packet::new();
    loop {
        let r = unsafe { ff::av_read_frame(ic.0, pkt.0) };
        if r < 0 {
            break;
        }
        if pkt.stream_index() == idx {
            let (ev, clear) = unsafe { decode_packet(codec.ctx.0, &pkt, tb, canvas) };
            if let Some(t) = clear {
                for e in events.iter_mut().rev().take(4) {
                    if e.end > t {
                        e.end = t;
                    }
                }
            }
            if let Some(ev) = ev {
                if matches!(ev.content, SubContent::Bitmap(_)) {
                    for e in events.iter_mut().rev().take(4) {
                        if e.end > ev.start && matches!(e.content, SubContent::Bitmap(_)) {
                            e.end = ev.start;
                        }
                    }
                }
                events.push(ev);
            }
        }
        unsafe { ff::av_packet_unref(pkt.0) };
    }
    events.sort_by(|a, b| a.start.total_cmp(&b.start));
    for e in events.iter_mut() {
        if !e.end.is_finite() {
            e.end = e.start + 5.0;
        }
    }
    Ok(events)
}

/// Rough charset sniffing for text subtitles: UTF-8/UTF-16 are recognised by
/// FFmpeg itself; for anything else fall back to Windows-1252.
fn detect_charset(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if !matches!(ext.as_str(), "srt" | "sub" | "txt" | "ssa" | "ass" | "smi") {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) || data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]) {
        return None;
    }
    if std::str::from_utf8(&data).is_ok() {
        return None;
    }
    Some("CP1252".into())
}

pub fn load_external_async(ctx: Arc<SessionCtx>, path: PathBuf, select: bool) {
    std::thread::Builder::new()
        .name("sub-load".into())
        .spawn(move || {
            let cfg = ctx.controls.config.lock().clone();
            match load_file(&path, &cfg) {
                Ok(events) => {
                    if !ctx.alive() {
                        return;
                    }
                    let idx = {
                        let mut ext = ctx.subs.external.lock();
                        let max_len = events.iter().map(|e| e.end - e.start).fold(0.0, f64::max);
                        ext.push(ExternalTrack { events: Arc::new(events), max_len });
                        ext.len() - 1
                    };
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    let lang = guess_lang_from_name(&path);
                    ctx.update_state(|st| {
                        if let Some(m) = st.media.as_mut() {
                            let m = Arc::make_mut(m);
                            m.tracks.push(Track {
                                id: TrackId::External(idx),
                                kind: TrackKind::Subtitle,
                                codec: path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default(),
                                language: lang,
                                title: name,
                                default: false,
                                forced: false,
                                hearing_impaired: false,
                                width: 0,
                                height: 0,
                                fps: 0.0,
                                is_cover_art: false,
                                channels: 0,
                                channel_layout: String::new(),
                                sample_rate: 0,
                                bitrate: 0,
                                external_path: Some(path.clone()),
                            });
                        }
                    });
                    if select {
                        ctx.subs.select_external(Some(idx));
                        ctx.switch_req.lock().push((TrackKind::Subtitle, None));
                        ctx.update_state(|st| st.selected.subtitle = Some(TrackId::External(idx)));
                        ctx.wake_demuxer();
                    }
                    ctx.emit(PlayerEvent::TracksChanged);
                }
                Err(e) => ctx.emit(PlayerEvent::Warning(format!("Could not load subtitles: {e}"))),
            }
        })
        .ok();
}

fn guess_lang_from_name(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy().to_string();
    let last = stem.rsplit('.').next()?;
    (last.len() == 2 || last.len() == 3)
        .then(|| last.to_string())
        .filter(|l| l.chars().all(|c| c.is_ascii_alphabetic()))
}

pub const SUB_EXTS: &[&str] = &["srt", "ass", "ssa", "vtt", "sub", "idx", "smi", "sup", "ttml", "lrc"];

/// Finds `movie.srt`, `movie.en.srt`, `Subs/movie.srt` and friends.
pub fn find_sidecar_subtitles(media: &Path) -> Vec<PathBuf> {
    let Some(dir) = media.parent() else { return vec![] };
    let Some(stem) = media.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else { return vec![] };
    let mut found = Vec::new();
    let mut scan = |d: &Path, any_name: bool| {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                let Some(ext) = p.extension().map(|x| x.to_string_lossy().to_lowercase()) else { continue };
                if !SUB_EXTS.contains(&ext.as_str()) || p == media {
                    continue;
                }
                let name = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
                if any_name || name == stem || name.starts_with(&format!("{stem}.")) {
                    // `.sub` next to an `.idx` is the VobSub data file, not a track.
                    if ext == "sub" && p.with_extension("idx").exists() {
                        continue;
                    }
                    found.push(p);
                }
            }
        }
    };
    scan(dir, false);
    for sub in ["Subs", "subs", "Subtitles", "subtitles"] {
        let d = dir.join(sub);
        if d.is_dir() {
            scan(&d, false);
            let d2 = d.join(media.file_stem().unwrap_or_default());
            if d2.is_dir() {
                scan(&d2, true);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}
