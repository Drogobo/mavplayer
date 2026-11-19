//! Audio: decode → libavfilter (effects, tempo, format conversion) → sample
//! buffer → cpal device callback. The callback drives the master clock.

use super::SessionCtx;
use super::av::{self, Frame, ff};
use super::decoder::{DecodeResult, Decoder};
use super::filters::Graph;
use super::sync::now;
use super::types::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::os::raw::c_int;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct OutFormat {
    pub rate: u32,
    pub channels: u16,
    pub device: String,
}

struct Chunk {
    data: Vec<f32>,
    pos: usize,
    pts: f64,
    speed: f64,
    serial: i32,
}

struct AbInner {
    chunks: VecDeque<Chunk>,
    samples: usize,
    serial: i32,
}

/// Decoded, device-ready samples waiting for the audio callback.
pub struct AudioBuffer {
    inner: Mutex<AbInner>,
    cv: Condvar,
    abort: AtomicBool,
}

pub struct Filled {
    /// (media pts of first written sample, serial, speed)
    pub first: Option<(f64, i32, f64)>,
    /// Media pts just after the last written sample.
    pub end_pts: Option<f64>,
    pub frames: usize,
}

impl AudioBuffer {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AbInner { chunks: VecDeque::new(), samples: 0, serial: 0 }),
            cv: Condvar::new(),
            abort: AtomicBool::new(false),
        }
    }

    fn push(&self, c: Chunk, max_samples: usize, stop: &AtomicBool, live_serial: impl Fn() -> i32) -> bool {
        let mut g = self.inner.lock();
        while g.samples >= max_samples {
            if self.abort.load(Ordering::Acquire) || stop.load(Ordering::Acquire) || live_serial() != c.serial {
                return false;
            }
            self.cv.wait_for(&mut g, Duration::from_millis(20));
        }
        if c.serial != g.serial {
            return true; // stale, silently dropped
        }
        g.samples += c.data.len();
        g.chunks.push_back(c);
        true
    }

    pub fn flush(&self, serial: i32) {
        let mut g = self.inner.lock();
        g.chunks.clear();
        g.samples = 0;
        g.serial = serial;
        drop(g);
        self.cv.notify_all();
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().samples == 0
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::Release);
        self.cv.notify_all();
    }

    fn fill(&self, out: &mut [f32], channels: usize, rate: f64) -> Filled {
        let mut res = Filled { first: None, end_pts: None, frames: 0 };
        let mut g = self.inner.lock();
        let serial = g.serial;
        let mut written = 0;
        while written < out.len() {
            let Some(c) = g.chunks.front_mut() else { break };
            if c.serial != serial {
                let n = c.data.len() - c.pos;
                g.samples -= n;
                g.chunks.pop_front();
                continue;
            }
            if res.first.is_none() {
                let off = (c.pos / channels) as f64 / rate * c.speed;
                res.first = Some((c.pts + off, c.serial, c.speed));
            }
            let n = (c.data.len() - c.pos).min(out.len() - written);
            out[written..written + n].copy_from_slice(&c.data[c.pos..c.pos + n]);
            c.pos += n;
            written += n;
            res.end_pts = Some(c.pts + (c.pos / channels) as f64 / rate * c.speed);
            if c.pos >= c.data.len() {
                g.chunks.pop_front();
            }
            g.samples -= n;
        }
        drop(g);
        self.cv.notify_all();
        out[written..].fill(0.0);
        res.frames = written / channels.max(1);
        res
    }
}

fn find_device(host: &cpal::Host, name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name {
        if let Ok(mut devs) = host.output_devices()
            && let Some(d) = devs.find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
        {
            return Some(d);
        }
        log::warn!("audio device '{name}' not found, using default");
    }
    host.default_output_device()
}

pub fn list_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut v: Vec<String> = host
        .output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|x| x.name().to_string())).collect())
        .unwrap_or_default();
    v.dedup();
    v
}

fn wanted_channels(mode: ChannelMode, media: i32) -> Option<u16> {
    match mode {
        ChannelMode::Auto => (media > 2).then_some(media.min(8) as u16),
        ChannelMode::Stereo => Some(2),
        ChannelMode::Mono => Some(1),
        ChannelMode::Surround51 => Some(6),
        ChannelMode::Surround71 => Some(8),
    }
}

