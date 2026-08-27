//! Long-lived process that holds the ASR/VAD/normalize models warm and
//! serves push-to-talk requests over a Unix socket.
//!
//! Wayland has no global keyboard grab, so `owf-ctl` (invoked by a Hyprland
//! keybind) writes one NDJSON request line here and reads one response line
//! back. See `owf_core::proto` for the wire format.

use anyhow::{Context, Result};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use owf_core::asr::SherpaTranscriber;
use owf_core::capture::{CaptureStats, Recorder};
use owf_core::config::{AudioConfig, Config, DebugConfig};
use owf_core::inject;
use owf_core::lang::WhatlangDetector;
use owf_core::llama::LlamaServer;
use owf_core::normalize::{Normalizer, S1MiniClient};
use owf_core::paths;
use owf_core::pipeline::Pipeline;
use owf_core::proto::{OverlayEvent, Request, Response, State};
use owf_core::vad::SileroTrimmer;

const WARMING: u8 = 0;
const IDLE: u8 = 1;
const RECORDING: u8 = 2;
// Spec 6.1 / 12: the daemon used to collapse everything from `ptt-stop`
// onward into one `BUSY` value, so `status` (and the overlay, once it
// existed) could never tell "still transcribing" from "stuck normalizing"
// from "about to inject" -- exactly the distinction spec 12 calls a real
// requirement, not decoration, because the round trip is seconds long and
// the stage label is the only way to tell a slow pipeline from a hung one.
// Three explicit sub-states replace it; `is_busy` is the "was BUSY" check
// every former `== BUSY` comparison becomes.
const TRANSCRIBING: u8 = 3;
const NORMALIZING: u8 = 4;
const INJECTING: u8 = 5;

fn is_busy(v: u8) -> bool {
    matches!(v, TRANSCRIBING | NORMALIZING | INJECTING)
}

fn state_of(v: u8) -> State {
    match v {
        WARMING => State::Warming,
        RECORDING => State::Recording,
        TRANSCRIBING => State::Transcribing,
        NORMALIZING => State::Normalizing,
        INJECTING => State::Injecting,
        _ => State::Idle,
    }
}

/// The `OverlayEvent` a just-connected subscriber is sent immediately, as a
/// snapshot of whatever state the daemon is in right now -- see
/// `serve_subscriber`. Mirrors `state_of` exactly, except `Recording`, whose
/// real `level`/`elapsed_ms` this function has no way to know for a
/// recording already in progress; `0.0`/`0` is corrected within one capture
/// callback (spec 7.1's ~50 ms cadence) by the next live `Recording` event.
fn snapshot_event(v: u8) -> OverlayEvent {
    match v {
        WARMING => OverlayEvent::Warming,
        RECORDING => OverlayEvent::Recording { level: 0.0, elapsed_ms: 0 },
        TRANSCRIBING => OverlayEvent::Transcribing,
        NORMALIZING => OverlayEvent::Normalizing,
        INJECTING => OverlayEvent::Injecting,
        _ => OverlayEvent::Idle,
    }
}

/// Spec 7.1's RMS cadence: `Recording` events are emitted at roughly this
/// interval, not on every `cpal` audio callback (~10 ms at 48 kHz, ~100/s --
/// far more than the overlay needs or a Unix-socket fan-out should carry).
const LEVEL_EMIT_INTERVAL: Duration = Duration::from_millis(50);

