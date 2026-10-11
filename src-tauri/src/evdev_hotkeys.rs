//! Global hotkeys for Wayland, read straight from the kernel input devices.
//!
//! Wayland has no way to grab keys globally, but `/dev/input/event*` works
//! under any compositor as long as the user is in the `input` group. Keys are
//! only observed, never grabbed, so they still reach the focused app too.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use evdev::{Device, EventType, KeyCode};
use tauri::AppHandle;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Hold,
    Toggle,
    PasteLast,
}

struct Binding {
    action: Action,
    /// One entry per required modifier; any key of the group satisfies it.
    mods: Vec<&'static [KeyCode]>,
    key: KeyCode,
    active: bool,
}

#[derive(Default)]
struct State {
    bindings: Vec<Binding>,
    pressed: HashSet<KeyCode>,
}

static STATE: OnceLock<Arc<Mutex<State>>> = OnceLock::new();

const CTRL: &[KeyCode] = &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL];
const SHIFT: &[KeyCode] = &[KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT];
const ALT: &[KeyCode] = &[KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT];
const SUPER: &[KeyCode] = &[KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA];
const ALL_MODS: [&[KeyCode]; 4] = [CTRL, SHIFT, ALT, SUPER];

/// Parses the accelerator strings the UI produces ("Control+Alt+Space").
fn parse(spec: &str) -> Option<(Vec<&'static [KeyCode]>, KeyCode)> {
    let mut mods = Vec::new();
    let mut key = None;
    for part in spec.split('+') {
        match part.trim() {
            "CommandOrControl" | "CmdOrCtrl" | "Control" | "Ctrl" | "Command" | "Cmd" => {
                mods.push(CTRL)
            }
            "Alt" | "Option" => mods.push(ALT),
            "Shift" => mods.push(SHIFT),
            "Super" | "Meta" => mods.push(SUPER),
            other => key = Some(key_from_code(other)?),
        }
    }
    Some((mods, key?))
}

/// Maps a DOM `KeyboardEvent.code` ("KeyA", "Digit1", "F5", "Space") to evdev.
fn key_from_code(code: &str) -> Option<KeyCode> {
    const LETTERS: [KeyCode; 26] = [
        KeyCode::KEY_A, KeyCode::KEY_B, KeyCode::KEY_C, KeyCode::KEY_D, KeyCode::KEY_E,
        KeyCode::KEY_F, KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_I, KeyCode::KEY_J,
        KeyCode::KEY_K, KeyCode::KEY_L, KeyCode::KEY_M, KeyCode::KEY_N, KeyCode::KEY_O,
        KeyCode::KEY_P, KeyCode::KEY_Q, KeyCode::KEY_R, KeyCode::KEY_S, KeyCode::KEY_T,
        KeyCode::KEY_U, KeyCode::KEY_V, KeyCode::KEY_W, KeyCode::KEY_X, KeyCode::KEY_Y,
        KeyCode::KEY_Z,
    ];
    const DIGITS: [KeyCode; 10] = [
        KeyCode::KEY_0, KeyCode::KEY_1, KeyCode::KEY_2, KeyCode::KEY_3, KeyCode::KEY_4,
        KeyCode::KEY_5, KeyCode::KEY_6, KeyCode::KEY_7, KeyCode::KEY_8, KeyCode::KEY_9,
    ];
    const FKEYS: [KeyCode; 12] = [
        KeyCode::KEY_F1, KeyCode::KEY_F2, KeyCode::KEY_F3, KeyCode::KEY_F4, KeyCode::KEY_F5,
        KeyCode::KEY_F6, KeyCode::KEY_F7, KeyCode::KEY_F8, KeyCode::KEY_F9, KeyCode::KEY_F10,
        KeyCode::KEY_F11, KeyCode::KEY_F12,
    ];
    // The UI sends DOM codes ("KeyV", "Digit1"); the defaults use bare "V".
    let code = code.strip_prefix("Key").or(code.strip_prefix("Digit")).unwrap_or(code);
    if let [c] = code.to_ascii_uppercase().as_bytes() {
        match c {
            b'A'..=b'Z' => return LETTERS.get((c - b'A') as usize).copied(),
            b'0'..=b'9' => return DIGITS.get((c - b'0') as usize).copied(),
            _ => {}
        }
    }
    if let Some(n) = code.strip_prefix('F').and_then(|n| n.parse::<usize>().ok()) {
        const HIGH: [KeyCode; 12] = [
            KeyCode::KEY_F13, KeyCode::KEY_F14, KeyCode::KEY_F15, KeyCode::KEY_F16,
            KeyCode::KEY_F17, KeyCode::KEY_F18, KeyCode::KEY_F19, KeyCode::KEY_F20,
            KeyCode::KEY_F21, KeyCode::KEY_F22, KeyCode::KEY_F23, KeyCode::KEY_F24,
        ];
        return match n {
            1..=12 => FKEYS.get(n - 1).copied(),
            13..=24 => HIGH.get(n - 13).copied(),
            _ => None,
        };
    }
    Some(match code {
        "Space" => KeyCode::KEY_SPACE,
        "Enter" => KeyCode::KEY_ENTER,
        "Escape" => KeyCode::KEY_ESC,
        "Backspace" => KeyCode::KEY_BACKSPACE,
        "Delete" => KeyCode::KEY_DELETE,
        "Tab" => KeyCode::KEY_TAB,
        "ArrowUp" | "Up" => KeyCode::KEY_UP,
        "ArrowDown" | "Down" => KeyCode::KEY_DOWN,
        "ArrowLeft" | "Left" => KeyCode::KEY_LEFT,
        "ArrowRight" | "Right" => KeyCode::KEY_RIGHT,
        "Minus" => KeyCode::KEY_MINUS,
        "Equal" => KeyCode::KEY_EQUAL,
        "Comma" => KeyCode::KEY_COMMA,
        "Period" => KeyCode::KEY_DOT,
        "Slash" => KeyCode::KEY_SLASH,
        "Backslash" => KeyCode::KEY_BACKSLASH,
        "Semicolon" => KeyCode::KEY_SEMICOLON,
        "Quote" => KeyCode::KEY_APOSTROPHE,
        "Backquote" => KeyCode::KEY_GRAVE,
        "BracketLeft" => KeyCode::KEY_LEFTBRACE,
        "BracketRight" => KeyCode::KEY_RIGHTBRACE,
        _ => return None,
    })
}