struct CallbackState {
    ctx: Arc<SessionCtx>,
    channels: usize,
    rate: f64,
    gain: f32,
    scratch: Vec<f32>,
    last_end: Option<(f64, i32, f64)>,
}

impl CallbackState {
    fn run(&mut self, out: &mut [f32], latency: f64) {
        let ctx = &self.ctx;
        if !ctx.alive() || ctx.paused() {
            out.fill(0.0);
            self.gain = 0.0;
            return;
        }
        let r = ctx.abuf.fill(out, self.channels, self.rate);
        let t = now();
        if let Some((pts, serial, speed)) = r.first {
            ctx.aclk.set_at(pts - latency * speed, serial, t);
            if let Some(e) = r.end_pts {
                self.last_end = Some((e, serial, speed));
            }
        } else if let Some((e, serial, speed)) = self.last_end {
            // Starved (buffering): hold the clock at the last audible sample.
            if serial == ctx.aq.serial() {
                ctx.aclk.set_at(e - latency * speed, serial, t);
            }
        }
        // Volume: cubic curve like most players, ramped per buffer to avoid clicks.
        let v = ctx.controls.volume.get() as f32;
        let target = if ctx.controls.muted.load(Ordering::Relaxed) { 0.0 } else { v * v * v };
        let frames = out.len() / self.channels.max(1);
        let start = self.gain;
        for (i, frame) in out.chunks_exact_mut(self.channels).enumerate() {
            let g = start + (target - start) * (i as f32 + 1.0) / frames.max(1) as f32;
            for s in frame {
                *s = (*s * g).clamp(-1.0, 1.0);
            }
        }
        self.gain = target;
    }
}

/// Opens the audio device on its own thread (cpal streams may not be `Send`).
/// How long playback waits for a device before continuing without sound.
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(3);

pub fn open_output(
    ctx: Arc<SessionCtx>,
    stop: Arc<AtomicBool>,
    cfg: &EngineConfig,
    media_channels: i32,
) -> anyhow::Result<(JoinHandle<()>, OutFormat)> {
    let (tx, rx) = crossbeam_channel::bounded::<OutFormat>(1);
    let device_name = cfg.audio_device.clone();
    let mode = cfg.channels;
    // Whoever sets this first owns the output: the device thread when it
    // opens in time, or the fallback below when it doesn't.
    let claimed = Arc::new(AtomicBool::new(false));
    let (c, s, cl) = (ctx.clone(), stop.clone(), claimed.clone());
    let handle = std::thread::Builder::new().name("audio-out".into()).spawn(move || {
        let ctx = c;
        let stop = s;
        let started = start_device(&ctx, device_name.as_deref(), mode, media_channels);
        if cl.swap(true, Ordering::AcqRel) {
            // Took too long; playback already continued without this device.
            log::warn!("audio device opened after the timeout; not using it for this file");
            return;
        }
        match started {
            Ok((stream, fmt, callbacks)) => {
                let _ = tx.send(fmt.clone());
                // Watchdog: a device that opens but never asks for samples
                // (stuck sound server, suspended sink) would freeze the master
                // clock. Fall back to the silent sink if that happens.
                let mut seen = callbacks.load(Ordering::Relaxed);
                let mut last_progress = std::time::Instant::now();
                while ctx.alive() && !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(30));
                    let c = callbacks.load(Ordering::Relaxed);
                    if c != seen || ctx.paused() {
                        seen = c;
                        last_progress = std::time::Instant::now();
                    } else if last_progress.elapsed() > Duration::from_secs(2) {
                        log::warn!("audio device stopped requesting samples; continuing silently");
                        ctx.emit(PlayerEvent::Warning(
                            "The audio device stopped responding; playing without sound".into(),
                        ));
                        drop(stream);
                        null_sink(&ctx, &stop, fmt);
                        return;
                    }
                }
                drop(stream);
            }
            Err(e) => {
                // No usable device: keep playing in real time without sound.
                log::warn!("audio output unavailable: {e:#}");
                ctx.emit(PlayerEvent::Warning("No audio device available — playing without sound".into()));
                let fmt = silent_format(mode, media_channels);
                let _ = tx.send(fmt.clone());
                null_sink(&ctx, &stop, fmt);
            }
        }
    })?;
    match rx.recv_timeout(DEVICE_OPEN_TIMEOUT) {
        Ok(fmt) => Ok((handle, fmt)),
        Err(_) if !claimed.swap(true, Ordering::AcqRel) => {
            // The device is hanging. Leave its thread behind (it exits on its
            // own if the device ever answers) and play silently instead.
            log::warn!("audio device did not open within {DEVICE_OPEN_TIMEOUT:?}; playing without sound");
            ctx.emit(PlayerEvent::Warning("The audio device isn't responding — playing without sound".into()));
            let fmt = silent_format(mode, media_channels);
            let f2 = fmt.clone();
            let sink =
                std::thread::Builder::new().name("audio-null".into()).spawn(move || null_sink(&ctx, &stop, f2))?;
            Ok((sink, fmt))
        }
        // The device won the race just as we timed out; its format is on the way.
        Err(_) => match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(fmt) => Ok((handle, fmt)),
            Err(_) => anyhow::bail!("audio device did not respond"),
        },
    }
}

