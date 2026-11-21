//! Audio CDs: the table of contents, CD-Text, and reading a track's raw
//! samples (44.1 kHz, 16-bit, stereo) for FFmpeg's PCM demuxer.
//!
//! Drives are read directly: `CDROMREADAUDIO` on Linux, `IOCTL_CDROM_RAW_READ`
//! on Windows. macOS mounts audio CDs as a folder of AIFF files, which play as
//! ordinary files. `.cue` sheets of audio CD images (`BINARY`/`MOTOROLA` data
//! files) read the same way.

use super::av::{IoSource, ff};
use std::io::{Read, Seek, SeekFrom};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};

/// Bytes in one CD audio sector (1/75 s of 44.1 kHz stereo 16-bit).
pub const SECTOR: usize = 2352;
/// Sectors per second.
pub const SECTORS_PER_SEC: u32 = 75;

#[derive(Clone, Debug, Default)]
pub struct CdTrack {
    pub number: u32,
    /// First sector (drives: logical block address; images: sector in its file).
    pub start: u32,
    pub sectors: u32,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// Images: the data file this track is in, and whether its samples are big-endian.
    file: Option<PathBuf>,
    big_endian: bool,
}

impl CdTrack {
    pub fn duration(&self) -> f64 {
        self.sectors as f64 / SECTORS_PER_SEC as f64
    }
}

#[derive(Clone, Debug, Default)]
pub struct CdToc {
    /// The audio tracks (data tracks of mixed-mode discs are left out).
    pub tracks: Vec<CdTrack>,
    pub album: Option<String>,
    pub artist: Option<String>,
}

impl CdToc {
    pub fn track(&self, n: u32) -> Option<&CdTrack> {
        self.tracks.iter().find(|t| t.number == n)
    }
}

