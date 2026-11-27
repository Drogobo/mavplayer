//! The playlist library: named playlists, each an ordinary M3U file. MavPlayer's
//! own live in `CONFIG/playlists/<id>.m3u`; linked ones are `.m3u` files
//! anywhere else, read and written where they are. `playlists.json` holds what
//! M3U can't: names, order, autosave, and the current item and position.
//!
//! Several windows (and processes) share the library. Every save locks
//! `playlists.json.lock`, re-reads the index and writes only the playlists this
//! window changed, so a save never brings back another window's old copies.
//! `refresh` (on window focus) picks up what other windows saved.

use crate::engine::MediaSource;
use crate::playlist::{M3uEntry, PLAYLIST_EXTS, is_hls, m3u_text, read_playlist_file};
use crate::settings::{config_dir, write_atomic, write_json};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

const VERSION: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub sources: Vec<MediaSource>,
    pub current: Option<usize>,
    pub position: Option<f64>,
}

impl Snapshot {
    fn normalize(&mut self) {
        self.current = self.current.filter(|&i| i < self.sources.len());
        self.position = self.position.filter(|p| self.current.is_some() && p.is_finite() && *p >= 0.0);
    }

    fn needs_autosave(&self, saved: &Self) -> bool {
        self.sources != saved.sources
            || self.current != saved.current
            || match (self.position, saved.position) {
                (Some(a), Some(b)) => (a - b).abs() >= 5.0,
                (a, b) => a != b,
            }
    }
}

/// A playlist's line in `playlists.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Entry {
    name: String,
    /// A linked playlist's file; MavPlayer's own are `playlists/<id>.m3u`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(default)]
    autosave: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    position: Option<f64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u32,
    /// IDs only go up, so a deleted playlist's number never names another.
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    order: Vec<u64>,
    #[serde(default)]
    playlists: BTreeMap<u64, Entry>,
    /// The first window's selected playlist and open playlists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<u64>,
    #[serde(default)]
    open: Vec<u64>,
}

impl Index {
    fn empty() -> Self {
        Self { version: VERSION, next_id: 1, ..Default::default() }
    }

    /// Every playlist, in order.
    fn ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.order.iter().copied().filter(|id| self.playlists.contains_key(id)).collect();
        ids.extend(self.playlists.keys().filter(|id| !self.order.contains(id)));
        ids
    }
}

/// `playlists.json` as MavPlayer 0.4 wrote it: everything in one file.
#[derive(Deserialize)]
struct V1Library {
    #[serde(default)]
    active: usize,
    slots: Vec<V1Slot>,
}

#[derive(Deserialize)]
struct V1Slot {
    name: String,
    #[serde(default)]
    autosave: bool,
    #[serde(default = "yes")]
    open: bool,
    #[serde(default)]
    saved: Snapshot,
}

fn yes() -> bool {
    true
}

enum OnDisk {
    Missing,
    V1(V1Library),
    Current(Index),
    /// From a newer MavPlayer, or damaged: never written over.
    Foreign,
}

/// A file's modification time and size, to notice when it changed.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path).ok().and_then(|m| Some((m.modified().ok()?, m.len())))
}

/// Where the library lives; tests use a scratch folder.
struct Store {
    dir: PathBuf,
}

impl Store {
    fn index(&self) -> PathBuf {
        self.dir.join("playlists.json")
    }

    fn numbered(&self, id: u64) -> PathBuf {
        self.dir.join("playlists").join(format!("{id}.m3u"))
    }

