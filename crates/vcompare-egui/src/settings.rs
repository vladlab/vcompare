//! Small things remembered between runs (the web UI kept them in
//! localStorage): the last mode, the last folder, "keep frame signatures",
//! the ticked image-match references.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: String,
    pub last_dir: Option<PathBuf>,
    pub keep_signatures: bool,
    /// Image-match references ticked (ids), across analyses and restarts.
    pub match_references: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: "diff".into(), last_dir: None, keep_signatures: true, match_references: vec![] }
    }
}

fn file(data_dir: &Path) -> PathBuf {
    data_dir.join("egui-settings.json")
}

impl Settings {
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read_to_string(file(data_dir))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) {
        let _ = std::fs::create_dir_all(data_dir);
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(file(data_dir), s);
        }
    }
}
