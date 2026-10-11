use std::path::PathBuf;
use std::sync::Mutex;

use voxly_core::audio::Recorder;
use voxly_core::local;
use voxly_core::settings::Settings;
use voxly_core::transcribe::transcribe;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    models_dir: PathBuf,
    recorder: Recorder,
    last: Mutex<String>,
    busy: Mutex<bool>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "lowercase")]
enum Status {
    Idle,
    Recording,
    Transcribing,
}

fn set_status(app: &AppHandle, s: Status) {
    let _ = app.emit("status", s);
}

fn report(app: &AppHandle, err: impl std::fmt::Display) {
    let _ = app.emit("error", err.to_string());
    set_status(app, Status::Idle);
}

fn start_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
    if *state.busy.lock().unwrap() {
        return;
    }
    match state.recorder.start() {
        Ok(()) => set_status(app, Status::Recording),
        Err(e) => report(app, e),
    }
}

fn stop_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
    if !state.recorder.is_recording() {
        return;
    }
    let wav = match state.recorder.stop() {
        Ok(w) => w,
        Err(e) => return report(app, e),
    };
    *state.busy.lock().unwrap() = true;
    set_status(app, Status::Transcribing);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let settings = state.settings.lock().unwrap().clone();
        let result = transcribe(&settings, wav, &state.models_dir).await;
        *state.busy.lock().unwrap() = false;
        match result {
            Ok(text) => {
                *state.last.lock().unwrap() = text.clone();
                let _ = app.emit("transcript", &text);
                set_status(&app, Status::Idle);
                if !text.is_empty() {
                    output(&app, &text, settings.auto_paste);
                }
            }
            Err(e) => report(&app, format!("{e:#}")),
        }
    });
}

fn toggle_recording(app: &AppHandle) {
    if app.state::<AppState>().recorder.is_recording() {
        stop_recording(app);
    } else {
        start_recording(app);
    }
}

/// Puts the text on the clipboard and, if asked, types Cmd/Ctrl+V into the
/// focused application.
fn output(app: &AppHandle, text: &str, paste: bool) {
    if let Err(e) = app.clipboard().write_text(text) {
        return report(app, format!("clipboard: {e}"));
    }
    #[cfg(desktop)]
    if paste {
        let handle = app.clone();
        // Enigo must run on the main thread on macOS.
        let _ = app.run_on_main_thread(move || {
            if let Err(e) = simulate_paste() {
                report(&handle, format!("paste failed: {e}"));
            }
        });
    }
    #[cfg(not(desktop))]
    let _ = paste;
}

#[cfg(desktop)]
fn simulate_paste() -> Result<(), String> {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;
    // Give the user a moment to release the hotkey modifiers.
    std::thread::sleep(std::time::Duration::from_millis(80));
    enigo.key(modifier, Direction::Press).map_err(|e| e.to_string())?;
    let click = enigo.key(Key::Unicode('v'), Direction::Click);
    let release = enigo.key(modifier, Direction::Release);
    click.and(release).map_err(|e| e.to_string())
}

fn paste_last(app: &AppHandle) {
    let state = app.state::<AppState>();
    let text = state.last.lock().unwrap().clone();
    let auto = state.settings.lock().unwrap().auto_paste;
    if text.is_empty() {
        return report(app, "nothing has been recorded yet");
    }
    output(app, &text, auto);
}

/// A transparent, click-through pill at the top of the screen. It stays open
/// and only fades in and out (see ui/overlay.html), so showing it never takes
/// focus away from the app being dictated into.
#[cfg(desktop)]
fn create_overlay(app: &AppHandle) -> tauri::Result<()> {
    use tauri::{LogicalPosition, WebviewUrl, WebviewWindowBuilder};

    const W: f64 = 220.0;
    const H: f64 = 56.0;
    let win = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("Voxly overlay")
        .inner_size(W, H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible_on_all_workspaces(true)
        .build()?;
    win.set_ignore_cursor_events(true)?;
    if let Some(mon) = win.primary_monitor()? {
        let scale = mon.scale_factor();
        let x = mon.position().x as f64 / scale + (mon.size().width as f64 / scale - W) / 2.0;
        let y = mon.position().y as f64 / scale + 44.0;
        win.set_position(LogicalPosition::new(x, y))?;
    }
    Ok(())
}

#[cfg(desktop)]
fn register_hotkeys(app: &AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

    let hk = app.state::<AppState>().settings.lock().unwrap().hotkeys.clone();
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(|e| e.to_string())?;

    let reg = |keys: &str, f: fn(&AppHandle, ShortcutState)| -> Result<(), String> {
        if keys.trim().is_empty() {
            return Ok(());
        }
        gs.on_shortcut(keys, move |app, _, ev| f(app, ev.state()))
            .map_err(|e| format!("'{keys}': {e}"))
    };
    reg(&hk.hold, |app, st| match st {
        ShortcutState::Pressed => start_recording(app),
        ShortcutState::Released => stop_recording(app),
    })?;
    reg(&hk.toggle, |app, st| {
        if st == ShortcutState::Pressed {
            toggle_recording(app)
        }
    })?;
    reg(&hk.paste_last, |app, st| {
        if st == ShortcutState::Pressed {
            paste_last(app)
        }
    })
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Result<(), String> {
    settings.save(&state.settings_path).map_err(|e| e.to_string())?;
    *state.settings.lock().unwrap() = settings;
    #[cfg(desktop)]
    register_hotkeys(&app)?;
    #[cfg(not(desktop))]
    let _ = app;
    Ok(())
}

#[tauri::command]
fn list_models(state: State<AppState>) -> Vec<local::ModelStatus> {
    local::list_models(&state.models_dir)
}

#[tauri::command]
async fn download_model(app: AppHandle, id: String) -> Result<(), String> {
    let dir = app.state::<AppState>().models_dir.clone();
    let emitter = app.clone();
    local::download_model(&dir, &id, move |done, total| {
        let _ = emitter.emit("download", (done, total));
    })
    .await
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn toggle(app: AppHandle) {
    toggle_recording(&app);
}

#[tauri::command]
fn get_last(state: State<AppState>) -> String {
    state.last.lock().unwrap().clone()
}

#[tauri::command]
fn paste_last_cmd(app: AppHandle) {
    paste_last(&app);
}

pub fn run() {
    let builder = tauri::Builder::default().plugin(tauri_plugin_clipboard_manager::init());
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());

    builder
        .setup(|app| {
            let path = app.path().app_config_dir()?.join("settings.json");
            let settings = Settings::load(&path).unwrap_or_else(|e| {
                eprintln!("could not load settings: {e:#}");
                Settings::default()
            });
            app.manage(AppState {
                settings: Mutex::new(settings),
                settings_path: path,
                models_dir: app.path().app_data_dir()?.join("models"),
                recorder: Recorder::default(),
                last: Mutex::new(String::new()),
                busy: Mutex::new(false),
            });
            #[cfg(desktop)]
            if let Err(e) = create_overlay(app.handle()) {
                eprintln!("could not create overlay: {e}");
            }
            #[cfg(desktop)]
            if let Err(e) = register_hotkeys(app.handle()) {
                eprintln!("could not register hotkeys: {e}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // The overlay window would otherwise keep the app alive.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    window.app_handle().exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            list_models,
            download_model,
            toggle,
            get_last,
            paste_last_cmd
        ])
        .run(tauri::generate_context!())
        .expect("error while running Voxly");
}