    /// Held while reading or writing the library; released when dropped.
    fn lock(&self) -> io::Result<std::fs::File> {
        std::fs::create_dir_all(&self.dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("playlists.json.lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn read(&self) -> OnDisk {
        let data = match std::fs::read(self.index()) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return OnDisk::Missing,
            Err(e) => {
                log::warn!("could not read {}: {e}", self.index().display());
                return OnDisk::Foreign;
            }
        };
        #[derive(Deserialize)]
        struct Probe {
            version: u32,
        }
        let parsed = match serde_json::from_slice::<Probe>(&data) {
            Ok(Probe { version: 1 }) => serde_json::from_slice(&data).map(OnDisk::V1),
            Ok(Probe { version: VERSION }) => serde_json::from_slice(&data).map(OnDisk::Current),
            Ok(Probe { version }) => {
                log::warn!("playlists.json is version {version}, from a newer MavPlayer: leaving it alone");
                return OnDisk::Foreign;
            }
            Err(e) => Err(e),
        };
        parsed.unwrap_or_else(|e| {
            log::warn!("ignoring unreadable playlists.json: {e}");
            OnDisk::Foreign
        })
    }

    /// Writes each playlist of a version-1 library to its own file. The old
    /// file is kept as `playlists.json.v1.bak`.
    fn migrate(&self, v1: V1Library) -> io::Result<Index> {
        let mut index = Index::empty();
        for (i, slot) in v1.slots.into_iter().enumerate() {
            let id = i as u64 + 1;
            let entries: Vec<M3uEntry> =
                slot.saved.sources.iter().map(|s| M3uEntry { source: s.clone(), extinf: None }).collect();
            write_atomic(&self.numbered(id), m3u_text(&entries, None).as_bytes(), true)?;
            index.playlists.insert(
                id,
                Entry {
                    name: slot.name,
                    path: None,
                    autosave: slot.autosave,
                    current: slot.saved.current,
                    position: slot.saved.position,
                },
            );
            index.order.push(id);
            if slot.open {
                index.open.push(id);
            }
            if i == v1.active {
                index.active = Some(id);
            }
            index.next_id = id + 1;
        }
        let _ = std::fs::copy(self.index(), self.dir.join("playlists.json.v1.bak"));
        write_json(&self.index(), &index)?;
        Ok(index)
    }

    fn slot(&self, id: u64, entry: &Entry) -> Slot {
        let mut slot = Slot::new(entry.name.clone(), entry.autosave);
        slot.id = Some(id);
        slot.link = entry.path.clone();
        slot.open = false;
        if let Some(path) = slot.file(self) {
            let _ = slot.read_items(&path);
        }
        slot.saved.current = entry.current;
        slot.saved.position = entry.position;
        slot.saved.normalize();
        slot
    }
}

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Slot {
    pub name: String,
    pub autosave: bool,
    /// Listed under "Switch playlist" in this window.
    pub open: bool,
    saved: Snapshot,
    /// What this window shows, when it differs from what was saved.
    draft: Option<Snapshot>,
    /// Tells slots apart in this window while IDs come and go.
    uid: u64,
    /// The playlist's number in `playlists.json`; none until first saved.
    id: Option<u64>,
    /// A linked playlist's file.
    pub link: Option<PathBuf>,
    /// `#EXTINF` lines read from the file, written back with their entries.
    extinf: HashMap<MediaSource, String>,
    /// Unreadable, missing, or not UTF-8: never written.
    pub read_only: bool,
    /// A linked file that isn't there (an unplugged drive, a moved file).
    pub missing: bool,
    items_dirty: bool,
    meta_dirty: bool,
    /// The file as last read or written here.
    stamp: Stamp,
}

impl Slot {
    fn new(name: String, autosave: bool) -> Self {
        Self {
            name,
            autosave,
            open: true,
            saved: Snapshot::default(),
            draft: None,
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            id: None,
            link: None,
            extinf: HashMap::new(),
            read_only: false,
            missing: false,
            items_dirty: false,
            meta_dirty: false,
            stamp: None,
        }
    }

    fn snapshot(&self) -> &Snapshot {
        self.draft.as_ref().unwrap_or(&self.saved)
    }

    /// The name as menus show it.
    pub fn label(&self) -> String {
        if self.missing { format!("{} (missing)", self.name) } else { self.name.clone() }
    }

    /// Edits not saved yet (autosave off).
    fn unsaved(&self) -> bool {
        self.draft.as_ref().is_some_and(|d| d.sources != self.saved.sources)
    }

    fn file(&self, store: &Store) -> Option<PathBuf> {
        self.link.clone().or_else(|| self.id.map(|id| store.numbered(id)))
    }

    fn entry(&self) -> Entry {
        Entry {
            name: self.name.clone(),
            path: self.link.clone(),
            autosave: self.autosave,
            current: self.saved.current,
            position: self.saved.position,
        }
    }

    fn read_items(&mut self, path: &Path) -> io::Result<()> {
        self.stamp = stamp(path);
        self.extinf.clear();
        self.saved.sources.clear();
        let result = read_playlist_file(path);
        match &result {
            Ok(file) => {
                self.read_only = file.lossy;
                self.missing = false;
                for e in &file.entries {
                    if let Some(extinf) = &e.extinf {
                        self.extinf.insert(e.source.clone(), extinf.clone());
                    }
                    self.saved.sources.push(e.source.clone());
                }
            }
            Err(e) => {
                let gone = e.kind() == io::ErrorKind::NotFound;
                self.missing = gone && self.link.is_some();
                // A numbered file that's gone is just an empty playlist.
                self.read_only = self.link.is_some() || !gone;
                if !gone {
                    log::warn!("could not read playlist {}: {e}", path.display());
                }
            }
        }
        self.saved.normalize();
        result.map(|_| ())
    }

