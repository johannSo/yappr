use owf_core::asr::Transcriber;
use owf_core::capture::CaptureStats;
use owf_core::config::Config;
use owf_core::inject::MockInjector;
use owf_core::lang::{Lang, LanguageDetector};
use owf_core::normalize::Normalizer;
use owf_core::pipeline::Pipeline;
use owf_core::proto::OverlayEvent;
use owf_core::vad::Trimmer;

struct FixedAsr(String);
impl Transcriber for FixedAsr {
    fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

/// Always errors, never producing a transcript -- used to prove the debug
/// record is still written when `self.asr.transcribe(...)?` is the return
/// path (`debug_record_is_written_when_asr_errors`).
struct FailingAsr;
impl Transcriber for FailingAsr {
    fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
        anyhow::bail!("asr exploded")
    }
}

struct WholeBuffer;
impl Trimmer for WholeBuffer {
    fn trim(&self, s: &[f32], _: u32) -> Option<(usize, usize)> {
        if s.is_empty() { None } else { Some((0, s.len())) }
    }
}

struct NoSpeechTrimmer;
impl Trimmer for NoSpeechTrimmer {
    fn trim(&self, _: &[f32], _: u32) -> Option<(usize, usize)> {
        None
    }
}

struct AlwaysEnglish;
impl LanguageDetector for AlwaysEnglish {
    fn detect(&self, _: &str) -> Lang {
        Lang::English
    }
}

struct FixedNormalizer(String);
impl Normalizer for FixedNormalizer {
    fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

struct BrokenNormalizer;
impl Normalizer for BrokenNormalizer {
    fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
        anyhow::bail!("llama-server is down")
    }
}

/// Used only by `disabling_normalization_skips_it_entirely`, in place of
/// `BrokenNormalizer`. `Pipeline::process` swallows a normalizer *error*
/// into exactly the same `normalized == false` / `reject_reason == None`
/// outcome a correctly *skipped* normalizer produces (see
/// `a_dead_normalizer_still_produces_text`) -- so `BrokenNormalizer` here
/// asserted the same thing whether or not `[normalize].enabled = false` was
/// actually honoured. A panic can't be silently swallowed the same way: if a
/// regression ever lets the pipeline call the normalizer despite
/// `enabled = false`, this fails loudly instead of passing for the wrong
/// reason.
struct PanickingNormalizer;
impl Normalizer for PanickingNormalizer {
    fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
        panic!("normalizer must never be called when normalize.enabled = false");
    }
}

/// Forwards to a shared `Arc<MockInjector>` so a test can inspect what was
/// injected after `Pipeline::new` (or `Pipeline::update_reloadable`) has
/// taken ownership of the `Box<dyn TextInjector>`. Mirrors the `Fwd`/
/// `SpyNormalizer` pattern `the_window_class_selects_the_control_line`
/// already uses on the normalizer side.
struct FwdInjector(std::sync::Arc<MockInjector>);
impl owf_core::inject::TextInjector for FwdInjector {
    fn inject(&self, text: &str) -> Result<(), owf_core::inject::InjectError> {
        self.0.inject(text)
    }
    fn name(&self) -> &'static str {
        self.0.name()
    }
}

/// Captures the control line the normalizer was handed.
struct SpyNormalizer(std::sync::Mutex<Option<String>>);
impl Normalizer for SpyNormalizer {
    fn normalize(&self, control: &str, raw: &str) -> anyhow::Result<String> {
        *self.0.lock().unwrap() = Some(control.to_string());
        Ok(raw.to_string())
    }
}

fn samples() -> Vec<f32> {
    vec![0.1; 16_000]
}

/// A fresh, collision-free scratch path for a single test's rejections log.
/// Mirrors the identical helper pattern in `owf-core`'s own `pipeline.rs`
/// and `inject.rs` unit tests -- `tempfile` isn't a dependency here either.
fn scratch_rejections_path(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("owf-core-test-pipeline-e2e-{tag}-{}-{n}", std::process::id()))
        .join("rejections.jsonl")
}

/// A fresh, collision-free scratch directory for a single test's
/// `[debug].dir`. Same pattern as `scratch_rejections_path` above.
fn scratch_debug_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("owf-core-test-pipeline-e2e-debug-{tag}-{}-{n}", std::process::id()))
}

