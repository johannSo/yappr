//! Orchestrates a single push-to-talk utterance end to end.
//!
//! The one invariant every stage after ASR must respect: once audio has been
//! transcribed, the user gets text. Normalization erroring, timing out, or
//! being rejected by the guardrail all degrade to the raw transcript rather
//! than losing the utterance. See spec 15.

use anyhow::Result;
use std::io::Write;
use std::path::PathBuf;
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
    /// Where guardrail rejections are appended, if overridden by
    /// `with_rejections_path`. `None` (the default from `new`) means the
    /// real M3 tuning dataset, `paths::rejections_file()` -- see
    /// `rejections_path` and `log_rejection_to`.
    rejections_path: Option<PathBuf>,
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
        Self { cfg, asr, trimmer, detector, normalizer, injector, rejections_path: None }
    }

    /// Points guardrail-rejection logging at `path` instead of the real M3
    /// tuning dataset.
    ///
    /// Additive on purpose: `new`'s six positional arguments are locked in
    /// by `owf-daemon.rs`'s call site, so this is a builder rather than a
    /// seventh parameter every caller would have to update. Only tests are
    /// expected to call it -- production always takes the `None` default,
    /// which resolves to `paths::rejections_file()`.
    pub fn with_rejections_path(mut self, path: PathBuf) -> Self {
        self.rejections_path = Some(path);
        self
    }

    /// The path guardrail rejections are appended to: `paths::rejections_file()`
    /// unless overridden by `with_rejections_path`.
    fn rejections_path(&self) -> PathBuf {
        self.rejections_path.clone().unwrap_or_else(paths::rejections_file)
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
            // Scoped tightly around just the normalizer call: `evaluate` and
            // `log_rejection` are cheap and unrelated to normalizer latency,
            // which is exactly what M3 threshold tuning wants out of this
            // number.
            let t = Instant::now();
            let normalize_result = self.normalizer.normalize(&control, &raw);
            timings.normalize_ms = t.elapsed().as_millis();

            match normalize_result {
                Ok(cleaned) => match guardrail::evaluate(&raw, &cleaned, lang, &self.cfg.guardrail)
                {
                    Verdict::Accept => {
                        text = cleaned;
                        normalized = true;
                    }
                    Verdict::Reject(reason) => {
                        reject_reason = Some(reason.code().to_string());
                        log_rejection_to(&self.rejections_path(), &raw, &cleaned, &reason, lang, &control);
                    }
                },
                Err(e) => {
                    // A failed cleanup must never cost the transcript, and it
                    // is not a guardrail rejection: `reject_reason` stays
                    // `None` here. Pinned by
                    // `a_dead_normalizer_still_produces_text` in
                    // tests/pipeline_e2e.rs.
                    tracing::warn!(error = %e, "normalization failed; using raw transcript");
                }
            }
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
}

/// Appends one JSON line to `path`, the input dataset for M3 guardrail
/// threshold tuning (see spec 9.3) when `path` is the real
/// `paths::rejections_file()`. Failures here are diagnostics, never fatal.
///
/// The destination is a parameter rather than hard-coded to
/// `paths::rejections_file()` so it can be pointed at a scratch file in
/// tests: `rejections.jsonl` is live tuning input, and a synthetic fixture
/// line written to the real file on every test run would quietly poison
/// that dataset. `Pipeline::process` calls this with `self.rejections_path()`,
/// which defaults to the real path but can be redirected via
/// `Pipeline::with_rejections_path` -- as `a_rejected_cleanup_falls_back_to_raw`
/// in `tests/pipeline_e2e.rs` now does, so that test's synthetic rejection no
/// longer lands in the real dataset.
fn log_rejection_to(
    path: &std::path::Path,
    raw: &str,
    cleaned: &str,
    reason: &RejectReason,
    lang: Lang,
    control: &str,
) {
    let record = serde_json::json!({
        "reason": reason.code(),
        "detail": format!("{reason:?}"),
        "lang": match lang { Lang::English => "English", Lang::Other => "Other" },
        "raw": raw,
        "cleaned": cleaned,
        "control": control,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{record}"));
    if let Err(e) = write {
        tracing::warn!(error = %e, "failed to write rejections.jsonl");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, collision-free scratch file path for a single test. Mirrors
    /// the `scratch_dir` helper in `inject.rs`'s own tests -- `tempfile`
    /// isn't in `[dev-dependencies]`, and this is the whole of what's
    /// needed here.
    fn scratch_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("owf-core-test-{tag}-{}-{n}", std::process::id()))
            .join("rejections.jsonl")
    }

    #[test]
    fn log_rejection_to_writes_one_line_with_the_expected_fields() {
        let path = scratch_path("rejection-log");
        let reason = RejectReason::WordRatio { ratio: 0.111 };

        log_rejection_to(
            &path,
            "the raw transcript",
            "Cleaned.",
            &reason,
            Lang::English,
            "[Styling: casual] [Structure: prose] [Context: general]",
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "should append exactly one line, got: {contents:?}");

        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["reason"], "word_ratio");
        assert_eq!(parsed["raw"], "the raw transcript");
        assert_eq!(parsed["cleaned"], "Cleaned.");
        assert_eq!(parsed["lang"], "English");
        assert_eq!(
            parsed["control"],
            "[Styling: casual] [Structure: prose] [Context: general]"
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn log_rejection_to_appends_rather_than_overwriting() {
        let path = scratch_path("rejection-log-append");
        let reason = RejectReason::Empty;

        log_rejection_to(&path, "one", "", &reason, Lang::English, "control");
        log_rejection_to(&path, "two", "", &reason, Lang::Other, "control");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2, "second call should append, not overwrite");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
