
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
    eprintln!("offline transcript: {text:?}");
    let lower = text.to_lowercase();
    assert!(!lower.trim().is_empty(), "got empty transcript");
}

/// The whole point of the CacheAwareStreaming flavour as yappr uses it: one
/// buffer in, one final result out, no partials.
///
/// Asserts on *content*, not on `Ok`-ness, deliberately. If `feature_dim`,
/// the decode loop or the language option were wrong, sherpa-onnx would
/// return confident nonsense rather than an error -- so a test that only
/// checked for `Ok` would pass against a completely broken transcriber.
/// See spec asr-model §8.
#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn the_streaming_model_transcribes_the_german_fixture_in_one_pass() {
    let samples = read_wav_16k_mono("fixtures/hallo_german.wav");
    let cfg = yappr_core::config::AsrConfig {
        model: yappr_core::config::AsrModel::Nemotron35,
        language: "de".to_string(),
        ..Default::default()
    };
    let t = yappr_core::asr::build(&yappr_core::paths::models_dir(), &cfg)
        .expect("build streaming transcriber");
    let text = t.transcribe(&samples).expect("transcribe");
    eprintln!("streaming transcript: {text:?}");

    assert!(!text.trim().is_empty(), "got empty transcript");

    // Assert the FIRST and LAST words specifically, not just "looks like
    // words". A cache-aware model silently drops the opening of an utterance
    // unless it is fed leading silence, and the end unless it is fed trailing
    // silence -- this exact fixture returned "Nur die Wurst" with neither.
    // A word-count assertion passed that happily, which is why this one names
    // the words: losing either end is invariant 1 text loss, and it is
    // invisible to any check that only counts tokens.
    let lower = text.to_lowercase();
    assert!(
        lower.contains("alles"),
        "the start of the utterance was dropped -- leading padding missing? got {text:?}"
    );
    assert!(
        lower.contains("zwei"),
        "the end of the utterance was dropped -- trailing padding missing? got {text:?}"
    );
    // This flavour joins segments with a doubled space; the offline one never
    // does. It would reach the target window verbatim on the raw-transcript
    // fallback path.
    assert!(!text.contains("  "), "doubled whitespace survived: {text:?}");
}

#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn the_streaming_transcriber_returns_empty_for_empty_input() {
    let cfg = yappr_core::config::AsrConfig {
        model: yappr_core::config::AsrModel::Nemotron35,
        ..Default::default()
    };
    let t = yappr_core::asr::build(&yappr_core::paths::models_dir(), &cfg)
        .expect("build streaming transcriber");
    assert_eq!(t.transcribe(&[]).expect("transcribe"), "");
}

/// primeline is the only catalogue entry we host ourselves, so this is also
/// the only one where a bad export would be our own doing rather than
/// k2-fsa's. It is German-only and the most accurate German model of the
/// four, so the bar is the full sentence, not merely plausible words.
#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn the_primeline_model_transcribes_the_german_fixture() {
    let samples = read_wav_16k_mono("fixtures/hallo_german.wav");
    let cfg = yappr_core::config::AsrConfig {
        model: yappr_core::config::AsrModel::ParakeetPrimelineDe,
        ..Default::default()
    };
    let t = yappr_core::asr::build(&yappr_core::paths::models_dir(), &cfg)
        .expect("build primeline transcriber");
    let text = t.transcribe(&samples).expect("transcribe");
    eprintln!("primeline transcript: {text:?}");

    let lower = text.to_lowercase();
    for word in ["alles", "ende", "wurst", "zwei"] {
        assert!(lower.contains(word), "expected {word:?} in: {text:?}");
    }
}