/// Decides whether enough time has passed since the last emitted `Recording`
/// event to emit another one. Pure and free of any audio/socket types
/// specifically so it's unit-testable without opening a real capture device
/// -- see `start_recording`'s `on_level` closure, the one real caller, and
/// this file's `#[cfg(test)]` module for the tests that exercise it.
fn should_emit_level(last_emitted: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last_emitted {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= interval,
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
    /// `None` until a `Recorder` has been successfully constructed -- either
    /// eagerly at startup, or lazily on the first `ptt-start` after a
    /// startup where it wasn't (I7). See `ensure_recorder`.
    recorder: Mutex<Option<Recorder>>,
    /// Kept so `ensure_recorder` can retry `Recorder::new` later with the
    /// same settings, independent of how many times it has already failed.
    audio_cfg: AudioConfig,
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
    /// One `Sender` per currently-connected `Request::Subscribe` client; see
    /// `broadcast_to` and `serve_subscriber`. A plain `Vec` behind a `Mutex`
    /// is enough: `send` never blocks (`mpsc` channels are unbounded), so
    /// broadcasting can never stall the caller on a slow or dead subscriber,
    /// and a subscriber whose `Receiver` has been dropped (its thread
    /// exited, because its connection closed) is pruned the next time
    /// something is broadcast.
    subscribers: Mutex<Vec<mpsc::Sender<OverlayEvent>>>,
}

/// Sends `event` to every registered subscriber, dropping any whose
/// receiving end has gone away (the client disconnected, or its thread hit a
/// write error and exited -- see `serve_subscriber`).
///
/// A free function taking `&Mutex<Vec<...>>` rather than a `&Daemon` method
/// so `IdleOnExit` (which only ever has the two fields it actually touches,
/// not a whole `Daemon`) and this module's tests can use it without
/// constructing a full `Daemon` -- which would need a real `Recorder`, and
/// thus live audio hardware, just to exist.
fn broadcast_to(subscribers: &Mutex<Vec<mpsc::Sender<OverlayEvent>>>, event: OverlayEvent) {
    let mut subs = lock_ignoring_poison(subscribers);
    subs.retain(|tx| tx.send(event.clone()).is_ok());
}

impl Daemon {
    fn broadcast(&self, event: OverlayEvent) {
        broadcast_to(&self.subscribers, event);
    }
}

/// Stands in for `S1MiniClient` whenever no working `llama-server` backs
/// normalization for this run.
///
/// Two distinct situations reach this, both correctly modelled the same way:
/// `[normalize].enabled = false` (no child was ever spawned at all), or
/// `enabled = true` but `warm_up` couldn't get `llama-server` spawned and
/// healthy (C1) -- a missing ggml compute backend, a missing binary, a port
/// conflict outside the retry range, or any other startup failure.
/// `Pipeline::process` already treats a normalizer *error* exactly like a
/// `llama-server` that goes down or times out mid-utterance: log it and fall
/// back to the raw transcript plus the rule-based pass (spec 15). Routing
/// both "never had one" and "tried and failed" through that same, already-
/// correct degraded path is what lets the daemon come up `Idle` and useful
/// in either case, rather than the previous behaviour of `warm_up` returning
/// `Err` on the second case and leaving the daemon stuck in `Warming`
/// forever.
struct UnavailableNormalizer(String);

impl Normalizer for UnavailableNormalizer {
    fn normalize(&self, _control_line: &str, _raw: &str) -> Result<String> {
        anyhow::bail!("normalization unavailable: {}", self.0)
    }
}

/// Sets up `tracing`: always to stdout, and additionally (append mode) to
/// `<debug.dir>/logs/daemon.log` when `[debug].enabled` -- `paths::log_file()`
/// lives under the unrelated XDG state dir and was never wired up to
/// anything, and per-utterance debug records already live under
/// `debug.dir`, so keeping the daemon's own log alongside them there (rather
/// than reaching for `paths::log_file()`'s separate location) keeps every
/// diagnostic artifact from one debug-enabled run in the same place.
///
/// Deliberately synchronous: `fmt::Layer::with_writer` just needs something
/// `io::Write`, and a `Mutex<File>` (`tracing_subscriber::fmt::MakeWriter`
/// is implemented for any `Mutex<W: Write>`) is exactly that -- no
/// background flushing thread or async runtime required, unlike
/// `tracing-appender`'s non-blocking writer.
fn init_tracing(debug: &DebugConfig) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = if debug.enabled {
        match open_debug_log_file(debug) {
            Ok(file) => Some(fmt::layer().with_ansi(false).with_writer(Mutex::new(file))),
            Err(e) => {
                eprintln!("warning: could not open debug log file: {e}");
                None
            }
        }
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(env_filter())
        .with(fmt::layer())
        .with(file_layer)
        .init();
}

/// Opens `<debug.dir>/logs/daemon.log` for append, creating the directory
/// tree if needed.
fn open_debug_log_file(debug: &DebugConfig) -> std::io::Result<std::fs::File> {
    let logs_dir = owf_core::debug::expand_tilde(&debug.dir).join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    std::fs::OpenOptions::new().create(true).append(true).open(logs_dir.join("daemon.log"))
}

fn main() -> Result<()> {
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

    // Loaded before tracing is set up so the log destination (stdout, plus
    // `<debug.dir>/logs/daemon.log` when debug capture is enabled) can
    // depend on `cfg.debug`. Nothing above this point ever logs via
    // `tracing`, so nothing is lost by the reordering.
    init_tracing(&cfg.debug);

    let sock_path = paths::runtime_socket();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&sock_path); // stale socket; we hold the lock
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    // I6: the socket would otherwise sit at whatever the umask leaves it --
    // world-writable-ish under the /tmp fallback `paths::runtime_socket`
    // uses when `$XDG_RUNTIME_DIR` is unset (mode 1777), which would let any
    // local user connect and send `ptt-start`, turning on this machine's
    // microphone and typing into whatever window has focus. Owner-only
    // access is all a client ever legitimately needs.
    secure_socket(&sock_path)?;

    // I7: a capture device that isn't there yet at startup (a USB mic not
    // enumerated in time under Hyprland's `exec-once`) must not take the
    // whole daemon down with it -- it used to, via this same `?` propagating
    // out of `main`, which meant the daemon never even bound its socket.
    // Attempted eagerly here so a startup failure is visible in the logs
    // immediately, but `ensure_recorder` retries construction on the next
    // `ptt-start` regardless of whether this attempt succeeded.
    let recorder = match Recorder::new(&cfg.audio) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!(
                error = ?e,
                "no capture device at startup; will retry on the next ptt-start"
            );
            None
        }
    };

    let daemon = Arc::new(Daemon {
        state: AtomicU8::new(WARMING),
        recorder: Mutex::new(recorder),
        audio_cfg: cfg.audio.clone(),
        pipeline: Mutex::new(None),
        llama: Mutex::new(None),
        window_class: Mutex::new(None),
        max_seconds: cfg.audio.max_seconds,
        recording_epoch: AtomicU64::new(0),
        subscribers: Mutex::new(Vec::new()),
    });

    // Warm up off the accept loop so `status` answers immediately.
    {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || match warm_up(cfg, &daemon) {
            Ok((pipeline, server)) => {
                *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
                *lock_ignoring_poison(&daemon.llama) = server;
                daemon.state.store(IDLE, Ordering::SeqCst);
                daemon.broadcast(OverlayEvent::Idle);
                tracing::info!("ready");
            }
            Err(e) => {
                tracing::error!(error = ?e, "warm-up failed; daemon stays in warming");
                daemon.broadcast(OverlayEvent::Error { reason: format!("warm-up failed: {e}") });
            }
        });
    }

    // Explicit shutdown on a terminating signal -- see `shutdown`'s doc
    // comment for why this can't be left to `Drop` firing through an
    // unwinding `main`; daemons are stopped by signals, which don't unwind.
    {
        let daemon = Arc::clone(&daemon);
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
            .context("registering SIGTERM/SIGINT/SIGHUP handlers")?;
        std::thread::spawn(move || {
            // Blocks until the first signal arrives. `signal-hook`'s
            // self-pipe iterator is synchronous and needs no async runtime,
            // matching the rest of this binary. There is nothing to loop
            // back for: `shutdown` followed by `exit` ends the process, so
            // only the first signal this thread observes is ever acted on.
            if signals.forever().next().is_some() {
                shutdown(&daemon);
                std::process::exit(0);
            }
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

/// Restricts a just-bound Unix socket to owner-only access (I6).
///
/// `UnixListener::bind` creates the socket file at whatever the process
/// umask leaves it, which on most desktop umasks (022) is merely
/// world-readable -- harmless on its own -- but `paths::runtime_socket`
/// falls back to `std::env::temp_dir()` (mode 1777, world-writable-with-
/// sticky-bit) when `$XDG_RUNTIME_DIR` is unset, and *that* combination
/// would let any other local user connect and issue `ptt-start`. Locking the
/// socket down to `0600` unconditionally costs nothing on a correctly
/// configured `$XDG_RUNTIME_DIR` (already mode 0700 by the systemd/logind
/// contract) and closes the gap on the fallback path.
fn secure_socket(path: &std::path::Path) -> Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting permissions on {}", path.display()))
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
///
/// C1: a `llama-server` that fails to spawn, or spawns but never becomes
/// healthy, no longer fails this function. Before this fix, either failure
/// propagated out via `?`, the caller (`main`'s warm-up thread) logged it and
/// left `daemon.state` at `WARMING` forever, and every subsequent
/// `ptt-start` answered `{"ok":false,"err":"warming"}` -- permanently, with
/// no retry -- even though ASR, VAD, and the guardrail's raw-text fallback
/// were completely unaffected and spec 15 explicitly calls for exactly that
/// degraded path ("`llama-server` down / unhealthy -> skip normalization,
/// inject raw + rule pass"). Only spawning the ASR/VAD models can still fail
/// this function: there is no raw-fallback path for a missing ASR the way
/// there is for a missing normalizer, so failing loudly there is correct.
fn warm_up(cfg: Config, daemon: &Arc<Daemon>) -> Result<(Pipeline, Option<LlamaServer>)> {
    let models = paths::models_dir();

    let (server, base_url) = if cfg.normalize.enabled {
        match spawn_and_wait_healthy(&cfg.normalize) {
            Ok(server) => {
                let base_url = server.base_url();
                (Some(server), base_url)
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "llama-server did not come up; continuing with normalization disabled \
                     for this run (ASR + guardrail raw-text fallback only)"
                );
                (None, String::new())
            }
        }
    } else {
        tracing::info!("normalize.enabled = false; not spawning llama-server");
        (None, String::new())
    };

    let asr = SherpaTranscriber::new(&models, cfg.asr.num_threads)?;
    let trimmer = SileroTrimmer::new(&models)?;
    let normalizer: Box<dyn Normalizer> = match &server {
        Some(_) => Box::new(S1MiniClient::new(base_url, cfg.normalize.timeout_ms)),
        None if cfg.normalize.enabled => {
            Box::new(UnavailableNormalizer("llama-server failed to start".to_string()))
        }
        None => Box::new(UnavailableNormalizer("normalize.enabled = false".to_string())),
    };
    let injector = inject::build(&cfg.inject);

    // Reports `Normalizing`/`Injecting` as the pipeline enters each stage --
    // the only two transitions `Pipeline` can see that the daemon can't
    // observe from outside (spec 12; see `Pipeline::with_stage_events`).
    // Updates `daemon.state` (a plain store: only one utterance is ever in
    // flight at a time -- see `IdleOnExit`'s identical reasoning) so `status`
    // reflects the real sub-stage, not just "busy", and broadcasts the same
    // transition to every subscriber. `Transcribing` needs no such wiring:
    // `dispatch`'s `PttStop` handler already knows the moment it happens.
    let daemon_for_stage = Arc::clone(daemon);
    let stage_events: Arc<dyn Fn(OverlayEvent) + Send + Sync> = Arc::new(move |ev: OverlayEvent| {
        match &ev {
            OverlayEvent::Normalizing => daemon_for_stage.state.store(NORMALIZING, Ordering::SeqCst),
            OverlayEvent::Injecting => daemon_for_stage.state.store(INJECTING, Ordering::SeqCst),
            _ => {}
        }
        daemon_for_stage.broadcast(ev);
    });

    let pipeline = Pipeline::new(
        cfg,
        Box::new(asr),
        Box::new(trimmer),
        Box::new(WhatlangDetector),
        normalizer,
        injector,
    )
    .with_stage_events(stage_events);
    Ok((pipeline, server))
}

/// Spawns `llama-server` and waits for it to report healthy, as one
/// `Result` -- the seam `warm_up` above matches on to fall back to
/// [`UnavailableNormalizer`] instead of failing outright (C1).
fn spawn_and_wait_healthy(cfg: &owf_core::config::NormalizeConfig) -> Result<LlamaServer> {
    let mut server = LlamaServer::spawn(cfg)?;
    server.wait_healthy(Duration::from_secs(120))?;
    Ok(server)
}

/// Guards `shutdown` against running its cleanup twice.
///
/// In the intended path this is redundant -- `shutdown` is followed
/// immediately by `std::process::exit`, so the signal-handling thread never
/// loops back to act on a second signal -- but a defensive, explicit guard
/// costs one atomic and documents the idempotence requirement (fix 2) rather
/// than leaving it as an accident of control flow that a future edit could
/// break.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Explicit, deterministic teardown run on `SIGTERM`/`SIGINT`/`SIGHUP`.
///
/// Rust destructors -- `LlamaServer::drop` in particular -- only run if
/// `main` returns normally. A daemon is ordinarily stopped by a signal
/// instead (`pkill`, `systemctl restart`, logging out), none of which unwind
/// the stack, so relying on `Drop` here would leave the supervised
/// `llama-server` child (~600 MB resident) orphaned on every restart. That
/// is exactly the leak storing `LlamaServer` in `Daemon` (rather than
/// `mem::forget`-ing it) was meant to prevent -- the ownership was fixed but
/// nothing ever drove the shutdown path that makes it matter, until now.
///
/// `SIGKILL` cannot be caught by any process, this one included; that is
/// acceptable here because it is uncatchable everywhere; the OS reaps
/// `owf-daemon`'s children when `owf-daemon` itself is killed out from under
/// them regardless of what this function does.
///
/// Idempotent: `SHUTTING_DOWN` makes a second call a no-op, and each
/// individual step (`Option::take`, `remove_file`) is already a no-op the
/// second time round even without that guard.
fn shutdown(daemon: &Daemon) {
    if SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("shutting down");
    kill_llama(&daemon.llama);
    if let Some(r) = lock_ignoring_poison(&daemon.recorder).as_ref() {
        let _ = r.stop();
    }
    remove_runtime_files();
}

/// Kills and reaps the supervised `llama-server` child, if any is running.
///
/// Split out from `shutdown` so it is testable with a stub child process
/// (e.g. `sleep 300`) instead of a real `Daemon`, which would otherwise
/// require live audio hardware just to construct its `Recorder` (see
/// `capture::Recorder::new`). `Option::take` leaves `None` behind, so a
/// second call has nothing to do -- that is this function's half of
/// `shutdown`'s idempotence.
fn kill_llama(llama: &Mutex<Option<LlamaServer>>) {
    // Dropping the `LlamaServer` -- not just signalling it -- is what runs
    // its `Drop` impl: kill, then wait (reap), then remove the port file.
    drop(lock_ignoring_poison(llama).take());
}

/// Removes the socket, lock, and port files this daemon owns, so a fresh
/// `owf-daemon` can start immediately afterward instead of finding a stale
/// socket or being told the (now-dead) lock is still held. Best-effort: a
/// file that is already gone (a second call, or it was never created) is not
/// an error.
fn remove_runtime_files() {
    let _ = std::fs::remove_file(paths::runtime_socket());
    let _ = std::fs::remove_file(paths::runtime_lock());
    let _ = std::fs::remove_file(paths::runtime_port());
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
    let req = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => req,
        Err(e) => {
            let mut w = stream;
            let resp = Response::err(format!("bad request: {e}"));
            let _ = writeln!(w, "{}", serde_json::to_string(&resp).unwrap_or_default());
            return;
        }
    };

    // `Subscribe` turns this connection into a long-lived event stream
    // (spec 12) rather than the usual one-shot `Response` -- and "long-lived"
    // is exactly what the accept loop, running `handle` synchronously for
    // every connection in turn, cannot afford to do itself. Handing it to
    // its own thread and returning immediately keeps the accept loop's
    // single-threaded invariant intact: every `dispatch` call -- the only
    // place `daemon.state` is plainly `store`d rather than CAS'd -- still
    // happens exactly where it always did, synchronously, one connection at
    // a time, on the accept loop's own thread. This new thread never calls
    // `dispatch` and never touches `daemon.state` at all; it only reads
    // `OverlayEvent`s off a channel and writes them out. A subscriber that
    // never reads is bounded by `write_timeout` (set above) on this one
    // thread, and a subscriber that disconnects is detected the moment a
    // write to it fails -- see `serve_subscriber` -- so neither wedges the
    // daemon nor leaks its thread once the connection ends.
    if let Request::Subscribe = req {
        std::thread::spawn(move || serve_subscriber(daemon, stream));
        return;
    }

    let resp = dispatch(&daemon, req);
    let mut w = stream;
    let _ = writeln!(w, "{}", serde_json::to_string(&resp).unwrap_or_default());
}

/// Serves one `Request::Subscribe` connection for as long as it stays open:
/// an immediate snapshot of the daemon's current state, then every
/// subsequent `OverlayEvent` the daemon broadcasts, one NDJSON line each.
///
/// Always runs on its own thread (spawned from `handle`, never inline in the
/// accept loop) -- see the comment at that call site for why that's what
/// keeps the accept loop's single-threaded invariant intact.
fn serve_subscriber(daemon: Arc<Daemon>, stream: UnixStream) {
    let mut w = stream;
    let snapshot = snapshot_event(daemon.state.load(Ordering::SeqCst));
    if write_event(&mut w, &snapshot).is_err() {
        return; // gone already; nothing left to register.
    }

    let (tx, rx) = mpsc::channel();
    lock_ignoring_poison(&daemon.subscribers).push(tx);

    // Blocks here between events. That costs this thread's stack and
    // nothing else: it holds no lock the rest of the daemon needs, and
    // `Daemon::broadcast`'s `Sender::send` never blocks *its* caller waiting
    // for this end to catch up (`mpsc` channels are unbounded).
    while let Ok(event) = rx.recv() {
        if write_event(&mut w, &event).is_err() {
            // The client disconnected, or `write_timeout` (set by `handle`
            // before handing off this connection) tripped on a client too
            // slow to keep up. Either way: drop the connection and let this
            // thread end. `tx`'s counterpart in `daemon.subscribers` gets
            // pruned automatically the next time something is broadcast
            // (`Sender::send` starts failing once `rx` -- dropped right
            // here -- is gone), so no separate cleanup is needed.
            return;
        }
    }
    // `rx.recv()` returned `Err`: this subscriber's `Sender` (the one pushed
    // into `daemon.subscribers` above) is gone. Not expected in practice --
    // `daemon.subscribers` outlives every subscriber thread for the life of
    // the process -- but handled rather than unwrapped so this loop still
    // terminates cleanly if that ever stops being true.
}

fn write_event(w: &mut UnixStream, event: &OverlayEvent) -> std::io::Result<()> {
    writeln!(w, "{}", serde_json::to_string(event).unwrap_or_default())
}

/// Atomically claims the RECORDING -> TRANSCRIBING transition (the first of
/// the three busy sub-states -- see `is_busy`). Both a `ptt-stop` request and
/// the 120 s safety valve can race to end the same recording; only one of
/// them may win and hand the buffer to `run_utterance`.
fn claim_busy(daemon: &Daemon) -> bool {
    daemon.state.compare_exchange(RECORDING, TRANSCRIBING, Ordering::SeqCst, Ordering::SeqCst).is_ok()
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
            s if is_busy(s) => {
                // Spec 6.1: `ptt-start` while Transcribing/Normalizing/
                // Injecting is rejected, and "the overlay flashes" -- this is
                // that flash's trigger.
                daemon.broadcast(OverlayEvent::BusyRejected);
                Response::err("busy")
            }
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
            daemon.broadcast(OverlayEvent::Transcribing);
            let d = Arc::clone(daemon);
            std::thread::spawn(move || run_utterance(d));
            Response::ok(State::Transcribing)
        }
        Request::Cancel => {
            // CAS, not a snapshot-then-store: `current` was read before this
            // match, and a `ptt-stop` or the safety valve can claim the same
            // RECORDING -> TRANSCRIBING transition in between. Racing an
            // unconditional `state.store(IDLE, ..)` against that would stomp
            // a real in-flight transcription back to "idle" while
            // `run_utterance` was still holding the pipeline. Only one of
            // Cancel's RECORDING -> IDLE and `claim_busy`'s RECORDING ->
            // TRANSCRIBING can win.
            if daemon
                .state
                .compare_exchange(RECORDING, IDLE, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                if let Some(r) = lock_ignoring_poison(&daemon.recorder).as_ref() {
                    let _ = r.stop();
                }
                daemon.broadcast(OverlayEvent::Idle);
                return Response::ok(State::Idle);
            }
            match daemon.state.load(Ordering::SeqCst) {
                // Audio has already been handed off to the pipeline; there is
                // nothing left to cancel. `run_utterance` will return the
                // state to idle on its own once it finishes.
                s if is_busy(s) => Response::err("cannot cancel: transcription already in progress"),
                s => Response::ok(state_of(s)), // already idle, or still warming
            }
        }
        Request::Reload => {
            if current != IDLE {
                return Response::err("reload requires idle");
            }
            // I5: this used to parse the config on disk, discard the result,
            // and report bare success -- a user who edited a guardrail
            // threshold (or a style rule, or the inject backend) and
            // reloaded got `{"ok":true}` and no actual change. `[guardrail]`,
            // `[inject]`, and `[style_default]`/`[style_rules]` are pure
            // per-utterance data the pipeline reads straight off its config
            // and its injector on every call (see
            // `Pipeline::update_reloadable`), so those now really do take
            // effect. `[asr]`/`[normalize]`'s model- and llama-server-facing
            // settings still require a restart -- swapping ASR/VAD models or
            // reconnecting to a different `llama-server` live remains out of
            // scope, exactly as before.
            match Config::load() {
                Ok(new_cfg) => {
                    let injector = inject::build(&new_cfg.inject);
                    let mut guard = lock_ignoring_poison(&daemon.pipeline);
                    match guard.as_mut() {
                        Some(p) => {
                            p.update_reloadable(new_cfg, injector);
                            Response::ok(State::Idle)
                        }
                        // Unreachable in normal operation: IDLE is only ever
                        // reached once warm-up has populated `pipeline` (see
                        // `process_utterance`'s identical note).
                        None => Response::err("reload requires the pipeline to be warmed up"),
                    }
                }
                Err(e) => Response::err(format!("config error: {e}")),
            }
        }
        // Never actually reached: `handle` special-cases `Subscribe` before
        // it ever calls `dispatch`, since a subscriber gets a long-lived
        // event stream instead of one `Response` (see `handle`'s doc
        // comment). Kept only so this match stays exhaustive.
        Request::Subscribe => Response::err("subscribe must be negotiated by the connection handler"),
    }
}

