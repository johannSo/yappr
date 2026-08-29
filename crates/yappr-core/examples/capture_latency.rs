//! Diagnostic: how long does the microphone take to start delivering samples?
//!
//! Measures the three boundaries on the ptt-start critical path:
//!   1. `Recorder::new`  -- device enumeration + the 16 kHz buildability probe
//!   2. `Recorder::start` -- `build_input_stream` + `play()`
//!   3. start() -> first audio callback -- the gap in which speech is LOST
//!
//! Opens the microphone for well under a second per device and discards every
//! sample: it prints timings and sample *counts* only, never audio, and writes
//! nothing to disk.
//!
//! Run: cargo run --release -p yappr-core --example capture_latency

use yappr_core::capture::Recorder;
use yappr_core::config::{AudioConfig, Config};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn probe(label: &str, cfg: &AudioConfig) {
    println!("\n=== {label}  (device = {:?}) ===", cfg.device);

    let t = Instant::now();
    let recorder = match Recorder::new(cfg) {
        Ok(r) => r,
        Err(e) => {
            println!("  Recorder::new FAILED after {:?}: {e}", t.elapsed());
            return;
        }
    };
    println!("  1. Recorder::new (enumerate + 16k probe) : {:>7.1} ms", ms(t.elapsed()));

    // Optional idle gap between construction and the first start(), which is
    // exactly the daemon's situation: it builds the Recorder once at warm-up
    // and then sits idle until the user presses the key. With
    // snd_hda_intel's power_save enabled the codec runtime-suspends during
    // that gap, and the first stream open pays to wake it.
    let idle = std::env::args()
        .position(|a| a == "--idle-secs")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if idle > 0 {
        println!("     ... idling {idle} s so the codec can runtime-suspend (pm: {})", pm_status());
        std::thread::sleep(Duration::from_secs(idle));
        println!("     ... idle over, codec pm state now: {}", pm_status());
    }

    let first: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let callbacks = Arc::new(AtomicUsize::new(0));
    let first_cb = Arc::clone(&first);
    let cb_count = Arc::clone(&callbacks);

    let busy = std::env::args().any(|a| a == "--busy-callback");
    let (work, _rxs) = DaemonLikeWork::new(if busy { 1 } else { 0 });
    let t_start = Instant::now();
    if let Err(e) = recorder.start(move |level| {
        cb_count.fetch_add(1, Ordering::Relaxed);
        if busy {
            work.on_level(level);
        }
        let mut slot = first_cb.lock().unwrap();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }) {
        println!("  Recorder::start FAILED after {:?}: {e}", t_start.elapsed());
        return;
    }
    let start_returned = t_start.elapsed();
    println!("  2. start() [build_input_stream + play]   : {:>7.1} ms", ms(start_returned));

    // Wait for the first callback, polling rather than sleeping a fixed time.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut first_at = None;
    while Instant::now() < deadline {
        if let Some(at) = *first.lock().unwrap() {
            first_at = Some(at);
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }

    match first_at {
        Some(at) => println!(
            "  3. start() -> FIRST SAMPLES              : {:>7.1} ms   <-- speech before this is lost",
            ms(at.duration_since(t_start))
        ),
        None => println!("  3. start() -> FIRST SAMPLES              :  NEVER (3 s timeout)"),
    }

    // Let audio flow for the configured hold, then discard it.
    std::thread::sleep(Duration::from_millis(hold_ms()));
    let t_stop = Instant::now();
    match recorder.stop() {
        Ok(out) => println!(
            "  4. stop() [drop + downmix + resample]    : {:>7.1} ms  ({} samples @16k discarded, {} callbacks, {} stream errors, native {} Hz x{}ch)",
            ms(t_stop.elapsed()),
            out.samples.len(),
            callbacks.load(Ordering::Relaxed),
            out.capture.stream_errors,
            out.capture.native_sample_rate,
            out.capture.channels,
        ),
        Err(e) => println!("  4. stop() FAILED: {e}"),
    }

    // Second start on the SAME recorder: is the cost per-keypress or one-off?
    let first2: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let first2_cb = Arc::clone(&first2);
    let t2 = Instant::now();
    if recorder
        .start(move |_| {
            let mut slot = first2_cb.lock().unwrap();
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
        })
        .is_ok()
    {
        let ret2 = t2.elapsed();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut at2 = None;
        while Instant::now() < deadline {
            if let Some(at) = *first2.lock().unwrap() {
                at2 = Some(at);
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        println!(
            "  5. SECOND start() (warm): return {:>6.1} ms, first samples {}",
            ms(ret2),
            match at2 {
                Some(at) => format!("{:>7.1} ms", ms(at.duration_since(t2))),
                None => "NEVER".to_string(),
            }
        );
        let _ = recorder.stop();
    }
}

/// Runtime-PM status of the PCI sound controller, so the log says plainly
/// whether the codec was suspended when the stream was opened.
fn pm_status() -> String {
    for entry in glob_sound_devices() {
        if let Ok(v) = std::fs::read_to_string(entry.join("power/runtime_status")) {
            return v.trim().to_string();
        }
    }
    "unknown".to_string()
}

fn glob_sound_devices() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/bus/pci/devices") {
        for e in rd.flatten() {
            if e.path().join("sound").is_dir() {
                out.push(e.path());
            }
        }
    }
    out
}

fn hold_ms() -> u64 {
    std::env::args()
        .position(|a| a == "--hold-ms")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Mimics the work `owf-daemon` does *inside* the cpal audio callback:
/// `should_emit_level`'s mutex, then `Daemon::broadcast`'s subscribers mutex
/// plus one `mpsc::Sender::send` per subscriber. Passing `--busy-callback`
/// runs that same shape here so the Xrun question can be answered without a
/// microphone in the room.
struct DaemonLikeWork {
    last_emit: Mutex<Option<Instant>>,
    subscribers: Mutex<Vec<std::sync::mpsc::Sender<String>>>,
}

impl DaemonLikeWork {
    fn new(n_subscribers: usize) -> (Arc<Self>, Vec<std::sync::mpsc::Receiver<String>>) {
        let mut txs = Vec::new();
        let mut rxs = Vec::new();
        for _ in 0..n_subscribers {
            let (tx, rx) = std::sync::mpsc::channel();
            txs.push(tx);
            rxs.push(rx);
        }
        (
            Arc::new(Self { last_emit: Mutex::new(None), subscribers: Mutex::new(txs) }),
            rxs,
        )
    }

    fn on_level(&self, level: f32) {
        let now = Instant::now();
        let mut last = self.last_emit.lock().unwrap();
        let due = last.map(|t| now.duration_since(t) >= Duration::from_millis(50)).unwrap_or(true);
        if due {
            *last = Some(now);
            let payload = format!("{{\"event\":\"recording\",\"level\":{level},\"elapsed_ms\":0}}");
            let subs = self.subscribers.lock().unwrap();
            for tx in subs.iter() {
                let _ = tx.send(payload.clone());
            }
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    let configured = Config::load().map(|c| c.audio).unwrap_or_default();
    println!("Microphone start-up latency probe -- timings only, all audio discarded.");

    probe("CONFIGURED DEVICE", &configured);

    if configured.device != "default" {
        let mut d = configured.clone();
        d.device = "default".into();
        probe("PIPEWIRE 'default' FOR COMPARISON", &d);
    }
}