    fn write_items(&mut self, store: &Store) -> io::Result<()> {
        let path = self.file(store).expect("a saved playlist has a file");
        let entries: Vec<M3uEntry> = self
            .saved
            .sources
            .iter()
            .map(|s| M3uEntry { source: s.clone(), extinf: self.extinf.get(s).cloned() })
            .collect();
        let base = self.link.as_deref().and_then(Path::parent);
        write_atomic(&path, m3u_text(&entries, base).as_bytes(), self.link.is_none())?;
        self.stamp = stamp(&path);
        Ok(())
    }

    /// Takes in what another window saved. True when the playlist's items
    /// changed and this window should show them again.
    fn update_from(&mut self, store: &Store, entry: &Entry) -> bool {
        let before = self.saved.clone();
        if !self.meta_dirty {
            self.name = entry.name.clone();
            self.autosave = entry.autosave;
        }
        if !self.items_dirty
            && let Some(path) = self.file(store)
            && stamp(&path) != self.stamp
        {
            let _ = self.read_items(&path);
        }
        if !self.meta_dirty {
            self.saved.current = entry.current;
            self.saved.position = entry.position;
        }
        self.saved.normalize();
        if self.saved == before {
            return false;
        }
        // Unsaved edits stay on top of the new saved version.
        if self.draft.as_ref().is_some_and(|d| d.sources != before.sources) {
            return false;
        }
        self.draft = None;
        self.saved.sources != before.sources
    }
}

pub(crate) struct Library {
    store: Store,
    pub active: usize,
    pub slots: Vec<Slot>,
    /// False when `playlists.json` is from a newer MavPlayer or damaged.
    pub writable: bool,
    /// The first window remembers which playlists are open and selected;
    /// other windows leave that alone.
    owns_layout: bool,
    layout_dirty: bool,
    /// Deleted here, not yet removed from disk: (id, has a numbered file).
    deleted: Vec<(u64, bool)>,
    index_stamp: Stamp,
}

impl Library {
    pub fn load(primary: bool) -> Self {
        Self::load_in(config_dir(), primary)
    }

    fn load_in(dir: PathBuf, primary: bool) -> Self {
        let mut lib = Self {
            store: Store { dir },
            active: 0,
            slots: Vec::new(),
            writable: true,
            owns_layout: primary,
            layout_dirty: false,
            deleted: Vec::new(),
            index_stamp: None,
        };
        let _lock = lib.store.lock().map_err(|e| log::warn!("could not lock the playlist library: {e}")).ok();
        let index = match lib.store.read() {
            OnDisk::Missing => None,
            OnDisk::Current(index) => Some(index),
            OnDisk::V1(v1) => match lib.store.migrate(v1) {
                Ok(index) => Some(index),
                Err(e) => {
                    log::warn!("could not convert playlists.json to playlist files: {e}");
                    lib.writable = false;
                    None
                }
            },
            OnDisk::Foreign => {
                lib.writable = false;
                None
            }
        };
        lib.index_stamp = stamp(&lib.store.index());
        if let Some(index) = &index {
            for id in index.ids() {
                let slot = lib.store.slot(id, &index.playlists[&id]);
                lib.slots.push(slot);
            }
            if primary {
                for slot in &mut lib.slots {
                    slot.open = slot.id.is_some_and(|id| index.open.contains(&id));
                }
                lib.active =
                    index.active.and_then(|a| lib.slots.iter().position(|s| s.id == Some(a))).unwrap_or_default();
            }
        }
        if lib.writable {
            lib.adopt_orphans();
        }
        if lib.slots.is_empty() {
            let mut slot = Slot::new("Default playlist".into(), true);
            // Registered with the first save, so it's there next time even
            // if it stays empty.
            slot.meta_dirty = primary;
            lib.slots.push(slot);
        }
        if primary {
            lib.slots[lib.active].open = true;
        }
        lib
    }

