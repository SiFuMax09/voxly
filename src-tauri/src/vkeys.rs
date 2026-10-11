//! A virtual keyboard made with uinput, to paste text and delete it again.
//!
//! The kernel hands the keys to whatever compositor or X server is running,
//! so this works on X11 and on every Wayland compositor (also GNOME, which
//! refuses the Wayland virtual-keyboard protocol). It needs write access to
//! `/dev/uinput`, which the "Enable global hotkeys" button also sets up.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

static DEVICE: OnceLock<Mutex<VirtualDevice>> = OnceLock::new();

fn device() -> Result<&'static Mutex<VirtualDevice>, String> {
    if let Some(d) = DEVICE.get() {
        return Ok(d);
    }
    let mut keys = AttributeSet::<KeyCode>::new();
    for k in [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_V, KeyCode::KEY_BACKSPACE] {
        keys.insert(k);
    }
    let dev = VirtualDevice::builder()
        .and_then(|b| b.name("Voxly virtual keyboard").with_keys(&keys))
        .and_then(|b| b.build())
        .map_err(|e| format!("could not create a virtual keyboard ({e}); is /dev/uinput writable?"))?;
    Ok(DEVICE.get_or_init(|| Mutex::new(dev)))
}

/// Creates the device ahead of time. The compositor needs a moment to notice
/// a new keyboard, so doing this at startup makes the first paste work.
pub fn init() {
    if let Err(e) = device() {
        eprintln!("{e}");
    }
}

fn send(dev: &mut VirtualDevice, key: KeyCode, down: bool) -> Result<(), String> {
    dev.emit(&[InputEvent::new(EventType::KEY.0, key.code(), i32::from(down))])
        .map_err(|e| e.to_string())
}

/// Presses `mods`, taps `key` `times` times, and releases `mods` again.
pub fn tap(mods: &[KeyCode], key: KeyCode, times: usize) -> Result<(), String> {
    let mut dev = device()?.lock().unwrap();
    let gap = Duration::from_millis(4);
    for &m in mods {
        send(&mut dev, m, true)?;
    }
    std::thread::sleep(gap);
    let result = (|| {
        for _ in 0..times {
            send(&mut dev, key, true)?;
            std::thread::sleep(gap);
            send(&mut dev, key, false)?;
            std::thread::sleep(gap);
        }
        Ok(())
    })();
    for &m in mods.iter().rev() {
        let _ = send(&mut dev, m, false);
    }
    result
}