/// Whether `p` is a cue sheet (an audio CD image).
pub fn is_cue(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// Reads the table of contents (and CD-Text, where the disc has it) of a
/// drive or a `.cue` image.
pub fn read_toc(device: &str) -> anyhow::Result<CdToc> {
    let p = Path::new(device);
    if is_cue(p) {
        return parse_cue_file(p);
    }
    let toc = sys::read_toc(device)?;
    if toc.tracks.is_empty() {
        anyhow::bail!("there is no audio CD in {device}");
    }
    Ok(toc)
}

/// Opens track `n` for reading: the byte stream is its raw PCM.
pub fn open_track(device: &str, n: u32) -> anyhow::Result<(Box<dyn IoSource>, CdToc)> {
    let toc = read_toc(device)?;
    let track = toc.track(n).cloned().ok_or_else(|| anyhow::anyhow!("the CD has no audio track {n}"))?;
    let reader: Box<dyn SectorReader> = match &track.file {
        Some(f) => Box::new(ImageReader { file: std::fs::File::open(f)?, big_endian: track.big_endian }),
        None => Box::new(sys::Drive::open(device)?),
    };
    Ok((Box::new(TrackIo::new(reader, track.start, track.sectors)), toc))
}

/// Reads whole sectors.
trait SectorReader: Send {
    /// Reads `buf.len() / SECTOR` sectors starting at `lba`.
    fn read(&mut self, lba: u32, buf: &mut [u8]) -> std::io::Result<()>;
}

struct ImageReader {
    file: std::fs::File,
    big_endian: bool,
}

impl SectorReader for ImageReader {
    fn read(&mut self, lba: u32, buf: &mut [u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(lba as u64 * SECTOR as u64))?;
        // The last track can end with a partial sector: pad with silence.
        let mut n = 0;
        while n < buf.len() {
            match self.file.read(&mut buf[n..])? {
                0 => break,
                r => n += r,
            }
        }
        buf[n..].fill(0);
        if self.big_endian {
            for s in buf.as_chunks_mut::<2>().0 {
                s.swap(0, 1);
            }
        }
        Ok(())
    }
}

/// A track as a seekable byte stream, read a chunk of sectors at a time.
struct TrackIo {
    reader: Box<dyn SectorReader>,
    start: u32,
    sectors: u32,
    pos: u64,
    /// Sectors read last: the first one's index in the track, and the bytes.
    cache_first: u32,
    cache: Vec<u8>,
    warned: bool,
}

/// Sectors per drive read (about 0.3 s; well under every drive's transfer limit).
const CHUNK: u32 = 24;

impl TrackIo {
    fn new(reader: Box<dyn SectorReader>, start: u32, sectors: u32) -> Self {
        TrackIo { reader, start, sectors, pos: 0, cache_first: 0, cache: Vec::new(), warned: false }
    }

    fn len(&self) -> u64 {
        self.sectors as u64 * SECTOR as u64
    }

    /// Makes sure sector `s` (track-relative) is cached.
    fn fill(&mut self, s: u32) -> std::io::Result<()> {
        let cached = (self.cache.len() / SECTOR) as u32;
        if s >= self.cache_first && s < self.cache_first + cached {
            return Ok(());
        }
        let n = CHUNK.min(self.sectors - s);
        self.cache.resize(n as usize * SECTOR, 0);
        self.cache_first = s;
        if self.reader.read(self.start + s, &mut self.cache).is_ok() {
            return Ok(());
        }
        // A scratch: retry sector by sector, and play silence for the ones
        // that still can't be read rather than stopping.
        let mut failed = 0;
        let mut last_err = None;
        for i in 0..n {
            let b = &mut self.cache[i as usize * SECTOR..(i as usize + 1) * SECTOR];
            if let Err(e) = self.reader.read(self.start + s + i, b) {
                b.fill(0);
                failed += 1;
                last_err = Some(e);
            }
        }
        if failed == n {
            // Nothing readable at all: the disc is gone, or the drive failed.
            self.cache.clear();
            return Err(last_err.unwrap_or_else(|| std::io::Error::other("read failed")));
        }
        if failed > 0 && !self.warned {
            self.warned = true;
            log::warn!("audio CD: {failed} unreadable sector(s) near sector {}, played as silence", self.start + s);
        }
        Ok(())
    }
}

impl IoSource for TrackIo {
    fn read(&mut self, buf: &mut [u8]) -> c_int {
        if self.pos >= self.len() {
            return 0;
        }
        let s = (self.pos / SECTOR as u64) as u32;
        if let Err(e) = self.fill(s) {
            log::warn!("audio CD read failed: {e}");
            return ff::AVERROR(5); // EIO
        }
        let off = (self.pos - self.cache_first as u64 * SECTOR as u64) as usize;
        let n = buf.len().min(self.cache.len() - off).min((self.len() - self.pos) as usize);
        buf[..n].copy_from_slice(&self.cache[off..off + n]);
        self.pos += n as u64;
        n as c_int
    }

    fn seek(&mut self, offset: i64, whence: c_int) -> i64 {
        let len = self.len() as i64;
        let target = match whence & !(ff::AVSEEK_FORCE as c_int) {
            w if w == ff::AVSEEK_SIZE as c_int => return len,
            0 => offset,                   // SEEK_SET
            1 => self.pos as i64 + offset, // SEEK_CUR
            2 => len + offset,             // SEEK_END
            _ => return -1,
        };
        if target < 0 {
            return -1;
        }
        self.pos = (target as u64).min(len as u64);
        self.pos as i64
    }

    fn seekable(&self) -> bool {
        true
    }
}

// ── Cue sheets ──────────────────────────────────────────────────────────────

fn parse_cue_file(p: &Path) -> anyhow::Result<CdToc> {
    let bytes = std::fs::read(p)?;
    // Cue sheets are often Latin-1 (older rippers) rather than UTF-8.
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
    };
    let dir = p.parent().unwrap_or(Path::new("."));
    let toc = parse_cue(&text, dir, |f| std::fs::metadata(f).ok().map(|m| m.len()))?;
    if toc.tracks.is_empty() {
        anyhow::bail!("{} has no audio tracks in raw (BINARY) data files", p.display());
    }
    Ok(toc)
}

/// Splits a cue line into words, keeping "quoted strings" whole.
fn cue_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.trim().chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// "mm:ss:ff" (frames are 1/75 s) → sectors.
fn cue_time(s: &str) -> Option<u32> {
    let mut it = s.split(':').map(|x| x.trim().parse::<u32>().ok());
    let (m, sec, f) = (it.next()??, it.next()??, it.next()??);
    Some((m * 60 + sec) * SECTORS_PER_SEC + f)
}

