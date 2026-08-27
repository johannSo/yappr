use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;

use crate::asr::SAMPLE_RATE;
use crate::config::AudioConfig;

/// Averages interleaved channels down to mono.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Offline resample of a complete mono buffer to 16 kHz.
///
/// The device is asked for 16 kHz first (PipeWire almost always obliges), so
/// this is a fallback path for hardware that refuses.
///
/// rubato 5.0 replaced the old `FftFixedIn` chunk-at-a-time API with an
/// `Fft` resampler driven through the `audioadapter` crate, plus a
/// `Resampler::process_all` helper that runs the whole chunk loop and trims
/// the resampler's startup delay in one call. That helper is what we use
/// here instead of hand-rolling a chunk loop.
fn resample_to_16k(input: &[f32], from_rate: u32) -> Result<Vec<f32>> {
    use rubato::audioadapter_buffers::direct::InterleavedSlice;
    use rubato::{Fft, FixedSync, Resampler};

    if from_rate == SAMPLE_RATE as u32 {
        return Ok(input.to_vec());
    }
    if input.is_empty() {
        return Ok(Vec::new());
    }

    const CHUNK: usize = 1024;
    let mut resampler = Fft::<f32>::new(
        from_rate as usize,
        SAMPLE_RATE as usize,
        CHUNK,
        1, // mono
        FixedSync::Input,
    )
    .map_err(|e| anyhow!("building resampler: {e}"))?;

    let adapter = InterleavedSlice::new(input, 1, input.len())
        .map_err(|e| anyhow!("wrapping input buffer: {e}"))?;

    let output = resampler
        .process_all(&adapter, input.len(), None)
        .map_err(|e| anyhow!("resample failed: {e}"))?;

    Ok(output.take_data())
}

/// Commands sent to the dedicated audio thread.
///
/// The thread — and only the thread — ever touches a `cpal::Stream`; on
/// Linux that stream wraps ALSA/PipeWire handles that are not `Send` (for
/// good reason: the underlying client objects are not thread-safe to hand
/// off), so it must be built, played, and dropped on one thread and never
/// cross a thread boundary. `Recorder` talks to that thread only through
/// these commands and their reply channels.
enum Command {
    Start {
        on_level: Box<dyn Fn(f32) + Send>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Stop {
        reply: mpsc::Sender<Result<Vec<f32>, String>>,
    },
}

/// Everything about the selected device the audio thread needs once, cached
/// at startup so `start`/`stop` don't repeat device enumeration.
struct DeviceSetup {
    device: cpal::Device,
    channels: usize,
    rate: u32,
    max_samples_native: usize,
}

fn setup_device(cfg: &AudioConfig) -> Result<DeviceSetup> {
    let host = cpal::default_host();
    let device = if cfg.device == "default" {
        host.default_input_device().context("no default input device")?
    } else {
        host.input_devices()?
            .find(|d| d.to_string() == cfg.device)
            .with_context(|| format!("input device not found: {}", cfg.device))?
    };

    // Prefer 16 kHz directly; PipeWire resamples transparently.
    let supported = device.default_input_config().context("default input config")?;
    let rate = if device
        .supported_input_configs()
        .context("supported input configs")?
        .any(|r| r.min_sample_rate() <= SAMPLE_RATE as u32 && r.max_sample_rate() >= SAMPLE_RATE as u32)
    {
        SAMPLE_RATE as u32
    } else {
        supported.sample_rate()
    };
    let channels = supported.channels() as usize;

    tracing::info!(rate, channels, device = %device, "input device selected");

    Ok(DeviceSetup {
        max_samples_native: rate as usize * channels * cfg.max_seconds as usize,
        device,
        channels,
        rate,
    })
}

/// Body of the dedicated audio thread spawned by `Recorder::new`.
///
/// Device selection, the `cpal::Stream`, and its teardown all happen here.
/// Nothing that touches cpal ever leaves this function.
fn audio_thread_main(
    cfg: AudioConfig,
    ready: mpsc::Sender<Result<(), String>>,
    commands: mpsc::Receiver<Command>,
    recording: Arc<AtomicBool>,
) {
    let setup = match setup_device(&cfg) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        // Recorder::new gave up waiting (shouldn't happen, but don't spin
        // forever holding an open audio device if it did).
        return;
    }

    let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let mut stream: Option<cpal::Stream> = None;

