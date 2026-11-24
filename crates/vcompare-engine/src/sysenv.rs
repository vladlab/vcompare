//! Child processes get the user's environment, not the AppImage's.
//!
//! Tauri's AppImage runtime and its linuxdeploy hooks rewrite the app's own
//! environment so the bundled WebKit / GTK can find their libraries: `PATH`
//! and `LD_LIBRARY_PATH` get the AppImage's directories in front,
//! `PYTHONHOME` / `PYTHONPATH` point inside it, `GTK_*` / `GDK_*` / `GST_*`
//! are forced. Everything vcompare starts inherits that: the system
//! `python3` then can't import its own standard library ("Fatal Python
//! error: Failed to import encodings", reported as "Python 3 not found"),
//! and ffmpeg, mpv and scopetap_grab run against the bundled libraries.
//!
//! The app itself must keep that environment (WebKit's helper processes need
//! it), so instead every child is built with [`command`], which undoes it:
//! entries under `$APPDIR` are dropped from path lists, variables whose value
//! points into the AppImage are removed, and the few the hooks force to
//! plain values are unset. Outside an AppImage it is `Command::new`.

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Variables the AppImage hooks set to values that don't mention `$APPDIR`.
const FORCED: &[&str] = &[
    "GDK_BACKEND",
    "GTK_THEME",
    "PYTHONDONTWRITEBYTECODE",
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "OWD",
];

/// `$APPDIR` when running from an AppImage.
fn appdir() -> Option<String> {
    let d = std::env::var("APPDIR").ok()?;
    let d = d.trim_end_matches('/').to_string();
    (!d.is_empty() && d != "/").then_some(d)
}

/// What to do with one variable of the app's environment, given `$APPDIR`.
/// `None` = leave it alone, `Some(None)` = remove it, `Some(Some(v))` = set v.
pub(crate) fn fix_var(appdir: &str, key: &str, value: &str) -> Option<Option<String>> {
    if FORCED.contains(&key) {
        return Some(None);
    }
    if !value.contains(appdir) {
        return None;
    }
    let kept: Vec<&str> = value
        .split(':')
        .filter(|p| !p.is_empty() && !p.starts_with(appdir))
        .collect();
    Some(if kept.is_empty() { None } else { Some(kept.join(":")) })
}

/// The environment changes `command` applies (empty outside an AppImage).
fn fixes() -> &'static [(OsString, Option<OsString>)] {
    static FIXES: std::sync::OnceLock<Vec<(OsString, Option<OsString>)>> = std::sync::OnceLock::new();
    FIXES.get_or_init(|| {
        let Some(appdir) = appdir() else { return Vec::new() };
        let mut out = Vec::new();
        for (k, v) in std::env::vars_os() {
            let (Some(ks), Some(vs)) = (k.to_str(), v.to_str()) else { continue };
            if let Some(fix) = fix_var(&appdir, ks, vs) {
                out.push((k.clone(), fix.map(OsString::from)));
            }
        }
        if !out.is_empty() {
            let names: Vec<String> = out.iter().map(|(k, _)| k.to_string_lossy().to_string()).collect();
            eprintln!("[vcompare] AppImage: child processes get {} cleaned: {}", names.len(), names.join(" "));
        }
        out
    })
}

/// `Command::new(program)` with the AppImage's environment changes undone.
/// The program is looked up on the cleaned `PATH`.
pub fn command<S: AsRef<OsStr>>(program: S) -> Command {
    let mut c = Command::new(program);
    for (k, v) in fixes() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c
}

/// The `PATH` children see, for looking binaries up by hand.
pub fn path() -> Option<OsString> {
    for (k, v) in fixes() {
        if k == "PATH" {
            return v.clone();
        }
    }
    std::env::var_os("PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appimage_environment_is_undone() {
        let a = "/tmp/.mount_vcompaXyZ";
        // Path lists lose the AppImage's entries (incl. "//usr" spellings).
        assert_eq!(
            fix_var(a, "PATH", "/tmp/.mount_vcompaXyZ/usr/bin/:/tmp/.mount_vcompaXyZ//usr/sbin/:/usr/local/bin:/usr/bin"),
            Some(Some("/usr/local/bin:/usr/bin".into()))
        );
        assert_eq!(
            fix_var(a, "LD_LIBRARY_PATH", "/tmp/.mount_vcompaXyZ/usr/lib/:/tmp/.mount_vcompaXyZ/usr/lib/x86_64-linux-gnu/"),
            Some(None)
        );
        // Single values pointing inside are removed: this is what broke python3.
        assert_eq!(fix_var(a, "PYTHONHOME", "/tmp/.mount_vcompaXyZ/usr/"), Some(None));
        assert_eq!(fix_var(a, "PYTHONPATH", "/tmp/.mount_vcompaXyZ/usr/share/pyshared/:"), Some(None));
        // Forced plain values are removed; unrelated ones are left alone.
        assert_eq!(fix_var(a, "GDK_BACKEND", "x11"), Some(None));
        assert_eq!(fix_var(a, "HOME", "/home/v"), None);
        assert_eq!(fix_var(a, "XDG_DATA_DIRS", "/usr/share"), None);
        assert_eq!(
            fix_var(a, "XDG_DATA_DIRS", "/tmp/.mount_vcompaXyZ/usr/share/:/usr/local/share:/usr/share"),
            Some(Some("/usr/local/share:/usr/share".into()))
        );
    }
}
