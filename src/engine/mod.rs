//! MavPlayer's playback engine.
//!
//! Built directly on FFmpeg's libraries (demuxing, decoding, filtering,
//! scaling, resampling) plus `cpal` for audio output. Thread layout per
//! opened file ("session"):
//!
//! ```text
//!   demux ──► video packets ──► video decode ─► filter ─► frame queue ─► presenter ─► UI
//!         ├─► audio packets ──► audio decode ─► filter ─► sample buffer ─► audio device (master clock)
//!         └─► subtitle packets ─► subtitle decode ─► event store ─► UI overlay
//! ```

mod audio;
pub mod av;
mod decoder;
mod demux;
pub mod disc;
mod filters;
mod present;
pub mod probe;
mod subs;
mod sync;
pub mod types;
mod video;

pub use types::*;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::{Condvar, Mutex};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::thread::JoinHandle;

/// An `f64` stored in an `AtomicU64`.
pub struct AtomicF64(AtomicU64);
impl AtomicF64 {
    pub fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Acquire))
    }
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Release)
    }
}

/// Player-wide controls that persist across files.
pub struct Controls {
    pub paused: AtomicBool,
    pub volume: AtomicF64,
    pub muted: AtomicBool,
    pub speed: AtomicF64,
    pub audio_delay: AtomicF64,
    pub config: Mutex<EngineConfig>,
    pub audio_fx: Mutex<(u64, AudioFx)>,
    pub video_adjust: Mutex<(u64, VideoAdjust)>,
    /// Largest useful output size in physical pixels (w << 32 | h).
    pub target_size: AtomicU64,
}

impl Controls {
    pub fn target_size(&self) -> (u32, u32) {
        let v = self.target_size.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
    pub fn speed(&self) -> f64 {
        self.speed.get()
    }
}

/// State visible to the UI.
pub struct Shared {
    pub state: Mutex<PlaybackState>,
    pub frame_slot: Mutex<Option<PresentedFrame>>,
    pub wake: Box<dyn Fn() + Send + Sync>,
    pub events: Sender<PlayerEvent>,
    pub current_session: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub struct SeekRequest {
    pub target: f64,
    pub relative: bool,
    pub precise: bool,
}

/// Everything shared by the threads of one opened file.
pub struct SessionCtx {
    pub id: u64,
    pub abort: AtomicBool,
    pub controls: Arc<Controls>,
    pub shared: Arc<Shared>,
    pub vq: Arc<sync::PacketQueue>,
    pub aq: Arc<sync::PacketQueue>,
    pub sq: Arc<sync::PacketQueue>,
    pub frames: sync::FrameQueue,
    pub abuf: audio::AudioBuffer,
    pub aclk: sync::Clock,
    pub vclk: sync::Clock,
    pub audio_active: AtomicBool,
    /// Non-zero while an audio track's output device is still being opened.
    /// Holds the token of the component doing it, so a stale one can't clear
    /// a newer one's flag. Video shows its first frame but waits meanwhile.
    pub audio_pending: AtomicU64,
    pub audio_tokens: AtomicU64,
    pub video_active: AtomicBool,
    pub audio_finished: AtomicI32,
    pub video_finished: AtomicI32,
    pub eof: AtomicBool,
    pub seek_req: Mutex<Option<SeekRequest>>,
    pub demux_cv: Condvar,
    pub demux_lock: Mutex<()>,
    /// Exact-seek target: decoders drop frames before it (pts, serial).
    pub seek_target: Mutex<Option<(f64, i32)>>,
    pub seek_display: AtomicF64,
    pub seeking: AtomicBool,
    pub step: AtomicBool,
    pub show_one: AtomicBool,
    pub stepped: AtomicBool,
    pub start_time: AtomicF64,
    pub duration: AtomicF64,
    pub switch_req: Mutex<Vec<(TrackKind, Option<TrackId>)>>,
    pub subs: subs::SubStore,
    pub last_frame: Mutex<Option<(av::Frame, f64)>>,
    pub sub_canvas: AtomicU64,
}

impl SessionCtx {
    pub fn alive(&self) -> bool {
        !self.abort.load(Ordering::Acquire)
    }