/// Replaces the active hotkeys. Starts the device listeners on first use.
pub fn configure(app: &AppHandle, keys: [(Action, &str); 3]) -> Result<(), String> {
    let mut bindings = Vec::new();
    let mut bad = Vec::new();
    for (action, spec) in keys {
        if spec.trim().is_empty() {
            continue;
        }
        // One bad key must not disable the others.
        match parse(spec) {
            Some((mods, key)) => bindings.push(Binding { action, mods, key, active: false }),
            None => bad.push(format!("'{spec}': unsupported key")),
        }
    }
    let state = STATE.get_or_init(Default::default).clone();
    {
        let mut s = state.lock().unwrap();
        s.bindings = bindings;
    }
    // Retried on every call until at least one keyboard could be opened, so
    // granting access later (see grant_access) works without a restart.
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !STARTED.load(std::sync::atomic::Ordering::SeqCst) {
        start_listeners(app, state)?;
        STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    if bad.is_empty() { Ok(()) } else { Err(bad.join(", ")) }
}

/// True while Ctrl, Shift, Alt or Super is physically held down. Text cannot
/// be pasted then: the held key would turn Ctrl+V into another shortcut.
pub fn modifiers_held() -> bool {
    STATE.get().is_some_and(|s| {
        let s = s.lock().unwrap();
        ALL_MODS.iter().flat_map(|g| g.iter()).any(|k| s.pressed.contains(k))
    })
}

/// True if the keyboards can be read, so hotkeys will work.
pub fn has_access() -> bool {
    evdev::enumerate().any(|(_, d)| d.supported_keys().is_some_and(|k| k.contains(KeyCode::KEY_A)))
}

/// Lets the logged-in user read keyboard devices without joining the `input`
/// group or logging out again: a udev rule tags keyboards with `uaccess`.
/// Runs through polkit, so the user sees the normal admin password dialog.
pub fn grant_access() -> Result<(), String> {
    // Keyboards (for the hotkeys) and /dev/uinput (to paste text).
    const RULE: &str = r#"SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_KEYBOARD}=="1", TAG+="uaccess""#;
    const RULE_UINPUT: &str = r#"KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput""#;
    let script = format!(
        "printf '%s\\n' '{RULE}' '{RULE_UINPUT}' > /etc/udev/rules.d/70-voxly-keyboard.rules \
         && udevadm control --reload \
         && udevadm trigger --subsystem-match=input --action=change"
    );
    let status = std::process::Command::new("pkexec")
        .args(["sh", "-c", &script])
        .status()
        .map_err(|e| format!("could not run pkexec: {e}"))?;
    if !status.success() {
        return Err("keyboard access was not granted".into());
    }
    // The ACL is applied asynchronously by udev.
    for _ in 0..20 {
        if has_access() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err("the rule was installed, but the keyboards are still not readable; log out and in again".into())
}

fn start_listeners(app: &AppHandle, state: Arc<Mutex<State>>) -> Result<(), String> {
    let mut found = 0;
    let mut denied = false;
    for (path, _) in evdev::enumerate() {
        let dev = match Device::open(&path) {
            Ok(d) => d,
            Err(e) => {
                denied |= e.kind() == std::io::ErrorKind::PermissionDenied;
                continue;
            }
        };
        // Real keyboards have letter keys; skips mice, power buttons, etc.
        if !dev.supported_keys().is_some_and(|k| k.contains(KeyCode::KEY_A)) {
            continue;
        }
        found += 1;
        let (app, state) = (app.clone(), state.clone());
        std::thread::spawn(move || listen(app, state, dev));
    }
    if found == 0 {
        return Err(if denied {
            "no access to /dev/input; add your user to the 'input' group (sudo usermod -aG input $USER) and log in again".into()
        } else {
            "no keyboard found in /dev/input".into()
        });
    }
    Ok(())
}

fn listen(app: AppHandle, state: Arc<Mutex<State>>, mut dev: Device) {
    loop {
        let events = match dev.fetch_events() {
            Ok(e) => e,
            Err(_) => return, // device unplugged
        };
        for ev in events {
            if ev.event_type() != EventType::KEY || ev.value() == 2 {
                continue;
            }
            let key = KeyCode::new(ev.code());
            let actions = {
                let mut s = state.lock().unwrap();
                if ev.value() == 1 {
                    s.pressed.insert(key);
                } else {
                    s.pressed.remove(&key);
                }
                evaluate(&mut s)
            };
            for (action, pressed) in actions {
                crate::app::hotkey_event(&app, action, pressed);
            }
        }
    }
}

/// Returns (action, pressed) transitions caused by the current key state.
fn evaluate(s: &mut State) -> Vec<(Action, bool)> {
    let State { bindings, pressed } = s;
    let mut out = Vec::new();
    for b in bindings.iter_mut() {
        let held = pressed.contains(&b.key)
            && b.mods.iter().all(|g| g.iter().any(|k| pressed.contains(k)))
            // Extra modifiers mean a different combo (Ctrl+Shift+Space != Ctrl+Space).
            && ALL_MODS
                .iter()
                .filter(|g| !b.mods.contains(g))
                .all(|g| !g.iter().any(|k| pressed.contains(k)));
        if held != b.active {
            b.active = held;
            out.push((b.action, held));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(spec: &str, pressed: &[KeyCode]) -> State {
        let (mods, key) = parse(spec).unwrap();
        State {
            bindings: vec![Binding { action: Action::Hold, mods, key, active: false }],
            pressed: pressed.iter().copied().collect(),
        }
    }

    #[test]
    fn hold_press_and_release() {
        let mut s = state("CommandOrControl+Alt+Space", &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTALT, KeyCode::KEY_SPACE]);
        assert_eq!(evaluate(&mut s).len(), 1);
        assert!(evaluate(&mut s).is_empty());
        s.pressed.remove(&KeyCode::KEY_SPACE);
        assert_eq!(evaluate(&mut s), vec![(Action::Hold, false)]);
    }

    #[test]
    fn extra_modifier_does_not_match() {
        let mut s = state("Control+Space", &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_SPACE]);
        assert!(evaluate(&mut s).is_empty());
    }

    #[test]
    fn parses_dom_codes() {
        assert_eq!(parse("Control+Shift+KeyV").unwrap().1, KeyCode::KEY_V);
        assert_eq!(parse("Alt+F5").unwrap().1, KeyCode::KEY_F5);
        assert_eq!(parse("Super+Digit1").unwrap().1, KeyCode::KEY_1);
        assert!(parse("Alt+Bogus").is_none());
        assert_eq!(parse("CommandOrControl+Shift+V").unwrap().1, KeyCode::KEY_V);
    }
}
