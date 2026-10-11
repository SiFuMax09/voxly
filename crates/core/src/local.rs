//! On-device transcription with whisper.cpp (feature `local`) and model
//! management. Without the feature, listing models still works but
//! transcribing returns an error.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

use crate::settings::Settings;

pub struct ModelInfo {
    pub id: &'static str,
    pub file: &'static str,
    pub label: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo { id: "base", file: "ggml-base.bin", label: "Base (142 MB, fast, ok accuracy)" },
    ModelInfo { id: "small", file: "ggml-small.bin", label: "Small (466 MB, good)" },
    ModelInfo {
        id: "large-v3-turbo",
        file: "ggml-large-v3-turbo-q5_0.bin",
        label: "Large v3 Turbo, quantized (574 MB, best)",
    },
];

#[derive(Serialize)]
pub struct ModelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub downloaded: bool,
}

pub fn model_path(models_dir: &Path, id: &str) -> Result<PathBuf> {
    let m = MODELS
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow!("unknown model '{id}'"))?;
    Ok(models_dir.join(m.file))
}

pub fn list_models(models_dir: &Path) -> Vec<ModelStatus> {
    MODELS
        .iter()
        .map(|m| ModelStatus {
            id: m.id,
            label: m.label,
            downloaded: models_dir.join(m.file).exists(),
        })
        .collect()
}

/// Downloads a model from the whisper.cpp repository on Hugging Face.
/// `progress` receives (bytes_done, bytes_total).
pub async fn download_model(
    models_dir: &Path,
    id: &str,
    progress: impl Fn(u64, u64),
) -> Result<()> {
    let dest = model_path(models_dir, id)?;
    let file = dest.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(models_dir)?;

    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{file}");
    let mut res = reqwest::get(&url).await?.error_for_status()?;
    let total = res.content_length().unwrap_or(0);

    // Write to a temp file so an interrupted download is never mistaken for a model.
    let tmp = dest.with_extension("part");
    let mut out = std::fs::File::create(&tmp)?;
    let mut done = 0u64;
    {
        use std::io::Write;
        while let Some(chunk) = res.chunk().await? {
            out.write_all(&chunk)?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        out.flush()?;
    }
    if total > 0 && done != total {
        bail!("download incomplete ({done} of {total} bytes)");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Converts the 16-bit WAV we record into the f32 samples whisper.cpp wants.
#[cfg_attr(not(feature = "local"), allow(dead_code))]
fn wav_to_f32(wav: &[u8]) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(wav))?;
    reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32).map_err(Into::into))
        .collect()
}

pub async fn transcribe(settings: &Settings, wav: Vec<u8>, models_dir: &Path) -> Result<String> {
    let path = model_path(models_dir, &settings.local_model)?;
    if !path.exists() {
        bail!("The local model '{}' is not downloaded yet. Download it in the settings.", settings.local_model);
    }
    let settings = settings.clone();
    tokio::task::spawn_blocking(move || run(&settings, &wav, &path))
        .await
        .map_err(|e| anyhow!("transcription thread died: {e}"))?
}

/// True when this build has a GPU backend compiled in.
pub const GPU_BUILD: bool = cfg!(any(
    feature = "hip",
    feature = "vulkan",
    feature = "cuda",
    all(target_os = "macos", target_arch = "aarch64")
));

/// Beam search is only affordable on a GPU; on the CPU greedy decoding is
/// several times faster for short dictation.
#[cfg_attr(not(feature = "local"), allow(dead_code))]
fn effective_beam(s: &Settings) -> u32 {
    match s.beam_size {
        0 if GPU_BUILD && s.use_gpu => 5,
        0 => 1,
        n => n,
    }
}

#[cfg_attr(not(feature = "local"), allow(dead_code))]
fn effective_threads(s: &Settings) -> u32 {
    match s.threads {
        0 => std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as u32,
        n => n,
    }
}