/// Ensures `slot` holds a constructed value, building one via `ctor` first if
/// it doesn't yet -- and leaving an existing value alone rather than
/// rebuilding it.
///
/// `ctor` stands in for `Recorder::new` at the one real call site so this is
/// testable without live audio hardware, which `Recorder::new` needs even
/// just to enumerate a device (see `capture::Recorder::new`). This is the
/// retry half of I7: a capture device that wasn't there at startup gets
/// tried again on every `ptt-start` until one succeeds, using this exact
/// same path either way -- there is no separate "first time" logic to fall
/// out of sync with normal operation.
fn ensure_recorder<T>(slot: &Mutex<Option<T>>, ctor: impl FnOnce() -> Result<T, String>) -> Result<(), String> {
    let mut guard = lock_ignoring_poison(slot);
    if guard.is_none() {
        *guard = Some(ctor()?);
    }
    Ok(())
}

fn start_recording(daemon: &Arc<Daemon>) -> Response {
    // I7: construct the recorder (or retry a previous failure) before doing
    // anything else -- and I4: query the window class *after* capture has
    // actually started, not before. `hyprctl` is a subprocess spawn (bounded
    // by a timeout, see I3 in `hypr.rs`), and running it first clipped the
    // beginning of every utterance behind that spawn, which also contradicts
    // `hypr.rs`'s own claim that this call is off the critical path.
    if let Err(e) = ensure_recorder(&daemon.recorder, || {
        Recorder::new(&daemon.audio_cfg).map_err(|e| e.to_string())
    }) {
        return Response::err(format!("no microphone: {e}"));
    }

    // Spec 7.1: the overlay's live bars are driven by RMS at roughly 50 ms
    // cadence, not by every `cpal` callback -- which at 48 kHz fires roughly
    // every 10 ms, ~100/s, far more often than a UI needs or a Unix-socket
    // fan-out should be asked to carry. `on_level` must be a plain `Fn`, not
    // `FnMut` (see `Recorder::start`'s signature), so the "when did we last
    // emit" state lives behind a `Mutex` rather than as captured mutable
    // state -- `should_emit_level` itself stays a pure function either way,
    // and is what's actually under test (see this file's `#[cfg(test)]`
    // module); nothing here opens real audio hardware.
    let recording_started = Instant::now();
    let last_emit: Mutex<Option<Instant>> = Mutex::new(None);
    let daemon_for_level = Arc::clone(daemon);
    let start_result =
        lock_ignoring_poison(&daemon.recorder).as_ref().expect("just ensured Some").start(move |level| {
            let now = Instant::now();
            let mut last = lock_ignoring_poison(&last_emit);
            if should_emit_level(*last, now, LEVEL_EMIT_INTERVAL) {
                *last = Some(now);
                let elapsed_ms = now.duration_since(recording_started).as_millis() as u64;
                daemon_for_level.broadcast(OverlayEvent::Recording { level, elapsed_ms });
            }
        });
    if let Err(e) = start_result {
        return Response::err(format!("cannot start capture: {e}"));
    }

    // Capturing the window class here, now that the mic is already open,
    // is still deliberate (spec 11): it is the window that was focused when
    // the user *started* talking, not whatever has focus once they finish.
    *lock_ignoring_poison(&daemon.window_class) = owf_core::hypr::active_window_class();

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
    // Emitted once here, immediately, rather than waiting for the first
    // throttled callback: without it the overlay's live bars would only
    // appear up to `LEVEL_EMIT_INTERVAL` after the mic actually opened.
    daemon.broadcast(OverlayEvent::Recording { level: 0.0, elapsed_ms: 0 });

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
/// daemon frozen busy forever, with every later `PttStart`/`PttStop`/
/// `Cancel` hitting one of the `is_busy` branches.
///
/// Also broadcasts `OverlayEvent::Idle`, for the same reason and on the same
/// every-exit-path guarantee: a subscriber must see the overlay return to
/// idle even when the pipeline panicked mid-utterance, not just on the happy
/// path.
///
/// Constructed as the very first thing in `run_utterance`, before anything
/// fallible, so its `Drop` covers every line after it. The store is
/// unconditional, not a CAS: at most one `run_utterance` (or safety-valve)
/// call is ever in flight at a time -- the RECORDING -> TRANSCRIBING
/// transition it exits is claimed exactly once per recording, by whichever
/// of `ptt-stop` or the safety valve won `claim_busy` -- so there is no
/// other legitimate writer for this guard to race or clobber.
struct IdleOnExit<'a> {
    state: &'a AtomicU8,
    subscribers: &'a Mutex<Vec<mpsc::Sender<OverlayEvent>>>,
}

