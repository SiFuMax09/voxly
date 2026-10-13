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
    /// Characters inserted by the last paste, for the "delete that" command.
    inserted: Mutex<usize>,
    stream: Mutex<Option<StreamJob>>,
    /// Streamed text that could not be pasted yet because a modifier key of
    /// the hotkey was still held down.
    pending: Mutex<String>,
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
    // The Wayland overlay is a layer surface that exists only while active.
    #[cfg(target_os = "linux")]
    if is_wayland() {
        if let Some(w) = app.get_webview_window("overlay") {
            if matches!(s, Status::Idle) {
                let _ = w.hide();
            } else {
                let _ = w.show();
                // Needs a realized window, so it can only be set after show.
                let _ = w.set_ignore_cursor_events(true);
            }
        }
    }
    let _ = app.emit("status", s);
}

fn report(app: &AppHandle, err: impl std::fmt::Display) {
    let _ = app.emit("error", err.to_string());
    set_status(app, Status::Idle);
}

/// Background task that transcribes finished parts of a running recording.
struct StreamJob {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: tauri::async_runtime::JoinHandle<Result<StreamOutcome, String>>,
}

fn start_recording(app: &AppHandle) {
    let state = app.state::<AppState>();
    if *state.busy.lock().unwrap() {
        return;
    }
    let settings = state.settings.lock().unwrap().clone();
    match state.recorder.start(&settings.input_device) {
        Ok(()) => {
            set_status(app, Status::Recording);
            // Streaming only helps the local model, and compare mode needs the
            // whole audio for both sides.
            if settings.streaming && settings.backend == "local" && !settings.compare_mode {
                *state.inserted.lock().unwrap() = 0;
                state.pending.lock().unwrap().clear();
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let handle = tauri::async_runtime::spawn(stream_worker(app.clone(), stop.clone()));
                *state.stream.lock().unwrap() = Some(StreamJob { stop, handle });
            }
        }
        Err(e) => report(app, e),
    }
}

fn modifiers_held() -> bool {
    #[cfg(target_os = "linux")]
    return crate::evdev_hotkeys::modifiers_held();
    #[cfg(not(target_os = "linux"))]
    false
}

/// Types one finished chunk into the focused app right away. While a
/// modifier of the hotkey is held, the text is kept and goes in with the next
/// chunk that finds the keys released, or at the end of the dictation.
fn insert_live(app: &AppHandle, piece: &str, paste: bool) {
    if !paste {
        return;
    }
    let state = app.state::<AppState>();
    if modifiers_held() {
        let first = {
            let mut pending = state.pending.lock().unwrap();
            let first = pending.is_empty();
            pending.push_str(piece);
            first
        };
        if first {
            let _ = app.emit(
                "notice",
                "Live typing waits while Ctrl, Alt or Shift of the hotkey are held. \
                 Use a function key such as F9 as hold hotkey to type while you speak.",
            );
        }
        return;
    }
    let text = std::mem::take(&mut *state.pending.lock().unwrap()) + piece;
    paste_now(app, &text);
}

