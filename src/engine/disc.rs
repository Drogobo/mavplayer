//! Disc helpers: recognising DVDs (discs, images, folders) and audio CD
//! images, finding optical drives, listing DVD titles and keeping a drive
//! open while its disc is in use. Titles play through FFmpeg's `dvdvideo`
//! demuxer (libdvdnav/libdvdread, plus libdvdcss when present), menus
//! through `nav.rs`.

use super::demux;
use super::types::*;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct DvdTitle {
    pub number: u32,
    pub duration: f64,
    pub chapters: usize,
    pub audio: Vec<String>,
    pub subtitles: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct OpticalDrive {
    pub path: String,
    pub label: String,
}

/// If `p` is a DVD-Video folder (or its VIDEO_TS dir) or a DVD ISO image,
/// returns the path the `dvdvideo` demuxer should open.
pub fn dvd_root(p: &Path) -> Option<String> {
    if p.is_dir() {
        if p.file_name().map(|n| n.eq_ignore_ascii_case("VIDEO_TS")).unwrap_or(false) && p.join("VIDEO_TS.IFO").exists()
        {
            return p.parent().map(|x| x.display().to_string());
        }
        for name in ["VIDEO_TS", "video_ts"] {
            if p.join(name).is_dir() {
                return Some(p.display().to_string());
            }
        }
        return None;
    }
    if p.file_name().map(|n| n.eq_ignore_ascii_case("VIDEO_TS.IFO")).unwrap_or(false) {
        return p.parent().and_then(|d| d.parent()).map(|x| x.display().to_string());
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("iso") | Some("img")) && iso_has_video_ts(p) {
        return Some(p.display().to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if std::fs::metadata(p).map(|m| m.file_type().is_block_device()).unwrap_or(false) {
            return Some(p.display().to_string());
        }
    }
    None
}

/// Cheap ISO-9660 sniff: valid volume descriptor and a VIDEO_TS entry in
/// the first directory sectors.
fn iso_has_video_ts(p: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut vd = [0u8; 2048];
    if f.seek(SeekFrom::Start(16 * 2048)).is_err() || f.read_exact(&mut vd).is_err() {
        return false;
    }
    if &vd[1..6] != b"CD001" {
        // UDF-only images: let the demuxer decide.
        return true;
    }
    let mut buf = vec![0u8; 2048 * 512];
    if f.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    let n = f.read(&mut buf).unwrap_or(0);
    buf[..n].windows(8).any(|w| w.eq_ignore_ascii_case(b"VIDEO_TS"))
}

pub fn optical_drives() -> Vec<OpticalDrive> {
    let mut v = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if let Ok(rd) = std::fs::read_dir("/dev") {
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("sr") && n[2..].chars().all(|c| c.is_ascii_digit()))
                .collect();
            names.sort();
            for n in names {
                let model = std::fs::read_to_string(format!("/sys/block/{n}/device/model")).unwrap_or_default();
                let label =
                    if model.trim().is_empty() { format!("/dev/{n}") } else { format!("{} (/dev/{n})", model.trim()) };
                v.push(OpticalDrive { path: format!("/dev/{n}"), label });
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(rd) = std::fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let p = e.path();
                if p.join("VIDEO_TS").is_dir() {
                    v.push(OpticalDrive {
                        path: p.display().to_string(),
                        label: e.file_name().to_string_lossy().into_owned(),
                    });
                } else if super::cdda::mounted_cd_tracks(&p).is_some() {
                    v.push(OpticalDrive {
                        path: p.display().to_string(),
                        label: format!("Audio CD ({})", e.file_name().to_string_lossy()),
                    });
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        const DRIVE_CDROM: u32 = 5;
        for letter in b'C'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            let optical = unsafe { GetDriveTypeW(wide.as_ptr()) } == DRIVE_CDROM;
            if optical || Path::new(&root).join("VIDEO_TS").is_dir() {
                v.push(OpticalDrive {
                    path: format!("{}:", letter as char),
                    label: format!("Disc drive {}:", letter as char),
                });
            }
        }
    }
    v
}

/// Lists the titles on a disc. Reads the disc, so call it from a
/// background thread.
pub fn probe_titles(path: &str, cfg: &EngineConfig, max: u32) -> Vec<DvdTitle> {
    let _hold = hold(path);
    // From the disc's tables alone (one open, a few small IFO files), when
    // libdvdnav is there; opening each title reads into its video.
    #[cfg(has_dvdnav)]
    if let Some(mut titles) = super::nav::list_titles(path)
        && !titles.is_empty()
    {
        titles.retain(|t| t.number <= max);
        return titles;
    }
    let mut titles = Vec::new();
    for n in 1..=max {
        let src = MediaSource::Dvd { path: path.to_string(), title: n, angle: 1 };
        match demux::open_input(&src, cfg, None) {
            Ok(ic) => {
                let info = demux::build_media_info(&ic, &src);
                let langs = |k: TrackKind| {
                    info.tracks_of(k)
                        .map(|t| t.language.clone().map(|l| language_name(&l)).unwrap_or_else(|| "?".into()))
                        .collect()
                };
                titles.push(DvdTitle {
                    number: n,
                    duration: info.duration,
                    chapters: info.chapters.len(),
                    audio: langs(TrackKind::Audio),
                    subtitles: langs(TrackKind::Subtitle),
                });
            }
            Err(_) => break,
        }
    }
    titles
}

/// The main feature is usually the longest title.
pub fn main_title(titles: &[DvdTitle]) -> Option<u32> {
    titles.iter().max_by(|a, b| a.duration.total_cmp(&b.duration)).map(|t| t.number)
}

/// Keeps an optical drive open while its disc is in use: when the last
/// handle on a block device closes, Linux drops its page cache, and every
/// open after that (each title probed, the menus played after a scan,
/// playing the disc again) read the disc from scratch at drive speed.
/// Dropping the hold leaves the drive open for `KEEP_AFTER` more.
pub struct DiscHold(#[cfg_attr(not(target_os = "linux"), allow(dead_code))] String);

/// How long a drive stays open after its disc was last used.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const KEEP_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

/// Holds `path` open if it is an optical drive (a block device).
pub fn hold(path: &str) -> Option<DiscHold> {
    #[cfg(target_os = "linux")]
    {
        keep::acquire(path).map(DiscHold)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

impl Drop for DiscHold {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        keep::release(&self.0);
    }
}

/// Closes drives unused for `KEEP_AFTER` (called once a second).
pub fn release_idle() {
    #[cfg(target_os = "linux")]
    keep::release_idle(KEEP_AFTER);
}

#[cfg(target_os = "linux")]
mod keep {
    use std::os::fd::OwnedFd;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Held {
        path: String,
        _fd: OwnedFd,
        users: usize,
        idle_since: Instant,
    }

    static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

    /// Returns the key the hold is filed under.
    pub fn acquire(path: &str) -> Option<String> {
        let real = std::fs::canonicalize(path).ok()?;
        let key = real.to_string_lossy().into_owned();
        let mut g = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = g.iter_mut().find(|h| h.path == key) {
            h.users += 1;
            return Some(key);
        }
        if !std::fs::metadata(&real).ok()?.file_type().is_block_device() {
            return None;
        }
        // O_NONBLOCK: an open "for ioctls" that doesn't lock the tray.
        let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&real).ok()?;
        g.push(Held { path: key.clone(), _fd: f.into(), users: 1, idle_since: Instant::now() });
        Some(key)
    }

    pub fn release(key: &str) {
        let mut g = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = g.iter_mut().find(|h| h.path == key) {
            h.users = h.users.saturating_sub(1);
            if h.users == 0 {
                h.idle_since = Instant::now();
            }
        }
    }

    pub fn release_idle(after: Duration) {
        let mut g = HELD.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|h| h.users > 0 || h.idle_since.elapsed() < after);
    }
}

pub fn is_disc_source(p: &Path) -> Option<MediaSource> {
    if super::cdda::is_cue(p) {
        // A cue sheet of an audio CD image (not one for a FLAC or WAV album,
        // whose audio file plays by itself).
        return super::cdda::read_toc(&p.to_string_lossy())
            .ok()
            .map(|_| MediaSource::Cd { device: p.display().to_string(), track: 0 });
    }
    dvd_root(p).map(|path| MediaSource::Dvd { path, title: 0, angle: 1 })
}

/// What is in an optical drive.
#[derive(Clone, Debug)]
pub enum DiscContent {
    Dvd(Vec<DvdTitle>),
    AudioCd(super::cdda::CdToc),
    /// An audio CD the system mounted as files (macOS), in track order.
    MountedCd(Vec<std::path::PathBuf>),
    Unreadable,
}

/// Looks at the disc in `path` (a drive, image or folder). Slow on
/// physical discs (it may spin one up): call it from a background thread.
pub fn read_disc(path: &str, cfg: &EngineConfig) -> DiscContent {
    let _hold = hold(path);
    let p = Path::new(path);
    if let Some(files) = super::cdda::mounted_cd_tracks(p) {
        return DiscContent::MountedCd(files);
    }
    let is_cue = super::cdda::is_cue(p);
    // A drive with an audio CD answers the TOC request quickly; trying the
    // DVD reader first would wait for it to give up on a disc without files.
    if is_cue || !p.is_dir() && !path.to_ascii_lowercase().ends_with(".iso") {
        if let Ok(toc) = super::cdda::read_toc(path) {
            return DiscContent::AudioCd(toc);
        }
        if is_cue {
            return DiscContent::Unreadable;
        }
    }
    let titles = probe_titles(path, cfg, 99);
    if titles.is_empty() { DiscContent::Unreadable } else { DiscContent::Dvd(titles) }
}