/// A loaded model plus its decoding state. Creating the state allocates the
/// compute buffers (and, on Vulkan, compiles shaders), so it is reused too.
#[cfg(feature = "local")]
struct Engine {
    state: std::sync::Mutex<whisper_rs::WhisperState>,
}

/// Loading a model takes seconds (more with a GPU), so keep the last one.
#[cfg(feature = "local")]
fn engine(model: &Path, use_gpu: bool, flash_attn: bool) -> Result<std::sync::Arc<Engine>> {
    use std::sync::{Arc, Mutex, OnceLock};
    use whisper_rs::{WhisperContext, WhisperContextParameters};

    static CACHE: OnceLock<Mutex<Option<(PathBuf, (bool, bool), Arc<Engine>)>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if let Some((p, key, e)) = &*cache {
        if p == model && *key == (use_gpu, flash_attn) {
            return Ok(e.clone());
        }
    }
    let mut params = WhisperContextParameters::default();
    params.use_gpu(use_gpu);
    params.flash_attn(use_gpu && flash_attn);
    let ctx = WhisperContext::new_with_params(
        model.to_str().ok_or_else(|| anyhow!("non-UTF-8 model path"))?,
        params,
    )
    .map_err(|e| anyhow!("could not load model: {e}"))?;
    let state = ctx.create_state().map_err(|e| anyhow!("{e}"))?;
    let e = Arc::new(Engine { state: Mutex::new(state) });
    *cache = Some((model.to_path_buf(), (use_gpu, flash_attn), e.clone()));
    Ok(e)
}

/// Loads the configured local model ahead of the first recording so that
/// dictation does not wait for it. Blocking; call from a background thread.
pub fn preload(settings: &Settings, models_dir: &Path) {
    #[cfg(feature = "local")]
    if settings.backend == "local" {
        if let Ok(path) = model_path(models_dir, &settings.local_model) {
            if path.exists() {
                if let Err(e) = engine(&path, settings.use_gpu, settings.flash_attn) {
                    eprintln!("could not preload model: {e:#}");
                }
            }
        }
    }
    #[cfg(not(feature = "local"))]
    let _ = (settings, models_dir);
}

#[cfg(feature = "local")]
fn run(settings: &Settings, wav: &[u8], model: &Path) -> Result<String> {
    use whisper_rs::{FullParams, SamplingStrategy};

    let engine = engine(model, settings.use_gpu, settings.flash_attn)?;

    let samples = wav_to_f32(wav)?;
    let mut state = engine.state.lock().unwrap();
    let beam = effective_beam(settings);
    let strategy = if beam > 1 {
        SamplingStrategy::BeamSearch { beam_size: beam as i32, patience: -1.0 }
    } else {
        SamplingStrategy::Greedy { best_of: 1 }
    };
    let mut params = FullParams::new(strategy);
    params.set_n_threads(effective_threads(settings) as i32);
    let lang = settings.language.trim();
    params.set_language(if lang.is_empty() { None } else { Some(lang) });
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    let prompt = crate::vocabulary::build_prompt(&settings.vocabulary);
    if let Some(p) = &prompt {
        params.set_initial_prompt(p);
    }
    state.full(params, &samples).map_err(|e| anyhow!("transcription failed: {e}"))?;

    let mut text = String::new();
    for seg in state.as_iter() {
        text.push_str(&seg.to_str().map_err(|e| anyhow!("{e}"))?);
    }
    Ok(text.trim().to_owned())
}

#[derive(Serialize, Clone)]
pub struct Trial {
    pub label: String,
    pub ms: u64,
    /// Same words as the reference run, so the speedup costs no accuracy.
    pub similar: bool,
    pub text: String,
}

#[derive(Serialize)]
pub struct TuneReport {
    pub best: String,
    pub trials: Vec<Trial>,
    pub use_gpu: bool,
    pub flash_attn: bool,
    pub beam_size: u32,
    pub threads: u32,
}