fn debug_config(dir: &std::path::Path, save_audio: bool) -> Config {
    Config::from_str(&format!(
        "[debug]\nenabled = true\ndir = \"{}\"\nsave_audio = {save_audio}\n",
        dir.display(),
    ))
    .unwrap()
}

fn capture_stats_fixture() -> CaptureStats {
    CaptureStats {
        device: "HDA Intel PCH, ALC3271 Analog".to_string(),
        native_sample_rate: 48_000,
        channels: 2,
        native_samples_captured: 240_000,
        stream_errors: 3,
        duration: std::time::Duration::from_secs(5),
    }
}

/// The single highest-value diagnostic this whole facility exists for: with
/// `[debug].enabled = true` and real `CaptureStats` handed in via
/// `process_with_capture`, the JSON record's `capture` section carries the
/// captured/expected native sample counts, their ratio, and the stream
/// error count -- exactly what distinguishes "ALSA dropped audio" from
/// "VAD over-trimmed" as the cause of a single-word transcript.
#[test]
fn debug_capture_records_capture_ratio_and_stream_errors_when_enabled() {
    let dir = scratch_debug_dir("capture-ratio");
    let p = Pipeline::new(
        debug_config(&dir, true),
        Box::new(FixedAsr("um so the meeting is at uh four thirty on tuesday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("So the meeting is at 4:30 on Tuesday.".into())),
        Box::new(MockInjector::default()),
    );

    let out = p
        .process_with_capture(&samples(), None, Some(capture_stats_fixture()))
        .unwrap()
        .expect("some outcome");
    assert!(out.normalized);

    let logs_dir = dir.join("logs");
    let json_path = owf_core::debug::latest_record_path(&logs_dir).unwrap();
    let record: owf_core::debug::DebugRecord =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();

    let capture = record.capture.expect("capture section must be present");
    assert_eq!(capture.native_samples_captured, 240_000);
    assert_eq!(capture.native_samples_expected, 480_000); // 5s * 48kHz * 2ch
    assert!((capture.capture_ratio - 0.5).abs() < 1e-9);
    assert_eq!(capture.stream_errors, 3);
    assert_eq!(capture.device, "HDA Intel PCH, ALC3271 Analog");

    assert_eq!(record.asr_raw.as_deref(), Some("um so the meeting is at uh four thirty on tuesday"));
    assert_eq!(record.lang.as_deref(), Some("English"));
    assert!(record.audio.trimmed.is_some());
    let guardrail = record.guardrail.expect("guardrail should have run");
    assert_eq!(guardrail.verdict, "accept");
    let inject = record.inject.expect("inject should have run");
    assert_eq!(inject.backend, "mock");
    assert_eq!(inject.final_text, out.text);

    assert!(dir.join("audio").join(format!("{}-raw.wav", record.ts)).exists());
    assert!(dir.join("audio").join(format!("{}-trimmed.wav", record.ts)).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_capture_omits_the_capture_section_when_process_is_called_without_stats() {
    let dir = scratch_debug_dir("no-capture-stats");
    let p = Pipeline::new(
        debug_config(&dir, false),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    );

    // The plain two-arg `process` (what every other test in this file
    // calls) has no `CaptureStats` to offer -- the record must still be
    // written, just without a `capture` section.
    p.process(&samples(), None).unwrap();

    let logs_dir = dir.join("logs");
    let json_path = owf_core::debug::latest_record_path(&logs_dir).unwrap();
    let record: owf_core::debug::DebugRecord =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert!(record.capture.is_none());

    // save_audio was false: no WAVs, only the JSON record.
    assert!(!dir.join("audio").join(format!("{}-raw.wav", record.ts)).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_capture_records_no_speech_utterances_too() {
    let dir = scratch_debug_dir("no-speech");
    let p = Pipeline::new(
        debug_config(&dir, true),
        Box::new(FixedAsr("should never be reached".into())),
        Box::new(NoSpeechTrimmer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(MockInjector::default()),
    );

    assert!(p.process(&samples(), None).unwrap().is_none());

    let logs_dir = dir.join("logs");
    let json_path = owf_core::debug::latest_record_path(&logs_dir).unwrap();
    let record: owf_core::debug::DebugRecord =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert!(!record.vad.found);
    assert!(record.asr_raw.is_none());
    // The raw buffer is still worth having even when VAD found nothing --
    // that's exactly the buffer to inspect for "did VAD over-trim?".
    assert!(dir.join("audio").join(format!("{}-raw.wav", record.ts)).exists());
    assert!(!dir.join("audio").join(format!("{}-trimmed.wav", record.ts)).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn debug_capture_is_a_no_op_when_disabled() {
    let dir = scratch_debug_dir("disabled");
    // enabled = false (the default), pointed at a scratch dir that must
    // never be created.
    let cfg = Config::from_str(&format!("[debug]\ndir = \"{}\"\n", dir.display())).unwrap();
    let p = Pipeline::new(
        cfg,
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    );

    p.process(&samples(), None).unwrap();

    assert!(!dir.exists(), "no debug directory should be created when [debug].enabled = false");
}

/// Structural fix: `self.asr.transcribe(...)?` used to skip the debug write
/// entirely on error, even with `[debug].enabled = true`. VAD had already
/// run by then, so the record must still capture that much.
#[test]
fn debug_record_is_written_when_asr_errors() {
    let dir = scratch_debug_dir("asr-error");
    let p = Pipeline::new(
        debug_config(&dir, false),
        Box::new(FailingAsr),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("unused".into())),
        Box::new(MockInjector::default()),
    );

    let err = p.process(&samples(), None).unwrap_err();
    assert!(err.to_string().contains("asr exploded"), "got {err}");

    let logs_dir = dir.join("logs");
    let json_path = owf_core::debug::latest_record_path(&logs_dir)
        .expect("a debug record must be written even when ASR errors");
    let record: owf_core::debug::DebugRecord =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert!(record.vad.found, "VAD had already run before ASR errored");
    assert!(record.asr_raw.is_none(), "ASR never produced a transcript");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Structural fix, second instance: `inject_with_fallback(...)?` used to skip
/// the debug write when *both* the primary and clipboard-fallback injectors
/// failed -- found by review, not disclosed by the implementer. By then the
/// raw transcript, the normalization result, and the guardrail verdict are
/// all already computed; this is exactly the utterance you'd most want a
/// record for.
///
/// Uses `with_fallback_injector`/`with_recovery_dir` (added alongside this
/// fix) so the test can force the fallback path deterministically instead of
/// depending on whether the real `wl-copy` happens to be installed, and
/// without writing to the real `paths::state_dir()`.
#[test]
fn debug_record_is_written_when_both_injectors_fail() {
    let dir = scratch_debug_dir("both-injectors-fail");
    let recovery_dir = scratch_debug_dir("both-injectors-fail-recovery");
    let p = Pipeline::new(
        debug_config(&dir, false),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::failing()),
    )
    .with_fallback_injector(Box::new(MockInjector::failing()))
    .with_recovery_dir(recovery_dir.clone());

    let err = p.process(&samples(), None).unwrap_err();
    assert!(err.to_string().contains("unsent.txt"), "got {err}");

    let logs_dir = dir.join("logs");
    let json_path = owf_core::debug::latest_record_path(&logs_dir)
        .expect("a debug record must be written even when both injectors fail");
    let record: owf_core::debug::DebugRecord =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(record.asr_raw.as_deref(), Some("send the invoice on friday"));
    let normalize = record.normalize.expect("normalization should have already run");
    assert_eq!(normalize.cleaned.as_deref(), Some("Send the invoice on Friday."));
    let guardrail = record.guardrail.expect("the guardrail should have already reached a verdict");
    assert_eq!(guardrail.verdict, "accept");
    assert!(record.inject.is_none(), "injection never completed");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&recovery_dir);
}

#[test]
fn a_good_cleanup_is_injected() {
    let injector = MockInjector::default();
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("um so the meeting is at uh four thirty on tuesday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("So the meeting is at 4:30 on Tuesday.".into())),
        Box::new(injector),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(out.normalized);
    assert_eq!(out.text, "So the meeting is at 4:30 on Tuesday. ");
    assert!(out.reject_reason.is_none());
}

#[test]
fn a_rejected_cleanup_falls_back_to_raw() {
    // Guardrail rejections are the input dataset for M3 threshold tuning
    // (see `owf_core::pipeline::log_rejection_to`); this test's synthetic
    // rejection must land in a scratch file, never the real
    // `rejections.jsonl`, or every CI run would quietly poison that data.
    let rejections_path = scratch_rejections_path("rejected-cleanup-falls-back-to-raw");

    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("the quarterly numbers came in higher than we forecast".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Numbers.".into())), // far too short
        Box::new(MockInjector::default()),
    )
    .with_rejections_path(rejections_path.clone());

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason.as_deref(), Some("word_ratio"));
    assert!(out.text.starts_with("The quarterly numbers"), "got {:?}", out.text);
    assert!(out.text.trim_end().ends_with('.'));

    let contents = std::fs::read_to_string(&rejections_path)
        .expect("the rejection should have been logged to the scratch path");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), 1, "should log exactly one rejection, got: {contents:?}");
    let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(parsed["reason"], "word_ratio");
    assert_eq!(parsed["raw"], "the quarterly numbers came in higher than we forecast");

    let _ = std::fs::remove_dir_all(rejections_path.parent().unwrap());
}

#[test]
fn a_dead_normalizer_still_produces_text() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(BrokenNormalizer),
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.text, "Send the invoice on friday. ");
    // A normalizer *error* is not a guardrail *rejection* -- no cleanup was
    // ever produced for the guardrail to evaluate, so there is nothing to
    // reject. A regression that set reject_reason here would still pass
    // every other assertion in this file.
    assert_eq!(out.reject_reason, None);
}

#[test]
fn disabling_normalization_skips_it_entirely() {
    let p = Pipeline::new(
        Config::from_str("[normalize]\nenabled = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(PanickingNormalizer), // must never be called
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason, None, "skipping is not a rejection");
}

#[test]
fn no_speech_injects_nothing() {
    let injector = std::sync::Arc::new(MockInjector::default());
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("should never be reached".into())),
        Box::new(NoSpeechTrimmer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(FwdInjector(injector.clone())),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
    assert!(
        injector.injected().is_empty(),
        "no speech means nothing should ever reach the injector"
    );
}

#[test]
fn an_empty_transcript_injects_nothing() {
    let injector = std::sync::Arc::new(MockInjector::default());
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("   ".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(FwdInjector(injector.clone())),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
    assert!(
        injector.injected().is_empty(),
        "an empty transcript means nothing should ever reach the injector"
    );
}

#[test]
fn the_injector_receives_exactly_what_the_outcome_reports() {
    // Spec 16 calls for a `MockInjector` capturing injected text for
    // full-pipeline assertions; nothing exercised that before this fix, even
    // though `Outcome.text`'s doc comment claims it is "exactly what was
    // handed to the injector".
    let injector = std::sync::Arc::new(MockInjector::default());
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(FwdInjector(injector.clone())),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");

    assert_eq!(
        injector.injected(),
        vec![out.text.clone()],
        "the injector must receive exactly what Outcome.text reports, once, and nothing else"
    );
}

#[test]
fn the_window_class_selects_the_control_line() {
    let spy = std::sync::Arc::new(SpyNormalizer(std::sync::Mutex::new(None)));

    struct Fwd(std::sync::Arc<SpyNormalizer>);
    impl Normalizer for Fwd {
        fn normalize(&self, c: &str, r: &str) -> anyhow::Result<String> {
            self.0.normalize(c, r)
        }
    }

    let p = Pipeline::new(
        Config::from_str(
            r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            styling = "semi-formal"
            context = "email"
            "#,
        )
        .unwrap(),
        Box::new(FixedAsr("please find the invoice attached below".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(Fwd(spy.clone())),
        Box::new(MockInjector::default()),
    );

    p.process(&samples(), Some("thunderbird")).unwrap();
    assert_eq!(
        spy.0.lock().unwrap().as_deref(),
        Some("[Styling: semi-formal] [Structure: prose] [Context: email]")
    );
}

#[test]
fn trailing_space_can_be_disabled() {
    let p = Pipeline::new(
        Config::from_str("[inject]\ntrailing_space = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    );
    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert_eq!(out.text, "Send the invoice on Friday.");
}

#[test]
fn update_reloadable_applies_a_new_guardrail_and_a_new_injector() {
    // I5: `owf-ctl reload` used to parse the config on disk, discard it, and
    // report bare success -- a user editing a guardrail threshold and
    // reloading got `{"ok":true}` and no actual change. This proves
    // `Pipeline::update_reloadable` (what `owf-daemon.rs`'s `Reload` handler
    // now calls) really does change guardrail behaviour without rebuilding
    // the pipeline.
    let strict = Config::from_str("").unwrap(); // default thresholds
    let old_injector = std::sync::Arc::new(MockInjector::default());
    let mut p = Pipeline::new(
        strict,
        Box::new(FixedAsr("the quarterly numbers came in higher than we forecast".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Numbers.".into())), // far too short under the default ratio
        Box::new(FwdInjector(old_injector.clone())),
    )
    .with_rejections_path(scratch_rejections_path("reload-strict"));

    let before = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!before.normalized, "the default guardrail should reject this cleanup");
    assert_eq!(old_injector.injected().len(), 1);

    // Reload with a guardrail permissive enough to accept the same cleanup,
    // and a brand-new injector -- both must take effect on the very next
    // utterance, with no restart.
    let permissive =
        Config::from_str("[guardrail]\nmin_word_ratio = 0.0\nmin_overlap_english = 0.0\n").unwrap();
    let new_injector = std::sync::Arc::new(MockInjector::default());
    p.update_reloadable(permissive, Box::new(FwdInjector(new_injector.clone())));

    let after = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(after.normalized, "the reloaded, permissive guardrail should accept this cleanup");
    assert!(old_injector.injected().len() == 1, "the old injector must not be used again after reload");
    assert_eq!(
        new_injector.injected(),
        vec![after.text],
        "the reloaded injector must receive the post-reload utterance"
    );
}

/// M2 Task 1: a subscriber (here, a plain `Vec` behind a `Mutex` standing in
/// for the daemon's socket fan-out -- `owf-daemon.rs`'s own tests cover the
/// actual `Request::Subscribe` wiring) must see `Normalizing` before
/// `Injecting`, in that order, for a synthetic utterance driven through the
/// existing fake `Transcriber`/`Normalizer`/`Trimmer`/`MockInjector`.
#[test]
fn stage_events_fire_normalizing_then_injecting_in_order() {
    let seen: std::sync::Arc<std::sync::Mutex<Vec<OverlayEvent>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_for_sink = seen.clone();

    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    )
    .with_stage_events(std::sync::Arc::new(move |ev| {
        seen_for_sink.lock().unwrap().push(ev);
    }));

    p.process(&samples(), None).unwrap().expect("some outcome");

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [OverlayEvent::Normalizing, OverlayEvent::Injecting],
        "Normalizing must be reported before Injecting, in pipeline order"
    );
}

/// When normalization is disabled, `Pipeline` never enters that stage at
/// all -- the subscriber must see only `Injecting`, not a `Normalizing`
/// event for a stage nothing actually ran.
#[test]
fn stage_events_omit_normalizing_when_normalization_is_disabled() {
    let seen: std::sync::Arc<std::sync::Mutex<Vec<OverlayEvent>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_for_sink = seen.clone();

    let p = Pipeline::new(
        Config::from_str("[normalize]\nenabled = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(PanickingNormalizer),
        Box::new(MockInjector::default()),
    )
    .with_stage_events(std::sync::Arc::new(move |ev| {
        seen_for_sink.lock().unwrap().push(ev);
    }));

    p.process(&samples(), None).unwrap().expect("some outcome");

    assert_eq!(seen.lock().unwrap().as_slice(), [OverlayEvent::Injecting]);
}

/// `with_stage_events`'s sink is a plain, infallible `Fn(OverlayEvent)`:
/// `Pipeline` never inspects a return value and never depends on the sink
/// doing anything in particular. That's what lets the daemon's real sink
/// (`Daemon::broadcast`, via `broadcast_to`) silently drop a disconnected
/// subscriber -- by design, see `owf-daemon.rs`'s own tests -- without that
/// choice ever being able to reach back into the utterance itself. This
/// just pins the call count/outcome so a future change can't quietly make
/// the pipeline's result depend on the sink.
#[test]
fn the_stage_events_sink_cannot_influence_the_pipeline_outcome() {
    struct CountingSink(std::sync::atomic::AtomicUsize);
    let calls = std::sync::Arc::new(CountingSink(std::sync::atomic::AtomicUsize::new(0)));
    let calls_for_sink = calls.clone();

    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    )
    .with_stage_events(std::sync::Arc::new(move |_ev| {
        calls_for_sink.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(out.normalized);
    assert_eq!(calls.0.load(std::sync::atomic::Ordering::SeqCst), 2, "Normalizing + Injecting");
}
