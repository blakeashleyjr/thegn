//! Device owner runs separately from inference, so stop closes the microphone
//! even when inference is slow. The parent process enforces the hard deadline.
use crate::{Normalizer, SAMPLE_RATE};
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Audio {
    samples: Vec<f32>,
    error: Option<String>,
    closed: bool,
}
pub struct Capture {
    audio: Arc<Mutex<Audio>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Capture {
    /// Called only in the disposable worker, after model and config validation.
    pub fn start(max_seconds: u64, stop: Arc<AtomicBool>, on_thread_start: fn()) -> Result<Self> {
        let audio = Arc::new(Mutex::new(Audio::default()));
        let shared = audio.clone();
        let stopped = stop.clone();
        let worker = std::thread::Builder::new()
            .name("thegn-mic".into())
            .spawn(move || {
                on_thread_start(); // Host supplies its platform scheduling class.
                let result = run(shared.clone(), max_seconds.clamp(1, 300), &stopped);
                if let Ok(mut a) = shared.lock() {
                    if let Err(e) = result {
                        a.error = Some(format!("microphone: {e:#}"));
                    }
                    a.closed = true;
                }
            })?;
        Ok(Self {
            audio,
            stop,
            worker: Some(worker),
        })
    }
    pub fn progress(&self) -> Result<(usize, bool)> {
        let a = self
            .audio
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone buffer unavailable"))?;
        if let Some(e) = &a.error {
            bail!("{e}");
        }
        Ok((a.samples.len(), a.closed))
    }
    pub fn snapshot(&self, last: Option<usize>) -> Result<(usize, Vec<f32>, bool)> {
        let a = self
            .audio
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone buffer unavailable"))?;
        if let Some(e) = &a.error {
            bail!("{e}");
        }
        let end = a.samples.len();
        let start = last.map_or(0, |n| end.saturating_sub(n));
        Ok((start, a.samples[start..].to_vec(), a.closed))
    }
    pub fn close(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("microphone worker crashed"))?;
        }
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn run(audio: Arc<Mutex<Audio>>, max_seconds: u64, stop: &AtomicBool) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default input device; check audio routing and microphone permission")?;
    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let limit = max_seconds as usize * SAMPLE_RATE;
    let mut normalizer = Normalizer::new(config.sample_rate, config.channels as usize);
    let on_error = audio.clone();
    let input = audio.clone();
    // Use CPAL's raw callback to handle all common hardware sample formats.
    let stream = device.build_input_stream_raw(
        config,
        format,
        move |data, _| {
            let samples: Vec<f32> = match format {
                cpal::SampleFormat::F32 => data.as_slice::<f32>().unwrap_or_default().to_vec(),
                cpal::SampleFormat::F64 => data
                    .as_slice::<f64>()
                    .unwrap_or_default()
                    .iter()
                    .map(|v| *v as f32)
                    .collect(),
                cpal::SampleFormat::I16 => data
                    .as_slice::<i16>()
                    .unwrap_or_default()
                    .iter()
                    .map(|v| *v as f32 / 32768.0)
                    .collect(),
                cpal::SampleFormat::U16 => data
                    .as_slice::<u16>()
                    .unwrap_or_default()
                    .iter()
                    .map(|v| (*v as f32 - 32768.0) / 32768.0)
                    .collect(),
                cpal::SampleFormat::I32 => data
                    .as_slice::<i32>()
                    .unwrap_or_default()
                    .iter()
                    .map(|v| *v as f32 / 2147483648.0)
                    .collect(),
                _ => {
                    if let Ok(mut a) = input.lock() {
                        a.error = Some("unsupported microphone sample format".into());
                    }
                    return;
                }
            };
            if let Ok(mut a) = input.lock() {
                normalizer.push(&samples, &mut a.samples, limit);
            }
        },
        move |e| {
            if let Ok(mut a) = on_error.lock() {
                a.error = Some(e.to_string());
            }
        },
        Some(Duration::from_secs(5)),
    )?;
    stream.play()?;
    let deadline = Instant::now() + Duration::from_secs(max_seconds);
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        if audio
            .lock()
            .map_err(|_| anyhow::anyhow!("microphone buffer unavailable"))?
            .error
            .is_some()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The duration cap aborts an in-flight preview just like an explicit stop,
    // leaving the independent final-pass deadline available for the recording.
    stop.store(true, Ordering::Release);
    drop(stream); // Close device before any final inference.
    Ok(())
}
