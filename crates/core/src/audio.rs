use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

/// Whisper downsamples to 16 kHz internally, so we send that directly.
const TARGET_RATE: u32 = 16_000;

struct Active {
    stop: Sender<()>,
    handle: JoinHandle<Vec<u8>>,
    /// Everything recorded so far, at the microphone's own sample rate.
    samples: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    /// Samples before this index were already handed out as streamed chunks;
    /// `stop` returns only what comes after.
    from: Arc<AtomicUsize>,
}

/// Records from the default microphone. The cpal stream is not `Send`, so it
/// lives on its own thread for the duration of a recording.
#[derive(Default)]
pub struct Recorder {
    active: Mutex<Option<Active>>,
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// `device` is a name from [`input_devices`]; empty uses the default.
    pub fn start(&self, device: &str) -> Result<()> {
        let mut slot = self.active.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
        let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
        let from = Arc::new(AtomicUsize::new(0));

        let device = device.to_owned();
        let (thread_samples, thread_from) = (samples.clone(), from.clone());
        let handle = std::thread::spawn(move || {
            match open_stream(thread_samples.clone(), &device) {
                Ok((stream, rate)) => {
                    let _ = ready_tx.send(Ok(rate));
                    let _ = stop_rx.recv();
                    drop(stream);
                    let data = std::mem::take(&mut *thread_samples.lock().unwrap());
                    let start = thread_from.load(Ordering::SeqCst).min(data.len());
                    encode_wav(&data[start..], rate).unwrap_or_default()
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    Vec::new()
                }
            }
        });

        let rate = ready_rx.recv().context("audio thread died")??;
        *slot = Some(Active { stop: stop_tx, handle, samples, rate, from });
        Ok(())
    }

    /// Number of samples recorded so far and the sample rate, while recording.
    pub fn progress(&self) -> Option<(usize, u32)> {
        let slot = self.active.lock().unwrap();
        slot.as_ref().map(|a| (a.samples.lock().unwrap().len(), a.rate))
    }

    /// A copy of the samples in `from..to`, while recording.
    pub fn copy_range(&self, from: usize, to: usize) -> Option<Vec<f32>> {
        let slot = self.active.lock().unwrap();
        let a = slot.as_ref()?;
        let data = a.samples.lock().unwrap();
        let to = to.min(data.len());
        Some(data[from.min(to)..to].to_vec())
    }

    /// Declares everything before `upto` as handled, so `stop` returns only
    /// the audio after it (used by streaming transcription).
    pub fn mark_processed(&self, upto: usize) {
        if let Some(a) = self.active.lock().unwrap().as_ref() {
            a.from.store(upto, Ordering::SeqCst);
        }
    }

    /// Stops recording and returns 16 kHz mono WAV bytes.
    pub fn stop(&self) -> Result<Vec<u8>> {
        let active = self
            .active
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("not recording"))?;
        let _ = active.stop.send(());
        let wav = active.handle.join().map_err(|_| anyhow!("audio thread panicked"))?;
        if wav.is_empty() {
            return Err(anyhow!("no audio captured"));
        }
        Ok(wav)
    }
}

/// A selectable microphone: `id` is stored in the settings, `label` is shown.
#[derive(serde::Serialize, Clone)]
pub struct InputDevice {
    pub id: String,
    pub label: String,
}

/// The available microphones. On Linux these come from PipeWire/PulseAudio,
/// like in other apps, instead of the raw ALSA devices (hw:, plughw:, ...).
pub fn input_devices() -> Vec<InputDevice> {
    #[cfg(target_os = "linux")]
    {
        let sources = pulse_sources();
        if !sources.is_empty() {
            return sources;
        }
    }
    cpal::default_host()
        .input_devices()
        .map(|d| {
            d.filter_map(|d| d.name().ok())
                .filter(|n| usable_name(n))
                .map(|n| InputDevice { id: n.clone(), label: n })
                .collect()
        })
        .unwrap_or_default()
}