    while let Ok(cmd) = commands.recv() {
        match cmd {
            Command::Start { on_level, reply } => {
                if stream.is_some() {
                    let _ = reply.send(Ok(())); // already recording: idempotent
                    continue;
                }
                {
                    let mut buf = buffer.lock().unwrap();
                    buf.clear();
                    buf.reserve(setup.max_samples_native);
                }
                let buf_for_cb = Arc::clone(&buffer);
                let cap = setup.max_samples_native;
                let stream_config = cpal::StreamConfig {
                    channels: setup.channels as u16,
                    sample_rate: setup.rate,
                    buffer_size: cpal::BufferSize::Default,
                };

                let built = setup.device.build_input_stream(
                    stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        on_level(rms(data));
                        let mut buf = buf_for_cb.lock().unwrap();
                        if buf.len() < cap {
                            let room = cap - buf.len();
                            buf.extend_from_slice(&data[..data.len().min(room)]);
                        }
                    },
                    |err| tracing::error!(?err, "input stream error"),
                    None,
                );

                let outcome = built.and_then(|s| s.play().map(|_| s));
                match outcome {
                    Ok(s) => {
                        stream = Some(s);
                        recording.store(true, Ordering::SeqCst);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            Command::Stop { reply } => {
                // Dropping the stream here, on the audio thread, is the
                // whole point of this design: it never has to be Send.
                stream.take();
                recording.store(false, Ordering::SeqCst);
                let raw = std::mem::take(&mut *buffer.lock().unwrap());
                let mono = downmix(&raw, setup.channels);
                let result = resample_to_16k(&mono, setup.rate).map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
    }
    // `commands` disconnected: the Recorder was dropped. Fall through and
    // let `stream` (if any) drop right here, on this thread.
}

/// Captures microphone audio and always yields 16 kHz mono `f32` samples.
///
/// Everything cpal-related — the host, the device, and the `Stream` — lives
/// on a single dedicated thread owned by this struct. `Recorder` itself
/// holds only channel endpoints and an atomic flag, so it is `Send + Sync`
/// without any `unsafe impl`.
pub struct Recorder {
    commands: Mutex<Option<mpsc::Sender<Command>>>,
    recording: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Recorder {
    pub fn new(cfg: &AudioConfig) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let recording = Arc::new(AtomicBool::new(false));
        let recording_for_thread = Arc::clone(&recording);
        let cfg = cfg.clone();

        let handle = std::thread::Builder::new()
            .name("owf-audio".into())
            .spawn(move || audio_thread_main(cfg, ready_tx, cmd_rx, recording_for_thread))
            .context("spawning audio capture thread")?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands: Mutex::new(Some(cmd_tx)),
                recording,
                handle: Mutex::new(Some(handle)),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(anyhow!(e))
            }
            Err(_) => {
                let _ = handle.join();
                Err(anyhow!("audio thread exited before initialising"))
            }
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn start(&self, on_level: impl Fn(f32) + Send + 'static) -> Result<()> {
        if self.is_recording() {
            return Ok(()); // idempotent, per spec 6
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::Start { on_level: Box::new(on_level), reply: reply_tx })?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("audio thread is not running"))?
            .map_err(|e| anyhow!(e))
    }

    /// Stops capture and returns 16 kHz mono samples.
    pub fn stop(&self) -> Result<Vec<f32>> {
        if !self.is_recording() {
            return Ok(Vec::new());
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        self.send(Command::Stop { reply: reply_tx })?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("audio thread is not running"))?
            .map_err(|e| anyhow!(e))
    }

    fn send(&self, cmd: Command) -> Result<()> {
        let guard = self.commands.lock().unwrap();
        let tx = guard.as_ref().ok_or_else(|| anyhow!("audio thread is not running"))?;
        tx.send(cmd).map_err(|_| anyhow!("audio thread is not running"))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Drop the command sender first so the audio thread's `recv()`
        // observes a disconnected channel and exits its loop — dropping
        // whatever `cpal::Stream` it still holds on that same thread — before
        // we join it. Joining without doing this first would deadlock: the
        // thread would block on `recv()` forever waiting for a sender that
        // this struct still held.
        self.commands.lock().unwrap().take();
        if let Some(handle) = self.handle.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_interleaved_channels() {
        // Two channels: L = [1.0, 3.0], R = [0.0, 1.0]
        let interleaved = [1.0f32, 0.0, 3.0, 1.0];
        assert_eq!(downmix(&interleaved, 2), vec![0.5, 2.0]);
    }

    #[test]
    fn downmix_is_a_copy_for_mono() {
        let mono = [0.1f32, -0.2, 0.3];
        assert_eq!(downmix(&mono, 1), mono.to_vec());
    }

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms(&[0.0; 128]), 0.0);
    }

    #[test]
    fn rms_of_a_constant_signal_is_its_magnitude() {
        assert!((rms(&[0.5; 128]) - 0.5).abs() < 1e-6);
        assert!((rms(&[-0.5; 128]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rms_of_an_empty_slice_is_zero() {
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn resampling_preserves_duration_within_a_frame() {
        // 1 second at 48 kHz must become ~1 second at 16 kHz.
        let input = vec![0.0f32; 48_000];
        let out = resample_to_16k(&input, 48_000).unwrap();
        let drift = (out.len() as i64 - 16_000).abs();
        assert!(drift < 1_000, "expected ~16000 samples, got {}", out.len());
    }

    #[test]
    fn resampling_is_a_passthrough_when_already_16k() {
        let input = vec![0.25f32; 1_000];
        let out = resample_to_16k(&input, 16_000).unwrap();
        assert_eq!(out, input);
    }
}
