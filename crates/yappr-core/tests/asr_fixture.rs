use yappr_core::asr::Transcriber;

fn read_wav_16k_mono(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open fixture");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16_000, "fixture must be 16 kHz");
    assert_eq!(spec.channels, 1, "fixture must be mono");
    r.samples::<i16>()
        .map(|s| s.expect("sample") as f32 / 32768.0)
        .collect()
}

#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn transcribes_the_english_fixture() {
    let samples = read_wav_16k_mono("fixtures/hello_english.wav");
    let t = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )
    .expect("build transcriber");
    let text = t.transcribe(&samples).expect("transcribe");

    // Assert on content that must survive any reasonable ASR, not exact wording.
    // NOTE: this fixture is derived from sherpa-onnx's own bundled Parakeet test
    // audio (test_wavs/en.wav) — the JFK inaugural line "ask not what your
    // country can do for you..." — not a recording of the sentence in the
    // original task brief, so the expected substring below reflects what this
    // fixture actually contains.
    let lower = text.to_lowercase();
    assert!(!lower.trim().is_empty(), "got empty transcript");
    assert!(
        lower.contains("for your country"),
        "expected 'for your country' in: {text}"
    );
}

#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn transcribes_the_german_fixture() {
    let samples = read_wav_16k_mono("fixtures/hallo_german.wav");
    let t = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )
    .expect("build transcriber");
    let text = t.transcribe(&samples).expect("transcribe");

    // Parakeet TDT 0.6b v3 is multilingual; this just proves the pipeline
    // produces non-empty output for a second, distinct clip. Not an accuracy
    // claim (see yappr-bench and the task report for that caveat).
    let lower = text.to_lowercase();
    assert!(!lower.trim().is_empty(), "got empty transcript");
}
