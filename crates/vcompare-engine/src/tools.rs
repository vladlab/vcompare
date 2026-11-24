//! Where ffmpeg, ffprobe and mpv are.
//!
//! In order: a path set by the app (`configure`, from its settings), then
//! `VCOMPARE_FFMPEG` / `VCOMPARE_FFPROBE` / `VCOMPARE_MPV`, then the bare
//! name on `PATH`, then a few usual install folders `PATH` may lack (see
//! `fallback_dirs`). If only ffmpeg is set and an `ffprobe` sits next to it,
//! that one is used (static builds ship the two together).

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
    Mpv,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Ffmpeg, Tool::Ffprobe, Tool::Mpv];

    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
            Tool::Mpv => "mpv",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "VCOMPARE_FFMPEG",
            Tool::Ffprobe => "VCOMPARE_FFPROBE",
            Tool::Mpv => "VCOMPARE_MPV",
        }
    }
}

/// Paths set by the app. `None` (or empty) = not set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPaths {
    #[serde(default)]
    pub ffmpeg: Option<PathBuf>,
    #[serde(default)]
    pub ffprobe: Option<PathBuf>,
    #[serde(default)]
    pub mpv: Option<PathBuf>,
}

impl ToolPaths {
    pub fn get(&self, tool: Tool) -> Option<&Path> {
        let p = match tool {
            Tool::Ffmpeg => &self.ffmpeg,
            Tool::Ffprobe => &self.ffprobe,
            Tool::Mpv => &self.mpv,
        };
        p.as_deref().filter(|p| !p.as_os_str().is_empty())
    }
}

static CONFIGURED: RwLock<ToolPaths> = RwLock::new(ToolPaths { ffmpeg: None, ffprobe: None, mpv: None });

/// Set the app's tool paths (at startup and when settings change).
pub fn configure(paths: ToolPaths) {
    *CONFIGURED.write().unwrap_or_else(|e| e.into_inner()) = paths;
}

pub fn configured() -> ToolPaths {
    CONFIGURED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn env_path(tool: Tool) -> Option<PathBuf> {
    std::env::var_os(tool.env_var()).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// What to run for `tool`: a path, or the bare name for a `PATH` lookup.
/// A bare name that isn't on `PATH` but is in one of `fallback_dirs`
/// comes back as that full path.
pub fn program(tool: Tool) -> OsString {
    let p = resolve(tool, &configured(), env_path);
    if p.components().count() == 1 && on_path(&p).is_none() {
        if let Some(found) = fallback_dirs().iter().map(|d| Path::new(d).join(&p)).find(|c| c.is_file()) {
            return found.into_os_string();
        }
    }
    p.into_os_string()
}

/// Where tools live that a desktop launch's `PATH` may not include. Apps
/// started from Finder get only /usr/bin:/bin:/usr/sbin:/sbin, so Homebrew
/// and MacPorts installs would never be found.
fn fallback_dirs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"]
    } else {
        &["/usr/local/bin"]
    }
}

fn on_path(name: &Path) -> Option<PathBuf> {
    let path = crate::sysenv::path()?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|c| c.is_file())
}

/// Where `program` got a tool's path from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The app's settings (`configure`).
    Settings,
    /// `VCOMPARE_FFMPEG` and friends.
    Env,
    /// The ffprobe next to a set ffmpeg.
    NextToFfmpeg,
    /// The bare name, looked up on `PATH`.
    Path,
}

/// Where `tool`'s path comes from right now.
pub fn source(tool: Tool) -> Source {
    resolve_with_source(tool, &configured(), env_path).1
}

fn resolve(tool: Tool, cfg: &ToolPaths, env: impl Fn(Tool) -> Option<PathBuf>) -> PathBuf {
    resolve_with_source(tool, cfg, env).0
}

fn resolve_with_source(tool: Tool, cfg: &ToolPaths, env: impl Fn(Tool) -> Option<PathBuf>) -> (PathBuf, Source) {
    if let Some(p) = cfg.get(tool) {
        return (p.to_path_buf(), Source::Settings);
    }
    if let Some(p) = env(tool) {
        return (p, Source::Env);
    }
    if tool == Tool::Ffprobe {
        let ffmpeg = cfg.get(Tool::Ffmpeg).map(Path::to_path_buf).or_else(|| env(Tool::Ffmpeg));
        if let Some(sibling) = ffmpeg.and_then(|f| sibling_ffprobe(&f)) {
            return (sibling, Source::NextToFfmpeg);
        }
    }
    (PathBuf::from(tool.name()), Source::Path)
}

