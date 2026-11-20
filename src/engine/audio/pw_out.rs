//! Native PipeWire output, used whenever a PipeWire server is running
//! (cpal's ALSA host is the fallback).
//!
//! Talking to PipeWire directly gives the stream proper channel positions
//! (so 5.1/7.1 reach the sink, or are downmixed by PipeWire, correctly), an
//! exact output delay for A/V sync, and a "MavPlayer" entry with its icon in
//! volume mixers.

use super::{OutFormat, OutputLink, callback_state, wanted_channels};
use crate::engine::av::ff;
use crate::engine::types::ChannelMode;
use pipewire as pw;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::pod::{Object, Pod, Value, serialize::PodSerializer};
use pw::stream::{StreamFlags, StreamState};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// PipeWire's default graph rate; anything else is resampled by PipeWire.
const RATE: u32 = 48000;
/// Requested period: 1024 frames, about 21 ms.
const PERIOD: u32 = 1024;
/// How long to wait for the server to answer a registry query.
const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
const APP_ID: &str = "io.github.drogobo.MavPlayer";

/// Why the output loop ended.
pub enum Outcome {
    /// The session ended or switched files.
    Stopped,
    /// The stream failed or stopped asking for samples after playback began.
    Failed(OutFormat, String),
}

struct Sink {
    name: String,
    description: String,
}

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pw::init);
}

fn app_props() -> pw::properties::PropertiesBox {
    pw::properties::properties! {
        *pw::keys::APP_NAME => "MavPlayer",
        *pw::keys::APP_ID => APP_ID,
        *pw::keys::APP_ICON_NAME => APP_ID,
    }
}

fn connect() -> anyhow::Result<(pw::main_loop::MainLoopRc, pw::context::ContextRc, pw::core::CoreRc)> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(Some(app_props()))?;
    Ok((mainloop, context, core))
}

/// Lists the server's audio sinks with one registry round trip.
fn sinks(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> anyhow::Result<Vec<Sink>> {
    let registry = core.get_registry_rc()?;
    let found = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0)?;
    let (d, ml) = (done.clone(), mainloop.clone());
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                d.set(true);
                ml.quit();
            }
        })
        .register();
    let f = found.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            if g.type_ != pw::types::ObjectType::Node {
                return;
            }
            let Some(p) = g.props else { return };
            if p.get(*pw::keys::MEDIA_CLASS) != Some("Audio/Sink") {
                return;
            }
            let Some(name) = p.get(*pw::keys::NODE_NAME) else { return };
            let description = p.get(*pw::keys::NODE_DESCRIPTION).unwrap_or(name);
            f.borrow_mut().push(Sink { name: name.to_string(), description: description.to_string() });
        })
        .register();
    let ml = mainloop.clone();
    let timeout = mainloop.loop_().add_timer(move |_| ml.quit());
    timeout.update_timer(Some(QUERY_TIMEOUT), None).into_result()?;
    mainloop.run();
    anyhow::ensure!(done.get(), "PipeWire did not answer");
    Ok(found.take())
}

/// The sinks' names as shown in settings, or `None` without PipeWire.
pub fn list_devices() -> Option<Vec<String>> {
    let (mainloop, _context, core) = connect().ok()?;
    let list = sinks(&mainloop, &core).ok()?;
    Some(list.into_iter().map(|s| s.description).collect())
}