/// Pastes the text that was held back, once the hotkey keys are released.
fn flush_pending(app: &AppHandle) {
    for _ in 0..40 {
        if !modifiers_held() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let text = std::mem::take(&mut *app.state::<AppState>().pending.lock().unwrap());
    if !text.is_empty() {
        paste_now(app, &text);
    }
}

fn paste_now(app: &AppHandle, text: &str) {
    if let Err(e) = set_clipboard(app, text) {
        return report(app, format!("clipboard: {e}"));
    }
    *app.state::<AppState>().inserted.lock().unwrap() += text.chars().count();
    // Wait until the paste is done: the clipboard is overwritten again by the
    // next chunk (and by the whole text at the end), so a paste that is still
    // queued would insert the wrong text.
    let (done, wait) = std::sync::mpsc::channel();
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = simulate_paste() {
            report(&handle, format!("paste failed: {e}"));
        }
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(std::time::Duration::from_secs(3));
    // The focused app reads the clipboard a moment after Ctrl+V; do not replace
    // it before it had the chance.
    std::thread::sleep(std::time::Duration::from_millis(150));
}

/// Lower-case letters and digits only, to compare words across two passes.
fn norm(word: &str) -> String {
    word.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Starts of multi-word voice commands. They are held back until the next
/// word shows whether a command follows.
const HOLD_BACK: &[&str] = &[
    "neue", "neuer", "new", "alles", "delete", "scratch", "clear", "lösche", "lösch", "undo",
    "question", "exclamation", "full",
];

/// What the streaming task has typed into the focused app so far.
struct StreamOutcome {
    typed: String,
    /// The (normalized) words of the current, unfinished segment that are
    /// already typed. New words are found by matching the end of this list
    /// against the next transcription, because Whisper may change or add a
    /// word earlier in the sentence and shift every position.
    typed_words: Vec<String>,
    /// Text of the segments before the current one, as context for Whisper.
    before: String,
}

/// Index in `current` right after the words that are already typed: the end
/// of `typed` is looked up in `current[..upto]`, preferring the latest and
/// longest match.
fn resume_index(typed: &[String], current: &[String], upto: usize) -> usize {
    if typed.is_empty() {
        return 0;
    }
    for k in (1..=typed.len().min(3)).rev() {
        let tail = &typed[typed.len() - k..];
        if upto >= k {
            if let Some(i) = (0..=upto - k).rev().find(|&i| current[i..i + k] == *tail) {
                return i + k;
            }
        }
    }
    // Could not align: assume nothing before the typed count has moved.
    typed.len().min(upto)
}

/// Types `words` into the focused app, with the voice commands applied.
fn type_words(app: &AppHandle, settings: &Settings, words: &[&str], typed: &mut String, finished: bool) {
    if words.is_empty() {
        return;
    }
    let raw = words.join(" ");
    let mut text = if settings.voice_commands {
        voxly_core::commands::apply_with(&raw, finished).text
    } else {
        raw
    };
    if text.is_empty() {
        return;
    }
    // Words are joined with a space, except after a line break or before
    // spoken punctuation, which already attached itself.
    let glue = !typed.is_empty()
        && !typed.ends_with('\n')
        && !text.starts_with(['\n', ',', '.', '!', '?', ':', ';']);
    if glue {
        text.insert(0, ' ');
    }
    eprintln!("stream: typing {text:?}");
    insert_live(app, &text, settings.auto_paste);
    typed.push_str(&text);
    let _ = app.emit("transcript", typed.as_str());
}

/// The last 200 characters of `text`.
fn context_of(text: &str) -> String {
    let skip = text.chars().count().saturating_sub(200);
    text.chars().skip(skip).collect()
}

/// Types the dictation word by word while the user is still speaking.
///
/// The audio of the current segment is transcribed again and again (about
/// every half second on a GPU). A word is typed as soon as two passes in a
/// row agree on it, because the end of a growing recording is still unstable.
/// Long segments are closed at a pause so that the passes stay short.
async fn stream_worker(
    app: AppHandle,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<StreamOutcome, String> {
    use std::sync::atomic::Ordering;
    use voxly_core::audio::{find_pause_cut, samples_to_wav, trim_silence};

    let mut out = StreamOutcome { typed: String::new(), typed_words: Vec::new(), before: String::new() };
    let mut seg_start = 0usize;
    let mut previous: Vec<String> = Vec::new();
    let mut last_len = 0usize;
    while !stop.load(Ordering::SeqCst) {
        let _ = tauri::async_runtime::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(100))
        })
        .await;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let state = app.state::<AppState>();
        let Some((len, rate)) = state.recorder.progress() else { break };
        // Wait for 300 ms of new audio before spending another pass on it.
        if len < last_len + rate as usize * 3 / 10 {
            continue;
        }
        let Some(region) = state.recorder.copy_range(seg_start, len) else { break };
        last_len = len;

        // Close a long segment at a pause; the words of this last pass are final.
        let cut = if region.len() >= rate as usize * 8 { find_pause_cut(&region, rate) } else { None };
        let audio = &region[..cut.unwrap_or(region.len())];
        let Ok(wav) = samples_to_wav(audio, rate) else { continue };
        let wav = match trim_silence(&wav) {
            Ok(Some(w)) => w,
            Ok(None) => {
                // Only silence so far; do not let it pile up.
                if region.len() >= rate as usize * 3 {
                    seg_start += region.len();
                    state.recorder.mark_processed(seg_start);
                    previous.clear();
                    out.typed_words.clear();
                }
                continue;
            }
            Err(_) => wav,
        };

        // Less than half a second of speech is mostly noise; Whisper would
        // make something up.
        if usage::wav_millis(&wav) < 500 {
            continue;
        }
        let settings = state.settings.lock().unwrap().clone();
        let raw = voxly_core::transcribe::transcribe_chunk(&settings, wav, &state.models_dir, &context_of(&out.before))
            .await
            .map_err(|e| format!("{e:#}"))?;
        let current: Vec<&str> = raw.split_whitespace().collect();
        let normalized: Vec<String> = current.iter().map(|w| norm(w)).collect();
        let finalize = cut.is_some();

        let mut upto = if finalize {
            current.len()
        } else {
            previous.iter().zip(&normalized).take_while(|(a, b)| a == b).count()
        };
        let from = resume_index(&out.typed_words, &normalized, upto);
        if !finalize && upto > from && HOLD_BACK.contains(&normalized[upto - 1].as_str()) {
            upto -= 1;
        }
        if upto > from {
            type_words(&app, &settings, &current[from..upto], &mut out.typed, false);
            out.typed_words.extend(normalized[from..upto].iter().cloned());
        }
        previous = normalized;

        if let Some(rel) = cut {
            seg_start += rel;
            state.recorder.mark_processed(seg_start);
            out.before = out.typed.clone();
            out.typed_words.clear();
            previous.clear();
        }
    }
    Ok(out)
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
        // Timed from here: what the user waits for after letting go of the key.
        let started = std::time::Instant::now();

        // Let the streaming task finish the chunk it is working on.
        let job = state.stream.lock().unwrap().take();
        let streamed = job.is_some();
        let chunks = match job {
            Some(j) => {
                j.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                j.handle.await.map_err(|e| e.to_string()).and_then(|r| r)
            }
            None => Ok(StreamOutcome { typed: String::new(), typed_words: Vec::new(), before: String::new() }),
        };
        let total_seconds = state.recorder.progress().map(|(n, r)| n as u64 / r.max(1) as u64);
        let wav = state.recorder.stop();
        let (wav, chunks) = match (wav, chunks) {
            (Ok(w), Ok(c)) => (w, c),
            (Err(e), _) => {
                *state.busy.lock().unwrap() = false;
                return report(&app, e);
            }
            (Ok(_), Err(e)) => {
                *state.busy.lock().unwrap() = false;
                return report(&app, e);
            }
        };
        let settings = state.settings.lock().unwrap().clone();
        let seconds = match (streamed, total_seconds) {
            (true, Some(t)) => t,
            _ => usage::wav_seconds(&wav),
        };

        let result: anyhow::Result<String> = if streamed {
            // Only the audio of the unfinished segment is left; its words that
            // are not typed yet come now.
            let mut out = chunks;
            let tail = match voxly_core::audio::trim_silence(&wav) {
                Ok(Some(w)) => Some(w),
                Ok(None) => None,
                Err(_) => Some(wav),
            };
            let mut failure = None;
            if let Some(w) = tail.filter(|w| usage::wav_millis(w) >= 500) {
                let context = context_of(&out.before);
                match voxly_core::transcribe::transcribe_chunk(&settings, w, &state.models_dir, &context).await {
                    Ok(raw) => {
                        let words: Vec<&str> = raw.split_whitespace().collect();
                        let normalized: Vec<String> = words.iter().map(|w| norm(w)).collect();
                        let from = resume_index(&out.typed_words, &normalized, words.len());
                        if words.len() > from {
                            type_words(&app, &settings, &words[from..], &mut out.typed, true);
                        }
                    }
                    Err(e) => failure = Some(e),
                }
            }
            flush_pending(&app);
            match failure {
                Some(e) => Err(e),
                None if out.typed.trim().is_empty() => Err(anyhow::anyhow!("No speech detected.")),
                None if settings.voice_commands && voxly_core::commands::apply(&out.typed).delete_last => {
                    // The whole dictation was "delete that": it is typed
                    // already, so remove it again.
                    delete_inserted(&app);
                    Err(anyhow::anyhow!("Deleted the last dictation."))
                }
                None => Ok(out.typed),
            }
        } else {
            let trimmed = if settings.trim_silence {
                match voxly_core::audio::trim_silence(&wav) {
                    Ok(Some(t)) => Some(t),
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!("could not trim silence: {e:#}");
                        Some(wav)
                    }
                }
            } else {
                Some(wav)
            };
            match trimmed {
                None => Err(anyhow::anyhow!("No speech detected.")),
                Some(wav) if settings.compare_mode => compare(&app, &settings, wav, seconds).await,
                Some(wav) => transcribe(&settings, wav, &state.models_dir).await,
            }
        };
        let elapsed = started.elapsed();
        *state.busy.lock().unwrap() = false;
        match result {
            Ok(text) => {
                let backend = if settings.backend == "local" {
                    if settings.use_gpu && local::GPU_BUILD { "local, GPU" } else { "local, CPU" }
                } else {
                    "cloud"
                };
                let backend = if streamed { format!("{backend}, streamed") } else { backend.to_owned() };
                eprintln!("transcribed {seconds}s of audio in {:.2}s ({backend})", elapsed.as_secs_f32());
                let _ = app.emit(
                    "timing",
                    serde_json::json!({ "ms": elapsed.as_millis() as u64, "audio_s": seconds, "backend": backend }),
                );
                // Streamed text already went through the voice commands and
                // into the focused app, one chunk at a time.
                let text = if settings.voice_commands && !streamed {
                    let outcome = voxly_core::commands::apply(&text);
                    if outcome.delete_last {
                        let _ = app.emit("transcript", "(deleted the last dictation)");
                        set_status(&app, Status::Idle);
                        return delete_inserted(&app);
                    }
                    outcome.text
                } else {
                    text
                };
                *state.last.lock().unwrap() = text.clone();
                let _ = app.emit("transcript", &text);
                set_status(&app, Status::Idle);
                if !text.is_empty() {
                    record_usage(&app, &text, seconds);
                    // Streamed: only keep the whole text on the clipboard.
                    output(&app, &text, settings.auto_paste && !streamed);
                }
            }
            Err(e) => report(&app, format!("{e:#}")),
        }
    });
}

