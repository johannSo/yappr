//! Per-utterance diagnostic capture: WAV dumps and a JSON record under
//! `[debug].dir`, plus the pure helpers behind them.
//!
//! Everything here is gated by `config::DebugConfig::enabled` at the call
//! site (`pipeline::Pipeline::process_with_capture`) and, once enabled, is
//! diagnostics rather than contract: any failure writing an artifact is
//! logged and swallowed by `record_utterance`, never propagated into the
//! pipeline. Hardware-dependent paths (an actual `cpal` stream) live in
//! `crate::capture` and cannot be unit tested here or there -- everything in
//! this module is pure logic or local filesystem I/O, both of which are.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::capture::CaptureStats;
use crate::guardrail::RejectReason;
use crate::lang::Lang;
use crate::pipeline::Timings;

/// Expands a leading `~/` to the user's home directory (`dirs::home_dir()`).
/// Any other input -- an absolute path, a relative path, or a bare `~` with
/// no following slash -- is returned unchanged; only the documented `~/foo`
/// form in `[debug].dir`'s default (`"~/yappr"`) needs to work.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// A sortable, filesystem-safe timestamp for record/file names, e.g.
/// `20260827-215304-123`. Built by reformatting
/// `pipeline::rfc3339_utc`'s output rather than adding a second date/time
/// formatter to the crate.
pub fn timestamp_for_filename(now: SystemTime) -> String {
    let dur = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let rfc = crate::pipeline::rfc3339_utc(dur.as_secs()); // "2026-08-27T21:53:04Z"
    let digits: String = rfc.chars().filter(char::is_ascii_digit).collect(); // "20260827215304"
    format!("{}-{}-{:03}", &digits[..8], &digits[8..14], dur.subsec_millis())
}

/// RMS and peak of one sample buffer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AudioStats {
    pub rms: f32,
    pub peak: f32,
}

impl AudioStats {
    pub fn of(samples: &[f32]) -> Self {
        Self { rms: crate::capture::rms(samples), peak: crate::capture::peak(samples) }
    }
}

/// Raw-buffer and (if VAD found speech) trimmed-buffer stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDebug {
    pub raw: AudioStats,
    pub trimmed: Option<AudioStats>,
}

/// Capture-side facts for the debug record's `capture` section: everything
/// `CaptureStats` carries, plus the derived expectation and ratio, plus the
/// length of the buffer capture actually handed the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureDebug {
    pub device: String,
    pub native_sample_rate: u32,
    pub channels: usize,
    pub native_samples_captured: usize,
    pub native_samples_expected: usize,
    pub capture_ratio: f64,
    pub stream_errors: usize,
    pub mono_16k_samples: usize,
}

impl CaptureDebug {
    pub fn from_stats(stats: &CaptureStats, mono_16k_samples: usize) -> Self {
        let expected = crate::capture::expected_native_samples(
            stats.duration,
            stats.native_sample_rate,
            stats.channels,
        );
        Self {
            device: stats.device.clone(),
            native_sample_rate: stats.native_sample_rate,
            channels: stats.channels,
            native_samples_captured: stats.native_samples_captured,
            native_samples_expected: expected,
            capture_ratio: crate::capture::capture_ratio(stats.native_samples_captured, expected),
            stream_errors: stats.stream_errors,
            mono_16k_samples,
        }
    }
}

/// The VAD span, or the absence of one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VadDebug {
    pub found: bool,
    pub start_sample: Option<usize>,
    pub end_sample: Option<usize>,
    pub start_secs: Option<f64>,
    pub end_secs: Option<f64>,
}

impl VadDebug {
    pub fn not_found() -> Self {
        Self { found: false, start_sample: None, end_sample: None, start_secs: None, end_secs: None }
    }

    pub fn span(start: usize, end: usize) -> Self {
        let rate = crate::asr::SAMPLE_RATE as f64;
        Self {
            found: true,
            start_sample: Some(start),
            end_sample: Some(end),
            start_secs: Some(start as f64 / rate),
            end_secs: Some(end as f64 / rate),
        }
    }
}

