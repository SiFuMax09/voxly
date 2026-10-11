//! Control socket for compositors without global shortcuts (Wayland).
//!
//! The running app listens on `$XDG_RUNTIME_DIR/voxly.sock`. `voxly start`,
//! `voxly stop`, `voxly toggle`, `voxly paste-last` and `voxly show` send one
//! command to it and exit, so the compositor's own keybinds can drive Voxly.
//! `start`/`stop` map to the press and release of a bind (Hyprland: `bind` and
//! `bindr`), which gives hold-to-record.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use tauri::AppHandle;

const COMMANDS: &[&str] = &["start", "stop", "toggle", "paste-last", "show"];

fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("voxly.sock")
}

/// Handles `voxly <command>` when the first argument is a known command.
/// Returns true if the process should exit instead of starting the app.
pub fn run_client() -> bool {
    let Some(cmd) = std::env::args().nth(1) else {
        return false;
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        eprintln!("usage: voxly [{}]", COMMANDS.join("|"));
        return true;
    }
    if !COMMANDS.contains(&cmd.as_str()) {
        return false;
    }
    match UnixStream::connect(socket_path()).and_then(|mut s| s.write_all(cmd.as_bytes())) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("voxly is not running ({e}); start the app first");
            std::process::exit(1);
        }
    }
    true
}

pub fn spawn_server(app: &AppHandle) {
    let path = socket_path();
    // A stale socket from a crashed run would make bind fail. If something
    // still answers, another instance owns it and we leave it alone.
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            eprintln!("another Voxly instance already owns {}", path.display());
            return;
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return eprintln!("could not open control socket: {e}"),
    };
    let app = app.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut cmd = String::new();
            let _ = stream.take(32).read_to_string(&mut cmd);
            crate::app::handle_command(&app, cmd.trim());
        }
    });
}
