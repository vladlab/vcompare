//! Test-only helpers shared by modules whose tests touch process-wide state
//! (environment variables) or need the fake Resolve / fake grabber.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Serialize tests that set environment variables.
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `crates/vcompare-engine/` (where this crate's Cargo.toml lives).
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The repository root (`scripts/` lives there).
pub fn repo_root() -> PathBuf {
    crate_dir().join("../..")
}

/// Point the Resolve bridge at `testdata/fake_resolve`, serving `timeline`
/// and recording the playhead to `playhead`. Call with the lock held.
pub fn use_fake_resolve(timeline: &Path, playhead: Option<&Path>) {
    let api = crate_dir().join("testdata").join("fake_resolve");
    let module = api.join("Modules").join("DaVinciResolveScript.py");
    std::env::set_var("RESOLVE_SCRIPT_API", &api);
    std::env::set_var("RESOLVE_SCRIPT_LIB", &module);
    std::env::set_var("VCOMPARE_FAKE_TIMELINE", timeline);
    match playhead {
        Some(p) => std::env::set_var("VCOMPARE_FAKE_PLAYHEAD", p),
        None => std::env::remove_var("VCOMPARE_FAKE_PLAYHEAD"),
    }
}
