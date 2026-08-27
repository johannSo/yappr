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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use owf_core::asr::SherpaTranscriber;
use owf_core::capture::{CaptureStats, Recorder};
use owf_core::config::{AudioConfig, Config, DebugConfig};
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
fn warm_up(cfg: Config) -> Result<(Pipeline, Option<LlamaServer>)> {
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
                if let Some(r) = lock_ignoring_poison(&daemon.recorder).as_ref() {
                    let _ = r.stop();
                }
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
    let start_result =
        lock_ignoring_poison(&daemon.recorder).as_ref().expect("just ensured Some").start(|_level| {});
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

    let stop_result = match lock_ignoring_poison(&daemon.recorder).as_ref() {
        Some(r) => r.stop(),
        // Unreachable in normal operation: reaching RECORDING requires
        // `start_recording` to have already ensured a recorder exists.
        None => {
            tracing::warn!("utterance finished but no recorder was ever constructed");
            return;
        }
    };
    let stop = match stop_result {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "capture stop failed");
            return;
        }
    };
    let class = lock_ignoring_poison(&daemon.window_class).clone();
    process_utterance(&daemon.pipeline, &stop.samples, class.as_deref(), Some(stop.capture));
}

/// Runs one utterance's already-captured samples through the pipeline.
///
/// Split out from `run_utterance` so it is testable without a real
/// `Recorder` (which needs live audio hardware to construct -- see
/// `capture::Recorder::new`): a test can drive this directly with a scratch
/// `Mutex<Option<Pipeline>>` and a panicking fake stage to prove
/// `IdleOnExit` recovers `state` even when this function unwinds.
fn process_utterance(
    pipeline: &Mutex<Option<Pipeline>>,
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
            process_utterance(&pipeline, &samples, None, None);
        }));

        assert!(result.is_err(), "the transcriber's panic should have propagated");
        assert_eq!(
            state.load(Ordering::SeqCst),
            IDLE,
            "a panic inside the pipeline must not leave the daemon stuck at BUSY"
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
}
