//! User settings and small bits of persistent state (recent files, resume
//! positions), stored as JSON in the platform config directory.

use crate::engine::{AudioFx, ChannelMode, Deinterlace, EngineConfig, HwDecode, MediaSource, ScaleQuality};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IconTheme {
    /// Use the desktop's icon theme where available (Linux), Papirus otherwise.
    System,
    Papirus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollAction {
    Volume,
    Seek,
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubStyle {
    Outline,
    Box,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    /// Opening files replaces the playlist.
    Replace,
    /// Opening files appends to the playlist and plays the first one.
    Append,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Interface
    pub theme: ThemeMode,
    pub icon_theme: IconTheme,
    pub show_menubar: bool,
    pub osd_messages: bool,
    pub controls_autohide_secs: f64,
    pub show_remaining_time: bool,
    pub fit_window_to_video: bool,
    pub single_click_pause: bool,
    pub double_click_fullscreen: bool,
    pub scroll_action: ScrollAction,
    pub show_playlist: bool,
    // Playback
    pub resume_playback: bool,
    pub auto_play_next: bool,
    pub open_mode: OpenMode,
    pub start_fullscreen: bool,
    pub seek_step: f64,
    pub seek_step_large: f64,
    pub volume_step: f64,
    pub max_volume: u32,
    pub exact_seeks: bool,
    pub screenshot_dir: Option<PathBuf>,
    // Video
    pub hwdec: HwDecode,
    pub deinterlace: Deinterlace,
    pub scale_quality: ScaleQuality,
    pub tone_map_hdr: bool,
    // Audio
    pub audio_device: Option<String>,
    pub channels: ChannelMode,
    pub audio_langs: String,
    pub audio_fx: AudioFx,
    // Subtitles
    pub sub_langs: String,
    pub subs_enabled_by_default: bool,
    pub auto_load_subs: bool,
    pub sub_scale: f32,
    pub sub_margin: f32,
    pub sub_style: SubStyle,
    pub sub_charset: String,
    // Network & discs
    pub network_cache_secs: f64,
    pub dvd_region: u32,
    pub dvd_device: Option<String>,
    // Advanced
    pub decoder_threads: u32,
    // Remembered state
    pub volume: f64,
    pub muted: bool,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub window_size: Option<(f32, f32)>,
    pub recent: Vec<MediaSource>,
    pub last_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            icon_theme: IconTheme::System,
            show_menubar: true,
            osd_messages: true,
            controls_autohide_secs: 2.0,
            show_remaining_time: false,
            fit_window_to_video: true,
            single_click_pause: false,
            double_click_fullscreen: true,
            scroll_action: ScrollAction::Volume,
            show_playlist: false,
            resume_playback: true,
            auto_play_next: true,
            open_mode: OpenMode::Replace,
            start_fullscreen: false,
            seek_step: 5.0,
            seek_step_large: 60.0,
            volume_step: 5.0,
            max_volume: 130,
            exact_seeks: true,
            screenshot_dir: None,
            hwdec: HwDecode::Auto,
            deinterlace: Deinterlace::Auto,
            scale_quality: ScaleQuality::Good,
            tone_map_hdr: true,
            audio_device: None,
            channels: ChannelMode::Auto,
            audio_langs: String::new(),
            audio_fx: AudioFx::default(),
            sub_langs: String::new(),
            subs_enabled_by_default: false,
            auto_load_subs: true,
            sub_scale: 1.0,
            sub_margin: 0.05,
            sub_style: SubStyle::Outline,
            sub_charset: String::new(),
            network_cache_secs: 10.0,
            dvd_region: 0,
            dvd_device: None,
            decoder_threads: 0,
            volume: 1.0,
            muted: false,
            repeat: RepeatMode::Off,
            shuffle: false,
            window_size: None,
            recent: Vec::new(),
            last_dir: None,
        }
    }
}

fn split_langs(s: &str) -> Vec<String> {
    s.split([',', ' ', ';']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

impl Settings {
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            hwdec: self.hwdec,
            deinterlace: self.deinterlace,
            channels: self.channels,
            audio_device: self.audio_device.clone(),
            scale_quality: self.scale_quality,
            tone_map_hdr: self.tone_map_hdr,
            audio_langs: split_langs(&self.audio_langs),
            sub_langs: split_langs(&self.sub_langs),
            subs_enabled_by_default: self.subs_enabled_by_default,
            auto_load_subs: self.auto_load_subs,
            sub_charset: Some(self.sub_charset.trim().to_string()).filter(|s| !s.is_empty()),
            network_cache_secs: self.network_cache_secs,
            decoder_threads: self.decoder_threads,
            dvd_region: self.dvd_region,
            ..EngineConfig::default()
        }
    }

    pub fn add_recent(&mut self, src: &MediaSource) {
        self.recent.retain(|r| r != src);
        self.recent.insert(0, src.clone());
        self.recent.truncate(15);
    }

    pub fn screenshot_dir(&self) -> PathBuf {
        self.screenshot_dir
            .clone()
            .or_else(|| dirs::picture_dir().map(|p| p.join("MavPlayer")))
            .or_else(|| dirs::home_dir().map(|p| p.join("MavPlayer Screenshots")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MAVPLAYER_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("mavplayer")
}

fn read_json<T: for<'de> Deserialize<'de>>(p: &Path) -> Option<T> {
    let data = std::fs::read(p).ok()?;
    match serde_json::from_slice(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("ignoring unreadable {}: {e}", p.display());
            None
        }
    }
}

/// Writes atomically so a crash never leaves a half-written file.
fn write_json<T: Serialize>(p: &Path, v: &T) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, p)
}

impl Settings {
    pub fn load() -> Self {
        read_json(&config_dir().join("settings.json")).unwrap_or_default()
    }
    pub fn save(&self) {
        if let Err(e) = write_json(&config_dir().join("settings.json"), self) {
            log::warn!("could not save settings: {e}");
        }
    }
}

/// Remembered playback positions, keyed by `MediaSource::key()`.
#[derive(Default, Serialize, Deserialize)]
pub struct ResumeStore {
    pub positions: HashMap<String, f64>,
}

impl ResumeStore {
    pub fn load() -> Self {
        read_json(&config_dir().join("resume.json")).unwrap_or_default()
    }
    pub fn save(&self) {
        if let Err(e) = write_json(&config_dir().join("resume.json"), self) {
            log::warn!("could not save resume positions: {e}");
        }
    }
    pub fn get(&self, key: &str) -> Option<f64> {
        self.positions.get(key).copied()
    }
    /// Stores a position unless it's at the very start or end.
    pub fn remember(&mut self, key: String, pos: f64, duration: f64) {
        if duration > 60.0 && pos > 10.0 && pos < duration - 20.0 {
            self.positions.insert(key, pos);
        } else {
            self.positions.remove(&key);
        }
        if self.positions.len() > 500 {
            let k: Vec<String> = self.positions.keys().take(100).cloned().collect();
            for k in k {
                self.positions.remove(&k);
            }
        }
    }
}
