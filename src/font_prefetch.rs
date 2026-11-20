//! Startup font prefetch (Linux).
//!
//! GPUI's Linux text system (cosmic-text) scans every installed font
//! (`fontdb::Database::load_system_fonts`) on the main thread inside
//! `Application::new()`. It does this one file at a time, and it memory-maps
//! each file with default read-ahead, so the kernel reads most of every file
//! just to parse its headers. When the fonts have dropped out of the page
//! cache (the player hasn't run for a while), that can mean reading hundreds
//! of MB and take seconds. With about 1 GB of fonts on a SATA SSD it was about
//! 1.7 s, against about 15 ms with a warm cache.
//!
//! This module runs the same parse first, on several threads. Each mapping
//! is set to `MADV_RANDOM`, so only the pages the parser touches are read
//! (about a tenth as much data). GPUI's scan then finds all of them cached.
//! On the same machine the cold prefetch took about 150 ms. With a warm
//! cache it costs a few ms.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

/// Starts prefetching in the background. Join the handle before `Application::new()`.
pub fn start() -> Option<JoinHandle<()>> {
    std::thread::Builder::new().name("font-prefetch".into()).spawn(run).ok()
}

fn run() {
    let t = std::time::Instant::now();
    let files = font_files();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 16);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                // A throwaway database: parsing is what pulls in the right pages.
                let mut db = fontdb::Database::new();
                while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let Ok(file) = std::fs::File::open(path) else { continue };
                    // SAFETY: read-only mapping. A font file truncated while being
                    // parsed could fault, and GPUI's own scan has the same exposure.
                    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };
                    let _ = map.advise(memmap2::Advice::Random);
                    db.load_font_source(fontdb::Source::Binary(Arc::new(map)));
                }
            });
        }
    });
    log::debug!("startup: prefetched {} font files in {:.0} ms", files.len(), t.elapsed().as_secs_f64() * 1000.0);
}

/// The font files fontdb's `load_system_fonts` visits (its fontconfig path):
/// the `<dir>`s from the same config files, walked the same way.
fn font_files() -> Vec<PathBuf> {
    let mut fc = fontconfig_parser::FontConfig::default();
    let home = std::env::var("HOME").ok();
    if let Ok(file) = std::env::var("FONTCONFIG_FILE") {
        let _ = fc.merge_config(Path::new(&file));
    } else {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| Path::new(h).join(".config")));
        let read_global = config_home.is_none_or(|p| fc.merge_config(&p.join("fontconfig/fonts.conf")).is_err());
        if read_global {
            let _ = fc.merge_config(Path::new("/etc/fonts/local.conf"));
        }
        let _ = fc.merge_config(Path::new("/etc/fonts/fonts.conf"));
    }
    let mut seen = HashSet::new();
    let mut files = vec![];
    for dir in fc.dirs {
        let path = match (dir.path.strip_prefix("~"), &home) {
            (Ok(rest), Some(h)) => Path::new(h).join(rest),
            (Ok(_), None) => continue,
            (Err(_), _) => dir.path,
        };
        walk(&path, &mut seen, &mut files);
    }
    files
}

fn walk(dir: &Path, seen: &mut HashSet<PathBuf>, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.is_file() {
            let font = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "ttc" | "otf" | "otc"));
            if font && seen.insert(path.clone()) {
                files.push(path);
            }
        } else if path.is_dir() && seen.insert(path.clone()) {
            walk(&path, seen, files);
        }
    }
}
