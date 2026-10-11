mod app;
#[cfg(target_os = "linux")]
mod evdev_hotkeys;
#[cfg(target_os = "linux")]
mod vkeys;
#[cfg(unix)]
mod ipc;

/// Returns true if this invocation was a control command (`voxly toggle`, ...)
/// that has been forwarded to the running instance.
#[cfg(unix)]
pub fn run_client() -> bool {
    ipc::run_client()
}

#[cfg(not(unix))]
pub fn run_client() -> bool {
    false
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    app::run();
}
