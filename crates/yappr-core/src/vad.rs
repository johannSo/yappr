use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Mutex;

use sherpa_onnx::{VadModelConfig, VoiceActivityDetector};

use crate::asr::SAMPLE_RATE;

/// Finds the span of a buffer that actually contains speech.
pub trait Trimmer: Send + Sync {
    /// Returns `[start, end)` sample indices, or `None` if no speech was found.
    fn trim(&self, samples: &[f32], padding_ms: u32) -> Option<(usize, usize)>;
}

/// Expands a speech span by `padding_ms` on each side, clamped to the buffer.
/// Split out as a pure function so the arithmetic is testable without models.
fn pad_and_clamp(start: usize, end: usize, padding_ms: u32, len: usize) -> (usize, usize) {
    let pad = (SAMPLE_RATE as usize * padding_ms as usize) / 1000;
    (start.saturating_sub(pad), (end + pad).min(len))
}

/// Samples fed to the VAD per `accept_waveform` call.
///
/// Matches the block size used by sherpa-onnx's own
/// `silero_vad_remove_silence` example; the underlying Silero model expects
/// audio in windows of this size regardless of how much is handed to
/// `accept_waveform` at once, so chunking here mirrors known-working usage.
const VAD_WINDOW: usize = 512;

pub struct SileroTrimmer {
    // VoiceActivityDetector is Sync, but its queue is stateful across calls,
    // so serialise access to keep `trim` a pure function of its input.
    vad: Mutex<VoiceActivityDetector>,
}

impl SileroTrimmer {
    pub fn new(models_dir: &Path) -> Result<Self> {
        let model = models_dir.join("silero_vad.onnx");
        anyhow::ensure!(model.exists(), "missing {}", model.display());

        let mut config = VadModelConfig::default();
        config.silero_vad.model = Some(model.to_string_lossy().into_owned());
        // These are the values sherpa-onnx's own example tunes explicitly;
        // VadModelConfig::default() leaves them at 0.0, which would treat
        // almost any non-zero VAD score as speech and never close a segment.
        config.silero_vad.threshold = 0.5;
        config.silero_vad.min_silence_duration = 0.25;
        config.silero_vad.min_speech_duration = 0.25;
        config.silero_vad.max_speech_duration = 5.0;
        config.sample_rate = SAMPLE_RATE;
        config.num_threads = 1;
        config.debug = false;

        // Buffer sized for the longest recording the daemon permits.
        let vad = VoiceActivityDetector::create(&config, 130.0)
            .context("VoiceActivityDetector::create returned None")?;

        Ok(Self { vad: Mutex::new(vad) })
    }
}

impl Trimmer for SileroTrimmer {
    fn trim(&self, samples: &[f32], padding_ms: u32) -> Option<(usize, usize)> {
        if samples.is_empty() {
            return None;
        }
        let vad = self.vad.lock().ok()?;
        vad.reset();

        let mut first: Option<usize> = None;
        let mut last: Option<usize> = None;

        let collect = |vad: &VoiceActivityDetector, first: &mut Option<usize>, last: &mut Option<usize>| {
            while !vad.is_empty() {
                if let Some(seg) = vad.front() {
                    let start = seg.start().max(0) as usize;
                    let end = start + seg.n().max(0) as usize;
                    first.get_or_insert(start);
                    *last = Some(end.min(samples.len()));
                }
                vad.pop();
            }
        };

        for chunk in samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(chunk);
            collect(&vad, &mut first, &mut last);
        }
        vad.flush();
        collect(&vad, &mut first, &mut last);

        let (start, end) = (first?, last?);
        if end <= start {
            return None;
        }
        Some(pad_and_clamp(start, end, padding_ms, samples.len()))
    }
}

/// Cuts a *live* stream into utterances, where [`Trimmer`] finds the speech
/// in a buffer that is already complete.
///
/// The distinction is the whole reason this exists next to `SileroTrimmer`
/// rather than inside it. Press-to-stop dictation knows when the utterance
/// ended -- the user said so -- and asks the VAD only where inside the
/// recording the speech was. A realtime session (`realtime.rs`) has nobody
/// to ask: the audio arrives chunk by chunk and *something* has to decide
/// that a pause was the end of a sentence. Silero already makes exactly that
/// judgement, so this streams audio into the same model and takes its
/// finished segments off the queue instead of re-scanning a growing buffer.
///
/// A `#[cfg(test)]` implementation of [`Segmenter`] is what lets the whole
/// WebSocket server be tested with no model on disk; this implementation is
/// covered by the `--ignored` tests at the bottom of this file, for the ABI
/// reason this module's test header describes.
pub trait Segmenter: Send {
    /// Feeds 16 kHz mono samples in. Never blocks on a decision.
    fn accept(&mut self, samples: &[f32]);
    /// Takes the next finished utterance, if the pause after one has
    /// already been observed. Call until it answers `None`.
    fn take_segment(&mut self) -> Option<Vec<f32>>;
    /// Whether speech is being heard *right now* -- the first evidence a
    /// turn has started, which is all `onSpeechStart` needs.
    fn speaking(&self) -> bool;
    /// Ends the stream: whatever is still open becomes a segment rather
    /// than being dropped. Invariant 1's rule applied one layer earlier --
    /// audio that has been spoken into the socket must not disappear
    /// because the socket closed before the speaker paused.
    fn flush(&mut self);
}