    /// Numbered files missing from the index (a crash between writing a
    /// playlist and the index) come back as recovered playlists.
    fn adopt_orphans(&mut self) {
        let Ok(dir) = std::fs::read_dir(self.store.dir.join("playlists")) else { return };
        let mut orphans: Vec<u64> = dir
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".m3u")?.parse().ok())
            .filter(|id| !self.slots.iter().any(|s| s.id == Some(*id)))
            .collect();
        orphans.sort_unstable();
        for id in orphans {
            let name = self.suggested_name("Recovered playlist");
            let mut slot = self.store.slot(id, &Entry { name, autosave: true, ..Default::default() });
            slot.meta_dirty = true;
            self.slots.push(slot);
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.slots[self.active].snapshot()
    }
    pub fn name(&self) -> &str {
        &self.slots[self.active].name
    }
    pub fn autosave(&self) -> bool {
        self.slots[self.active].autosave
    }
    pub fn active_slot(&self) -> &Slot {
        &self.slots[self.active]
    }

    pub fn capture(&mut self, mut snapshot: Snapshot, exact: bool, manual: bool) {
        snapshot.normalize();
        let slot = &mut self.slots[self.active];
        if (manual || (slot.autosave && (exact || snapshot.needs_autosave(&slot.saved)))) && slot.saved != snapshot {
            if slot.saved.sources != snapshot.sources && !slot.read_only {
                slot.items_dirty = true;
            }
            if (slot.saved.current, slot.saved.position) != (snapshot.current, snapshot.position) {
                slot.meta_dirty = true;
            }
            slot.saved = snapshot.clone();
        }
        slot.draft = Some(snapshot);
    }

    pub fn set_autosave(&mut self, enabled: bool) {
        let slot = &mut self.slots[self.active];
        if slot.autosave != enabled {
            slot.autosave = enabled;
            slot.meta_dirty = true;
        }
    }

    pub fn switch(&mut self, index: usize) -> bool {
        if index == self.active || index >= self.slots.len() {
            return false;
        }
        self.active = index;
        self.slots[index].open = true;
        self.layout_dirty = true;
        true
    }

    pub fn can_close(&self) -> bool {
        self.active != 0 || self.slots.iter().enumerate().any(|(i, s)| i != self.active && s.open)
    }

    pub fn close_active(&mut self) -> bool {
        if !self.can_close() {
            return false;
        }
        self.slots[self.active].open = false;
        self.active = self.slots.iter().position(|s| s.open).unwrap_or(0);
        self.slots[self.active].open = true;
        self.layout_dirty = true;
        true
    }

    pub fn can_delete(&self) -> bool {
        self.writable && self.slots.len() > 1
    }

    /// Deletes a playlist of MavPlayer's own, or takes a linked one off the
    /// list (its file stays where it is).
    pub fn delete_active(&mut self) -> bool {
        if !self.can_delete() {
            return false;
        }
        let slot = self.slots.remove(self.active);
        if let Some(id) = slot.id {
            self.deleted.push((id, slot.link.is_none()));
        }
        let next = self.active.min(self.slots.len() - 1);
        self.active = if self.slots[next].open { next } else { self.slots.iter().position(|s| s.open).unwrap_or(next) };
        self.slots[self.active].open = true;
        self.layout_dirty = true;
        true
    }