/// What happened when normalization ran (or didn't).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizeDebug {
    pub control: String,
    pub ran: bool,
    pub cleaned: Option<String>,
    pub error: Option<String>,
}

/// The guardrail's verdict on a produced cleanup. `None` in the enclosing
/// `DebugRecord` means the guardrail was never reached at all (normalization
/// disabled, or the normalizer itself errored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailDebug {
    pub verdict: String, // "accept" | "reject"
    pub reason: Option<String>,
    pub overlap: Option<f64>,
    pub word_ratio: Option<f64>,
}

impl GuardrailDebug {
    pub fn accept() -> Self {
        Self { verdict: "accept".to_string(), reason: None, overlap: None, word_ratio: None }
    }

    /// `overlap` and `word_ratio` are recomputed directly here (mirroring
    /// `pipeline::log_rejection_to`'s own reasoning) rather than pulled out
    /// of `reason`, so every rejection carries both numbers regardless of
    /// which specific check actually tripped it.
    pub fn reject(raw: &str, cleaned: &str, reason: &RejectReason) -> Self {
        let raw_tokens = crate::guardrail::tokenize(raw);
        let clean_tokens = crate::guardrail::tokenize(cleaned);
        let overlap = crate::guardrail::overlap(&raw_tokens, &clean_tokens);
        let word_ratio = if raw_tokens.is_empty() {
            1.0
        } else {
            clean_tokens.len() as f64 / raw_tokens.len() as f64
        };
        Self {
            verdict: "reject".to_string(),
            reason: Some(reason.code().to_string()),
            overlap: Some(overlap),
            word_ratio: Some(word_ratio),
        }
    }
}

/// The injector backend used, and exactly what it was handed.
///
/// Two groups of optional fields, and they are optional for different
/// reasons. `window_class` is recorded on *every* injection and is written
/// even when it is `null`, because "the class was unknown" is the single
/// most diagnostic thing a record can say (see the field's own comment).
/// `primary_backend`/`primary_error` exist only on records where the
/// primary injector failed and the fallback carried the text, and are
/// skipped when absent so happy-path records carry no `null`s.
/// `#[serde(default)]` on all three for the same reason `vocab` has it --
/// `--debug` reads whatever record is newest on disk, which may predate
/// them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectDebug {
    pub backend: String,
    pub final_text: String,
    /// The target window's class as captured at recording start, or `null`
    /// when no provider could name it. `#[serde(default)]` so records
    /// written before this field existed still deserialize.
    ///
    /// Added because its absence made a real bug undiagnosable: the ydotool
    /// backend picks Ctrl+V vs Ctrl+Shift+V from this value, and with it
    /// missing there was no way to tell, after the fact, whether a dictation
    /// that produced no text in a terminal had been sent the wrong chord.
    /// It is also what the per-window style rules resolve against, so it is
    /// a real diagnostic under every backend.
    ///
    /// Deliberately *not* `skip_serializing_if`, unlike the two fields
    /// below: an omitted key would make "the class was unknown" byte-
    /// identical to a record written before the field existed, which is
    /// precisely the distinction it was added to draw. A written `null` is
    /// the diagnosis.
    #[serde(default)]
    pub window_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_error: Option<String>,
}

/// One utterance's full diagnostic record, written as
/// `<debug.dir>/logs/<ts>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugRecord {
    pub ts: String,
    pub capture: Option<CaptureDebug>,
    pub audio: AudioDebug,
    pub vad: VadDebug,
    pub asr_raw: Option<String>,
    /// The vocabulary corrections that fired, or `None` when none did.
    /// `#[serde(default)]` so records written before the vocabulary existed
    /// still deserialize -- `owf-ctl debug` reads whatever is newest on disk,
    /// which may well predate this field.
    #[serde(default)]
    pub vocab: Option<Vec<crate::vocab::Substitution>>,
    pub lang: Option<String>,
    pub normalize: Option<NormalizeDebug>,
    pub guardrail: Option<GuardrailDebug>,
    pub inject: Option<InjectDebug>,
    pub timings: Timings,
}

