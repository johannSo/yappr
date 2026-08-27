//! Long-lived process that holds the ASR/VAD/normalize models warm and
//! serves push-to-talk requests over a Unix socket.
//!
//! Wayland has no global keyboard grab, so `owf-ctl` (invoked by a Hyprland
//! keybind) writes one NDJSON request line here and reads one response line
//! back. See `owf_core::proto` for the wire format.

use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use owf_core::asr::SherpaTranscriber;
use owf_core::capture::Recorder;
use owf_core::config::Config;
use owf_core::inject;
use owf_core::lang::WhatlangDetector;
use owf_core::llama::LlamaServer;
use owf_core::normalize::{Normalizer, S1MiniClient};
use owf_core::paths;
use owf_core::pipeline::Pipeline;
use owf_core::proto::{Request, Response, State};
use owf_core::vad::SileroTrimmer;

const WARMING: u8 = 0;
const IDLE: u8 = 1;
const RECORDING: u8 = 2;
const BUSY: u8 = 3;

fn state_of(v: u8) -> State {
    match v {
        WARMING => State::Warming,
        RECORDING => State::Recording,
        BUSY => State::Transcribing,
        _ => State::Idle,
    }
}

/// Recovers the inner value from a poisoned mutex instead of panicking.
///
/// A panic on the utterance thread while `daemon.pipeline`'s lock is held
/// (see `IdleOnExit`) marks that `Mutex` poisoned even though the data it
/// guards was never left half-written -- `process_utterance` only ever reads
/// through the guard (`Pipeline::process` takes `&self`), so there is
/// nothing here to distrust. Left as `.lock().unwrap()`, that one panic would
/// turn every subsequent lock on the same mutex into its own panic, cascading
/// one bad utterance into a permanently broken daemon. Applied uniformly to
/// every lock in this file, not only `pipeline`'s, so the same cascade can't
/// happen via `llama` or `window_class` either.
fn lock_ignoring_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Daemon {
    state: AtomicU8,
    recorder: Recorder,
    pipeline: Mutex<Option<Pipeline>>,
    /// Owns the supervised `llama-server` child (R3): storing it here, rather
    /// than leaking it with `mem::forget`, keeps `LlamaServer::drop` reachable
    /// for the lifetime of the daemon instead of permanently severing it.
    /// `None` for the whole run when `[normalize].enabled = false` (R9): no
    /// child is ever spawned in that case.
    llama: Mutex<Option<LlamaServer>>,
    window_class: Mutex<Option<String>>,
    max_seconds: u32,
    /// Bumped by every `start_recording`. The 120 s safety-valve timer it
    /// spawns captures the epoch at spawn time and only acts if the epoch is
    /// still current -- otherwise a timer left over from an earlier session
    /// that already stopped (or was cancelled) could fire against a *later*
    /// recording and truncate it early. See concurrency note 3.
    recording_epoch: AtomicU64,
}

/// Never invoked: `Pipeline::process` only calls the normalizer when
/// `cfg.normalize.enabled` is true, and `warm_up` skips spawning
/// `llama-server` (and builds this stub instead of `S1MiniClient`) exactly
/// when it's false (R9). If this is ever reached, that is itself the bug --
/// report it loudly rather than silently hang trying to reach a
/// `llama-server` that was never started.
struct DisabledNormalizer;

impl Normalizer for DisabledNormalizer {
    fn normalize(&self, _control_line: &str, _raw: &str) -> Result<String> {
        anyhow::bail!("normalization is disabled (normalize.enabled = false)")
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Single-instance guard: an exclusive, non-blocking lock on a runtime
    // file, held for the life of the process via `lock` staying in scope.
    let lock_path = paths::runtime_lock();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    if !try_lock_exclusive(&lock) {
        eprintln!("openwhisprflow is already running (lock held on {})", lock_path.display());
        std::process::exit(1);
    }

    let cfg = Config::load().context("loading config")?;

    let sock_path = paths::runtime_socket();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&sock_path); // stale socket; we hold the lock
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;

    let daemon = Arc::new(Daemon {
        state: AtomicU8::new(WARMING),
        recorder: Recorder::new(&cfg.audio)?,
        pipeline: Mutex::new(None),
        llama: Mutex::new(None),
        window_class: Mutex::new(None),
        max_seconds: cfg.audio.max_seconds,
        recording_epoch: AtomicU64::new(0),
    });

    // Warm up off the accept loop so `status` answers immediately.
    {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || match warm_up(cfg) {
            Ok((pipeline, server)) => {
                *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
                *lock_ignoring_poison(&daemon.llama) = server;
                daemon.state.store(IDLE, Ordering::SeqCst);
                tracing::info!("ready");
            }
            Err(e) => tracing::error!(error = ?e, "warm-up failed; daemon stays in warming"),
        });
    }

    tracing::info!(socket = %sock_path.display(), "listening");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle(Arc::clone(&daemon), s),
            Err(e) => tracing::warn!(error = ?e, "accept failed"),
        }
    }
    Ok(())
}