/// Parses a cue sheet. `file_len` gives a data file's size (to know where
/// its last track ends).
fn parse_cue(text: &str, dir: &Path, file_len: impl Fn(&Path) -> Option<u64>) -> anyhow::Result<CdToc> {
    struct Pending {
        number: u32,
        audio: bool,
        title: Option<String>,
        artist: Option<String>,
        start: Option<u32>,
        file: usize,
    }
    let mut toc = CdToc::default();
    // Data files: (path, big-endian, usable).
    let mut files: Vec<(PathBuf, bool, bool)> = Vec::new();
    let mut tracks: Vec<Pending> = Vec::new();
    for line in text.lines() {
        let w = cue_words(line);
        let Some(cmd) = w.first().map(|c| c.to_ascii_uppercase()) else { continue };
        match cmd.as_str() {
            "FILE" if w.len() >= 2 => {
                let kind = w.get(2).map(|k| k.to_ascii_uppercase()).unwrap_or_default();
                let path = dir.join(&w[1]);
                files.push((path, kind == "MOTOROLA", kind == "BINARY" || kind == "MOTOROLA"));
            }
            "TRACK" if w.len() >= 3 && !files.is_empty() => tracks.push(Pending {
                number: w[1].parse().unwrap_or(tracks.len() as u32 + 1),
                audio: w[2].eq_ignore_ascii_case("AUDIO"),
                title: None,
                artist: None,
                start: None,
                file: files.len() - 1,
            }),
            "INDEX" if w.len() >= 3 && w[1] == "01" => {
                if let Some(t) = tracks.last_mut() {
                    t.start = cue_time(&w[2]);
                }
            }
            "TITLE" | "PERFORMER" if w.len() >= 2 => {
                let v = Some(w[1..].join(" ")).filter(|s| !s.trim().is_empty());
                let slot = match (tracks.last_mut(), cmd.as_str()) {
                    (Some(t), "TITLE") => &mut t.title,
                    (Some(t), _) => &mut t.artist,
                    (None, "TITLE") => &mut toc.album,
                    (None, _) => &mut toc.artist,
                };
                *slot = v;
            }
            _ => {}
        }
    }
    for (i, t) in tracks.iter().enumerate() {
        let (path, big_endian, usable) = &files[t.file];
        let Some(start) = t.start else { continue };
        if !t.audio || !usable {
            continue;
        }
        // Ends where the next track in the same file starts, or at the file's end.
        let end = match tracks.get(i + 1).filter(|n| n.file == t.file).and_then(|n| n.start) {
            Some(e) => e,
            None => match file_len(path) {
                Some(len) => len.div_ceil(SECTOR as u64) as u32,
                None => continue,
            },
        };
        if end <= start {
            continue;
        }
        toc.tracks.push(CdTrack {
            number: t.number,
            start,
            sectors: end - start,
            title: t.title.clone(),
            artist: t.artist.clone().or_else(|| toc.artist.clone()),
            file: Some(path.clone()),
            big_endian: *big_endian,
        });
    }
    Ok(toc)
}

// ── CD-Text ─────────────────────────────────────────────────────────────────

