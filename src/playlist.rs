//! The play queue: ordering, shuffle/repeat, folder expansion and
//! M3U / PLS import & export.

use crate::engine::MediaSource;
use crate::engine::disc;
use crate::settings::RepeatMode;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

pub const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "webm", "wmv", "flv", "mpg", "mpeg", "m2v", "ts", "m2ts", "mts", "vob", "ogv",
    "3gp", "3g2", "asf", "divx", "f4v", "rm", "rmvb", "mxf", "y4m", "dv", "nut", "ivf", "hevc", "h264", "264", "265",
    "av1", "obu", "evo", "mk3d", "bik", "gif", "wtv", "dvr-ms",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "ape", "wv", "mka", "ac3", "eac3", "dts", "aiff",
    "aif", "alac", "mpc", "tta", "amr", "au", "caf", "spx", "mid", "midi", "mod", "xm", "it", "s3m", "dsf", "dff",
    "w64", "voc", "mp2", "m4b", "thd", "mlp", "tak", "shn",
];
pub const PLAYLIST_EXTS: &[&str] = &["m3u", "m3u8", "pls"];

pub fn is_media_path(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    VIDEO_EXTS.contains(&ext.as_str()) || AUDIO_EXTS.contains(&ext.as_str()) || ext == "iso"
}

#[derive(Clone, Debug)]
pub struct PlaylistItem {
    pub id: u64,
    pub source: MediaSource,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration: Option<f64>,
    pub probed: bool,
    pub failed: bool,
}

impl PlaylistItem {
    pub fn display_title(&self) -> String {
        match (&self.title, &self.artist) {
            (Some(t), Some(a)) => format!("{a} – {t}"),
            (Some(t), None) => t.clone(),
            _ => self.source.display_name(),
        }
    }
}

pub struct Playlist {
    pub items: Vec<PlaylistItem>,
    pub current: Option<usize>,
    pub repeat: RepeatMode,
    shuffle: bool,
    order: Vec<usize>,
    next_id: u64,
    rng: u64,
}