/// `std::fs::File::try_lock` is a stable `flock`(2) wrapper (stabilized in
/// Rust 1.89) that needs neither an extra dependency nor an `unsafe` block on
/// this toolchain -- preferred here over `libc::flock` for exactly that
/// reason.
fn try_lock_exclusive(f: &std::fs::File) -> bool {
    match f.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(error = %e, "lock probe failed; treating as not locked");
            false
        }
    }
}

/// Builds the pipeline and, when normalization is enabled, the supervised
/// `llama-server` behind it.
///
/// R9: when `cfg.normalize.enabled` is `false`, `llama-server` is never
/// spawned at all -- not spawned-then-ignored. On this machine the system
/// `ggml` package ships no compute backend, so a real `llama-server` cannot
/// load any model; running with `[normalize] enabled = false` is how the
/// rest of the daemon (capture -> VAD -> ASR -> guardrail-fallback -> wtype)
/// is verified without it.
fn warm_up(cfg: Config) -> Result<(Pipeline, Option<LlamaServer>)> {
    let models = paths::models_dir();

    let (server, base_url) = if cfg.normalize.enabled {
        let server = LlamaServer::spawn(&cfg.normalize)?;
        server.wait_healthy(Duration::from_secs(120))?;
        let base_url = server.base_url();
        (Some(server), base_url)
    } else {
        tracing::info!("normalize.enabled = false; not spawning llama-server");
        (None, String::new())
    };

    let asr = SherpaTranscriber::new(&models, cfg.asr.num_threads)?;
    let trimmer = SileroTrimmer::new(&models)?;
    let normalizer: Box<dyn Normalizer> = if cfg.normalize.enabled {
        Box::new(S1MiniClient::new(base_url, cfg.normalize.timeout_ms))
    } else {
        Box::new(DisabledNormalizer)
    };
    let injector = inject::build(&cfg.inject);

    let pipeline = Pipeline::new(
        cfg,
        Box::new(asr),
        Box::new(trimmer),
        Box::new(WhatlangDetector),
        normalizer,
        injector,
    );
    Ok((pipeline, server))
}

fn handle(daemon: Arc<Daemon>, stream: UnixStream) {
    // The accept loop is single-threaded by design (see the note on
    // `Daemon::pipeline`'s field and `run_utterance`) and `handle` runs
    // synchronously within it, so a client that connects and never writes
    // would otherwise block *every* command -- including `status` -- inside
    // `read_line` for as long as that connection stayed open. A couple of
    // seconds is far more than a one-line NDJSON request or response needs.
    let timeout = Some(Duration::from_secs(2));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return;
    }

    let mut line = String::new();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    // A timed-out read is a plain `io::Error` (`WouldBlock`), not a panic --
    // this already drops the connection on any read failure, timeouts
    // included, rather than blocking the accept loop indefinitely.
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => dispatch(&daemon, req),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    let mut w = stream;
    let _ = writeln!(w, "{}", serde_json::to_string(&resp).unwrap_or_default());
}

