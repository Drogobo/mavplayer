//! The main window view: state, playback logic and action handlers.
//! Rendering lives in `render.rs`, dialogs in `dialogs.rs`, menus in `menus.rs`.

use super::actions::*;
use super::dialogs::Dialog;
use super::menu::OpenMenu;
use super::text_input::{InputEvent, TextInput};
use super::theme::Theme;
use crate::engine::disc::{self, DvdTitle};
use crate::engine::{
    self, Deinterlace, HwDecode, LoadOptions, MediaSource, Player, PlayerEvent, Rotation, Status, SubEvent, TrackId,
    TrackKind, VideoAdjust,
};
use crate::icons;
use crate::playlist::{self, Playlist};
use crate::settings::{RepeatMode, ResumeStore, Settings};
use futures::StreamExt;
use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, PathPromptOptions, Pixels, RenderImage,
    Subscription, Window, px, size,
};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// URLs/paths handed to us by the OS while running (macOS "Open With", dock drops).
pub static OPEN_REQUESTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Called with a slider's new value (0..1).
pub type SliderFn = Rc<dyn Fn(&mut PlayerView, f32, &mut Window, &mut Context<PlayerView>)>;

pub const SPEEDS: &[f64] = &[0.25, 0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Aspect {
    Auto,
    Ratio(f64, &'static str),
    Stretch,
    Fill,
}

pub const ASPECTS: &[Aspect] = &[
    Aspect::Auto,
    Aspect::Ratio(16.0 / 9.0, "16:9"),
    Aspect::Ratio(4.0 / 3.0, "4:3"),
    Aspect::Ratio(2.35, "2.35:1"),
    Aspect::Ratio(2.39, "2.39:1"),
    Aspect::Ratio(1.85, "1.85:1"),
    Aspect::Ratio(16.0 / 10.0, "16:10"),
    Aspect::Ratio(5.0 / 4.0, "5:4"),
    Aspect::Ratio(1.0, "1:1"),
    Aspect::Fill,
    Aspect::Stretch,
];

impl Aspect {
    pub fn label(&self) -> &'static str {
        match self {
            Aspect::Auto => "Default",
            Aspect::Ratio(_, l) => l,
            Aspect::Stretch => "Stretch to window",
            Aspect::Fill => "Fill window (crop)",
        }
    }
}

/// An active slider drag.
pub struct Drag {
    pub bounds: Rc<Cell<Bounds<Pixels>>>,
    pub vertical: bool,
    pub on_change: SliderFn,
    pub on_release: Option<SliderFn>,
    pub last: f32,
}

pub struct PlayerView {
    pub player: Player,
    pub settings: Settings,
    pub resume: ResumeStore,
    pub playlist: Playlist,
    pub theme: Theme,
    pub focus: FocusHandle,
    wake_pending: Arc<AtomicBool>,

    // Picture
    pub frame: Option<Arc<RenderImage>>,
    pub frame_size: (u32, u32),
    retired: VecDeque<(Arc<RenderImage>, u8)>,
    pub sub_images: HashMap<u64, (Arc<RenderImage>, u32)>,
    pub subs: Vec<SubEvent>,
    pub video_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub sent_target: (u32, u32),
    /// Pause state just before the last single-click pause toggle, so the
    /// second click of a double-click can put it back.
    pub click_pause: Option<(bool, Instant)>,

    // Interface state
    pub last_activity: Instant,
    pub pointer_over_controls: bool,
    pub osd: Option<(String, Instant)>,
    pub menu: Option<OpenMenu>,
    pub dialog: Option<Dialog>,
    pub drag: Option<Drag>,
    pub seek_hover: Option<f32>,
    pub seek_drag_last: Instant,
    pub show_playlist: bool,
    pub playlist_selected: Option<usize>,
    pub ab_loop: (Option<f64>, Option<f64>),
    pub loop_file: bool,
    pub aspect: Aspect,
    pub zoom: f32,
    pub video_adjust: VideoAdjust,
    pub sub_delay: f64,
    pub subs_visible: bool,
    pub dvd_titles: Option<(String, Vec<DvdTitle>)>,
    pub reading_disc: bool,
    pub fitted_for: Option<u64>,
    /// A requested window size and how many more times to re-apply it if
    /// the compositor's configure overrode it.
    pub pending_fit: Option<(gpui::Size<Pixels>, u8)>,
    pub load_counter: u64,
    pub last_status: Status,
    pub was_fullscreen: bool,
    pub inputs: HashMap<&'static str, Entity<TextInput>>,
    pub audio_devices: Vec<String>,
    pub dropping: bool,
    /// Insertion point (0..=len) shown while something is dragged over the playlist.
    pub pl_drop: Option<usize>,
    pub pl_scroll: gpui::UniformListScrollHandle,
    /// The volume percentage is being edited as text.
    pub editing_volume: bool,
    volume_blur: Option<Subscription>,
    _subs: Vec<Subscription>,
}

impl Focusable for PlayerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Everything that can start before the window exists. `main` creates this
/// before GPUI sets up the GPU (which takes 100–200 ms on some drivers), so
/// the first file is already opening, decoding and finding its audio device
/// by the time the window appears.
pub struct Boot {
    pub settings: Settings,
    player: Player,
    wake_pending: Arc<AtomicBool>,
    wake_rx: futures::channel::mpsc::UnboundedReceiver<()>,
    icons_rx: futures::channel::mpsc::UnboundedReceiver<()>,
    resume: ResumeStore,
    sources: Vec<MediaSource>,
    /// `sources[0]` is already loading.
    started: bool,
}

impl Boot {
    pub fn start(sources: Vec<MediaSource>) -> Self {
        let settings = Settings::load();
        icons::set_use_system_theme(settings.icon_theme == crate::settings::IconTheme::System);
        // Icons from the desktop theme are resolved off the UI thread.
        let (itx, icons_rx) = futures::channel::mpsc::unbounded::<()>();
        icons::init_system_icons(move || {
            let _ = itx.unbounded_send(());
        });

        let wake_pending = Arc::new(AtomicBool::new(false));
        let (tx, wake_rx) = futures::channel::mpsc::unbounded::<()>();
        let wp = wake_pending.clone();
        // Wake-ups sent before the view exists queue up in the channel.
        let mut player = Player::new(settings.engine_config(), move || {
            if !wp.swap(true, Ordering::AcqRel) {
                let _ = tx.unbounded_send(());
            }
        });
        player.set_volume(settings.volume);
        player.set_muted(settings.muted);
        player.set_audio_fx(settings.audio_fx.clone());
        let resume = ResumeStore::load();
        // A whole DVD (title 0) needs its titles probed first; that runs in the view.
        let started = match sources.first() {
            Some(MediaSource::Dvd { title: 0, .. }) | None => false,
            Some(src) => {
                let start_at = if settings.resume_playback { resume.get(&src.key()) } else { None };
                // Paused until the window exists, so sound doesn't start
                // before there is anything to see.
                player.load(src.clone(), LoadOptions { start_at, start_paused: true, ..Default::default() });
                log::debug!("startup: first file loading at {:.0} ms", crate::ms_since_start());
                true
            }
        };
        Self { settings, player, wake_pending, wake_rx, icons_rx, resume, sources, started }
    }
}

impl PlayerView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, boot: Boot) -> Self {
        let Boot { settings, player, wake_pending, wake_rx: mut rx, icons_rx: mut irx, resume, sources, started } =
            boot;
        log::debug!("startup: creating player view at {:.0} ms", crate::ms_since_start());
        // Engine → UI wake-ups.
        cx.spawn_in(window, async move |this, cx| {
            while rx.next().await.is_some() {
                if this.update_in(cx, |v, window, cx| v.on_engine_wake(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        // Periodic UI refresh (time display, auto-hide, A-B loop, OSD expiry).
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if this.update_in(cx, |v, window, cx| v.tick(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut subs = vec![];
        subs.push(cx.observe_window_appearance(window, |v, window, cx| {
            v.theme = Theme::resolve(v.settings.theme, window);
            cx.notify();
        }));
        subs.push(cx.on_app_quit(|v, _| {
            v.save_state();
            async {}
        }));

        // Repaint once the desktop icon theme has been resolved.
        cx.spawn_in(window, async move |this, cx| {
            if irx.next().await.is_some() {
                let _ = this.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();

        let theme = Theme::resolve(settings.theme, window);
        let focus = cx.focus_handle();
        window.focus(&focus);
        let mut v = Self {
            playlist: Playlist::new(settings.repeat, settings.shuffle),
            show_playlist: settings.show_playlist,
            player,
            resume,
            theme,
            focus,
            wake_pending,
            frame: None,
            frame_size: (0, 0),
            retired: VecDeque::new(),
            sub_images: HashMap::new(),
            subs: Vec::new(),
            video_bounds: Rc::new(Cell::new(Bounds::default())),
            sent_target: (0, 0),
            click_pause: None,
            last_activity: Instant::now(),
            pointer_over_controls: false,
            osd: None,
            menu: None,
            dialog: None,
            drag: None,
            seek_hover: None,
            seek_drag_last: Instant::now(),
            playlist_selected: None,
            ab_loop: (None, None),
            loop_file: false,
            aspect: Aspect::Auto,
            zoom: 1.0,
            video_adjust: VideoAdjust::default(),
            sub_delay: 0.0,
            subs_visible: true,
            dvd_titles: None,
            reading_disc: false,
            fitted_for: None,
            pending_fit: None,
            load_counter: 0,
            last_status: Status::Idle,
            was_fullscreen: false,
            inputs: HashMap::new(),
            audio_devices: Vec::new(),
            dropping: false,
            pl_drop: None,
            pl_scroll: gpui::UniformListScrollHandle::new(),
            editing_volume: false,
            volume_blur: None,
            settings,
            _subs: subs,
        };
        window.set_window_title("MavPlayer");
        if !sources.is_empty() {
            v.playlist.add(sources);
            v.probe_playlist(cx);
            v.start_item(0, !started, window, cx);
            if started {
                v.player.set_paused(false);
            }
        }
        if v.settings.start_fullscreen && !window.is_fullscreen() {
            window.toggle_fullscreen();
        }
        #[cfg(target_os = "linux")]
        if crate::integration::is_appimage()
            && !crate::integration::is_integrated()
            && !crate::integration::prompt_marker().exists()
        {
            v.dialog = Some(Dialog::Integrate);
        }
        v
    }

    // ───────────────────────── engine plumbing ─────────────────────────

    fn on_engine_wake(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wake_pending.store(false, Ordering::Release);
        if let Some(f) = self.player.take_frame() {
            if self.frame.is_none()
                && let Some(t0) = crate::START.get()
            {
                log::info!(
                    "first video frame {}x{} at {:.0} ms after launch",
                    f.width,
                    f.height,
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
            self.frame_size = (f.width, f.height);
            if let Some(buf) = image::RgbaImage::from_raw(f.width, f.height, f.bgra) {
                let img = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(buf)]));
                if let Some(old) = self.frame.replace(img) {
                    self.retired.push_back((old, 3));
                }
            }
        }
        for ev in self.player.drain_events() {
            match ev {
                PlayerEvent::Loaded => self.on_loaded(window, cx),
                PlayerEvent::EndOfFile => self.on_end_of_file(window, cx),
                PlayerEvent::Error(e) => {
                    if let Some(i) = self.playlist.current
                        && let Some(it) = self.playlist.items.get_mut(i)
                    {
                        it.failed = true;
                    }
                    self.osd(format!("⚠ {e}"), cx);
                    log::error!("{e}");
                    // Skip unplayable items, but don't loop forever over a broken list.
                    let all_failed = self.playlist.items.iter().all(|i| i.failed);
                    if !all_failed
                        && let Some(n) = self.playlist.next_index(true)
                        && Some(n) != self.playlist.current
                    {
                        self.play_index(n, window, cx);
                    }
                }
                PlayerEvent::Warning(w) => self.osd(w, cx),
                PlayerEvent::TracksChanged => {}
                PlayerEvent::ScreenshotSaved(p) => self.osd(format!("Screenshot saved: {}", p.display()), cx),
            }
        }
        cx.notify();
    }

    fn on_loaded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let st = self.player.state();
        let Some(media) = st.media.clone() else { return };
        let title = media.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| media.source_name.clone());
        window.set_window_title(&format!("{title} — MavPlayer"));
        if let Some(i) = self.playlist.current
            && let Some(it) = self.playlist.items.get_mut(i)
        {
            it.duration = Some(media.duration).filter(|d| *d > 0.0);
            if it.title.is_none() {
                it.title = media.title.clone();
                it.artist = media.artist.clone();
            }
            it.failed = false;
        }
        if let Some(src) = &st.source
            && let Some(pos) = self.resume.get(&src.key()).filter(|_| self.settings.resume_playback)
        {
            self.osd(format!("Resumed at {}", super::widgets::format_time(pos)), cx);
        }
        self.fit_window_if_needed(window, cx);
    }

    fn on_end_of_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(src) = self.player.with_state(|s| s.source.clone()) {
            self.resume.positions.remove(&src.key());
        }
        if self.loop_file {
            self.player.seek(0.0, false, true);
            self.player.set_paused(false);
            return;
        }
        if !self.settings.auto_play_next && self.playlist.repeat != RepeatMode::One {
            return;
        }
        if let Some(n) = self.playlist.next_index(false) {
            self.play_index(n, window, cx);
        } else if self.is_fullscreen(window) {
            // Playlist finished: leave fullscreen like most players do.
            window.toggle_fullscreen();
        }
    }

    pub fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        if let Some((want, tries)) = self.pending_fit {
            let got = window.viewport_size();
            let off = (f32::from(got.width) - f32::from(want.width)).abs()
                + (f32::from(got.height) - f32::from(want.height)).abs();
            if off < 4.0 || tries == 0 || window.is_fullscreen() || window.is_maximized() {
                self.pending_fit = None;
            } else {
                window.resize(want);
                self.pending_fit = Some((want, tries - 1));
            }
        }
        let requests = OPEN_REQUESTS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        if !requests.is_empty() {
            let sources = requests.iter().map(|u| playlist::source_for_arg(u)).collect();
            self.open_sources(sources, window, cx);
        }
        let status = self.player.with_state(|s| s.status.clone());
        if status != self.last_status {
            self.last_status = status.clone();
            changed = true;
        }
        if matches!(status, Status::Playing | Status::Loading) || self.player.with_state(|s| s.buffering) {
            changed = true;
        }
        if let (Some(a), Some(b)) = self.ab_loop
            && b > a
            && self.player.position() >= b
        {
            self.player.seek(a, false, true);
        }
        if let Some((_, t)) = &self.osd
            && t.elapsed() > Duration::from_millis(2200)
        {
            self.osd = None;
            changed = true;
        }
        // Auto-hide transitions.
        let idle = self.last_activity.elapsed().as_secs_f64();
        let hide_after = self.settings.controls_autohide_secs.max(0.5);
        if idle > hide_after && idle < hide_after + 0.5 {
            changed = true;
        }
        if self.was_fullscreen != window.is_fullscreen() {
            self.was_fullscreen = window.is_fullscreen();
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Drops GPU textures of frames that have been off screen for a few
    /// renders (the GPU may still be reading the most recent ones).
    pub fn retire_textures(&mut self, window: &mut Window) {
        for e in self.retired.iter_mut() {
            e.1 = e.1.saturating_sub(1);
        }
        while self.retired.front().is_some_and(|e| e.1 == 0) {
            let (img, _) = self.retired.pop_front().unwrap();
            let _ = window.drop_image(img);
        }
        // Subtitle bitmaps not shown for a while.
        let stale: Vec<u64> = self
            .sub_images
            .iter_mut()
            .filter_map(|(id, (_, age))| {
                *age += 1;
                (*age > 120).then_some(*id)
            })
            .collect();
        for id in stale {
            if let Some((img, _)) = self.sub_images.remove(&id) {
                self.retired.push_back((img, 3));
            }
        }
    }

    pub fn update_target_size(&mut self, window: &Window) {
        let b = self.video_bounds.get();
        let scale = window.scale_factor();
        let w = (f32::from(b.size.width) * scale * self.zoom).round().max(16.0) as u32;
        let h = (f32::from(b.size.height) * scale * self.zoom).round().max(16.0) as u32;
        if (w, h) != self.sent_target && w > 16 && h > 16 {
            self.sent_target = (w, h);
            self.player.set_target_size(w, h);
        }
    }

    // ───────────────────────── opening media ─────────────────────────

    pub fn osd(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        if self.settings.osd_messages {
            self.osd = Some((text.into(), Instant::now()));
            cx.notify();
        }
    }

    pub fn save_current_position(&mut self) {
        let st = self.player.state();
        if let (Some(src), Some(m)) = (st.source, st.media)
            && !m.is_live
            && m.seekable
            && matches!(st.status, Status::Playing | Status::Paused)
        {
            self.resume.remember(src.key(), self.player.position(), m.duration);
        }
    }

    pub fn save_state(&mut self) {
        self.save_current_position();
        self.settings.volume = self.player.volume();
        self.settings.muted = self.player.muted();
        self.settings.repeat = self.playlist.repeat;
        self.settings.shuffle = self.playlist.shuffle();
        self.settings.show_playlist = self.show_playlist;
        self.settings.save();
        self.resume.save();
    }

    pub fn play_index(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_item(idx, true, window, cx);
    }

    /// Makes `idx` the current item. With `load` false the engine is
    /// already playing it (started early by [`Boot`]).
    fn start_item(&mut self, idx: usize, load: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.playlist.items.get(idx).cloned() else { return };
        self.save_current_position();
        self.playlist.current = Some(idx);
        self.playlist_selected = Some(idx);
        self.ab_loop = (None, None);
        self.sub_delay = 0.0;
        self.frame = None;
        self.subs.clear();
        if let MediaSource::Dvd { path, title: 0, .. } = &item.source {
            self.open_dvd_auto(path.clone(), window, cx);
            return;
        }
        self.load_counter += 1;
        if load {
            let start_at = if self.settings.resume_playback { self.resume.get(&item.source.key()) } else { None };
            self.player.load(item.source.clone(), LoadOptions { start_at, ..Default::default() });
        }
        self.settings.add_recent(&item.source);
        window.set_window_title(&format!("{} — MavPlayer", item.display_title()));
        if let MediaSource::Dvd { path, .. } = &item.source
            && self.dvd_titles.as_ref().map(|(p, _)| p != path).unwrap_or(true)
        {
            self.probe_dvd_titles(path.clone(), None, window, cx);
        }
        cx.notify();
    }

    /// Reloads the current item at the current position (after settings
    /// that need a fresh decoder, like hardware decoding).
    pub fn reload_current(&mut self, cx: &mut Context<Self>) {
        let st = self.player.state();
        let Some(src) = st.source else { return };
        let pos = self.player.position();
        let paused = self.player.paused();
        self.player.set_config(self.settings.engine_config());
        self.player.load(src, LoadOptions { start_at: Some(pos), start_paused: paused, ..Default::default() });
        cx.notify();
    }

    pub fn open_sources(&mut self, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        if sources.is_empty() {
            self.osd("No playable files found", cx);
            return;
        }
        let append = self.settings.open_mode == crate::settings::OpenMode::Append && !self.playlist.items.is_empty();
        if !append {
            self.save_current_position();
            self.playlist.clear();
        }
        let range = self.playlist.add(sources);
        self.probe_playlist(cx);
        self.play_index(range.start, window, cx);
    }

    /// Adds without interrupting playback.
    pub fn enqueue(&mut self, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        self.enqueue_at(self.playlist.items.len(), sources, window, cx);
    }

    /// Inserts into the playlist before index `at` without interrupting
    /// playback (starts playing if nothing is).
    pub fn enqueue_at(&mut self, at: usize, sources: Vec<MediaSource>, window: &mut Window, cx: &mut Context<Self>) {
        if sources.is_empty() {
            return;
        }
        let was_empty = self.playlist.items.is_empty();
        let n = sources.len();
        let range = self.playlist.insert(at, sources);
        // Keep the selection on the same item.
        if let Some(sel) = self.playlist_selected.as_mut()
            && *sel >= range.start
        {
            *sel += n;
        }
        self.probe_playlist(cx);
        if was_empty || !self.player.is_active() {
            self.play_index(range.start, window, cx);
        } else {
            self.osd(format!("Added {n} item(s) to the playlist"), cx);
        }
    }

    pub fn open_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // A lone subtitle file dropped onto a playing video loads as subtitles.
        if paths.len() == 1 && self.player.is_active() {
            let ext = paths[0].extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if engine_sub_ext(&ext) {
                self.player.add_subtitle_file(&paths[0]);
                self.osd(format!("Subtitles: {}", paths[0].file_name().unwrap_or_default().to_string_lossy()), cx);
                return;
            }
        }
        if let Some(d) = paths.first().and_then(|p| p.parent()) {
            self.settings.last_dir = Some(d.to_path_buf());
        }
        let sources = playlist::expand_paths(&paths);
        self.open_sources(sources, window, cx);
    }

    fn probe_playlist(&mut self, cx: &mut Context<Self>) {
        let todo: Vec<(u64, MediaSource)> = self
            .playlist
            .items
            .iter_mut()
            .filter(|i| !i.probed && matches!(i.source, MediaSource::File(_)))
            .map(|i| {
                i.probed = true;
                (i.id, i.source.clone())
            })
            .take(2000)
            .collect();
        if todo.is_empty() {
            return;
        }
        let cfg = self.settings.engine_config();
        let task = cx.background_executor().spawn(async move {
            todo.into_iter().map(|(id, s)| (id, engine::probe::probe(&s, &cfg))).collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let results = task.await;
            let _ = this.update(cx, |v, cx| {
                for (id, r) in results {
                    if let (Some(it), Some(r)) = (v.playlist.items.iter_mut().find(|i| i.id == id), r) {
                        it.duration = Some(r.duration).filter(|d| *d > 0.0);
                        it.title = it.title.take().or(r.title);
                        it.artist = it.artist.take().or(r.artist);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn prompt_open(&mut self, dirs_mode: bool, subtitle: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: !dirs_mode,
            directories: dirs_mode,
            multiple: !subtitle,
            prompt: Some(if subtitle { "Load Subtitles" } else { "Open" }.into()),
        });
        cx.spawn_in(window, async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) if !paths.is_empty() => {
                let _ = this.update_in(cx, |v, window, cx| {
                    if subtitle {
                        v.player.add_subtitle_file(&paths[0]);
                    } else {
                        v.open_paths(paths, window, cx);
                    }
                });
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |v, cx| v.osd(format!("File dialog failed: {e}"), cx));
            }
            _ => {}
        })
        .detach();
    }

    /// Opens a disc: reads the title list first so the main feature plays.
    pub fn open_dvd_auto(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reading_disc = true;
        self.player.stop();
        self.osd("Reading disc…", cx);
        self.probe_dvd_titles(path, Some(()), window, cx);
    }

    fn probe_dvd_titles(&mut self, path: String, then_play: Option<()>, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.settings.engine_config();
        let p = path.clone();
        let task = cx.background_executor().spawn(async move { disc::probe_titles(&p, &cfg, 99) });
        cx.spawn_in(window, async move |this, cx| {
            let titles = task.await;
            let _ = this.update_in(cx, |v, window, cx| {
                let main = disc::main_title(&titles);
                v.dvd_titles = Some((path.clone(), titles));
                if then_play.is_some() {
                    v.reading_disc = false;
                    match main {
                        Some(t) => v.play_dvd_title(path, t, window, cx),
                        None => v.osd("Could not read the disc (no titles found)", cx),
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn play_dvd_title(&mut self, path: String, title: u32, window: &mut Window, cx: &mut Context<Self>) {
        let src = MediaSource::Dvd { path, title, angle: 1 };
        let idx = self.playlist.current.filter(|&i| matches!(self.playlist.items[i].source, MediaSource::Dvd { .. }));
        match idx {
            Some(i) => {
                self.playlist.items[i].source = src.clone();
                self.playlist.items[i].title = Some(format!("DVD – Title {title}"));
                let start_at = if self.settings.resume_playback { self.resume.get(&src.key()) } else { None };
                self.player.load(src.clone(), LoadOptions { start_at, ..Default::default() });
                self.settings.add_recent(&src);
                self.load_counter += 1;
                window.set_window_title(&format!("DVD – Title {title} — MavPlayer"));
            }
            None => {
                self.playlist.clear();
                self.playlist.add(vec![src]);
                self.play_index(0, window, cx);
            }
        }
        cx.notify();
    }

    fn fit_window_if_needed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fitted_for == Some(self.load_counter) {
            return;
        }
        self.fitted_for = Some(self.load_counter);
        if self.settings.fit_window_to_video {
            self.fit_window(window, cx);
        }
    }

    pub fn fit_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.is_fullscreen() || window.is_maximized() {
            return;
        }
        let Some(v) = self.player.with_state(|s| s.video.clone()) else { return };
        if self.player.with_state(|s| s.media.as_ref().map(|m| !m.has_video()).unwrap_or(true)) {
            return;
        }
        let aspect = match self.aspect {
            Aspect::Ratio(r, _) => r,
            _ => v.display_aspect,
        };
        if v.height <= 0 || !aspect.is_finite() {
            return;
        }
        let scale = window.scale_factor() as f64;
        let mut vh = v.height as f64 / scale;
        let mut vw = vh * aspect;
        // Wayland only reports the window's output once it has been entered, and
        // never a "primary" one; fall back to the smallest known output.
        let screen = window
            .display(cx)
            .or_else(|| cx.primary_display())
            .map(|d| d.bounds().size)
            .or_else(|| {
                cx.displays()
                    .iter()
                    .map(|d| d.bounds().size)
                    .min_by(|a, b| a.height.partial_cmp(&b.height).unwrap_or(std::cmp::Ordering::Equal))
            })
            .unwrap_or(size(px(1920.), px(1080.)));
        let (sw, sh) = (f32::from(screen.width) as f64 * 0.85, f32::from(screen.height) as f64 * 0.85);
        let chrome_h =
            f32::from(window.viewport_size().height) as f64 - f32::from(self.video_bounds.get().size.height) as f64;
        let side = if self.show_playlist { 320.0 } else { 0.0 };
        let k = ((sw - side) / vw).min((sh - chrome_h) / vh).min(1.0);
        vw *= k;
        vh *= k;
        let (w, h) = ((vw + side).max(480.0), (vh + chrome_h).max(320.0));
        let want = size(px(w as f32), px(h as f32));
        window.resize(want);
        self.pending_fit = Some((want, 4));
    }

    pub fn is_fullscreen(&self, window: &Window) -> bool {
        window.is_fullscreen()
    }

    pub fn input(
        &mut self,
        key: &'static str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        if let Some(e) = self.inputs.get(key) {
            return e.clone();
        }
        let ph = placeholder.to_string();
        let e = cx.new(|cx| TextInput::new(cx, ph));
        let sub = cx.subscribe_in(&e, window, move |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Submit => this.on_input_submit(key, window, cx),
            InputEvent::Cancel if key == "volume" => this.finish_volume_edit(false, window, cx),
            InputEvent::Cancel => this.close_dialog(window, cx),
            InputEvent::Changed => this.on_input_changed(key, cx),
        });
        self._subs.push(sub);
        self.inputs.insert(key, e.clone());
        e
    }

    /// Swaps the volume percentage for a text field (double-click on it).
    pub fn start_volume_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input("volume", "%", window, cx);
        let t = self.theme;
        let pct = format!("{:.0}", self.player.volume() * 100.0);
        input.update(cx, |i, cx| {
            i.colors = (t.text, t.text_disabled, t.accent_soft, t.accent);
            i.set_text(pct, cx);
        });
        let fh = input.focus_handle(cx);
        window.focus(&fh);
        self.editing_volume = true;
        // Clicking elsewhere applies what was typed.
        self.volume_blur = Some(cx.on_blur(&fh, window, |this, window, cx| this.finish_volume_edit(true, window, cx)));
        cx.notify();
    }

    /// Ends volume editing; `apply` sets the typed percentage (Enter / blur),
    /// otherwise it is discarded (Escape).
    pub fn finish_volume_edit(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editing_volume {
            return;
        }
        self.editing_volume = false;
        self.volume_blur = None;
        if apply {
            let text = self.inputs.get("volume").map(|i| i.read(cx).text()).unwrap_or_default();
            match parse_volume(&text) {
                Some(pct) => self.set_volume(pct / 100.0, cx),
                None => self.osd("Type a volume like 80 or 80%", cx),
            }
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Dialog::Integrate) = self.dialog {
            let _ = std::fs::create_dir_all(crate::settings::config_dir());
            let _ = std::fs::write(crate::integration::prompt_marker(), b"asked");
        }
        if self.dialog.take().is_some() {
            self.settings.save();
        }
        window.focus(&self.focus);
        cx.notify();
    }

    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        self.player.set_config(self.settings.engine_config());
        icons::set_use_system_theme(self.settings.icon_theme == crate::settings::IconTheme::System);
        self.settings.save();
        cx.notify();
    }

    // ───────────────────────── track helpers ─────────────────────────

    pub fn select_track(&mut self, kind: TrackKind, id: Option<TrackId>, cx: &mut Context<Self>) {
        self.player.select_track(kind, id);
        let label = self.track_label(kind, id);
        let what = match kind {
            TrackKind::Audio => "Audio",
            TrackKind::Subtitle => "Subtitles",
            TrackKind::Video => "Video",
        };
        if kind == TrackKind::Subtitle {
            self.subs_visible = true;
        }
        self.osd(format!("{what}: {label}"), cx);
    }

    pub fn track_label(&self, kind: TrackKind, id: Option<TrackId>) -> String {
        let Some(id) = id else { return "Off".into() };
        self.player.with_state(|s| {
            s.media
                .as_ref()
                .and_then(|m| m.tracks_of(kind).enumerate().find(|(_, t)| t.id == id).map(|(n, t)| t.label(n + 1)))
                .unwrap_or_else(|| "?".into())
        })
    }

    fn cycle_track(&mut self, kind: TrackKind, cx: &mut Context<Self>) {
        let (ids, cur) = self.player.with_state(|s| {
            let ids: Vec<TrackId> = s
                .media
                .as_ref()
                .map(|m| m.tracks_of(kind).filter(|t| !t.is_cover_art).map(|t| t.id).collect())
                .unwrap_or_default();
            let cur = match kind {
                TrackKind::Audio => s.selected.audio,
                TrackKind::Subtitle => s.selected.subtitle,
                TrackKind::Video => s.selected.video,
            };
            (ids, cur)
        });
        if ids.is_empty() {
            self.osd(if kind == TrackKind::Audio { "No audio tracks" } else { "No subtitles" }, cx);
            return;
        }
        let mut options: Vec<Option<TrackId>> = ids.into_iter().map(Some).collect();
        if kind == TrackKind::Subtitle {
            options.push(None);
        }
        let pos = options.iter().position(|o| *o == cur).map(|p| (p + 1) % options.len()).unwrap_or(0);
        self.select_track(kind, options[pos], cx);
    }

    pub fn set_volume(&mut self, v: f64, cx: &mut Context<Self>) {
        let max = self.settings.max_volume as f64 / 100.0;
        let v = v.clamp(0.0, max);
        self.player.set_volume(v);
        self.player.set_muted(false);
        self.settings.volume = v;
        self.osd(format!("Volume {:.0}%", v * 100.0), cx);
    }

    pub fn set_speed(&mut self, s: f64, cx: &mut Context<Self>) {
        self.player.set_speed(s);
        self.osd(format!("Speed {:.2}×", self.player.speed()), cx);
    }

    pub fn set_video_adjust(&mut self, adj: VideoAdjust, cx: &mut Context<Self>) {
        self.video_adjust = adj.clone();
        self.player.set_video_adjust(adj);
        cx.notify();
    }

    pub fn seek_relative(&mut self, secs: f64, precise: bool, cx: &mut Context<Self>) {
        if !self.player.is_active() {
            return;
        }
        self.player.seek(secs, true, precise || self.settings.exact_seeks);
        let target = (self.player.position()).max(0.0);
        let d = self.player.duration();
        self.osd(format!("{} / {}", super::widgets::format_time(target), super::widgets::format_time(d)), cx);
        cx.notify();
    }

    pub fn seek_fraction(&mut self, f: f64, cx: &mut Context<Self>) {
        let d = self.player.duration();
        if d > 0.0 {
            self.player.seek(d * f.clamp(0.0, 1.0), false, true);
            cx.notify();
        }
    }

    pub fn next_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(n) = self.playlist.next_index(true) {
            self.play_index(n, window, cx);
        } else {
            self.osd("End of playlist", cx);
        }
    }

    pub fn prev_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Like most players: restart the current item unless near its start.
        if self.player.position() > 5.0 {
            self.player.seek(0.0, false, true);
            return;
        }
        if let Some(p) = self.playlist.prev_index() {
            self.play_index(p, window, cx);
        }
    }

    pub fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_current_position();
        self.player.stop();
        self.frame = None;
        self.subs.clear();
        window.set_window_title("MavPlayer");
        cx.notify();
    }

    pub fn chapter_jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        let chapters = self.player.with_state(|s| s.media.as_ref().map(|m| m.chapters.clone()).unwrap_or_default());
        if chapters.is_empty() {
            let step = self.settings.seek_step_large;
            self.seek_relative(if forward { step } else { -step }, false, cx);
            return;
        }
        let pos = self.player.position();
        let target = if forward {
            chapters.iter().find(|c| c.start > pos + 0.5)
        } else {
            chapters.iter().rev().find(|c| c.start < pos - 2.0).or(chapters.first())
        };
        if let Some(c) = target {
            let (start, title) = (c.start, c.title.clone());
            self.player.seek(start, false, true);
            self.osd(format!("Chapter: {title}"), cx);
        }
    }

    pub fn cycle_repeat(&mut self, cx: &mut Context<Self>) {
        self.playlist.repeat = match self.playlist.repeat {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.settings.repeat = self.playlist.repeat;
        let s = match self.playlist.repeat {
            RepeatMode::Off => "Repeat: off",
            RepeatMode::All => "Repeat: all",
            RepeatMode::One => "Repeat: current item",
        };
        self.osd(s, cx);
    }

    pub fn screenshot(&mut self, cx: &mut Context<Self>) {
        if !self.player.has_frame() {
            self.osd("No picture to capture", cx);
            return;
        }
        let name = self
            .player
            .with_state(|s| s.source.as_ref().map(|x| x.display_name()))
            .unwrap_or_else(|| "mavplayer".into());
        let stem = Path::new(&name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(name);
        let t = self.player.position();
        let ms = (t * 1000.0) as u64;
        let file = format!(
            "{}_{:02}-{:02}-{:02}.{:03}.png",
            stem.replace(['/', '\\', ':'], "_"),
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            ms % 1000
        );
        self.player.screenshot(self.settings.screenshot_dir().join(file));
    }

    // ───────────────────────── action handlers ─────────────────────────

    pub fn register_actions<E: gpui::InteractiveElement>(&self, el: E, cx: &mut Context<Self>) -> E {
        macro_rules! on {
            ($el:expr, $($action:ident => |$this:ident, $w:ident, $cx:ident| $body:expr),* $(,)?) => {
                $el$(.on_action(cx.listener(move |$this: &mut Self, _: &$action, $w: &mut Window, $cx: &mut Context<Self>| { $body; $cx.notify(); })))*
            };
        }
        let step = self.settings.seek_step;
        let big = self.settings.seek_step_large;
        let vstep = self.settings.volume_step / 100.0;
        on!(el,
            TogglePause => |this, _w, _cx| this.player.toggle_pause(),
            Stop => |this, w, cx| this.stop(w, cx),
            PlayNext => |this, w, cx| this.next_item(w, cx),
            PlayPrevious => |this, w, cx| this.prev_item(w, cx),
            SeekForward => |this, _w, cx| this.seek_relative(step, false, cx),
            SeekBackward => |this, _w, cx| this.seek_relative(-step, false, cx),
            SeekForwardSmall => |this, _w, cx| this.seek_relative(1.0, true, cx),
            SeekBackwardSmall => |this, _w, cx| this.seek_relative(-1.0, true, cx),
            SeekForwardLarge => |this, _w, cx| this.seek_relative(big, false, cx),
            SeekBackwardLarge => |this, _w, cx| this.seek_relative(-big, false, cx),
            NextChapter => |this, _w, cx| this.chapter_jump(true, cx),
            PreviousChapter => |this, _w, cx| this.chapter_jump(false, cx),
            FrameStep => |this, _w, _cx| this.player.frame_step(true),
            FrameBackStep => |this, _w, _cx| this.player.frame_step(false),
            VolumeUp => |this, _w, cx| { let v = this.player.volume() + vstep; this.set_volume(v, cx) },
            VolumeDown => |this, _w, cx| { let v = this.player.volume() - vstep; this.set_volume(v, cx) },
            ToggleMute => |this, _w, cx| {
                let m = !this.player.muted();
                this.player.set_muted(m);
                this.settings.muted = m;
                this.osd(if m { "Muted" } else { "Unmuted" }, cx)
            },
            SpeedUp => |this, _w, cx| {
                let s = this.player.speed();
                let n = SPEEDS.iter().copied().find(|&x| x > s + 1e-3).unwrap_or(4.0);
                this.set_speed(n, cx)
            },
            SpeedDown => |this, _w, cx| {
                let s = this.player.speed();
                let n = SPEEDS.iter().rev().copied().find(|&x| x < s - 1e-3).unwrap_or(0.25);
                this.set_speed(n, cx)
            },
            SpeedReset => |this, _w, cx| this.set_speed(1.0, cx),
            CycleAudio => |this, _w, cx| this.cycle_track(TrackKind::Audio, cx),
            CycleSubtitle => |this, _w, cx| this.cycle_track(TrackKind::Subtitle, cx),
            ToggleSubtitles => |this, _w, cx| {
                this.subs_visible = !this.subs_visible;
                let v = this.subs_visible;
                this.osd(if v { "Subtitles shown" } else { "Subtitles hidden" }, cx)
            },
            SubDelayIncrease => |this, _w, cx| { this.sub_delay += 0.1; let d = this.sub_delay; this.osd(format!("Subtitle delay {:+.0} ms", d * 1000.0), cx) },
            SubDelayDecrease => |this, _w, cx| { this.sub_delay -= 0.1; let d = this.sub_delay; this.osd(format!("Subtitle delay {:+.0} ms", d * 1000.0), cx) },
            AudioDelayIncrease => |this, _w, cx| { let d = this.player.audio_delay() + 0.1; this.player.set_audio_delay(d); this.osd(format!("Audio delay {:+.0} ms", d * 1000.0), cx) },
            AudioDelayDecrease => |this, _w, cx| { let d = this.player.audio_delay() - 0.1; this.player.set_audio_delay(d); this.osd(format!("Audio delay {:+.0} ms", d * 1000.0), cx) },
            SubScaleUp => |this, _w, cx| { this.settings.sub_scale = (this.settings.sub_scale + 0.1).min(3.0); let s = this.settings.sub_scale; this.osd(format!("Subtitle size {:.0}%", s * 100.0), cx) },
            SubScaleDown => |this, _w, cx| { this.settings.sub_scale = (this.settings.sub_scale - 0.1).max(0.3); let s = this.settings.sub_scale; this.osd(format!("Subtitle size {:.0}%", s * 100.0), cx) },
            ToggleFullscreen => |_this, w, _cx| w.toggle_fullscreen(),
            ExitFullscreen => |this, w, cx| {
                if this.menu.is_some() { this.close_menu(cx) }
                else if this.dialog.is_some() { this.close_dialog(w, cx) }
                else if w.is_fullscreen() { w.toggle_fullscreen() }
            },
            Screenshot => |this, _w, cx| this.screenshot(cx),
            ShowMediaInfo => |this, w, cx| this.show_dialog(Dialog::MediaInfo, w, cx),
            ToggleAbLoop => |this, _w, cx| {
                let pos = this.player.position();
                this.ab_loop = match this.ab_loop {
                    (None, _) => (Some(pos), None),
                    (Some(a), None) if pos > a + 0.1 => (Some(a), Some(pos)),
                    _ => (None, None),
                };
                let msg = match this.ab_loop {
                    (Some(a), None) => format!("Loop A: {}", super::widgets::format_time(a)),
                    (Some(a), Some(b)) => format!("Loop {} → {}", super::widgets::format_time(a), super::widgets::format_time(b)),
                    _ => "A-B loop cleared".into(),
                };
                this.osd(msg, cx)
            },
            ToggleLoopFile => |this, _w, cx| { this.loop_file = !this.loop_file; let l = this.loop_file; this.osd(if l { "Loop file: on" } else { "Loop file: off" }, cx) },
            CycleRepeat => |this, _w, cx| this.cycle_repeat(cx),
            ToggleShuffle => |this, _w, cx| {
                let s = !this.playlist.shuffle();
                this.playlist.set_shuffle(s);
                this.settings.shuffle = s;
                this.osd(if s { "Shuffle: on" } else { "Shuffle: off" }, cx)
            },
            OpenFile => |this, w, cx| this.prompt_open(false, false, w, cx),
            OpenFolder => |this, w, cx| this.prompt_open(true, false, w, cx),
            OpenUrl => |this, w, cx| this.show_dialog(Dialog::OpenUrl, w, cx),
            OpenDisc => |this, w, cx| this.show_dialog(Dialog::OpenDisc, w, cx),
            OpenSubtitle => |this, w, cx| {
                if this.player.is_active() { this.prompt_open(false, true, w, cx) } else { this.osd("Open a video first", cx) }
            },
            SavePlaylist => |this, w, cx| this.save_playlist(w, cx),
            ShowPreferences => |this, w, cx| this.show_dialog(Dialog::Settings(0), w, cx),
            ShowEffects => |this, w, cx| this.show_dialog(Dialog::Effects(0), w, cx),
            TogglePlaylist => |this, _w, _cx| { this.show_playlist = !this.show_playlist; this.settings.show_playlist = this.show_playlist },
            ToggleMenubar => |this, _w, cx| {
                this.settings.show_menubar = !this.settings.show_menubar;
                this.settings.save();
                if !this.settings.show_menubar { this.osd("Menu bar hidden — right-click or press Ctrl+M to bring it back", cx) }
            },
            JumpToTime => |this, w, cx| this.show_dialog(Dialog::JumpTo, w, cx),
            CycleAspect => |this, _w, cx| {
                let i = ASPECTS.iter().position(|a| *a == this.aspect).map(|i| (i + 1) % ASPECTS.len()).unwrap_or(0);
                this.aspect = ASPECTS[i];
                let l = this.aspect.label();
                this.osd(format!("Aspect ratio: {l}"), cx)
            },
            CycleDeinterlace => |this, _w, cx| {
                this.settings.deinterlace = match this.settings.deinterlace { Deinterlace::Auto => Deinterlace::On, Deinterlace::On => Deinterlace::Off, Deinterlace::Off => Deinterlace::Auto };
                this.apply_settings(cx);
                let s = format!("Deinterlacing: {:?}", this.settings.deinterlace);
                this.osd(s, cx)
            },
            ToggleHwdec => |this, _w, cx| {
                this.settings.hwdec = if this.settings.hwdec == HwDecode::Off { HwDecode::Always } else { HwDecode::Off };
                this.apply_settings(cx);
                this.reload_current(cx);
                let s = if this.settings.hwdec == HwDecode::Off { "Hardware decoding: off" } else { "Hardware decoding: on" };
                this.osd(s, cx)
            },
            RotateClockwise => |this, _w, cx| {
                let mut a = this.video_adjust.clone();
                a.rotation = match a.rotation { Rotation::None => Rotation::R90, Rotation::R90 => Rotation::R180, Rotation::R180 => Rotation::R270, Rotation::R270 => Rotation::None };
                let deg = match a.rotation { Rotation::None => 0, Rotation::R90 => 90, Rotation::R180 => 180, Rotation::R270 => 270 };
                this.set_video_adjust(a, cx);
                this.osd(format!("Rotation {deg}°"), cx)
            },
            ZoomIn => |this, _w, cx| { this.zoom = (this.zoom * 1.1).min(8.0); let z = this.zoom; this.osd(format!("Zoom {:.0}%", z * 100.0), cx) },
            ZoomOut => |this, _w, cx| { this.zoom = (this.zoom / 1.1).max(0.25); let z = this.zoom; this.osd(format!("Zoom {:.0}%", z * 100.0), cx) },
            ZoomReset => |this, _w, cx| { this.zoom = 1.0; this.aspect = Aspect::Auto; this.osd("Zoom reset", cx) },
            FitWindow => |this, w, cx| this.fit_window(w, cx),
            ShowShortcuts => |this, w, cx| this.show_dialog(Dialog::Shortcuts, w, cx),
            ShowAbout => |this, w, cx| this.show_dialog(Dialog::About, w, cx),
            CloseOverlay => |this, w, cx| { this.close_menu(cx); this.close_dialog(w, cx) },
            Quit => |this, _w, cx| { this.save_state(); cx.quit() },
            Seek0 => |this, _w, cx| this.seek_fraction(0.0, cx),
            Seek1 => |this, _w, cx| this.seek_fraction(0.1, cx),
            Seek2 => |this, _w, cx| this.seek_fraction(0.2, cx),
            Seek3 => |this, _w, cx| this.seek_fraction(0.3, cx),
            Seek4 => |this, _w, cx| this.seek_fraction(0.4, cx),
            Seek5 => |this, _w, cx| this.seek_fraction(0.5, cx),
            Seek6 => |this, _w, cx| this.seek_fraction(0.6, cx),
            Seek7 => |this, _w, cx| this.seek_fraction(0.7, cx),
            Seek8 => |this, _w, cx| this.seek_fraction(0.8, cx),
            Seek9 => |this, _w, cx| this.seek_fraction(0.9, cx),
        )
    }

    pub fn save_playlist(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist.items.is_empty() {
            self.osd("The playlist is empty", cx);
            return;
        }
        let dir = self.settings.last_dir.clone().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("playlist.m3u"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let _ = this.update(cx, |v, cx| match v.playlist.save_m3u(&path) {
                    Ok(()) => v.osd(format!("Playlist saved: {}", path.display()), cx),
                    Err(e) => v.osd(format!("Could not save playlist: {e}"), cx),
                });
            }
        })
        .detach();
    }

    pub fn note_activity(&mut self, cx: &mut Context<Self>) {
        let was_hidden = !self.controls_visible();
        self.last_activity = Instant::now();
        if was_hidden {
            cx.notify();
        }
    }

    pub fn controls_visible(&self) -> bool {
        self.last_activity.elapsed().as_secs_f64() < self.settings.controls_autohide_secs.max(0.5)
            || self.pointer_over_controls
            || self.menu.is_some()
            || self.dialog.is_some()
            || self.drag.is_some()
            || self.player.paused()
            || !self.player.is_active()
    }
}

fn engine_sub_ext(ext: &str) -> bool {
    matches!(ext, "srt" | "ass" | "ssa" | "vtt" | "sub" | "idx" | "smi" | "sup" | "ttml" | "lrc")
}

/// Parses a typed volume percentage: "80", "80%", " 72.5 % ".
pub fn parse_volume(text: &str) -> Option<f64> {
    let n: f64 = text.trim().trim_end_matches('%').trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::parse_volume;

    #[test]
    fn volume_parsing() {
        assert_eq!(parse_volume("80"), Some(80.0));
        assert_eq!(parse_volume(" 72.5 % "), Some(72.5));
        assert_eq!(parse_volume("120%"), Some(120.0));
        assert_eq!(parse_volume("-5"), None);
        assert_eq!(parse_volume("loud"), None);
        assert_eq!(parse_volume(""), None);
    }
}