struct Side {
    text: Result<String, String>,
    ms: u64,
}

async fn timed(settings: Settings, wav: Vec<u8>, dir: PathBuf) -> Side {
    let started = std::time::Instant::now();
    let text = transcribe(&settings, wav, &dir).await.map_err(|e| format!("{e:#}"));
    Side { text, ms: started.elapsed().as_millis() as u64 }
}

/// Compare mode: runs the local model and the cloud API on the same audio at
/// the same time and reports both to the UI. The configured backend's result
/// is the one that gets pasted; the other side failing only shows up in the
/// comparison.
async fn compare(
    app: &AppHandle,
    settings: &Settings,
    wav: Vec<u8>,
    seconds: u64,
) -> anyhow::Result<String> {
    let dir = app.state::<AppState>().models_dir.clone();
    let side = |backend: &str| {
        let mut s = settings.clone();
        s.backend = backend.into();
        tauri::async_runtime::spawn(timed(s, wav.clone(), dir.clone()))
    };
    let (local, cloud) = (side("local"), side("openai"));
    let join = |r: Result<Side, tauri::Error>| {
        r.unwrap_or_else(|e| Side { text: Err(e.to_string()), ms: 0 })
    };
    let (local, cloud) = (join(local.await), join(cloud.await));

    let similarity = match (&local.text, &cloud.text) {
        (Ok(a), Ok(b)) => Some(local::similarity(a, b)),
        _ => None,
    };
    let gpu = if settings.use_gpu && local::GPU_BUILD { "GPU" } else { "CPU" };
    let view = |s: &Side, label: String| {
        serde_json::json!({
            "label": label,
            "ms": s.ms,
            "text": s.text.as_ref().ok(),
            "error": s.text.as_ref().err(),
        })
    };
    let payload = serde_json::json!({
        "audio_s": seconds,
        "similarity": similarity,
        "local": view(&local, format!("Local · {} · {gpu}", settings.local_model)),
        "cloud": view(&cloud, format!("Cloud · {}", settings.model)),
    });
    eprintln!(
        "compare: local {} ms, cloud {} ms, similarity {:?}",
        local.ms, cloud.ms, similarity
    );
    let _ = app.emit("compare", payload);

    let primary = if settings.backend == "local" { local } else { cloud };
    primary.text.map_err(|e| anyhow::anyhow!(e))
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

/// Puts `text` on the clipboard. On Wayland the clipboard plugin only reaches
/// X11 apps, so the text is offered through the Wayland data-control protocol
/// instead, and `wl-copy` is the last fallback.
fn set_clipboard(app: &AppHandle, text: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if is_wayland() {
        if wayland_clipboard(text).is_ok() {
            return Ok(());
        }
        if wayland_copy(text).is_ok() {
            return Ok(());
        }
    }
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

/// Serves `text` as the Wayland selection from a background thread. The
/// thread ends when something else is copied. An immediate error means the
/// compositor does not offer the data-control protocol.
#[cfg(target_os = "linux")]
fn wayland_clipboard(text: &str) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    let data: Box<[u8]> = text.as_bytes().into();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut options = Options::new();
        options.foreground(true);
        let _ = tx.send(options.copy(Source::Bytes(data), MimeType::Text));
    });
    match rx.recv_timeout(std::time::Duration::from_millis(120)) {
        Ok(Err(e)) => Err(e.to_string()),
        // Still serving the selection (or finished): it works.
        _ => Ok(()),
    }
}

