//! Public, FFmpeg-free data types shared between the engine and the UI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// What to play.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaSource {
    File(PathBuf),
    Url(String),
    /// A DVD-Video disc: a device (`/dev/sr0`, `D:`), an ISO image, or a
    /// folder containing `VIDEO_TS`. `title` 0 means "pick automatically".
    Dvd {
        path: String,
        title: u32,
        angle: u32,
    },
}

impl MediaSource {
    pub fn display_name(&self) -> String {
        match self {
            MediaSource::File(p) => {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
            }
            MediaSource::Url(u) => {
                let trimmed = u.split(['?', '#']).next().unwrap_or(u);
                trimmed.rsplit('/').find(|s| !s.is_empty()).map(percent_decode).unwrap_or_else(|| u.clone())
            }
            MediaSource::Dvd { path, title, .. } => {
                let base = std::path::Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                if *title == 0 { format!("DVD ({base})") } else { format!("DVD ({base}) – Title {title}") }
            }
        }
    }

    /// Stable key used for resume positions and the recent list.
    pub fn key(&self) -> String {
        match self {
            MediaSource::File(p) => p.display().to_string(),
            MediaSource::Url(u) => u.clone(),
            MediaSource::Dvd { path, title, .. } => format!("dvd://{title}/{path}"),
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// Identifies a track. Embedded tracks use the container stream index,
/// external subtitle files get their own id space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrackId {
    Stream(usize),
    External(usize),
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub hearing_impaired: bool,
    // video
    pub width: i32,
    pub height: i32,
    pub fps: f64,
    pub is_cover_art: bool,
    // audio
    pub channels: i32,
    pub channel_layout: String,
    pub sample_rate: i32,
    pub bitrate: i64,
    pub external_path: Option<PathBuf>,
}

impl Track {
    pub fn label(&self, n: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = &self.title {
            parts.push(t.clone());
        }
        if let Some(l) = &self.language {
            parts.push(language_name(l));
        }
        let mut s = if parts.is_empty() { format!("Track {n}") } else { parts.join(" – ") };
        let mut tags: Vec<String> = vec![self.codec.clone()];
        match self.kind {
            TrackKind::Audio if self.channels > 0 => tags.push(self.channel_layout.clone()),
            TrackKind::Video if self.width > 0 => tags.push(format!("{}×{}", self.width, self.height)),
            _ => {}
        }
        if self.forced {
            tags.push("forced".into());
        }
        if self.hearing_impaired {
            tags.push("SDH".into());
        }
        if self.external_path.is_some() {
            tags.push("external".into());
        }
        s.push_str(&format!("  [{}]", tags.join(", ")));
        s
    }
}

/// Expands common ISO 639 language codes.
pub fn language_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "en" | "eng" => "English",
        "fr" | "fre" | "fra" => "French",
        "de" | "ger" | "deu" => "German",
        "es" | "spa" => "Spanish",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "ru" | "rus" => "Russian",
        "ja" | "jpn" => "Japanese",
        "zh" | "chi" | "zho" => "Chinese",
        "ko" | "kor" => "Korean",
        "nl" | "dut" | "nld" => "Dutch",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nob" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "pl" | "pol" => "Polish",
        "cs" | "cze" | "ces" => "Czech",
        "hu" | "hun" => "Hungarian",
        "tr" | "tur" => "Turkish",
        "el" | "gre" | "ell" => "Greek",
        "he" | "heb" => "Hebrew",
        "ar" | "ara" => "Arabic",
        "hi" | "hin" => "Hindi",
        "th" | "tha" => "Thai",
        "uk" | "ukr" => "Ukrainian",
        "ro" | "rum" | "ron" => "Romanian",
        "bg" | "bul" => "Bulgarian",
        "hr" | "hrv" => "Croatian",
        "sr" | "srp" => "Serbian",
        "vi" | "vie" => "Vietnamese",
        "id" | "ind" => "Indonesian",
        "und" => "Undetermined",
        _ => return code.to_string(),
    };
    name.to_string()
}

/// Does `lang` (a stream tag) match a user preference such as "en" / "eng"?
pub fn language_matches(lang: &str, pref: &str) -> bool {
    let (a, b) = (lang.trim().to_ascii_lowercase(), pref.trim().to_ascii_lowercase());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || language_name(&a).eq_ignore_ascii_case(&language_name(&b))
}

#[derive(Clone, Debug)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub title: String,
}

#[derive(Clone, Debug, Default)]
pub struct MediaInfo {
    pub source_name: String,
    pub format_name: String,
    pub format_long_name: String,
    pub duration: f64,
    pub start_time: f64,
    pub bitrate: i64,
    pub seekable: bool,
    pub is_live: bool,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub metadata: Vec<(String, String)>,
    pub tracks: Vec<Track>,
    pub chapters: Vec<Chapter>,
    pub file_size: Option<u64>,
}

impl MediaInfo {
    pub fn tracks_of(&self, kind: TrackKind) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }
    pub fn has_video(&self) -> bool {
        self.tracks_of(TrackKind::Video).any(|t| !t.is_cover_art)
    }
}

