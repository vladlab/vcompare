//! Recent analyses list, persisted as `recents.json` in the app data dir.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use vcompare_engine::cache;

const MAX_RECENTS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    pub ref_path: String,
    pub dist_path: String,
    pub cache_path: String,
    pub mode: String,
    pub frame_count: u64,
    pub timestamp: u64,
}

pub struct Recents {
    path: PathBuf,
    entries: Mutex<Vec<RecentEntry>>,
}

impl Recents {
    pub fn new(data_dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(data_dir);
        let path = data_dir.join("recents.json");
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, entries: Mutex::new(entries) }
    }

    pub fn list(&self) -> Vec<RecentEntry> {
        self.entries.lock().unwrap().clone()
    }

    /// Put `entry` first, replacing any entry with the same cache path.
    pub fn add(&self, entry: RecentEntry) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.cache_path != entry.cache_path);
        entries.insert(0, entry);
        entries.truncate(MAX_RECENTS);
        self.save(&entries);
    }

    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        entries.clear();
        self.save(&entries);
    }

    fn save(&self, entries: &[RecentEntry]) {
        if let Ok(json) = serde_json::to_string_pretty(entries) {
            let _ = cache::write_atomic(&self.path.to_string_lossy(), json.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cache: &str) -> RecentEntry {
        RecentEntry {
            ref_path: "a.mov".into(),
            dist_path: String::new(),
            cache_path: cache.into(),
            mode: "levels".into(),
            frame_count: 10,
            timestamp: 1,
        }
    }

    #[test]
    fn add_dedupes_caps_and_persists() {
        let dir = std::env::temp_dir().join(format!("vcompare-recents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = Recents::new(&dir);
        for i in 0..25 {
            r.add(entry(&format!("c{i}")));
        }
        r.add(entry("c20"));
        let l = r.list();
        assert_eq!(l.len(), MAX_RECENTS);
        assert_eq!(l[0].cache_path, "c20");
        assert_eq!(l.iter().filter(|e| e.cache_path == "c20").count(), 1);
        let again = Recents::new(&dir);
        assert_eq!(again.list().len(), MAX_RECENTS);
        again.clear();
        assert!(Recents::new(&dir).list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
