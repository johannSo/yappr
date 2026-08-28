use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

pub(crate) fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Largest absolute sample value, regardless of sign. `0.0` for an empty
/// slice, matching `rms`'s convention for the same case.
pub(crate) fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
}

/// Native (interleaved, pre-downmix) samples a recording of `duration`
/// *should* have produced at `native_rate` Hz across `channels` channels, if
/// nothing were ever dropped.
///
/// This is the "5 s held at 48 kHz stereo should deliver ~480,000 samples"
/// arithmetic the capture-debug record is built around -- see
/// `capture_ratio` for what actually got delivered gets compared against it.
pub fn expected_native_samples(duration: Duration, native_rate: u32, channels: usize) -> usize {
    (duration.as_secs_f64() * native_rate as f64 * channels as f64).round() as usize
}

/// Fraction of `expected` native samples that were actually captured.
///
/// `1.0` when nothing was expected (a zero-duration recording) rather than
/// dividing by zero -- there is nothing to have dropped. Deliberately
/// unclamped above 1.0: a ratio above one is itself a real (if different)
/// diagnostic signal, not an error to hide.
pub fn capture_ratio(captured: usize, expected: usize) -> f64 {
    if expected == 0 {
        1.0
    } else {
        captured as f64 / expected as f64
    }
}

/// Snapshot of one recording's raw capture-side facts -- everything the
/// debug record's `capture` section needs that isn't derivable from the
/// resampled sample buffer itself. See `crate::debug::CaptureDebug`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureStats {
    pub device: String,
    pub native_sample_rate: u32,
    pub channels: usize,
    /// Interleaved samples actually handed to the callback across the whole
    /// recording, regardless of whether they fit in the capacity-bounded
    /// buffer `Command::Start` allocates.
    pub native_samples_captured: usize,
    /// Count of `cpal` stream error-callback invocations (ALSA Xruns and the
    /// like) during the recording.
    pub stream_errors: usize,
    /// Wall-clock time between `start` actually building the stream and
    /// `stop` tearing it down.
    pub duration: Duration,
}

impl Default for CaptureStats {
    /// The "nothing was ever recorded" case: `Recorder::stop()` returns this
    /// when called while not recording, mirroring the empty-samples default
    /// it has always returned in that situation.
    fn default() -> Self {
        Self {
            device: String::new(),
            native_sample_rate: 0,
            channels: 0,
            native_samples_captured: 0,
            stream_errors: 0,
            duration: Duration::ZERO,
        }
    }
}

/// What `Recorder::stop()` yields: the 16 kHz mono samples plus the raw
/// capture-side facts behind them.
#[derive(Debug, Clone, PartialEq)]
pub struct StopOutcome {
    pub samples: Vec<f32>,
    pub capture: CaptureStats,
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
        reply: mpsc::Sender<Result<StopOutcome, String>>,
    },
}

/// Everything about the selected device the audio thread needs once, cached
/// at startup so `start`/`stop` don't repeat device enumeration.
struct DeviceSetup {
    device: cpal::Device,
    /// `device.to_string()`, cached once rather than recomputed on every
    /// `stop()` -- it feeds straight into `CaptureStats::device`.
    name: String,
    channels: usize,
    rate: u32,
    max_samples_native: usize,
}

/// Whether the device can actually open an f32 input stream at `rate`.
///
/// Builds a throwaway stream and drops it. This is the only reliable test:
/// the rate ranges cpal reports are advisory, and ALSA refuses combinations
/// that fall inside them.
fn can_build_at(device: &cpal::Device, channels: usize, rate: u32) -> bool {
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    device
        .build_input_stream(
            config,
            |_: &[f32], _: &cpal::InputCallbackInfo| {},
            |_| {},
            None,
        )
        .is_ok()
}

/// The value `[audio] device` takes to mean "whatever the host calls the
/// default input". Not a device name: the default device's own name is not
/// present in `input_devices()` at all on this machine.
pub const DEFAULT_DEVICE: &str = "default";

