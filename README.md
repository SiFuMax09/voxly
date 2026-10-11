<p align="center"><img src="logo.svg" width="96" alt="Voxly logo"></p>

# Voxly

Open-source dictation for coding. Speak, and the text lands in your editor, with your technical vocabulary spelled correctly. Transcription runs either **locally** (whisper.cpp, no account, no key, audio never leaves your machine) or through OpenAI Whisper with **your own API key**.

Written in Rust with [Tauri 2](https://tauri.app): one codebase for macOS, Linux, Windows, iOS and Android.

## Features

- Local backend: pick a model (Base / Small / Large v3 Turbo) and download it in the app
- Or cloud backend with your own OpenAI API key (stored locally in `settings.json`, mode 0600 on Unix)
- Vocabulary: terms are sent as a Whisper prompt, and "sounds like" phrases are rewritten afterwards (`use state` → `useState`)
- Global hotkeys (desktop): hold to record, toggle recording, paste last recording
- Auto-paste into the focused app
- Models: `whisper-1`, `gpt-4o-transcribe`, `gpt-4o-mini-transcribe`

## Layout

- `crates/core`: settings, vocabulary, microphone capture (cpal), OpenAI client. No UI dependencies.
- `src-tauri`: the Tauri app (hotkeys, clipboard, paste, commands)
- `ui`: plain HTML/JS frontend, no bundler

## Run

Building needs `cmake` and a C++ compiler (for whisper.cpp). Local transcription is desktop-only for now.

```sh
cargo install tauri-cli --version "^2"
cargo tauri dev          # desktop
cargo tauri build        # installers
cargo test --workspace
```

Mobile (needs Xcode / Android Studio + NDK):

```sh
cargo tauri ios init && cargo tauri ios dev
cargo tauri android init && cargo tauri android dev
```

## Platform notes

- **macOS**: grant Microphone and Accessibility (needed to simulate Cmd+V) permission.
- **Linux**: global hotkeys and simulated paste work on X11. Wayland restricts both; copy-from-clipboard still works.
- **iOS / Android**: no global hotkeys or system-wide paste from a regular app. You record in the app and the result goes to the clipboard. A system keyboard (IME) is the route to true system-wide dictation and is not built yet.

## License

MIT

## Releases

Installers for macOS (Apple Silicon and Intel), Windows and Linux are built by GitHub Actions. Push a tag to create a draft release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

macOS builds are signed and notarized when these repository secrets are set: `APPLE_CERTIFICATE` (base64 of the Developer ID Application `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific password) and `APPLE_TEAM_ID`. Without them the macOS build is unsigned. Windows builds are not signed yet.