/// `Lang` as the record's `lang` field spells it -- mirrors
/// `pipeline::log_rejection_to`'s identical match rather than deriving
/// `Serialize` on `Lang` itself for one call site.
pub fn lang_str(lang: Lang) -> &'static str {
    match lang {
        Lang::English => "English",
        Lang::Other => "Other",
    }
}

/// Writes `samples` (16 kHz mono `f32`, in `[-1.0, 1.0]`) as 16-bit PCM WAV.
/// Creates `path`'s parent directory if needed.
pub fn write_wav_16k_mono(path: &Path, samples: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: crate::asr::SAMPLE_RATE as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .with_context(|| format!("creating {}", path.display()))?;
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer.write_sample(v).context("writing wav sample")?;
    }
    writer.finalize().context("finalizing wav")?;
    Ok(())
}

/// Everything `record_utterance` needs to build and write one utterance's
/// debug record. Optional fields are `None` exactly when that stage never
/// ran (VAD found no speech, ASR produced an empty transcript, ...).
pub struct DebugInput<'a> {
    pub capture: Option<&'a CaptureStats>,
    pub raw: &'a [f32],
    pub trimmed: Option<&'a [f32]>,
    pub vad: VadDebug,
    pub asr_raw: Option<&'a str>,
    pub vocab: Option<Vec<crate::vocab::Substitution>>,
    pub lang: Option<Lang>,
    pub normalize: Option<NormalizeDebug>,
    pub guardrail: Option<GuardrailDebug>,
    pub inject: Option<InjectDebug>,
    pub timings: Timings,
}

/// Builds and writes one utterance's debug artifacts under `dir`
/// (`config::DebugConfig::dir`, already `~/`-expanded): the raw and (if
/// present) trimmed WAVs when `save_audio`, and the JSON record always.
///
/// Every failure is logged via `tracing::warn!` and swallowed -- this is
/// diagnostics, not contract, and must never cost the caller an utterance.
pub fn record_utterance(dir: &Path, ts: &str, save_audio: bool, input: DebugInput) {
    if save_audio {
        let raw_path = dir.join("audio").join(format!("{ts}-raw.wav"));
        if let Err(e) = write_wav_16k_mono(&raw_path, input.raw) {
            tracing::warn!(error = %e, path = %raw_path.display(), "failed to write raw debug wav");
        }
        if let Some(trimmed) = input.trimmed {
            let trimmed_path = dir.join("audio").join(format!("{ts}-trimmed.wav"));
            if let Err(e) = write_wav_16k_mono(&trimmed_path, trimmed) {
                tracing::warn!(error = %e, path = %trimmed_path.display(), "failed to write trimmed debug wav");
            }
        }
    }

    let record = DebugRecord {
        ts: ts.to_string(),
        capture: input.capture.map(|c| CaptureDebug::from_stats(c, input.raw.len())),
        audio: AudioDebug {
            raw: AudioStats::of(input.raw),
            trimmed: input.trimmed.map(AudioStats::of),
        },
        vad: input.vad,
        asr_raw: input.asr_raw.map(str::to_string),
        vocab: input.vocab,
        lang: input.lang.map(lang_str).map(str::to_string),
        normalize: input.normalize,
        guardrail: input.guardrail,
        inject: input.inject,
        timings: input.timings,
    };

    let json_path = dir.join("logs").join(format!("{ts}.json"));
    if let Err(e) = write_json_record(&json_path, &record) {
        tracing::warn!(error = %e, path = %json_path.display(), "failed to write debug json record");
    }
}

fn write_json_record(path: &Path, record: &DebugRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(record).context("serializing debug record")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))
}

/// Picks the chronologically most recent `<ts>.json` name out of a directory
/// listing, given just the file names (`"20260827-215304-123.json"`, ...).
/// Pure so it's testable without touching the filesystem; the zero-padded,
/// fixed-width timestamp format means lexicographic order is chronological
/// order, so this is a plain max.
fn pick_latest_json_name(names: &[String]) -> Option<&String> {
    names.iter().filter(|n| n.ends_with(".json")).max()
}