    pub fn is_current(&self) -> bool {
        self.shared.current_session.load(Ordering::Acquire) == self.id
    }

    pub fn emit(&self, ev: PlayerEvent) {
        if self.is_current() && self.alive() {
            let _ = self.shared.events.send(ev);
            (self.shared.wake)();
        }
    }

    pub fn update_state(&self, f: impl FnOnce(&mut PlaybackState)) {
        if self.is_current() {
            f(&mut self.shared.state.lock());
        }
    }

    pub fn wake(&self) {
        if self.is_current() {
            (self.shared.wake)();
        }
    }

    pub fn wake_demuxer(&self) {
        let _g = self.demux_lock.lock();
        self.demux_cv.notify_all();
    }

    pub fn paused(&self) -> bool {
        self.controls.paused.load(Ordering::Acquire)
    }

    /// The clock everything is synchronised to: the audio device when audio
    /// is playing, otherwise the video clock.
    pub fn master_clock(&self) -> f64 {
        if self.audio_active.load(Ordering::Acquire) {
            self.aclk.get(self.aq.serial())
        } else {
            self.vclk.get(self.vq.serial())
        }
    }

    /// True while playback is held until the audio device is ready.
    pub fn audio_pending(&self) -> bool {
        self.audio_pending.load(Ordering::Acquire) != 0
    }

    pub fn audio_is_master(&self) -> bool {
        self.audio_active.load(Ordering::Acquire)
    }

    /// Playback position in seconds from the start of the media.
    pub fn position(&self) -> f64 {
        if self.seeking.load(Ordering::Acquire) {
            return self.seek_display.get();
        }
        let start = self.start_time.get();
        let m = self.master_clock();
        if m.is_finite() {
            let d = self.duration.get();
            let p = (m - start).max(0.0);
            if d > 0.0 { p.min(d) } else { p }
        } else {
            self.seek_display.get()
        }
    }

    pub fn request_seek(&self, req: SeekRequest) {
        if !req.relative {
            self.seek_display.set(req.target.max(0.0));
        } else {
            let base = if self.seeking.load(Ordering::Acquire) { self.seek_display.get() } else { self.position() };
            self.seek_display.set((base + req.target).max(0.0));
        }
        // Coalesce: an absolute target replaces whatever was pending.
        let abs = SeekRequest { target: self.seek_display.get(), relative: false, precise: req.precise };
        *self.seek_req.lock() = Some(abs);
        self.seeking.store(true, Ordering::Release);
        self.wake_demuxer();
    }
}

#[derive(Default, Clone)]
pub struct LoadOptions {
    pub start_at: Option<f64>,
    pub external_subs: Vec<PathBuf>,
    pub start_paused: bool,
}

struct Session {
    ctx: Arc<SessionCtx>,
    demux: Option<JoinHandle<()>>,
}

impl Session {
    fn shutdown(mut self) {
        let ctx = self.ctx.clone();
        ctx.abort.store(true, Ordering::Release);
        ctx.vq.abort();
        ctx.aq.abort();
        ctx.sq.abort();
        ctx.frames.abort();
        ctx.abuf.abort();
        ctx.wake_demuxer();
        // Join in the background so switching files never blocks the UI.
        if let Some(h) = self.demux.take() {
            std::thread::Builder::new()
                .name("session-reaper".into())
                .spawn(move || {
                    let _ = h.join();
                })
                .ok();
        }
    }
}

/// The handle the UI uses to control playback.
pub struct Player {
    controls: Arc<Controls>,
    shared: Arc<Shared>,
    session: Option<Session>,
    events_rx: Receiver<PlayerEvent>,
    next_id: u64,
}

impl Player {
    pub fn new(config: EngineConfig, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Player {
            controls: Arc::new(Controls {
                paused: AtomicBool::new(false),
                volume: AtomicF64::new(1.0),
                muted: AtomicBool::new(false),
                speed: AtomicF64::new(1.0),
                audio_delay: AtomicF64::new(0.0),
                config: Mutex::new(config),
                audio_fx: Mutex::new((0, AudioFx::default())),
                video_adjust: Mutex::new((0, VideoAdjust::default())),
                target_size: AtomicU64::new((3840u64 << 32) | 2160),
            }),
            shared: Arc::new(Shared {
                state: Mutex::new(PlaybackState::default()),
                frame_slot: Mutex::new(None),
                wake: Box::new(wake),
                events: tx,
                current_session: AtomicU64::new(0),
            }),
            session: None,
            events_rx: rx,
            next_id: 1,
        }
    }

