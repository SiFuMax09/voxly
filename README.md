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

## GPU acceleration

Local transcription runs on the GPU by default (setting "Use the GPU when available"), and falls back to the CPU if none is usable.

The GPU backend is part of every build (also plain `cargo run`), so there is nothing to enable:

| Platform | Backend | Build requirements |
| --- | --- | --- |
| macOS, Apple Silicon | Metal | none |
| Windows / Linux, AMD + NVIDIA + Intel | Vulkan | Vulkan SDK (headers and `glslc`); Arch: `vulkan-headers shaderc` |

Optional vendor backends instead of Vulkan: `cuda` (NVIDIA, needs the CUDA toolkit) or `hip` (AMD, needs ROCm and `AMDGPU_TARGETS=gfx1030`, or your card's id from `rocminfo`). They are added on top of Vulkan.

The release installers for Windows and Linux are built with Vulkan, so they need a Vulkan driver, which ships with the normal AMD/NVIDIA/Intel GPU drivers.

## Platform notes

- **macOS**: grant Microphone and Accessibility (needed to simulate Cmd+V) permission.
- **Linux (X11)**: global hotkeys and simulated paste work out of the box.
- **Linux (Wayland, e.g. Hyprland)**: everything is built in, nothing to install. Voxly reads the keyboards for its global hotkeys; on first use, Settings shows "Enable global hotkeys", which installs a small udev rule after a one-time administrator password prompt (no group change, no re-login). The clipboard and Ctrl+V paste use the Wayland protocols directly (Ctrl+Shift+V in terminals on Hyprland); on compositors that refuse simulated keys, such as GNOME, `wl-copy` and `wtype`/`ydotool` are used if installed, otherwise paste manually. Optionally, `voxly start|stop|toggle|paste-last|show` controls the running app from compositor binds. Wayland compositors ignore window positions, so to keep the overlay pill from stealing focus on Hyprland add `windowrule = no_focus on, match:title ^Voxly overlay$`.
- **iOS / Android**: no global hotkeys or system-wide paste from a regular app. You record in the app and the result goes to the clipboard. A system keyboard (IME) is the route to true system-wide dictation and is not built yet.

## License

MIT

## Releases

Installers for macOS (Apple Silicon and Intel), Windows and Linux are built by GitHub Actions. Push a tag to create a draft release:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

macOS builds are signed and notarized when these repository secrets are set: `APPLE_CERTIFICATE` (base64 of the Developer ID Application `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific password) and `APPLE_TEAM_ID`. Without them the macOS build is unsigned. Windows builds are not signed yet.
