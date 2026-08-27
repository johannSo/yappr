//! Orchestrates a single push-to-talk utterance end to end.
//!
//! The one invariant every stage after ASR must respect: once audio has been
//! transcribed, the user gets text. Normalization erroring, timing out, or
//! being rejected by the guardrail all degrade to the raw transcript rather
//! than losing the utterance. See spec 15.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use crate::asr::Transcriber;
use crate::capture::CaptureStats;
use crate::config::Config;
use crate::debug;
use crate::guardrail::{self, RejectReason, Verdict};
use crate::inject::{inject_with_fallback, TextInjector};
use crate::lang::{Lang, LanguageDetector};
use crate::normalize::Normalizer;
use crate::paths;
use crate::style;
use crate::vad::Trimmer;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
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

    /// Applies a freshly-loaded [`Config`] and a rebuilt injector to an
    /// already-warmed-up pipeline, for `owf-ctl reload` (spec 6).
    ///
    /// I5: deliberately narrow. `[guardrail]`, `[inject]`, `[style_default]`,
    /// and `[style_rules]` are pure per-utterance data this struct reads
    /// straight off `self.cfg`/`self.injector` on every call, so swapping
    /// both here is enough to make an edited guardrail threshold or style
    /// rule take effect immediately -- which is the whole point of `reload`
    /// existing. `asr`, the VAD/ASR models, and the `llama-server` connection
    /// underneath `self.normalizer` are untouched: those require a restart
    /// (see `owf-daemon.rs`'s `Reload` handler), exactly as the comment this
    /// replaces already said -- the bug was applying that restriction to
    /// *everything* in `Config`, including the fields that need no restart.
    pub fn update_reloadable(&mut self, cfg: Config, injector: Box<dyn TextInjector>) {
        self.cfg = cfg;
        self.injector = injector;
    }

    /// Runs a complete utterance. `Ok(None)` means there was nothing to say
    /// and nothing was injected.
    ///
    /// A thin wrapper around [`Pipeline::process_with_capture`] for the many
    /// callers (every test in this crate, plus any future caller that has no
    /// `CaptureStats` to hand) that don't have -- or don't care about --
    /// capture-side debug info. Real production use goes through
    /// `process_with_capture` instead, via `owf-daemon.rs`'s
    /// `process_utterance`.
    pub fn process(&self, samples: &[f32], window_class: Option<&str>) -> Result<Option<Outcome>> {
        self.process_with_capture(samples, window_class, None)
    }

    /// Like [`Pipeline::process`], but additionally takes the capture-side
    /// facts behind `samples` (device, native rate, samples actually
    /// delivered, stream error count, ...) so that, when `[debug].enabled`,
    /// the per-utterance debug record's `capture` section can be populated.
    ///
    /// `capture` is `None` whenever the caller has no `CaptureStats` for
    /// this buffer (every test in this crate, and `process`'s own callers);
    /// the debug record is still written in that case, just without a
    /// `capture` section -- see `debug::DebugRecord::capture`.
    pub fn process_with_capture(
        &self,
        samples: &[f32],
        window_class: Option<&str>,
        capture: Option<CaptureStats>,
    ) -> Result<Option<Outcome>> {
        let mut timings = Timings::default();
        // `None` when `[debug].enabled` is false -- the common case -- so
        // that recording an utterance costs this function nothing beyond the
        // one bool check: no timestamp formatted, no path expanded, no
        // record built or written.
        let debug_ctx: Option<(String, std::path::PathBuf)> = self.cfg.debug.enabled.then(|| {
            (debug::timestamp_for_filename(SystemTime::now()), debug::expand_tilde(&self.cfg.debug.dir))
        });

        let t = Instant::now();
        let Some((start, end)) = self.trimmer.trim(samples, self.cfg.audio.vad_padding_ms) else {
            tracing::info!("no speech detected");
            if let Some((ts, dir)) = &debug_ctx {
                debug::record_utterance(
                    dir,
                    ts,
                    self.cfg.debug.save_audio,
                    debug::DebugInput {
                        capture: capture.as_ref(),
                        raw: samples,
                        trimmed: None,
                        vad: debug::VadDebug::not_found(),
                        asr_raw: None,
                        lang: None,
                        normalize: None,
                        guardrail: None,
                        inject: None,
                        timings,
                    },
                );
            }
            return Ok(None);
        };
        timings.vad_ms = t.elapsed().as_millis();
        let vad_debug = debug::VadDebug::span(start, end);

        let t = Instant::now();
        let raw = self.asr.transcribe(&samples[start..end])?;
        timings.asr_ms = t.elapsed().as_millis();
        if raw.trim().is_empty() {
            tracing::info!("empty transcript");
            if let Some((ts, dir)) = &debug_ctx {
                debug::record_utterance(
                    dir,
                    ts,
                    self.cfg.debug.save_audio,
                    debug::DebugInput {
                        capture: capture.as_ref(),
                        raw: samples,
                        trimmed: Some(&samples[start..end]),
                        vad: vad_debug,
                        asr_raw: Some(&raw),
                        lang: None,
                        normalize: None,
                        guardrail: None,
                        inject: None,
                        timings,
                    },
                );
            }
            return Ok(None);
        }

        let lang = self.detector.detect(&raw);
        let axes = style::resolve(&self.cfg, window_class);
        let control = style::control_line(&axes);

        let mut normalized = false;
        let mut reject_reason: Option<String> = None;
        let mut text = guardrail::rule_based_fallback(&raw);
        let mut normalize_debug = debug::NormalizeDebug {
            control: control.clone(),
            ran: false,
            cleaned: None,
            error: None,
        };
        let mut guardrail_debug: Option<debug::GuardrailDebug> = None;

        if self.cfg.normalize.enabled {
            // Scoped tightly around just the normalizer call: `evaluate` and
            // `log_rejection` are cheap and unrelated to normalizer latency,
            // which is exactly what M3 threshold tuning wants out of this
            // number.
            let t = Instant::now();
            let normalize_result = self.normalizer.normalize(&control, &raw);
            timings.normalize_ms = t.elapsed().as_millis();
            normalize_debug.ran = true;

            match normalize_result {
                Ok(cleaned) => {
                    normalize_debug.cleaned = Some(cleaned.clone());
                    match guardrail::evaluate(&raw, &cleaned, lang, &self.cfg.guardrail) {
                        Verdict::Accept => {
                            guardrail_debug = Some(debug::GuardrailDebug::accept());
                            text = cleaned;
                            normalized = true;
                        }
                        Verdict::Reject(reason) => {
                            guardrail_debug =
                                Some(debug::GuardrailDebug::reject(&raw, &cleaned, &reason));
                            reject_reason = Some(reason.code().to_string());
                            log_rejection_to(&self.rejections_path(), &raw, &cleaned, &reason, lang, &control);
                        }
                    }
                }
                Err(e) => {
                    // A failed cleanup must never cost the transcript, and it
                    // is not a guardrail rejection: `reject_reason` stays
                    // `None` here. Pinned by
                    // `a_dead_normalizer_still_produces_text` in
                    // tests/pipeline_e2e.rs.
                    normalize_debug.error = Some(e.to_string());
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

        if let Some((ts, dir)) = &debug_ctx {
            debug::record_utterance(
                dir,
                ts,
                self.cfg.debug.save_audio,
                debug::DebugInput {
                    capture: capture.as_ref(),
                    raw: samples,
                    trimmed: Some(&samples[start..end]),
                    vad: vad_debug,
                    asr_raw: Some(&raw),
                    lang: Some(lang),
                    normalize: Some(normalize_debug),
                    guardrail: guardrail_debug,
                    inject: Some(debug::InjectDebug {
                        backend: backend.to_string(),
                        final_text: text.clone(),
                    }),
                    timings,
                },
            );
        }

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
    // Spec 9.3 pins `ts` (RFC 3339) and a numeric `overlap` field in every
    // record. Neither was present before ("Also fix" item): `ts` was missing
    // outright, and the overlap score was only reachable -- for the one
    // reject reason that is actually an overlap rejection -- by parsing it
    // back out of the `detail` Debug string below. Recomputing it directly
    // via `guardrail::overlap` instead means *every* rejection carries a
    // real, parseable overlap number, not just the ones rejected for low
    // overlap specifically -- which is what M3 tuning, the sole consumer of
    // this file, actually needs to correlate overlap against every reason.
    let overlap = guardrail::overlap(&guardrail::tokenize(raw), &guardrail::tokenize(cleaned));

    let record = serde_json::json!({
        "ts": rfc3339_now(),
        "reason": reason.code(),
        "detail": format!("{reason:?}"),
        "lang": match lang { Lang::English => "English", Lang::Other => "Other" },
        "raw": raw,
        "cleaned": cleaned,
        "overlap": overlap,
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

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_utc(secs)
}

/// Formats whole seconds since the Unix epoch as an RFC 3339 UTC timestamp
/// with second precision, e.g. `0` -> `"1970-01-01T00:00:00Z"`.
///
/// Hand-rolled (Howard Hinnant's well-known `civil_from_days` algorithm)
/// rather than pulling in `chrono` or `time` as a direct dependency for this
/// one call site -- this is the entire extent of the calendar math
/// `rejections.jsonl` needs. Takes a plain integer specifically so it's
/// testable without touching the wall clock.
///
/// `pub(crate)` (not private) so `crate::debug::timestamp_for_filename` can
/// reuse this same calendar math for its filesystem-safe timestamps instead
/// of adding a second date/time formatter to the crate.
pub(crate) fn rfc3339_utc(secs_since_epoch: u64) -> String {
    let days = (secs_since_epoch / 86_400) as i64;
    let rem = secs_since_epoch % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days: days since 1970-01-01 -> (year, month, day). All
    // intermediate values are non-negative for any date on or after the
    // epoch, so plain (truncating) integer division agrees with floor
    // division throughout -- no negative-operand pitfalls to worry about
    // here.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_utc_formats_the_unix_epoch() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_utc_formats_a_leap_day() {
        // 2024-02-29T12:34:56Z
        assert_eq!(rfc3339_utc(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn rfc3339_utc_formats_a_recent_timestamp() {
        // 2026-08-27T18:04:11Z -- spec 9.3's own example record.
        assert_eq!(rfc3339_utc(1_787_853_851), "2026-08-27T18:04:11Z");
    }

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
        // Spec 9.3: "the meeting is at 4:30" (3 raw tokens) vs "Cleaned."
        // (1 cleaned token, disjoint) -> 0 of 3 raw tokens survive.
        assert_eq!(parsed["overlap"], 0.0, "overlap must be a plain numeric field, got: {parsed}");
        let ts = parsed["ts"].as_str().expect("ts must be a string");
        assert_eq!(ts.len(), "2026-08-27T18:04:11Z".len(), "ts should be RFC 3339, got: {ts}");
        assert!(ts.ends_with('Z'), "ts should be RFC 3339 UTC (trailing 'Z'), got: {ts}");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn log_rejection_to_computes_overlap_regardless_of_the_reject_reason() {
        let path = scratch_path("rejection-log-overlap");
        // A Loop rejection, not an Overlap rejection -- before this fix, only
        // an actual Overlap-reason rejection carried the score at all (and
        // only inside an unparseable `{reason:?}` Debug string). "please" and
        // "send" both survive from 4 raw tokens into the (looping) cleaned
        // text -> overlap = 2/4 = 0.5.
        log_rejection_to(
            &path,
            "please send that now",
            "please send please send please send",
            &RejectReason::Loop { ngram: "please send".into() },
            Lang::English,
            "control",
        );

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(contents.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["reason"], "loop");
        assert_eq!(
            parsed["overlap"], 0.5,
            "overlap must be computed for every rejection, not just Overlap-reason ones"
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