/// The single string that identifies a device, both to `[audio] device` in
/// config.toml and to the settings GUI's dropdown.
///
/// Shared by `setup_device` (which matches against it) and
/// `list_input_devices` (which offers it), because they must agree exactly: a
/// GUI that writes a name the daemon cannot resolve produces a config that
/// fails at the next dictation, which is strictly worse than editing the TOML
/// by hand. One function means they cannot drift.
fn device_key(device: &cpal::Device) -> String {
    device.to_string()
}

fn setup_device(cfg: &AudioConfig) -> Result<DeviceSetup> {
    let host = cpal::default_host();
    let device = if cfg.device == DEFAULT_DEVICE {
        host.default_input_device().context("no default input device")?
    } else {
        host.input_devices()?
            .find(|d| device_key(d) == cfg.device)
            .with_context(|| format!("input device not found: {}", cfg.device))?
    };

    // Prefer 16 kHz so the resampler can be skipped -- but VERIFY it by
    // building a throwaway stream rather than trusting the advertised range.
    //
    // `supported_input_configs()` reports a min..max span, and a rate inside
    // that span is NOT necessarily buildable with this device's channel count
    // and sample format. PipeWire accepts 16 kHz transparently because it
    // resamples for us; real ALSA hardware rejects it at `build_input_stream`
    // with "The requested stream configuration is not supported by the
    // device." Trusting the range meant we never fell back, so the resampler
    // below was unreachable and capture failed outright on such hardware.
    let supported = device.default_input_config().context("default input config")?;
    let channels = supported.channels() as usize;
    let rate = if can_build_at(&device, channels, SAMPLE_RATE as u32) {
        SAMPLE_RATE as u32
    } else {
        tracing::info!(
            native_rate = supported.sample_rate(),
            "device refused 16 kHz; capturing native and resampling"
        );
        supported.sample_rate()
    };

    tracing::info!(rate, channels, device = %device, "input device selected");
    let name = device_key(&device);

    Ok(DeviceSetup {
        max_samples_native: rate as usize * channels * cfg.max_seconds as usize,
        device,
        name,
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
    // Reset on every `Start` (see below) so each recording's counts are its
    // own, not a running total across the whole daemon lifetime.
    let delivered = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let mut stream: Option<cpal::Stream> = None;
    let mut started_at: Option<Instant> = None;

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
                delivered.store(0, Ordering::SeqCst);
                errors.store(0, Ordering::SeqCst);
                let buf_for_cb = Arc::clone(&buffer);
                let delivered_for_cb = Arc::clone(&delivered);
                let errors_for_cb = Arc::clone(&errors);
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
                        // Counts every sample the callback actually
                        // delivered -- including any beyond `cap`, which the
                        // buffer below silently drops on the floor. This is
                        // the diagnostic this whole facility exists for: the
                        // hardware/ALSA side can be dropping samples (Xruns)
                        // well before this buffer's own capacity is ever a
                        // factor, and `native_samples_captured` needs to
                        // reflect what cpal actually handed us, not what we
                        // chose to keep.
                        delivered_for_cb.fetch_add(data.len(), Ordering::Relaxed);
                        let mut buf = buf_for_cb.lock().unwrap();
                        if buf.len() < cap {
                            let room = cap - buf.len();
                            buf.extend_from_slice(&data[..data.len().min(room)]);
                        }
                    },
                    move |err| {
                        errors_for_cb.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(?err, "input stream error");
                    },
                    None,
                );

                let outcome = built.and_then(|s| s.play().map(|_| s));
                match outcome {
                    Ok(s) => {
                        stream = Some(s);
                        started_at = Some(Instant::now());
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
                let duration = started_at.take().map(|t| t.elapsed()).unwrap_or_default();
                let native_samples_captured = delivered.load(Ordering::SeqCst);
                let stream_errors = errors.load(Ordering::SeqCst);
                let raw = std::mem::take(&mut *buffer.lock().unwrap());
                let mono = downmix(&raw, setup.channels);
                let result = resample_to_16k(&mono, setup.rate)
                    .map(|samples| StopOutcome {
                        samples,
                        capture: CaptureStats {
                            device: setup.name.clone(),
                            native_sample_rate: setup.rate,
                            channels: setup.channels,
                            native_samples_captured,
                            stream_errors,
                            duration,
                        },
                    })
                    .map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
        }
    }
    // `commands` disconnected: the Recorder was dropped. Fall through and
    // let `stream` (if any) drop right here, on this thread.
}