/// Puts the text on the clipboard and, if asked, types Cmd/Ctrl+V into the
/// focused application.
fn output(app: &AppHandle, text: &str, paste: bool) {
    if let Err(e) = set_clipboard(app, text) {
        return report(app, format!("clipboard: {e}"));
    }
    #[cfg(desktop)]
    if paste {
        *app.state::<AppState>().inserted.lock().unwrap() = text.chars().count();
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

/// "Delete that": removes the text inserted last by sending Backspace.
fn delete_inserted(app: &AppHandle) {
    let count = std::mem::take(&mut *app.state::<AppState>().inserted.lock().unwrap());
    if count == 0 {
        return;
    }
    #[cfg(desktop)]
    {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            use enigo::{Direction, Enigo, Key, Keyboard, Settings};
            #[cfg(target_os = "linux")]
            if crate::vkeys::tap(&[], evdev::KeyCode::KEY_BACKSPACE, count.min(5000)).is_ok() {
                return;
            }
            let result = Enigo::new(&Settings::default())
                .map_err(|e| e.to_string())
                .and_then(|mut e| {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    (0..count.min(5000))
                        .try_for_each(|_| e.key(Key::Backspace, Direction::Click))
                        .map_err(|e| e.to_string())
                });
            if let Err(e) = result {
                report(&handle, format!("could not delete the text: {e}"));
            }
        });
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Fallback for compositors without the data-control protocol: `wl-copy`
/// forks into the background and keeps serving the clipboard.
#[cfg(target_os = "linux")]
fn wayland_copy(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("wl-copy: {status}"))
}

/// Last resort when the built-in virtual keyboard is refused (GNOME has no
/// such protocol): wtype, then ydotool, if the user has them installed.
#[cfg(target_os = "linux")]
fn external_paste(shift: bool) -> Result<(), String> {
    use std::process::Command;
    let mut wtype = Command::new("wtype");
    wtype.args(["-M", "ctrl"]);
    if shift {
        wtype.args(["-M", "shift"]);
    }
    wtype.args(["-k", "v"]);
    if shift {
        wtype.args(["-m", "shift"]);
    }
    wtype.args(["-m", "ctrl"]);
    if wtype.status().is_ok_and(|s| s.success()) {
        return Ok(());
    }
    // ydotool uses raw keycodes: ctrl=29, shift=42, v=47
    let seq: &[&str] = if shift {
        &["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
    } else {
        &["key", "29:1", "47:1", "47:0", "29:0"]
    };
    if Command::new("ydotool").args(seq).status().is_ok_and(|s| s.success()) {
        Ok(())
    } else {
        Err("this compositor does not allow simulated keys; paste manually with Ctrl+V".into())
    }
}

/// Terminals paste with Ctrl+Shift+V. Only Hyprland can tell us which window
/// has focus; elsewhere plain Ctrl+V is sent.
#[cfg(target_os = "linux")]
fn focused_is_terminal() -> bool {
    let Ok(out) = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
    else {
        return false;
    };
    let json = String::from_utf8_lossy(&out.stdout).to_lowercase();
    const TERMS: &[&str] = &[
        "kitty", "alacritty", "foot", "wezterm", "ghostty", "konsole", "gnome-terminal",
        "xterm", "terminator", "tilix", "st-256color", "urxvt",
    ];
    json.lines()
        .find(|l| l.contains("\"class\""))
        .is_some_and(|l| TERMS.iter().any(|t| l.contains(t)))
}

#[cfg(desktop)]
fn simulate_paste() -> Result<(), String> {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    #[cfg(target_os = "macos")]
    let modifier = Key::Meta;
    #[cfg(not(target_os = "macos"))]
    let modifier = Key::Control;
    #[cfg(target_os = "linux")]
    let shift = focused_is_terminal();
    #[cfg(not(target_os = "linux"))]
    let shift = false;

    // Give the user a moment to release the hotkey modifiers.
    std::thread::sleep(std::time::Duration::from_millis(80));
    // A uinput keyboard works on X11 and on every Wayland compositor.
    #[cfg(target_os = "linux")]
    {
        use evdev::KeyCode;
        let mods: &[KeyCode] = if shift {
            &[KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT]
        } else {
            &[KeyCode::KEY_LEFTCTRL]
        };
        if crate::vkeys::tap(mods, KeyCode::KEY_V, 1).is_ok() {
            return Ok(());
        }
    }
    let result = (|| {
        let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
        enigo.key(modifier, Direction::Press).map_err(|e| e.to_string())?;
        if shift {
            enigo.key(Key::Shift, Direction::Press).map_err(|e| e.to_string())?;
        }
        let click = enigo.key(Key::Unicode('v'), Direction::Click);
        if shift {
            let _ = enigo.key(Key::Shift, Direction::Release);
        }
        let release = enigo.key(modifier, Direction::Release);
        click.and(release).map_err(|e| e.to_string())
    })();
    #[cfg(target_os = "linux")]
    if result.is_err() && is_wayland() {
        return external_paste(shift);
    }
    result
}

#[cfg(target_os = "linux")]
pub(crate) fn hotkey_event(app: &AppHandle, action: crate::evdev_hotkeys::Action, pressed: bool) {
    use crate::evdev_hotkeys::Action;
    match (action, pressed) {
        (Action::Hold, true) => start_recording(app),
        (Action::Hold, false) => stop_recording(app),
        (Action::Toggle, true) => toggle_recording(app),
        (Action::PasteLast, true) => paste_last(app),
        _ => {}
    }
}

#[cfg(unix)]
pub(crate) fn handle_command(app: &AppHandle, cmd: &str) {
    match cmd {
        "start" => start_recording(app),
        "stop" => stop_recording(app),
        "toggle" => toggle_recording(app),
        "paste-last" => paste_last(app),
        "show" => show_main(app),
        _ => {}
    }
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
const OVERLAY_W: f64 = 220.0;
#[cfg(desktop)]
const OVERLAY_H: f64 = 56.0;
/// Distance from the screen edges, in logical pixels.
#[cfg(desktop)]
const EDGE_GAP: f64 = 44.0;

/// Splits `top`, `bottom-left`, ... into (bottom?, horizontal) where
/// horizontal is -1 (left), 0 (center) or 1 (right).
#[cfg(desktop)]
fn parse_overlay_position(pos: &str) -> (bool, i32) {
    let h = if pos.ends_with("left") {
        -1
    } else if pos.ends_with("right") {
        1
    } else {
        0
    };
    (pos.starts_with("bottom"), h)
}

/// Moves the overlay to the configured spot. Must run on the main thread.
#[cfg(desktop)]
fn apply_overlay_position(app: &AppHandle) {
    let Some(win) = app.get_webview_window("overlay") else {
        return;
    };
    let pos = app.state::<AppState>().settings.lock().unwrap().overlay_position.clone();
    let (bottom, horiz) = parse_overlay_position(&pos);

    #[cfg(target_os = "linux")]
    if is_wayland() {
        use gtk_layer_shell::{Edge, LayerShell};
        if let Ok(gw) = win.gtk_window() {
            gw.set_anchor(Edge::Top, !bottom);
            gw.set_anchor(Edge::Bottom, bottom);
            gw.set_anchor(Edge::Left, horiz < 0);
            gw.set_anchor(Edge::Right, horiz > 0);
            gw.set_layer_shell_margin(Edge::Top, if bottom { 0 } else { EDGE_GAP as i32 });
            gw.set_layer_shell_margin(Edge::Bottom, if bottom { EDGE_GAP as i32 } else { 0 });
            gw.set_layer_shell_margin(Edge::Left, EDGE_GAP as i32);
            gw.set_layer_shell_margin(Edge::Right, EDGE_GAP as i32);
        }
        return;
    }

    let Ok(Some(mon)) = win.primary_monitor() else {
        return;
    };
    let scale = mon.scale_factor();
    let (mx, my) = (mon.position().x as f64 / scale, mon.position().y as f64 / scale);
    let (mw, mh) = (mon.size().width as f64 / scale, mon.size().height as f64 / scale);
    let x = match horiz {
        -1 => mx + EDGE_GAP,
        1 => mx + mw - OVERLAY_W - EDGE_GAP,
        _ => mx + (mw - OVERLAY_W) / 2.0,
    };
    let y = if bottom { my + mh - OVERLAY_H - EDGE_GAP } else { my + EDGE_GAP };
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

/// A transparent, click-through pill at the edge of the screen. It stays open
/// and only fades in and out (see ui/overlay.html), so showing it never takes
/// focus away from the app being dictated into.
#[cfg(desktop)]
fn create_overlay(app: &AppHandle) -> tauri::Result<()> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let win = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("Voxly overlay")
        .inner_size(OVERLAY_W, OVERLAY_H)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible_on_all_workspaces(true)
        .visible(false)
        .build()?;
    #[cfg(target_os = "linux")]
    if is_wayland() {
        // Wayland ignores window positions and tiles/decorates normal windows,
        // so the pill is a layer-shell surface instead. It is shown only while
        // recording or transcribing (see set_status).
        layer_overlay(&win, OVERLAY_W as i32, OVERLAY_H as i32)?;
        apply_overlay_position(app);
        return Ok(());
    }
    win.set_ignore_cursor_events(true)?;
    raise_above_everything(&win);
    win.show()?;
    apply_overlay_position(app);
    Ok(())
}

#[cfg(target_os = "linux")]
fn layer_overlay(win: &tauri::WebviewWindow, w: i32, h: i32) -> tauri::Result<()> {
    use gtk::prelude::*;
    use gtk_layer_shell::{Layer, LayerShell};

    let gw = win.gtk_window()?;
    gw.init_layer_shell();
    gw.set_namespace("voxly-overlay");
    gw.set_layer(Layer::Overlay);
    gw.set_keyboard_interactivity(false);
    gw.set_exclusive_zone(-1);
    gw.set_size_request(w, h);
    gw.set_decorated(false);
    Ok(())
}

#[cfg(desktop)]
fn register_hotkeys(app: &AppHandle) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

    // X11 key grabs do not work on Wayland; read the keyboards directly.
    #[cfg(target_os = "linux")]
    if is_wayland() {
        use crate::evdev_hotkeys::{configure, Action};
        let hk = app.state::<AppState>().settings.lock().unwrap().hotkeys.clone();
        return configure(
            app,
            [
                (Action::Hold, &hk.hold),
                (Action::Toggle, &hk.toggle),
                (Action::PasteLast, &hk.paste_last),
            ],
        );
    }

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

/// Loads the local model in the background (see `local::preload`).
fn preload_model(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let dir = state.models_dir.clone();
    std::thread::spawn(move || local::preload(&settings, &dir));
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Result<(), String> {
    settings.save(&state.settings_path).map_err(|e| e.to_string())?;
    *state.settings.lock().unwrap() = settings;
    preload_model(&app);
    #[cfg(desktop)]
    {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || apply_overlay_position(&handle));
        register_hotkeys(&app)?;
    }
    #[cfg(not(desktop))]
    let _ = app;
    Ok(())
}

/// Wayland only: whether the global hotkeys can work yet.
#[tauri::command]
fn hotkey_access() -> &'static str {
    #[cfg(target_os = "linux")]
    if is_wayland() {
        return if crate::evdev_hotkeys::has_access() { "ok" } else { "denied" };
    }
    "ok"
}

#[tauri::command]
async fn grant_hotkey_access(app: AppHandle) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        tauri::async_runtime::spawn_blocking(crate::evdev_hotkeys::grant_access)
            .await
            .map_err(|e| e.to_string())??;
        #[allow(clippy::needless_return)]
        return register_hotkeys(&app);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        Ok(())
    }
}

