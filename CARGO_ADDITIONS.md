# vcompare Phase 1+2: Cargo.toml additions
#
# Add these to your [dependencies] in src-tauri/Cargo.toml if not already present.
# Most are likely already there from Tauri's defaults.

# Required for sidecar process group management (setpgid, killpg)
# libc = "0.2"

# Required for tokio::task::spawn_blocking in bridge commands
# If Tauri already pulls in tokio (it does by default), you just need
# the feature flag. Check your existing tokio dep.
# tokio = { version = "1", features = ["rt", "sync"] }

# Already required by Tauri — just verify these are present:
# serde = { version = "1", features = ["derive"] }
# serde_json = "1"
# log = "0.4"
# env_logger = "0.11"           # or whatever version you're using
# tauri = { version = "2", features = [...] }
# tauri-plugin-dialog = "2"
# tauri-plugin-shell = "2"
