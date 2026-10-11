use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
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

    pub fn start(&self) -> Result<()> {
        let mut slot = self.active.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        let handle = std::thread::spawn(move || {
            let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
            match open_stream(samples.clone()) {
                Ok((stream, rate)) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = stop_rx.recv();
                    drop(stream);
                    let data = std::mem::take(&mut *samples.lock().unwrap());
                    encode_wav(&data, rate).unwrap_or_default()
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    Vec::new()
                }
            }
        });

        ready_rx.recv().context("audio thread died")??;
        *slot = Some(Active { stop: stop_tx, handle });
        Ok(())
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

fn open_stream(samples: Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32)> {
    let device = cpal::default_host()
        .default_input_device()
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

fn resample(input: &[f32], from: u32) -> Vec<f32> {
    if from == TARGET_RATE {
        return input.to_vec();
    }
    let ratio = from as f64 / TARGET_RATE as f64;
    let len = (input.len() as f64 / ratio) as usize;
    (0..len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx];
            let b = *input.get(idx + 1).unwrap_or(&a);
            a + (b - a) * frac
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_halves_length_for_32k() {
        let out = resample(&vec![0.0; 32_000], 32_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn wav_has_riff_header() {
        let wav = encode_wav(&[0.0; 1600], 16_000).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }
}