fn words(t: &str) -> Vec<String> {
    t.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 1.0 for identical word sequences, lower the more words differ.
fn similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (words(a), words(b));
    let n = a.len().max(b.len());
    if n == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f32 / n as f32
}

/// Times every sensible combination of GPU, flash attention, decoding width
/// and thread count on `wav` and returns the fastest one whose text matches
/// the reference run. `progress` is called after each trial.
pub fn benchmark(
    settings: &Settings,
    wav: &[u8],
    models_dir: &Path,
    progress: impl Fn(&Trial),
) -> Result<TuneReport> {
    let path = model_path(models_dir, &settings.local_model)?;
    if !path.exists() {
        bail!("The local model '{}' is not downloaded yet.", settings.local_model);
    }
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get() as u32);

    // (use_gpu, flash_attn, beam_size, threads)
    let mut candidates: Vec<(bool, bool, u32, u32)> = Vec::new();
    if GPU_BUILD {
        for flash in [true, false] {
            for beam in [5, 1] {
                candidates.push((true, flash, beam, 4));
            }
        }
    }
    let mut threads = vec![4, (cores / 2).max(4), cores.min(16)];
    threads.sort_unstable();
    threads.dedup();
    if !GPU_BUILD {
        candidates.push((false, false, 5, cores.min(8)));
    }
    for t in threads {
        candidates.push((false, false, 1, t));
    }

    let mut trials: Vec<(Trial, (bool, bool, u32, u32))> = Vec::new();
    let mut reference = String::new();
    for (i, c) in candidates.iter().copied().enumerate() {
        let (gpu, flash, beam, threads) = c;
        let mut s = settings.clone();
        (s.use_gpu, s.flash_attn, s.beam_size, s.threads) = (gpu, flash, beam, threads);
        let label = if gpu {
            format!("GPU, {}, {}", if flash { "flash attention" } else { "no flash attention" },
                if beam > 1 { "beam search" } else { "greedy" })
        } else {
            format!("CPU, {threads} threads, {}", if beam > 1 { "beam search" } else { "greedy" })
        };
        // The first run loads the model and warms the GPU up, so it is not timed.
        let mut text = run(&s, wav, &path)?;
        let runs = if gpu { 2 } else { 1 };
        let mut best = u128::MAX;
        for _ in 0..runs {
            let t = std::time::Instant::now();
            text = run(&s, wav, &path)?;
            best = best.min(t.elapsed().as_millis());
        }
        if i == 0 {
            reference = text.clone();
            if reference.trim().is_empty() {
                bail!("No speech was recognized in the sample. Speak clearly during the 5 seconds and try again.");
            }
        }
        let trial = Trial { label, ms: best as u64, similar: similarity(&reference, &text) >= 0.9, text };
        progress(&trial);
        trials.push((trial, c));
    }
    let (best, (gpu, flash, beam, threads)) = trials
        .iter()
        .filter(|(t, _)| t.similar)
        .min_by_key(|(t, _)| t.ms)
        .map(|(t, c)| (t.label.clone(), *c))
        .ok_or_else(|| anyhow!("no setting produced usable text"))?;
    Ok(TuneReport {
        best,
        trials: trials.into_iter().map(|(t, _)| t).collect(),
        use_gpu: gpu,
        flash_attn: flash,
        beam_size: beam,
        threads,
    })
}

#[cfg(not(feature = "local"))]
fn run(_: &Settings, _: &[u8], _: &Path) -> Result<String> {
    Err(anyhow!("this build does not include local transcription"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_an_error() {
        assert!(model_path(Path::new("/tmp"), "nope").is_err());
    }

    #[test]
    fn wav_roundtrip_length() {
        let mut buf = std::io::Cursor::new(Vec::new());
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for _ in 0..100 { w.write_sample(0i16).unwrap(); }
        w.finalize().unwrap();
        assert_eq!(wav_to_f32(&buf.into_inner()).unwrap().len(), 100);
    }
}