impl Drop for IdleOnExit<'_> {
    fn drop(&mut self) {
        self.state.store(IDLE, Ordering::SeqCst);
        broadcast_to(self.subscribers, OverlayEvent::Idle);
    }
}

fn run_utterance(daemon: Arc<Daemon>) {
    let _idle_on_exit = IdleOnExit { state: &daemon.state, subscribers: &daemon.subscribers };

    let stop_result = match lock_ignoring_poison(&daemon.recorder).as_ref() {
        Some(r) => r.stop(),
        // Unreachable in normal operation: reaching RECORDING requires
        // `start_recording` to have already ensured a recorder exists.
        None => {
            tracing::warn!("utterance finished but no recorder was ever constructed");
            daemon.broadcast(OverlayEvent::Error { reason: "no recorder available".to_string() });
            return;
        }
    };
    let stop = match stop_result {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "capture stop failed");
            daemon.broadcast(OverlayEvent::Error { reason: format!("capture stop failed: {e}") });
            return;
        }
    };
    let class = lock_ignoring_poison(&daemon.window_class).clone();
    process_utterance(
        &daemon.pipeline,
        &daemon.subscribers,
        &stop.samples,
        class.as_deref(),
        Some(stop.capture),
    );
}

/// The first ~60 characters of `text` -- spec 12's `Done` preview flash.
/// Truncates on a `char` boundary so multi-byte UTF-8 is never split
/// mid-codepoint.
fn preview_of(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    text.chars().take(MAX_CHARS).collect()
}

