//! Diagnostic: which input device actually hears anything, and does an
//! explicit ALSA buffer size stop the Xruns?
//!
//! Reports sample counts, stream-error counts, and signal level (RMS/peak)
//! only. Audio is discarded; nothing is written to disk. RMS is a loudness
//! number, not content -- ambient room noise is enough to tell a live
//! microphone from a dead digital input.
//!
//! Run: cargo run --release -p owf-core --example capture_devices

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn rms_peak(s: &[f32]) -> (f32, f32) {
    if s.is_empty() {
        return (0.0, 0.0);
    }
    let sum: f64 = s.iter().map(|v| (*v as f64) * (*v as f64)).sum();
    let peak = s.iter().fold(0.0f32, |a, v| a.max(v.abs()));
    (((sum / s.len() as f64).sqrt()) as f32, peak)
}

fn trial(device: &cpal::Device, label: &str, rate: u32, channels: u16, buffer: cpal::BufferSize, secs: f32) {
    let cfg = cpal::StreamConfig { channels, sample_rate: rate, buffer_size: buffer };
    let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    let errs = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    let (b, e, d) = (Arc::clone(&buf), Arc::clone(&errs), Arc::clone(&delivered));

    let stream = match device.build_input_stream(
        cfg,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            d.fetch_add(data.len(), Ordering::Relaxed);
            b.lock().unwrap().extend_from_slice(data);
        },
        move |_| {
            e.fetch_add(1, Ordering::Relaxed);
        },
        None,
    ) {
        Ok(s) => s,
        Err(err) => {
            println!("    {label:34} -> build FAILED: {err}");
            return;
        }
    };
    if let Err(err) = stream.play() {
        println!("    {label:34} -> play FAILED: {err}");
        return;
    }
    let t = Instant::now();
    std::thread::sleep(Duration::from_secs_f32(secs));
    let elapsed = t.elapsed().as_secs_f64();
    drop(stream);

    let got = delivered.load(Ordering::SeqCst);
    let expected = (elapsed * rate as f64 * channels as f64) as usize;
    let samples = std::mem::take(&mut *buf.lock().unwrap());
    let (r, p) = rms_peak(&samples);
    println!(
        "    {label:34} -> ratio {:.3}  xruns {:>3}   rms {:.5} peak {:.4}  {}",
        got as f64 / expected.max(1) as f64,
        errs.load(Ordering::SeqCst),
        r,
        p,
        if r < 0.00002 { "<-- SILENT (dead input?)" } else { "signal present" }
    );
}

fn main() {
    let host = cpal::default_host();
    let secs = 3.0f32;

    // Explicit single-device mode: `--device <name>`, so a specific ALSA
    // device (optionally routed by PIPEWIRE_NODE) can be tested on its own.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--device") {
        let want = args[i + 1].clone();
        let dev = if want == "default" {
            host.default_input_device()
        } else {
            host.input_devices().unwrap().find(|d| d.to_string() == want)
        };
        match dev {
            Some(dev) => {
                let cfgd = dev.default_input_config().expect("default cfg");
                println!(
                    "=== {} (PIPEWIRE_NODE={:?}) default cfg {} Hz x{}ch ===",
                    dev,
                    std::env::var("PIPEWIRE_NODE").unwrap_or_else(|_| "unset".into()),
                    cfgd.sample_rate(),
                    cfgd.channels()
                );
                trial(&dev, "BufferSize::Default", cfgd.sample_rate(), cfgd.channels(), cpal::BufferSize::Default, secs);
            }
            None => println!("device not found: {want}"),
        }
        return;
    }

    let mut devices: Vec<cpal::Device> = host.input_devices().unwrap().collect();
    // Put `default` first for reference.
    if let Some(def) = host.default_input_device() {
        devices.insert(0, def);
    }

    let mut seen = Vec::new();
    for dev in devices {
        let name = dev.to_string();
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        // Only the interesting ones: skip the many raw hw:CARD=... aliases.
        let interesting = name == "default"
            || name.contains("pipewire")
            || name.contains("pulse")
            || name.starts_with("HDA Intel")
            || name.contains("Plugable");
        if !interesting {
            continue;
        }
        let Ok(def_cfg) = dev.default_input_config() else { continue };
        let ch = def_cfg.channels();
        let rate = def_cfg.sample_rate();
        println!("\n=== {name}   (default cfg: {rate} Hz x{ch}ch) ===");
        trial(&dev, "BufferSize::Default", rate, ch, cpal::BufferSize::Default, secs);
        for frames in [512u32, 1024, 2048, 4096] {
            trial(
                &dev,
                &format!("BufferSize::Fixed({frames})"),
                rate,
                ch,
                cpal::BufferSize::Fixed(frames),
                secs,
            );
        }
    }
}