    /// Starts playing `source`, replacing whatever was playing.
    pub fn load(&mut self, source: MediaSource, opts: LoadOptions) {
        self.stop_session();
        let id = self.next_id;
        self.next_id += 1;
        self.shared.current_session.store(id, Ordering::Release);
        self.controls.paused.store(opts.start_paused, Ordering::Release);
        {
            let mut st = self.shared.state.lock();
            *st = PlaybackState { status: Status::Loading, source: Some(source.clone()), ..Default::default() };
        }
        *self.shared.frame_slot.lock() = None;
        let ctx = Arc::new(SessionCtx {
            id,
            abort: AtomicBool::new(false),
            controls: self.controls.clone(),
            shared: self.shared.clone(),
            vq: Arc::new(sync::PacketQueue::new()),
            aq: Arc::new(sync::PacketQueue::new()),
            sq: Arc::new(sync::PacketQueue::new()),
            frames: sync::FrameQueue::new(4),
            abuf: audio::AudioBuffer::new(),
            aclk: sync::Clock::new(),
            vclk: sync::Clock::new(),
            audio_active: AtomicBool::new(false),
            audio_pending: AtomicU64::new(0),
            audio_tokens: AtomicU64::new(0),
            video_active: AtomicBool::new(false),
            audio_finished: AtomicI32::new(-1),
            video_finished: AtomicI32::new(-1),
            eof: AtomicBool::new(false),
            seek_req: Mutex::new(None),
            demux_cv: Condvar::new(),
            demux_lock: Mutex::new(()),
            seek_target: Mutex::new(None),
            seek_display: AtomicF64::new(opts.start_at.unwrap_or(0.0)),
            seeking: AtomicBool::new(false),
            step: AtomicBool::new(false),
            show_one: AtomicBool::new(false),
            stepped: AtomicBool::new(false),
            start_time: AtomicF64::new(0.0),
            duration: AtomicF64::new(0.0),
            switch_req: Mutex::new(Vec::new()),
            subs: subs::SubStore::new(),
            last_frame: Mutex::new(None),
            sub_canvas: AtomicU64::new(0),
        });
        let c = ctx.clone();
        let demux = std::thread::Builder::new()
            .name("demux".into())
            .spawn(move || demux::run(c, source, opts))
            .expect("spawn demux thread");
        self.session = Some(Session { ctx, demux: Some(demux) });
        (self.shared.wake)();
    }

    fn stop_session(&mut self) {
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
    }

    pub fn stop(&mut self) {
        self.stop_session();
        self.shared.current_session.store(0, Ordering::Release);
        *self.shared.state.lock() = PlaybackState::default();
        *self.shared.frame_slot.lock() = None;
        (self.shared.wake)();
    }

    pub fn is_active(&self) -> bool {
        self.session.is_some()
    }

    pub fn state(&self) -> PlaybackState {
        self.shared.state.lock().clone()
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&PlaybackState) -> R) -> R {
        f(&self.shared.state.lock())
    }

