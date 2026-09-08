//! Ad-hoc: transcribe wav files given on the command line and print raw ASR output.
//! Usage: cargo run --release -p yappr-core --example transcribe_file -- a.wav b.wav

fn read_wav_16k_mono(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16_000, "wav must be 16 kHz: {path}");
    assert_eq!(spec.channels, 1, "wav must be mono: {path}");
    match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .map(|s| s.expect("sample") as f32 / 32768.0)
            .collect(),
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.expect("sample")).collect(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let t = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )
    .expect("build transcriber");
    for p in &args {
        let samples = read_wav_16k_mono(p);
        let text = t.transcribe(&samples).expect("transcribe");
        println!("=== {p}\n{text}\n");
    }
}
