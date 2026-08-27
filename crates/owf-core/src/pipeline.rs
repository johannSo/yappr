//! Orchestrates a single push-to-talk utterance end to end.
//!
//! The one invariant every stage after ASR must respect: once audio has been
//! transcribed, the user gets text. Normalization erroring, timing out, or
//! being rejected by the guardrail all degrade to the raw transcript rather
//! than losing the utterance. See spec 15.

use anyhow::Result;
use std::io::Write;
use std::time::Instant;

use crate::asr::Transcriber;
use crate::config::Config;
use crate::guardrail::{self, RejectReason, Verdict};
use crate::inject::{inject_with_fallback, TextInjector};
use crate::lang::{Lang, LanguageDetector};
use crate::normalize::Normalizer;
use crate::paths;
use crate::style;
use crate::vad::Trimmer;

#[derive(Debug, Default, Clone, Copy)]
pub struct Timings {
    pub vad_ms: u128,
    pub asr_ms: u128,
    pub normalize_ms: u128,
    pub inject_ms: u128,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// Exactly what was handed to the injector, trailing space included.
    pub text: String,
    pub raw: String,
    pub normalized: bool,
    /// The guardrail code, when a cleanup was produced and rejected. `None`
    /// both when normalization was skipped/disabled and when it errored --
    /// neither of those is a guardrail *rejection*.
    pub reject_reason: Option<String>,
    pub backend: &'static str,
    pub timings: Timings,
}

pub struct Pipeline {
    cfg: Config,
    asr: Box<dyn Transcriber>,
    trimmer: Box<dyn Trimmer>,
    detector: Box<dyn LanguageDetector>,
    normalizer: Box<dyn Normalizer>,
    injector: Box<dyn TextInjector>,
}

impl Pipeline {
    pub fn new(
        cfg: Config,
        asr: Box<dyn Transcriber>,
        trimmer: Box<dyn Trimmer>,
        detector: Box<dyn LanguageDetector>,
        normalizer: Box<dyn Normalizer>,
        injector: Box<dyn TextInjector>,
    ) -> Self {
        Self { cfg, asr, trimmer, detector, normalizer, injector }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Runs a complete utterance. `Ok(None)` means there was nothing to say
    /// and nothing was injected.
    pub fn process(&self, samples: &[f32], window_class: Option<&str>) -> Result<Option<Outcome>> {
        let mut timings = Timings::default();

        let t = Instant::now();
        let Some((start, end)) = self.trimmer.trim(samples, self.cfg.audio.vad_padding_ms) else {
            tracing::info!("no speech detected");
            return Ok(None);
        };
        timings.vad_ms = t.elapsed().as_millis();

        let t = Instant::now();
        let raw = self.asr.transcribe(&samples[start..end])?;
        timings.asr_ms = t.elapsed().as_millis();
        if raw.trim().is_empty() {
            tracing::info!("empty transcript");
            return Ok(None);
        }

        let lang = self.detector.detect(&raw);
        let axes = style::resolve(&self.cfg, window_class);
        let control = style::control_line(&axes);

        let mut normalized = false;
        let mut reject_reason: Option<String> = None;
        let mut text = guardrail::rule_based_fallback(&raw);

        if self.cfg.normalize.enabled {
            let t = Instant::now();
            match self.normalizer.normalize(&control, &raw) {
                Ok(cleaned) => match guardrail::evaluate(&raw, &cleaned, lang, &self.cfg.guardrail)
                {
                    Verdict::Accept => {
                        text = cleaned;
                        normalized = true;
                    }
                    Verdict::Reject(reason) => {
                        reject_reason = Some(reason.code().to_string());
                        self.log_rejection(&raw, &cleaned, &reason, lang, &control);
                    }
                },
                Err(e) => {
                    // A failed cleanup must never cost the transcript, and it
                    // is not a guardrail rejection: `reject_reason` stays
                    // `None` here.
                    tracing::warn!(error = %e, "normalization failed; using raw transcript");
                }
            }
            timings.normalize_ms = t.elapsed().as_millis();
        }

        if self.cfg.inject.trailing_space {
            text.push(' ');
        }

        let t = Instant::now();
        let backend = inject_with_fallback(self.injector.as_ref(), &text)?;
        timings.inject_ms = t.elapsed().as_millis();

        tracing::info!(
            ?timings,
            normalized,
            ?reject_reason,
            backend,
            "utterance complete"
        );

        Ok(Some(Outcome { text, raw, normalized, reject_reason, backend, timings }))
    }

    /// Appends one JSON line to `rejections.jsonl`, the input to threshold
    /// tuning (see spec 9.3). Failures here are diagnostics, never fatal.
    fn log_rejection(&self, raw: &str, cleaned: &str, reason: &RejectReason, lang: Lang, control: &str) {
        let record = serde_json::json!({
            "reason": reason.code(),
            "detail": format!("{reason:?}"),
            "lang": match lang { Lang::English => "English", Lang::Other => "Other" },
            "raw": raw,
            "cleaned": cleaned,
            "control": control,
        });
        let path = paths::rejections_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let write = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{record}"));
        if let Err(e) = write {
            tracing::warn!(error = %e, "failed to write rejections.jsonl");
        }
    }
}