    pub fn drain_events(&self) -> Vec<PlayerEvent> {
        self.events_rx.try_iter().collect()
    }

    pub fn take_frame(&self) -> Option<PresentedFrame> {
        self.shared.frame_slot.lock().take()
    }

    pub fn position(&self) -> f64 {
        self.session.as_ref().map(|s| s.ctx.position()).unwrap_or(0.0)
    }

    pub fn duration(&self) -> f64 {
        self.session.as_ref().map(|s| s.ctx.duration.get()).unwrap_or(0.0)
    }

    pub fn paused(&self) -> bool {
        self.controls.paused.load(Ordering::Acquire)
    }

    pub fn set_paused(&mut self, paused: bool) {
        let was = self.controls.paused.swap(paused, Ordering::AcqRel);
        if let Some(s) = &self.session {
            let ctx = &s.ctx;
            ctx.aclk.set_paused(paused);
            ctx.vclk.set_paused(paused);
            if was && !paused && ctx.stepped.swap(false, Ordering::AcqRel) {
                // Frame stepping desynchronised audio; realign precisely.
                let pos = ctx.position();
                ctx.request_seek(SeekRequest { target: pos, relative: false, precise: true });
            }
            ctx.update_state(|st| {
                if matches!(st.status, Status::Playing | Status::Paused) {
                    st.status = if paused { Status::Paused } else { Status::Playing };
                }
            });
            ctx.wake_demuxer();
        }
        (self.shared.wake)();
    }

    pub fn toggle_pause(&mut self) {
        let ended = self.with_state(|s| s.status == Status::Ended);
        if ended {
            self.seek(0.0, false, true);
            self.set_paused(false);
            return;
        }
        let p = !self.paused();
        self.set_paused(p);
    }

    /// Seeks to `target` seconds (or by `target` seconds if `relative`).
    pub fn seek(&mut self, target: f64, relative: bool, precise: bool) {
        if let Some(s) = &self.session {
            s.ctx.request_seek(SeekRequest { target, relative, precise });
            s.ctx.update_state(|st| {
                if st.status == Status::Ended {
                    st.status = if s.ctx.paused() { Status::Paused } else { Status::Playing };
                }
            });
        }
    }

    pub fn frame_step(&mut self, forward: bool) {
        if self.session.is_none() {
            return;
        }
        if !self.paused() {
            self.set_paused(true);
        }
        let s = self.session.as_ref().unwrap();
        if forward {
            s.ctx.step.store(true, Ordering::Release);
            s.ctx.stepped.store(true, Ordering::Release);
        } else {
            let fps = self.with_state(|st| st.video.as_ref().map(|v| v.fps).unwrap_or(25.0));
            let fd = if fps > 1.0 { 1.0 / fps } else { 0.04 };
            let pos = s.ctx.position();
            s.ctx.request_seek(SeekRequest { target: (pos - fd * 1.5).max(0.0), relative: false, precise: true });
            s.ctx.stepped.store(true, Ordering::Release);
        }
        let _ = s;
    }

    pub fn volume(&self) -> f64 {
        self.controls.volume.get()
    }
    /// Volume as a linear 0.0..=1.5 scale (1.0 = 100%).
    pub fn set_volume(&self, v: f64) {
        self.controls.volume.set(v.clamp(0.0, 1.5));
    }
    pub fn muted(&self) -> bool {
        self.controls.muted.load(Ordering::Relaxed)
    }
    pub fn set_muted(&self, m: bool) {
        self.controls.muted.store(m, Ordering::Relaxed);
    }

    pub fn speed(&self) -> f64 {
        self.controls.speed()
    }
    pub fn set_speed(&self, speed: f64) {
        let speed = speed.clamp(0.25, 4.0);
        self.controls.speed.set(speed);
        if let Some(s) = &self.session {
            s.ctx.aclk.set_speed(speed);
            s.ctx.vclk.set_speed(speed);
        }
    }

