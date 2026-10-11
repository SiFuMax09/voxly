use std::path::PathBuf;
use std::sync::Mutex;

use voxly_core::audio::Recorder;
use voxly_core::history::History;
use voxly_core::local;
use voxly_core::settings::Settings;
use voxly_core::transcribe::transcribe;
use voxly_core::usage::{self, Usage};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    usage: Mutex<Usage>,
    usage_path: PathBuf,
    history: Mutex<History>,
    history_path: PathBuf,
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

/// How long to keep recording after the key is released, so the last word of
/// fast speech is not cut off.
const TAIL: std::time::Duration = std::time::Duration::from_millis(350);

fn stop_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
    if !state.recorder.is_recording() {
        return;
    }
    {
        let mut busy = state.busy.lock().unwrap();
        if *busy {
            return;
        }
        *busy = true;
    }
    set_status(app, Status::Transcribing);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = tauri::async_runtime::spawn_blocking(|| std::thread::sleep(TAIL)).await;
        let state = app.state::<AppState>();
        let wav = match state.recorder.stop() {
            Ok(w) => w,
            Err(e) => {
                *state.busy.lock().unwrap() = false;
                return report(&app, e);
            }
        };
        let settings = state.settings.lock().unwrap().clone();
        let seconds = usage::wav_seconds(&wav);
        let result = transcribe(&settings, wav, &state.models_dir).await;
        *state.busy.lock().unwrap() = false;
        match result {
            Ok(text) => {
                *state.last.lock().unwrap() = text.clone();
                let _ = app.emit("transcript", &text);
                set_status(&app, Status::Idle);
                if !text.is_empty() {
                    record_usage(&app, &text, seconds);
                    output(&app, &text, settings.auto_paste);
                }
            }
            Err(e) => report(&app, format!("{e:#}")),
        }
    });
}

fn record_usage(app: &AppHandle, text: &str, seconds: u64) {
    let state = app.state::<AppState>();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let snapshot = {
        let mut u = state.usage.lock().unwrap();
        u.record(&date, usage::count_words(text), seconds);
        u.clone()
    };
    if let Err(e) = snapshot.save(&state.usage_path) {
        eprintln!("could not save usage: {e:#}");
    }
    let _ = app.emit("usage", snapshot);

    let history = {
        let mut h = state.history.lock().unwrap();
        h.add(chrono::Local::now().timestamp_millis(), text);
        h.clone()
    };
    if let Err(e) = history.save(&state.history_path) {
        eprintln!("could not save history: {e:#}");
    }
    let _ = app.emit("history", history);
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

/// `always_on_top` only floats above normal windows on macOS. To also cover
/// full-screen apps, the menu bar and every Space, the window needs the
/// screen-saver level and a collection behavior that joins all Spaces.
#[cfg(target_os = "macos")]
fn raise_above_everything(win: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    const SCREEN_SAVER_LEVEL: isize = 1000;
    // canJoinAllSpaces | stationary | ignoresCycle | fullScreenAuxiliary
    const BEHAVIOR: usize = (1 << 0) | (1 << 4) | (1 << 6) | (1 << 8);
    if let Ok(ptr) = win.ns_window() {
        let ns = ptr as *mut AnyObject;
        unsafe {
            let _: () = msg_send![ns, setLevel: SCREEN_SAVER_LEVEL];
            let _: () = msg_send![ns, setCollectionBehavior: BEHAVIOR];
        }
    }
}

#[cfg(all(desktop, not(target_os = "macos")))]
fn raise_above_everything(_win: &tauri::WebviewWindow) {}

/// Tray icon so the app keeps running (and the hotkeys keep working) after
/// the settings window is closed.
#[cfg(desktop)]
fn create_tray(app: &AppHandle) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::TrayIconBuilder;

    let open = MenuItem::with_id(app, "open", "Open Voxly", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Voxly", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::new()
        .tooltip("Voxly")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg(desktop)]
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
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
    raise_above_everything(&win);
    win.show()?;
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
fn get_usage(state: State<AppState>) -> Usage {
    state.usage.lock().unwrap().clone()
}

#[tauri::command]
fn get_history(state: State<AppState>) -> History {
    state.history.lock().unwrap().clone()
}

#[tauri::command]
fn delete_history_entry(state: State<AppState>, id: i64) -> Result<History, String> {
    update_history(&state, |h| h.remove(id))
}

#[tauri::command]
fn clear_history(state: State<AppState>) -> Result<History, String> {
    update_history(&state, History::clear)
}

fn update_history(state: &AppState, f: impl FnOnce(&mut History)) -> Result<History, String> {
    let snapshot = {
        let mut h = state.history.lock().unwrap();
        f(&mut h);
        h.clone()
    };
    snapshot.save(&state.history_path).map_err(|e| format!("{e:#}"))?;
    Ok(snapshot)
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
            let usage_path = app.path().app_config_dir()?.join("usage.json");
            let usage = Usage::load(&usage_path).unwrap_or_else(|e| {
                eprintln!("could not load usage: {e:#}");
                Usage::default()
            });
            let history_path = app.path().app_config_dir()?.join("history.json");
            let history = History::load(&history_path).unwrap_or_else(|e| {
                eprintln!("could not load history: {e:#}");
                History::default()
            });
            app.manage(AppState {
                history: Mutex::new(history),
                history_path,
                settings: Mutex::new(settings),
                settings_path: path,
                usage: Mutex::new(usage),
                usage_path,
                models_dir: app.path().app_data_dir()?.join("models"),
                recorder: Recorder::default(),
                last: Mutex::new(String::new()),
                busy: Mutex::new(false),
            });
            // A regular (Dock) app cannot show windows over full-screen Spaces,
            // whatever the window level. As a tray-only "accessory" app it can.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            #[cfg(desktop)]
            if let Err(e) = create_overlay(app.handle()) {
                eprintln!("could not create overlay: {e}");
            }
            #[cfg(desktop)]
            if let Err(e) = create_tray(app.handle()) {
                eprintln!("could not create tray icon: {e}");
            }
            #[cfg(desktop)]
            if let Err(e) = register_hotkeys(app.handle()) {
                eprintln!("could not register hotkeys: {e}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the settings window only hides it; the app keeps
            // running in the tray. Quit via the tray menu.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
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
            get_usage,
            get_history,
            delete_history_entry,
            clear_history,
            paste_last_cmd
        ])
        .build(tauri::generate_context!())
        .expect("error while building Voxly")
        .run(|_app, _event| {
            // Clicking the Dock icon brings the settings window back.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = _event {
                show_main(_app);
            }
        });
}
