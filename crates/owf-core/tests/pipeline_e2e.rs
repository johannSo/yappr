use owf_core::asr::Transcriber;
use owf_core::config::Config;
use owf_core::inject::MockInjector;
use owf_core::lang::{Lang, LanguageDetector};
use owf_core::normalize::Normalizer;
use owf_core::pipeline::Pipeline;
use owf_core::vad::Trimmer;

struct FixedAsr(String);
impl Transcriber for FixedAsr {
    fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
        Ok(self.0.clone())
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
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("the quarterly numbers came in higher than we forecast".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Numbers.".into())), // far too short
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason.as_deref(), Some("word_ratio"));
    assert!(out.text.starts_with("The quarterly numbers"), "got {:?}", out.text);
    assert!(out.text.trim_end().ends_with('.'));
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
}

#[test]
fn disabling_normalization_skips_it_entirely() {
    let p = Pipeline::new(
        Config::from_str("[normalize]\nenabled = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(BrokenNormalizer), // would error if it were called
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason, None, "skipping is not a rejection");
}

#[test]
fn no_speech_injects_nothing() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("should never be reached".into())),
        Box::new(NoSpeechTrimmer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(MockInjector::default()),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
}

#[test]
fn an_empty_transcript_injects_nothing() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("   ".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(MockInjector::default()),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
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