fn silent_format(mode: ChannelMode, media_channels: i32) -> OutFormat {
    OutFormat {
        rate: 48000,
        channels: wanted_channels(mode, media_channels).unwrap_or(2),
        device: "None (silent)".into(),
    }
}

fn callback_state(ctx: &Arc<SessionCtx>, fmt: &OutFormat) -> CallbackState {
    CallbackState {
        ctx: ctx.clone(),
        channels: fmt.channels as usize,
        rate: fmt.rate as f64,
        gain: 0.0,
        scratch: Vec::new(),
        last_end: None,
    }
}

/// Consumes audio in real time and discards it, so the audio clock keeps
/// driving playback when there is no working output device.
fn null_sink(ctx: &Arc<SessionCtx>, stop: &AtomicBool, fmt: OutFormat) {
    const PERIOD: Duration = Duration::from_millis(20);
    let mut state = callback_state(ctx, &fmt);
    let mut buf = vec![0f32; (fmt.rate as usize / 50).max(1) * fmt.channels as usize];
    let mut next = std::time::Instant::now();
    while ctx.alive() && !stop.load(Ordering::Acquire) {
        state.run(&mut buf, 0.0);
        next += PERIOD;
        let now = std::time::Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            next = now;
        }
    }
}

type Started = (cpal::Stream, OutFormat, Arc<std::sync::atomic::AtomicU64>);

fn start_device(
    ctx: &Arc<SessionCtx>,
    device_name: Option<&str>,
    mode: ChannelMode,
    media_channels: i32,
) -> anyhow::Result<Started> {
    // Debug builds only: simulate a slow or hanging sound server.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("MAVPLAYER_TEST_AUDIO_DELAY_MS").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    let host = cpal::default_host();
    let device = find_device(&host, device_name).ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
    let dev_label = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "default".into());
    let def = device.default_output_config()?;
    let rate = def.sample_rate();
    let mut channels = def.channels();
    if let Some(want) = wanted_channels(mode, media_channels) {
        let ok = device
            .supported_output_configs()
            .map(|mut it| {
                it.any(|c| c.channels() == want && c.min_sample_rate() <= rate && c.max_sample_rate() >= rate)
            })
            .unwrap_or(false);
        if ok || want <= 2 {
            channels = want;
        }
    }
    let fmt = OutFormat { rate, channels, device: dev_label };
    let config = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
    let mut state = callback_state(ctx, &fmt);
    let callbacks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let err_ctx = ctx.clone();
    let on_err = move |e: cpal::Error| {
        log::warn!("audio stream error: {e}");
        err_ctx.emit(PlayerEvent::Warning(format!("Audio device error: {e}")));
    };
    let latency_of = |info: &cpal::OutputCallbackInfo| {
        let ts = info.timestamp();
        ts.playback.duration_since(ts.callback).as_secs_f64().min(0.5)
    };
    let cb = callbacks.clone();
    let stream = match def.sample_format() {
        cpal::SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |out, info| {
                cb.fetch_add(1, Ordering::Relaxed);
                let lat = latency_of(info);
                let mut scratch = std::mem::take(&mut state.scratch);
                scratch.resize(out.len(), 0.0);
                state.run(&mut scratch, lat);
                for (o, s) in out.iter_mut().zip(scratch.iter()) {
                    *o = (s * 32767.0) as i16;
                }
                state.scratch = scratch;
            },
            on_err,
            None,
        )?,
        _ => device.build_output_stream::<f32, _, _>(
            config,
            move |out, info| {
                cb.fetch_add(1, Ordering::Relaxed);
                let lat = latency_of(info);
                state.run(out, lat);
            },
            on_err,
            None,
        )?,
    };
    stream.play()?;
    Ok((stream, fmt, callbacks))
}