/// Atomically claims the RECORDING -> BUSY transition. Both a `ptt-stop`
/// request and the 120 s safety valve can race to end the same recording;
/// only one of them may win and hand the buffer to `run_utterance`.
fn claim_busy(daemon: &Daemon) -> bool {
    daemon.state.compare_exchange(RECORDING, BUSY, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

fn dispatch(daemon: &Arc<Daemon>, req: Request) -> Response {
    let current = daemon.state.load(Ordering::SeqCst);
    match req {
        Request::Status => {
            let mut r = Response::ok(state_of(current));
            r.warm = Some(current != WARMING);
            r
        }
        Request::PttStart => match current {
            WARMING => Response::err("warming"),
            RECORDING => Response::ok(State::Recording), // idempotent
            BUSY => Response::err("busy"),
            _ => start_recording(daemon),
        },
        Request::PttStop => {
            if !claim_busy(daemon) {
                // Not recording (idle/warming), or already claimed by
                // another caller (double ptt-stop, or the safety valve won
                // the race) -- report the current state rather than acting
                // twice.
                return Response::ok(state_of(daemon.state.load(Ordering::SeqCst)));
            }
            let d = Arc::clone(daemon);
            std::thread::spawn(move || run_utterance(d));
            Response::ok(State::Transcribing)
        }
        Request::Cancel => {
            // CAS, not a snapshot-then-store: `current` was read before this
            // match, and a `ptt-stop` or the safety valve can claim the same
            // RECORDING -> BUSY transition in between. Racing an
            // unconditional `state.store(IDLE, ..)` against that would stomp
            // a real in-flight transcription back to "idle" while
            // `run_utterance` was still holding the pipeline. Only one of
            // Cancel's RECORDING -> IDLE and `claim_busy`'s RECORDING -> BUSY
            // can win.
            if daemon
                .state
                .compare_exchange(RECORDING, IDLE, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = daemon.recorder.stop();
                return Response::ok(State::Idle);
            }
            match daemon.state.load(Ordering::SeqCst) {
                // Audio has already been handed off to the pipeline; there is
                // nothing left to cancel. `run_utterance` will return the
                // state to idle on its own once it finishes.
                BUSY => Response::err("cannot cancel: transcription already in progress"),
                s => Response::ok(state_of(s)), // already idle, or still warming
            }
        }
        Request::Reload => {
            if current != IDLE {
                return Response::err("reload requires idle");
            }
            // Validates the config on disk without disturbing the running
            // pipeline -- swapping the pipeline live (new ASR/VAD models,
            // possibly a different llama-server) is out of scope here.
            match Config::load() {
                Ok(_) => Response::ok(State::Idle),
                Err(e) => Response::err(format!("config error: {e}")),
            }
        }
    }
}

fn start_recording(daemon: &Arc<Daemon>) -> Response {
    // Captured before recording so the overlay (M2) can never confuse it.
    *lock_ignoring_poison(&daemon.window_class) = owf_core::hypr::active_window_class();

    if let Err(e) = daemon.recorder.start(|_level| {}) {
        return Response::err(format!("cannot start capture: {e}"));
    }
    // Bump the epoch *before* flipping the state, not after: they are two
    // independent atomics, and the safety-valve timer below reads the epoch
    // to decide whether it still owns this recording. If the state store ran
    // first, a stale timer waking in the gap between the two stores would
    // see its own (old) epoch still current *and* `state == RECORDING`
    // (for what is actually a brand-new session), pass its epoch check, and
    // wrongly win `claim_busy` against a recording it has nothing to do
    // with. Bumping first closes that window: any timer that reads the new
    // epoch after this point also reads `state == RECORDING` from this same
    // call, never from a still-in-flight earlier one.
    let epoch = daemon.recording_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    daemon.state.store(RECORDING, Ordering::SeqCst);

    // Safety valve: a stuck key must not leave the microphone hot (spec 6.1).
    let d = Arc::clone(daemon);
    let limit = Duration::from_secs(d.max_seconds as u64);
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        // Only act if this is still the same recording session: a stale
        // timer from an earlier, already-finished session must never stop a
        // later one (concurrency note 3).
        if d.recording_epoch.load(Ordering::SeqCst) == epoch && claim_busy(&d) {
            tracing::warn!("recording hit the time limit; stopping");
            run_utterance(d);
        }
    });

    Response::ok(State::Recording)
}

/// RAII guard that returns the daemon to `IDLE` when `run_utterance` ends,
/// on *every* exit path -- including an unwinding panic from anywhere inside
/// `Pipeline::process` (ASR, VAD, the guardrail, injection, or the
/// sherpa-onnx FFI underneath ASR/VAD, which this codebase's own comments
/// flag as resting on an unverified upstream `unsafe impl Send + Sync`).
///
/// Without this, a panic on the utterance thread unwinds only that thread
/// (`panic = "unwind"` is the workspace default) and never reaches a
/// `state.store(IDLE, ..)` placed at the end of the function -- leaving the
/// daemon frozen at BUSY forever, with every later
/// `PttStart`/`PttStop`/`Cancel` hitting the busy branch.
///
/// Constructed as the very first thing in `run_utterance`, before anything
/// fallible, so its `Drop` covers every line after it. The store is
/// unconditional, not a CAS: at most one `run_utterance` (or safety-valve)
/// call is ever in flight at a time -- the RECORDING -> BUSY transition it
/// exits is claimed exactly once per recording, by whichever of `ptt-stop`
/// or the safety valve won `claim_busy` -- so there is no other legitimate
/// writer for this guard to race or clobber.
struct IdleOnExit<'a>(&'a AtomicU8);

impl Drop for IdleOnExit<'_> {
    fn drop(&mut self) {
        self.0.store(IDLE, Ordering::SeqCst);
    }
}

fn run_utterance(daemon: Arc<Daemon>) {
    let _idle_on_exit = IdleOnExit(&daemon.state);

    let samples = match daemon.recorder.stop() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "capture stop failed");
            return;
        }
    };
    let class = lock_ignoring_poison(&daemon.window_class).clone();
    process_utterance(&daemon.pipeline, &samples, class.as_deref());
}