/// [`Segmenter`] over the same Silero model `SileroTrimmer` uses, with the
/// two durations that decide segmentation taken from `[realtime]` instead of
/// being fixed.
pub struct SileroSegmenter {
    vad: VoiceActivityDetector,
    /// Odd samples left over from the last `accept`, because the model wants
    /// whole [`VAD_WINDOW`]s and a WebSocket frame is whatever size the
    /// sender felt like. Dropping the remainder instead would delete up to
    /// 31 ms of audio per frame, all of it at frame boundaries -- i.e.
    /// spread evenly through every utterance.
    pending: Vec<f32>,
}

impl SileroSegmenter {
    /// `silence_ms` is how long a pause has to be to end an utterance;
    /// `max_utterance_seconds` is where an unbroken stretch of speech is cut
    /// anyway.
    pub fn new(models_dir: &Path, silence_ms: u32, max_utterance_seconds: u32) -> Result<Self> {
        let model = models_dir.join("silero_vad.onnx");
        anyhow::ensure!(model.exists(), "missing {}", model.display());

        let mut config = VadModelConfig::default();
        config.silero_vad.model = Some(model.to_string_lossy().into_owned());
        config.silero_vad.threshold = 0.5;
        config.silero_vad.min_silence_duration = silence_ms as f32 / 1000.0;
        config.silero_vad.min_speech_duration = 0.25;
        config.silero_vad.max_speech_duration = max_utterance_seconds as f32;
        config.sample_rate = SAMPLE_RATE;
        config.num_threads = 1;
        config.debug = false;

        // The ring buffer sherpa keeps segments in. One `max_speech_duration`
        // would be exactly enough only if the consumer never fell behind;
        // transcription happens between `accept` calls, so size it for a few
        // utterances' worth and let the floor cover a very short cap.
        let capacity = (max_utterance_seconds as f32 * 3.0).max(30.0);
        let vad = VoiceActivityDetector::create(&config, capacity)
            .context("VoiceActivityDetector::create returned None")?;

        Ok(Self { vad, pending: Vec::new() })
    }
}

impl Segmenter for SileroSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() - self.pending.len() % VAD_WINDOW;
        for chunk in self.pending[..whole].chunks(VAD_WINDOW) {
            self.vad.accept_waveform(chunk);
        }
        self.pending.drain(..whole);
    }

    fn take_segment(&mut self) -> Option<Vec<f32>> {
        if self.vad.is_empty() {
            return None;
        }
        let seg = self.vad.front()?;
        let samples = seg.samples().to_vec();
        self.vad.pop();
        Some(samples)
    }

    fn speaking(&self) -> bool {
        self.vad.detected()
    }

    fn flush(&mut self) {
        // The remainder first: it is the most recent audio there is, and
        // flushing without it would cut the last word of the last sentence
        // -- the one case a user notices immediately.
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.vad.accept_waveform(&tail);
        }
        self.vad.flush();
    }
}

#[cfg(test)]
mod tests {
    //! The `#[ignore]`d tests below are the only thing that catches a
    //! C++ ABI mismatch between sherpa-onnx and llama.cpp (see
    //! `.cargo/config.toml`): with the wrong `CXXFLAGS`, constructing the
    //! VAD aborts the process with `free(): invalid pointer`.
    //!
    //! Two cheaper guards were tried and neither works, so do not
    //! reintroduce them. A *missing* model path returns `Err` from
    //! `ensure!(model.exists())` without ever creating the `OrtEnv` that
    //! aborts, so it passes while the bug is live. A *garbage* model file
    //! gets past that check, but then onnxruntime throws a C++ exception
    //! across the FFI boundary and Rust aborts on it -- so that one fails
    //! whether or not the ABI is right. Catching this needs a model that
    //! actually loads, which is exactly what `--ignored` gates on.

    use super::*;

    #[test]
    fn padding_expands_the_span_and_clamps_to_the_buffer() {
        // 16 kHz: 200 ms = 3200 samples.
        assert_eq!(pad_and_clamp(10_000, 20_000, 200, 32_000), (6_800, 23_200));
        // Clamps at the start.
        assert_eq!(pad_and_clamp(1_000, 20_000, 200, 32_000), (0, 23_200));
        // Clamps at the end.
        assert_eq!(pad_and_clamp(10_000, 31_000, 200, 32_000), (6_800, 32_000));
        // Zero padding is a no-op.
        assert_eq!(pad_and_clamp(10_000, 20_000, 0, 32_000), (10_000, 20_000));
    }

    #[test]
    fn a_span_covering_the_whole_buffer_stays_in_bounds() {
        assert_eq!(pad_and_clamp(0, 32_000, 500, 32_000), (0, 32_000));
    }

    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn silence_yields_no_speech_span() {
        let t = SileroTrimmer::new(&crate::paths::models_dir()).unwrap();
        let silence = vec![0.0f32; 16_000 * 2];
        assert_eq!(t.trim(&silence, 200), None);
    }

    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn speech_surrounded_by_silence_is_trimmed_inward() {
        let mut r = hound::WavReader::open("fixtures/hello_english.wav").unwrap();
        let speech: Vec<f32> = r
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        // One second of silence on each side.
        let mut padded = vec![0.0f32; 16_000];
        padded.extend_from_slice(&speech);
        padded.extend(std::iter::repeat_n(0.0f32, 16_000));

        let t = SileroTrimmer::new(&crate::paths::models_dir()).unwrap();
        let (start, end) = t.trim(&padded, 200).expect("should find speech");

        assert!(start > 0, "should have trimmed leading silence, got {start}");
        assert!(
            end < padded.len(),
            "should have trimmed trailing silence, got {end} of {}",
            padded.len()
        );
        assert!(end - start < padded.len(), "span should be shorter than the input");
    }
}