/// Maps FFmpeg's default layout for `channels` (what the filter graph
/// outputs) to PipeWire channel positions.
fn positions(channels: u16) -> [u32; MAX_CHANNELS] {
    use ff::AVChannel::*;
    use spa::sys::*;
    let mut pos = [SPA_AUDIO_CHANNEL_UNKNOWN; MAX_CHANNELS];
    if channels <= 1 {
        pos[0] = SPA_AUDIO_CHANNEL_MONO;
        return pos;
    }
    let mut l: ff::AVChannelLayout = unsafe { std::mem::zeroed() };
    unsafe { ff::av_channel_layout_default(&mut l, channels as _) };
    for (i, p) in pos.iter_mut().enumerate().take(channels as usize) {
        let ch = unsafe { ff::av_channel_layout_channel_from_index(&l, i as _) };
        *p = match ch {
            AV_CHAN_FRONT_LEFT => SPA_AUDIO_CHANNEL_FL,
            AV_CHAN_FRONT_RIGHT => SPA_AUDIO_CHANNEL_FR,
            AV_CHAN_FRONT_CENTER => SPA_AUDIO_CHANNEL_FC,
            AV_CHAN_LOW_FREQUENCY => SPA_AUDIO_CHANNEL_LFE,
            AV_CHAN_BACK_LEFT => SPA_AUDIO_CHANNEL_RL,
            AV_CHAN_BACK_RIGHT => SPA_AUDIO_CHANNEL_RR,
            AV_CHAN_FRONT_LEFT_OF_CENTER => SPA_AUDIO_CHANNEL_FLC,
            AV_CHAN_FRONT_RIGHT_OF_CENTER => SPA_AUDIO_CHANNEL_FRC,
            AV_CHAN_BACK_CENTER => SPA_AUDIO_CHANNEL_RC,
            AV_CHAN_SIDE_LEFT => SPA_AUDIO_CHANNEL_SL,
            AV_CHAN_SIDE_RIGHT => SPA_AUDIO_CHANNEL_SR,
            AV_CHAN_TOP_CENTER => SPA_AUDIO_CHANNEL_TC,
            AV_CHAN_TOP_FRONT_LEFT => SPA_AUDIO_CHANNEL_TFL,
            AV_CHAN_TOP_FRONT_CENTER => SPA_AUDIO_CHANNEL_TFC,
            AV_CHAN_TOP_FRONT_RIGHT => SPA_AUDIO_CHANNEL_TFR,
            AV_CHAN_TOP_BACK_LEFT => SPA_AUDIO_CHANNEL_TRL,
            AV_CHAN_TOP_BACK_CENTER => SPA_AUDIO_CHANNEL_TRC,
            AV_CHAN_TOP_BACK_RIGHT => SPA_AUDIO_CHANNEL_TRR,
            _ => SPA_AUDIO_CHANNEL_UNKNOWN,
        };
    }
    unsafe { ff::av_channel_layout_uninit(&mut l) };
    pos
}

fn format_param(channels: u16) -> anyhow::Result<Vec<u8>> {
    let mut info = AudioInfoRaw::new();
    info.set_format(if cfg!(target_endian = "little") { AudioFormat::F32LE } else { AudioFormat::F32BE });
    info.set_rate(RATE);
    info.set_channels(channels as u32);
    info.set_position(positions(channels));
    let obj =
        Object { type_: spa::sys::SPA_TYPE_OBJECT_Format, id: spa::sys::SPA_PARAM_EnumFormat, properties: info.into() };
    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| anyhow::anyhow!("serializing the audio format: {e:?}"))?;
    Ok(cursor.into_inner())
}

struct ProcessData {
    state: super::CallbackState,
    scratch: Vec<f32>,
    channels: usize,
    callbacks: Arc<AtomicU64>,
}