/// The first line of `tool -version` (`--version` for mpv), trimmed of
/// the copyright tail: "ffmpeg version 6.1.1", "mpv 0.37.0". `Err` says
/// why it couldn't run.
pub fn version(tool: Tool) -> Result<String, String> {
    let flag = if tool == Tool::Mpv { "--version" } else { "-version" };
    let out = command(tool).arg(flag).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    Ok(short_version(line).to_string())
}

fn short_version(line: &str) -> &str {
    let cut = line.find(" Copyright").or_else(|| line.find(" (C)")).unwrap_or(line.len());
    line[..cut].trim()
}

fn sibling_ffprobe(ffmpeg: &Path) -> Option<PathBuf> {
    let name = ffmpeg.file_name()?.to_str()?;
    let probe = ffmpeg.with_file_name(name.replacen("ffmpeg", "ffprobe", 1));
    (probe != ffmpeg && probe.is_file()).then_some(probe)
}

/// A `Command` for `tool` (with the AppImage environment undone, see
/// `sysenv`).
pub fn command(tool: Tool) -> Command {
    crate::sysenv::command(program(tool))
}

/// The file that would run for `tool`, if there is one.
pub fn locate(tool: Tool) -> Option<PathBuf> {
    let p = PathBuf::from(program(tool));
    if p.components().count() > 1 {
        return p.is_file().then_some(p);
    }
    on_path(&p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: Tool) -> Option<PathBuf> {
        None
    }

    #[test]
    fn order_is_config_then_env_then_name() {
        let cfg = ToolPaths { mpv: Some("/opt/mpv/bin/mpv".into()), ..Default::default() };
        assert_eq!(resolve(Tool::Mpv, &cfg, no_env), PathBuf::from("/opt/mpv/bin/mpv"));
        assert_eq!(resolve(Tool::Ffmpeg, &cfg, no_env), PathBuf::from("ffmpeg"));
        let env = |t: Tool| (t == Tool::Ffmpeg).then(|| PathBuf::from("/env/ffmpeg"));
        assert_eq!(resolve(Tool::Ffmpeg, &cfg, env), PathBuf::from("/env/ffmpeg"));
        // Config beats env.
        let cfg2 = ToolPaths { ffmpeg: Some("/cfg/ffmpeg".into()), ..Default::default() };
        assert_eq!(resolve(Tool::Ffmpeg, &cfg2, env), PathBuf::from("/cfg/ffmpeg"));
        // Empty counts as unset.
        let empty = ToolPaths { ffmpeg: Some(PathBuf::new()), ..Default::default() };
        assert_eq!(resolve(Tool::Ffmpeg, &empty, no_env), PathBuf::from("ffmpeg"));
    }

    #[test]
    fn source_says_where_a_path_came_from() {
        let cfg = ToolPaths { mpv: Some("/opt/mpv".into()), ..Default::default() };
        assert_eq!(resolve_with_source(Tool::Mpv, &cfg, no_env).1, Source::Settings);
        assert_eq!(resolve_with_source(Tool::Ffmpeg, &cfg, no_env).1, Source::Path);
        let env = |t: Tool| (t == Tool::Ffmpeg).then(|| PathBuf::from("/env/ffmpeg"));
        assert_eq!(resolve_with_source(Tool::Ffmpeg, &cfg, env).1, Source::Env);
    }

    #[test]
    fn version_line_drops_the_copyright() {
        assert_eq!(
            short_version("ffmpeg version 6.1.1 Copyright (c) 2000-2023 the FFmpeg developers"),
            "ffmpeg version 6.1.1"
        );
        assert_eq!(short_version("mpv 0.37.0 Copyright © 2000-2023 mpv/MPlayer/mplayer2 projects"), "mpv 0.37.0");
        assert_eq!(short_version("ffprobe version n7.1"), "ffprobe version n7.1");
    }

    #[test]
    fn ffprobe_follows_a_configured_ffmpeg() {
        let dir = std::env::temp_dir().join(format!("vcompare_tools_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ffmpeg = dir.join("ffmpeg");
        std::fs::write(&ffmpeg, b"").unwrap();
        let cfg = ToolPaths { ffmpeg: Some(ffmpeg.clone()), ..Default::default() };
        // No sibling: falls back to PATH.
        assert_eq!(resolve(Tool::Ffprobe, &cfg, no_env), PathBuf::from("ffprobe"));
        std::fs::write(dir.join("ffprobe"), b"").unwrap();
        assert_eq!(resolve(Tool::Ffprobe, &cfg, no_env), dir.join("ffprobe"));
        // An explicit ffprobe still wins.
        let cfg2 = ToolPaths { ffprobe: Some("/x/ffprobe".into()), ..cfg };
        assert_eq!(resolve(Tool::Ffprobe, &cfg2, no_env), PathBuf::from("/x/ffprobe"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