/// Finds the most recently written record under `<debug.dir>/logs`.
pub fn latest_record_path(logs_dir: &Path) -> Result<PathBuf> {
    let names: Vec<String> = std::fs::read_dir(logs_dir)
        .with_context(|| format!("reading {}", logs_dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let latest = pick_latest_json_name(&names)
        .with_context(|| format!("no debug records (*.json) found in {}", logs_dir.display()))?;
    Ok(logs_dir.join(latest))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unknown_window_class_is_written_as_null_not_omitted() {
        // The whole diagnostic value of this field is telling "no provider
        // could name the focused window" apart from "this record predates
        // the field". Skipping the key when it is `None` would make those
        // two byte-identical, which is exactly the question the field was
        // added to answer.
        let rec = InjectDebug {
            backend: "script".to_string(),
            final_text: "hallo ".to_string(),
            window_class: None,
            primary_backend: None,
            primary_error: None,
        };
        let json = serde_json::to_value(&rec).unwrap();
        assert!(
            json.get("window_class").is_some(),
            "an unknown class must be written as null, got: {json}"
        );
        assert!(json["window_class"].is_null());
        // The other two stay skipped: a happy-path record carries no nulls.
        assert!(json.get("primary_error").is_none());

        // And a record written before the field existed still loads.
        let old: InjectDebug =
            serde_json::from_str(r#"{"backend":"wtype","final_text":"hi "}"#).unwrap();
        assert_eq!(old.window_class, None);
    }

    use super::*;
    use std::time::Duration;

    #[test]
    fn expand_tilde_expands_a_leading_tilde_slash() {
        let expanded = expand_tilde("~/yappr");
        assert!(expanded.is_absolute(), "got {}", expanded.display());
        assert!(expanded.ends_with("yappr"), "got {}", expanded.display());
        assert_ne!(expanded, PathBuf::from("~/yappr"));
    }

    #[test]
    fn expand_tilde_leaves_other_paths_alone() {
        assert_eq!(expand_tilde("/tmp/yappr"), PathBuf::from("/tmp/yappr"));
        assert_eq!(expand_tilde("relative/yappr"), PathBuf::from("relative/yappr"));
        // A bare `~` with no following slash is not the documented form and
        // is deliberately left unexpanded.
        assert_eq!(expand_tilde("~"), PathBuf::from("~"));
        assert_eq!(expand_tilde("~yappr"), PathBuf::from("~yappr"));
    }

    #[test]
    fn timestamp_for_filename_is_filesystem_safe_and_sorts_chronologically() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_millis(0);
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_millis(1);
        let s0 = timestamp_for_filename(t0);
        let s1 = timestamp_for_filename(t1);
        assert_eq!(s0, "19700101-000000-000");
        assert_eq!(s1, "19700101-000000-001");
        assert!(s0 < s1, "timestamps must sort chronologically as strings");
        assert!(
            s0.chars().all(|c| c.is_ascii_digit() || c == '-'),
            "must be filesystem-safe, got {s0:?}"
        );
    }

    #[test]
    fn timestamp_for_filename_matches_the_documented_example_shape() {
        // 2026-08-27T21:53:04.123Z
        let secs = 1_787_867_584u64; // 2026-08-27T21:53:04Z (verified via `date -u -d ... +%s`)
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(123);
        let s = timestamp_for_filename(t);
        assert_eq!(s, "20260827-215304-123");
    }

    #[test]
    fn audio_stats_of_computes_rms_and_peak_together() {
        let stats = AudioStats::of(&[0.5, -0.5, 0.5, -0.5]);
        assert!((stats.rms - 0.5).abs() < 1e-6);
        assert!((stats.peak - 0.5).abs() < 1e-6);
    }

    #[test]
    fn write_wav_16k_mono_round_trips_through_hound() {
        let dir = scratch_dir("wav-roundtrip");
        let path = dir.join("audio").join("test.wav");
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0, 0.25];

        write_wav_16k_mono(&path, &samples).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);

        let read_back: Vec<f32> =
            reader.samples::<i16>().map(|s| s.unwrap() as f32 / i16::MAX as f32).collect();
        assert_eq!(read_back.len(), samples.len());
        for (a, b) in samples.iter().zip(read_back.iter()) {
            assert!((a - b).abs() < 1e-3, "expected {a}, got {b}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_wav_16k_mono_clamps_out_of_range_samples() {
        let dir = scratch_dir("wav-clamp");
        let path = dir.join("audio").join("clamp.wav");
        write_wav_16k_mono(&path, &[2.0, -2.0]).unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        let read_back: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(read_back, vec![i16::MAX, -i16::MAX]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guardrail_debug_reject_computes_overlap_and_word_ratio() {
        let g = GuardrailDebug::reject(
            "please send that now",
            "please send please send please send",
            &RejectReason::Loop { ngram: "please send".into() },
        );
        assert_eq!(g.verdict, "reject");
        assert_eq!(g.reason.as_deref(), Some("loop"));
        assert_eq!(g.overlap, Some(0.5));
        assert_eq!(g.word_ratio, Some(1.5));
    }

    #[test]
    fn guardrail_debug_accept_carries_no_numbers() {
        let g = GuardrailDebug::accept();
        assert_eq!(g.verdict, "accept");
        assert_eq!(g.overlap, None);
        assert_eq!(g.word_ratio, None);
    }

    #[test]
    fn vad_debug_span_computes_seconds_from_the_16k_sample_rate() {
        let v = VadDebug::span(3_200, 15_800);
        assert_eq!(v.start_sample, Some(3_200));
        assert_eq!(v.end_sample, Some(15_800));
        assert!((v.start_secs.unwrap() - 0.2).abs() < 1e-9);
        assert!((v.end_secs.unwrap() - 0.9875).abs() < 1e-9);
    }

    #[test]
    fn vad_debug_not_found_has_no_span() {
        let v = VadDebug::not_found();
        assert!(!v.found);
        assert_eq!(v.start_sample, None);
        assert_eq!(v.end_secs, None);
    }

    #[test]
    fn debug_record_round_trips_through_json_with_stable_field_names() {
        let record = DebugRecord {
            ts: "20260827-215304-123".to_string(),
            capture: Some(CaptureDebug {
                device: "HDA Intel PCH, ALC3271 Analog".to_string(),
                native_sample_rate: 48_000,
                channels: 2,
                native_samples_captured: 240_000,
                native_samples_expected: 480_000,
                capture_ratio: 0.5,
                stream_errors: 7,
                mono_16k_samples: 40_000,
            }),
            audio: AudioDebug {
                raw: AudioStats { rms: 0.04, peak: 0.3 },
                trimmed: Some(AudioStats { rms: 0.05, peak: 0.3 }),
            },
            vad: VadDebug::span(3_200, 15_800),
            asr_raw: Some("hello there".to_string()),
            vocab: None,
            lang: Some("English".to_string()),
            normalize: Some(NormalizeDebug {
                control: "[Styling: casual] [Structure: prose] [Context: general]".to_string(),
                ran: true,
                cleaned: Some("Hello there.".to_string()),
                error: None,
            }),
            guardrail: Some(GuardrailDebug::accept()),
            inject: Some(InjectDebug {
                backend: "wtype".to_string(),
                final_text: "Hello there. ".to_string(),
                window_class: Some("kitty".to_string()),
                primary_backend: None,
                primary_error: None,
            }),
            timings: Timings { vad_ms: 1, asr_ms: 2, normalize_ms: 3, inject_ms: 4 },
        };

        let json = serde_json::to_value(&record).unwrap();
        // Pin the exact top-level and capture-nested field names: these are
        // the names a controller/operator reads the JSON by.
        assert_eq!(json["capture"]["native_samples_captured"], 240_000);
        assert_eq!(json["capture"]["native_samples_expected"], 480_000);
        assert_eq!(json["capture"]["capture_ratio"], 0.5);
        assert_eq!(json["capture"]["stream_errors"], 7);

        let round_tripped: DebugRecord = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped.ts, record.ts);
        assert_eq!(round_tripped.asr_raw, record.asr_raw);
        assert_eq!(round_tripped.timings.asr_ms, 2);
    }

    #[test]
    fn inject_debug_written_before_the_failure_fields_existed_still_deserializes() {
        // Records on disk predate primary_error -- `--debug` reads
        // whatever is newest, which may be an old record.
        let json = r#"{"backend":"clipboard","final_text":"Hallo. "}"#;
        let d: InjectDebug = serde_json::from_str(json).unwrap();
        assert_eq!(d.backend, "clipboard");
        assert!(d.primary_backend.is_none());
        assert!(d.primary_error.is_none());
    }

    #[test]
    fn pick_latest_json_name_picks_the_lexicographically_greatest() {
        let names = vec![
            "20260827-090000-000.json".to_string(),
            "20260827-215304-123.json".to_string(),
            "20260827-100000-000.json".to_string(),
            "not-a-record.txt".to_string(),
        ];
        assert_eq!(pick_latest_json_name(&names), Some(&"20260827-215304-123.json".to_string()));
    }

    #[test]
    fn pick_latest_json_name_of_an_empty_or_non_matching_list_is_none() {
        assert_eq!(pick_latest_json_name(&[]), None);
        assert_eq!(pick_latest_json_name(&["readme.md".to_string()]), None);
    }

    #[test]
    fn latest_record_path_finds_the_newest_file_on_disk() {
        let dir = scratch_dir("latest-record");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("20260827-090000-000.json"), "{}").unwrap();
        std::fs::write(dir.join("20260827-215304-123.json"), "{}").unwrap();

        let path = latest_record_path(&dir).unwrap();
        assert_eq!(path.file_name().unwrap(), "20260827-215304-123.json");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_record_path_errors_when_the_directory_has_no_records() {
        let dir = scratch_dir("latest-record-empty");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(latest_record_path(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_utterance_writes_wav_and_json_and_swallows_nothing_fatal() {
        let dir = scratch_dir("record-utterance");
        let ts = "20260827-215304-123";

        record_utterance(
            &dir,
            ts,
            true,
            DebugInput {
                capture: None,
                raw: &[0.1, 0.2, 0.3],
                trimmed: Some(&[0.2]),
                vad: VadDebug::span(1, 2),
                asr_raw: Some("hi"),
                vocab: None,
                lang: Some(Lang::English),
                normalize: None,
                guardrail: None,
                inject: Some(InjectDebug {
                    backend: "mock".to_string(),
                    final_text: "Hi. ".to_string(),
                    window_class: None,
                    primary_backend: None,
                    primary_error: None,
                }),
                timings: Timings::default(),
            },
        );

        assert!(dir.join("audio").join(format!("{ts}-raw.wav")).exists());
        assert!(dir.join("audio").join(format!("{ts}-trimmed.wav")).exists());
        let json_path = dir.join("logs").join(format!("{ts}.json"));
        assert!(json_path.exists());
        let record: DebugRecord =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(record.asr_raw.as_deref(), Some("hi"));
        assert!(record.capture.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_utterance_skips_audio_files_when_save_audio_is_false() {
        let dir = scratch_dir("record-utterance-no-audio");
        let ts = "20260827-215304-124";

        record_utterance(
            &dir,
            ts,
            false,
            DebugInput {
                capture: None,
                raw: &[0.1],
                trimmed: None,
                vad: VadDebug::not_found(),
                asr_raw: None,
                vocab: None,
                lang: None,
                normalize: None,
                guardrail: None,
                inject: None,
                timings: Timings::default(),
            },
        );

        assert!(!dir.join("audio").join(format!("{ts}-raw.wav")).exists());
        assert!(dir.join("logs").join(format!("{ts}.json")).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh, collision-free scratch directory for a single test. Mirrors
    /// the identical helper already used by `capture.rs`/`inject.rs`/
    /// `pipeline.rs`'s own tests -- `tempfile` isn't a dependency here
    /// either.
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-core-test-debug-{tag}-{}-{n}", std::process::id()))
    }
}