/// Plays the session's audio through PipeWire until it stops. `ready` is
/// called once the stream is connected; when it returns false someone else
/// already took over the output and this returns at once. An `Err` means
/// PipeWire couldn't be used at all, so the caller can try another backend.
pub fn run(
    link: &Arc<OutputLink>,
    device: Option<&str>,
    mode: ChannelMode,
    media_channels: i32,
    ready: impl FnOnce(&OutFormat) -> bool,
) -> anyhow::Result<Outcome> {
    let (mainloop, _context, core) = connect()?;

    // A device chosen in settings is stored by its description.
    let mut target = None;
    let mut label = "Default (PipeWire)".to_string();
    if let Some(want) = device {
        match sinks(&mainloop, &core) {
            Ok(list) => match list.into_iter().find(|s| s.description == want || s.name == want) {
                Some(s) => {
                    label = s.description;
                    target = Some(s.name);
                }
                None => log::warn!("audio device '{want}' not found, using the default"),
            },
            Err(e) => log::warn!("could not list PipeWire sinks: {e:#}"),
        }
    }

    let channels = wanted_channels(mode, media_channels).unwrap_or(2);
    let fmt = OutFormat { rate: RATE, channels, device: label };

    let mut props = app_props();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, "Playback");
    props.insert(*pw::keys::MEDIA_ROLE, "Movie");
    props.insert(*pw::keys::NODE_NAME, "MavPlayer");
    props.insert(*pw::keys::MEDIA_NAME, "Playback");
    props.insert(*pw::keys::NODE_LATENCY, format!("{PERIOD}/{RATE}"));
    if let Some(t) = &target {
        props.insert(*pw::keys::TARGET_OBJECT, t.as_str());
    }
    let stream = pw::stream::StreamRc::new(core.clone(), "MavPlayer", props)?;

    let callbacks = Arc::new(AtomicU64::new(0));
    let data = ProcessData {
        state: callback_state(link, &fmt),
        scratch: Vec::new(),
        channels: channels as usize,
        callbacks: callbacks.clone(),
    };
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let (fail, ml) = (failure.clone(), mainloop.clone());
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, _, _, new| {
            if let StreamState::Error(e) = new {
                *fail.borrow_mut() = Some(e);
                ml.quit();
            }
        })
        .process(|stream, d| {
            d.callbacks.fetch_add(1, Ordering::Relaxed);
            let frame_bytes = d.channels * std::mem::size_of::<f32>();
            // Seconds until the first sample written now is heard.
            let latency = stream
                .time()
                .map(|t| {
                    let r = t.rate();
                    let delay = if r.denom > 0 { t.delay() as f64 * r.num as f64 / r.denom as f64 } else { 0.0 };
                    delay + t.queued() as f64 / frame_bytes as f64 / RATE as f64
                })
                .unwrap_or(0.0)
                .clamp(0.0, 0.5);
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(out) = datas.first_mut() else { return };
            // One period per buffer. (pw_buffer.requested would say exactly
            // how much the graph wants, but needs PipeWire 0.3.49 headers.)
            let frames = match out.data() {
                Some(bytes) => {
                    let frames = (bytes.len() / frame_bytes).min(PERIOD as usize);
                    d.scratch.resize(frames * d.channels, 0.0);
                    d.state.run(&mut d.scratch, latency);
                    for (o, s) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(d.scratch.iter()) {
                        o.copy_from_slice(&s.to_ne_bytes());
                    }
                    frames
                }
                None => 0,
            };
            let chunk = out.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = frame_bytes as i32;
            *chunk.size_mut() = (frames * frame_bytes) as u32;
        })
        .register()?;

    let param = format_param(channels)?;
    let mut params = [Pod::from_bytes(&param).ok_or_else(|| anyhow::anyhow!("invalid audio format"))?];
    stream.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    log::debug!("PipeWire stream connected ({} channels)", channels);
    if !ready(&fmt) {
        return Ok(Outcome::Stopped);
    }

    // Stop when nothing is left to play; give up on a stream that stops
    // asking for samples (no sink, stuck server) so the clock doesn't freeze.
    let stalled = Rc::new(Cell::new(false));
    let (l, ml, st) = (link.clone(), mainloop.clone(), stalled.clone());
    let seen = Cell::new(0u64);
    let last_progress = Cell::new(Instant::now());
    let ticker = mainloop.loop_().add_timer(move |_| {
        if !l.keep_serving() {
            ml.quit();
            return;
        }
        let n = callbacks.load(Ordering::Relaxed);
        if n != seen.get() || l.paused() {
            seen.set(n);
            last_progress.set(Instant::now());
        } else if last_progress.get().elapsed() > Duration::from_secs(2) {
            st.set(true);
            ml.quit();
        }
    });
    let tick = Duration::from_millis(30);
    ticker.update_timer(Some(tick), Some(tick)).into_result()?;
    mainloop.run();

    if let Some(e) = failure.take() {
        return Ok(Outcome::Failed(fmt, format!("PipeWire stream error: {e}")));
    }
    if stalled.get() {
        return Ok(Outcome::Failed(fmt, "the audio device stopped requesting samples".into()));
    }
    let _ = stream.disconnect();
    Ok(Outcome::Stopped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spa::sys::*;

    #[test]
    fn surround_positions_follow_ffmpeg_default_layouts() {
        assert_eq!(positions(1)[0], SPA_AUDIO_CHANNEL_MONO);
        assert_eq!(positions(2)[..2], [SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR]);
        let p6 = positions(6);
        assert_eq!(p6[..4], [SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR, SPA_AUDIO_CHANNEL_FC, SPA_AUDIO_CHANNEL_LFE]);
        for n in 1..=8u16 {
            let p = positions(n);
            assert!(p[..n as usize].iter().all(|&c| c != SPA_AUDIO_CHANNEL_UNKNOWN), "{n} channels: {p:?}");
            assert!(p[n as usize..].iter().all(|&c| c == SPA_AUDIO_CHANNEL_UNKNOWN));
        }
    }
}