fn atempo_chain(speed: f64) -> Vec<String> {
    if (speed - 1.0).abs() < 1e-3 {
        return vec![];
    }
    let mut v = Vec::new();
    let mut s = speed;
    while s < 0.5 {
        v.push("atempo=0.5".to_string());
        s /= 0.5;
    }
    v.push(format!("atempo={s:.4}"));
    v
}

fn fx_chain(fx: &AudioFx, in_channels: i32) -> Vec<String> {
    let mut v = Vec::new();
    let stereo = in_channels == 2;
    if fx.swap_channels && stereo {
        v.push("channelmap=map=1|0".into());
    }
    if fx.karaoke && stereo {
        v.push("pan=stereo|c0=c0-c1|c1=c1-c0".into());
    }
    if fx.stereo_widen && stereo {
        v.push("extrastereo=m=1.8".into());
    }
    if fx.eq_enabled {
        if fx.preamp_db.abs() > 0.05 {
            v.push(format!("volume={:.2}dB", fx.preamp_db));
        }
        for (f, g) in EQ_BANDS.iter().zip(fx.eq_db.iter()) {
            if g.abs() > 0.05 {
                v.push(format!("equalizer=f={f}:t=o:w=1:g={g:.2}"));
            }
        }
    }
    if fx.night_mode {
        v.push("acompressor=threshold=-24dB:ratio=6:attack=10:release=200:makeup=8dB".into());
    }
    if fx.normalize {
        v.push("dynaudnorm=f=200:g=11:p=0.9:m=8".into());
    }
    v
}

fn layout_string(l: &ff::AVChannelLayout) -> String {
    if l.order == ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC || l.nb_channels <= 0 {
        let mut d: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
        unsafe { ff::av_channel_layout_default(&mut d, l.nb_channels.max(1)) };
        let s = av::channel_layout_name(&d);
        unsafe { ff::av_channel_layout_uninit(&mut d) };
        s
    } else {
        av::channel_layout_name(l)
    }
}

fn out_layout_string(channels: u16) -> String {
    let mut d: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
    unsafe { ff::av_channel_layout_default(&mut d, channels as c_int) };
    let s = av::channel_layout_name(&d);
    unsafe { ff::av_channel_layout_uninit(&mut d) };
    s
}

#[derive(PartialEq)]
struct Key {
    rate: i32,
    fmt: c_int,
    layout: String,
    fx_gen: u64,
    speed: u64,
}

