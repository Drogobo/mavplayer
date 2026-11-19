//! Lightweight metadata probing for playlist entries.

use super::demux;
use super::types::*;

#[derive(Clone, Debug, Default)]
pub struct ProbeResult {
    pub duration: f64,
    pub title: Option<String>,
    pub artist: Option<String>,
}

pub fn probe(source: &MediaSource, cfg: &EngineConfig) -> Option<ProbeResult> {
    if !matches!(source, MediaSource::File(_)) {
        return None;
    }
    let ic = demux::open_input(source, cfg, None).ok()?;
    let info = demux::build_media_info(&ic, source);
    Some(ProbeResult { duration: info.duration, title: info.title.clone(), artist: info.artist.clone() })
}