/// Runs one utterance's already-captured samples through the pipeline.
///
/// Split out from `run_utterance` so it is testable without a real
/// `Recorder` (which needs live audio hardware to construct -- see
/// `capture::Recorder::new`): a test can drive this directly with a scratch
/// `Mutex<Option<Pipeline>>` and a panicking fake stage to prove
/// `IdleOnExit` recovers `state` even when this function unwinds.
fn process_utterance(pipeline: &Mutex<Option<Pipeline>>, samples: &[f32], window_class: Option<&str>) {
    // `pipeline` is a `Mutex<Option<Pipeline>>` shared by the whole daemon,
    // and the guard is held for the entire `process` call below. That is
    // deliberate, not incidental: `SherpaTranscriber` wraps sherpa-onnx's
    // `OfflineRecognizer`, which the sherpa-onnx crate marks `Send + Sync`
    // via a bare `unsafe impl` rather than a documented thread-safety
    // guarantee, and `create_stream`/`decode` take `&self` straight into
    // FFI. Only one utterance may ever be inside `process` at a time; this
    // lock is what guarantees that, so nothing here may clone the pipeline
    // out from under the guard or call `process` without holding it.
    let guard = lock_ignoring_poison(pipeline);
    if let Some(p) = guard.as_ref() {
        match p.process(samples, window_class) {
            Ok(Some(out)) => tracing::info!(chars = out.text.len(), "injected"),
            Ok(None) => tracing::info!("nothing to inject"),
            Err(e) => tracing::error!(error = ?e, "pipeline failed"),
        }
    } else {
        // Unreachable in normal operation: reaching RECORDING/BUSY requires
        // having passed through IDLE, which is only set once warm-up has
        // populated `pipeline`. Logged rather than unwrapped so a future
        // change to that invariant fails loudly instead of panicking.
        tracing::warn!("utterance finished but the pipeline was not ready");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owf_core::asr::Transcriber;
    use owf_core::config::Config;
    use owf_core::inject::MockInjector;
    use owf_core::lang::{Lang, LanguageDetector};
    use owf_core::normalize::Normalizer;
    use owf_core::vad::Trimmer;
    use std::panic::AssertUnwindSafe;

    struct WholeBuffer;
    impl Trimmer for WholeBuffer {
        fn trim(&self, s: &[f32], _: u32) -> Option<(usize, usize)> {
            if s.is_empty() {
                None
            } else {
                Some((0, s.len()))
            }
        }
    }

    /// The panic seam: stands in for a real ASR/VAD/guardrail/inject failure,
    /// or the sherpa-onnx FFI underneath ASR/VAD -- any of which could panic
    /// on the real utterance thread per fix 1's report.
    struct PanickingTranscriber;
    impl Transcriber for PanickingTranscriber {
        fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
            panic!("boom: transcriber panicked mid-utterance");
        }
    }

    struct AlwaysEnglish;
    impl LanguageDetector for AlwaysEnglish {
        fn detect(&self, _: &str) -> Lang {
            Lang::English
        }
    }

    /// Never reached: `PanickingTranscriber::transcribe` panics before
    /// `process_utterance` gets anywhere near normalization.
    struct NeverNormalizer;
    impl Normalizer for NeverNormalizer {
        fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
            unreachable!("normalization must be unreachable: the panic happens before it")
        }
    }

    /// Fix 1: proves the daemon's state returns to `IDLE` even when a
    /// pipeline stage panics, instead of staying stuck at `BUSY` forever.
    ///
    /// Exercises the real `IdleOnExit` guard and the real `process_utterance`
    /// (the part of `run_utterance` that actually runs the pipeline), wired
    /// together the same way `run_utterance` wires them -- guard constructed
    /// first, fallible work run inside its scope -- so this is the same
    /// unwind path a panic on the real utterance thread would take. It stops
    /// short of a full `Daemon`/`run_utterance` because `Daemon` embeds a
    /// real `capture::Recorder`, which needs live audio hardware just to
    /// construct and so can't run in this sandbox or CI.
    #[test]
    fn a_panic_inside_the_pipeline_still_returns_the_daemon_to_idle() {
        let pipeline = Mutex::new(Some(Pipeline::new(
            Config::from_str("").unwrap(),
            Box::new(PanickingTranscriber),
            Box::new(WholeBuffer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )));
        let state = AtomicU8::new(BUSY);
        let samples = vec![0.1f32; 16_000];

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _idle_on_exit = IdleOnExit(&state);
            process_utterance(&pipeline, &samples, None);
        }));

        assert!(result.is_err(), "the transcriber's panic should have propagated");
        assert_eq!(
            state.load(Ordering::SeqCst),
            IDLE,
            "a panic inside the pipeline must not leave the daemon stuck at BUSY"
        );
    }
}