    pub fn valid_name(&self, name: &str, rename: bool) -> bool {
        !name.trim().is_empty()
            && !self
                .slots
                .iter()
                .enumerate()
                .any(|(i, s)| (!rename || i != self.active) && s.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn suggested_name(&self, base: &str) -> String {
        if self.valid_name(base, false) {
            return base.into();
        }
        (2..).map(|i| format!("{base} {i}")).find(|name| self.valid_name(name, false)).unwrap()
    }

    pub fn create(&mut self, name: String) {
        let mut slot = Slot::new(name, true);
        slot.meta_dirty = true;
        self.slots.push(slot);
        self.active = self.slots.len() - 1;
        self.layout_dirty = true;
    }

    pub fn duplicate(&mut self, name: String, mut snapshot: Snapshot) -> bool {
        if !self.writable || !self.valid_name(&name, false) {
            return false;
        }
        snapshot.normalize();
        let mut slot = Slot::new(name, true);
        slot.extinf = self.slots[self.active].extinf.clone();
        slot.saved = snapshot;
        slot.items_dirty = true;
        slot.meta_dirty = true;
        self.slots.push(slot);
        self.active = self.slots.len() - 1;
        self.layout_dirty = true;
        true
    }

    /// Renames the playlist in MavPlayer; a linked file keeps its name.
    pub fn rename(&mut self, name: String) {
        let slot = &mut self.slots[self.active];
        slot.name = name;
        slot.meta_dirty = true;
    }

    /// Explicitly launched media must not replace an already saved playlist.
    /// This temporary playlist is not written unless the user saves, renames
    /// it or turns on autosave. The first window reuses an empty playlist of
    /// its own instead.
    pub fn start_launch_playlist(&mut self) {
        let active = &self.slots[self.active];
        if self.owns_layout && active.snapshot().sources.is_empty() && active.link.is_none() {
            return;
        }
        self.push_temporary("Launch playlist");
    }

    /// A later window starts on an empty playlist of its own, with the rest
    /// of the library a menu away.
    pub fn start_blank(&mut self) {
        self.push_temporary("New playlist");
    }

    fn push_temporary(&mut self, base: &str) {
        let name = self.suggested_name(base);
        self.slots.push(Slot::new(name, false));
        self.active = self.slots.len() - 1;
    }

    /// Adds a playlist file from anywhere to the library and returns its index.
    /// M3U files are linked: edits are saved back to them. MavPlayer can't
    /// write PLS, so those become a copy of MavPlayer's own.
    pub fn link(&mut self, path: &Path) -> Result<usize, String> {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if !PLAYLIST_EXTS.contains(&ext.as_str()) {
            return Err("not a playlist file (.m3u, .m3u8 or .pls)".into());
        }
        let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        if ext == "m3u8" && is_hls(&path) {
            return Err("it is a stream, not a playlist".into());
        }
        if let Some(i) = self.slots.iter().position(|s| s.link.as_deref() == Some(&path)) {
            return Ok(i);
        }
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut slot = Slot::new(self.suggested_name(stem.trim()), true);
        if ext == "pls" {
            slot.read_items(&path).map_err(|e| e.to_string())?;
            slot.read_only = false;
            slot.stamp = None;
            slot.items_dirty = true;
        } else {
            slot.link = Some(path.clone());
            slot.read_items(&path).map_err(|e| e.to_string())?;
        }
        slot.meta_dirty = true;
        self.slots.push(slot);
        Ok(self.slots.len() - 1)
    }

    /// Saves what this window changed: only those playlists' files and
    /// entries, merged into the index as it is on disk now.
    pub fn flush(&mut self) -> io::Result<()> {
        let layout = self.owns_layout && self.layout_dirty;
        let pending = layout || !self.deleted.is_empty() || self.slots.iter().any(|s| s.items_dirty || s.meta_dirty);
        if !self.writable || !pending {
            return Ok(());
        }
        let _lock = self.store.lock()?;
        let mut index = match self.store.read() {
            OnDisk::Current(index) => index,
            OnDisk::Missing => Index::empty(),
            OnDisk::V1(_) | OnDisk::Foreign => {
                self.writable = false;
                return Err(io::Error::other("playlists.json was replaced by another version of MavPlayer"));
            }
        };
        let mut next_id = [
            index.next_id,
            index.playlists.keys().max().map_or(1, |m| m + 1),
            self.slots.iter().filter_map(|s| s.id).max().map_or(1, |m| m + 1),
        ]
        .into_iter()
        .max()
        .unwrap_or(1);
        for (id, numbered) in self.deleted.drain(..) {
            index.playlists.remove(&id);
            if numbered {
                let _ = std::fs::remove_file(self.store.numbered(id));
            }
        }
        let mut failed = None;
        for slot in &mut self.slots {
            if !slot.items_dirty && !slot.meta_dirty {
                continue;
            }
            let id = *slot.id.get_or_insert_with(|| {
                next_id += 1;
                next_id - 1
            });
            // New, or deleted in another window since: write its file too.
            if slot.link.is_none() && !index.playlists.contains_key(&id) {
                slot.items_dirty = true;
            }
            if slot.items_dirty
                && !slot.read_only
                && let Err(e) = slot.write_items(&self.store)
            {
                // Don't try again every second.
                slot.read_only = true;
                failed = Some(io::Error::new(e.kind(), format!("{}: {e}", slot.name)));
            }
            slot.items_dirty = false;
            slot.meta_dirty = false;
            index.playlists.insert(id, slot.entry());
        }
        index.order.retain(|id| index.playlists.contains_key(id));
        for id in self.slots.iter().filter_map(|s| s.id) {
            if index.playlists.contains_key(&id) && !index.order.contains(&id) {
                index.order.push(id);
            }
        }
        if self.owns_layout {
            if let Some(id) = self.slots[self.active].id {
                index.active = Some(id);
            }
            index.open = self.slots.iter().filter(|s| s.open).filter_map(|s| s.id).collect();
            self.layout_dirty = false;
        }
        index.open.retain(|id| index.playlists.contains_key(id));
        index.active = index.active.filter(|id| index.playlists.contains_key(id));
        index.version = VERSION;
        index.next_id = next_id;
        write_json(&self.store.index(), &index)?;
        self.index_stamp = stamp(&self.store.index());
        failed.map_or(Ok(()), Err)
    }

    /// Takes in what other windows saved since this one last looked. The
    /// active playlist is left alone while `active_busy` (it's playing).
    /// Returns true when the active playlist's items changed and the window
    /// should show them again.
    pub fn refresh(&mut self, active_busy: bool) -> bool {
        if !self.writable {
            return false;
        }
        let changed = stamp(&self.store.index()) != self.index_stamp
            || self.slots.iter().any(|s| s.id.is_some() && s.file(&self.store).is_some_and(|p| stamp(&p) != s.stamp));
        if !changed {
            return false;
        }
        let Ok(_lock) = self.store.lock() else { return false };
        let OnDisk::Current(index) = self.store.read() else { return false };
        self.index_stamp = stamp(&self.store.index());
        let active_uid = self.slots[self.active].uid;
        let mut old = std::mem::take(&mut self.slots);
        let mut reload = false;
        for id in index.ids() {
            let entry = &index.playlists[&id];
            let Some(pos) = old.iter().position(|s| s.id == Some(id)) else {
                let slot = self.store.slot(id, entry);
                self.slots.push(slot);
                continue;
            };
            let mut slot = old.remove(pos);
            let is_active = slot.uid == active_uid;
            if !(is_active && active_busy) && slot.update_from(&self.store, entry) && is_active {
                reload = true;
            }
            self.slots.push(slot);
        }
        // Temporary playlists, and ones another window deleted.
        for mut slot in old {
            if slot.id.is_some() && !slot.items_dirty && !slot.meta_dirty {
                if slot.uid != active_uid && !slot.unsaved() {
                    continue;
                }
                // Shown or edited here: kept, as a playlist not saved yet.
                slot.id = None;
            }
            self.slots.push(slot);
        }
        self.active = self.slots.iter().position(|s| s.uid == active_uid).unwrap_or(0);
        self.slots[self.active].open = true;
        reload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(name: &str, position: f64) -> Snapshot {
        // The window always saves absolute paths.
        let path = Path::new("/m").join(name);
        Snapshot { sources: vec![MediaSource::File(path)], current: Some(0), position: Some(position) }
    }

    fn files(names: &[&str]) -> Snapshot {
        Snapshot { sources: names.iter().map(|n| MediaSource::File(n.into())).collect(), current: None, position: None }
    }

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mavplayer-lib-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn slot<'a>(lib: &'a Library, name: &str) -> &'a Slot {
        lib.slots.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no playlist {name}"))
    }

    fn select(lib: &mut Library, name: &str) {
        let i = lib.slots.iter().position(|s| s.name == name).unwrap();
        lib.switch(i);
    }

    #[test]
    fn playlist_workspaces_preserve_independent_drafts_and_saved_positions() {
        let dir = scratch("drafts");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.capture(snapshot("first.mp4", 12.5), true, false);
        lib.create("Second".into());
        lib.capture(snapshot("first.mp4", 70.0), true, false);
        lib.set_autosave(false);
        lib.capture(snapshot("unsaved.mp4", 23.0), true, false);
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &snapshot("first.mp4", 12.5));
        assert!(lib.switch(1));
        assert_eq!(lib.snapshot(), &snapshot("unsaved.mp4", 23.0));
        lib.flush().unwrap();
        let mut restored = Library::load_in(dir.clone(), true);
        assert_eq!(restored.active, 1);
        assert_eq!(restored.snapshot(), &snapshot("first.mp4", 70.0));
        assert!(!restored.autosave());
        assert!(restored.switch(0));
        assert_eq!(restored.snapshot(), &snapshot("first.mp4", 12.5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlists_are_m3u_files_named_by_number() {
        let dir = scratch("files");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.capture(files(&["/m/a.mkv", "/m/b.mkv"]), true, false);
        lib.flush().unwrap();
        let text = std::fs::read_to_string(dir.join("playlists/1.m3u")).unwrap();
        assert_eq!(text, "#EXTM3U\n/m/a.mkv\n/m/b.mkv\n");
        lib.rename("Renamed".into());
        lib.flush().unwrap();
        assert!(dir.join("playlists/1.m3u").exists());
        assert_eq!(Library::load_in(dir.clone(), true).name(), "Renamed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_playlist_preserves_other_saved_slots() {
        let dir = scratch("delete");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.capture(snapshot("first.mp4", 12.5), true, true);
        lib.create("Second".into());
        lib.capture(snapshot("second.mp4", 42.), true, false);
        lib.create("Third".into());
        lib.capture(snapshot("third.mp4", 70.), true, false);
        lib.flush().unwrap();
        assert!(lib.switch(1));
        assert!(lib.delete_active());
        lib.flush().unwrap();
        assert!(!dir.join("playlists/2.m3u").exists());
        let mut lib = Library::load_in(dir.clone(), true);
        assert_eq!(lib.slots.len(), 2);
        assert_eq!(lib.name(), "Third");
        assert_eq!(lib.snapshot(), &snapshot("third.mp4", 70.));
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &snapshot("first.mp4", 12.5));
        assert!(lib.delete_active());
        assert!(!lib.delete_active());
        assert_eq!(lib.name(), "Third");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_playlist_preserves_saved_contents() {
        let dir = scratch("close");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.create("Second".into());
        lib.capture(snapshot("movie.mp4", 42.), true, false);
        assert!(lib.close_active());
        assert_eq!(lib.active, 0);
        assert!(!lib.slots[1].open);
        lib.flush().unwrap();
        let mut restored = Library::load_in(dir.clone(), true);
        assert!(!restored.slots[1].open);
        assert!(restored.switch(1));
        assert_eq!(restored.snapshot(), &snapshot("movie.mp4", 42.));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_preserves_live_contents_without_overwriting_original() {
        let dir = scratch("duplicate");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.capture(snapshot("saved.mp4", 10.0), true, true);
        lib.start_launch_playlist();
        let live = snapshot("launch.mp4", 42.0);
        lib.capture(live.clone(), true, false);
        assert!(!lib.duplicate("Launch playlist".into(), live.clone()));
        assert!(lib.duplicate("Movies".into(), live.clone()));
        assert_eq!(lib.name(), "Movies");
        assert_eq!(lib.snapshot(), &live);
        lib.flush().unwrap();
        // The launch playlist itself was never saved.
        let mut restored = Library::load_in(dir.clone(), true);
        assert_eq!(restored.slots.len(), 2);
        assert_eq!(restored.snapshot(), &live);
        assert!(restored.switch(0));
        assert_eq!(restored.snapshot(), &snapshot("saved.mp4", 10.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launching_media_does_not_overwrite_saved_playlist() {
        let dir = scratch("launch");
        let mut lib = Library::load_in(dir.clone(), true);
        lib.capture(snapshot("saved.mp4", 10.0), true, true);
        lib.flush().unwrap();
        lib.start_launch_playlist();
        assert!(!lib.autosave());
        lib.capture(snapshot("launch.mp4", 0.0), true, false);
        assert!(!lib.slots.iter().any(|s| s.items_dirty || s.meta_dirty));
        assert!(lib.switch(0));
        assert_eq!(lib.snapshot(), &snapshot("saved.mp4", 10.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_windows_keep_each_others_changes() {
        let dir = scratch("two-windows");
        let mut first = Library::load_in(dir.clone(), true);
        first.capture(files(&["/m/a.mkv"]), true, false);
        first.create("Album".into());
        first.capture(files(&["/m/song1.flac"]), true, false);
        first.flush().unwrap();

        let mut second = Library::load_in(dir.clone(), false);
        second.start_blank();
        select(&mut second, "Default playlist");
        // The second window edits one playlist while the first keeps
        // saving its playback position in another.
        second.capture(files(&["/m/a.mkv", "/m/b.mkv"]), true, false);
        second.flush().unwrap();
        first.capture(Snapshot { current: Some(0), position: Some(30.0), ..files(&["/m/song1.flac"]) }, true, false);
        first.flush().unwrap();

        let fresh = Library::load_in(dir.clone(), true);
        assert_eq!(slot(&fresh, "Default playlist").saved.sources.len(), 2);
        assert_eq!(slot(&fresh, "Album").saved.position, Some(30.0));
        // The second window's blank playlist was never saved, and it
        // didn't change which playlist the first window has selected.
        assert_eq!(fresh.slots.len(), 2);
        assert_eq!(fresh.name(), "Album");

        // Switching to the first window shows the second's edit.
        select(&mut first, "Default playlist");
        assert!(first.refresh(false));
        assert_eq!(first.snapshot().sources.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_leaves_the_playing_playlist_alone() {
        let dir = scratch("busy");
        let mut first = Library::load_in(dir.clone(), true);
        first.capture(files(&["/m/a.mkv"]), true, false);
        first.flush().unwrap();
        let mut second = Library::load_in(dir.clone(), false);
        select(&mut second, "Default playlist");
        second.capture(files(&["/m/a.mkv", "/m/b.mkv"]), true, false);
        second.flush().unwrap();
        assert!(!first.refresh(true));
        assert_eq!(first.snapshot().sources.len(), 1);
        // Once it stops, the change comes through.
        assert!(first.refresh(false));
        assert_eq!(first.snapshot().sources.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_bring_back_a_playlist_deleted_elsewhere() {
        let dir = scratch("resurrect");
        let mut first = Library::load_in(dir.clone(), true);
        first.create("Keep".into());
        first.flush().unwrap();
        let mut second = Library::load_in(dir.clone(), false);
        select(&mut second, "Keep");
        assert!(second.delete_active());
        second.flush().unwrap();
        first.capture(files(&["/m/x.mkv"]), true, false);
        first.flush().unwrap();
        let fresh = Library::load_in(dir.clone(), true);
        assert_eq!(slot(&fresh, "Keep").saved.sources, files(&["/m/x.mkv"]).sources);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_1_libraries_become_playlist_files() {
        let dir = scratch("migrate");
        std::fs::create_dir_all(&dir).unwrap();
        let v1 = r#"{"version":1,"active":1,"slots":[
            {"name":"Default playlist","autosave":false,"open":true,"saved":{"sources":[{"File":"/m/a.mkv"}],"current":null,"position":null}},
            {"name":"Shows","autosave":true,"open":true,"saved":{"sources":[{"File":"/m/s1.mkv"},{"File":"/m/s2.mkv"}],"current":1,"position":95.0}}
        ]}"#;
        std::fs::write(dir.join("playlists.json"), v1).unwrap();
        let lib = Library::load_in(dir.clone(), true);
        assert_eq!(lib.name(), "Shows");
        assert_eq!(
            lib.snapshot(),
            &Snapshot { current: Some(1), position: Some(95.0), ..files(&["/m/s1.mkv", "/m/s2.mkv"]) }
        );
        assert!(dir.join("playlists/1.m3u").exists());
        assert!(dir.join("playlists.json.v1.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn linked_playlists_are_saved_where_they_are() {
        let dir = scratch("linked");
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        let file = music.join("party.m3u");
        std::fs::write(&file, "#EXTM3U\n#EXTINF:200,Band – Hit\na.mp3\n/elsewhere/b.mp3\n").unwrap();
        let mut lib = Library::load_in(dir.join("config"), true);
        let i = lib.link(&file).unwrap();
        assert_eq!(lib.link(&file).unwrap(), i, "the same file is linked once");
        lib.switch(i);
        assert_eq!(lib.name(), "party");
        let mut snap = lib.snapshot().clone();
        // Playing doesn't touch the file.
        snap.current = Some(0);
        snap.position = Some(12.0);
        lib.capture(snap.clone(), true, false);
        lib.flush().unwrap();
        let untouched = "#EXTM3U\n#EXTINF:200,Band – Hit\na.mp3\n/elsewhere/b.mp3\n";
        assert_eq!(std::fs::read_to_string(&file).unwrap(), untouched);
        // Editing does, keeping titles and relative paths.
        snap.sources.push(MediaSource::File(music.join("sub/c.mp3")));
        lib.capture(snap, true, false);
        lib.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{untouched}sub/c.mp3\n"));
        // Renaming only renames it in MavPlayer.
        lib.rename("Party mix".into());
        lib.flush().unwrap();
        assert!(file.exists());
        let lib = Library::load_in(dir.join("config"), true);
        assert_eq!(slot(&lib, "Party mix").saved.position, Some(12.0));
        // A linked file that's gone is listed as missing, and never recreated.
        std::fs::remove_file(&file).unwrap();
        let lib = Library::load_in(dir.join("config"), true);
        assert!(slot(&lib, "Party mix").missing);
        assert!(slot(&lib, "Party mix").read_only);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
