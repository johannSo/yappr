use anyhow::{Context, Result};
use std::path::Path;

use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig, OnlineModelConfig,
    OnlineRecognizer, OnlineRecognizerConfig, OnlineTransducerModelConfig,
};

use crate::config::AsrConfig;
use crate::models::{self, AsrFlavor};

pub const SAMPLE_RATE: i32 = 16_000;

/// Anything that turns 16 kHz mono f32 samples into text.
///
/// This trait exists so the ASR engine can be swapped without touching the
/// pipeline — see spec §17.1.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, samples: &[f32]) -> Result<String>;
}

pub struct SherpaTranscriber {
    recognizer: OfflineRecognizer,
}

impl SherpaTranscriber {
    /// `rel_path` is the model directory's name under `models_dir`, taken
    /// from the catalogue (`models::AsrModelSpec::artifact.rel_path`) rather
    /// than hardcoded -- there is more than one offline model now.
    pub fn new(models_dir: &Path, rel_path: &str, num_threads: i32) -> Result<Self> {
        let dir = models_dir.join(rel_path);
        let p = |f: &str| -> Option<String> { Some(dir.join(f).to_string_lossy().into_owned()) };

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: p("encoder.int8.onnx"),
            decoder: p("decoder.int8.onnx"),
            joiner: p("joiner.int8.onnx"),
        };
        config.model_config.tokens = p("tokens.txt");
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = num_threads;
        config.model_config.debug = false;

        let recognizer = OfflineRecognizer::create(&config).context(
            "OfflineRecognizer::create returned None — check model paths and model_type",
        )?;

        Ok(Self { recognizer })
    }
}

impl Transcriber for SherpaTranscriber {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, samples);
        self.recognizer.decode(&stream);
        let text = stream.get_result().map(|r| r.text).unwrap_or_default();
        Ok(text.trim().to_string())
    }
}

/// The whole utterance at once, through sherpa-onnx's *online* recognizer.
///
/// The nemotron export is published only as a cache-aware streaming model,
/// so this is the only way to read it -- but yappr has no streaming UI yet
/// (spec asr-model §10), and the VAD has already decided where this
/// utterance ends. So the buffer goes in in one go, `input_finished` closes
/// it, and the single final result is what gets injected. Endpointing stays
/// off for the same reason: it exists to cut a live stream into utterances,
/// and that decision was already made upstream.
/// Silence fed *before and after* the real audio, so the whole utterance is
/// actually decoded.
///
/// Both halves are load-bearing, and each was measured against
/// `fixtures/hallo_german.wav` (2.75 s, "Alles hat ein Ende, nur die Wurst
/// hat zwei."):
///
/// | padding | transcript |
/// |---|---|
/// | none | "Nur die Wurst" |
/// | tail only | "Nur die Wurst hat zwei" |
/// | lead + tail | "Alles hat ein Ende, nur die Wurst hat zwei" |
///
/// The tail is what the upstream reference does (0.66 s), and without it the
/// final chunks never become `is_ready` so the end is never emitted. The
/// *lead* is not in any upstream example and matters more here than it would
/// anywhere else: a cache-aware model starts with zeroed caches and spends
/// its first chunk priming them, so whatever audio arrives during that chunk
/// is lost -- and `SileroTrimmer` has already stripped the leading silence
/// that would otherwise have absorbed it. Every real dictation hits this
/// model with an abrupt start.
///
/// Losing the opening words of an utterance is invariant 1 territory, which
/// is why the regression test asserts on the *first* word surviving rather
/// than merely on non-empty output.
static SILENCE_PADDING: [f32; (SAMPLE_RATE as usize) * 66 / 100] =
    [0.0; (SAMPLE_RATE as usize) * 66 / 100];

pub struct SherpaStreamingTranscriber {
    recognizer: OnlineRecognizer,
    language: String,
}

