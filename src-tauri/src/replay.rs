//! Drives the overlay from a checked-in NDJSON `OverlayEvent` log instead of
//! the daemon socket -- the acceptance evidence for every overlay state
//! without a microphone (M2 plan, Task 5). Enabled with `--replay <path>`.
//!
//! The fixture at `src-tauri/fixtures/replay-full.ndjson` exercises every
//! variant in `wire::OverlayEvent`, including a realistic `recording` burst
//! with varying levels.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::Duration;

use crate::wire::OverlayEvent;

/// How long to hold each event on screen before advancing to the next line,
/// chosen per event kind: state-transition events get long enough to
/// comfortably screenshot by hand or script; `Recording` events get a short
/// hold so a run of them reads as a live ~50ms-cadence meter rather than a
/// slideshow (spec 7.1's real cadence).
fn hold_for(event: &OverlayEvent) -> Duration {
    match event {
        OverlayEvent::Recording { .. } => Duration::from_millis(150),
        OverlayEvent::BusyRejected => Duration::from_millis(1_200),
        OverlayEvent::Error { .. } => Duration::from_millis(2_500),
        OverlayEvent::Done { .. } => Duration::from_millis(1_500),
        OverlayEvent::Warming
        | OverlayEvent::Idle
        | OverlayEvent::Transcribing
        | OverlayEvent::Normalizing
        | OverlayEvent::Injecting => Duration::from_millis(1_800),
    }
}

/// Runs forever: plays every line of `path` in order, holding each on
/// screen per `hold_for`, then loops back to the start. Looping (rather
/// than exiting after one pass) means the process can be left running while
/// a screenshot script captures each state at its own pace.
pub fn run(path: &Path, on_event: impl Fn(OverlayEvent) + Send + 'static) -> ! {
    let events = load(path).unwrap_or_else(|e| {
        eprintln!("overlay: failed to read replay file {}: {e}", path.display());
        std::process::exit(1);
    });
    if events.is_empty() {
        eprintln!("overlay: replay file {} contains no valid events", path.display());
        std::process::exit(1);
    }
    loop {
        for event in &events {
            on_event(event.clone());
            std::thread::sleep(hold_for(event));
        }
    }
}

/// Parses one `OverlayEvent` per non-blank, non-`#`-comment line. A
/// malformed line is reported to stderr and skipped rather than aborting
/// the whole replay -- consistent with `connection.rs`'s treatment of a
/// bad line from the real daemon.
fn load(path: &Path) -> std::io::Result<Vec<OverlayEvent>> {
    let file = std::fs::File::open(path)?;
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<OverlayEvent>(trimmed) {
            Ok(event) => events.push(event),
            Err(e) => eprintln!("overlay: skipping malformed replay line ({e}): {trimmed:?}"),
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn scratch_file(tag: &str, contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("owf-overlay-replay-test-{tag}-{}-{n}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn loads_every_event_kind_in_order() {
        let path = scratch_file(
            "all-kinds",
            "{\"event\":\"warming\"}\n\
             # a comment line, ignored\n\
             \n\
             {\"event\":\"recording\",\"level\":0.1,\"elapsed_ms\":50}\n\
             {\"event\":\"done\",\"preview\":\"hi\"}\n",
        );
        let events = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            events,
            vec![
                OverlayEvent::Warming,
                OverlayEvent::Recording { level: 0.1, elapsed_ms: 50 },
                OverlayEvent::Done { preview: "hi".to_string() },
            ]
        );
    }

    #[test]
    fn skips_malformed_lines_without_failing_the_whole_file() {
        let path = scratch_file(
            "malformed",
            "{\"event\":\"warming\"}\n\
             not json at all\n\
             {\"event\":\"idle\"}\n",
        );
        let events = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(events, vec![OverlayEvent::Warming, OverlayEvent::Idle]);
    }

    #[test]
    fn an_empty_file_loads_as_zero_events() {
        let path = scratch_file("empty", "");
        let events = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(events.is_empty());
    }

    #[test]
    fn checked_in_fixture_covers_every_event_kind() {
        // Guards against the fixture rotting silently: every OverlayEvent
        // variant must appear in the checked-in replay log at least once.
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/replay-full.ndjson");
        let events = load(&fixture).expect("fixture must parse");
        assert!(!events.is_empty(), "fixture must contain events");

        let has = |pred: &dyn Fn(&OverlayEvent) -> bool| events.iter().any(pred);
        assert!(has(&|e| matches!(e, OverlayEvent::Warming)), "missing warming");
        assert!(has(&|e| matches!(e, OverlayEvent::Idle)), "missing idle");
        assert!(has(&|e| matches!(e, OverlayEvent::Recording { .. })), "missing recording");
        assert!(has(&|e| matches!(e, OverlayEvent::Transcribing)), "missing transcribing");
        assert!(has(&|e| matches!(e, OverlayEvent::Normalizing)), "missing normalizing");
        assert!(has(&|e| matches!(e, OverlayEvent::Injecting)), "missing injecting");
        assert!(has(&|e| matches!(e, OverlayEvent::Done { .. })), "missing done");
        assert!(has(&|e| matches!(e, OverlayEvent::Error { .. })), "missing error");
        assert!(has(&|e| matches!(e, OverlayEvent::BusyRejected)), "missing busy_rejected");

        // The recording burst should have more than a couple of samples
        // and varying levels, or it wouldn't be "realistic" per the
        // acceptance criteria.
        let levels: Vec<f32> = events
            .iter()
            .filter_map(|e| match e {
                OverlayEvent::Recording { level, .. } => Some(*level),
                _ => None,
            })
            .collect();
        assert!(levels.len() >= 10, "recording burst should have a realistic number of samples");
        let min = levels.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = levels.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(max - min > 0.05, "recording burst levels should actually vary");
    }
}