/// Runs one utterance's already-captured samples through the pipeline, and
/// broadcasts the outcome (`Done`/`Error`) to every subscriber.
///
/// Split out from `run_utterance` so it is testable without a real
/// `Recorder` (which needs live audio hardware to construct -- see
/// `capture::Recorder::new`): a test can drive this directly with a scratch
/// `Mutex<Option<Pipeline>>` and a panicking fake stage to prove
/// `IdleOnExit` recovers `state` even when this function unwinds. `subscribers`
/// is likewise a bare `&Mutex<Vec<...>>` rather than `&Daemon`, for the same
/// reason `IdleOnExit` takes one directly instead of a whole `Daemon`.
fn process_utterance(
    pipeline: &Mutex<Option<Pipeline>>,
    subscribers: &Mutex<Vec<mpsc::Sender<OverlayEvent>>>,
    samples: &[f32],
    window_class: Option<&str>,
    capture: Option<CaptureStats>,
) {
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
        match p.process_with_capture(samples, window_class, capture) {
            Ok(Some(out)) => {
                tracing::info!(chars = out.text.len(), "injected");
                broadcast_to(subscribers, OverlayEvent::Done { preview: preview_of(&out.text) });
            }
            Ok(None) => {
                tracing::info!("nothing to inject");
                broadcast_to(
                    subscribers,
                    OverlayEvent::Error { reason: "no speech detected".to_string() },
                );
            }
            Err(e) => {
                tracing::error!(error = ?e, "pipeline failed");
                broadcast_to(subscribers, OverlayEvent::Error { reason: e.to_string() });
            }
        }
    } else {
        // Unreachable in normal operation: reaching RECORDING/busy requires
        // having passed through IDLE, which is only set once warm-up has
        // populated `pipeline`. Logged rather than unwrapped so a future
        // change to that invariant fails loudly instead of panicking.
        tracing::warn!("utterance finished but the pipeline was not ready");
        broadcast_to(subscribers, OverlayEvent::Error { reason: "pipeline not ready".to_string() });
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
        let state = AtomicU8::new(TRANSCRIBING);
        let subscribers: Mutex<Vec<mpsc::Sender<OverlayEvent>>> = Mutex::new(Vec::new());
        let (tx, rx) = mpsc::channel();
        subscribers.lock().unwrap().push(tx);
        let samples = vec![0.1f32; 16_000];

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _idle_on_exit = IdleOnExit { state: &state, subscribers: &subscribers };
            process_utterance(&pipeline, &subscribers, &samples, None, None);
        }));

        assert!(result.is_err(), "the transcriber's panic should have propagated");
        assert_eq!(
            state.load(Ordering::SeqCst),
            IDLE,
            "a panic inside the pipeline must not leave the daemon stuck busy"
        );
        assert_eq!(
            rx.try_recv(),
            Ok(OverlayEvent::Idle),
            "a subscriber must see the overlay return to idle even when the pipeline panicked"
        );
    }

    /// Fix 2: proves the mechanism that guarantees no `llama-server` process
    /// survives shutdown. A real `llama-server` can't be started on this
    /// machine (no ggml compute backend), so this stubs the supervised child
    /// with a plain `sleep 300` via the test-only `LlamaServer::from_child`
    /// -- the same `Drop` impl (kill, then wait/reap) runs either way.
    #[test]
    fn kill_llama_terminates_a_stub_child_and_is_idempotent() {
        let child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("spawning a stub child (`sleep 300`) for this test");
        let pid = child.id();
        let llama = Mutex::new(Some(LlamaServer::from_child(child, 0)));

        kill_llama(&llama);
        // A second call must be a no-op, not a double-kill or a panic --
        // `Option::take` on an already-`None` mutex is exactly that.
        kill_llama(&llama);

        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "child process {pid} survived kill_llama: no llama-server may outlive shutdown"
        );
    }

    /// I7: `ensure_recorder` is the retry mechanism behind "a missing
    /// microphone at startup must not kill the daemon" -- this exercises it
    /// with a fake `u32` "recorder" and a call counter instead of a real
    /// `capture::Recorder`, which needs live audio hardware just to
    /// construct. Proves both halves: it builds lazily when empty, and it
    /// never rebuilds once something is there.
    #[test]
    fn ensure_recorder_constructs_lazily_and_only_once() {
        let slot: Mutex<Option<u32>> = Mutex::new(None);
        let calls = std::sync::atomic::AtomicU32::new(0);

        ensure_recorder(&slot, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(7)
        })
        .unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(7));

        // The slot is already populated: this must not reconstruct.
        ensure_recorder(&slot, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(99)
        })
        .unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(7), "must not reconstruct once populated");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the constructor should only run once");
    }

    /// I7: a failed construction must propagate the error and leave the slot
    /// exactly as it was (empty), so the very next call retries rather than
    /// getting stuck on a poisoned placeholder -- this is what lets a
    /// missing-at-startup microphone recover once it's actually plugged in,
    /// without the daemon needing a restart.
    #[test]
    fn ensure_recorder_propagates_the_constructor_error_and_leaves_the_slot_empty() {
        let slot: Mutex<Option<u32>> = Mutex::new(None);

        let err = ensure_recorder(&slot, || Err("no microphone".to_string())).unwrap_err();

        assert_eq!(err, "no microphone");
        assert!(
            slot.lock().unwrap().is_none(),
            "a failed construction must not poison the slot with a placeholder"
        );

        // And the retry that matters: a later call with a working
        // constructor must still succeed.
        ensure_recorder(&slot, || Ok(1)).unwrap();
        assert_eq!(*slot.lock().unwrap(), Some(1));
    }

    /// I6: proves the socket this daemon binds ends up owner-only, not at
    /// whatever the ambient umask would otherwise leave it -- exercised
    /// against a scratch socket path rather than the real
    /// `$XDG_RUNTIME_DIR` one, since `main` itself isn't unit-testable.
    #[test]
    fn secure_socket_locks_the_socket_down_to_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("owf-test-socket-perms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let _bound = UnixListener::bind(&sock_path).unwrap();

        secure_socket(&sock_path).unwrap();

        let mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the socket must be owner-only, got {mode:o}");

        drop(_bound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Overlay state broadcast (M2 Task 1) -----------------------------

    #[test]
    fn state_of_reports_the_real_busy_substage_not_a_collapsed_busy() {
        // Spec 6.1/12: distinguishing Transcribing/Normalizing/Injecting is
        // a real requirement -- before this fix every one of these
        // collapsed into a single generic "busy" value.
        assert_eq!(state_of(WARMING), State::Warming);
        assert_eq!(state_of(IDLE), State::Idle);
        assert_eq!(state_of(RECORDING), State::Recording);
        assert_eq!(state_of(TRANSCRIBING), State::Transcribing);
        assert_eq!(state_of(NORMALIZING), State::Normalizing);
        assert_eq!(state_of(INJECTING), State::Injecting);
    }

    #[test]
    fn is_busy_covers_exactly_the_three_processing_substates() {
        assert!(is_busy(TRANSCRIBING));
        assert!(is_busy(NORMALIZING));
        assert!(is_busy(INJECTING));
        assert!(!is_busy(WARMING));
        assert!(!is_busy(IDLE));
        assert!(!is_busy(RECORDING));
    }

    #[test]
    fn snapshot_event_mirrors_state_of_for_the_steady_states() {
        assert_eq!(snapshot_event(WARMING), OverlayEvent::Warming);
        assert_eq!(snapshot_event(IDLE), OverlayEvent::Idle);
        assert_eq!(snapshot_event(TRANSCRIBING), OverlayEvent::Transcribing);
        assert_eq!(snapshot_event(NORMALIZING), OverlayEvent::Normalizing);
        assert_eq!(snapshot_event(INJECTING), OverlayEvent::Injecting);
        assert_eq!(
            snapshot_event(RECORDING),
            OverlayEvent::Recording { level: 0.0, elapsed_ms: 0 },
            "a subscriber connecting mid-recording gets a placeholder that the \
             next live ~50ms tick immediately corrects"
        );
    }

    /// Spec 7.1's ~50 ms cadence, as a pure function -- no audio device
    /// involved.
    #[test]
    fn should_emit_level_emits_on_the_first_call() {
        assert!(should_emit_level(None, Instant::now(), LEVEL_EMIT_INTERVAL));
    }

    #[test]
    fn should_emit_level_holds_back_before_the_interval_elapses() {
        let last = Instant::now();
        let now = last + Duration::from_millis(10);
        assert!(!should_emit_level(Some(last), now, LEVEL_EMIT_INTERVAL));
    }

    #[test]
    fn should_emit_level_emits_once_the_interval_has_fully_elapsed() {
        let last = Instant::now();
        let now = last + LEVEL_EMIT_INTERVAL;
        assert!(should_emit_level(Some(last), now, LEVEL_EMIT_INTERVAL));
    }

    #[test]
    fn preview_of_truncates_to_sixty_characters_on_a_char_boundary() {
        let text = "a".repeat(100);
        let preview = preview_of(&text);
        assert_eq!(preview.chars().count(), 60);

        // A multi-byte character sitting right at the cutoff must not split
        // a codepoint -- `chars().take(60)` guarantees this by construction,
        // but pin it with a real multi-byte example anyway.
        let with_emoji = format!("{}\u{1F600}", "b".repeat(59));
        let preview = preview_of(&with_emoji);
        assert_eq!(preview.chars().count(), 60);
        assert!(preview.ends_with('\u{1F600}'));
    }

    /// The core safety property `Request::Subscribe`'s own-thread design
    /// depends on: a dead subscriber (its `Receiver` dropped, exactly what
    /// happens when `serve_subscriber`'s thread exits after a failed write)
    /// must be silently pruned, not cause a panic or a block, and a live
    /// subscriber alongside it must be unaffected.
    #[test]
    fn broadcast_to_prunes_a_disconnected_subscriber_without_blocking_live_ones() {
        let subscribers: Mutex<Vec<mpsc::Sender<OverlayEvent>>> = Mutex::new(Vec::new());

        let (dead_tx, dead_rx) = mpsc::channel();
        subscribers.lock().unwrap().push(dead_tx);
        drop(dead_rx); // simulates the subscriber's thread having exited

        let (live_tx, live_rx) = mpsc::channel();
        subscribers.lock().unwrap().push(live_tx);

        broadcast_to(&subscribers, OverlayEvent::Idle);

        assert_eq!(
            subscribers.lock().unwrap().len(),
            1,
            "the dead subscriber must be pruned, leaving only the live one"
        );
        assert_eq!(live_rx.try_recv(), Ok(OverlayEvent::Idle), "the live subscriber must still be delivered to");
    }

    #[test]
    fn broadcast_to_an_empty_subscriber_list_is_a_no_op() {
        let subscribers: Mutex<Vec<mpsc::Sender<OverlayEvent>>> = Mutex::new(Vec::new());
        broadcast_to(&subscribers, OverlayEvent::BusyRejected); // must not panic
        assert!(subscribers.lock().unwrap().is_empty());
    }

    /// A minimal `Daemon` for socket-level tests: no real `Recorder` or
    /// `llama-server` (both need resources this sandbox doesn't have), just
    /// enough to drive `handle`/`dispatch` for `Status`/`Subscribe`.
    fn fake_daemon(initial_state: u8) -> Arc<Daemon> {
        Arc::new(Daemon {
            state: AtomicU8::new(initial_state),
            recorder: Mutex::new(None),
            audio_cfg: AudioConfig::default(),
            pipeline: Mutex::new(None),
            llama: Mutex::new(None),
            window_class: Mutex::new(None),
            max_seconds: 120,
            recording_epoch: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
        })
    }

    /// The concern behind giving `Subscribe` its own thread, proven
    /// end-to-end over a real socket: a subscriber that connects and
    /// disconnects mid-stream must not wedge the accept loop for any other
    /// connection, and its dead sender must eventually be pruned.
    #[test]
    fn a_disconnected_subscriber_does_not_wedge_the_accept_loop_or_other_connections() {
        let dir =
            std::env::temp_dir().join(format!("owf-daemon-test-subscribe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let daemon = fake_daemon(IDLE);

        // Mirrors `main`'s accept loop exactly: `handle` runs synchronously,
        // once per connection, in the order connections arrive. Bounded to
        // two connections so this thread joins cleanly at the end of the
        // test instead of blocking on `incoming()` forever.
        let accept_daemon = Arc::clone(&daemon);
        let accept_listener = listener.try_clone().unwrap();
        let accept_thread = std::thread::spawn(move || {
            for s in accept_listener.incoming().take(2).flatten() {
                handle(Arc::clone(&accept_daemon), s);
            }
        });

        // Client 1: subscribe, read the snapshot, then hang up immediately
        // -- before the daemon has broadcast anything at all.
        {
            let mut stream = UnixStream::connect(&sock_path).unwrap();
            writeln!(stream, "{}", serde_json::to_string(&Request::Subscribe).unwrap()).unwrap();
            stream.flush().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(
                line.trim(),
                r#"{"event":"idle"}"#,
                "the snapshot must reflect the fake daemon's actual (idle) state"
            );
            // `stream` and its clone drop here: the client hangs up.
        }

        // Client 2: an ordinary request. If the accept loop were blocked on
        // the (now-gone) subscriber, this would hang rather than return
        // promptly.
        let mut stream2 = UnixStream::connect(&sock_path).unwrap();
        writeln!(stream2, "{}", serde_json::to_string(&Request::Status).unwrap()).unwrap();
        stream2.flush().unwrap();
        let mut reader2 = BufReader::new(stream2);
        let mut line2 = String::new();
        reader2.read_line(&mut line2).unwrap();
        let resp: Response = serde_json::from_str(line2.trim()).unwrap();
        assert!(resp.ok, "a normal request must still be served after a subscriber disconnects");

        accept_thread.join().unwrap();

        // The dead subscriber is only discovered on the next attempted
        // write to it (by design -- see `serve_subscriber`), and a small,
        // local AF_UNIX write can succeed into a kernel buffer once even
        // after the peer has closed, so retry the broadcast itself rather
        // than only sleeping: this is a bounded poll (at most ~1s total),
        // not a fixed sleep-and-hope.
        let mut tries = 0;
        while !lock_ignoring_poison(&daemon.subscribers).is_empty() && tries < 200 {
            daemon.broadcast(OverlayEvent::Idle);
            std::thread::sleep(Duration::from_millis(5));
            tries += 1;
        }
        assert!(
            lock_ignoring_poison(&daemon.subscribers).is_empty(),
            "a disconnected subscriber must eventually be pruned, not accumulate forever"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
