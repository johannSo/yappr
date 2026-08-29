use anyhow::{Context, Result};
use std::path::Path;

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

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
    pub fn new(models_dir: &Path, num_threads: i32) -> Result<Self> {
        let dir = models_dir.join("parakeet-tdt-0.6b-v3-int8");
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