/// Runs `f` on a worker thread and gives up after `timeout`.
///
/// Device enumeration is the reason this exists. Invariant 6 -- every
/// subprocess call goes through `procutil::run_with_timeout` -- was written
/// after a hung `wtype` wedged the daemon's single-threaded accept loop
/// forever. `cpal` enumeration on a sick ALSA or PipeWire stack is the same
/// hazard reached by a different route: it is an in-process call, so
/// `procutil` cannot help, but it can block just as indefinitely.
///
/// The worker thread is deliberately not killed on timeout -- there is no safe
/// way to do that -- it is abandoned. The daemon stays responsive, and a
/// wedged enumeration costs one leaked thread rather than the whole process.
fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
        .map_err(|_| anyhow::anyhow!("timed out after {timeout:?}"))
}

/// The input devices `cpal` can see, shaped for the settings GUI's dropdown.
///
/// The raw enumeration is not directly usable, as measured on this machine:
///
/// - The host default reports its name as `Default Audio Device`, and that
///   name appears nowhere in `input_devices()`. Selecting it by name is
///   therefore impossible -- but `[audio] device` already special-cases the
///   literal `"default"`, so that is what the first row offers.
/// - Four distinct devices came back with the identical name
///   `HDA Intel PCH, ALC3271 Analog`. Since `setup_device` resolves a name by
///   taking the *first* match, the duplicates are not separately selectable by
///   any config this GUI could write, so offering four identical rows would
///   promise a choice that does not exist.
pub fn list_input_devices(timeout: Duration) -> Result<Vec<crate::proto::InputDevice>> {
    with_timeout(timeout, || {
        let host = cpal::default_host();
        let names = host.input_devices()?.map(|d| device_key(&d)).collect();
        Ok(shape_device_list(names))
    })?
}