impl SherpaStreamingTranscriber {
    pub fn new(
        models_dir: &Path,
        rel_path: &str,
        num_threads: i32,
        language: &str,
    ) -> Result<Self> {
        let dir = models_dir.join(rel_path);
        let p = |f: &str| -> Option<String> { Some(dir.join(f).to_string_lossy().into_owned()) };

        let mut config = OnlineRecognizerConfig::default();
        // The crate's default is 80; every NeMo FastConformer export in the
        // catalogue is 128. `OfflineRecognizer` reads this from the encoder's
        // ONNX metadata, so the offline path never had to set it -- the online
        // one takes it from here, and a wrong value yields plausible-looking
        // wrong text rather than an error. See spec asr-model §8.
        config.feat_config.feature_dim = 128;
        config.model_config = OnlineModelConfig {
            transducer: OnlineTransducerModelConfig {
                encoder: p("encoder.int8.onnx"),
                decoder: p("decoder.int8.onnx"),
                joiner: p("joiner.int8.onnx"),
            },
            tokens: p("tokens.txt"),
            num_threads,
            ..OnlineModelConfig::default()
        };
        config.enable_endpoint = false;

        let recognizer = OnlineRecognizer::create(&config).context(
            "OnlineRecognizer::create returned None -- check model paths and feature_dim",
        )?;

        Ok(Self {
            recognizer,
            language: language.to_string(),
        })
    }
}

impl Transcriber for SherpaStreamingTranscriber {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let stream = self.recognizer.create_stream();
        stream.set_option("language", &self.language);
        stream.accept_waveform(SAMPLE_RATE, &SILENCE_PADDING);
        stream.accept_waveform(SAMPLE_RATE, samples);
        stream.accept_waveform(SAMPLE_RATE, &SILENCE_PADDING);
        stream.input_finished();
        while self.recognizer.is_ready(&stream) {
            self.recognizer.decode(&stream);
        }
        let text = self
            .recognizer
            .get_result(&stream)
            .map(|r| r.text)
            .unwrap_or_default();
        // Collapse internal whitespace. This flavour joins its segments with
        // a doubled space ("Alles hat ein Ende.  Nur die Wurst..."), which
        // the offline recognizer never produces -- and which would be
        // injected verbatim whenever normalization is off or falls back to
        // the raw transcript (invariant 1). Fixed here rather than in
        // `finish` because it is this recognizer's quirk, not a property of
        // dictated text.
        Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

/// Resolves `[asr] model` to a loaded transcriber.
///
/// The one place that knows a model can have more than one flavour;
/// everything downstream sees `Box<dyn Transcriber>` and cannot tell the
/// difference (spec §17.1). That is what keeps the guardrail, the
/// vocabulary, `finish` and injection untouched by model selection.
pub fn build(models_dir: &Path, cfg: &AsrConfig) -> Result<Box<dyn Transcriber>> {
    let spec = models::spec_for(cfg.model);
    match spec.flavor {
        AsrFlavor::Offline => Ok(Box::new(SherpaTranscriber::new(
            models_dir,
            spec.artifact.rel_path,
            cfg.num_threads,
        )?)),
        AsrFlavor::CacheAwareStreaming => Ok(Box::new(SherpaStreamingTranscriber::new(
            models_dir,
            spec.artifact.rel_path,
            cfg.num_threads,
            &cfg.language,
        )?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AsrConfig, AsrModel};

    /// `build` must fail cleanly, not panic, when the selected model is not
    /// on disk. That is an ordinary state now that only the *selected* model
    /// is ever downloaded (spec asr-model §4), and it has to reach the user
    /// as `run_utterance`'s retryable error (invariant 12), not as a crash
    /// that takes the tray down with it.
    #[test]
    fn building_a_transcriber_for_an_absent_model_is_an_error_not_a_panic() {
        let empty = std::env::temp_dir()
            .join(format!("yappr-asr-build-test-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        for model in [AsrModel::ParakeetTdtV3, AsrModel::Nemotron35] {
            let cfg = AsrConfig { model, ..AsrConfig::default() };
            assert!(
                build(&empty, &cfg).is_err(),
                "an absent {model:?} must be an Err, not a panic and not an Ok"
            );
        }
        std::fs::remove_dir_all(&empty).ok();
    }

    #[test]
    fn the_default_config_selects_the_parakeet_v3_directory() {
        let spec = crate::models::spec_for(AsrConfig::default().model);
        assert_eq!(spec.artifact.rel_path, "parakeet-tdt-0.6b-v3-int8");
        assert_eq!(spec.flavor, crate::models::AsrFlavor::Offline);
    }
}