    pub fn audio_delay(&self) -> f64 {
        self.controls.audio_delay.get()
    }
    pub fn set_audio_delay(&self, d: f64) {
        self.controls.audio_delay.set(d);
    }

    pub fn set_audio_fx(&self, fx: AudioFx) {
        let mut g = self.controls.audio_fx.lock();
        if g.1 != fx {
            g.0 += 1;
            g.1 = fx;
        }
    }

    pub fn set_video_adjust(&self, adj: VideoAdjust) {
        let mut g = self.controls.video_adjust.lock();
        if g.1 != adj {
            g.0 += 1;
            g.1 = adj;
        }
        drop(g);
        // Re-show the current picture with the new filters when paused.
        if let Some(s) = &self.session
            && self.paused()
        {
            let pos = s.ctx.position();
            s.ctx.request_seek(SeekRequest { target: pos, relative: false, precise: true });
        }
    }

    pub fn set_config(&self, cfg: EngineConfig) {
        *self.controls.config.lock() = cfg;
    }

    /// Tells the engine how large the video is drawn, in physical pixels, so
    /// it can avoid converting more pixels than will be shown.
    pub fn set_target_size(&self, w: u32, h: u32) {
        let v = ((w.max(16) as u64) << 32) | h.max(16) as u64;
        self.controls.target_size.store(v, Ordering::Relaxed);
    }

    pub fn select_track(&self, kind: TrackKind, id: Option<TrackId>) {
        if let Some(s) = &self.session {
            if kind == TrackKind::Subtitle {
                if let Some(TrackId::External(i)) = id {
                    s.ctx.subs.select_external(Some(i));
                    s.ctx.update_state(|st| st.selected.subtitle = id);
                    // Stop routing any embedded subtitle stream.
                    s.ctx.switch_req.lock().push((kind, None));
                    s.ctx.wake_demuxer();
                    (self.shared.wake)();
                    return;
                }
                s.ctx.subs.select_external(None);
            }
            s.ctx.switch_req.lock().push((kind, id));
            s.ctx.update_state(|st| match kind {
                TrackKind::Video => st.selected.video = id,
                TrackKind::Audio => st.selected.audio = id,
                TrackKind::Subtitle => st.selected.subtitle = id,
            });
            s.ctx.wake_demuxer();
            (self.shared.wake)();
        }
    }

    /// Loads an external subtitle file and selects it.
    pub fn add_subtitle_file(&self, path: &Path) {
        if let Some(s) = &self.session {
            subs::load_external_async(s.ctx.clone(), path.to_path_buf(), true);
        }
    }

    /// Subtitle events visible at `t` seconds (media time).
    pub fn subtitles_at(&self, t: f64, out: &mut Vec<SubEvent>) {
        out.clear();
        if let Some(s) = &self.session {
            s.ctx.subs.active_at(t, s.ctx.start_time.get(), out);
        }
    }

    /// Size of the canvas bitmap subtitles are positioned on.
    pub fn subtitle_canvas(&self) -> (u32, u32) {
        self.session
            .as_ref()
            .map(|s| {
                let v = s.ctx.sub_canvas.load(Ordering::Relaxed);
                ((v >> 32) as u32, v as u32)
            })
            .unwrap_or((0, 0))
    }

    /// Saves the current picture at its native resolution.
    pub fn screenshot(&self, path: PathBuf) {
        if let Some(s) = &self.session {
            present::screenshot_async(s.ctx.clone(), path);
        }
    }

    pub fn has_frame(&self) -> bool {
        self.session.as_ref().map(|s| s.ctx.last_frame.lock().is_some()).unwrap_or(false)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop_session();
    }
}

/// One-time library initialisation.
pub fn init(verbose: bool) {
    av::init_logging(verbose);
    unsafe {
        av::ff::avformat_network_init();
    }
}

/// Lists audio output devices by name.
pub fn audio_devices() -> Vec<String> {
    audio::list_devices()
}