pub fn run_decoder(ctx: Arc<SessionCtx>, mut dec: Decoder, tb: ff::AVRational, out: OutFormat, stop: Arc<AtomicBool>) {
    let mut frame = Frame::new();
    let mut filtered = Frame::new();
    let mut graph: Option<(Key, Graph, f64)> = None;
    let mut next_pts = f64::NAN;
    let out_layout = out_layout_string(out.channels);
    let max_samples = (out.rate as usize * out.channels as usize) / 4; // ~250 ms
    let aq = ctx.aq.clone();

    while ctx.alive() && !stop.load(Ordering::Acquire) {
        let res = dec.decode(&mut frame);
        let serial = dec.pkt_serial;
        match res {
            DecodeResult::Aborted => break,
            DecodeResult::Eof => {
                // Drain samples held by the filters (tempo, normaliser).
                if let Some((_, g, start)) = graph.as_mut() {
                    g.push_eof();
                    let start = *start;
                    let speed = ctx.controls.speed();
                    drain(&ctx, g, &mut filtered, start, speed, serial, out.channels, max_samples, &stop, &aq);
                }
                graph = None;
                ctx.audio_finished.store(dec.finished, Ordering::Release);
                ctx.wake_demuxer();
                continue;
            }
            DecodeResult::Frame => {}
        }
        let rate = frame.raw().sample_rate.max(1);
        let n = frame.nb_samples();
        let mut pts = av::ts_to_secs(frame.raw().pts, tb);
        if !pts.is_finite() {
            pts = next_pts;
        }
        let dur = n as f64 / rate as f64;
        next_pts = if pts.is_finite() { pts + dur } else { f64::NAN };

        if let Some((target, s)) = *ctx.seek_target.lock()
            && s == serial
            && pts.is_finite()
            && pts + dur < target
        {
            frame.unref();
            continue;
        }

        let speed = ctx.controls.speed();
        let (fx_gen, fx) = ctx.controls.audio_fx.lock().clone();
        let layout = layout_string(&frame.raw().ch_layout);
        let key = Key { rate, fmt: frame.format(), layout: layout.clone(), fx_gen, speed: speed.to_bits() };
        if graph.as_ref().map(|(k, _, _)| k != &key).unwrap_or(true) {
            // Flush the old graph's tail so speed/effect changes don't drop audio.
            if let Some((_, g, start)) = graph.as_mut() {
                g.push_eof();
                let (start, old_speed) = (*start, f64::from_bits(graph.as_ref().unwrap().0.speed));
                let g = &mut graph.as_mut().unwrap().1;
                drain(&ctx, g, &mut filtered, start, old_speed, serial, out.channels, max_samples, &stop, &aq);
            }
            let mut chain = fx_chain(&fx, frame.raw().ch_layout.nb_channels);
            chain.extend(atempo_chain(speed));
            chain.push(format!("aformat=sample_fmts=flt:sample_rates={}:channel_layouts={}", out.rate, out_layout));
            let args = format!(
                "time_base=1/{rate}:sample_rate={rate}:sample_fmt={}:channel_layout={}",
                av::sample_fmt_name(frame.format()),
                layout
            );
            match Graph::new("abuffer", &args, "abuffersink", &chain.join(","), 1) {
                Ok(g) => graph = Some((key, g, if pts.is_finite() { pts } else { 0.0 })),
                Err(e) => {
                    log::warn!("audio filters failed ({e}); falling back to plain conversion");
                    let chain =
                        format!("aformat=sample_fmts=flt:sample_rates={}:channel_layouts={}", out.rate, out_layout);
                    match Graph::new("abuffer", &args, "abuffersink", &chain, 1) {
                        Ok(g) => graph = Some((key, g, if pts.is_finite() { pts } else { 0.0 })),
                        Err(e) => {
                            log::error!("audio conversion failed: {e}");
                            frame.unref();
                            continue;
                        }
                    }
                }
            }
        }
        let (_, g, start) = graph.as_mut().unwrap();
        let start = *start;
        unsafe {
            (*frame.0).pts = if pts.is_finite() { (pts * rate as f64).round() as i64 } else { av::NOPTS };
        }
        if let Err(e) = g.push(&mut frame) {
            log::debug!("{e}");
        }
        frame.unref();
        drain(&ctx, g, &mut filtered, start, speed, serial, out.channels, max_samples, &stop, &aq);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain(
    ctx: &SessionCtx,
    g: &mut Graph,
    filtered: &mut Frame,
    start: f64,
    speed: f64,
    serial: i32,
    channels: u16,
    max_samples: usize,
    stop: &AtomicBool,
    aq: &super::sync::PacketQueue,
) {
    let otb = g.out_time_base();
    while g.pull(filtered) {
        let n = filtered.nb_samples() as usize * channels as usize;
        let out_pts = av::ts_to_secs(filtered.raw().pts, otb);
        // atempo stamps output on its own timeline; map back to media time.
        let media_pts = if out_pts.is_finite() { start + (out_pts - start) * speed } else { start };
        let data = unsafe { std::slice::from_raw_parts((*filtered.0).data[0] as *const f32, n) }.to_vec();
        filtered.unref();
        let chunk = Chunk { data, pos: 0, pts: media_pts, speed, serial };
        if !ctx.abuf.push(chunk, max_samples, stop, || aq.serial()) {
            return;
        }
    }
}
