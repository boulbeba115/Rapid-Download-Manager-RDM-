//! The `.rdmstate` resume file and crash-safe JSON persistence.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::segment::SegRecord;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeState {
    pub url: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<SegRecord>,
}

impl ResumeState {
    pub fn validator(&self) -> Option<String> {
        match &self.etag {
            Some(tag) if !tag.starts_with("W/") => Some(tag.clone()),
            _ => self.last_modified.clone(),
        }
    }

    /// Segments must tile `[0, size)` exactly; anything else is a damaged file.
    pub fn is_consistent(&self) -> bool {
        let mut at = 0;
        for s in &self.segments {
            if s.start != at || s.end < s.start || s.written < s.start || s.written > s.end {
                return false;
            }
            at = s.end;
        }
        at == self.size && !self.segments.is_empty()
    }

    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.written - s.start).sum()
    }

    pub fn load(path: &Path) -> Option<Self> {
        load_json::<Self>(path).filter(Self::is_consistent)
    }
}

/// The folder next to the executable that a portable copy keeps its settings
/// in: used when a `portable.txt` marker (or an existing `data` folder) is there.
pub fn portable_data_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    (dir.join("portable.txt").exists() || dir.join("data").join("config.json").exists()).then(|| dir.join("data"))
}

/// Where settings and the download list live: `RDM_DATA_DIR` if set (automated
/// tests that must not touch the real profile), the portable folder, otherwise
/// `%APPDATA%\RDM`. Every RDM program resolves it the same way, which is how
/// `rdm-host` finds the running application's port and secret.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RDM_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    portable_data_dir()
        .unwrap_or_else(|| std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("RDM"))
}

pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Write to a temp file, flush it, then rename over the target, so a crash at
/// any instant leaves either the old file or the new one — never half of each.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let data = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(segments: Vec<SegRecord>) -> ResumeState {
        ResumeState { url: "http://x/y".into(), size: 100, etag: None, last_modified: None, segments }
    }

    #[test]
    fn consistency() {
        let ok = state(vec![
            SegRecord { start: 0, written: 10, end: 50 },
            SegRecord { start: 50, written: 50, end: 100 },
        ]);
        assert!(ok.is_consistent());
        assert_eq!(ok.downloaded(), 10);
        let gap = state(vec![SegRecord { start: 0, written: 0, end: 40 }, SegRecord { start: 50, written: 50, end: 100 }]);
        assert!(!gap.is_consistent());
        let short = state(vec![SegRecord { start: 0, written: 0, end: 90 }]);
        assert!(!short.is_consistent());
        let over = state(vec![SegRecord { start: 0, written: 101, end: 100 }]);
        assert!(!over.is_consistent());
        assert!(!state(vec![]).is_consistent());
    }
}