#[tauri::command]
fn list_input_devices() -> Vec<voxly_core::audio::InputDevice> {
    voxly_core::audio::input_devices()
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

/// Records a few seconds of speech, then times the local transcription
/// settings on it and keeps the fastest one that gives the same text.
#[tauri::command]
async fn tune_speed(app: AppHandle) -> Result<local::TuneReport, String> {
    const SAMPLE: std::time::Duration = std::time::Duration::from_secs(5);
    {
        let state = app.state::<AppState>();
        if state.settings.lock().unwrap().backend != "local" {
            return Err("Switch the backend to local transcription first.".into());
        }
        let mut busy = state.busy.lock().unwrap();
        if *busy || state.recorder.is_recording() {
            return Err("Voxly is busy, try again in a moment.".into());
        }
        *busy = true;
    }
    let result = tune_speed_inner(&app, SAMPLE).await;
    *app.state::<AppState>().busy.lock().unwrap() = false;
    set_status(&app, Status::Idle);
    result
}

async fn tune_speed_inner(
    app: &AppHandle,
    sample: std::time::Duration,
) -> Result<local::TuneReport, String> {
    let state = app.state::<AppState>();
    let device = state.settings.lock().unwrap().input_device.clone();
    state.recorder.start(&device).map_err(|e| format!("{e:#}"))?;
    set_status(app, Status::Recording);
    let _ = app.emit("tune", "Speak now for 5 seconds…");
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(sample)).await;
    let wav = state.recorder.stop().map_err(|e| format!("{e:#}"))?;
    set_status(app, Status::Transcribing);
    let _ = app.emit("tune", "Testing settings…");

    let settings = state.settings.lock().unwrap().clone();
    let dir = state.models_dir.clone();
    let emitter = app.clone();
    let report = tauri::async_runtime::spawn_blocking(move || {
        local::benchmark(&settings, &wav, &dir, |t| {
            let _ = emitter.emit("tune_trial", t.clone());
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))?;

    let updated = {
        let mut s = state.settings.lock().unwrap();
        s.use_gpu = report.use_gpu;
        s.flash_attn = report.flash_attn;
        s.beam_size = report.beam_size;
        s.threads = report.threads;
        s.clone()
    };
    updated.save(&state.settings_path).map_err(|e| format!("{e:#}"))?;
    preload_model(app);
    Ok(report)
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
                inserted: Mutex::new(0),
                stream: Mutex::new(None),
                pending: Mutex::new(String::new()),
                busy: Mutex::new(false),
            });
            preload_model(app.handle());
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
            #[cfg(unix)]
            crate::ipc::spawn_server(app.handle());
            #[cfg(target_os = "linux")]
            std::thread::spawn(crate::vkeys::init);
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
            list_input_devices,
            hotkey_access,
            grant_hotkey_access,
            save_settings,
            list_models,
            download_model,
            toggle,
            tune_speed,
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

#[cfg(test)]
mod tests {
    use super::resume_index;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn continues_after_the_typed_words() {
        let cur = v("das ist ein test mit mehr worten");
        assert_eq!(resume_index(&v("das ist ein"), &cur, cur.len()), 3);
        assert_eq!(resume_index(&[], &cur, cur.len()), 0);
    }

    #[test]
    fn survives_a_word_inserted_before_the_typed_end() {
        // "jetzt" was typed, then Whisper added "bereits" before it.
        let cur = v("du dir bereits jetzt so ein bisschen");
        assert_eq!(resume_index(&v("du dir jetzt"), &cur, cur.len()), 4);
    }

    #[test]
    fn falls_back_to_the_count_when_nothing_matches() {
        let cur = v("ganz anderer text jetzt");
        assert_eq!(resume_index(&v("gar nicht"), &cur, 4), 2);
    }
}