/// Applies CD-Text packs (the payload of READ TOC format 5: 18-byte packs)
/// to `toc`: titles and performers of the album and its tracks.
fn apply_cd_text(toc: &mut CdToc, packs: &[u8]) {
    // Pack type → concatenated text of block 0, and the track it starts at.
    let mut texts: [(Vec<u8>, Option<u8>); 2] = [(Vec::new(), None), (Vec::new(), None)];
    for p in packs.as_chunks::<18>().0 {
        let kind = match p[0] {
            0x80 => 0,
            0x81 => 1,
            _ => continue,
        };
        let block = (p[3] >> 4) & 0x07;
        let double_byte = p[3] & 0x80 != 0;
        if block != 0 || double_byte {
            continue;
        }
        let t = &mut texts[kind];
        if t.1.is_none() {
            t.1 = Some(p[1] & 0x7f);
        }
        t.0.extend_from_slice(&p[4..16]);
    }
    for (kind, (bytes, first)) in texts.iter().enumerate() {
        let Some(first) = *first else { continue };
        let mut prev: Option<String> = None;
        let mut parts: Vec<&[u8]> = bytes.split(|&b| b == 0).collect();
        // The text ends with a terminator: drop the empty remainder.
        while parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        for (i, raw) in parts.into_iter().enumerate() {
            // A tab means "same as the previous track".
            let s = if raw == b"\t" {
                prev.clone()
            } else {
                Some(raw.iter().map(|&b| b as char).collect::<String>().trim().to_string()).filter(|s| !s.is_empty())
            };
            prev = s.clone();
            let n = first as u32 + i as u32;
            let slot = if n == 0 {
                if kind == 0 { &mut toc.album } else { &mut toc.artist }
            } else if let Some(t) = toc.tracks.iter_mut().find(|t| t.number == n) {
                if kind == 0 { &mut t.title } else { &mut t.artist }
            } else {
                continue;
            };
            if slot.is_none() {
                *slot = s;
            }
        }
    }
    let album_artist = toc.artist.clone();
    for t in &mut toc.tracks {
        if t.artist.is_none() {
            t.artist = album_artist.clone();
        }
    }
}

/// Track entries of a disc (number, start sector, data track?) plus the
/// lead-out → the audio tracks with their lengths.
fn tracks_from_toc(entries: &[(u32, u32, bool)], leadout: u32) -> Vec<CdTrack> {
    let mut v = Vec::new();
    for (i, &(number, start, data)) in entries.iter().enumerate() {
        let mut end = entries.get(i + 1).map(|e| e.1).unwrap_or(leadout);
        // On a CD-Extra disc the data session follows the audio after a
        // gap of 11 400 sectors that isn't part of the last audio track.
        if entries.get(i + 1).is_some_and(|e| e.2) && end > start + 11_400 {
            end -= 11_400;
        }
        if data || end <= start {
            continue;
        }
        v.push(CdTrack { number, start, sectors: end - start, ..Default::default() });
    }
    v
}

#[cfg(target_os = "linux")]
mod sys {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::{c_int, c_uint, c_ulong, c_void};

    const CDROMREADTOCHDR: c_ulong = 0x5305;
    const CDROMREADTOCENTRY: c_ulong = 0x5306;
    const CDROMREADAUDIO: c_ulong = 0x530e;
    const CDROM_SEND_PACKET: c_ulong = 0x5393;
    const CDROM_LBA: u8 = 0x01;
    const CDROM_LEADOUT: u8 = 0xAA;
    const CDROM_DATA_TRACK: u8 = 0x04;
    const CGC_DATA_READ: u8 = 2;

    #[repr(C)]
    #[derive(Default)]
    struct TocHeader {
        first: u8,
        last: u8,
    }

    #[repr(C)]
    #[derive(Default)]
    struct TocEntry {
        track: u8,
        /// `adr:4, ctrl:4` (adr in the low nibble).
        adr_ctrl: u8,
        format: u8,
        lba: c_int,
        datamode: u8,
    }

    #[repr(C)]
    struct ReadAudio {
        lba: c_int,
        addr_format: u8,
        nframes: c_int,
        buf: *mut u8,
    }

    #[repr(C)]
    struct GenericCommand {
        cmd: [u8; 12],
        buffer: *mut u8,
        buflen: c_uint,
        stat: c_int,
        sense: *mut c_void,
        data_direction: u8,
        quiet: c_int,
        timeout: c_int,
        reserved: *mut c_void,
    }

    pub struct Drive {
        fd: c_int,
    }

    impl Drop for Drive {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }

    impl Drive {
        pub fn open(device: &str) -> anyhow::Result<Drive> {
            let c = CString::new(device)?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
            if fd < 0 {
                let e = std::io::Error::last_os_error();
                anyhow::bail!("could not open {device}: {e}");
            }
            Ok(Drive { fd })
        }