/// Without PipeWire/PulseAudio, ALSA lists dozens of raw PCM names per card
/// (hw:, plughw:, surround51, iec958, dmix, ...). Keep the useful ones;
/// Windows and macOS device names pass through unchanged.
fn usable_name(name: &str) -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    matches!(name, "default" | "pipewire" | "pulse") || name.starts_with("sysdefault:")
}

/// Real inputs known to PipeWire/PulseAudio, without the "Monitor of ..."
/// entries that capture playback.
#[cfg(target_os = "linux")]
fn pulse_sources() -> Vec<InputDevice> {
    all_pulse_sources().into_iter().filter(|s| !s.id.ends_with(".monitor")).collect()
}

/// Every PipeWire/PulseAudio source, monitors included (they stay selectable
/// by id, for example to record what is being played).
#[cfg(target_os = "linux")]
fn all_pulse_sources() -> Vec<InputDevice> {
    let Ok(out) = std::process::Command::new("pactl")
        .args(["list", "sources"])
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut list, mut name) = (Vec::new(), None::<String>);
    for line in text.lines().map(str::trim) {
        if let Some(n) = line.strip_prefix("Name: ") {
            name = Some(n.to_owned());
        } else if let (Some(d), Some(n)) = (line.strip_prefix("Description: "), name.take()) {
            list.push(InputDevice { id: n, label: d.to_owned() });
        }
    }
    list
}

fn open_stream(samples: Arc<Mutex<Vec<f32>>>, name: &str) -> Result<(cpal::Stream, u32)> {
    let host = cpal::default_host();
    let find = |wanted: &str| {
        host.input_devices()
            .ok()
            .and_then(|mut d| d.find(|d| d.name().is_ok_and(|n| n == wanted)))
    };
    // PipeWire/PulseAudio inputs are picked through the ALSA "pulse" device,
    // which records from $PULSE_SOURCE.
    #[cfg(target_os = "linux")]
    let pulse = if name.is_empty() {
        std::env::remove_var("PULSE_SOURCE");
        None
    } else if all_pulse_sources().iter().any(|s| s.id == name) {
        std::env::set_var("PULSE_SOURCE", name);
        Some(find("pulse").or_else(|| find("pipewire")).ok_or_else(|| {
            anyhow!("this system cannot select PipeWire/PulseAudio microphones (ALSA pulse plugin missing); choose System default")
        })?)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let pulse: Option<cpal::Device> = None;
    let device = if let Some(d) = pulse {
        Some(d)
    } else if name.is_empty() {
        host.default_input_device()
    } else {
        // A saved microphone may be unplugged; fall back to the default.
        find(name).or_else(|| host.default_input_device())
    }
    .ok_or_else(|| anyhow!("no microphone found"))?;
    let config = device.default_input_config()?;
    let rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();

    let stream = match config.sample_format() {
        SampleFormat::F32 => build::<f32>(&device, &stream_config, channels, samples)?,
        SampleFormat::I16 => build::<i16>(&device, &stream_config, channels, samples)?,
        SampleFormat::U16 => build::<u16>(&device, &stream_config, channels, samples)?,
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    };
    stream.play()?;
    Ok((stream, rate))
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    samples: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mut buf = samples.lock().unwrap();
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                buf.push(sum / channels as f32);
            }
        },
        |e| eprintln!("audio stream error: {e}"),
        None,
    )?)
}

/// Band-limited resampling (Hann-windowed sinc). Plain linear interpolation
/// folds everything above 8 kHz back into the speech band, which smears
/// fricatives and hurts recognition, especially for fast speech.
fn resample(input: &[f32], from: u32) -> Vec<f32> {
    if from == TARGET_RATE {
        return input.to_vec();
    }
    let ratio = from as f64 / TARGET_RATE as f64;
    let len = (input.len() as f64 / ratio) as usize;
    // Cut off at the target Nyquist frequency when downsampling.
    let cutoff = (1.0 / ratio).min(1.0);
    const ZEROS: f64 = 8.0;
    let half = (ZEROS / cutoff).ceil() as isize;
    let sinc = |x: f64| if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
    (0..len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let center = pos.floor() as isize;
            let mut acc = 0.0f64;
            let mut norm = 0.0f64;
            for k in (center - half + 1)..=(center + half) {
                let d = k as f64 - pos;
                let w = 0.5 * (1.0 + (std::f64::consts::PI * d / half as f64).cos());
                let c = cutoff * sinc(cutoff * d) * w;
                norm += c;
                if k >= 0 && (k as usize) < input.len() {
                    acc += input[k as usize] as f64 * c;
                }
            }
            (acc / norm) as f32
        })
        .collect()
}