impl Playlist {
    pub fn new(repeat: RepeatMode, shuffle: bool) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(42);
        Self { items: vec![], current: None, repeat, shuffle, order: vec![], next_id: 1, rng: seed | 1 }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.rebuild_order();
    }

    fn rebuild_order(&mut self) {
        let n = self.items.len();
        self.order = (0..n).collect();
        if self.shuffle {
            for i in (1..n).rev() {
                let j = (self.rand() % (i as u64 + 1)) as usize;
                self.order.swap(i, j);
            }
            // Keep the current item first so "next" continues through the rest.
            if let Some(c) = self.current
                && let Some(pos) = self.order.iter().position(|&x| x == c)
            {
                self.order.remove(pos);
                self.order.insert(0, c);
            }
        }
    }

    pub fn add(&mut self, sources: Vec<MediaSource>) -> std::ops::Range<usize> {
        let start = self.items.len();
        for s in sources {
            self.items.push(PlaylistItem {
                id: self.next_id,
                source: s,
                title: None,
                artist: None,
                duration: None,
                probed: false,
                failed: false,
            });
            self.next_id += 1;
        }
        let end = self.items.len();
        if self.shuffle {
            // Insert the new items at random positions after the current one.
            let cur_pos =
                self.current.and_then(|c| self.order.iter().position(|&x| x == c)).map(|p| p + 1).unwrap_or(0);
            for i in start..end {
                let span = (self.order.len() - cur_pos + 1) as u64;
                let at = cur_pos + (self.rand() % span) as usize;
                self.order.insert(at.min(self.order.len()), i);
            }
        } else {
            self.order.extend(start..end);
        }
        start..end
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.order.clear();
        self.current = None;
    }

    pub fn remove(&mut self, idx: usize) {
        if idx >= self.items.len() {
            return;
        }
        self.items.remove(idx);
        self.current = match self.current {
            Some(c) if c == idx => None,
            Some(c) if c > idx => Some(c - 1),
            c => c,
        };
        self.order.retain(|&x| x != idx);
        for x in self.order.iter_mut() {
            if *x > idx {
                *x -= 1;
            }
        }
    }

    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.items.len() || to >= self.items.len() || from == to {
            return;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
        let map = |i: usize| -> usize {
            if i == from {
                to
            } else if from < to && i > from && i <= to {
                i - 1
            } else if from > to && i >= to && i < from {
                i + 1
            } else {
                i
            }
        };
        self.current = self.current.map(map);
        if self.shuffle {
            for x in self.order.iter_mut() {
                *x = map(*x);
            }
        } else {
            self.order = (0..self.items.len()).collect();
        }
    }

    /// Index of the item after the current one. `user` = explicit "next".
    pub fn next_index(&self, user: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One && !user {
            return self.current;
        }
        let pos = self.current.and_then(|c| self.order.iter().position(|&x| x == c));
        match pos {
            None => self.order.first().copied(),
            Some(p) if p + 1 < self.order.len() => Some(self.order[p + 1]),
            Some(_) if self.repeat != RepeatMode::Off => self.order.first().copied(),
            Some(_) => None,
        }
    }

    pub fn prev_index(&self) -> Option<usize> {
        let pos = self.current.and_then(|c| self.order.iter().position(|&x| x == c))?;
        if pos > 0 {
            Some(self.order[pos - 1])
        } else if self.repeat != RepeatMode::Off {
            self.order.last().copied()
        } else {
            Some(self.order[0])
        }
    }

    pub fn save_m3u(&self, path: &Path) -> std::io::Result<()> {
        let mut s = String::from("#EXTM3U\n");
        for it in &self.items {
            let dur = it.duration.map(|d| d.round() as i64).unwrap_or(-1);
            s.push_str(&format!("#EXTINF:{dur},{}\n", it.display_title()));
            match &it.source {
                MediaSource::File(p) => s.push_str(&format!("{}\n", p.display())),
                MediaSource::Url(u) => s.push_str(&format!("{u}\n")),
                MediaSource::Dvd { path, title, .. } => s.push_str(&format!("dvd://{title}/{path}\n")),
            }
        }
        std::fs::write(path, s)
    }
}

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(|c| c.is_ascii_digit()) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(|c| c.is_ascii_digit()) {
                    nb.push(c);
                    bi.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let o = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                let o = x.to_lowercase().cmp(y.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn scan_dir(dir: &Path, out: &mut Vec<MediaSource>, depth: u32) {
    if let Some(dvd) = disc::is_disc_source(dir) {
        out.push(dvd);
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort_by(|a, b| natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    for p in entries {
        if p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false) {
            continue;
        }
        if p.is_dir() {
            if depth < 8 {
                scan_dir(&p, out, depth + 1);
            }
        } else if is_media_path(&p) {
            out.push(source_for_path(&p));
        }
    }
}

pub fn source_for_path(p: &Path) -> MediaSource {
    disc::is_disc_source(p).unwrap_or_else(|| MediaSource::File(p.to_path_buf()))
}

/// Parses a string the user typed or passed on the command line.
pub fn source_for_arg(arg: &str) -> MediaSource {
    let lower = arg.to_ascii_lowercase();
    if lower.starts_with("dvd://") {
        let rest = &arg[6..];
        let (title, path) = match rest.split_once('/') {
            Some((t, p)) if t.chars().all(|c| c.is_ascii_digit()) => (t.parse().unwrap_or(0), p.to_string()),
            _ if rest.chars().all(|c| c.is_ascii_digit()) => (rest.parse().unwrap_or(0), default_dvd_device()),
            _ => (0, rest.to_string()),
        };
        let path = if path.is_empty() {
            default_dvd_device()
        } else if cfg!(unix) && !path.starts_with('/') {
            format!("/{path}")
        } else {
            path
        };
        return MediaSource::Dvd { path, title, angle: 1 };
    }
    if let Some(p) = arg.strip_prefix("file://") {
        return source_for_path(Path::new(&percent_decode(p)));
    }
    if lower.contains("://") {
        return MediaSource::Url(arg.to_string());
    }
    source_for_path(Path::new(arg))
}

pub fn default_dvd_device() -> String {
    disc::optical_drives().first().map(|d| d.path.clone()).unwrap_or_else(|| {
        if cfg!(windows) {
            "D:".into()
        } else if cfg!(target_os = "macos") {
            "/dev/rdisk1".into()
        } else {
            "/dev/sr0".into()
        }
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hls(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.contains("#EXT-X-")).unwrap_or(false)
}

fn parse_playlist_file(path: &Path) -> Vec<MediaSource> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    let base = path.parent().unwrap_or(Path::new("."));
    let resolve = |entry: &str| -> MediaSource {
        let e = entry.trim();
        if e.contains("://") {
            source_for_arg(e)
        } else {
            let p = Path::new(e);
            source_for_path(&if p.is_absolute() { p.to_path_buf() } else { base.join(p) })
        }
    };
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "pls" {
        text.lines()
            .filter_map(|l| l.trim().split_once('='))
            .filter(|(k, _)| k.to_ascii_lowercase().starts_with("file"))
            .map(|(_, v)| resolve(v))
            .collect()
    } else {
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(resolve).collect()
    }
}

/// Expands files, folders and playlist files into playable sources.
pub fn expand_paths(paths: &[PathBuf]) -> Vec<MediaSource> {
    let mut out = Vec::new();
    for p in paths {
        let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if p.is_dir() {
            scan_dir(p, &mut out, 0);
        } else if PLAYLIST_EXTS.contains(&ext.as_str()) && !(ext == "m3u8" && is_hls(p)) {
            out.extend(parse_playlist_file(p));
        } else {
            out.push(source_for_path(p));
        }
    }
    out
}