        fn entry(&self, track: u8) -> std::io::Result<TocEntry> {
            let mut e = TocEntry { track, format: CDROM_LBA, ..Default::default() };
            if unsafe { libc::ioctl(self.fd, CDROMREADTOCENTRY as _, &mut e) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(e)
        }

        /// CD-Text packs (READ TOC, format 5), if the disc has any.
        fn cd_text(&self) -> Option<Vec<u8>> {
            let mut buf = vec![0u8; 8192];
            let mut cmd = [0u8; 12];
            cmd[0] = 0x43; // READ TOC/PMA/ATIP
            cmd[2] = 0x05; // CD-Text
            cmd[7] = (buf.len() >> 8) as u8;
            cmd[8] = buf.len() as u8;
            let mut cgc = GenericCommand {
                cmd,
                buffer: buf.as_mut_ptr(),
                buflen: buf.len() as c_uint,
                stat: 0,
                sense: std::ptr::null_mut(),
                data_direction: CGC_DATA_READ,
                quiet: 1,
                timeout: 5000,
                reserved: std::ptr::null_mut(),
            };
            if unsafe { libc::ioctl(self.fd, CDROM_SEND_PACKET as _, &mut cgc) } < 0 {
                return None;
            }
            let len = ((buf[0] as usize) << 8 | buf[1] as usize).saturating_sub(2).min(buf.len() - 4);
            Some(buf[4..4 + len].to_vec())
        }
    }

    impl SectorReader for Drive {
        fn read(&mut self, lba: u32, buf: &mut [u8]) -> std::io::Result<()> {
            let mut ra = ReadAudio {
                lba: lba as c_int,
                addr_format: CDROM_LBA,
                nframes: (buf.len() / SECTOR) as c_int,
                buf: buf.as_mut_ptr(),
            };
            if unsafe { libc::ioctl(self.fd, CDROMREADAUDIO as _, &mut ra) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }

    pub fn read_toc(device: &str) -> anyhow::Result<CdToc> {
        let d = Drive::open(device)?;
        let mut h = TocHeader::default();
        if unsafe { libc::ioctl(d.fd, CDROMREADTOCHDR as _, &mut h) } < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("no readable CD in {device} ({e})");
        }
        let mut entries = Vec::new();
        for t in h.first..=h.last {
            let e = d.entry(t)?;
            entries.push((t as u32, e.lba.max(0) as u32, (e.adr_ctrl >> 4) & CDROM_DATA_TRACK != 0));
        }
        let leadout = d.entry(CDROM_LEADOUT)?.lba.max(0) as u32;
        let mut toc = CdToc { tracks: tracks_from_toc(&entries, leadout), ..Default::default() };
        if let Some(packs) = d.cd_text() {
            apply_cd_text(&mut toc, &packs);
        }
        Ok(toc)
    }
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::os::raw::c_void;
    use std::os::windows::ffi::OsStrExt;

    type Handle = *mut c_void;
    const INVALID_HANDLE: Handle = -1isize as Handle;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;
    const IOCTL_CDROM_READ_TOC: u32 = 0x0002_4000;
    const IOCTL_CDROM_RAW_READ: u32 = 0x0002_403E;
    const IOCTL_CDROM_READ_TOC_EX: u32 = 0x0002_4054;
    const CDDA: u32 = 2;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            dev: Handle,
            code: u32,
            inbuf: *mut c_void,
            inlen: u32,
            outbuf: *mut c_void,
            outlen: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TrackData {
        reserved: u8,
        /// `Control:4, Adr:4` (control in the low nibble).
        control_adr: u8,
        number: u8,
        reserved1: u8,
        address: [u8; 4],
    }

    #[repr(C)]
    struct Toc {
        length: [u8; 2],
        first: u8,
        last: u8,
        tracks: [TrackData; 100],
    }

    #[repr(C)]
    struct RawReadInfo {
        disk_offset: i64,
        sector_count: u32,
        track_mode: u32,
    }

    pub struct Drive {
        h: Handle,
    }

    unsafe impl Send for Drive {}