/// The pure half of [`list_input_devices`]: prepend the host-default row and
/// drop unselectable duplicates. Separated so the shaping rules are testable
/// without a sound card.
///
/// Deliberately does *not* report whether a device can be opened. That probe
/// existed and was removed: measured from inside the daemon, it reported
/// `false` for every device -- including the one dictation was working on --
/// because the daemon holds its own recorder open, and a second stream on the
/// same device from the same process fails. From a standalone process the same
/// probe reported `true` for all of them. A flag that is wrong precisely for
/// the device the user is using would grey out their working microphone, so
/// there is no flag. A device that cannot be opened fails at the next
/// dictation, with the error the daemon already surfaces for that.
fn shape_device_list(enumerated: Vec<String>) -> Vec<crate::proto::InputDevice> {
    let mut out = vec![crate::proto::InputDevice {
        name: DEFAULT_DEVICE.to_string(),
        is_default: true,
    }];
    let mut seen = std::collections::HashSet::new();
    for name in enumerated {
        if name == DEFAULT_DEVICE || !seen.insert(name.clone()) {
            continue;
        }
        out.push(crate::proto::InputDevice { name, is_default: false });
    }
    out
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

    /// Stops capture and returns 16 kHz mono samples plus the raw
    /// capture-side stats behind them (device, native rate/channels,
    /// samples actually delivered, and stream error count) -- see
    /// [`StopOutcome`].
    pub fn stop(&self) -> Result<StopOutcome> {
        if !self.is_recording() {
            return Ok(StopOutcome { samples: Vec::new(), capture: CaptureStats::default() });
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

    fn names(list: &[crate::proto::InputDevice]) -> Vec<&str> {
        list.iter().map(|d| d.name.as_str()).collect()
    }

    /// `[audio] device = "default"` is the only way to say "host default",
    /// because the default device's own name is absent from the enumeration.
    /// The dropdown must therefore always offer it, even on a machine with no
    /// input devices at all.
    #[test]
    fn the_device_list_always_opens_with_the_host_default() {
        assert_eq!(names(&shape_device_list(vec![])), ["default"]);
        assert!(shape_device_list(vec![])[0].is_default);
    }

    /// Measured on this machine: four devices, one name. `setup_device`
    /// resolves a name to the first match, so the other three are not
    /// selectable by any config this GUI could write.
    #[test]
    fn devices_sharing_a_name_collapse_to_one_selectable_row() {
        let hw = |n: &str| n.to_string();
        let got = shape_device_list(vec![
            hw("HDA Intel PCH"),
            hw("HDA Intel PCH"),
            hw("PipeWire"),
            hw("HDA Intel PCH"),
        ]);
        assert_eq!(names(&got), ["default", "HDA Intel PCH", "PipeWire"]);
    }

    /// A host that really does enumerate something called `default` must not
    /// produce two rows the GUI renders identically.
    #[test]
    fn an_enumerated_device_named_default_does_not_duplicate_the_synthetic_row() {
        let got = shape_device_list(vec!["default".to_string()]);
        assert_eq!(names(&got), ["default"]);
    }

    #[test]
    fn with_timeout_returns_a_fast_result() {
        let got = with_timeout(Duration::from_secs(5), || 7).unwrap();
        assert_eq!(got, 7);
    }

    /// The property the daemon depends on: a call that never returns must not
    /// become a daemon that never answers again.
    #[test]
    fn with_timeout_gives_up_on_a_worker_that_never_finishes() {
        let err = with_timeout(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(30));
            7
        })
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

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

    #[test]
    fn peak_of_silence_is_zero() {
        assert_eq!(peak(&[0.0; 128]), 0.0);
    }

    #[test]
    fn peak_finds_the_largest_magnitude_regardless_of_sign() {
        assert_eq!(peak(&[0.1, -0.9, 0.4, 0.2]), 0.9);
        assert_eq!(peak(&[-0.7, 0.3]), 0.7);
    }

    #[test]
    fn peak_of_an_empty_slice_is_zero() {
        assert_eq!(peak(&[]), 0.0);
    }

    #[test]
    fn expected_native_samples_matches_the_reported_bug_scenario() {
        // Spec: 5 s held at 48 kHz stereo should deliver ~480,000 samples.
        let expected = expected_native_samples(Duration::from_secs(5), 48_000, 2);
        assert_eq!(expected, 480_000);
    }

    #[test]
    fn expected_native_samples_rounds_fractional_durations() {
        // 0.1 s at 16 kHz mono = 1600 samples exactly.
        assert_eq!(expected_native_samples(Duration::from_millis(100), 16_000, 1), 1_600);
    }

    #[test]
    fn expected_native_samples_of_a_zero_duration_is_zero() {
        assert_eq!(expected_native_samples(Duration::ZERO, 48_000, 2), 0);
    }

    #[test]
    fn capture_ratio_is_captured_over_expected() {
        assert_eq!(capture_ratio(240_000, 480_000), 0.5);
        assert_eq!(capture_ratio(480_000, 480_000), 1.0);
    }

    #[test]
    fn capture_ratio_of_a_degenerate_zero_expected_is_one() {
        // A zero-duration recording expects zero samples; treat that as
        // "nothing was dropped" rather than dividing by zero.
        assert_eq!(capture_ratio(0, 0), 1.0);
        assert_eq!(capture_ratio(5, 0), 1.0);
    }

    #[test]
    fn capture_ratio_can_exceed_one_without_panicking() {
        // Timing jitter between the wall-clock duration and the callback's
        // actual delivery can legitimately push this a little over 1.0; it
        // must not be clamped away, since an unexpectedly high ratio is its
        // own diagnostic signal.
        assert!((capture_ratio(481_000, 480_000) - 1.0020833333333334).abs() < 1e-9);
    }
}