#[derive(Clone, Debug, Default)]
pub struct VideoParams {
    pub width: i32,
    pub height: i32,
    pub display_aspect: f64,
    pub pix_fmt: String,
    pub hwdec: Option<String>,
    pub fps: f64,
    pub decoder: String,
    pub color: String,
    pub hdr: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AudioParams {
    pub decoder: String,
    pub in_rate: i32,
    pub in_layout: String,
    pub in_format: String,
    pub out_rate: u32,
    pub out_channels: u16,
    pub device: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Idle,
    Loading,
    Playing,
    Paused,
    Ended,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct Selected {
    pub video: Option<TrackId>,
    pub audio: Option<TrackId>,
    pub subtitle: Option<TrackId>,
}

#[derive(Clone, Debug)]
pub struct PlaybackState {
    pub status: Status,
    pub source: Option<MediaSource>,
    pub media: Option<Arc<MediaInfo>>,
    pub selected: Selected,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
    pub buffering: bool,
    /// Seconds of media buffered ahead of the play position.
    pub cache_secs: f64,
    pub dropped_frames: u64,
    pub decoded_frames: u64,
    pub seeking: bool,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            status: Status::Idle,
            source: None,
            media: None,
            selected: Selected::default(),
            video: None,
            audio: None,
            buffering: false,
            cache_secs: 0.0,
            dropped_frames: 0,
            decoded_frames: 0,
            seeking: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    /// The file was opened and its tracks are known.
    Loaded,
    /// Playback reached the end of the file.
    EndOfFile,
    /// Could not open or play the file.
    Error(String),
    /// A non-fatal problem worth telling the user about.
    Warning(String),
    /// A track changed (e.g. after a switch).
    TracksChanged,
    ScreenshotSaved(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HwDecode {
    /// Use hardware decoding for content software decoding may struggle with
    /// (above 1080p), falling back to software automatically.
    Auto,
    /// Always try hardware decoding first.
    Always,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Deinterlace {
    /// Deinterlace only frames flagged as interlaced.
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelMode {
    /// Use the media's channel count if the device supports it, otherwise downmix.
    Auto,
    Stereo,
    Mono,
    Surround51,
    Surround71,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScaleQuality {
    /// Nearest neighbour: hard pixel edges, for pixel art and retro games.
    /// Also used when enlarging, so pixels stay crisp at any size.
    Nearest,
    /// Bilinear. Saved settings from before used the name "Fast".
    #[serde(alias = "Fast")]
    Linear,
    /// Bicubic.
    Good,
    /// Lanczos.
    Best,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rotation {
    None,
    R90,
    R180,
    R270,
}

/// Picture adjustments, all applied by libavfilter on the decode thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoAdjust {
    pub brightness: f32, // -1..1
    pub contrast: f32,   // 0..3, 1 = neutral
    pub saturation: f32, // 0..3, 1 = neutral
    pub gamma: f32,      // 0.1..4, 1 = neutral
    pub hue: f32,        // degrees
    pub sharpen: f32,    // 0..2
    pub denoise: bool,
    pub hflip: bool,
    pub vflip: bool,
    pub rotation: Rotation,
    pub grayscale: bool,
    pub negate: bool,
    /// Crop as fractions removed from each edge (left, top, right, bottom).
    pub crop: Option<[f32; 4]>,
}

impl Default for VideoAdjust {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            gamma: 1.0,
            hue: 0.0,
            sharpen: 0.0,
            denoise: false,
            hflip: false,
            vflip: false,
            rotation: Rotation::None,
            grayscale: false,
            negate: false,
            crop: None,
        }
    }
}

pub const EQ_BANDS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioFx {
    pub eq_enabled: bool,
    pub preamp_db: f32,
    pub eq_db: [f32; 10],
    pub normalize: bool,
    pub night_mode: bool,
    pub stereo_widen: bool,
    pub swap_channels: bool,
    pub karaoke: bool,
}

impl Default for AudioFx {
    fn default() -> Self {
        Self {
            eq_enabled: false,
            preamp_db: 0.0,
            eq_db: [0.0; 10],
            normalize: false,
            night_mode: false,
            stereo_widen: false,
            swap_channels: false,
            karaoke: false,
        }
    }
}

/// Engine-level configuration (the user-facing settings that influence decoding).
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub hwdec: HwDecode,
    pub deinterlace: Deinterlace,
    pub channels: ChannelMode,
    pub audio_device: Option<String>,
    pub scale_quality: ScaleQuality,
    pub tone_map_hdr: bool,
    pub audio_langs: Vec<String>,
    pub sub_langs: Vec<String>,
    pub subs_enabled_by_default: bool,
    pub auto_load_subs: bool,
    pub sub_charset: Option<String>,
    pub network_cache_secs: f64,
    pub decoder_threads: u32,
    pub dvd_region: u32,
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            hwdec: HwDecode::Auto,
            deinterlace: Deinterlace::Auto,
            channels: ChannelMode::Auto,
            audio_device: None,
            scale_quality: ScaleQuality::Good,
            tone_map_hdr: true,
            audio_langs: vec![],
            sub_langs: vec![],
            subs_enabled_by_default: false,
            auto_load_subs: true,
            sub_charset: None,
            network_cache_secs: 10.0,
            decoder_threads: 0,
            dvd_region: 0,
            user_agent: format!("MavPlayer/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// A decoded, colour-converted picture ready for the screen. Pixels are BGRA,
/// tightly packed, with square pixels (aspect already applied).
pub struct PresentedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// One displayed subtitle event.
#[derive(Clone, Debug)]
pub struct SubEvent {
    pub start: f64,
    pub end: f64,
    pub content: SubContent,
}

#[derive(Clone, Debug)]
pub enum SubContent {
    Text(String),
    Bitmap(Arc<Vec<SubBitmap>>),
}

#[derive(Debug)]
pub struct SubBitmap {
    pub id: u64,
    /// Placement relative to the subtitle canvas (normally the video frame).
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub canvas_w: u32,
    pub canvas_h: u32,
    /// BGRA pixels, premultiplication not applied.
    pub bgra: Vec<u8>,
}