fn encode_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    let mono = resample(samples, rate);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for s in mono {
            writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Encodes microphone samples (at `rate`) as the 16 kHz mono WAV that the
/// transcription backends expect.
pub fn samples_to_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    encode_wav(samples, rate)
}

/// Looks for a good place to hand the audio recorded so far to the
/// transcriber while the user is still speaking: a short pause after a bit of
/// speech, or, if there is no pause for a few seconds, the quietest moment
/// of the last seconds. Returns the sample index to cut at, relative to the
/// start of `region`.
pub fn find_pause_cut(region: &[f32], rate: u32) -> Option<usize> {
    const RUN: usize = 3;
    const PAUSE: usize = 12; // frames of 20 ms = 240 ms
    const MIN_SPEECH: usize = 40; // 800 ms
    const KEEP: usize = 6; // keep 120 ms of the pause in the chunk
    const FORCE_AFTER: usize = 6 * 50; // 6 s without a pause
    const FORCE_WINDOW: usize = 2 * 50; // look for a dip in the last 2 s

    let frame = (rate as usize / 50).max(1);
    let loud: Vec<f32> = region
        .chunks_exact(frame)
        .map(|f| (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt())
        .collect();
    if loud.len() < 50 {
        return None;
    }
    let mut sorted = loud.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let noise = sorted[sorted.len() / 5];
    let loudest = sorted[sorted.len() * 9 / 10];
    let threshold = (noise * 4.0).min(loudest * 0.5).max(0.01);

    let speech = |i: usize| loud[i..i + RUN].iter().all(|&l| l > threshold);
    let last_speech = (0..=loud.len() - RUN).rev().find(|&i| speech(i));
    let speech_frames = loud.iter().filter(|&&l| l > threshold).count();

    if let Some(ls) = last_speech {
        let end = ls + RUN;
        if loud.len() - end >= PAUSE && speech_frames >= MIN_SPEECH {
            return Some(((end + KEEP) * frame).min(region.len()));
        }
    }
    if loud.len() >= FORCE_AFTER && speech_frames >= MIN_SPEECH {
        // Fluent speech without a pause: cut at the quietest point of the
        // last two seconds, which is usually between two words.
        let from = loud.len() - FORCE_WINDOW;
        let quietest = (from..loud.len() - RUN).min_by(|&a, &b| loud[a].total_cmp(&loud[b]))?;
        return Some(quietest * frame);
    }
    None
}

/// Cuts silence from the start and end of a recording (16 kHz mono WAV from
/// [`Recorder::stop`]). Whisper then has less to process, and silence no
/// longer produces invented sentences. Returns `None` if there is no speech.
pub fn trim_silence(wav: &[u8]) -> Result<Option<Vec<u8>>> {
    const FRAME: usize = (TARGET_RATE as usize) / 50; // 20 ms
    const RUN: usize = 3; // a click is not speech; require 60 ms in a row
    const PAD: usize = 12; // keep 240 ms around the speech

    let mut reader = hound::WavReader::new(Cursor::new(wav))?;
    let spec = reader.spec();
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;

    let loud: Vec<f32> = samples
        .chunks(FRAME)
        .map(|f| (f.iter().map(|&s| (s as f32).powi(2)).sum::<f32>() / f.len() as f32).sqrt() / i16::MAX as f32)
        .collect();
    if loud.len() < RUN {
        return Ok(Some(wav.to_vec()));
    }
    // The quietest fifth of the frames approximates the room noise. When the
    // whole clip is speech that estimate is far too high, so it is capped at
    // half of the loud end.
    let mut sorted = loud.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let noise = sorted[sorted.len() / 5];
    let loudest = sorted[sorted.len() * 9 / 10];
    let threshold = (noise * 4.0).min(loudest * 0.5).max(0.01);

    let speech = |i: usize| loud[i..i + RUN].iter().all(|&l| l > threshold);
    let Some(first) = (0..=loud.len() - RUN).find(|&i| speech(i)) else {
        return Ok(None);
    };
    let last = (0..=loud.len() - RUN).rev().find(|&i| speech(i)).unwrap_or(first) + RUN;

    let from = first.saturating_sub(PAD) * FRAME;
    let to = ((last + PAD) * FRAME).min(samples.len());
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for &s in &samples[from..to] {
            writer.write_sample(s)?;
        }
        writer.finalize()?;
    }
    Ok(Some(cursor.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_of(parts: &[(f32, usize)]) -> Vec<u8> {
        // (amplitude, seconds) segments of a 440 Hz tone
        let mut samples = Vec::new();
        for &(amp, secs) in parts {
            for i in 0..(secs * TARGET_RATE as usize) {
                samples.push(amp * (i as f32 * 440.0 * std::f32::consts::TAU / TARGET_RATE as f32).sin());
            }
        }
        encode_wav(&samples, TARGET_RATE).unwrap()
    }

    fn tone(amp: f32, secs: f32, rate: u32) -> Vec<f32> {
        (0..(secs * rate as f32) as usize)
            .map(|i| amp * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin())
            .collect()
    }

    #[test]
    fn finds_a_pause_after_speech() {
        let mut region = tone(0.3, 3.0, 48_000);
        region.extend(tone(0.001, 1.0, 48_000));
        let cut = find_pause_cut(&region, 48_000).expect("pause");
        assert!(cut > 3 * 48_000 && cut < 3 * 48_000 + 48_000 / 2, "cut at {cut}");
    }

    #[test]
    fn no_cut_while_still_speaking_or_when_short() {
        assert!(find_pause_cut(&tone(0.3, 4.0, 48_000), 48_000).is_none());
        assert!(find_pause_cut(&tone(0.3, 5.5, 48_000), 48_000).is_none());
        let mut short = tone(0.3, 0.5, 48_000);
        short.extend(tone(0.001, 0.5, 48_000));
        assert!(find_pause_cut(&short, 48_000).is_none());
    }

    #[test]
    fn long_speech_is_cut_before_the_window_ends() {
        assert!(find_pause_cut(&tone(0.3, 25.0, 16_000), 16_000).is_some());
    }

    #[test]
    fn trims_leading_and_trailing_silence() {
        let wav = wav_of(&[(0.001, 2), (0.3, 1), (0.001, 2)]);
        let trimmed = trim_silence(&wav).unwrap().unwrap();
        let secs = (trimmed.len() - 44) as f32 / 32_000.0;
        assert!((1.0..2.0).contains(&secs), "got {secs}s");
    }

    #[test]
    fn pure_silence_has_no_speech() {
        assert!(trim_silence(&wav_of(&[(0.001, 3)])).unwrap().is_none());
    }

    #[test]
    fn speech_only_is_kept_whole() {
        let wav = wav_of(&[(0.3, 2)]);
        let trimmed = trim_silence(&wav).unwrap().unwrap();
        assert!(trimmed.len() >= wav.len() - 2 * 640);
    }

    #[test]
    fn resample_halves_length_for_32k() {
        let out = resample(&vec![0.0; 32_000], 32_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn resample_removes_content_above_target_nyquist() {
        // A 20 kHz tone at 48 kHz must not alias into the 16 kHz output.
        let input: Vec<f32> = (0..48_000)
            .map(|n| (2.0 * std::f32::consts::PI * 20_000.0 * n as f32 / 48_000.0).sin())
            .collect();
        let out = resample(&input, 48_000);
        let peak = out[200..out.len() - 200].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 0.05, "aliased energy: {peak}");
    }

    #[test]
    fn wav_has_riff_header() {
        let wav = encode_wav(&[0.0; 1600], 16_000).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
