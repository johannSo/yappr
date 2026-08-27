use anyhow::{ensure, Result};
use owf_core::asr::{SherpaTranscriber, Transcriber};
use std::time::Instant;

fn read_wav_16k_mono(path: &str) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    ensure!(
        spec.sample_rate == 16_000 && spec.channels == 1,
        "bench input must be 16 kHz mono, got {} Hz / {} channel(s)",
        spec.sample_rate,
        spec.channels
    );
    r.samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0).map_err(Into::into))
        .collect()
}

/// Repeats `samples` back-to-back `times` times.
///
/// The M0 latency budget in spec §17.2 is written for a ~10 s utterance, but
/// the fixture clips derived from Parakeet's bundled test audio are only a
/// few seconds long (see task-3-report.md for why: no microphone/human is
/// available to record a real 10 s take). Concatenating a short clip gives a
/// buffer of realistic length to exercise ASR latency at something close to
/// the spec's budget. It is not meant to produce coherent speech, and is
/// only used for timing — not for the accuracy-relevant assertions in
/// asr_fixture.rs.
fn repeat_buffer(samples: &[f32], times: usize) -> Vec<f32> {
    samples
        .iter()
        .copied()
        .cycle()
        .take(samples.len() * times)
        .collect()
}

struct Measurement {
    audio_secs: f64,
    asr_ms: u128,
    text: String,
}

fn measure(asr: &dyn Transcriber, samples: &[f32]) -> Result<Measurement> {
    let audio_secs = samples.len() as f64 / owf_core::asr::SAMPLE_RATE as f64;
    // First pass warms ONNX Runtime's internal allocations; report the second.
    let _ = asr.transcribe(samples)?;
    let t0 = Instant::now();
    let text = asr.transcribe(samples)?;
    let asr_ms = t0.elapsed().as_millis();
    Ok(Measurement {
        audio_secs,
        asr_ms,
        text,
    })
}

fn print_measurement(label: &str, m: &Measurement) {
    println!("== {label} ==");
    println!("audio        {:.2} s", m.audio_secs);
    println!("asr (warm)   {} ms", m.asr_ms);
    println!("asr RTF      {:.3}", m.asr_ms as f64 / 1000.0 / m.audio_secs);
    println!("transcript   {}", m.text);
    println!();
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "crates/owf-core/fixtures/hello_english.wav".to_string());

    let short_samples = read_wav_16k_mono(&path)?;
    // ~3.85 s fixture x3 -> ~11.5 s, close to the spec's 10 s budget.
    let long_samples = repeat_buffer(&short_samples, 3);

    let t0 = Instant::now();
    let asr = SherpaTranscriber::new(&owf_core::paths::models_dir(), 4)?;
    let load_ms = t0.elapsed().as_millis();
    println!("model load   {load_ms} ms  (once, at daemon start)");
    println!();

    print_measurement("single clip", &measure(&asr, &short_samples)?);
    print_measurement("~10s buffer (clip x3)", &measure(&asr, &long_samples)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_buffer_concatenates_without_overlap() {
        let samples = vec![1.0, 2.0, 3.0];
        let out = repeat_buffer(&samples, 3);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn repeat_buffer_of_zero_times_is_empty() {
        assert!(repeat_buffer(&[1.0, 2.0], 0).is_empty());
    }
}