    impl Drop for Drive {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.h) };
        }
    }

    impl Drive {
        pub fn open(device: &str) -> anyhow::Result<Drive> {
            // "D:", "D:\" or "D" → \\.\D:
            let letter = device.trim_end_matches(['\\', '/']).trim_end_matches(':');
            let path: Vec<u16> =
                std::ffi::OsStr::new(&format!(r"\\.\{letter}:")).encode_wide().chain(std::iter::once(0)).collect();
            let h = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE || h.is_null() {
                anyhow::bail!("could not open drive {device}: {}", std::io::Error::last_os_error());
            }
            Ok(Drive { h })
        }

        fn ioctl(&self, code: u32, input: &mut [u8], output: &mut [u8]) -> std::io::Result<usize> {
            let mut n = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.h,
                    code,
                    if input.is_empty() { std::ptr::null_mut() } else { input.as_mut_ptr() as *mut c_void },
                    input.len() as u32,
                    output.as_mut_ptr() as *mut c_void,
                    output.len() as u32,
                    &mut n,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(n as usize) }
        }
    }

    impl SectorReader for Drive {
        fn read(&mut self, lba: u32, buf: &mut [u8]) -> std::io::Result<()> {
            let mut info = RawReadInfo {
                disk_offset: lba as i64 * 2048,
                sector_count: (buf.len() / SECTOR) as u32,
                track_mode: CDDA,
            };
            let inp = unsafe {
                std::slice::from_raw_parts_mut(
                    &mut info as *mut RawReadInfo as *mut u8,
                    std::mem::size_of::<RawReadInfo>(),
                )
            };
            self.ioctl(IOCTL_CDROM_RAW_READ, inp, buf).map(|_| ())
        }
    }

    fn msf_lba(a: &[u8; 4]) -> u32 {
        ((a[1] as u32 * 60 + a[2] as u32) * SECTORS_PER_SEC + a[3] as u32).saturating_sub(150)
    }

    pub fn read_toc(device: &str) -> anyhow::Result<CdToc> {
        let d = Drive::open(device)?;
        let mut toc: Toc = unsafe { std::mem::zeroed() };
        let out =
            unsafe { std::slice::from_raw_parts_mut(&mut toc as *mut Toc as *mut u8, std::mem::size_of::<Toc>()) };
        d.ioctl(IOCTL_CDROM_READ_TOC, &mut [], out).map_err(|e| anyhow::anyhow!("no readable CD in {device} ({e})"))?;
        let count = (toc.last.saturating_sub(toc.first) as usize + 1).min(99);
        let mut entries = Vec::new();
        for t in &toc.tracks[..count] {
            entries.push((t.number as u32, msf_lba(&t.address), t.control_adr & 0x04 != 0));
        }
        let leadout = msf_lba(&toc.tracks[count].address);
        let mut out = CdToc { tracks: tracks_from_toc(&entries, leadout), ..Default::default() };
        // CD-Text: READ_TOC_EX format 5.
        let mut req = [0x05u8, 0, 0, 0];
        let mut buf = vec![0u8; 8192];
        if let Ok(n) = d.ioctl(IOCTL_CDROM_READ_TOC_EX, &mut req, &mut buf)
            && n > 4
        {
            apply_cd_text(&mut out, &buf[4..n]);
        }
        Ok(out)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod sys {
    use super::*;

    pub struct Drive;

    impl Drive {
        pub fn open(device: &str) -> anyhow::Result<Drive> {
            anyhow::bail!("reading audio CDs from {device} directly isn't supported here")
        }
    }

    impl SectorReader for Drive {
        fn read(&mut self, _lba: u32, _buf: &mut [u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("unsupported"))
        }
    }

    pub fn read_toc(device: &str) -> anyhow::Result<CdToc> {
        anyhow::bail!("reading audio CDs from {device} directly isn't supported here")
    }
}

/// macOS mounts an audio CD as a folder of AIFF files (`cddafs`). Returns
/// them in track order if `dir` is such a folder.
pub fn mounted_cd_tracks(dir: &Path) -> Option<Vec<PathBuf>> {
    if !dir.join(".TOC.plist").exists() {
        return None;
    }
    let mut v: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("aiff")))
        .filter_map(|p| {
            let n = p.file_name()?.to_string_lossy().split(' ').next()?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    v.sort();
    (!v.is_empty()).then(|| v.into_iter().map(|(_, p)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cue_sheet_tracks() {
        let cue = r#"
PERFORMER "Some Band"
TITLE "An Album"
FILE "disc.bin" BINARY
  TRACK 01 AUDIO
    TITLE "First"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Second"
    PERFORMER "Guest"
    INDEX 00 00:30:00
    INDEX 01 00:32:00
  TRACK 03 MODE1/2352
    INDEX 01 01:00:00
"#;
        let toc = parse_cue(cue, Path::new("/x"), |_| Some(SECTOR as u64 * 75 * 120)).unwrap();
        assert_eq!(toc.album.as_deref(), Some("An Album"));
        assert_eq!(toc.tracks.len(), 2, "the data track is left out");
        let (a, b) = (&toc.tracks[0], &toc.tracks[1]);
        assert_eq!((a.number, a.start, a.sectors), (1, 0, 32 * 75));
        assert_eq!(a.artist.as_deref(), Some("Some Band"));
        assert_eq!((b.number, b.start, b.sectors), (2, 32 * 75, 28 * 75));
        assert_eq!(b.title.as_deref(), Some("Second"));
        assert_eq!(b.artist.as_deref(), Some("Guest"));
        assert_eq!(a.file.as_deref(), Some(Path::new("/x/disc.bin")));
    }

    #[test]
    fn last_track_runs_to_the_end_of_its_file() {
        let cue = "FILE \"a.bin\" BINARY\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 00:02:00\n";
        let toc = parse_cue(cue, Path::new("."), |_| Some(SECTOR as u64 * 1000 + 100)).unwrap();
        assert_eq!(toc.tracks[1].sectors, 1001 - 150);
        // FLAC/WAVE cue sheets describe files FFmpeg plays directly; not CD images.
        let toc = parse_cue("FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n", Path::new("."), |_| Some(1))
            .unwrap();
        assert!(toc.tracks.is_empty());
    }

    #[test]
    fn toc_skips_data_tracks_and_the_cd_extra_gap() {
        let t = tracks_from_toc(&[(1, 0, false), (2, 20_000, false), (3, 50_000, true)], 60_000);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].sectors, 20_000);
        assert_eq!(t[1].sectors, 30_000 - 11_400);
    }

    #[test]
    fn cd_text_titles() {
        // Two title packs: album "AB", track 1 "Cd", track 2 "E", then a
        // performer pack for the album.
        fn pack(kind: u8, track: u8, text: &[u8]) -> Vec<u8> {
            let mut p = vec![kind, track, 0, 0];
            let mut t = text.to_vec();
            t.resize(12, 0);
            p.extend(t);
            p.extend([0, 0]);
            p
        }
        let mut packs = pack(0x80, 0, b"AB\0Cd\0E\0");
        packs.extend(pack(0x81, 0, b"Band\0\t\0"));
        let mut toc = CdToc {
            tracks: vec![CdTrack { number: 1, ..Default::default() }, CdTrack { number: 2, ..Default::default() }],
            ..Default::default()
        };
        apply_cd_text(&mut toc, &packs);
        assert_eq!(toc.album.as_deref(), Some("AB"));
        assert_eq!(toc.tracks[0].title.as_deref(), Some("Cd"));
        assert_eq!(toc.tracks[1].title.as_deref(), Some("E"));
        assert_eq!(toc.artist.as_deref(), Some("Band"));
        assert_eq!(toc.tracks[0].artist.as_deref(), Some("Band"), "tab: same as before");
        assert_eq!(toc.tracks[1].artist.as_deref(), Some("Band"), "falls back to the album artist");
    }

    #[test]
    fn track_io_reads_and_seeks() {
        struct Fake;
        impl SectorReader for Fake {
            fn read(&mut self, lba: u32, buf: &mut [u8]) -> std::io::Result<()> {
                for (i, s) in buf.chunks_exact_mut(SECTOR).enumerate() {
                    s.fill((lba as usize + i) as u8);
                }
                Ok(())
            }
        }
        let mut io = TrackIo::new(Box::new(Fake), 10, 100);
        assert_eq!(io.seek(0, ff::AVSEEK_SIZE as c_int), 100 * SECTOR as i64);
        let mut b = vec![0u8; 3000];
        assert_eq!(io.read(&mut b), 3000);
        assert_eq!((b[0], b[SECTOR]), (10, 11));
        io.seek(99 * SECTOR as i64 + 10, 0);
        assert_eq!(io.read(&mut b), (SECTOR - 10) as c_int);
        assert_eq!(b[0], 109);
        assert_eq!(io.read(&mut b), 0);
    }
}
