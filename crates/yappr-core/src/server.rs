//! Long-lived process that holds the ASR/VAD/normalize models warm and
//! serves push-to-talk requests over a Unix socket.
//!
//! Wayland has no global keyboard grab, so `owf-ctl` (invoked by a Hyprland
//! keybind) writes one NDJSON request line here and reads one response line
//! back. See `yappr_core::proto` for the wire format.

use anyhow::{Context, Result};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::asr;
use crate::capture::{CaptureStats, Recorder};
use crate::config::{self, AudioConfig, Config, DebugConfig, ModelsConfig, RealtimeConfig};
use crate::config_write;
use crate::inject;
use crate::lang::WhatlangDetector;
use crate::llama::{LlamaEngine, MODEL_FILE};
use crate::normalize::Normalizer;
use crate::paths;
use crate::pipeline::{Pipeline, Timings};
use crate::proto::{OverlayEvent, Request, Response, State};
use crate::realtime::{EngineInfo, RealtimeEngine, RealtimeHandle};
use crate::vad::{Segmenter, SileroSegmenter, SileroTrimmer};

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
// Task 3, Work Item 3: a fatal, unrecoverable warm-up failure (ASR/VAD
// failed to load -- the *only* way `load_models` still returns `Err`, since a
// `llama-server` failure is already absorbed into `UnavailableNormalizer`
// and never propagates). Before this state existed, a fatal warm-up failure
// left `daemon.state` at `WARMING` forever: a subscriber connected at the
// moment of failure saw the one-shot `OverlayEvent::Error`, but anyone
// connecting *afterward* got `snapshot_event(WARMING)` -- a permanent
// spinner instead of the error pill spec 12 requires. `FAILED` is a real,
// steady state `state_of`/`snapshot_event` can report to a late joiner.
const FAILED: u8 = 6;
// Task 13: tray-driven, via `Request::SetPaused`. Deliberately not one of
// `is_busy`'s three values -- pausing is not "processing an utterance", it's
// "refuse to start a new one" -- and reached only by a compare-and-exchange
// from `IDLE`, never a store, so a pause request arriving mid-utterance
// (RECORDING included, not just the three `is_busy` names) defers to it
// instead of seizing it (invariant 1).
const PAUSED: u8 = 7;

fn is_busy(v: u8) -> bool {
    matches!(v, TRANSCRIBING | NORMALIZING | INJECTING)
}

/// Enumerating devices talks to ALSA/PipeWire, which can hang. Bounded for
/// the same reason invariant 6 bounds every subprocess call.
const DEVICE_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Which of a config change's sections cannot take effect without a restart.
///
/// This lives here, in Rust, rather than in the settings GUI: the rule is a
/// restatement of what `Pipeline::update_reloadable` will and will not swap,
/// and a second copy in TypeScript would drift from it the first time the
/// pipeline learned to reload something new.
///
/// `models_loaded` is why this takes a third argument. `[asr]` and
/// `[normalize]` are read when the models are *built*, and since the lazy
/// lifecycle landed that is `ensure_models_loaded`, which re-reads
/// `config.toml` from disk at the moment of the call -- so with nothing
/// resident, a changed model or normalizer takes effect on the next press
/// and a restart buys the user exactly nothing. The lazy-lifecycle design
/// doc (2026-08-29 §1) already recorded this as leaving the requirement
/// "pessimistic -- not wrong", which a passive pill could afford. A dialog
/// that interrupts to demand a restart cannot: being pessimistic there
/// means nagging every user who has not dictated recently, which -- with
/// `preload_at_startup` defaulting to `false` and `idle_unload_seconds` to
/// 60 -- is the ordinary state of an idle daemon, not an edge case. So the
/// question is asked against what is actually resident.
///
/// Read `daemon.models_loaded` for this, never `pipeline.is_some()`:
/// invariant 12's first bullet, and `SetConfig` is dispatched from the same
/// accept loop that would deadlock on it.
fn restart_reason(old: &Config, new: &Config, models_loaded: bool) -> Option<String> {
    if !models_loaded {
        return None;
    }
    let mut sections = Vec::new();
    if old.asr != new.asr {
        sections.push("[asr] -- the ASR model is loaded once, when the models come up");
    }
    if old.normalize != new.normalize {
        sections.push("[normalize] -- the normalizer is built once, when the models come up");
    }
    if sections.is_empty() {
        return None;
    }
    Some(format!("saved, but a restart is needed for: {}", sections.join("; ")))
}

fn state_of(v: u8) -> State {
    match v {
        WARMING => State::Warming,
        RECORDING => State::Recording,
        TRANSCRIBING => State::Transcribing,
        NORMALIZING => State::Normalizing,
        INJECTING => State::Injecting,
        FAILED => State::Error,
        PAUSED => State::Paused,
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
        PAUSED => OverlayEvent::Paused,
        // A generic fallback reason: `serve_subscriber` overrides this with
        // the real stored `daemon.fatal_error` when one is available, which
        // it always is by the time `FAILED` is ever observable. Kept here
        // too so this function stays a total, sensible mapping on its own.
        FAILED => OverlayEvent::Error { reason: "daemon failed to start; see logs".to_string() },
        _ => OverlayEvent::Idle,
    }
}

/// The events a subscriber -- socket or in-process -- must see immediately
/// upon attaching: `snapshot_event` for `state_now`, replaced by the real
/// stored fatal reason when `state_now == FAILED` (`fatal_error`), plus a
/// `NormalizeDegraded` replay when `degraded`. Pure and side-effect-free so
/// `serve_subscriber` (which must compute `state_now`/`degraded` inside its
/// own subscriber-registration critical section -- see that function's doc
/// comment for the connect-time race that guards against) and
/// `Daemon::connect_snapshot` (the in-process sink has no registration step
/// to race: see that method's doc comment) can share the exact same mapping
/// from "state right now" to "events to send" without hand-copying it.
fn connect_events(state_now: u8, degraded: bool, fatal_error: Option<String>) -> Vec<OverlayEvent> {
    let mut snapshot = snapshot_event(state_now);
    if state_now == FAILED {
        if let Some(reason) = fatal_error {
            snapshot = OverlayEvent::Error { reason };
        }
    }
    let mut events = vec![snapshot];
    if degraded {
        events.push(OverlayEvent::NormalizeDegraded {
            reason: "llama-server is down or unhealthy".to_string(),
        });
    }
    events
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

/// A sink for events the server produces. The standalone daemon had exactly
/// one consumer (socket subscribers); the app has two, because the overlay is
/// now in-process and gets its events through Tauri rather than a socket.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &OverlayEvent);
    /// Shows and focuses the settings window (`Request::ShowSettings`). A
    /// no-op default so the standalone daemon's `run()` sink (`NoExtraSink`)
    /// and every test sink in this file stay valid without edits -- only
    /// `TauriSink` (`src-tauri/src/lib.rs`), which actually owns a window to
    /// show, needs to override it.
    fn show_settings(&self) {}
    /// Shows the settings window with the wizard on top
    /// (`Request::ShowWizard`). A no-op default for the same reason
    /// `show_settings`'s is: only `TauriSink` owns a window to show.
    fn show_wizard(&self) {}
    /// Unregisters the tray item, spec §8 step 3 -- called once from
    /// [`shutdown`], before removing the runtime socket/lock (step 4).
    /// A no-op default for the same
    /// reason `show_settings`'s is: the standalone daemon and every test
    /// sink in this file own no tray to unregister. Only `TauriSink`
    /// (Task 12, `src-tauri/src/tray.rs`) overrides it.
    fn unregister_tray(&self) {}
    /// Starts a fresh copy of this binary, for [`Request::Restart`]. Called
    /// once, from that arm's thread, *after* [`shutdown`] and immediately
    /// before `std::process::exit(0)`.
    ///
    /// A no-op default for the same reason `show_settings`'s is -- but here
    /// the reason it must live behind the sink at all is narrower than
    /// "only `TauriSink` owns the thing": resolving *which* file to spawn
    /// needs `tauri::process::current_binary`, and this crate does not
    /// depend on `tauri`. `std::env::current_exe` is not a substitute --
    /// under an AppImage it names a path inside the mount that disappears
    /// the moment the parent exits, where `current_binary` prefers
    /// `$APPIMAGE`. So the decision lives in `src-tauri` and this is the
    /// seam it reaches through.
    ///
    /// Called after `shutdown` deliberately: `shutdown` removes
    /// `yappr.lock`, so the successor's `OpenOptions::create` makes a fresh
    /// inode and its `flock` cannot contend with the one this process still
    /// holds on the now-unlinked old one. That is what makes the handoff
    /// race-free without any retry or backoff on the successor's side.
    fn relaunch(&self) {}
}

pub struct Daemon {
    state: AtomicU8,
    /// Spec §8 step 1's first clause, missed by this task's initial pass:
    /// "stop accepting new utterances" -- set once, synchronously, by
    /// `Request::Quit` (and by `Request::Restart`, which is that arm plus a
    /// relaunch and needs the latch for the identical reason) before it
    /// spawns the wait/shutdown thread, and
    /// checked at the top of `PttStart` and `Toggle` (never cleared; a
    /// daemon that has been asked to quit never un-quits -- a *restarting*
    /// one does come back, but as a new process with a fresh `Daemon`).
    /// Without this, a
    /// `PttStart`/`Toggle` landing on the very next `accept()` after Beenden
    /// -- the accept loop is single-threaded, so "next" can be milliseconds
    /// away -- would be accepted normally and could reach `TRANSCRIBING`
    /// inside the short window while `shutdown` is stopping housekeeping and
    /// reaping `llama-server`, destroying a transcript invariant 1 says must
    /// never be lost. `PttStop` is deliberately not guarded here: it only
    /// ever finishes a recording that was already accepted before Quit was
    /// dispatched, not a new one.
    quitting: AtomicBool,
    /// `None` until a `Recorder` has been successfully constructed -- either
    /// eagerly at startup, or lazily on the first `ptt-start` after a
    /// startup where it wasn't (I7). See `ensure_recorder`.
    recorder: Mutex<Option<Recorder>>,
    /// Kept so `ensure_recorder` can retry `Recorder::new` later with the
    /// same settings, independent of how many times it has already failed.
    /// Behind a mutex so `set-config` can point the daemon at a different
    /// microphone without a restart: changing it and clearing `recorder`
    /// makes the next dictation build a recorder on the new device. The
    /// microphone is the most-changed setting in the settings GUI, and
    /// "restart the daemon" would be a poor answer for it.
    audio_cfg: Mutex<AudioConfig>,
    pipeline: Mutex<Option<Pipeline>>,
    /// Held across a model load so two concurrent presses share one load
    /// rather than racing two, and so an unload can never interleave with
    /// one.
    ///
    /// **Lock order: this is always taken before `pipeline`, never after.**
    /// `process_utterance` takes only `pipeline`, and holds it for the whole
    /// `process` call; `ensure_models_loaded` and `unload_models` take this
    /// first and hold `pipeline` only for the assignment.
    load_lock: Mutex<()>,
    /// Lock-free projection of "`pipeline` is `Some`", for readers that must
    /// never block on it.
    ///
    /// Not redundant bookkeeping, for the same reason `normalize_available`
    /// is not: `process_utterance` holds `pipeline`'s mutex for the entire
    /// `process` call (deliberately -- see its own comment on the
    /// sherpa-onnx FFI), so any thread that answers a question by locking
    /// `pipeline` blocks for the length of a whole transcription. Two such
    /// readers exist -- `dispatch`'s `Status` handler, which would make
    /// `--status` hang mid-utterance, and the housekeeping thread, which
    /// would stop reaping subscribers for the same window. Both read this.
    models_loaded: AtomicBool,
    /// When the daemon was last doing something dictation-shaped: bumped by
    /// `start_recording` (via `touch_activity`) and by every utterance that
    /// ends (`IdleOnExit`). The idle-unload deadline is measured from this --
    /// read by `unload_models` (via `should_unload`), and re-read under
    /// `load_lock` there specifically so a press landing after the
    /// housekeeping thread's decision but before it takes the lock can still
    /// change the answer.
    last_activity: Mutex<Instant>,
    /// `[models]`, mirrored here so a Settings change takes effect without a
    /// restart -- the same reason, and the same shape, as `audio_cfg`.
    models_cfg: Mutex<ModelsConfig>,
    /// `[realtime]`, mirrored for the same reason once more -- and read by
    /// one caller the others do not have: a realtime session thread, which
    /// must never block on `pipeline` to learn its own segmentation
    /// settings (invariant 12's rule, applied to a second reader).
    realtime_cfg: Mutex<RealtimeConfig>,
    window_class: Mutex<Option<String>>,
    /// Where `get-config`/`set-config` read and write. A field rather than a
    /// call to `paths::config_file()` at each use site so tests can point it
    /// at a scratch file: `set-config` *writes*, and a test running against
    /// the real path would overwrite the config of whoever ran the suite.
    config_path: PathBuf,
    /// Bumped by every `start_recording`. The 120 s safety-valve timer it
    /// spawns captures the epoch at spawn time and only acts if the epoch is
    /// still current -- otherwise a timer left over from an earlier session
    /// that already stopped (or was cancelled) could fire against a *later*
    /// recording and truncate it early. See concurrency note 3.
    recording_epoch: AtomicU64,
    /// One entry per currently-connected `Request::Subscribe` client; see
    /// `broadcast_to`, `register_subscriber`, and `serve_subscriber`.
    subscribers: Mutex<Vec<Subscriber>>,
    /// Set once at startup from `[normalize].enabled` and never changed
    /// afterward (reload does not currently rebuild the normalizer -- plan
    /// Task 7). Distinguishes "normalization was never turned on" (`status`
    /// reports `normalize_available: None`) from "it's turned on but
    /// currently down" (`Some(false)`) -- see `Response::normalize_available`.
    normalize_enabled: bool,
    /// The supervisor's (Task 3) best-known answer to "is normalization
    /// usable right now" -- an `AtomicBool` rather than something requiring
    /// a lock so `dispatch`'s `Status` handler can read it without blocking
    /// on, or being blocked by, a health probe in progress. Meaningless
    /// (stays `false` forever) when `normalize_enabled` is `false`.
    normalize_available: AtomicBool,
    /// Set on a fatal warm-up failure (ASR/VAD failed to load) so a
    /// subscriber connecting after the fact -- not just one connected at the
    /// moment it happened -- can still learn *why* the daemon is stuck in
    /// `FAILED` (Task 3, Work Item 3). `None` for the entire run otherwise.
    fatal_error: Mutex<Option<String>>,
    /// The German sentence startup leaves behind when the config on disk was
    /// not usable (spec §3.1) -- either it was quarantined, or a legacy config
    /// in `~/.config` could not be migrated. Reported by `Status` and
    /// `GetConfig` so the settings window can say what happened and name the
    /// file; `None` for the entire run otherwise.
    ///
    /// A built sentence rather than the structured cause, because both causes
    /// end at the same banner and the two facts that go into it -- the path and
    /// the parser's own message -- are server-side. The GUI has no business
    /// reconstructing either.
    ///
    /// Separate from `fatal_error` on purpose. That one is only read when the
    /// state is `FAILED`, and a config that had to be replaced is not a failed
    /// daemon -- reusing it would report a working app as broken.
    config_notice: Mutex<Option<String>>,
    /// The realtime transcription endpoint (`realtime.rs`), when
    /// `[realtime] enabled` is on and the port was free. `None` otherwise,
    /// which is the default and the overwhelmingly common case -- nothing
    /// in dictation touches this.
    ///
    /// Owned here rather than by `start`'s caller because it has to survive
    /// a `SetConfig`: `sync_realtime` stops the old listener and starts a
    /// new one whenever `[realtime]` changes, so that turning the endpoint
    /// on from the settings window does not need a restart. That is also
    /// why `RealtimeHandle` remembers the config it was started from.
    realtime: Mutex<Option<RealtimeHandle>>,
    /// Why the endpoint is not listening while `[realtime] enabled` is on.
    /// `None` whenever the last `sync_realtime` succeeded or switched it
    /// off. See `realtime_status`.
    realtime_error: Mutex<Option<String>>,
    /// The background thread reaping dead subscribers and (when enabled)
    /// supervising `llama-server` -- see `spawn_housekeeping`. `None` until
    /// `main` installs it, and again after `shutdown` stops it.
    housekeeping: Mutex<Option<HousekeepingHandle>>,
    /// The per-stage timing breakdown from the most recent utterance that
    /// actually produced an `Outcome` (Task 2: `Outcome.timings` used to be
    /// computed and read by nothing in production). Surfaced by `status` as
    /// `last_ms` so `owf-ctl status` can report where the time actually went.
    /// `None` until the first such utterance; a "no speech detected" or
    /// pipeline-error utterance has no `Outcome` to take timings from and
    /// leaves whatever was last recorded in place rather than clearing it.
    last_timings: Mutex<Option<Timings>>,
    /// The in-process consumer of every broadcast. The standalone daemon had
    /// none; the app's overlay is no longer a socket client, so it is one.
    sink: Arc<dyn EventSink>,
    /// Where `shutdown`'s `remove_runtime_files` deletes the socket, lock,
    /// and port files this daemon owns. Real `paths::runtime_*()` values in
    /// production (`start`); a scratch, never-real path in `fake_daemon_at`/
    /// `fake_daemon_with_sink` -- for the same reason `config_path` is a
    /// field rather than a bare `paths::config_file()` call: a test that
    /// calls `shutdown` (which several in this module now do, to prove the
    /// llama child is reaped) must never be able to delete
    /// `$XDG_RUNTIME_DIR/yappr.sock` out from under a real daemon
    /// running on the same machine as the test suite.
    runtime_socket_path: PathBuf,
    runtime_lock_path: PathBuf,
    /// The single-instance guard (see `start`'s doc comment): an exclusive,
    /// non-blocking `flock` on a runtime file. Held for the life of the
    /// daemon only because this `File` lives here -- `start` used to keep it
    /// in a local that dropped, and released the lock, the moment `start`
    /// returned, letting a second `yappr` race this one for the
    /// socket. Never read; it exists only so `Drop` doesn't run early.
    _runtime_lock: std::fs::File,
}

/// A single registered `Request::Subscribe` connection: the channel
/// `broadcast_to` sends events through, plus a liveness flag
/// `serve_subscriber` clears (via `ClearAliveOnDrop`) the moment its thread
/// exits, for any reason: a clean write failure, or its own liveness poll
/// noticing the peer is gone (see `socket_peer_gone`).
///
/// This is what lets `reap_dead_subscribers` prune a dead entry even when
/// nothing has ever been broadcast since it died (Task 3, Work Item 2):
/// before this, a dead subscriber's thread and `Sender` were reaped only as
/// an incidental side effect of the *next* broadcast, which may never come
/// while the daemon sits `Idle`.
struct Subscriber {
    tx: mpsc::Sender<OverlayEvent>,
    alive: Arc<AtomicBool>,
}

/// Caps how many `Request::Subscribe` connections may be registered at
/// once (Task 3, Work Item 2). The intended consumer is a single overlay
/// that reconnects with backoff and autostarts with the session (spec 12);
/// this is generous headroom for that one overlay plus a couple of manual
/// `owf-ctl subscribe` debugging sessions, while still bounding an overlay
/// stuck crash-looping against an idle daemon to a handful of parked
/// threads rather than an unbounded pile.
const MAX_SUBSCRIBERS: usize = 8;

/// Registers a new subscriber into an already-locked subscriber list,
/// evicting the oldest registered one first if already at `MAX_SUBSCRIBERS`.
/// Dropping the evicted entry's `Sender` ends that subscriber's `rx.recv()`
/// loop (see `serve_subscriber`), closing its connection.
///
/// Opportunistically prunes already-dead entries first, so a pile of
/// merely-not-yet-reaped subscribers doesn't evict a still-live one ahead of
/// a truly stale one.
///
/// Takes an already-locked `&mut Vec<Subscriber>` (rather than locking a
/// `&Mutex` itself) so `serve_subscriber` can register the new subscriber
/// and read `daemon.state` inside the *same* critical section -- see that
/// function's doc comment for why that's what closes Work Item 3's
/// connect-time race.
fn register_subscriber(subs: &mut Vec<Subscriber>, tx: mpsc::Sender<OverlayEvent>, alive: Arc<AtomicBool>) {
    subs.retain(|s| s.alive.load(Ordering::SeqCst));
    while subs.len() >= MAX_SUBSCRIBERS {
        subs.remove(0);
    }
    subs.push(Subscriber { tx, alive });
}

/// Drops every subscriber whose thread has already exited, without needing
/// to broadcast anything -- see `Subscriber`'s doc comment for why this
/// exists alongside `broadcast_to`'s own incidental pruning. Called from
/// `spawn_housekeeping`'s periodic tick.
fn reap_dead_subscribers(subscribers: &Mutex<Vec<Subscriber>>) {
    lock_ignoring_poison(subscribers).retain(|s| s.alive.load(Ordering::SeqCst));
}

/// Sends `event` to every registered subscriber, dropping any whose
/// receiving end has gone away (the client disconnected, or its thread hit a
/// write error and exited -- see `serve_subscriber`).
///
/// A free function taking `&Mutex<Vec<...>>` rather than a `&Daemon` method
/// so this module's low-level subscriber-list tests can use it without
/// constructing a full `Daemon` -- which would need a real `Recorder`, and
/// thus live audio hardware, just to exist. Every *production* broadcast
/// goes through [`Broadcaster::broadcast`] instead, which calls this and
/// then the in-process sink: calling this directly from production code,
/// the way `IdleOnExit`/`process_utterance` used to,
/// is exactly the bug a code review caught after Task 6 first added the
/// sink -- `Done`, both `Error` variants, the panic-recovery `Idle`, and
/// `NormalizeDegraded`/`NormalizeRecovered` all reached socket subscribers
/// but never the app's overlay, because none of those call sites went
/// through `Daemon::broadcast` at all.
fn broadcast_to(subscribers: &Mutex<Vec<Subscriber>>, event: OverlayEvent) {
    let mut subs = lock_ignoring_poison(subscribers);
    subs.retain(|s| s.tx.send(event.clone()).is_ok());
}

/// The two consumers every broadcast must reach, bundled together so a
/// function that emits an `OverlayEvent` cannot reach the socket-subscriber
/// half without the in-process sink half, or vice versa -- see
/// `broadcast_to`'s doc comment for the incident this exists to prevent.
/// Cheap to construct (one `Arc` clone) and to pass around by value.
#[derive(Clone)]
struct Broadcaster<'a> {
    subscribers: &'a Mutex<Vec<Subscriber>>,
    sink: Arc<dyn EventSink>,
}

impl Broadcaster<'_> {
    fn broadcast(&self, event: OverlayEvent) {
        broadcast_to(self.subscribers, event.clone());
        self.sink.emit(&event);
    }
}

impl Daemon {
    fn broadcast(&self, event: OverlayEvent) {
        self.broadcaster().broadcast(event);
    }

    /// Borrows the two fields `Broadcaster` bundles. Built fresh on every
    /// call rather than cached on `Daemon` -- it only ever needs to live as
    /// long as the caller's own use of it, e.g. for the lifetime of one
    /// `IdleOnExit` guard.
    fn broadcaster(&self) -> Broadcaster<'_> {
        Broadcaster { subscribers: &self.subscribers, sink: Arc::clone(&self.sink) }
    }

    /// Whether a subscriber connecting *right now* should be told
    /// normalization is degraded -- the connect-time counterpart of the live
    /// `NormalizeDegraded` broadcast a failed load sends.
    ///
    /// Requires model residency, not just `!normalize_available`: before lazy
    /// loading, `normalize_enabled && !normalize_available` alone was a
    /// reliable fault signal, because a resident daemon only ever reached
    /// that combination by a real, unhealthy `llama-server`. Lazy unloading
    /// (this task) makes `!normalize_available` the *steady state between
    /// dictations* too -- `unload_models` clears it on every unload, with no
    /// fault involved -- so a subscriber connecting while idle-unloaded would
    /// otherwise be told about a fault that isn't happening: there is no
    /// `llama-server` to be down while nothing is loaded. Gating on
    /// `models_loaded` restores the original meaning: a genuinely broken
    /// `llama-server` still raises this the moment models are next loaded
    /// (`ensure_models_loaded_with`'s own `normalize_available` write runs
    /// before `models_loaded`'s, so the two are never observed out of step --
    /// see `unload_models`'s doc comment for the other half of that
    /// argument).
    ///
    /// Called from both `connect_snapshot` and `serve_subscriber` so they
    /// can never diverge on this question again -- see `connect_snapshot`'s
    /// own doc comment on why the two must "mirror" each other.
    fn normalize_degraded(&self) -> bool {
        self.normalize_enabled
            && self.models_loaded.load(Ordering::SeqCst)
            && !self.normalize_available.load(Ordering::SeqCst)
    }

    /// The events an in-process consumer must see right now, as though it
    /// had just connected -- mirrors what `serve_subscriber` sends a
    /// freshly connected socket subscriber, via the same [`connect_events`].
    ///
    /// Unlike `serve_subscriber`, this needs no critical section: a socket
    /// subscriber's snapshot must be read atomically with *registering* it
    /// into `daemon.subscribers` (otherwise a broadcast landing in the gap
    /// could be missed forever -- see `serve_subscriber`'s doc comment), but
    /// the in-process sink has no equivalent registration step. It already
    /// receives every future broadcast unconditionally, from `Daemon`
    /// construction onward (`Daemon::sink`), so there is nothing to race:
    /// the caller either sees this snapshot reflect a given transition, or
    /// receives that transition as a live event moments later, never both
    /// and never neither.
    ///
    /// This is what the app's `overlay_ready` Tauri command calls once the
    /// webview has registered its own event listener (`src/Overlay.tsx`),
    /// closing the gap where nothing ever *broadcasts* `Warming` (`start`
    /// only stores it) and a warm-up failure broadcast from a background
    /// thread could otherwise outrun that listener's registration.
    pub fn connect_snapshot(&self) -> Vec<OverlayEvent> {
        let state_now = self.state.load(Ordering::SeqCst);
        let degraded = self.normalize_degraded();
        let fatal_error =
            if state_now == FAILED { lock_ignoring_poison(&self.fatal_error).clone() } else { None };
        connect_events(state_now, degraded, fatal_error)
    }
}

/// Stands in for `S1MiniClient` whenever no working `llama-server` backs
/// normalization for this run.
///
/// Two distinct situations reach this, both correctly modelled the same way:
/// `[normalize].enabled = false` (no child was ever spawned at all), or
/// `enabled = true` but `load_models` couldn't get `llama-server` spawned and
/// healthy (C1) -- a missing ggml compute backend, a missing binary, a port
/// conflict outside the retry range, or any other startup failure.
/// `Pipeline::process` already treats a normalizer *error* exactly like a
/// `llama-server` that goes down or times out mid-utterance: log it and fall
/// back to the raw transcript plus the rule-based pass (spec 15). Routing
/// both "never had one" and "tried and failed" through that same, already-
/// correct degraded path is what lets the daemon come up `Idle` and useful
/// in either case, rather than the previous behaviour of `load_models` returning
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
    let logs_dir = crate::debug::expand_tilde(&debug.dir).join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    std::fs::OpenOptions::new().create(true).append(true).open(logs_dir.join("daemon.log"))
}

/// Everything `run()` used to do except the accept loop: acquires the
/// single-instance lock, loads config, sets up tracing, binds and secures
/// the socket, builds the `Daemon`, and spawns the warm-up, housekeeping,
/// and signal-handling threads. Returns the constructed `Daemon` and the
/// bound `UnixListener` so a host that owns its own event loop (the Tauri
/// app) can call this during its `setup()` and then run [`serve`] on a
/// thread of its own, instead of blocking the caller the way `run` does.
///
/// `sink` is the in-process consumer of every broadcast alongside socket
/// subscribers -- see [`EventSink`]. The standalone daemon (`run`, below)
/// passes a sink that discards every event, since it has no in-process
/// consumer of its own.
/// Builds the one German sentence that describes whatever went wrong with the
/// config at startup (spec §3.1), from the two ways it can go wrong.
///
/// Only the parser's **first line** reaches the banner. A `toml` diagnostic is
/// a five-line caret-gutter dump; rendered in a proportional font inside a
/// sentence the gutter does not line up and the useful half -- "unknown field
/// `nope`" -- is buried in the middle of it. The whole diagnostic still goes to
/// the log, where it lines up.
///
/// A failed migration wins over a quarantine when both somehow occur: the
/// legacy file is the one holding the settings the user actually had.
fn startup_config_notice(
    migration: &config::Migration,
    quarantine: Option<&config::Quarantine>,
) -> Option<String> {
    fn first_line(e: &str) -> String {
        e.lines().next().unwrap_or(e).trim().to_string()
    }

    if let config::Migration::LegacyUnreadable { left_at, error } = migration {
        return Some(format!(
            "Die bisherige Konfiguration unter {} konnte nicht gelesen werden und wurde \
             nicht übernommen. Sie liegt unverändert dort; yappr läuft mit \
             Standardwerten. Grund: {}",
            left_at.display(),
            first_line(error),
        ));
    }

    let q = quarantine?;
    Some(match &q.moved_to {
        Some(p) => format!(
            "Die Konfigurationsdatei konnte nicht gelesen werden und wurde nach {} \
             verschoben. yappr läuft mit Standardwerten. Grund: {}",
            p.display(),
            first_line(&q.error),
        ),
        // The rename failed too, so the file is still where it always was --
        // saying it "was moved to" its own path would send the user looking
        // for something that is not there.
        None => format!(
            "Die Konfigurationsdatei konnte nicht gelesen werden und liegt unverändert an \
             ihrem Platz. yappr läuft mit Standardwerten. Grund: {}",
            first_line(&q.error),
        ),
    })
}

pub fn start(sink: Arc<dyn EventSink>) -> Result<(Arc<Daemon>, UnixListener)> {
    // Single-instance guard: an exclusive, non-blocking lock on a runtime
    // file. The `File` is stored in `Daemon::_runtime_lock` so the lock is
    // held for the life of the *daemon*, not just this function -- a local
    // here would drop, and release the exclusive flock with it, the moment
    // `start` returns, letting a second `yappr` race this one for
    // the socket.
    // Before anything else that can be slow: the focused-window tracker has
    // to be listening well before the first `ptt-start`, because what it
    // records is the state *prior* to the overlay taking focus. Spawning a
    // thread that may find no accessibility bus is free.
    crate::winclass::start_tracking();

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
        eprintln!("yappr is already running (lock held on {})", lock_path.display());
        std::process::exit(1);
    }

    // Spec §4: a pre-2026-09-02 config in `~/.config` moves here once, before
    // anything reads the new path. Best-effort -- a migration that fails must
    // not stop the app; it means the user starts from defaults with their old
    // file still sitting exactly where they left it.
    let migration = match config::migrate_from_legacy(
        &paths::legacy_config_file(),
        &paths::config_file(),
    ) {
        Ok(m) => m,
        // An I/O failure mid-migration. Not fatal -- the legacy file is still
        // there and the next start retries, because `write_atomically` means a
        // torn write left nothing at the new path to block it.
        Err(e) => config::Migration::LegacyUnreadable {
            left_at: paths::legacy_config_file(),
            error: format!("{e:#}"),
        },
    };

    // Spec §3: never fatal. This is the only caller that quarantines -- every
    // other `Config::load_from` in this file stays strict, because `Reload`
    // and `SetConfig` reporting a broken file is the honest answer where
    // silently resetting a user's settings would not be.
    let (cfg, quarantine) = config::load_or_quarantine(&paths::config_file());
    let startup_notice = startup_config_notice(&migration, quarantine.as_ref());

    // Loaded before tracing is set up so the log destination (stdout, plus
    // `<debug.dir>/logs/daemon.log` when debug capture is enabled) can
    // depend on `cfg.debug`. Nothing above this point ever logs via
    // `tracing`, so nothing is lost by the reordering.
    init_tracing(&cfg.debug);

    // Before the first `ptt-start`, for the same reason `winclass` is: the
    // `libei` backend's portal session is what raises an approval dialog,
    // and that dialog takes keyboard focus. Raising it at INJECTING, with
    // the overlay up, is invariant 2 head-on. Does nothing unless this is
    // the selected backend, and never fails outwardly -- see
    // `libei::prewarm_if_selected`.
    crate::libei::prewarm_if_selected(&cfg.inject);

    match &migration {
        config::Migration::NotNeeded => {}
        config::Migration::Moved(renamed) => {
            tracing::info!(
                old_file = %renamed.display(),
                "migrated the config out of ~/.config"
            );
        }
        // Logged at error level: the user still has this file and every
        // setting in it, and nothing else is going to mention that yappr
        // just started without any of them.
        config::Migration::LegacyUnreadable { left_at, error } => {
            tracing::error!(
                left_at = %left_at.display(),
                error = %error,
                "the config in ~/.config could not be read; it was NOT migrated"
            );
        }
    }
    if let Some(q) = &quarantine {
        // The full multi-line parser diagnostic goes here, where a caret
        // gutter lines up. The banner gets only its first line.
        tracing::error!(
            moved_to = ?q.moved_to,
            error = %q.error,
            "config could not be loaded; started on defaults"
        );
    }

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
    // whole daemon down with it -- see `warm_up_recorder`'s doc comment, where
    // this attempt now actually happens. `recorder` starts `None` here
    // unconditionally: `Recorder::new` can block for a long time (cpal
    // enumerating devices and probing throwaway streams with no timeout --
    // see CLAUDE.md's cpal gotcha), and this function runs on the caller's
    // own thread, which for the Tauri app is the `setup()`/event-loop
    // thread -- a wedge here would mean no window, no tray, and no way to
    // quit. `ensure_recorder` retries construction lazily on the next
    // `ptt-start` regardless of whether warm-up's own attempt succeeded, so
    // nothing is lost by deferring it but the timing of one log line.

    let daemon = Arc::new(Daemon {
        state: AtomicU8::new(WARMING),
        quitting: AtomicBool::new(false),
        recorder: Mutex::new(None),
        audio_cfg: Mutex::new(cfg.audio.clone()),
        pipeline: Mutex::new(None),
        load_lock: Mutex::new(()),
        models_loaded: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
        models_cfg: Mutex::new(cfg.models.clone()),
        realtime_cfg: Mutex::new(cfg.realtime.clone()),
        window_class: Mutex::new(None),
        config_path: paths::config_file(),
        recording_epoch: AtomicU64::new(0),
        subscribers: Mutex::new(Vec::new()),
        normalize_enabled: cfg.normalize.enabled,
        normalize_available: AtomicBool::new(false),
        fatal_error: Mutex::new(None),
        config_notice: Mutex::new(startup_notice),
        realtime: Mutex::new(None),
        realtime_error: Mutex::new(None),
        housekeeping: Mutex::new(None),
        last_timings: Mutex::new(None),
        sink,
        runtime_socket_path: sock_path,
        runtime_lock_path: lock_path,
        _runtime_lock: lock,
    });

    // Taken before `cfg` is moved into the warm-up thread below.
    let cfg_for_realtime = cfg.realtime.clone();

    // Warm up off the accept loop so `status` answers immediately.
    {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || {
            warm_up_recorder(&cfg, &daemon);

            // The lazy default: startup is finished the moment the recorder
            // attempt is. Nothing model-shaped is loaded, `models_loaded`
            // stays false, and the first `ptt-start` is what brings the
            // models up (`ensure_models_loaded`). `WARMING` still means
            // "startup has not finished" -- it is simply brief here.
            if !cfg.models.preload_at_startup {
                daemon.state.store(IDLE, Ordering::SeqCst);
                daemon.broadcast(OverlayEvent::Idle);
                tracing::info!("ready (models load on demand)");
                return;
            }

            match load_models(cfg, &daemon) {
                Ok((pipeline, available)) => {
                    *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
                    // Settled once, here, and not revisited: an in-process
                    // engine either loaded or it didn't. There is no health
                    // to poll and nothing that can die underneath us, which
                    // is what removed the supervision loop that used to
                    // re-confirm this every 10 s.
                    daemon.normalize_available.store(available, Ordering::SeqCst);
                    daemon.models_loaded.store(true, Ordering::SeqCst);
                    daemon.state.store(IDLE, Ordering::SeqCst);
                    daemon.broadcast(OverlayEvent::Idle);
                    tracing::info!("ready");
                }
                Err(e) => {
                    // The only way `load_models` returns `Err`: ASR/VAD failed
                    // to load. An S1-mini load failure never reaches here (see
                    // `load_models`'s doc comment) -- it's absorbed into
                    // `UnavailableNormalizer` and the daemon still comes up
                    // `Idle`. This is a permanent, fatal failure (Task 3, Work
                    // Item 3): `FAILED` plus the stored reason is what lets a
                    // subscriber connecting *after* this moment still learn the
                    // daemon is broken, instead of `snapshot_event(WARMING)`'s
                    // permanent spinner.
                    //
                    // Reachable only via `preload_at_startup`. The lazy path
                    // treats the same failure as retryable instead -- see
                    // `run_utterance`, and spec 2026-08-29 §4.
                    let reason = format!("warm-up failed: {e}");
                    tracing::error!(error = ?e, "warm-up failed; daemon marked failed");
                    *lock_ignoring_poison(&daemon.fatal_error) = Some(reason.clone());
                    daemon.state.store(FAILED, Ordering::SeqCst);
                    daemon.broadcast(OverlayEvent::Error { reason });
                }
            }
        });
    }

    // The realtime endpoint, if `[realtime] enabled`. After the warm-up
    // thread is spawned rather than before it: binding a port is
    // instantaneous, and a session that connects immediately calls
    // `ensure_models_loaded` itself, so there is nothing here that has to
    // wait for warm-up to finish.
    sync_realtime(&daemon, &cfg_for_realtime);

    // Reaps dead subscribers (Task 3, Work Item 2) and attempts the
    // idle-unload deadline on the same tick (Task 6, `unload_models`).
    // Spawned unconditionally and immediately -- not gated on warm-up
    // finishing -- so subscriber hygiene starts from the very first
    // connection.
    {
        let handle = spawn_housekeeping(Arc::clone(&daemon));
        *lock_ignoring_poison(&daemon.housekeeping) = Some(handle);
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

    tracing::info!(socket = %daemon.runtime_socket_path.display(), "listening");
    Ok((daemon, listener))
}

/// The accept loop `run()` used to end with, moved out so a host that owns
/// its own event loop can run this on a thread of its own instead. Blocks
/// forever (or until the process is torn down by `shutdown`, e.g. on a
/// terminating signal): **never call this on the Tauri event-loop thread** --
/// a blocked event loop is a frozen window and an unclickable tray.
pub fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle(Arc::clone(&daemon), s),
            Err(e) => tracing::warn!(error = ?e, "accept failed"),
        }
    }
}

/// The standalone daemon binary's entry point: `start` plus `serve` with no
/// in-process event consumer, run on the calling thread (`owf-ctl daemon`
/// blocks here for the life of the process, same as before this was split).
pub fn run() -> Result<()> {
    struct NoExtraSink;
    impl EventSink for NoExtraSink {
        fn emit(&self, _: &OverlayEvent) {}
    }
    let (daemon, listener) = start(Arc::new(NoExtraSink))?;
    serve(daemon, listener);
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

/// I7's eager first attempt at `daemon.recorder`, split out of `warm_up` so
/// it still runs at startup even when `[models] preload_at_startup` is off
/// and no model is loaded at all.
///
/// Infallible by design: `ensure_recorder` retries `Recorder::new` lazily on
/// the next `ptt-start` regardless, so nothing is lost by this attempt
/// failing but the timing of one log line. It runs at startup, off the Tauri
/// `setup()` thread, because `Recorder::new` can block for a long time in
/// `cpal` (see CLAUDE.md's cpal gotcha) and a wedge there means no window, no
/// tray, and no way to quit.
fn warm_up_recorder(cfg: &Config, daemon: &Arc<Daemon>) {
    match Recorder::new(&cfg.audio) {
        Ok(r) => *lock_ignoring_poison(&daemon.recorder) = Some(r),
        Err(e) => {
            tracing::error!(
                error = ?e,
                "no capture device at startup; will retry on the next ptt-start"
            );
        }
    }
}

/// Builds the pipeline and, when normalization is enabled, loads S1-mini
/// into this process behind it.
///
/// Called at startup only when `[models] preload_at_startup` is on; otherwise
/// called on demand by `ensure_models_loaded`, off the press that needs it.
/// It therefore must not assume it is running exactly once, or at startup.
///
/// R9: when `cfg.normalize.enabled` is `false`, S1-mini is never loaded at
/// all -- not loaded-then-ignored. Running with `[normalize] enabled = false`
/// is how the rest of the daemon (capture -> VAD -> ASR -> guardrail-fallback
/// -> wtype) is verified without it, and it now also saves the ~480 MB the
/// weights occupy.
///
/// C1: a model that fails to load does not fail this function. Before that
/// fix the failure propagated out via `?`, the caller left `daemon.state` at
/// `WARMING` forever, and every subsequent `ptt-start` answered
/// `{"ok":false,"err":"warming"}` -- permanently, with no retry -- even
/// though ASR, VAD, and the guardrail's raw-text fallback were completely
/// unaffected and spec 15 explicitly calls for exactly that degraded path.
/// Only the ASR/VAD models can still fail this function: there is no
/// raw-fallback path for a missing ASR the way there is for a missing
/// normalizer, so failing loudly there is correct.
///
/// Returns whether normalization is actually available, which the caller
/// stores into `normalize_available`. That used to be `server.is_some()` on
/// a returned child handle; there is no handle any more, because the engine
/// is owned by the `Pipeline`'s normalizer and lives exactly as long as it.
fn load_models(cfg: Config, daemon: &Arc<Daemon>) -> Result<(Pipeline, bool)> {
    let models = paths::models_dir();

    // ASR and VAD first, deliberately: they are the only loads that can
    // fail this function, and failing before spending ~480 MB and ~440 ms
    // on S1-mini is the cheaper order. The previous ordering existed to get
    // `llama-server`'s cold start overlapping the ASR build; with both loads
    // in this process and on this thread there is nothing left to overlap.
    let asr = asr::build(&models, &cfg.asr)?;
    let trimmer = SileroTrimmer::new(&models)?;

    let engine = if cfg.normalize.enabled {
        match LlamaEngine::load(&models.join(MODEL_FILE), &cfg.normalize) {
            Ok(engine) => Some(engine),
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "S1-mini did not load; continuing with normalization disabled \
                     for this run (ASR + guardrail raw-text fallback only)"
                );
                None
            }
        }
    } else {
        tracing::info!("normalize.enabled = false; not loading S1-mini");
        None
    };

    let available = engine.is_some();
    let normalizer: Box<dyn Normalizer> = match engine {
        Some(engine) => Box::new(engine),
        None if cfg.normalize.enabled => {
            Box::new(UnavailableNormalizer("S1-mini failed to load".to_string()))
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
        asr,
        Box::new(trimmer),
        Box::new(WhatlangDetector),
        normalizer,
        injector,
    )
    .with_stage_events(stage_events);
    Ok((pipeline, available))
}

/// Brings the models up if they are not already, and does nothing if they
/// are. Idempotent, safe to call from any thread, and safe to call
/// concurrently: `load_lock` makes two presses share one load rather than
/// racing two.
///
/// Reads its `Config` from disk at the moment of the call rather than from a
/// snapshot taken at startup, as `GetConfig` and `Reload` already do. A
/// consequence worth naming: for a user running lazily, a changed `[asr]` or
/// `[normalize]` setting takes effect on the next dictation rather than the
/// next restart. That is a side effect, not a promise -- `schema.ts`'s
/// `RESTART_SECTIONS` is unchanged because it is still correct whenever the
/// models happen to be resident.
///
/// The read happens inside `load`'s closure body, not above it, so it runs
/// only on the path that actually loads: `ensure_models_loaded_with`'s
/// `models_loaded` fast path returns before `load` is ever called. Hoisting
/// it back out and passing an already-parsed `Config` in would put a
/// fallible disk read and TOML parse on every single utterance on an
/// already-warm daemon, and -- worse than the wasted work -- would make a
/// `config.toml` broken by a hand-edit (invariant 4: even one unknown key is
/// a hard parse failure) turn every later press into a dropped,
/// untranscribed recording, on a daemon whose pipeline was already resident
/// and would have transcribed it fine.
fn ensure_models_loaded(daemon: &Arc<Daemon>) -> Result<(), String> {
    let mut load = || {
        let cfg = Config::load_from(&daemon.config_path).context("config error")?;
        load_models(cfg, daemon)
    };
    ensure_models_loaded_with(daemon, &mut load, &SHUTTING_DOWN)
}

/// [`ensure_models_loaded`] with the loader and the shutdown flag injected --
/// the seam a test uses to exercise the locking, the idempotence and the
/// failure path without an ASR model on disk, in the same shape
/// the injected loader already establishes.
///
/// `shutting_down` is `&SHUTTING_DOWN` in production, and the sole reason it
/// is a parameter is that `SHUTTING_DOWN` is a process-global static: a test
/// that set it to drive the re-check below would make every *other* test in
/// the same binary that happened to be loading at that moment take the
/// teardown path and fail, and `cargo test` runs them in parallel by
/// default. Injecting a local `AtomicBool` instead removes the global from
/// the test path entirely rather than serialising around it. The one thing
/// that leaves unproven is the wiring itself -- that `ensure_models_loaded`
/// passes the *real* static -- which is a single literal argument above.
///
/// On failure it installs nothing at all: `pipeline` stays `None`,
/// `models_loaded` stays `false`, and `state` is left alone. That is what
/// makes a lazy load failure retryable on the next press (spec 2026-08-29 §4)
/// rather than the permanent `FAILED` a startup failure still produces.
///
/// A load that finishes after `shutdown` has begun is released again rather
/// than installed -- see the `SHUTTING_DOWN` re-check below, which is the
/// only thing standing between a cancelled press and an orphaned
/// `llama-server`.
fn ensure_models_loaded_with(
    daemon: &Arc<Daemon>,
    load: &mut dyn FnMut() -> Result<(Pipeline, bool)>,
    shutting_down: &AtomicBool,
) -> Result<(), String> {
    let _guard = lock_ignoring_poison(&daemon.load_lock);
    if daemon.models_loaded.load(Ordering::SeqCst) {
        return Ok(());
    }
    match load() {
        Ok((pipeline, available)) => {
            *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
            daemon.normalize_available.store(available, Ordering::SeqCst);
            // Last, and only on the success path: this is what `Status`
            // reads, so it must never be true before the pipeline is
            // installed.
            daemon.models_loaded.store(true, Ordering::SeqCst);
            // Re-checked here, *after* the install, still holding
            // `load_lock`: a load that finished after teardown began must
            // release what it just built rather than leave it resident.
            //
            // This used to carry a much longer argument about an orphaned
            // `llama-server`. A load spawned the child several seconds
            // before it returned, `shutdown` deliberately does not take
            // `load_lock` (that would hang Beenden behind an in-flight
            // load), and `std::process::exit(0)` follows `shutdown` within
            // milliseconds -- so a load still inside `load_models` never
            // reached this re-check at all, and the ~955 MB child it had
            // already spawned survived the daemon that started it. Silently:
            // the only symptom was the memory this feature exists to
            // reclaim never coming back.
            //
            // An in-process engine cannot be orphaned. There is no child to
            // outlive us; the weights are this process's own heap, and the
            // kernel reclaims them at `exit` whether or not this arm ever
            // runs. What remains here is the ordinary case -- freeing
            // promptly rather than sitting on half a gigabyte until the
            // process actually goes away.
            //
            // `SHUTTING_DOWN`, not `Daemon::quitting`: `quitting` is set only
            // by the `Request::Quit` arm, so it would miss `SIGTERM` at
            // logout and Tauri's `RunEvent::Exit`, both of which call
            // `shutdown` directly. `SHUTTING_DOWN` is set by `shutdown`
            // itself -- the one point all four teardown routes converge on.
            if shutting_down.load(Ordering::SeqCst) {
                // Same order, and the same steps, as `unload_models`.
                daemon.models_loaded.store(false, Ordering::SeqCst);
                *lock_ignoring_poison(&daemon.pipeline) = None;
                daemon.normalize_available.store(false, Ordering::SeqCst);
                tracing::info!("models finished loading during shutdown; released again");
                return Err("shutting down".to_string());
            }
            tracing::info!("models loaded");
            Ok(())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The daemon, as the realtime endpoint sees it.
///
/// A `Weak`, not an `Arc`: the listener's threads outlive nothing, but they
/// are the only holders of this handle, and a strong reference here would
/// make the `Daemon` -- and with it the loaded models -- unreachable for
/// `Drop` while a session thread is still winding down. Every method
/// degrades to a stated error if the daemon is already gone, which is what
/// a session sees when the app quits underneath it.
struct DaemonEngine(std::sync::Weak<Daemon>);

impl DaemonEngine {
    fn daemon(&self) -> Result<Arc<Daemon>, String> {
        self.0.upgrade().ok_or_else(|| "yappr is shutting down".to_string())
    }
}

impl RealtimeEngine for DaemonEngine {
    fn ensure_ready(&self) -> Result<(), String> {
        let daemon = self.daemon()?;
        // The same call `run_utterance` makes, with the same retry
        // semantics: a failure here is this session's problem, never a
        // latched `FAILED` for the whole app (invariant 12).
        ensure_models_loaded(&daemon)
    }

    fn segmenter(&self) -> Result<Box<dyn Segmenter>, String> {
        let daemon = self.daemon()?;
        let cfg = lock_ignoring_poison(&daemon.realtime_cfg).clone();
        SileroSegmenter::new(&paths::models_dir(), cfg.silence_ms, cfg.max_utterance_seconds)
            .map(|s| Box::new(s) as Box<dyn Segmenter>)
            .map_err(|e| format!("{e:#}"))
    }

    fn transcribe(&self, samples: &[f32]) -> Result<Option<String>, String> {
        let daemon = self.daemon()?;
        // Held for the whole call, exactly as `process_utterance` holds it,
        // and for the same reason: `SherpaTranscriber` is `Sync` only by a
        // bare `unsafe impl`, and one utterance at a time inside the
        // recognizer is what this lock guarantees. The consequence worth
        // knowing is that a realtime utterance and a push-to-talk one
        // serialise against each other -- dictating in two places at once
        // makes the second wait, rather than corrupting either.
        let guard = lock_ignoring_poison(&daemon.pipeline);
        let Some(p) = guard.as_ref() else {
            return Err("the models are not loaded".to_string());
        };
        p.dictate_text(samples, p.config().realtime.normalize)
            .map_err(|e| format!("{e:#}"))
    }

    fn touch(&self) {
        if let Ok(daemon) = self.daemon() {
            touch_activity(&daemon);
        }
    }

    fn describe(&self) -> EngineInfo {
        let Ok(daemon) = self.daemon() else {
            return EngineInfo { model: String::new(), normalize: false };
        };
        // Answered from the resident pipeline when there is one, and from
        // the file otherwise. Both are honest; the pipeline's copy is the
        // one that will actually run this session's audio, which is what a
        // client logging the answer wants.
        let from_pipeline = lock_ignoring_poison(&daemon.pipeline).as_ref().map(|p| {
            let cfg = p.config();
            EngineInfo {
                model: asr_model_id(cfg.asr.model),
                normalize: cfg.normalize.enabled && cfg.realtime.normalize,
            }
        });
        from_pipeline.unwrap_or_else(|| match Config::load_from(&daemon.config_path) {
            Ok(cfg) => EngineInfo {
                model: asr_model_id(cfg.asr.model),
                normalize: cfg.normalize.enabled && cfg.realtime.normalize,
            },
            Err(_) => EngineInfo { model: String::new(), normalize: false },
        })
    }
}

/// `[asr] model` as the string `config.toml` spells it, for the `ready`
/// frame.
///
/// `AsrModel` has no `Display`, and the serde rename is the only spelling
/// that is a *contract* -- it is what the user's config file says and what
/// `openclaw-plugin`'s `models` list has to match. Going through serde
/// rather than a hand-written match is what keeps a fifth model from
/// silently reporting the wrong name here.
fn asr_model_id(model: config::AsrModel) -> String {
    serde_json::to_value(model)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Brings the realtime listener in line with `cfg`, starting it, stopping
/// it, or restarting it on a new port as needed.
///
/// Called from `start` and from every accepted `SetConfig`/`Reload`, which
/// is what makes `[realtime]` take effect without a restart -- and that is
/// not a nicety: the OpenClaw card's install button flips
/// `realtime.enabled` as its last step, and an endpoint that only came up
/// on the next launch would make the button's own success message a lie.
///
/// A failed bind is logged and left as `None`. It is a real state the
/// settings window reports ("enabled, not running") rather than an error
/// that fails the save: the commonest cause is a port already in use, and
/// refusing the save would leave the user unable to change the port that
/// caused it.
fn sync_realtime(daemon: &Arc<Daemon>, cfg: &RealtimeConfig) {
    *lock_ignoring_poison(&daemon.realtime_cfg) = cfg.clone();
    let mut slot = lock_ignoring_poison(&daemon.realtime);

    if let Some(running) = slot.as_ref() {
        if running.config() == cfg {
            return; // nothing about the endpoint changed
        }
        if let Some(h) = slot.take() {
            h.stop();
        }
    }
    if !cfg.enabled {
        *lock_ignoring_poison(&daemon.realtime_error) = None;
        return;
    }
    let engine = Arc::new(DaemonEngine(Arc::downgrade(daemon)));
    match crate::realtime::start(cfg, engine) {
        Ok(handle) => {
            *slot = Some(handle);
            *lock_ignoring_poison(&daemon.realtime_error) = None;
        }
        Err(e) => {
            // Kept, not just logged. "Enabled but not listening" is the one
            // realtime state a user cannot diagnose by looking -- the port
            // is almost always already in use -- and the settings window
            // has no other way to learn which it was.
            let reason = format!("{e:#}");
            tracing::error!(error = %reason, "the realtime endpoint could not start");
            *lock_ignoring_poison(&daemon.realtime_error) = Some(reason);
        }
    }
}

/// What the settings window's OpenClaw card reports: whether the endpoint is
/// configured on, which port it was asked for, and whether it is actually
/// listening.
///
/// The third is not the first: a port already in use leaves `enabled` true
/// and `running` false, and that gap is precisely the thing a user needs
/// told. A plain method rather than a `Request` variant because the caller
/// is in-process (`src-tauri/src/openclaw.rs`) and this adds nothing to the
/// wire format.
pub fn realtime_status(daemon: &Daemon) -> RealtimeStatus {
    let cfg = lock_ignoring_poison(&daemon.realtime_cfg).clone();
    let running = lock_ignoring_poison(&daemon.realtime)
        .as_ref()
        .map(|h| h.addr().port())
        .unwrap_or(0);
    RealtimeStatus {
        enabled: cfg.enabled,
        port: if running != 0 { running } else { cfg.port },
        running: running != 0,
        error: lock_ignoring_poison(&daemon.realtime_error).clone(),
    }
}

/// [`realtime_status`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealtimeStatus {
    pub enabled: bool,
    /// The port actually bound when `running`, the configured one otherwise
    /// -- so a card showing this never names a port nothing is on.
    pub port: u16,
    pub running: bool,
    /// Why the last start attempt failed, when `enabled && !running`.
    ///
    /// The token deliberately has no field here, not even as a "is one
    /// set" bool: the one caller that needs to know reads `[realtime]`
    /// itself, because the question it is really asking is "would a re-run
    /// of the install write something different", and that is a question
    /// about the file the install reads.
    pub error: Option<String>,
}

/// Guards `shutdown` against running its cleanup twice.
///
/// In the intended path this is redundant -- `shutdown` is followed
/// immediately by `std::process::exit`, so the signal-handling thread never
/// loops back to act on a second signal -- but a defensive, explicit guard
/// costs one atomic and documents the idempotence requirement (fix 2) rather
/// than leaving it as an accident of control flow that a future edit could
/// break.
///
/// Read by one other place: `ensure_models_loaded_with`, which uses it to
/// tell "the load finished normally" from "the load finished after teardown
/// began, and its `llama-server` must be released rather than installed".
/// That is why this is the flag to check there rather than
/// `Daemon::quitting` -- this one is set by `shutdown` itself, so it covers
/// all four teardown routes (signal, `Request::Quit`, `RunEvent::Exit`, and
/// a direct call), where `quitting` covers only `Request::Quit`.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Explicit, deterministic teardown, run on `SIGTERM`/`SIGINT`/`SIGHUP`
/// (`start`'s signal-handling thread), on `Request::Quit`
/// (`wait_for_busy_to_clear_then_shutdown`, below -- spec §8's Beenden/`--quit`), and,
/// from the Tauri app, on `RunEvent::Exit` (`src-tauri/src/lib.rs`) so a
/// compositor-issued window close that lets Tauri's own event loop exit
/// still tears this down rather than orphaning `llama-server`.
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
/// second time round even without that guard -- deliberately, now that there
/// are four call sites instead of one and a redundant call from any of them
/// must stay harmless.
///
/// `pub`: called directly from `src-tauri` (a different crate) for the
/// `RunEvent::Exit` case above, which has no `Request` to dispatch through.
pub fn shutdown(daemon: &Daemon) {
    if SHUTTING_DOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!("shutting down");
    // Joined, not just signalled: once this returns the housekeeping thread
    // has fully exited. With the `llama-server` supervisor gone, this loop
    // only reaps subscribers and attempts the idle-unload deadline, so
    // there is no longer a restart attempt that could hold it up -- a stop
    // signal is acted on as soon as the current tick's work finishes.
    stop_housekeeping(&daemon.housekeeping);
    // Before the recorder and the runtime files, for one reason: a session
    // in `dictate_text` holds the pipeline lock, and stopping the listener
    // is what stops new ones arriving to queue behind it. In-flight
    // sessions end on their own within `READ_TIMEOUT`.
    if let Some(h) = lock_ignoring_poison(&daemon.realtime).take() {
        h.stop();
    }
    if let Some(r) = lock_ignoring_poison(&daemon.recorder).as_ref() {
        let _ = r.stop();
    }
    // Spec §8 step 3, before removing the runtime socket/lock (step 4).
    // A no-op for every sink but
    // `TauriSink`, and idempotent the same way the rest of this function is
    // -- ksni's own `Handle::shutdown` is just an atomic flag set.
    daemon.sink.unregister_tray();
    remove_runtime_files(&daemon.runtime_socket_path, &daemon.runtime_lock_path);
}

/// Stops the housekeeping thread, if one was ever installed. Split out so it
/// is testable directly with a scratch
/// `Mutex<Option<HousekeepingHandle>>` instead of a full `Daemon`.
/// `Option::take` leaves `None` behind, so a second call is a no-op -- half
/// of `shutdown`'s idempotence, matching `kill_llama`'s own.
fn stop_housekeeping(housekeeping: &Mutex<Option<HousekeepingHandle>>) {
    if let Some(handle) = lock_ignoring_poison(housekeeping).take() {
        handle.stop();
    }
}

/// Removes the socket and lock files this daemon owns, so a fresh daemon can
/// start immediately afterward instead of finding a stale socket or being
/// told the (now-dead) lock is still held. Best-effort: a file that is
/// already gone (a second call, or it was never created) is not an error.
///
/// Takes explicit paths -- rather than calling `paths::runtime_*()` itself --
/// so `shutdown` can be exercised in a test against `Daemon::runtime_*_path`
/// scratch values instead of always resolving to
/// `$XDG_RUNTIME_DIR/yappr.sock`, which a real daemon elsewhere on
/// the same machine may be holding open at the moment the test suite runs.
fn remove_runtime_files(socket: &Path, lock: &Path) {
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(lock);
}

// -- Request::Quit (Task 10 / spec §8) --------------------------------------

/// Spec §8 step 1: Beenden/`--quit` must not tear down while an utterance is
/// in flight (`is_busy`) -- invariant 1 says a transcribed utterance is never
/// lost, and `shutdown` stops housekeeping and kills `llama-server`, either
/// of which could pull the rug out from under a `Normalizing`/`Injecting`
/// stage that hasn't finished yet. This bounds how long `wait_for_busy_to_
/// clear` will wait for that stage to finish on its own, so a wedged
/// pipeline (a `llama-server` or `wtype` call that never returns despite
/// invariant 6's per-subprocess timeouts) cannot make Beenden unresponsive
/// forever -- past this bound it shuts down anyway.
const QUIT_BUSY_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often `wait_for_busy_to_clear` re-checks the state while waiting.
const QUIT_BUSY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Blocks until `state` has left the busy set (`is_busy`), or until `timeout`
/// has elapsed, whichever comes first.
///
/// A pure, `Daemon`-free helper over a bare `&AtomicU8` -- deliberately, so a
/// test can drive it directly with tiny durations and assert on the waiting
/// itself, without ever going anywhere near `wait_for_busy_to_clear_then_shutdown`'s
/// real `shutdown` call or the `std::process::exit` that follows it in the
/// `Request::Quit` dispatch arm. Calling `std::process::exit` from a test
/// would kill the test *runner*, not just the "daemon" under test, so the
/// exit is kept at that one call site and nowhere near anything this file's
/// test module invokes.
fn wait_for_busy_to_clear(state: &AtomicU8, timeout: Duration, poll: Duration) {
    let deadline = Instant::now() + timeout;
    while is_busy(state.load(Ordering::SeqCst)) && Instant::now() < deadline {
        std::thread::sleep(poll);
    }
}

/// `Request::Quit`'s real work: wait (bounded) for any in-flight utterance to
/// leave the busy states, then run `shutdown`. Split out from the dispatch
/// arm below purely so the *waiting* half is reachable without the `exit`
/// that must immediately follow it in production -- see
/// `wait_for_busy_to_clear`'s doc comment for why that split matters for
/// testing.
///
/// Deliberately does **not** wait on `RECORDING`, even though it is not
/// `IDLE` either -- `is_busy` (what this waits on) was already narrower than
/// "not idle" before this task, and that is correct here too. Invariant 1
/// protects text ASR has already *produced*; nothing has been transcribed
/// yet while merely `RECORDING`, so tearing down and discarding that audio
/// is not the loss invariant 1 forbids. It is also the behaviour a user
/// clicking Beenden mid-recording is actually asking for -- stop now, not
/// "finish transcribing what I've said so far first". `Daemon::quitting`
/// (set by the `Request::Quit` arm before this runs) independently closes
/// the *other* half of this: no *new* recording can start once quitting is
/// set, so this never has to choose between waiting on `RECORDING` and
/// racing a fresh one. Do not "fix" this into waiting on `RECORDING` too --
/// that would make Beenden hang for however long the user has been
/// recording, entirely defeating the point of a responsive quit.
fn wait_for_busy_to_clear_then_shutdown(daemon: &Daemon) {
    wait_for_busy_to_clear_then_shutdown_with(daemon, QUIT_BUSY_WAIT_TIMEOUT, QUIT_BUSY_POLL_INTERVAL);
}

/// `wait_for_busy_to_clear_then_shutdown` with an injectable timeout/poll,
/// for the same reason `wait_for_busy_to_clear` itself takes them: so a test
/// can observe both "still busy => shutdown hasn't run yet" and "bound
/// elapsed => shutdown ran anyway" against a `sleep 300` stub `llama-server`
/// in milliseconds, rather than waiting out the real ~10 s
/// `QUIT_BUSY_WAIT_TIMEOUT`. Production (`wait_for_busy_to_clear_then_shutdown`,
/// above, and thus the `Request::Quit` dispatch arm) always calls this with
/// the real constants.
fn wait_for_busy_to_clear_then_shutdown_with(daemon: &Daemon, timeout: Duration, poll: Duration) {
    wait_for_busy_to_clear(&daemon.state, timeout, poll);
    if is_busy(daemon.state.load(Ordering::SeqCst)) {
        tracing::warn!(
            timeout = ?timeout,
            "quit: still busy after the wait bound; shutting down anyway (spec 8)"
        );
    }
    shutdown(daemon);
}

/// [`Request::Restart`]'s real work: everything `Request::Quit` does, and
/// then start the successor. Split out from the dispatch arm for exactly the
/// reason the quit pair above is -- so the *ordering* is reachable in a test
/// without the `std::process::exit` that must follow it in production.
///
/// The order of these two lines is the entire feature. `shutdown` unlinks
/// `yappr.lock`; only after that can the successor take an uncontended
/// `flock` on a freshly created one. Swap them -- or hoist the relaunch up
/// beside the `quitting` store, or reach for Tauri's `AppHandle::restart`,
/// which spawns first and tears down second -- and the successor loses the
/// lock, prints "yappr is already running", exits 1, and then this process
/// exits too: a restart button that reliably closes the app for good.
fn wait_for_busy_to_clear_then_shutdown_and_relaunch(daemon: &Daemon) {
    wait_for_busy_to_clear_then_shutdown_and_relaunch_with(
        daemon,
        QUIT_BUSY_WAIT_TIMEOUT,
        QUIT_BUSY_POLL_INTERVAL,
    );
}

/// [`wait_for_busy_to_clear_then_shutdown_and_relaunch`] with an injectable
/// timeout/poll, for the same reason its quit-only counterpart takes them.
fn wait_for_busy_to_clear_then_shutdown_and_relaunch_with(
    daemon: &Daemon,
    timeout: Duration,
    poll: Duration,
) {
    wait_for_busy_to_clear_then_shutdown_with(daemon, timeout, poll);
    daemon.sink.relaunch();
}

// -- subscriber housekeeping (Task 3, Work Item 2) and the idle-unload
// deadline (Task 6) --------------------------------------------------------
//
// This block used to also carry `llama-server` supervision: a 10 s health
// poll, a 1 -> 30 s restart backoff, and the zombie reaping that went with
// them. An in-process engine has no health to poll and no child to reap, so
// all of it is gone -- along with `HEALTH_POLL_INTERVAL`, `INITIAL_BACKOFF`,
// `MAX_BACKOFF`, `RESTART_HEALTH_TIMEOUT`, `next_backoff`,
// `supervise_llama_once` and `should_supervise_llama`.

/// How often the housekeeping loop wakes to reap dead subscribers.
const SUBSCRIBER_REAP_INTERVAL: Duration = Duration::from_secs(10);

struct HousekeepingHandle {
    stop_tx: mpsc::Sender<()>,
    join: std::thread::JoinHandle<()>,
}

impl HousekeepingHandle {
    /// Signals the loop to stop and waits for it to actually exit.
    ///
    /// The wait is interrupted immediately: the loop sleeps via
    /// `stop_rx.recv_timeout`, which this wakes the instant `stop_tx` sends,
    /// well before the wait would otherwise time out on its own -- it is
    /// this, not the eventual `std::process::exit` in `main`'s signal
    /// handler, that makes "shutdown during a long wait terminates promptly"
    /// true, and provable in a test that never calls `std::process::exit`.
    /// Nothing else can block `join` for any real time: the only work a tick
    /// does is reaping subscribers and an idle unload, both of which are
    /// prompt.
    fn stop(self) {
        let _ = self.stop_tx.send(());
        let _ = self.join.join();
    }
}

/// The floor on an unload-derived housekeeping tick.
///
/// A deadline that has already passed leaves `remaining` at zero, and
/// `unload_models` can still decline once it has the lock -- it re-reads the
/// state and the deadline under `load_lock`, so a press landing between this
/// calculation and that re-check (or a lazy load holding `load_lock` while
/// this tick runs) leaves the deadline passed and nothing unloaded. Without
/// this floor the housekeeping loop would spin at a zero timeout until that
/// resolved. One second is invisible against a timeout measured in minutes.
const MIN_UNLOAD_TICK: Duration = Duration::from_secs(1);

/// Whether an idle unload may act on a daemon in this state at all.
///
/// `PAUSED` counts as idle deliberately. It is reached only by a CAS from
/// `IDLE` (Task 13), so there is never an utterance to protect while paused,
/// and a user who has explicitly paused dictation is the clearest possible
/// signal that the models are not about to be needed.
///
/// Shared by `should_unload`, which acts on the deadline, and
/// `time_until_unload`, which decides whether the housekeeping loop should
/// wake early for it: two copies of "idle means `IDLE` or `PAUSED`" would be
/// free to drift, and a `time_until_unload` that answered `Some` for a state
/// `should_unload` refuses is exactly the busy-spin the fix below removes.
fn state_permits_unload(state: u8) -> bool {
    state == IDLE || state == PAUSED
}

/// How long until the idle-unload deadline, or `None` when there is nothing
/// to wake early for: nothing is loaded, unloading is disabled, or the
/// daemon is in a state no unload may act on.
///
/// This is what makes `idle_unload_seconds = 60` mean 60 seconds rather than
/// "somewhere in 60-70" at the mercy of `SUBSCRIBER_REAP_INTERVAL`: the
/// housekeeping loop takes the smaller of its own wait and this.
///
/// The state is part of the answer, not just the deadline, and that is the
/// whole point of the `state_permits_unload` check: `unload_models` declines
/// in every other state, so a busy daemon whose deadline has passed would
/// otherwise get `MIN_UNLOAD_TICK` back on every tick -- and the loop takes
/// `wait.min(..)` of it *unconditionally*, including on the arm carrying
/// spec 15's 1 -> 2 -> 4 -> ... -> 30 s `llama-server` restart backoff. With
/// a `llama-server` that fails fast (`wait_healthy` polls `Child::try_wait`,
/// so a missing ggml backend is detected in milliseconds), that backoff
/// collapses to a fresh fork/exec every single second for the whole length
/// of the recording -- roughly 30 attempts across a 40 s dictation instead
/// of about four, on the thread that shares this machine with live audio
/// capture.
///
/// Returning `None` while busy costs the unload almost nothing. `IdleOnExit`'s
/// `Drop` refreshes `last_activity` *before* it stores `IDLE`, so by the
/// moment this can answer `Some` again the deadline is a full
/// `idle_unload_seconds` away -- there is no passed deadline sitting there
/// waiting to be honoured. From the loop's next wake onward the `wait.min(..)`
/// converges on the deadline exactly, as it does for a daemon that was never
/// busy at all.
///
/// The one thing it does cost: the loop may already be inside a long
/// `recv_timeout` it entered while busy, and it cannot be woken early from
/// here. So an unload whose deadline falls inside that sleep is late by
/// whatever is left of it -- at most `HEALTH_POLL_INTERVAL`, or `MAX_BACKOFF`
/// with `llama-server` down. That only bites an `idle_unload_seconds` shorter
/// than the sleep, and being 10 s late to reclaim memory is a straight trade
/// against fork/exec-ing a dead `llama-server` once a second for the whole
/// recording.
///
/// Reads `models_loaded` and `state`, never the `pipeline` lock -- this runs
/// on the same thread that reaps dead subscribers, and blocking it behind a
/// transcription would stall that unrelated work for the whole utterance.
fn time_until_unload(daemon: &Daemon) -> Option<Duration> {
    let idle_seconds = lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds;
    if idle_seconds == 0 || !daemon.models_loaded.load(Ordering::SeqCst) {
        return None;
    }
    if !state_permits_unload(daemon.state.load(Ordering::SeqCst)) {
        return None;
    }
    let elapsed = Instant::now().duration_since(*lock_ignoring_poison(&daemon.last_activity));
    let remaining = Duration::from_secs(idle_seconds as u64).saturating_sub(elapsed);
    Some(remaining.max(MIN_UNLOAD_TICK))
}

/// Whether the idle-unload deadline has passed and it is safe to act on it.
///
/// Pure, and therefore directly unit-testable, for the same reason
/// `should_emit_level` is: the interesting part is the boundary conditions,
/// not the clock.
///
/// Which states qualify -- and why `PAUSED` is one of them -- lives in
/// `state_permits_unload`, shared with `time_until_unload`.
fn should_unload(
    last_activity: Instant,
    now: Instant,
    idle_seconds: u32,
    state: u8,
    loaded: bool,
) -> bool {
    if !loaded || idle_seconds == 0 {
        return false;
    }
    if !state_permits_unload(state) {
        return false;
    }
    now.duration_since(last_activity) >= Duration::from_secs(idle_seconds as u64)
}

/// Releases the models, if the deadline still says to. Returns whether it
/// actually did.
///
/// Takes `&Daemon` rather than the seven bare pieces it touches, which
/// `fake_daemon` makes perfectly testable -- `claim_busy` already takes one
/// the same way. The pieces-not-`&Daemon` convention elsewhere in this file
/// exists for functions a test cannot otherwise reach without live audio
/// hardware; this is not one of them.
///
/// The re-check under `load_lock` is what makes this race-free.
/// `start_recording` refreshes `last_activity` *before* it stores
/// `RECORDING`, so a recording that begins between the housekeeping thread's
/// decision and its acquisition of the lock has already moved the deadline,
/// and this declines. The worst remaining outcome is an unload immediately
/// followed by the load that same press requested: wasteful for one
/// dictation, never incorrect.
///
/// Nothing is broadcast. The daemon's `State` does not change -- it was
/// `IDLE` before and is `IDLE` after -- and the tray's icon is driven
/// entirely by broadcasts (`tray.rs`, "Icon and daemon state"), so an event
/// here would mean inventing a tray icon for it too.
fn unload_models(daemon: &Daemon) -> bool {
    let _guard = lock_ignoring_poison(&daemon.load_lock);

    let idle_seconds = lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds;
    if !should_unload(
        *lock_ignoring_poison(&daemon.last_activity),
        Instant::now(),
        idle_seconds,
        daemon.state.load(Ordering::SeqCst),
        daemon.models_loaded.load(Ordering::SeqCst),
    ) {
        return false;
    }

    // Cleared first: it is what `Status` reads, and it must never still say
    // "loaded" while the pipeline is being torn out.
    daemon.models_loaded.store(false, Ordering::SeqCst);
    // Dropping the pipeline is what frees S1-mini's ~480 MB: the engine is
    // owned by the pipeline's normalizer, so there is no separate teardown
    // step -- this line replaces the `kill_llama` that used to follow it.
    *lock_ignoring_poison(&daemon.pipeline) = None;
    // Not `NormalizeDegraded`: normalization is not degraded, it is not
    // currently loaded, and the overlay's degraded badge means the former.
    daemon.normalize_available.store(false, Ordering::SeqCst);
    tracing::info!(idle_seconds, "models unloaded after idle timeout");
    true
}

/// Reaps dead subscribers on a periodic tick (Task 3, Work Item 2) and
/// attempts the idle-unload deadline on the same tick (Task 6,
/// `unload_models`).
///
/// Spawned unconditionally and immediately from `main` -- not gated on
/// `load_models` finishing -- so subscriber reaping starts right away.
///
/// It used to take a `NormalizeConfig` and supervise the `llama-server`
/// child here too: restart a missing or unhealthy one with exponential
/// backoff, poll a healthy one every 10 s, never touch one already healthy.
/// With S1-mini in this process there is no child, so the parameter and the
/// whole second half of the loop are gone; what is left never needs to know
/// anything about normalization.
fn spawn_housekeeping(daemon: Arc<Daemon>) -> HousekeepingHandle {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let join = std::thread::spawn(move || loop {
        // Wake for whichever comes first: this loop's own business, or the
        // idle-unload deadline. Without this, a 60 s timeout would fire
        // whenever `SUBSCRIBER_REAP_INTERVAL` next happened to come round.
        let wait = time_until_unload(&daemon)
            .map_or(SUBSCRIBER_REAP_INTERVAL, |d| SUBSCRIBER_REAP_INTERVAL.min(d));
        match stop_rx.recv_timeout(wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        reap_dead_subscribers(&daemon.subscribers);

        // Gated on the atomic, not called unconditionally: `unload_models`
        // itself would happily re-check and decline, but it does that
        // *after* unconditionally acquiring `load_lock` -- and a lazy load
        // holds that same lock for as long as `load_models` takes, with
        // `models_loaded` false for the whole of it. Without this
        // pre-check, a tick landing during that window would block this
        // thread on `load_lock` until the load finished -- delaying
        // `reap_dead_subscribers` and, via `HousekeepingHandle::stop`,
        // shutdown itself, for the load's duration. Reading the atomic
        // first keeps this thread off `load_lock` entirely whenever
        // nothing is loaded, which is exactly when a load could be the
        // thing holding it.
        if daemon.models_loaded.load(Ordering::SeqCst) {
            unload_models(&daemon);
        }
    });
    HousekeepingHandle { stop_tx, join }
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

/// How often a subscriber's own thread checks whether its socket peer is
/// still there, when no `OverlayEvent` has arrived to write in the
/// meantime. This is what lets a disconnected subscriber be detected (and
/// its `Subscriber::alive` flag cleared) even while the daemon sits `Idle`
/// broadcasting nothing at all (Task 3, Work Item 2) -- periodically
/// re-broadcasting the current snapshot instead would work for detection
/// too, but would also re-arm the overlay's `Done`/`Error`/`BusyRejected`
/// flash timers (or cut a flash short by replacing it with a stale `Idle`)
/// every time it fired, which is not acceptable collateral damage for a
/// pure housekeeping mechanism. A local, already-established Unix socket
/// makes this cheap: a 1 ms non-blocking peek, not a real wait.
const SUBSCRIBER_LIVENESS_POLL: Duration = Duration::from_millis(500);

/// Serves one `Request::Subscribe` connection for as long as it stays open:
/// an immediate snapshot of the daemon's current state (plus a
/// `NormalizeDegraded` replay if normalization is currently down -- a late
/// joiner must be able to learn that too, not just a client that happened to
/// be connected at the moment it went down), then every subsequent
/// `OverlayEvent` the daemon broadcasts, one NDJSON line each.
///
/// Registers this subscriber's `Sender` and reads the state snapshot in one
/// critical section, under `daemon.subscribers`'s own lock -- `broadcast_to`
/// takes that same lock before it iterates, so no transition's broadcast can
/// land in the gap between "read the snapshot" and "start receiving
/// broadcasts" and be missed until some later broadcast (Task 3, Work Item
/// 3's connect-time race). Either `broadcast_to`'s whole critical section
/// completes before this one starts (so this snapshot's `daemon.state.load`
/// already reflects it), or after (so it's delivered live once `rx` is
/// registered).
///
/// Always runs on its own thread (spawned from `handle`, never inline in the
/// accept loop) -- see the comment at that call site for why that's what
/// keeps the accept loop's single-threaded invariant intact.
fn serve_subscriber(daemon: Arc<Daemon>, stream: UnixStream) {
    let mut w = stream;
    let (tx, rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));

    // Clears `alive` on *every* exit from this function -- a write failure,
    // the liveness poll noticing the peer is gone, or the early return below
    // -- so `reap_dead_subscribers`/`register_subscriber`'s opportunistic
    // prune can find this entry even though nothing was ever broadcast after
    // it died.
    struct ClearAliveOnDrop(Arc<AtomicBool>);
    impl Drop for ClearAliveOnDrop {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let _clear_alive = ClearAliveOnDrop(Arc::clone(&alive));

    let (state_now, degraded) = {
        let mut subs = lock_ignoring_poison(&daemon.subscribers);
        register_subscriber(&mut subs, tx, alive);
        (daemon.state.load(Ordering::SeqCst), daemon.normalize_degraded())
    };

    let fatal_error =
        if state_now == FAILED { lock_ignoring_poison(&daemon.fatal_error).clone() } else { None };
    for event in connect_events(state_now, degraded, fatal_error) {
        if write_event(&mut w, &event).is_err() {
            return;
        }
    }

    // Blocks here between events, waking early to check the socket only
    // when nothing has arrived within `SUBSCRIBER_LIVENESS_POLL`. That costs
    // this thread's stack and a rare, cheap syscall, and nothing else: it
    // holds no lock the rest of the daemon needs, and `Daemon::broadcast`'s
    // `Sender::send` never blocks *its* caller waiting for this end to catch
    // up (`mpsc` channels are unbounded).
    loop {
        match rx.recv_timeout(SUBSCRIBER_LIVENESS_POLL) {
            Ok(event) => {
                if write_event(&mut w, &event).is_err() {
                    // The client disconnected, or `write_timeout` (set by
                    // `handle` before handing off this connection) tripped
                    // on a client too slow to keep up. Either way: drop the
                    // connection and let this thread end.
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if socket_peer_gone(&mut w) {
                    return;
                }
            }
            // This subscriber's `Sender` (the one registered above) is
            // gone. Not expected in practice -- `daemon.subscribers`
            // outlives every subscriber thread for the life of the process
            // -- but handled rather than unwrapped so this loop still
            // terminates cleanly if that ever stops being true.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A best-effort check for "has the peer closed this connection." `peek`
/// would be the non-consuming way to do this, but `UnixStream::peek` is
/// still gated behind the unstable `unix_socket_peek` feature on this
/// toolchain (verified: `cargo build` rejects it with E0658, tracking issue
/// rust-lang/rust#76923) -- so this reads instead. That's harmless here: a
/// `Subscribe` connection is send-only from the daemon's side after the
/// initial request line, so the client is never expected to send anything
/// for this to consume. `Ok(0)` is an orderly shutdown (the overlay process
/// exited or closed the socket); `Ok(n>0)` would mean the client sent
/// something anyway, treated as "still there" rather than misread as dead; a
/// timed-out read (no data waiting within the 1 ms budget) means "no data,
/// but still connected."
fn socket_peer_gone(stream: &mut UnixStream) -> bool {
    let mut buf = [0u8; 1];
    // `set_read_timeout` panics on `Duration::ZERO` ("must not be zero"), so
    // this uses the smallest practical non-zero timeout rather than 0 --
    // 1 ms is still effectively instantaneous for a local, already-
    // established Unix socket with nothing in its receive buffer.
    if stream.set_read_timeout(Some(Duration::from_millis(1))).is_err() {
        return false; // can't tell; assume still alive rather than false-evict.
    }
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => false,
        Err(_) => true, // any other I/O error: treat the connection as gone.
    }
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

/// Spec §2's toggle table, expressed as a pure state -> request mapping so
/// it is testable exhaustively over every state constant without ever
/// constructing a `Daemon` or touching audio hardware -- see the "Task 7"
/// test-module comment below for why that matters here specifically.
/// `--toggle` is resolved against this, the server's *current* state, never
/// a client-side memory of the last press: the client (`owf-ctl` run by the
/// Hyprland keybind) is a fresh process on every keypress and has no memory
/// to consult.
///
/// Only `RECORDING` maps to `PttStop`; every other state -- the busy
/// sub-states, `WARMING`, `FAILED`, and (Task 13) `PAUSED` included -- maps
/// to `PttStart`, whose own match arms already refuse all of those (see
/// `dispatch` below).
fn toggle_target(state: u8) -> Request {
    match state {
        RECORDING => Request::PttStop,
        _ => Request::PttStart,
    }
}

/// What `Status` and `GetConfig` hand to the settings window when startup could
/// not use the config on disk (spec §3.1).
fn quarantine_notice(daemon: &Daemon) -> Option<String> {
    lock_ignoring_poison(&daemon.config_notice).clone()
}

/// `pub` so `src-tauri/src/settings_cmds.rs` can call it directly: the
/// settings window's three commands run in-process now, not over the
/// socket, and this is the same request/response handling the socket path
/// (`handle`, above) already funnels every other request through.
pub fn dispatch(daemon: &Arc<Daemon>, req: Request) -> Response {
    let current = daemon.state.load(Ordering::SeqCst);
    match req {
        Request::Status => {
            let mut r = Response::ok(state_of(current));
            // "The models are resident right now", not "startup finished".
            // The atomic, never the `pipeline` lock -- see the field's own
            // doc comment for the hang that would otherwise be.
            r.warm = Some(daemon.models_loaded.load(Ordering::SeqCst));
            // Task 3: "status must stop lying" -- only meaningful (and only
            // reported) when normalization was ever turned on; see
            // `Daemon::normalize_enabled`'s doc comment.
            if daemon.normalize_enabled {
                r.normalize_available = Some(daemon.normalize_available.load(Ordering::SeqCst));
            }
            // Task 2: `last_ms` was declared and never populated. `Timings`
            // derives `Serialize`, so this is just handing its fields
            // through as the generic JSON bag `Response::last_ms` already
            // is; `to_value` on a plain struct of small `u128` millisecond
            // counts cannot fail in practice.
            if let Some(t) = *lock_ignoring_poison(&daemon.last_timings) {
                r.last_ms = serde_json::to_value(t).ok();
            }
            if current == FAILED {
                r.err = lock_ignoring_poison(&daemon.fatal_error).clone();
            }
            // Not gated on `FAILED`, unlike `fatal_error` above: a quarantined
            // config is a *running* daemon whose settings were lost, which is
            // exactly the state a user would otherwise never be told about.
            r.config_notice = quarantine_notice(daemon);
            r
        }
        Request::PttStart => {
            // Spec §8 step 1: stop accepting new utterances once Beenden has
            // been requested. See `Daemon::quitting`'s doc comment.
            if daemon.quitting.load(Ordering::SeqCst) {
                return Response::err("shutting down");
            }
            match current {
                WARMING => Response::err("warming"),
                // Task 13: the tray's "Diktat pausieren" is checked. The
                // shortcut stays bound (this refusal is reachable at all
                // only because it is), it just opens no microphone until
                // `Request::SetPaused { paused: false }` returns to `IDLE`.
                PAUSED => Response::err("paused"),
                // The real stored reason, not a generic pointer to logs: on a
                // fresh install with no models yet, this is
                // `SherpaTranscriber`/`SileroTrimmer`'s own "model paths"
                // error, which is what tells a `--toggle` user to open the
                // Setup pane rather than a log file. `Status` already does
                // the same thing (`r.err = daemon.fatal_error`); this just
                // stops `ptt-start`'s own refusal from being the one place
                // that still hides it.
                FAILED => {
                    let reason = lock_ignoring_poison(&daemon.fatal_error)
                        .clone()
                        .unwrap_or_else(|| "see logs".to_string());
                    Response::err(format!("daemon failed to start: {reason}"))
                }
                RECORDING => Response::ok(State::Recording), // idempotent
                s if is_busy(s) => {
                    // Spec 6.1: `ptt-start` while Transcribing/Normalizing/
                    // Injecting is rejected, and "the overlay flashes" -- this is
                    // that flash's trigger.
                    daemon.broadcast(OverlayEvent::BusyRejected);
                    Response::err("busy")
                }
                _ => start_recording(daemon),
            }
        }
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
        // Task 13: the tray's "Diktat pausieren". A compare-and-exchange,
        // never an unconditional store: a `SetPaused` arriving mid-utterance
        // -- RECORDING included, not just the TRANSCRIBING/NORMALIZING/
        // INJECTING sub-states `is_busy` names -- must defer to it rather
        // than seize it. Invariant 1 protects text already produced; nothing
        // else would protect it from a pause that stole a state that isn't
        // `IDLE`. A CAS that fails here is silently dropped, with no retry
        // and no queue: a pause requested mid-utterance simply does not take,
        // and the tray's checkbox -- driven by this broadcast, never an
        // optimistic guess -- correctly never shows checked for it. The user
        // presses "Diktat pausieren" again once the daemon is back to `IDLE`.
        //
        // Broadcast only on an actual transition (mirroring `Cancel` above),
        // not on a failed or no-op CAS: `TauriSink::emit` -> `refresh_tray_icon`
        // is the only thing that ever moves the tray's icon/checkbox (see
        // `tray.rs`'s "Icon and daemon state"), so a transition nothing
        // broadcasts is a transition the tray never learns about.
        Request::SetPaused { paused } => {
            let (from, to) = if paused { (IDLE, PAUSED) } else { (PAUSED, IDLE) };
            if daemon.state.compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                daemon.broadcast(if paused { OverlayEvent::Paused } else { OverlayEvent::Idle });
            }
            Response::ok(state_of(daemon.state.load(Ordering::SeqCst)))
        }
        Request::Reload => {
            // Task 13 review: widened from `!= IDLE` to also accept `PAUSED`.
            // `PAUSED` is reached only by a CAS from `IDLE` (`SetPaused`
            // above), so there is never a recorder or pipeline mid-utterance
            // to protect while paused -- exactly the same absence of risk
            // `IDLE` itself already has. Before this widening, a user who
            // paused dictation from the tray could not `owf-ctl reload` (or
            // save a settings change, see `SetConfig` below) without
            // unpausing first, for a state that is not busy in any sense
            // this guard actually cares about.
            if current != IDLE && current != PAUSED {
                return Response::err("reload requires idle or paused");
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
            // scope, exactly as before. R15: that restart requirement used to
            // be silently violated for exactly one field, `[normalize].enabled`
            // -- `update_reloadable` now refuses such a reload outright (see
            // its doc comment for why rebuilding the normalizer here instead
            // is the wrong fix) rather than reporting success while leaving
            // the old normalizer in place.
            // `daemon.config_path`, matching `GetConfig`/`SetConfig` below --
            // not `Config::load()`, which resolves `paths::config_file()`
            // unconditionally. A test must never be pointed at the real
            // config file: `Config::load_from`'s own doc comment warns that
            // it *creates* the file when absent, so the bare `Config::load()`
            // this used to call would have silently written into whoever ran
            // the suite's real `~/.config/yappr/config.toml`. In
            // production `daemon.config_path` *is* `paths::config_file()`
            // (set in `start`), so this changes nothing there.
            match Config::load_from(&daemon.config_path) {
                Ok(new_cfg) => {
                    // Mirrored for exactly the reason `SetConfig` mirrors it:
                    // the housekeeping thread reads the idle timeout from
                    // `daemon.models_cfg` on every tick, and nothing else
                    // ever refreshes it. Without this, hand-editing
                    // `[models] idle_unload_seconds` and running
                    // `yappr --reload` reported success and changed
                    // nothing, while the same edit made in the settings GUI
                    // applied live -- and README documents `--reload` as
                    // applying every reloadable section.
                    //
                    // Unconditional, and before the live-apply below rather
                    // than after it: the config parsed, which is all the
                    // validation `[models]` needs (nothing here talks to a
                    // model), so every way the rest of this arm can end --
                    // pipeline resident, pipeline unloaded, or
                    // `update_reloadable` refusing an `[asr]`/`[normalize]`
                    // change -- must still leave the daemon tracking the file
                    // on disk. `SetConfig` mirrors unconditionally after a
                    // declined live reload for the same reason. Taken and
                    // released before `pipeline` is locked, so this adds no
                    // lock nesting of any kind (invariant 12).
                    *lock_ignoring_poison(&daemon.models_cfg) = new_cfg.models.clone();

                    // Same argument as `[models]` above, one section over:
                    // `--reload` is documented as applying every reloadable
                    // section, and `[realtime]` is one. A hand-edited port
                    // that needed a restart while the same change made in
                    // the settings window did not would be the exact
                    // inconsistency this block already exists to prevent.
                    sync_realtime(daemon, &new_cfg.realtime);

                    // The other moment a user picks this backend, and the
                    // other moment there is someone at the keyboard to
                    // answer the portal's dialog. Idempotent and cheap.
                    crate::libei::prewarm_if_selected(&new_cfg.inject);
                    let injector = inject::build(&new_cfg.inject);
                    let mut guard = lock_ignoring_poison(&daemon.pipeline);
                    match guard.as_mut() {
                        Some(p) => match p.update_reloadable(new_cfg, injector) {
                            Ok(()) => Response::ok(State::Idle),
                            Err(msg) => Response::err(msg),
                        },
                        // No longer unreachable, and no longer an error: with
                        // `[models] preload_at_startup` off, an idle daemon
                        // between dictations routinely has no pipeline. The
                        // config parsed above is all the validation there is
                        // to do -- `ensure_models_loaded` reads the file
                        // again when the next press brings the models up.
                        None => Response::ok(State::Idle),
                    }
                }
                Err(e) => Response::err(format!("config error: {e}")),
            }
        }
        Request::GetConfig => match Config::load_from(&daemon.config_path) {
            Ok(cfg) => {
                let mut r = Response::ok(state_of(current));
                r.config = Some(serde_json::to_value(&cfg).expect("Config serializes"));
                r.config_path = Some(daemon.config_path.display().to_string());
                // What the settings GUI's per-row reset button restores to.
                // Sent from here rather than reconstructed in the GUI for the
                // same reason `config` is: `Config` is the only thing that
                // knows its own defaults, and a second copy in TypeScript
                // would be free to drift from it.
                r.defaults =
                    Some(serde_json::to_value(Config::default()).expect("Config serializes"));
                // The settings window reads this one: it is the only surface
                // that can tell the user their settings are gone and where the
                // old file went (spec §3.1).
                r.config_notice = quarantine_notice(daemon);
                r
            }
            Err(e) => Response::err(format!("config error: {e}")),
        },

        Request::ListInputDevices => {
            match crate::capture::list_input_devices(DEVICE_LIST_TIMEOUT) {
                Ok(devices) => {
                    let mut r = Response::ok(state_of(current));
                    r.devices = Some(devices);
                    r
                }
                Err(e) => Response::err(format!("listing input devices: {e}")),
            }
        }

        Request::SetConfig { config } => {
            // Same guard as `reload`, widened the same way (Task 13 review):
            // swapping the pipeline's config or dropping the recorder
            // mid-utterance is not something to do, but `PAUSED` is not
            // mid-utterance -- it is reached only by a CAS from `IDLE`, so
            // there is no in-flight recorder or pipeline state here to
            // protect. Without this, pausing dictation from the tray (one
            // menu row above Einstellungen) silently broke every settings
            // autosave until the user unpaused again.
            if current != IDLE && current != PAUSED {
                return Response::err("changing settings requires idle or paused");
            }
            let old = match Config::load_from(&daemon.config_path) {
                Ok(c) => c,
                Err(e) => return Response::err(format!("config error: {e}")),
            };
            // Writes only if the result validates; on rejection the file on
            // disk is untouched. See `yappr_core::config_write`.
            if let Err(e) = config_write::save_config(&daemon.config_path, &config) {
                return Response::err(format!("{e:#}"));
            }
            let new_cfg = match Config::load_from(&daemon.config_path) {
                Ok(c) => c,
                Err(e) => return Response::err(format!("config error after write: {e}")),
            };

            let reason = restart_reason(
                &old,
                &new_cfg,
                daemon.models_loaded.load(Ordering::SeqCst),
            );

            // Best-effort live apply. `update_reloadable` refuses a
            // `[normalize].enabled` flip, which is exactly one of the cases
            // `restart_reason` already reports, so its error is not an error
            // here -- the file is written either way, and the response tells
            // the truth about what took effect.
            crate::libei::prewarm_if_selected(&new_cfg.inject);
            let injector = inject::build(&new_cfg.inject);
            {
                let mut guard = lock_ignoring_poison(&daemon.pipeline);
                if let Some(p) = guard.as_mut() {
                    if let Err(e) = p.update_reloadable(new_cfg.clone(), injector) {
                        tracing::info!(error = %e, "set-config: live reload declined");
                    }
                }
            }

            // Mirrored for the same reason `audio_cfg` is: the housekeeping
            // thread reads the idle timeout on every tick, and a user who
            // changes it in Settings must not have to restart for it.
            *lock_ignoring_poison(&daemon.models_cfg) = new_cfg.models.clone();

            // Starts, stops or re-binds the realtime endpoint. A no-op
            // unless `[realtime]` actually changed -- see `sync_realtime`,
            // which compares against the config the running listener was
            // started from rather than trusting the caller to have noticed.
            sync_realtime(daemon, &new_cfg.realtime);

            if new_cfg.audio != old.audio {
                *lock_ignoring_poison(&daemon.audio_cfg) = new_cfg.audio.clone();
                // Dropped, not rebuilt: `ensure_recorder` constructs lazily on
                // the next `ptt-start`, so a device that is currently
                // unplugged costs a failed dictation rather than a failed save.
                *lock_ignoring_poison(&daemon.recorder) = None;
                tracing::info!(device = %new_cfg.audio.device, "set-config: recorder will be rebuilt");
            }

            let mut r = Response::ok(State::Idle);
            r.restart_required = Some(reason.is_some());
            r.restart_reason = reason;
            r
        }

        // Never actually reached: `handle` special-cases `Subscribe` before
        // it ever calls `dispatch`, since a subscriber gets a long-lived
        // event stream instead of one `Response` (see `handle`'s doc
        // comment). Kept only so this match stays exhaustive.
        Request::Subscribe => Response::err("subscribe must be negotiated by the connection handler"),

        // Spec §2: resolved against the current state, not a client-side
        // memory of the last press (see `toggle_target`'s doc comment).
        // Delegates to the two existing arms rather than reimplementing
        // them -- PttStart and PttStop already carry the busy-rejection
        // broadcast, the idempotent restart, and `claim_busy`'s CAS against
        // the safety valve, and Toggle inherits all of it by construction.
        // The same is true of `Daemon::quitting`'s guard (spec §8 step 1):
        // for every state but RECORDING, `toggle_target` resolves to
        // `PttStart`, whose own guard the recursive `dispatch` call below
        // re-enters and applies. Deliberately not also checked directly
        // here -- doing so would refuse the RECORDING -> `PttStop` case too,
        // which (like a direct `PttStop`) only finishes a recording already
        // accepted before Quit, not a new one, and must stay allowed.
        Request::Toggle => dispatch(daemon, toggle_target(current)),
        Request::Quit => {
            // Spec §8 step 1, first clause: stop accepting new utterances.
            // Set synchronously, here, before spawning the wait/shutdown
            // thread below -- not inside that thread -- so a `PttStart`/
            // `Toggle` landing on the very next `accept()` (the accept loop
            // is single-threaded; see `handle`'s doc comment) already sees
            // it. See `Daemon::quitting`'s doc comment for the race this
            // closes.
            daemon.quitting.store(true, Ordering::SeqCst);

            // Second clause: run the wait-then-shutdown off this thread so
            // `dispatch` (and thus the client's socket round trip) returns
            // immediately, then exit -- `exit` belongs here, at the one
            // production call site, and nowhere near
            // `wait_for_busy_to_clear_then_shutdown`/`wait_for_busy_to_clear`
            // themselves; see their doc comments for why a test must never
            // reach an `exit` call.
            //
            // No terminal `OverlayEvent::Idle` is broadcast here: every
            // other teardown path (a terminating signal, a future tray
            // Beenden) goes straight from `shutdown` to `exit` with no
            // final event either, and a frontend that is about to lose its
            // process has nothing to do with one more state it'll never
            // render.
            let d = Arc::clone(daemon);
            std::thread::spawn(move || {
                wait_for_busy_to_clear_then_shutdown(&d);
                std::process::exit(0);
            });
            Response::ok(state_of(current))
        }
        Request::Restart => {
            // Quit's arm, verbatim, plus one call -- and for all the same
            // reasons, so read that arm's comments as this one's too. In
            // particular `quitting` is set here synchronously as well: a
            // restart is a teardown like any other from a `PttStart`'s point
            // of view, and the utterance it would otherwise let through
            // would be destroyed by the `shutdown` below exactly as invariant
            // 1 forbids.
            daemon.quitting.store(true, Ordering::SeqCst);

            let d = Arc::clone(daemon);
            std::thread::spawn(move || {
                // The successor is spawned inside this call, after
                // `shutdown` -- not here, and emphatically not before the
                // wait. `EventSink::relaunch`'s doc comment has the full
                // argument; the short version is that `shutdown` is what
                // unlinks `yappr.lock`, and a successor that starts any
                // earlier loses the race for it and exits 1.
                wait_for_busy_to_clear_then_shutdown_and_relaunch(&d);
                std::process::exit(0);
            });
            Response::ok(state_of(current))
        }
        Request::ShowSettings => {
            daemon.sink.show_settings();
            Response::ok(state_of(current))
        }
        Request::ShowWizard => {
            daemon.sink.show_wizard();
            Response::ok(state_of(current))
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

/// Spec 15 row 1: `start_recording`'s two failure sites (no microphone /
/// cannot start capture) used to return `Response::err` with no
/// `OverlayEvent::Error` broadcast at all. The error reached only
/// `owf-ctl`'s stderr -- which the Hyprland keybind that invokes it discards
/// -- so a user who pressed SUPER+D with, say, a Bluetooth headset's A2DP
/// profile routed to `default` (no microphone at all) got nothing: no
/// recording, no overlay, no clue why. Broadcasting `reason` here before
/// returning the same `Response::err` makes every way `start_recording` can
/// fail visible to a subscribed overlay, not just to a discarded CLI stderr.
fn recording_start_failed(daemon: &Daemon, reason: String) -> Response {
    daemon.broadcast(OverlayEvent::Error { reason: reason.clone() });
    Response::err(reason)
}

/// Refreshes the idle-unload deadline. Called at the start of every
/// recording and at the end of every utterance, which between them cover
/// every way the daemon does dictation-shaped work.
fn touch_activity(daemon: &Daemon) {
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now();
}

fn start_recording(daemon: &Arc<Daemon>) -> Response {
    // Before anything else, and specifically before the `RECORDING` store
    // below: `unload_models` re-checks this deadline while holding
    // `load_lock`, so a press that refreshes it first can never have its
    // models pulled out from under it by an unload that was already in
    // flight. Same discipline as `recording_epoch` and the safety valve.
    touch_activity(daemon);

    // Tell the overlay "wait" *before* anything that can take real time.
    // The microphone is not live yet: `ensure_recorder` below may still have
    // to build the recorder, and even once `start()` has returned,
    // ALSA/PipeWire delivers the first buffer tens of milliseconds later
    // (measured ~55 ms through PipeWire on this hardware). Speech in that
    // window is genuinely lost, so the user must be told to hold off rather
    // than shown live bars over a mic that is not capturing.
    //
    // This has to be broadcast here, not further down next to the state
    // store, because `start()`'s own audio callback can fire before this
    // function reaches that point -- emitting `Recording` first and leaving
    // a "wait" pill to land *after* capture had already gone live.
    daemon.broadcast(OverlayEvent::Opening);

    // I7: construct the recorder (or retry a previous failure) before doing
    // anything else -- and I4: query the window class *after* capture has
    // actually started, not before. `hyprctl` is a subprocess spawn (bounded
    // by a timeout, see I3 in `hypr.rs`), and running it first clipped the
    // beginning of every utterance behind that spawn, which also contradicts
    // `hypr.rs`'s own claim that this call is off the critical path.
    if let Err(e) = ensure_recorder(&daemon.recorder, || {
        Recorder::new(&lock_ignoring_poison(&daemon.audio_cfg).clone()).map_err(|e| e.to_string())
    }) {
        return recording_start_failed(daemon, format!("no microphone: {e}"));
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
        return recording_start_failed(daemon, format!("cannot start capture: {e}"));
    }

    // Capturing the window class here, now that the mic is already open,
    // is still deliberate (spec 11): it is the window that was focused when
    // the user *started* talking, not whatever has focus once they finish.
    // On GNOME that is a claim this call can only honour because
    // `winclass`'s accessibility provider has been *tracking* focus since
    // startup -- by now the overlay itself holds it (invariant 2).
    *lock_ignoring_poison(&daemon.window_class) = crate::winclass::active_window_class();

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
    // Deliberately does NOT broadcast a synthetic `Recording { level: 0.0 }`
    // here any more. That placeholder used to exist so the bars appeared
    // without waiting for the first throttled callback -- but it announced
    // "recording" while the microphone had not yet produced a single sample,
    // which is exactly the lie the `Opening` event above replaces. The first
    // real audio callback emits `Recording` itself with no throttle delay
    // (`should_emit_level`'s `last_emitted` starts `None`), so the bars still
    // appear at the earliest honest moment: when capture is actually live.

    // Safety valve: under hold-to-talk this only ever caught a stuck key
    // (spec 6.1). Under press/press toggle it is the *sole* terminator of a
    // recording whose second press never comes -- nothing else will ever
    // close that microphone -- so it must transcribe what it captured
    // (invariant 1), not discard it. See `spawn_safety_valve`'s own doc.
    spawn_safety_valve(daemon, epoch);

    // The models come up on their own thread, deliberately *after* capture is
    // already live. Nothing in the capture path needs a model, so the
    // microphone opens on exactly the timeline it always did and the load
    // overlaps with the user speaking -- for any utterance longer than the
    // load, it costs nothing at all. `run_utterance` calls the same function
    // and blocks there if this has not finished by the second press.
    //
    // A failure here is logged and dropped: `run_utterance` retries and is
    // the one that reports it to the user, so a failed load never produces
    // two error broadcasts for one press.
    {
        let d = Arc::clone(daemon);
        std::thread::spawn(move || {
            if let Err(e) = ensure_models_loaded(&d) {
                tracing::warn!(error = %e, "background model load failed; ptt-stop will retry");
            }
        });
    }

    Response::ok(State::Recording)
}

/// The body of `start_recording`'s safety-valve timer, extracted to a named
/// function so it can be driven directly in tests -- see the "safety valve"
/// tests below -- without going through `start_recording`, which would
/// require a real `Recorder` (`ensure_recorder` calls `Recorder::new`, which
/// needs live audio hardware). Moved here unchanged: same sleep, same epoch
/// check, same `claim_busy`/`run_utterance` call: no behaviour change.
///
/// `run_utterance` with `daemon.recorder` still `None` -- exactly the case
/// for every fake daemon in this module's tests -- takes its early `None`
/// arm and returns without calling any `Recorder` method, the same property
/// `toggle_stops_and_begins_transcribing_when_recording` already relies on
/// for `PttStop`'s identical `std::thread::spawn(move || run_utterance(d))`.
fn spawn_safety_valve(daemon: &Arc<Daemon>, epoch: u64) {
    let d = Arc::clone(daemon);
    let limit = Duration::from_secs(lock_ignoring_poison(&d.audio_cfg).max_seconds as u64);
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
/// Also broadcasts `OverlayEvent::Idle` -- but only when nothing already told
/// every subscriber the utterance was over. `Done`/`Error` (spec 12's two
/// terminal outcomes) already *mean* "returning to idle"; before this fix,
/// `Drop` broadcast `Idle` unconditionally right behind whichever of those
/// `process_utterance` had just sent, and the overlay's own
/// `clearHideTimer()` (see `src/Overlay.tsx`) ran on every event including
/// that redundant `Idle` -- cancelling the 800 ms `Done` flash or the 2 s
/// `Error` pill before a single frame painted (fix 1). `terminal_sent` is how
/// `run_utterance` tells this guard "a terminal event already went out,
/// don't send another" -- it does not change whether `state` itself returns
/// to `IDLE`, which stays unconditional on every exit path (including an
/// unwinding panic, when `terminal_sent` is still `false` and `Idle` is
/// exactly what a subscriber must see).
///
/// Constructed as the very first thing in `run_utterance`, before anything
/// fallible, so its `Drop` covers every line after it. The `state` store is
/// unconditional, not a CAS: at most one `run_utterance` (or safety-valve)
/// call is ever in flight at a time -- the RECORDING -> TRANSCRIBING
/// transition it exits is claimed exactly once per recording, by whichever
/// of `ptt-stop` or the safety valve won `claim_busy` -- so there is no
/// other legitimate writer for this guard to race or clobber.
struct IdleOnExit<'a> {
    state: &'a AtomicU8,
    broadcaster: Broadcaster<'a>,
    /// Refreshed on every exit path, unwinding included: a panicking
    /// utterance that left a stale deadline behind would have its models
    /// unloaded early, on a clock that started before the utterance did.
    last_activity: &'a Mutex<Instant>,
    terminal_sent: Cell<bool>,
}

impl<'a> IdleOnExit<'a> {
    fn new(
        state: &'a AtomicU8,
        broadcaster: Broadcaster<'a>,
        last_activity: &'a Mutex<Instant>,
    ) -> Self {
        Self { state, broadcaster, last_activity, terminal_sent: Cell::new(false) }
    }
}

impl Drop for IdleOnExit<'_> {
    fn drop(&mut self) {
        *lock_ignoring_poison(self.last_activity) = Instant::now();
        self.state.store(IDLE, Ordering::SeqCst);
        if !self.terminal_sent.get() {
            self.broadcaster.broadcast(OverlayEvent::Idle);
        }
    }
}

fn run_utterance(daemon: Arc<Daemon>) {
    let idle_on_exit = IdleOnExit::new(&daemon.state, daemon.broadcaster(), &daemon.last_activity);

    let stop_result = match lock_ignoring_poison(&daemon.recorder).as_ref() {
        Some(r) => r.stop(),
        // Unreachable in normal operation: reaching RECORDING requires
        // `start_recording` to have already ensured a recorder exists.
        None => {
            tracing::warn!("utterance finished but no recorder was ever constructed");
            daemon.broadcast(OverlayEvent::Error { reason: "no recorder available".to_string() });
            idle_on_exit.terminal_sent.set(true);
            return;
        }
    };
    let stop = match stop_result {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "capture stop failed");
            daemon.broadcast(OverlayEvent::Error { reason: format!("capture stop failed: {e}") });
            idle_on_exit.terminal_sent.set(true);
            return;
        }
    };
    let class = lock_ignoring_poison(&daemon.window_class).clone();
    // Blocks only if the load started by `start_recording` has not finished.
    // No new overlay state: the daemon really is `TRANSCRIBING` (`claim_busy`
    // ran before this thread was spawned), so the overlay shows its existing
    // Transcribing view for however long this takes.
    if let Err(reason) = ensure_models_loaded(&daemon) {
        // Spec 2026-08-29 §4: retryable, not fatal. `FAILED` and
        // `fatal_error` are left alone, so the next press tries again -- on a
        // fresh install that means downloading the models in the Setup pane
        // and pressing again, with no restart. `reason` carries
        // `SherpaTranscriber`/`SileroTrimmer`'s own "model paths" error,
        // which is the string that tells the user which pane to open.
        tracing::error!(error = %reason, "models could not be loaded for this utterance");
        daemon.broadcast(OverlayEvent::Error { reason });
        idle_on_exit.terminal_sent.set(true);
        return;
    }

    process_utterance(
        &daemon.pipeline,
        daemon.broadcaster(),
        &daemon.last_timings,
        &stop.samples,
        class.as_deref(),
        Some(stop.capture),
    );
    // `process_utterance` always broadcasts exactly one terminal event
    // (`Done` or `Error`) on every path it returns from normally -- the only
    // way to reach this line without having sent one is for it to have
    // panicked instead, in which case unwinding skips this store and
    // `IdleOnExit::drop` correctly still sends `Idle`.
    idle_on_exit.terminal_sent.set(true);
}

/// The first ~60 characters of `text` -- spec 12's `Done` preview flash.
/// Truncates on a `char` boundary so multi-byte UTF-8 is never split
/// mid-codepoint.
fn preview_of(text: &str) -> String {
    const MAX_CHARS: usize = 60;
    text.chars().take(MAX_CHARS).collect()
}

/// Runs one utterance's already-captured samples through the pipeline, and
/// broadcasts the outcome (`Done`/`Error`) to every subscriber and the
/// in-process sink alike, via `broadcaster`.
///
/// Split out from `run_utterance` so it is testable without a real
/// `Recorder` (which needs live audio hardware to construct -- see
/// `capture::Recorder::new`): a test can drive this directly with a scratch
/// `Mutex<Option<Pipeline>>` and a panicking fake stage to prove
/// `IdleOnExit` recovers `state` even when this function unwinds.
/// `broadcaster` is likewise built from bare parts rather than a whole
/// `&Daemon`, for the same reason `IdleOnExit` takes one directly.
///
/// `last_timings` is likewise a bare `&Mutex<Option<Timings>>` -- it is
/// updated here, from the produced `Outcome`, whenever there is one to take
/// timings from (see `Daemon::last_timings`'s doc comment for why a
/// no-speech/error utterance leaves it untouched instead of clearing it).
fn process_utterance(
    pipeline: &Mutex<Option<Pipeline>>,
    broadcaster: Broadcaster<'_>,
    last_timings: &Mutex<Option<Timings>>,
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
                *lock_ignoring_poison(last_timings) = Some(out.timings);
                broadcaster.broadcast(OverlayEvent::Done { preview: preview_of(&out.text) });
            }
            Ok(None) => {
                tracing::info!("nothing to inject");
                broadcaster.broadcast(OverlayEvent::Error { reason: "no speech detected".to_string() });
            }
            Err(e) => {
                tracing::error!(error = ?e, "pipeline failed");
                broadcaster.broadcast(OverlayEvent::Error { reason: e.to_string() });
            }
        }
    } else {
        // Unreachable in normal operation, but for a different reason than
        // before lazy loading: `IDLE` is no longer proof that `pipeline` is
        // populated (a lazy daemon reaches it with no models loaded at all).
        // What makes this arm unreachable now is `run_utterance`, which calls
        // `ensure_models_loaded` and returns before ever reaching this
        // function if that call failed -- so by the time `process_utterance`
        // runs, the load has already succeeded. Logged rather than unwrapped
        // so a future change to that guarantee fails loudly instead of
        // panicking.
        tracing::warn!("utterance finished but the pipeline was not ready");
        broadcaster.broadcast(OverlayEvent::Error { reason: "pipeline not ready".to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::Transcriber;
    use crate::config::Config;
    use crate::inject::MockInjector;
    use crate::lang::{Lang, LanguageDetector};
    use crate::normalize::Normalizer;
    use crate::vad::Trimmer;
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
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let (tx, rx) = mpsc::channel();
        subscribers.lock().unwrap().push(Subscriber { tx, alive: Arc::new(AtomicBool::new(true)) });
        let samples = vec![0.1f32; 16_000];
        let last_timings: Mutex<Option<Timings>> = Mutex::new(None);
        let last_activity: Mutex<Instant> = Mutex::new(Instant::now());

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let broadcaster = dropping_broadcaster(&subscribers);
            let _idle_on_exit = IdleOnExit::new(&state, broadcaster.clone(), &last_activity);
            process_utterance(&pipeline, broadcaster, &last_timings, &samples, None, None);
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

    /// Fix 1 (F1): spec 12's `Done` and `Error` rows never rendered, because
    /// `IdleOnExit::drop` broadcast a redundant `Idle` immediately behind
    /// whichever one `process_utterance` had just sent, and the overlay's
    /// `clearHideTimer()` ran on every event -- including that `Idle` --
    /// cancelling the flash before a frame painted. No test anywhere asserted
    /// the *sequence* a subscriber receives (every other assertion in this
    /// file is a single `try_recv()`); this drives one full successful
    /// utterance through the real `IdleOnExit`/`process_utterance` pair, the
    /// same way `run_utterance` wires them together (guard constructed
    /// first, `terminal_sent` marked after `process_utterance` returns, the
    /// same as the real function), and asserts the whole ordered sequence a
    /// subscriber actually receives -- including that nothing at all follows
    /// `Done`.
    #[test]
    fn a_successful_utterance_broadcasts_done_with_no_redundant_idle_after_it() {
        let pipeline = Mutex::new(Some(Pipeline::new(
            Config::from_str("[normalize]\nenabled = false\n").unwrap(),
            Box::new(FixedAsr("hello there")),
            Box::new(WholeBuffer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )));
        let state = AtomicU8::new(TRANSCRIBING);
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let (tx, rx) = mpsc::channel();
        subscribers.lock().unwrap().push(Subscriber { tx, alive: Arc::new(AtomicBool::new(true)) });
        let samples = vec![0.1f32; 16_000];
        let last_timings: Mutex<Option<Timings>> = Mutex::new(None);
        let last_activity: Mutex<Instant> = Mutex::new(Instant::now());

        {
            let broadcaster = dropping_broadcaster(&subscribers);
            let idle_on_exit = IdleOnExit::new(&state, broadcaster.clone(), &last_activity);
            process_utterance(&pipeline, broadcaster, &last_timings, &samples, None, None);
            idle_on_exit.terminal_sent.set(true);
        }

        assert_eq!(state.load(Ordering::SeqCst), IDLE, "the daemon must still return to idle");
        // Guardrail capitalization/terminal punctuation plus the default
        // `inject.trailing_space` (see `pipeline.rs`) turn "hello there"
        // into "Hello there. " -- this test cares about the *sequence* of
        // broadcasts, not the exact text transform, so it just matches
        // whatever `process_utterance` actually produced.
        let done = rx.try_recv().expect("a subscriber must see Done");
        assert!(
            matches!(&done, OverlayEvent::Done { preview } if preview.starts_with("Hello there")),
            "unexpected Done payload: {done:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "no redundant Idle (or anything else) may follow Done -- Done already means \
             'returning to idle', and a lingering broadcast right behind it is exactly what \
             cancelled the overlay's 800ms Done flash before a frame painted"
        );
    }

    /// The `Error` half of the same fix: `process_utterance`'s "no speech
    /// detected" path broadcasts `Error`, and that must be the only thing a
    /// subscriber sees -- no trailing `Idle`.
    #[test]
    fn a_no_speech_utterance_broadcasts_error_with_no_redundant_idle_after_it() {
        struct SilentTrimmer;
        impl Trimmer for SilentTrimmer {
            fn trim(&self, _: &[f32], _: u32) -> Option<(usize, usize)> {
                None
            }
        }

        let pipeline = Mutex::new(Some(Pipeline::new(
            Config::from_str("[normalize]\nenabled = false\n").unwrap(),
            Box::new(FixedAsr("unused")),
            Box::new(SilentTrimmer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )));
        let state = AtomicU8::new(TRANSCRIBING);
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let (tx, rx) = mpsc::channel();
        subscribers.lock().unwrap().push(Subscriber { tx, alive: Arc::new(AtomicBool::new(true)) });
        let samples = vec![0.1f32; 16_000];
        let last_timings: Mutex<Option<Timings>> = Mutex::new(None);
        let last_activity: Mutex<Instant> = Mutex::new(Instant::now());

        {
            let broadcaster = dropping_broadcaster(&subscribers);
            let idle_on_exit = IdleOnExit::new(&state, broadcaster.clone(), &last_activity);
            process_utterance(&pipeline, broadcaster, &last_timings, &samples, None, None);
            idle_on_exit.terminal_sent.set(true);
        }

        assert_eq!(state.load(Ordering::SeqCst), IDLE);
        assert_eq!(
            rx.try_recv(),
            Ok(OverlayEvent::Error { reason: "no speech detected".to_string() }),
        );
        assert!(rx.try_recv().is_err(), "no redundant Idle may follow Error either");
    }

    /// A transcriber that always succeeds with fixed text -- drives
    /// `process_utterance` down its happy path, unlike `PanickingTranscriber`
    /// above.
    struct FixedAsr(&'static str);
    impl Transcriber for FixedAsr {
        fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// Task 2: `Response.last_ms` was declared and never populated, even
    /// though `Outcome.timings` was computed on every successful utterance
    /// and simply never read by anything in production. Proves
    /// `process_utterance` actually stores those timings where `dispatch`'s
    /// `Request::Status` arm can find them -- see
    /// `status_reports_last_ms_after_a_successful_utterance` for the other
    /// half.
    #[test]
    fn a_successful_utterance_populates_last_timings() {
        let pipeline = Mutex::new(Some(Pipeline::new(
            // Normalization off so `NeverNormalizer` is safe to reuse here
            // too: nothing about this test cares whether normalization ran.
            Config::from_str("[normalize]\nenabled = false\n").unwrap(),
            Box::new(FixedAsr("hello there")),
            Box::new(WholeBuffer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )));
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let last_timings: Mutex<Option<Timings>> = Mutex::new(None);
        let samples = vec![0.1f32; 16_000];

        process_utterance(&pipeline, dropping_broadcaster(&subscribers), &last_timings, &samples, None, None);

        assert!(
            last_timings.lock().unwrap().is_some(),
            "a successful utterance must record its per-stage timings"
        );
    }

    /// The regression this whole task exists to prevent: before `Broadcaster`
    /// existed, `process_utterance` called `broadcast_to` directly, so its
    /// `Done`/`Error` terminal events reached socket subscribers but never
    /// the app's in-process sink -- every existing test here asserted only
    /// on a socket subscriber (an `mpsc::Receiver`), so the gap was invisible
    /// to the whole suite even after `Daemon::broadcast` itself was fixed to
    /// reach both. This drives `process_utterance` with *no* socket
    /// subscriber registered at all, so the only way this can pass is if the
    /// `Done` it produces reaches the sink through `Broadcaster`.
    #[test]
    fn a_successful_utterance_reaches_the_in_process_sink_via_process_utterance() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<OverlayEvent>>);
        impl EventSink for Recorder {
            fn emit(&self, e: &OverlayEvent) {
                self.0.lock().unwrap().push(e.clone());
            }
        }

        let pipeline = Mutex::new(Some(Pipeline::new(
            Config::from_str("[normalize]\nenabled = false\n").unwrap(),
            Box::new(FixedAsr("hello there")),
            Box::new(WholeBuffer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )));
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let sink = Arc::new(Recorder::default());
        let broadcaster = broadcaster_with(&subscribers, sink.clone());
        let last_timings: Mutex<Option<Timings>> = Mutex::new(None);
        let samples = vec![0.1f32; 16_000];

        process_utterance(&pipeline, broadcaster, &last_timings, &samples, None, None);

        let events = sink.0.lock().unwrap();
        assert!(
            matches!(events.as_slice(), [OverlayEvent::Done { .. }]),
            "expected exactly one Done event in the in-process sink, got {events:?}"
        );
    }

    /// `status`'s own end of the same fix: once an utterance has recorded
    /// timings, `Request::Status` must surface them as `last_ms`; before any
    /// utterance has ever completed it must report nothing at all, not even
    /// the key (`Response::last_ms`'s `skip_serializing_if` promise -- an old
    /// client that never looks for the field must be unaffected).
    #[test]
    fn status_reports_last_ms_after_a_successful_utterance() {
        let daemon = fake_daemon(IDLE);
        let before = dispatch(&daemon, Request::Status);
        assert!(before.last_ms.is_none(), "no utterance has completed yet");

        *daemon.last_timings.lock().unwrap() =
            Some(Timings { vad_ms: 12, asr_ms: 340, normalize_ms: 0, inject_ms: 5 });

        let after = dispatch(&daemon, Request::Status);
        let last_ms =
            after.last_ms.expect("last_ms must be populated once an utterance has completed");
        assert_eq!(last_ms["vad_ms"], serde_json::json!(12));
        assert_eq!(last_ms["asr_ms"], serde_json::json!(340));
        assert_eq!(last_ms["normalize_ms"], serde_json::json!(0));
        assert_eq!(last_ms["inject_ms"], serde_json::json!(5));
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

    /// The `Opening` ("wait") pill ends when the first audio callback emits
    /// `Recording`. That handoff is only immediate because the very first
    /// callback is never throttled -- if `should_emit_level` ever started
    /// suppressing it, the user would sit on a "wait" pill for up to
    /// `LEVEL_EMIT_INTERVAL` after the microphone had genuinely gone live.
    #[test]
    fn the_first_level_callback_is_never_throttled_so_wait_ends_as_soon_as_audio_arrives() {
        let now = Instant::now();
        assert!(
            should_emit_level(None, now, LEVEL_EMIT_INTERVAL),
            "the first callback after ptt-start must emit immediately"
        );
        // ... and the throttle still applies from the second one onward.
        assert!(!should_emit_level(Some(now), now, LEVEL_EMIT_INTERVAL));
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

    /// Fix 3 (F3) / spec 15 row 1: `start_recording`'s two failure sites
    /// (no microphone, cannot start capture) used to return `Response::err`
    /// with no `OverlayEvent::Error` broadcast -- invisible to anything but
    /// `owf-ctl`'s discarded stderr. This is exactly the failure the
    /// machine's owner hit: a Bluetooth headset in A2DP has no microphone,
    /// ALSA's `default` routed to it anyway, and SUPER+D did nothing
    /// visible. Exercises `recording_start_failed` directly (the helper both
    /// call sites in `start_recording` now go through) rather than calling
    /// `start_recording` itself, which would need a real `Recorder` --
    /// opening live audio hardware is off-limits here.
    #[test]
    fn recording_start_failed_broadcasts_error_and_returns_the_same_reason() {
        let daemon = fake_daemon(IDLE);
        let (tx, rx) = mpsc::channel();
        daemon.subscribers.lock().unwrap().push(Subscriber { tx, alive: Arc::new(AtomicBool::new(true)) });

        let resp =
            recording_start_failed(&daemon, "no microphone: no default input device".to_string());

        assert!(!resp.ok);
        assert_eq!(resp.err.as_deref(), Some("no microphone: no default input device"));
        assert_eq!(
            rx.try_recv(),
            Ok(OverlayEvent::Error { reason: "no microphone: no default input device".to_string() }),
            "a subscriber must see why nothing happened, not just owf-ctl's discarded stderr"
        );
    }

    /// I6: proves the socket this daemon binds ends up owner-only, not at
    /// whatever the ambient umask would otherwise leave it -- exercised
    /// against a scratch socket path rather than the real
    /// `$XDG_RUNTIME_DIR` one, since `main` itself isn't unit-testable.
    #[test]
    fn secure_socket_locks_the_socket_down_to_owner_only() {
        let dir =
            std::env::temp_dir().join(format!("yappr-test-socket-perms-{}", std::process::id()));
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

    /// The classification table in the settings-GUI spec, made executable.
    /// Each of these is a claim the GUI shows the user as a banner (or does
    /// not), so getting it wrong means telling them a change took effect when
    /// it did not.
    #[test]
    fn live_reloadable_sections_do_not_ask_for_a_restart() {
        let base = Config::from_str("").unwrap();
        for toml in [
            "[guardrail]\nngram_size = 8\n",
            "[inject]\ntrailing_space = false\n",
            // The row that actually swaps the injector object, not just a
            // field `process_with_capture` reads out of `cfg`.
            "[inject]\nbackend = \"ydotool\"\n",
            "[style_default]\nstyling = \"formal\"\n",
            "[vocabulary]\nterms = [\"Hyprland\"]\n",
            "[debug]\nenabled = true\n",
            // Live via the rebuilt recorder -- see `set-config`'s handler.
            "[audio]\ndevice = \"hw:1,0\"\n",
            "[audio]\nmax_seconds = 60\n",
        ] {
            let changed = Config::from_str(toml).unwrap();
            assert_eq!(
                restart_reason(&base, &changed, true),
                None,
                "should be live-reloadable: {toml:?}"
            );
        }
    }

    #[test]
    fn changing_the_asr_or_normalizer_asks_for_a_restart() {
        let base = Config::from_str("").unwrap();

        let asr = Config::from_str("[asr]\nnum_threads = 8\n").unwrap();
        let reason = restart_reason(&base, &asr, true).expect("asr needs a restart");
        assert!(reason.contains("[asr]"), "got: {reason}");

        let norm = Config::from_str("[normalize]\ntimeout_ms = 9000\n").unwrap();
        let reason = restart_reason(&base, &norm, true).expect("normalize needs a restart");
        assert!(reason.contains("[normalize]"), "got: {reason}");
    }

    /// The reason string is shown to the user, and until 2026-09-09 its
    /// `[normalize]` half said "llama-server is spawned once, at startup" --
    /// a sentence describing a supervised child process that the in-process
    /// llama.cpp rewrite deleted. It went unnoticed because no test read the
    /// text, only `is_some()`. This one reads it, so the next such rewrite
    /// has to update the copy or go red.
    #[test]
    fn the_restart_reason_does_not_name_anything_this_app_no_longer_runs() {
        let base = Config::from_str("").unwrap();
        let norm = Config::from_str("[normalize]\ntimeout_ms = 9000\n").unwrap();
        let reason = restart_reason(&base, &norm, true).expect("normalize needs a restart");
        assert!(
            !reason.contains("llama-server"),
            "there is no llama-server any more; got: {reason}"
        );
    }

    /// The gate that keeps the restart *dialog* honest. With no models
    /// resident, `ensure_models_loaded` re-reads `config.toml` from disk on
    /// the next press, so a changed `[asr]`/`[normalize]` takes effect
    /// without a restart and asking for one buys the user nothing. That is
    /// the ordinary state of an idle daemon, not an edge case:
    /// `preload_at_startup` defaults to `false` and `idle_unload_seconds` to
    /// 60. Before the dialog existed this was merely pessimistic (the
    /// lazy-lifecycle doc says as much); a modal makes it a nag.
    #[test]
    fn nothing_needs_a_restart_while_no_models_are_resident() {
        let base = Config::from_str("").unwrap();
        for toml in ["[asr]\nnum_threads = 8\n", "[normalize]\ntimeout_ms = 9000\n"] {
            let changed = Config::from_str(toml).unwrap();
            assert_eq!(
                restart_reason(&base, &changed, false),
                None,
                "unloaded models take the new config on the next press: {toml:?}"
            );
            // The same diff with the models up is the case that *does* need
            // one -- so this test cannot pass by the diff being empty.
            assert!(
                restart_reason(&base, &changed, true).is_some(),
                "and must still ask once they are resident: {toml:?}"
            );
        }
    }

    #[test]
    fn an_unchanged_config_asks_for_nothing() {
        let base = Config::from_str("").unwrap();
        assert_eq!(restart_reason(&base, &base.clone(), true), None);
    }

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
        assert_eq!(state_of(PAUSED), State::Paused);
    }

    #[test]
    fn is_busy_covers_exactly_the_three_processing_substates() {
        assert!(is_busy(TRANSCRIBING));
        assert!(is_busy(NORMALIZING));
        assert!(is_busy(INJECTING));
        assert!(!is_busy(WARMING));
        assert!(!is_busy(IDLE));
        assert!(!is_busy(RECORDING));
        assert!(!is_busy(PAUSED));
    }

    #[test]
    fn snapshot_event_mirrors_state_of_for_the_steady_states() {
        assert_eq!(snapshot_event(WARMING), OverlayEvent::Warming);
        assert_eq!(snapshot_event(IDLE), OverlayEvent::Idle);
        assert_eq!(snapshot_event(TRANSCRIBING), OverlayEvent::Transcribing);
        assert_eq!(snapshot_event(NORMALIZING), OverlayEvent::Normalizing);
        assert_eq!(snapshot_event(INJECTING), OverlayEvent::Injecting);
        assert_eq!(snapshot_event(PAUSED), OverlayEvent::Paused);
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
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());

        let (dead_tx, dead_rx) = mpsc::channel();
        subscribers.lock().unwrap().push(Subscriber { tx: dead_tx, alive: Arc::new(AtomicBool::new(true)) });
        drop(dead_rx); // simulates the subscriber's thread having exited

        let (live_tx, live_rx) = mpsc::channel();
        subscribers.lock().unwrap().push(Subscriber { tx: live_tx, alive: Arc::new(AtomicBool::new(true)) });

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
        let subscribers: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        broadcast_to(&subscribers, OverlayEvent::BusyRejected); // must not panic
        assert!(subscribers.lock().unwrap().is_empty());
    }

    /// A minimal `Daemon` for socket-level tests: no real `Recorder` or
    /// `llama-server` (both need resources this sandbox doesn't have), just
    /// enough to drive `handle`/`dispatch` for `Status`/`Subscribe`.
    fn fake_daemon(initial_state: u8) -> Arc<Daemon> {
        fake_daemon_with_normalize(initial_state, false)
    }

    /// The general form of `fake_daemon`: every other test wants
    /// `normalize_enabled: false` (the plain `fake_daemon` above), which
    /// used to be the *only* value any test could get, hardcoded -- making
    /// `serve_subscriber`'s `NormalizeDegraded`-replay-on-connect path
    /// (`degraded` in its doc comment) unreachable by any test in this file.
    /// See `a_late_subscriber_sees_the_degraded_badge_replayed_on_connect`,
    /// the test that needs `true`.
    fn fake_daemon_with_normalize(initial_state: u8, normalize_enabled: bool) -> Arc<Daemon> {
        fake_daemon_at(initial_state, normalize_enabled, PathBuf::from("/nonexistent/yappr-test/config.toml"))
    }

    /// The seam the config handlers need: `set-config` writes, so a test must
    /// never be pointed at the real `paths::config_file()`.
    fn fake_daemon_at(initial_state: u8, normalize_enabled: bool, config_path: PathBuf) -> Arc<Daemon> {
        fake_daemon_at_with_sink(initial_state, normalize_enabled, config_path, Arc::new(DropSink))
    }

    /// The general form of `fake_daemon_at`, for the one test that has to
    /// observe what the daemon does *to its sink* rather than to its state:
    /// `Request::Restart` calls `EventSink::relaunch`, and the only thing
    /// worth asserting about that call is when it happens relative to
    /// `shutdown`.
    fn fake_daemon_at_with_sink(
        initial_state: u8,
        normalize_enabled: bool,
        config_path: PathBuf,
        sink: Arc<dyn EventSink>,
    ) -> Arc<Daemon> {
        let (runtime_socket_path, runtime_lock_path) = fake_runtime_files();
        Arc::new(Daemon {
            state: AtomicU8::new(initial_state),
            quitting: AtomicBool::new(false),
            recorder: Mutex::new(None),
            audio_cfg: Mutex::new(AudioConfig::default()),
            pipeline: Mutex::new(None),
            load_lock: Mutex::new(()),
            models_loaded: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
            models_cfg: Mutex::new(ModelsConfig::default()),
            realtime_cfg: Mutex::new(RealtimeConfig::default()),
            window_class: Mutex::new(None),
            config_path: config_path.clone(),
            recording_epoch: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
            normalize_enabled,
            normalize_available: AtomicBool::new(false),
            fatal_error: Mutex::new(None),
            config_notice: Mutex::new(None),
            realtime: Mutex::new(None),
            realtime_error: Mutex::new(None),
            housekeeping: Mutex::new(None),
            last_timings: Mutex::new(None),
            sink,
            runtime_socket_path,
            runtime_lock_path,
            _runtime_lock: fake_runtime_lock(),
        })
    }

    /// What `fake_daemon_at`/`fake_daemon_with_normalize` pass for `sink`:
    /// broadcast now has an in-process consumer as well as socket
    /// subscribers, and the vast majority of tests in this module care about
    /// neither, so this keeps their construction unchanged.
    struct DropSink;
    impl EventSink for DropSink {
        fn emit(&self, _: &OverlayEvent) {}
    }

    /// Builds a `Broadcaster` over a bare `subscribers` list for the tests
    /// that drive `IdleOnExit`/`process_utterance`/`supervise_llama_once`
    /// directly, without a full `Daemon`, the same way those functions
    /// already took a bare `&Mutex<Vec<Subscriber>>` before `Broadcaster`
    /// existed.
    fn broadcaster_with(subscribers: &Mutex<Vec<Subscriber>>, sink: Arc<dyn EventSink>) -> Broadcaster<'_> {
        Broadcaster { subscribers, sink }
    }

    /// The `Broadcaster` most such tests want: they assert on the socket
    /// subscriber (a plain `mpsc::Receiver`), and care nothing about the
    /// in-process sink -- that half is proven by
    /// `broadcast_reaches_the_in_process_sink_even_with_no_socket_subscribers`
    /// and `a_successful_utterance_reaches_the_in_process_sink_via_process_utterance`
    /// below.
    fn dropping_broadcaster(subscribers: &Mutex<Vec<Subscriber>>) -> Broadcaster<'_> {
        broadcaster_with(subscribers, Arc::new(DropSink))
    }

    /// A `File` for `Daemon::_runtime_lock` in tests. No fake daemon here
    /// ever binds the real socket or competes with a real one for it, so the
    /// file just needs to exist -- nothing ever calls `try_lock_exclusive`
    /// on it the way `start` does.
    fn fake_runtime_lock() -> std::fs::File {
        let path = std::env::temp_dir().join("yappr-core-tests-fake-runtime-lock");
        std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(path).unwrap()
    }

    /// `Daemon::runtime_{socket,lock,port}_path` for every fake daemon:
    /// scratch paths under the OS temp dir, never the real
    /// `paths::runtime_*()` locations. `shutdown`'s `remove_runtime_files`
    /// only ever `remove_file`s these -- best-effort, so it does not matter
    /// that nothing here ever creates them -- but a test in this suite
    /// (`quit_reaps_the_llama_child_and_removes_the_runtime_files`) really
    /// does call `shutdown`, and a real daemon on this same machine may be
    /// holding `$XDG_RUNTIME_DIR/yappr.sock` open at that exact
    /// moment. These paths must never be able to collide with that.
    fn fake_runtime_files() -> (PathBuf, PathBuf) {
        // A fresh directory per call, not one shared path. Three tests
        // (`quit_removes_the_runtime_files` and the two
        // `wait_for_busy_to_clear_then_shutdown_*`) now *create* these files
        // and assert `shutdown` removed them, and `cargo test` runs them in
        // parallel -- on one shared path they would delete each other's
        // evidence and fail by scheduling accident.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "yappr-core-tests-fake-runtime-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        (dir.join("yappr.sock"), dir.join("yappr.lock"))
    }

    /// `fake_daemon_at` and `fake_daemon_with_normalize` keep their
    /// signatures and pass a sink that drops events, so no existing test
    /// changes. Only the test asserting on the second consumer needs this.
    fn fake_daemon_with_sink(initial_state: u8, sink: Arc<dyn EventSink>) -> Arc<Daemon> {
        let (runtime_socket_path, runtime_lock_path) = fake_runtime_files();
        Arc::new(Daemon {
            state: AtomicU8::new(initial_state),
            quitting: AtomicBool::new(false),
            recorder: Mutex::new(None),
            audio_cfg: Mutex::new(AudioConfig::default()),
            pipeline: Mutex::new(None),
            load_lock: Mutex::new(()),
            models_loaded: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
            models_cfg: Mutex::new(ModelsConfig::default()),
            realtime_cfg: Mutex::new(RealtimeConfig::default()),
            window_class: Mutex::new(None),
            config_path: PathBuf::from("/nonexistent/yappr-test/config.toml"),
            recording_epoch: AtomicU64::new(0),
            subscribers: Mutex::new(Vec::new()),
            normalize_enabled: false,
            normalize_available: AtomicBool::new(false),
            fatal_error: Mutex::new(None),
            config_notice: Mutex::new(None),
            realtime: Mutex::new(None),
            realtime_error: Mutex::new(None),
            housekeeping: Mutex::new(None),
            last_timings: Mutex::new(None),
            sink,
            runtime_socket_path,
            runtime_lock_path,
            _runtime_lock: fake_runtime_lock(),
        })
    }

    /// The overlay stopped being a socket client, so `broadcast` has two
    /// consumers now. A future edit that returns early for one of them (a
    /// no-subscribers fast path, say) would silently blind the overlay while
    /// every socket test still passed.
    #[test]
    fn broadcast_reaches_the_in_process_sink_even_with_no_socket_subscribers() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<OverlayEvent>>);
        impl EventSink for Recorder {
            fn emit(&self, e: &OverlayEvent) {
                self.0.lock().unwrap().push(e.clone());
            }
        }
        let sink = Arc::new(Recorder::default());
        let daemon = fake_daemon_with_sink(IDLE, sink.clone());
        assert!(lock_ignoring_poison(&daemon.subscribers).is_empty());

        daemon.broadcast(OverlayEvent::Transcribing);

        assert_eq!(sink.0.lock().unwrap().as_slice(), &[OverlayEvent::Transcribing]);
    }

    fn scratch_config(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-cfg-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // A hand-shaped fragment on purpose, not a rendered config: these
        // tests exercise what `dispatch` does with whatever is on disk, and a
        // file naming one key out of forty is the case where a writer that
        // rendered the incoming patch directly would destroy the rest.
        std::fs::write(&path, "[audio]\ndevice = \"default\"\n").unwrap();
        path
    }

    #[test]
    fn get_config_returns_the_whole_config_and_the_path_it_came_from() {
        let path = scratch_config("get");
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        let r = dispatch(&daemon, Request::GetConfig);

        assert!(r.ok, "{:?}", r.err);
        let config = r.config.expect("config missing");
        assert_eq!(config["audio"]["device"], "default");
        // Every section must be present -- the settings GUI renders whatever
        // it is given, so a section missing here is a section the user cannot
        // reach.
        for section in
            ["audio", "asr", "normalize", "guardrail", "inject", "vocabulary", "debug"]
        {
            assert!(config.get(section).is_some(), "section missing: {section}");
        }
        assert_eq!(r.config_path.as_deref(), Some(path.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The settings GUI's per-row reset button restores a field to its
    /// default, and the only place that knows the defaults is `Config` itself.
    /// Shipping them alongside the config is what keeps the GUI from carrying
    /// a second, drifting copy of the schema -- the same reason `config`
    /// crosses as JSON rather than as a mirrored struct.
    #[test]
    fn get_config_also_returns_the_defaults_the_gui_resets_a_field_to() {
        let path = scratch_config("get-defaults");
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        let r = dispatch(&daemon, Request::GetConfig);

        assert!(r.ok, "{:?}", r.err);
        let config = r.config.expect("config missing");
        let defaults = r.defaults.expect("defaults missing");
        // Every section the GUI can render must have a default to reset to.
        // A section present in one and absent from the other means rows that
        // show no reset button at all, which reads as "this is the default".
        let sections: Vec<&String> = config.as_object().expect("config is an object").keys().collect();
        assert!(!sections.is_empty());
        for section in sections {
            assert!(defaults.get(section).is_some(), "default missing for section: {section}");
        }
        // The scratch config sets `audio.device` explicitly; the default is
        // whatever `Config::default()` says, which is what makes the two
        // distinguishable at all.
        assert_eq!(defaults["audio"]["device"], serde_json::json!("default"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn set_config_writes_the_change_and_reports_that_nothing_needs_a_restart() {
        let path = scratch_config("set-live");
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        let r = dispatch(
            &daemon,
            Request::SetConfig {
                config: serde_json::json!({"vocabulary": {"terms": ["Hyprland"]}}),
            },
        );

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(r.restart_required, Some(false));
        let on_disk = Config::load_from(&path).unwrap();
        assert_eq!(on_disk.vocabulary.terms, ["Hyprland"]);
        // Everything the patch did not name keeps its on-disk value. This is
        // the partial-patch guarantee seen from the socket rather than from
        // `config_write`'s own tests: `SetConfig` passes its JSON through
        // untouched, so a writer that rendered it directly would blank the
        // rest of the file from right here.
        assert_eq!(on_disk.audio.device, "default");
        assert_eq!(on_disk.asr.num_threads, Config::default().asr.num_threads);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn set_config_reports_the_sections_that_need_a_restart() {
        let path = scratch_config("set-restart");
        let daemon = fake_daemon_at(IDLE, false, path.clone());
        // The precondition the assertion below is about. A `fake_daemon` has
        // nothing resident, which since the `models_loaded` gate landed is
        // the case that needs *no* restart -- so without this line this test
        // would pass on `Some(false)` and assert nothing.
        daemon.models_loaded.store(true, Ordering::SeqCst);

        let r = dispatch(
            &daemon,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 8}}) },
        );

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(r.restart_required, Some(true));
        assert!(r.restart_reason.unwrap().contains("[asr]"));
        // Written regardless: the file is the source of truth, and the change
        // takes effect at the next start.
        assert_eq!(Config::load_from(&path).unwrap().asr.num_threads, 8);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The same save on the daemon a user actually has -- `preload_at_startup`
    /// defaults to `false`, so nothing is resident until the first press.
    /// `SetConfig` must answer that no restart is needed, because
    /// `ensure_models_loaded` will read this very file on that press. This is
    /// the end-to-end counterpart of
    /// `nothing_needs_a_restart_while_no_models_are_resident`, which pins the
    /// rule; this pins that `SetConfig` actually consults it.
    #[test]
    fn set_config_does_not_ask_for_a_restart_when_nothing_is_resident_yet() {
        let path = scratch_config("set-restart-lazy");
        let daemon = fake_daemon_at(IDLE, false, path.clone());
        assert!(!daemon.models_loaded.load(Ordering::SeqCst), "precondition");

        let r = dispatch(
            &daemon,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 8}}) },
        );

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(r.restart_required, Some(false));
        assert_eq!(r.restart_reason, None);
        assert_eq!(Config::load_from(&path).unwrap().asr.num_threads, 8);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn set_config_rejects_an_invalid_config_and_leaves_the_file_untouched() {
        let path = scratch_config("set-invalid");
        let before = std::fs::read_to_string(&path).unwrap();
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        let r = dispatch(
            &daemon,
            Request::SetConfig {
                config: serde_json::json!({"guardrail": {"min_overlap_english": 9.0}}),
            },
        );

        assert!(!r.ok, "an out-of-range threshold must not be accepted");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "the file was modified");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Same guard as `reload`: swapping config or dropping the recorder while
    /// an utterance is in flight is not something to do.
    #[test]
    fn set_config_is_refused_while_the_daemon_is_busy() {
        let path = scratch_config("set-busy");
        let before = std::fs::read_to_string(&path).unwrap();
        let daemon = fake_daemon_at(RECORDING, false, path.clone());

        let r = dispatch(
            &daemon,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 8}}) },
        );

        assert!(!r.ok);
        assert!(r.err.unwrap().contains("idle"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Task 13 review: `PAUSED` is reached only by a CAS from `IDLE`, so
    /// there is never an in-flight recorder or pipeline state here to
    /// protect -- the same absence of risk `IDLE` itself already has. Before
    /// this widening, pausing dictation from the tray silently broke every
    /// settings autosave (a toggle immediately, a text field 700 ms after
    /// the last keystroke) until the user unpaused again, which is one menu
    /// row away from the pause checkbox itself.
    #[test]
    fn set_config_is_accepted_while_paused_since_pausing_only_ever_starts_from_idle() {
        let path = scratch_config("set-paused");
        let daemon = fake_daemon_at(PAUSED, false, path.clone());

        let r = dispatch(
            &daemon,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 8}}) },
        );

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(Config::load_from(&path).unwrap().asr.num_threads, 8);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Pinned separately from `set_config_is_refused_while_the_daemon_is_busy`
    /// (which uses `RECORDING`) so the widening above stays exactly
    /// `IDLE`/`PAUSED` and does not silently creep to cover a real `is_busy`
    /// sub-state too -- swapping the pipeline's config while an utterance is
    /// actually being transcribed is exactly the case this guard exists to
    /// prevent.
    #[test]
    fn set_config_is_still_refused_while_transcribing_after_widening_the_guard_for_paused() {
        let path = scratch_config("set-transcribing");
        let before = std::fs::read_to_string(&path).unwrap();
        let daemon = fake_daemon_at(TRANSCRIBING, false, path.clone());

        let r = dispatch(
            &daemon,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 8}}) },
        );

        assert!(!r.ok);
        assert!(r.err.unwrap().contains("idle"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "the file was modified");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The concern behind giving `Subscribe` its own thread, proven
    /// end-to-end over a real socket: a subscriber that connects and
    /// disconnects mid-stream must not wedge the accept loop for any other
    /// connection, and its dead sender must eventually be pruned.
    #[test]
    fn a_disconnected_subscriber_does_not_wedge_the_accept_loop_or_other_connections() {
        let dir =
            std::env::temp_dir().join(format!("yappr-daemon-test-subscribe-{}", std::process::id()));
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

    // -- Task 3, Work Item 3: FAILED state and the connect-time race ------

    #[test]
    fn state_of_and_snapshot_event_report_failed_as_a_real_error_state() {
        assert_eq!(state_of(FAILED), State::Error);
        assert_eq!(
            snapshot_event(FAILED),
            OverlayEvent::Error { reason: "daemon failed to start; see logs".to_string() }
        );
    }

    /// `dispatch` itself, not just the pure `state_of`/`snapshot_event`
    /// mappings: `status` against a `FAILED` daemon must report
    /// `state: "error"` plus the real stored reason (not just a generic
    /// one), and `ptt-start` must be rejected rather than trying to record
    /// with no pipeline ever built.
    #[test]
    fn dispatch_reports_and_rejects_correctly_against_a_failed_daemon() {
        let daemon = fake_daemon(FAILED);
        *lock_ignoring_poison(&daemon.fatal_error) = Some("no ggml compute backend".to_string());

        let status = dispatch(&daemon, Request::Status);
        assert!(status.ok);
        assert_eq!(status.state, Some(State::Error));
        assert_eq!(status.err.as_deref(), Some("no ggml compute backend"));

        let ptt_start = dispatch(&daemon, Request::PttStart);
        assert!(!ptt_start.ok, "ptt-start must be rejected against a daemon with no working pipeline");
    }

    /// Task 15: on a fresh install with no models yet, `load_models` fails with
    /// `SherpaTranscriber`/`SileroTrimmer`'s own "model paths" error and the
    /// daemon lands in `FAILED` with that exact message stored -- see
    /// `crates/yappr-core/src/asr.rs`'s `SherpaTranscriber::new`. `ptt-start`
    /// (and therefore `--toggle`, which delegates to it for every
    /// non-`RECORDING` state) must state that real reason, not the generic
    /// "see logs" `snapshot_event` falls back to when no reason was ever
    /// stored -- a user's only interface to this is a shortcut with no
    /// terminal, and "see logs" is not a stated reason to someone who has no
    /// logs open.
    #[test]
    fn ptt_start_against_a_failed_daemon_states_the_real_reason_not_a_generic_one() {
        let daemon = fake_daemon(FAILED);
        *lock_ignoring_poison(&daemon.fatal_error) = Some(
            "OfflineRecognizer::create returned None — check model paths and model_type"
                .to_string(),
        );

        let resp = dispatch(&daemon, Request::PttStart);

        assert!(!resp.ok);
        assert_eq!(
            resp.err.as_deref(),
            Some(
                "daemon failed to start: OfflineRecognizer::create returned None — check model paths and model_type"
            )
        );

        // Toggle delegates to PttStart for every state but RECORDING
        // (`toggle_target`), so the same stated reason must reach a
        // `--toggle` invocation too, not just a direct `ptt-start`.
        let toggled = dispatch(&daemon, Request::Toggle);
        assert_eq!(toggled.err, resp.err);
    }

    /// The fallback half of the same change: `fatal_error` is always
    /// populated by the time `FAILED` is externally observable (see
    /// `snapshot_event`'s doc comment), but this keeps `ptt-start`'s own
    /// formatting total rather than panicking or printing "None" if that
    /// were ever not true.
    #[test]
    fn ptt_start_against_a_failed_daemon_with_no_stored_reason_falls_back_gracefully() {
        let daemon = fake_daemon(FAILED);

        let resp = dispatch(&daemon, Request::PttStart);

        assert!(!resp.ok);
        assert_eq!(resp.err.as_deref(), Some("daemon failed to start: see logs"));
    }

    /// The specific gap Work Item 3 names: a fatal warm-up failure used to
    /// leave `daemon.state` at `WARMING` forever, so a subscriber connecting
    /// *after* the failure (not just one connected at the moment it
    /// happened) saw a permanent spinner instead of the error pill spec 12
    /// requires. This drives the real `handle`/`serve_subscriber` path over
    /// a socket against a daemon already in `FAILED` with a stored reason.
    #[test]
    fn a_late_subscriber_learns_about_a_fatal_warm_up_failure_instead_of_spinning_forever() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-test-subscribe-failed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let daemon = fake_daemon(FAILED);
        *lock_ignoring_poison(&daemon.fatal_error) = Some("no ggml compute backend".to_string());

        let accept_daemon = Arc::clone(&daemon);
        let accept_listener = listener.try_clone().unwrap();
        let accept_thread = std::thread::spawn(move || {
            for s in accept_listener.incoming().take(1).flatten() {
                handle(Arc::clone(&accept_daemon), s);
            }
        });

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        writeln!(stream, "{}", serde_json::to_string(&Request::Subscribe).unwrap()).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let event: OverlayEvent = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(
            event,
            OverlayEvent::Error { reason: "no ggml compute backend".to_string() },
            "a subscriber connecting after the failure must see the real reason, not a spinner"
        );

        accept_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `serve_subscriber`'s `NormalizeDegraded`-replay-on-connect path (spec
    /// 15): a late joiner that connects while normalization is *already*
    /// down must learn that immediately, as a second line right behind the
    /// state snapshot, not only if it happens to be subscribed at the exact
    /// moment a future `NormalizeDegraded` broadcast fires. Every other test
    /// in this file builds its daemon with `fake_daemon`, which hardcodes
    /// `normalize_enabled: false` -- making this path structurally
    /// unreachable no matter what any of them asserted. This is the one that
    /// actually drives it, over a real socket exactly like
    /// `a_late_subscriber_learns_about_a_fatal_warm_up_failure_instead_of_spinning_forever`
    /// above.
    #[test]
    fn a_late_subscriber_sees_the_degraded_badge_replayed_on_connect() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-test-subscribe-degraded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        // `normalize_available` defaults to `false` in `fake_daemon_with_normalize`
        // -- combined with `normalize_enabled: true` and `models_loaded: true`
        // (models are resident; the child just isn't healthy), this is
        // exactly "normalization is turned on but currently down", the
        // condition `serve_subscriber`/`normalize_degraded` check to decide
        // whether to replay the badge. Without `models_loaded: true` this
        // would instead be the ordinary idle-unloaded steady state, which
        // must NOT replay the badge -- see
        // `connect_snapshot_omits_the_degraded_badge_while_models_are_unloaded`.
        let daemon = fake_daemon_with_normalize(IDLE, true);
        daemon.models_loaded.store(true, Ordering::SeqCst);

        let accept_daemon = Arc::clone(&daemon);
        let accept_listener = listener.try_clone().unwrap();
        let accept_thread = std::thread::spawn(move || {
            for s in accept_listener.incoming().take(1).flatten() {
                handle(Arc::clone(&accept_daemon), s);
            }
        });

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        writeln!(stream, "{}", serde_json::to_string(&Request::Subscribe).unwrap()).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);

        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert_eq!(
            serde_json::from_str::<OverlayEvent>(first.trim()).unwrap(),
            OverlayEvent::Idle,
            "the state snapshot comes first, unaffected by normalization's own availability"
        );

        let mut second = String::new();
        reader.read_line(&mut second).unwrap();
        assert_eq!(
            serde_json::from_str::<OverlayEvent>(second.trim()).unwrap(),
            OverlayEvent::NormalizeDegraded {
                reason: "llama-server is down or unhealthy".to_string()
            },
            "a late joiner must learn normalization is already down, not just a future subscriber"
        );

        accept_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `connect_snapshot` is the in-process mirror of what `serve_subscriber`
    /// sends a freshly connected socket subscriber (proven over a real
    /// socket by the `a_late_subscriber_*` tests above) -- this proves
    /// the two agree directly, via the public entry point the app's
    /// `overlay_ready` Tauri command actually calls, covering both the
    /// degraded badge and the stored fatal-failure reason.
    #[test]
    fn connect_snapshot_mirrors_what_a_freshly_connected_subscriber_would_see() {
        // Same precondition `a_late_subscriber_sees_the_degraded_badge_replayed_on_connect`
        // relies on: `normalize_available` defaults to `false`, and
        // `models_loaded` is set explicitly so this models a real fault
        // (resident but unhealthy), not the ordinary idle-unloaded state.
        let daemon = fake_daemon_with_normalize(IDLE, true);
        daemon.models_loaded.store(true, Ordering::SeqCst);
        assert_eq!(
            daemon.connect_snapshot(),
            vec![
                OverlayEvent::Idle,
                OverlayEvent::NormalizeDegraded { reason: "llama-server is down or unhealthy".to_string() },
            ]
        );

        let failed = fake_daemon(FAILED);
        *lock_ignoring_poison(&failed.fatal_error) = Some("no ggml compute backend".to_string());
        assert_eq!(
            failed.connect_snapshot(),
            vec![OverlayEvent::Error { reason: "no ggml compute backend".to_string() }]
        );
    }

    /// Finding 2 of the Task 5 review: `normalize_enabled && !normalize_available`
    /// alone used to be treated as a fault, but lazy unloading makes
    /// `!normalize_available` the ordinary state between dictations too
    /// (`unload_models` clears it on every unload). A subscriber connecting
    /// while idle-unloaded must not be told normalization is degraded --
    /// there is no `llama-server` to be down while nothing is loaded.
    #[test]
    fn connect_snapshot_omits_the_degraded_badge_while_models_are_unloaded() {
        let daemon = fake_daemon_with_normalize(IDLE, true);
        // `models_loaded` defaults to `false` here -- the idle-unloaded (or
        // never-yet-loaded) steady state -- and `normalize_available` also
        // defaults to `false`, the exact combination that used to be
        // mistaken for a fault.
        assert_eq!(
            daemon.connect_snapshot(),
            vec![OverlayEvent::Idle],
            "the degraded badge must not be replayed while models are unloaded"
        );
    }

    /// Work Item 3's connect-time race: registering the subscriber and
    /// reading the snapshot happen inside one critical section on
    /// `daemon.subscribers`'s lock, the same lock `broadcast_to` takes
    /// before it iterates -- so a transition broadcast from another thread,
    /// fired concurrently with a new subscriber connecting, is never lost:
    /// the new subscriber either sees it in the snapshot already, or
    /// receives it live once registered. Driven many times with real
    /// threads (not asserted analytically) since this is exactly the kind
    /// of race that only shows up under real scheduling.
    ///
    /// F6: the racy socket subscriber's own assertion below is deliberately
    /// loose (`matches!`, not payload equality) -- `snapshot_event(RECORDING)`
    /// always synthesises `{level: 0.0, elapsed_ms: 0}` regardless of whether
    /// any broadcast ever happened (see its doc comment), so a subscriber
    /// that loses the race and only ever sees that synthetic snapshot is a
    /// legitimate, accepted outcome, not evidence of anything broken. That
    /// synthesis is exactly what let this test pass even with
    /// `racer_daemon.broadcast(..)` deleted outright: `racer_daemon.state
    /// .store(RECORDING, ..)` alone is enough to make some interleavings
    /// produce a first line that `matches!` a `Recording` variant, with the
    /// real `broadcast` call never having run at all. `control_rx` below
    /// closes that hole: it is registered, deterministically, *before* the
    /// racer thread is even spawned, so it is guaranteed to be listening
    /// when `broadcast_to` runs and must receive the exact real payload --
    /// deleting the broadcast call now fails this test every time, not just
    /// on an unlucky interleaving.
    #[test]
    fn a_transition_racing_a_new_subscriber_is_never_lost() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-test-subscribe-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();

        let daemon = fake_daemon(IDLE);
        let accept_daemon = Arc::clone(&daemon);
        let accept_listener = listener.try_clone().unwrap();
        let accept_thread = std::thread::spawn(move || {
            for s in accept_listener.incoming().take(1).flatten() {
                handle(Arc::clone(&accept_daemon), s);
            }
        });

        // A deterministic witness, registered before the race even starts:
        // proves the real broadcast (with its real payload) actually fires,
        // independent of however the socket subscriber's own race lands.
        let (control_tx, control_rx) = mpsc::channel();
        {
            let mut subs = lock_ignoring_poison(&daemon.subscribers);
            register_subscriber(&mut subs, control_tx, Arc::new(AtomicBool::new(true)));
        }

        // Race a broadcast against the connect: neither strictly happens
        // before the other from this test's point of view.
        let racer_daemon = Arc::clone(&daemon);
        let racer = std::thread::spawn(move || {
            racer_daemon.state.store(RECORDING, Ordering::SeqCst);
            racer_daemon.broadcast(OverlayEvent::Recording { level: 0.1, elapsed_ms: 5 });
        });

        let mut stream = UnixStream::connect(&sock_path).unwrap();
        writeln!(stream, "{}", serde_json::to_string(&Request::Subscribe).unwrap()).unwrap();
        stream.flush().unwrap();
        racer.join().unwrap();

        assert_eq!(
            control_rx.recv_timeout(Duration::from_secs(2)),
            Ok(OverlayEvent::Recording { level: 0.1, elapsed_ms: 5 }),
            "broadcast_to must actually deliver the real transition payload to an \
             already-registered subscriber"
        );

        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let first_event: OverlayEvent = serde_json::from_str(first.trim()).unwrap();

        // Whichever way the race landed, the racy subscriber must see the
        // `Recording` transition somewhere in its stream -- either as the
        // snapshot itself, or as the very next line if the snapshot still
        // caught `Idle`.
        let saw_recording = matches!(first_event, OverlayEvent::Recording { .. }) || {
            let mut second = String::new();
            reader.get_ref().set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            reader.read_line(&mut second).is_ok()
                && matches!(
                    serde_json::from_str::<OverlayEvent>(second.trim()),
                    Ok(OverlayEvent::Recording { .. })
                )
        };
        assert!(saw_recording, "the Recording transition must never be lost, got first={first_event:?}");

        accept_thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Task 3, Work Item 2: subscriber cap, eviction, and reaping -------

    #[test]
    fn register_subscriber_evicts_the_oldest_once_at_the_cap() {
        let mut subs: Vec<Subscriber> = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS {
            let (tx, rx) = mpsc::channel();
            register_subscriber(&mut subs, tx, Arc::new(AtomicBool::new(true)));
            receivers.push(rx);
        }
        assert_eq!(subs.len(), MAX_SUBSCRIBERS);

        let (new_tx, new_rx) = mpsc::channel();
        register_subscriber(&mut subs, new_tx, Arc::new(AtomicBool::new(true)));

        assert_eq!(subs.len(), MAX_SUBSCRIBERS, "must stay capped, not grow unbounded");
        assert!(
            receivers[0].recv().is_err(),
            "the oldest subscriber must be evicted (its Sender dropped) to make room"
        );

        let subs = Mutex::new(subs);
        broadcast_to(&subs, OverlayEvent::Idle);
        assert_eq!(new_rx.try_recv(), Ok(OverlayEvent::Idle), "the newest subscriber must still be registered");
    }

    #[test]
    fn reap_dead_subscribers_prunes_dead_entries_without_sending_anything() {
        let subs: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());
        let (dead_tx, dead_rx) = mpsc::channel();
        // Simulates the dead subscriber's own thread having already cleared
        // its liveness flag on exit (`ClearAliveOnDrop`).
        subs.lock().unwrap().push(Subscriber { tx: dead_tx, alive: Arc::new(AtomicBool::new(false)) });

        let (live_tx, live_rx) = mpsc::channel();
        subs.lock().unwrap().push(Subscriber { tx: live_tx, alive: Arc::new(AtomicBool::new(true)) });

        reap_dead_subscribers(&subs);

        assert_eq!(subs.lock().unwrap().len(), 1, "only the dead entry should be pruned");
        assert!(dead_rx.try_recv().is_err(), "reaping must not send anything at all");
        assert!(live_rx.try_recv().is_err(), "reaping must not send anything to the live subscriber either");
    }

    #[test]
    fn socket_peer_gone_detects_an_orderly_shutdown_but_not_an_open_idle_connection() {
        let (mut a, b) = UnixStream::pair().unwrap();
        assert!(!socket_peer_gone(&mut a), "a freshly connected, idle peer must not look gone");
        drop(b);
        assert!(socket_peer_gone(&mut a), "a closed peer must be detected");
    }

    /// The end-to-end version of Work Item 2's fix: a subscriber that
    /// disconnects while the daemon sits `Idle` -- broadcasting nothing at
    /// all -- must still be reaped, via its own liveness poll
    /// (`socket_peer_gone`) rather than needing any broadcast to happen.
    /// Before this fix, only a broadcast's incidental pruning ever removed a
    /// dead subscriber (see the older, broadcast-driven test above), which
    /// is exactly the gap review flagged: "never comes while the daemon
    /// sits idle."
    #[test]
    fn a_disconnected_subscriber_is_reaped_without_any_broadcast_while_idle() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-test-subscribe-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock_path = dir.join("test.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        let daemon = fake_daemon(IDLE);

        let accept_daemon = Arc::clone(&daemon);
        let accept_listener = listener.try_clone().unwrap();
        let accept_thread = std::thread::spawn(move || {
            for s in accept_listener.incoming().take(1).flatten() {
                handle(Arc::clone(&accept_daemon), s);
            }
        });

        {
            let mut stream = UnixStream::connect(&sock_path).unwrap();
            writeln!(stream, "{}", serde_json::to_string(&Request::Subscribe).unwrap()).unwrap();
            stream.flush().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // `stream` and its clone drop here: the client hangs up, and
            // nothing is ever broadcast afterward.
        }
        accept_thread.join().unwrap();

        // Mirrors `spawn_housekeeping`'s periodic tick, which is what
        // actually removes a dead entry from `daemon.subscribers` in
        // production -- the subscriber's own thread only clears its
        // `alive` flag (via `socket_peer_gone`); something else has to act
        // on that flag to shrink the list. Bounded poll: the flag itself
        // should flip within one `SUBSCRIBER_LIVENESS_POLL` (500ms) of the
        // disconnect, well inside this budget.
        let mut tries = 0;
        loop {
            reap_dead_subscribers(&daemon.subscribers);
            if lock_ignoring_poison(&daemon.subscribers).is_empty() || tries >= 200 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            tries += 1;
        }
        assert!(
            lock_ignoring_poison(&daemon.subscribers).is_empty(),
            "a disconnected subscriber must be reaped even though nothing was ever broadcast"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Task 3, Work Item 1: llama-server supervision (spec 15, 5.1) -----

    /// Task 3's shutdown-vs-backoff requirement, proven with the real
    /// threaded loop (`spawn_housekeeping`/`HousekeepingHandle`): a bogus
    /// `llama_server_path` makes every restart attempt fail near-instantly
    /// (an `ENOENT` on `Command::spawn`, never a real `llama-server` --
    /// satisfying the hard constraint against ever touching a real one),
    /// growing backoff past its 1s initial value. `stop()` called while the
    /// loop is asleep inside a *later* backoff wait must return promptly --
    /// nowhere near that wait's real duration -- proving it woke via the
    /// stop signal, not by timing out and attempting one more spawn first.
    ///
    /// F6: this used to pass `spawn_housekeeping` its production default of
    /// `initial_last_known_available: true`, which makes the loop's *first*
    /// wait `HEALTH_POLL_INTERVAL` (10s), not a backoff wait at all -- the
    /// 1200ms sleep below never got anywhere near even the first real health
    /// check, let alone a second backoff wait, so this test could never
    /// F6: `stop()` must interrupt the loop's `recv_timeout` rather than
    /// wait it out. The loop is asleep in a full `SUBSCRIBER_REAP_INTERVAL`
    /// (10 s) here, so a `stop()` that returned only when the wait expired
    /// on its own would blow the assertion by an order of magnitude.
    ///
    /// This used to be `shutdown_during_a_backoff_wait_...`, and had to
    /// arrange for the loop to be inside its *second*, doubled backoff wait
    /// to be meaningful. With `llama-server` supervision gone there is no
    /// backoff and only one wait, which makes the same property simpler to
    /// state and no weaker: the loop is provably asleep for 10 s, and
    /// `stop()` returns in well under one.
    #[test]
    fn stopping_housekeeping_interrupts_its_wait_instead_of_waiting_it_out() {
        let daemon = fake_daemon(IDLE);
        daemon.models_loaded.store(true, Ordering::SeqCst);

        let handle = spawn_housekeeping(Arc::clone(&daemon));

        // Long enough that the loop is certainly parked inside
        // `recv_timeout`, short enough that the 10 s wait is nowhere near
        // expiring on its own.
        std::thread::sleep(Duration::from_millis(200));

        let t0 = Instant::now();
        handle.stop();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "stop() should return almost immediately, not wait out the tick; took {elapsed:?}"
        );
    }


    // -- Task 7: Request::Toggle -------------------------------------------

    /// `toggle_target` carries all of Toggle's state-dependent behaviour as a
    /// pure function precisely so it can be tested exhaustively, over every
    /// state constant, without ever constructing a `Daemon`.
    ///
    /// The reason that separation exists at all: `start_recording` (reached
    /// from every non-RECORDING state via `PttStart`) calls
    /// `ensure_recorder(&daemon.recorder, || Recorder::new(..))`, which
    /// constructs a *real* recorder the moment the slot is empty -- and every
    /// fake daemon's slot is empty (`fake_daemon_at` sets
    /// `recorder: Mutex::new(None)`). A test that dispatched `Toggle` from
    /// `IDLE` through `dispatch` and asserted `RECORDING` -- the shape this
    /// task's plan originally called for -- would therefore open the real
    /// microphone every time `cargo test --workspace` ran. CLAUDE.md
    /// prohibits that outright ("do not record from the microphone without
    /// explicit permission"), and no test anywhere in this file drives
    /// `PttStart` from `IDLE` through `dispatch` for the same reason: the one
    /// existing `PttStart`-through-`dispatch` test,
    /// `dispatch_reports_and_rejects_correctly_against_a_failed_daemon`,
    /// starts from `FAILED`, which is refused before `start_recording` is
    /// ever called.
    ///
    /// Do not "fix" that gap by adding an IDLE-through-`dispatch` test here.
    /// There is no seam to stub `Recorder` behind -- `daemon.recorder` holds
    /// a concrete `Recorder`, not a trait object -- so proving the mapping
    /// this function embodies, exhaustively and with no `Daemon` in sight, is
    /// the actual assurance available; the one-line delegation in `dispatch`
    /// (`Request::Toggle => dispatch(daemon, toggle_target(current))`) is
    /// then correct by inspection, the same trade this project already made
    /// for `owf-cli`'s `route()`.
    #[test]
    fn toggle_target_maps_recording_to_ptt_stop_and_every_other_state_to_ptt_start() {
        assert_eq!(toggle_target(RECORDING), Request::PttStop);
        for other in [WARMING, IDLE, TRANSCRIBING, NORMALIZING, INJECTING, FAILED, PAUSED] {
            assert_eq!(toggle_target(other), Request::PttStart, "state {other} must toggle to PttStart");
        }
    }

    /// The one non-refusal case reachable through `dispatch` without
    /// hardware: `RECORDING` maps to `PttStop`, whose handler never touches
    /// `daemon.recorder` for *construction* -- it only calls `.stop()` on
    /// whatever is already there -- and the `run_utterance` it spawns (on
    /// its own thread, same as `PttStop` always does) takes an early,
    /// hardware-free return when that slot is `None`, exactly as every fake
    /// daemon's is: it logs a warning, broadcasts `OverlayEvent::Error`, and
    /// returns without ever calling `Recorder::new`. This test relies on
    /// nothing about that spawned thread beyond what `PttStop`'s own
    /// behaviour already guarantees.
    #[test]
    fn toggle_stops_and_begins_transcribing_when_recording() {
        let d = fake_daemon(RECORDING);
        let r = dispatch(&d, Request::Toggle);
        assert!(r.ok);
        assert_eq!(r.state, Some(State::Transcribing));
    }

    /// Spec 6.1's busy flash, reached through Toggle: a press while the
    /// previous utterance is still being processed must not open a second
    /// recording behind it. Invariant 1 protects the text an utterance has
    /// already produced; nothing protects it from a second recording
    /// clobbering the pipeline while that text is still on its way out.
    /// None of these three states reach `start_recording` -- `is_busy` is
    /// exactly `TRANSCRIBING | NORMALIZING | INJECTING`, and `PttStart`'s own
    /// match refuses all three before ever falling through to the `_` arm
    /// that calls it.
    #[test]
    fn toggle_refuses_while_an_utterance_is_still_being_processed() {
        for busy in [TRANSCRIBING, NORMALIZING, INJECTING] {
            let d = fake_daemon(busy);
            let r = dispatch(&d, Request::Toggle);
            assert!(!r.ok, "toggle must be refused in state {busy}");
            assert_eq!(d.state.load(Ordering::SeqCst), busy, "and must not change the daemon's state");
        }
    }

    /// `WARMING` and `FAILED` are both refused by name in `PttStart`'s match,
    /// before `start_recording` is ever reached.
    #[test]
    fn toggle_refuses_while_warming_or_failed_with_a_stated_reason() {
        let warming = fake_daemon(WARMING);
        let r = dispatch(&warming, Request::Toggle);
        assert!(!r.ok);
        assert_eq!(r.err.as_deref(), Some("warming"));
        assert_eq!(warming.state.load(Ordering::SeqCst), WARMING, "and must not change the daemon's state");

        let failed = fake_daemon(FAILED);
        let r = dispatch(&failed, Request::Toggle);
        assert!(!r.ok);
        assert!(r.err.is_some());
        assert_eq!(failed.state.load(Ordering::SeqCst), FAILED, "and must not change the daemon's state");
    }

    /// Hold-to-talk had a physical guarantee that recording ends: you let
    /// go. Toggle has none. This watchdog is now the only thing that closes
    /// a microphone whose second press never came, and it must *transcribe*
    /// what it captured rather than discard it (invariant 1).
    ///
    /// Driven directly against `spawn_safety_valve`, never through
    /// `dispatch(&d, Request::Toggle)`/`start_recording`: reaching the valve
    /// that way would call `ensure_recorder` -> `Recorder::new`, which opens
    /// a real microphone -- forbidden in this suite. `fake_daemon`'s
    /// `recorder` slot is `None`, so the `run_utterance` this test exercises
    /// takes the same hardware-free `None` arm that
    /// `toggle_stops_and_begins_transcribing_when_recording` already relies
    /// on for `PttStop`'s identical spawned call.
    #[test]
    fn a_recording_with_no_second_press_is_ended_and_transcribed_by_the_safety_valve() {
        let d = fake_daemon(RECORDING);
        // The valve reads `audio_cfg`, so shorten it rather than sleeping
        // out a real 120 s default.
        lock_ignoring_poison(&d.audio_cfg).max_seconds = 1;
        let epoch = d.recording_epoch.load(Ordering::SeqCst);

        spawn_safety_valve(&d, epoch);
        std::thread::sleep(Duration::from_millis(1_500));

        assert_ne!(
            d.state.load(Ordering::SeqCst),
            RECORDING,
            "the microphone is still open after max_seconds"
        );
    }

    /// Concurrency note 3, pinned: a safety valve spawned for one recording
    /// session must not act on a later one. Under hold-to-talk this was a
    /// narrow race (a stuck-key timer outliving a key that was, in fact,
    /// released promptly next time); under toggle a forgotten press means
    /// valve firings and real second presses interleave far more often, so
    /// this matters more than it used to.
    #[test]
    fn a_stale_safety_valve_from_an_earlier_session_does_not_stop_a_later_one() {
        let d = fake_daemon(RECORDING);
        lock_ignoring_poison(&d.audio_cfg).max_seconds = 1;
        let stale_epoch = d.recording_epoch.load(Ordering::SeqCst);

        spawn_safety_valve(&d, stale_epoch);
        // A later session (a real second press ending this recording and a
        // fresh one beginning, or simply `start_recording` running again)
        // bumps the epoch before the stale timer above fires.
        d.recording_epoch.fetch_add(1, Ordering::SeqCst);

        std::thread::sleep(Duration::from_millis(1_500));

        assert_eq!(
            d.state.load(Ordering::SeqCst),
            RECORDING,
            "a stale safety valve must not touch a session it does not own"
        );
    }

    // -- Task 13: Pause -----------------------------------------------------

    /// The shortcut stays bound while paused -- it just opens no microphone.
    /// `Toggle` from `IDLE` normally resolves to `PttStart`; while `PAUSED`
    /// it must still resolve to `PttStart` (`toggle_target`'s "every other
    /// state" case) and that `PttStart` must be refused by name, never fall
    /// through to `start_recording`.
    #[test]
    fn toggle_is_refused_while_paused_and_opens_no_microphone() {
        let d = fake_daemon_with_normalize(IDLE, false);
        let paused = dispatch(&d, Request::SetPaused { paused: true });
        assert!(paused.ok);
        assert_eq!(paused.state, Some(State::Paused));
        assert_eq!(d.state.load(Ordering::SeqCst), PAUSED);

        let r = dispatch(&d, Request::Toggle);
        assert!(!r.ok);
        assert_eq!(r.err.as_deref(), Some("paused"));
        assert_eq!(d.state.load(Ordering::SeqCst), PAUSED, "toggle must not change the daemon's state");
    }

    /// The same refusal, reached directly through `PttStart` rather than via
    /// `Toggle`'s delegation -- `owf-ctl ptt-start` is a real, separate
    /// client of this arm (the Hyprland keybind's press half under
    /// press/press toggle uses `Toggle`, but nothing stops a hand-run
    /// `ptt-start` from reaching it too).
    #[test]
    fn ptt_start_is_refused_while_paused_with_a_stated_reason() {
        let d = fake_daemon(PAUSED);
        let r = dispatch(&d, Request::PttStart);
        assert!(!r.ok);
        assert_eq!(r.err.as_deref(), Some("paused"));
        assert_eq!(d.state.load(Ordering::SeqCst), PAUSED);
    }

    /// Invariant 1: pausing is not a way to lose an utterance already in
    /// flight. The compare-and-exchange only ever fires from `IDLE`, so a
    /// `SetPaused` reaching the daemon mid-utterance is silently dropped --
    /// not queued, not retried -- rather than seizing whatever busy
    /// sub-state it landed on.
    #[test]
    fn pausing_never_interrupts_an_utterance_already_in_flight() {
        for busy in [TRANSCRIBING, NORMALIZING, INJECTING] {
            let d = fake_daemon_with_normalize(busy, false);
            let r = dispatch(&d, Request::SetPaused { paused: true });
            assert!(r.ok, "SetPaused itself is not refused, it just does not take");
            assert_eq!(
                d.state.load(Ordering::SeqCst),
                busy,
                "pause must defer, never seize, busy state {busy}"
            );
        }
    }

    /// `RECORDING` gets the identical treatment, even though it is
    /// deliberately outside `is_busy` (see that function's own tests): the
    /// CAS only ever fires from `IDLE`, so a recording already underway is
    /// just as protected as a busy sub-state, with no separate check needed.
    #[test]
    fn pausing_defers_rather_than_interrupts_a_recording_in_progress() {
        let d = fake_daemon(RECORDING);
        let r = dispatch(&d, Request::SetPaused { paused: true });
        assert!(r.ok);
        assert_eq!(d.state.load(Ordering::SeqCst), RECORDING, "pause must not stop an open microphone");
    }

    #[test]
    fn unpausing_returns_to_idle() {
        let d = fake_daemon_with_normalize(PAUSED, false);
        let r = dispatch(&d, Request::SetPaused { paused: false });
        assert!(r.ok);
        assert_eq!(r.state, Some(State::Idle));
        assert_eq!(d.state.load(Ordering::SeqCst), IDLE);
    }

    /// An unpause request against a daemon that was never paused is a no-op,
    /// not an error -- there is nothing for the CAS to do, and `SetPaused`
    /// always reports `ok` (mirroring `PttStop`'s "already idle" branch).
    #[test]
    fn unpausing_a_daemon_that_is_not_paused_is_a_harmless_no_op() {
        let d = fake_daemon(IDLE);
        let r = dispatch(&d, Request::SetPaused { paused: false });
        assert!(r.ok);
        assert_eq!(d.state.load(Ordering::SeqCst), IDLE);
    }

    /// The tray's icon/checkbox is driven entirely by broadcasts
    /// (`tray.rs`'s "Icon and daemon state") -- a transition that changed
    /// `daemon.state` but broadcast nothing would leave the tray showing
    /// stale state forever. This is the one test in this file that reaches
    /// past `dispatch`'s return value into the in-process sink to prove the
    /// transition itself is actually observable.
    #[test]
    fn pausing_and_unpausing_each_broadcast_exactly_one_event_to_the_in_process_sink() {
        let seen: Arc<Mutex<Vec<OverlayEvent>>> = Arc::new(Mutex::new(Vec::new()));
        struct RecordingSink(Arc<Mutex<Vec<OverlayEvent>>>);
        impl EventSink for RecordingSink {
            fn emit(&self, event: &OverlayEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        let d = fake_daemon_with_sink(IDLE, Arc::new(RecordingSink(Arc::clone(&seen))));

        dispatch(&d, Request::SetPaused { paused: true });
        assert_eq!(seen.lock().unwrap().as_slice(), [OverlayEvent::Paused]);

        dispatch(&d, Request::SetPaused { paused: false });
        assert_eq!(seen.lock().unwrap().as_slice(), [OverlayEvent::Paused, OverlayEvent::Idle]);
    }

    /// A pause request that does not take (mid-utterance) must not broadcast
    /// anything either -- there was no transition, so there is nothing for
    /// the tray to learn about.
    #[test]
    fn a_pause_request_that_does_not_take_broadcasts_nothing() {
        let seen: Arc<Mutex<Vec<OverlayEvent>>> = Arc::new(Mutex::new(Vec::new()));
        struct RecordingSink(Arc<Mutex<Vec<OverlayEvent>>>);
        impl EventSink for RecordingSink {
            fn emit(&self, event: &OverlayEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        let d = fake_daemon_with_sink(TRANSCRIBING, Arc::new(RecordingSink(Arc::clone(&seen))));

        dispatch(&d, Request::SetPaused { paused: true });
        assert!(seen.lock().unwrap().is_empty());
    }

    // -- Task 10: Request::Quit / Request::ShowSettings (spec §8) ----------

    /// Serializes the handful of tests in this file that call the *real*
    /// `shutdown` (as opposed to `kill_llama`/`stop_housekeeping` directly).
    /// `shutdown` mutates the module-wide `SHUTTING_DOWN` static, which is
    /// designed for "called at most once per process" -- true in production,
    /// but not true of a test binary where `cargo test`'s default thread-per-
    /// test parallelism could otherwise run two such tests at once, letting
    /// whichever calls `shutdown` second see `SHUTTING_DOWN` already `true`
    /// and silently skip its cleanup (a spurious, scheduling-order-dependent
    /// failure, not a real bug). Holding this lock for the test body and
    /// resetting the flag to `false` first makes each such test observe
    /// `SHUTTING_DOWN` exactly as if it were the only caller in the process.
    static SHUTDOWN_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_a_fresh_shutting_down_guard(f: impl FnOnce()) {
        let _guard = lock_ignoring_poison(&SHUTDOWN_TEST_LOCK);
        SHUTTING_DOWN.store(false, Ordering::SeqCst);
        f();
    }

    /// Spec §8: Beenden must leave nothing behind. The llama-server child is
    /// the one that leaks today when the process is killed rather than
    /// dropped. Exercises the real `shutdown` (not just `kill_llama`
    /// directly, which `kill_llama_terminates_a_stub_child_and_is_idempotent`
    /// already covers) so the full `Request::Quit` teardown path -- minus
    /// the wait and the `exit` themselves, both covered by the tests below
    /// -- is proven end to end.
    ///
    /// Uses `fake_daemon_at`, whose `runtime_*_path` fields are scratch
    /// paths (`fake_runtime_files`), never the real
    /// `$XDG_RUNTIME_DIR/yappr.sock` -- this machine may have a real
    /// daemon holding that file open while this suite runs, and `shutdown`
    /// really does call `remove_file` on whatever paths it's given.
    ///
    /// The observable used to be a `sleep 300` stub child that `shutdown`
    /// had to reap. With S1-mini in-process there is no child to observe,
    /// so the runtime files `shutdown` removes are the observable instead --
    /// which is the more direct test of this path anyway: it asserts on
    /// `shutdown`'s own last step rather than on a side effect of it.
    #[test]
    fn quit_removes_the_runtime_files() {
        with_a_fresh_shutting_down_guard(|| {
            let d = fake_daemon_at(IDLE, false, PathBuf::from("/nonexistent/yappr-test/config.toml"));
            create_runtime_files(&d);

            shutdown(&d);

            assert!(!d.runtime_socket_path.exists(), "the socket file survived shutdown");
            assert!(!d.runtime_lock_path.exists(), "the lock file survived shutdown");
        });
    }

    /// Creates the scratch socket/lock files `shutdown` is expected to
    /// remove, so their absence afterwards is evidence rather than the
    /// default state.
    fn create_runtime_files(d: &Daemon) {
        let dir = d.runtime_socket_path.parent().expect("scratch runtime dir");
        std::fs::create_dir_all(dir).expect("creating the scratch runtime dir");
        std::fs::write(&d.runtime_socket_path, b"").expect("creating the scratch socket file");
        std::fs::write(&d.runtime_lock_path, b"").expect("creating the scratch lock file");
    }

    /// Closes the gap a reviewer found in the first pass: `wait_for_busy_to_
    /// clear` and `shutdown` each had tests, but never their *composition*
    /// (`wait_for_busy_to_clear_then_shutdown`, what `Request::Quit` actually
    /// calls) -- so a regression that shuffled the two calls into `shutdown`
    /// running *before* the wait would have kept every existing test in this
    /// file green while reintroducing the exact defect this task exists to
    /// fix. Proves both halves of the real sequencing end to end, observed
    /// through the runtime files `shutdown` removes: they must still be
    /// there while the pipeline is `TRANSCRIBING`, and gone once the state
    /// clears and `shutdown` finally runs. No `std::process::exit` anywhere
    /// near this test -- only `wait_for_busy_to_clear_then_shutdown_with`,
    /// never the `Request::Quit` dispatch arm itself.
    #[test]
    fn wait_for_busy_to_clear_then_shutdown_waits_for_the_state_to_clear() {
        with_a_fresh_shutting_down_guard(|| {
            let d = fake_daemon_at(TRANSCRIBING, false, PathBuf::from("/nonexistent/yappr-test/config.toml"));
            create_runtime_files(&d);

            let d2 = Arc::clone(&d);
            let handle = std::thread::spawn(move || {
                wait_for_busy_to_clear_then_shutdown_with(&d2, Duration::from_secs(10), Duration::from_millis(5));
            });

            std::thread::sleep(Duration::from_millis(100));
            assert!(
                d.runtime_socket_path.exists(),
                "shutdown must not run while still TRANSCRIBING"
            );

            d.state.store(IDLE, Ordering::SeqCst);
            handle.join().expect("wait/shutdown thread panicked");

            assert!(
                !d.runtime_socket_path.exists(),
                "shutdown must run once the state clears"
            );
        });
    }

    /// `Request::Restart`'s whole correctness condition, and the one thing
    /// about this feature that cannot be discovered by trying it once on a
    /// good day: the successor must be started *after* `shutdown`, because
    /// `shutdown` is what unlinks `yappr.lock` and the successor takes an
    /// exclusive `flock` on it. Start it any earlier -- which is exactly
    /// what Tauri's own `AppHandle::restart` does, spawning before it tears
    /// down -- and the successor loses the lock, prints "yappr is already
    /// running", exits 1, and then this process exits too: a restart button
    /// that closes the app for good.
    ///
    /// The spy answers "was the lock still there when you were called?", so
    /// a passing run is evidence about ordering rather than about the mere
    /// fact of the call. It also re-proves the busy wait for the relaunch
    /// path specifically: a restart that relaunched while `TRANSCRIBING`
    /// would destroy the utterance invariant 1 protects, and the assertion
    /// before the state is cleared is what catches that.
    ///
    /// `wait_for_busy_to_clear_then_shutdown_and_relaunch_with`, never the
    /// `Request::Restart` dispatch arm -- that arm ends in
    /// `std::process::exit(0)`, which would take the test runner with it.
    #[test]
    fn restart_starts_the_successor_only_after_shutdown_released_the_lock() {
        with_a_fresh_shutting_down_guard(|| {
            #[derive(Default)]
            struct RelaunchSpy {
                lock_path: Mutex<Option<PathBuf>>,
                /// `None` until `relaunch` is called; then whether the lock
                /// file was still present at that moment.
                lock_present_at_relaunch: Mutex<Option<bool>>,
            }
            impl EventSink for RelaunchSpy {
                fn emit(&self, _: &OverlayEvent) {}
                fn relaunch(&self) {
                    let path = self
                        .lock_path
                        .lock()
                        .unwrap()
                        .clone()
                        .expect("the test sets the lock path before dispatching");
                    *self.lock_present_at_relaunch.lock().unwrap() = Some(path.exists());
                }
            }

            let spy = Arc::new(RelaunchSpy::default());
            let d = fake_daemon_at_with_sink(
                TRANSCRIBING,
                false,
                PathBuf::from("/nonexistent/yappr-test/config.toml"),
                spy.clone(),
            );
            *spy.lock_path.lock().unwrap() = Some(d.runtime_lock_path.clone());
            create_runtime_files(&d);

            let d2 = Arc::clone(&d);
            let handle = std::thread::spawn(move || {
                wait_for_busy_to_clear_then_shutdown_and_relaunch_with(
                    &d2,
                    Duration::from_secs(10),
                    Duration::from_millis(5),
                );
            });

            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                *spy.lock_present_at_relaunch.lock().unwrap(),
                None,
                "must not relaunch while an utterance is still TRANSCRIBING"
            );

            d.state.store(IDLE, Ordering::SeqCst);
            handle.join().expect("wait/shutdown/relaunch thread panicked");

            assert_eq!(
                *spy.lock_present_at_relaunch.lock().unwrap(),
                Some(false),
                "the successor was started before shutdown released the lock -- it would exit 1"
            );
        });
    }

    /// The bound half of the same composition: a pipeline wedged forever in
    /// a busy state must not hang `shutdown` forever either -- past
    /// `server.rs`'s wait bound, `wait_for_busy_to_clear_then_shutdown` shuts
    /// down anyway. The state is never cleared here, so a passing assertion
    /// proves the bound itself, not a state change, is what let `shutdown`
    /// run.
    #[test]
    fn wait_for_busy_to_clear_then_shutdown_gives_up_at_the_bound_and_shuts_down_anyway() {
        with_a_fresh_shutting_down_guard(|| {
            let d = fake_daemon_at(TRANSCRIBING, false, PathBuf::from("/nonexistent/yappr-test/config.toml"));
            create_runtime_files(&d);

            wait_for_busy_to_clear_then_shutdown_with(&d, Duration::from_millis(60), Duration::from_millis(5));

            assert!(
                !d.runtime_socket_path.exists(),
                "must shut down anyway once the bound elapses, even while still busy"
            );
        });
    }

    /// The defect fix at the heart of this task: the plan's original `Quit`
    /// handler would shut down unconditionally, which for a `TRANSCRIBING`/
    /// `NORMALIZING`/`INJECTING` daemon would kill `llama-server` and stop
    /// housekeeping out from under an utterance that has not yet reached
    /// `finish`/injection -- invariant 1 says a transcribed utterance is
    /// never lost. `wait_for_busy_to_clear` is what a real `Request::Quit`
    /// waits on before calling `shutdown`; this proves it actually blocks
    /// while busy and releases the instant the state clears, using
    /// `std::thread::scope` so the busy `AtomicU8` can be a plain stack
    /// value shared with the waiting thread -- no `Daemon`, no `shutdown`
    /// call, and, per `wait_for_busy_to_clear`'s doc comment, no
    /// `std::process::exit` anywhere near this test.
    #[test]
    fn a_quit_arriving_during_transcribing_does_not_proceed_until_the_state_clears() {
        let state = AtomicU8::new(TRANSCRIBING);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                wait_for_busy_to_clear(&state, Duration::from_secs(10), Duration::from_millis(5));
            });

            std::thread::sleep(Duration::from_millis(150));
            assert!(
                !waiter.is_finished(),
                "must not treat TRANSCRIBING as safe to shut down while it is still the state"
            );

            state.store(IDLE, Ordering::SeqCst);
            waiter.join().expect("waiter thread panicked");
        });
    }

    /// The other half of the same fix: the wait is bounded (spec §8's "must
    /// not make Beenden unresponsive forever"), so a pipeline wedged forever
    /// in a busy state does not hang `Request::Quit`'s shutdown thread
    /// forever either. Uses a tiny timeout/poll pair so the test itself
    /// stays fast; `QUIT_BUSY_WAIT_TIMEOUT` is the real ~10 s bound used in
    /// production.
    #[test]
    fn wait_for_busy_to_clear_gives_up_once_its_bound_elapses_if_still_busy() {
        let state = AtomicU8::new(TRANSCRIBING);
        let start = Instant::now();

        wait_for_busy_to_clear(&state, Duration::from_millis(60), Duration::from_millis(5));

        assert!(
            start.elapsed() >= Duration::from_millis(60),
            "must actually wait out the bound, not return early"
        );
        assert!(
            is_busy(state.load(Ordering::SeqCst)),
            "state is still busy: the bound, not a cleared state, is what ended the wait"
        );
    }

    /// Closes the other gap a reviewer found: spec §8 step 1's first clause
    /// ("stop accepting new utterances") was never implemented -- only its
    /// second half ("let one already in flight finish") was. Without
    /// `Daemon::quitting`, a `PttStart`/`Toggle` landing on the accept loop's
    /// very next connection after Beenden would be accepted normally and
    /// could reach `TRANSCRIBING` inside the window while the spawned
    /// shutdown thread is stopping housekeeping and reaping `llama-server`.
    /// Drives `dispatch` directly (not through `Request::Quit`, which this
    /// suite must never call -- see this file's other `#[test]`s' doc
    /// comments) by setting the flag exactly as that arm now does.
    #[test]
    fn ptt_start_and_toggle_are_refused_once_the_quitting_flag_is_set() {
        let d = fake_daemon(IDLE);
        d.quitting.store(true, Ordering::SeqCst);

        let ptt_start = dispatch(&d, Request::PttStart);
        assert!(!ptt_start.ok, "PttStart must be refused once quitting");

        let toggle = dispatch(&d, Request::Toggle);
        assert!(!toggle.ok, "Toggle must be refused once quitting (it would resolve to PttStart from IDLE)");
    }

    /// `Request::ShowSettings` has nothing to validate against daemon state
    /// (spec §8: "show and focus the settings window" is unconditional), so
    /// the only behaviour worth pinning is that it reaches the sink -- the
    /// seam `TauriSink::show_settings` (`src-tauri/src/lib.rs`) hangs off.
    #[test]
    fn show_settings_reaches_the_sink() {
        #[derive(Default)]
        struct Recorder(Mutex<bool>);
        impl EventSink for Recorder {
            fn emit(&self, _: &OverlayEvent) {}
            fn show_settings(&self) {
                *lock_ignoring_poison(&self.0) = true;
            }
        }
        let sink = Arc::new(Recorder::default());
        let daemon = fake_daemon_with_sink(IDLE, sink.clone());

        let r = dispatch(&daemon, Request::ShowSettings);

        assert!(r.ok, "{:?}", r.err);
        assert!(*lock_ignoring_poison(&sink.0), "show_settings must reach the sink");
    }

    /// A `Pipeline` built from the stub stages this module already defines, for
    /// tests that only need *a* pipeline to be installed. None of them ever call
    /// `process` on it -- a real one needs an ASR model on disk, which is why
    /// this file's four model-dependent tests are `#[ignore]`d.
    fn stub_pipeline() -> Pipeline {
        Pipeline::new(
            Config::from_str("").unwrap(),
            Box::new(PanickingTranscriber),
            Box::new(WholeBuffer),
            Box::new(AlwaysEnglish),
            Box::new(NeverNormalizer),
            Box::new(MockInjector::default()),
        )
    }

    /// The seam that makes the loader testable at all. Counts its own calls, so
    /// idempotence is observable.
    fn counting_loader(
        calls: Arc<AtomicU64>,
    ) -> impl FnMut() -> anyhow::Result<(Pipeline, bool)> {
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((stub_pipeline(), false))
        }
    }

    /// What every load test that is *not* about shutdown passes for
    /// `ensure_models_loaded_with`'s flag: a static that nothing ever sets.
    ///
    /// The production `SHUTTING_DOWN` is a process-global, and three tests in
    /// this file drive the real `shutdown` and leave it `true` behind them.
    /// Reading it here would make those tests and these ones fail each other
    /// by scheduling accident under `cargo test`'s default parallelism --
    /// which is exactly why the flag is a parameter at all (see
    /// `ensure_models_loaded_with`'s doc comment).
    static NOT_SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

    /// Final-review fix 2. `shutdown` takes no `load_lock` -- it must not,
    /// because that would hang Beenden for up to `STARTUP_HEALTH_TIMEOUT`
    /// behind an in-flight load -- so a load still running when teardown
    /// begins used to install its `llama-server` into `daemon.llama` *after*
    /// `shutdown`'s `kill_llama` had already looked there and found `None`.
    /// `Request::Quit`'s `std::process::exit(0)` then left a ~955 MB child
    /// resident, silently: nothing errors, the next start's `pick_port`
    /// simply walks past it, and the user loses precisely the memory this
    /// feature exists to reclaim. Reachable in three keystrokes -- SUPER+D,
    /// SUPER+ALT+D, Beenden -- because cancelling leaves the state `IDLE`,
    /// which `wait_for_busy_to_clear` does not wait on, while the loader
    /// thread `start_recording` spawned is still going. `SIGTERM` at logout
    /// is the same shape.
    ///
    /// The flag is flipped *inside* the loader, which is the moment that
    /// matters: `load_lock` is held and the caller is committed.
    /// A real load cannot run in this suite (no ASR model on the test path),
    /// so `stub_pipeline` stands in -- the same substitution
    /// `unloading_drops_the_pipeline_and_with_it_the_model` already makes.
    ///
    /// This test used to also assert, via `process_is_alive`, that the
    /// `llama-server` child the load had spawned was killed rather than
    /// orphaned -- that assertion was the one carrying the fix, because
    /// clearing `pipeline` and `models_loaded` satisfied everything else
    /// while still leaving a ~955 MB child behind. There is no child now:
    /// the engine is owned by the pipeline's normalizer, so dropping the
    /// pipeline *is* releasing the model, and the three assertions below
    /// are the whole of the property.
    #[test]
    fn a_load_finishing_during_shutdown_releases_it_instead_of_installing_it() {
        let daemon = fake_daemon(IDLE);
        let shutting_down = AtomicBool::new(false);
        let mut load = || {
            // Beenden lands here: `shutdown` runs to completion on its own
            // thread while this load is still in flight.
            shutting_down.store(true, Ordering::SeqCst);
            Ok((stub_pipeline(), true))
        };

        let err = ensure_models_loaded_with(&daemon, &mut load, &shutting_down)
            .expect_err("a load that finished after teardown began must not report success");

        assert!(err.contains("shutting down"), "unexpected reason: {err}");
        assert!(!daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_none());
        assert!(!daemon.normalize_available.load(Ordering::SeqCst));
    }

    /// The other half, and the one that stops the re-check above from being
    /// implemented as "always tear down": an ordinary load, with the flag
    /// clear, must still install everything it produced.
    #[test]
    fn a_load_finishing_normally_still_installs_what_it_produced() {
        let daemon = fake_daemon(IDLE);
        let mut load = || Ok((stub_pipeline(), true));

        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();

        assert!(daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
        assert!(daemon.normalize_available.load(Ordering::SeqCst));
    }

    #[test]
    fn loading_the_models_twice_only_loads_them_once() {
        let daemon = fake_daemon(IDLE);
        let calls = Arc::new(AtomicU64::new(0));

        let mut load = counting_loader(Arc::clone(&calls));
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second call rebuilt the pipeline");
        assert!(daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
    }

    #[test]
    fn a_failed_load_leaves_the_daemon_unloaded_and_retryable() {
        // Spec 2026-08-29 §4: lazily, a load failure is not fatal. It must leave
        // no half-installed state behind, so the next press can simply try again.
        let daemon = fake_daemon(IDLE);
        let mut load = || anyhow::bail!("model paths: no such file");

        let err = ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap_err();

        assert!(err.contains("model paths"), "the real reason must survive: {err}");
        assert!(!daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_none());
        assert_eq!(daemon.state.load(Ordering::SeqCst), IDLE, "must not latch FAILED");
    }

    /// Review finding on this task: `ensure_models_loaded` used to read
    /// `Config::load_from` *before* delegating to `ensure_models_loaded_with`,
    /// so that fallible read ran even when the daemon was already warm and
    /// `ensure_models_loaded_with`'s `models_loaded` fast path was about to
    /// return `Ok(())` unconditionally. A `config.toml` broken by a hand-edit
    /// (invariant 4: `deny_unknown_fields` makes even one unknown key a hard
    /// parse failure) turned every later press into a dropped, untranscribed
    /// recording -- on a daemon whose pipeline was already resident and would
    /// have transcribed it fine. The read now lives inside `load`'s closure,
    /// so it only ever runs on the path that actually loads.
    #[test]
    fn an_already_warm_daemon_ignores_a_config_that_no_longer_parses() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-daemon-cfg-broken-warm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // `deny_unknown_fields` (invariant 4) makes this a hard parse
        // failure, the same failure mode a hand-edit typo produces.
        std::fs::write(&path, "not_a_real_key = true\n").unwrap();
        let daemon = fake_daemon_at(IDLE, false, path);
        daemon.models_loaded.store(true, Ordering::SeqCst);

        let result = ensure_models_loaded(&daemon);

        assert!(
            result.is_ok(),
            "an already-warm daemon must not care that config.toml stopped parsing: {result:?}"
        );
    }

    #[test]
    fn status_reports_model_residency_rather_than_startup_completion() {
        let daemon = fake_daemon(IDLE);

        let before = dispatch(&daemon, Request::Status);
        assert_eq!(before.warm, Some(false), "idle with no models is not warm");

        let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();

        let after = dispatch(&daemon, Request::Status);
        assert_eq!(after.warm, Some(true));
    }

    /// `process_utterance` holds `pipeline`'s mutex for the whole `process`
    /// call, so a `Status` handler that asked `pipeline.is_some()` would hang
    /// for the length of a transcription. `models_loaded` is read instead.
    ///
    /// The call under test runs on its own thread and is awaited with a
    /// timeout: a regression here is a deadlock, and an inline call would hang
    /// the suite forever rather than fail this test (`std::sync::Mutex` is not
    /// reentrant, so a same-thread call that regressed to locking `pipeline`
    /// would deadlock, not merely run slow -- see this test's own history for
    /// why it is not written that way).
    #[test]
    fn status_answers_promptly_while_an_utterance_holds_the_pipeline_lock() {
        let daemon = fake_daemon(TRANSCRIBING);
        let held = lock_ignoring_poison(&daemon.pipeline);

        let d = Arc::clone(&daemon);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(dispatch(&d, Request::Status).ok);
        });

        let answered = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        assert_eq!(answered, Ok(true), "Status blocked on the pipeline lock");
    }

    #[test]
    fn an_unloaded_pipeline_is_not_a_reason_to_refuse_a_reload() {
        // Before lazy loading this arm was documented as unreachable. It is now
        // the ordinary state of an idle daemon between dictations, and the next
        // `load_models` reads the file anyway.
        // `scratch_config` is this module's existing helper; `tempfile` is not a
        // dependency of this crate.
        let path = scratch_config("reload-unloaded");
        let daemon = fake_daemon_at(IDLE, false, path);
        assert!(lock_ignoring_poison(&daemon.pipeline).is_none());

        let r = dispatch(&daemon, Request::Reload);

        assert!(r.ok, "reload refused with no pipeline: {:?}", r.err);
    }

    /// Spec §3's per-call-site table, at the one site where getting it wrong
    /// would be silent data loss. `server::start` quarantines a broken config
    /// because it has no way to report one and no user to report it to yet.
    /// `Reload` has both: the user asked, and there is a socket to answer on.
    /// A `Reload` that quietly reset their settings and said "ok" would be the
    /// worst possible reading of "never fail".
    #[test]
    fn reload_reports_a_broken_config_instead_of_quarantining_it() {
        let path = scratch_config("reload-broken");
        std::fs::write(&path, "[audio]\nthis_key_does_not_exist = 1\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        let r = dispatch(&daemon, Request::Reload);

        assert!(!r.ok, "a broken config must be reported, not swallowed");
        assert!(r.err.unwrap().contains("config error"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the file must be exactly as the user left it"
        );
        let strays: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".broken-"))
            .collect();
        assert!(strays.is_empty(), "Reload must not quarantine: {strays:?}");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Spec §3.1: the settings window is the only surface that can tell a user
    /// their settings were lost, so the notice has to survive the trip to it.
    /// `Status` carries it too, and neither is gated on `FAILED` -- a
    /// quarantined config is a *working* daemon, which is precisely why it
    /// would otherwise go unmentioned.
    #[test]
    fn a_quarantine_is_reported_by_both_get_config_and_status() {
        let path = scratch_config("quarantine-report");
        let daemon = fake_daemon_at(IDLE, false, path.clone());
        *lock_ignoring_poison(&daemon.config_notice) = startup_config_notice(
            &config::Migration::NotNeeded,
            Some(&config::Quarantine {
                moved_to: Some(path.with_file_name("config.toml.broken-1")),
                error: "parsing config.toml: unknown field `nope`\n  |\n1 | nope = 1".to_string(),
            }),
        );

        for req in [Request::GetConfig, Request::Status] {
            let r = dispatch(&daemon, req);
            assert!(r.ok, "a quarantine is not a failure: {:?}", r.err);
            let notice = r.config_notice.expect("the notice must reach the GUI");
            assert!(notice.contains("config.toml.broken-1"), "it must name the file: {notice}");
            assert!(notice.contains("nope"), "and say what was wrong: {notice}");
        }

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A migration that could not read the user's old config is the case that
    /// silently reset every setting: the legacy file is correctly left alone,
    /// but the new path does not exist yet, so the quarantine never fires and
    /// nothing else notices. The notice has to come from the migration itself.
    #[test]
    fn an_unreadable_legacy_config_still_produces_a_notice_naming_it() {
        let notice = startup_config_notice(
            &config::Migration::LegacyUnreadable {
                left_at: std::path::PathBuf::from("/home/u/.config/yappr/config.toml"),
                error: "parsing config.toml: unknown field `nope`\n  |\n1 | nope = 1\n  | ^"
                    .to_string(),
            },
            None,
        )
        .expect("an unreadable legacy config must be reported");

        assert!(notice.contains("/home/u/.config/yappr/config.toml"), "{notice}");
        assert!(notice.contains("nicht übernommen"), "{notice}");
        assert!(notice.contains("nope"), "{notice}");
    }

    /// Only the first line of a `toml` diagnostic belongs in a banner: the rest
    /// is a caret gutter that does not line up in a proportional font.
    #[test]
    fn the_notice_carries_one_line_of_the_parser_diagnostic_not_its_gutter() {
        let notice = startup_config_notice(
            &config::Migration::NotNeeded,
            Some(&config::Quarantine {
                moved_to: Some(std::path::PathBuf::from("/tmp/config.toml.broken-1")),
                error: "parsing config.toml: unknown field `nope`\n  |\n1 | nope = 1\n  | ^"
                    .to_string(),
            }),
        )
        .unwrap();

        assert!(notice.contains("unknown field `nope`"), "{notice}");
        assert!(!notice.contains('|'), "the caret gutter must not reach the banner: {notice}");
    }

    /// When even the rename failed the file is still where it always was, so
    /// the sentence must not send the user somewhere else to look for it.
    #[test]
    fn a_quarantine_that_could_not_move_the_file_does_not_claim_it_moved() {
        let notice = startup_config_notice(
            &config::Migration::NotNeeded,
            Some(&config::Quarantine {
                moved_to: None,
                error: "parsing config.toml: bad".to_string(),
            }),
        )
        .unwrap();

        assert!(notice.contains("liegt unverändert"), "{notice}");
        assert!(!notice.contains("verschoben"), "{notice}");
    }

    /// The healthy path stays silent -- a banner on every launch would train
    /// the user to dismiss the one that matters.
    #[test]
    fn a_healthy_start_reports_no_config_notice() {
        let path = scratch_config("no-quarantine");
        let daemon = fake_daemon_at(IDLE, false, path.clone());

        assert!(dispatch(&daemon, Request::GetConfig).config_notice.is_none());
        assert!(dispatch(&daemon, Request::Status).config_notice.is_none());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Final-review fix 3. `SetConfig` mirrors `[models]` into
    /// `daemon.models_cfg`; `Reload` returned `ok(Idle)` on both of its
    /// branches without doing so, and `daemon.models_cfg` is the *only* thing
    /// the housekeeping thread reads the idle timeout from. So a `[models]
    /// idle_unload_seconds` change followed by `yappr --reload` reported
    /// success and changed nothing until the next restart, while the same
    /// change made in the settings GUI applied live -- and README documents
    /// `--reload` as applying every reloadable section.
    ///
    /// Driven with no pipeline installed, which is the ordinary state of a
    /// lazily-loaded idle daemon *and* the branch that had no plausible
    /// excuse for skipping the mirror.
    #[test]
    fn reload_mirrors_the_models_section_the_way_set_config_does() {
        let path = scratch_config("reload-models");
        std::fs::write(&path, "[models]\nidle_unload_seconds = 5\n").unwrap();
        let daemon = fake_daemon_at(IDLE, false, path);
        assert_eq!(
            lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds,
            ModelsConfig::default().idle_unload_seconds,
            "precondition: the daemon still holds the startup value"
        );

        let r = dispatch(&daemon, Request::Reload);

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(
            lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds,
            5,
            "--reload reported success without applying the new idle timeout"
        );
    }

    /// The other branch of the same arm: a daemon with a pipeline resident
    /// must mirror too. Split from the test above rather than looped, because
    /// the two branches sit either side of `update_reloadable` and a fix
    /// applied to only one of them is exactly the shape of this bug.
    #[test]
    fn reload_mirrors_the_models_section_with_a_pipeline_resident_too() {
        let path = scratch_config("reload-models-warm");
        std::fs::write(&path, "[models]\nidle_unload_seconds = 7\n").unwrap();
        let daemon = fake_daemon_at(IDLE, false, path);
        *lock_ignoring_poison(&daemon.pipeline) = Some(stub_pipeline());

        let r = dispatch(&daemon, Request::Reload);

        assert!(r.ok, "{:?}", r.err);
        assert_eq!(lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds, 7);
    }

    /// Pins `touch_activity`'s own body only -- not the ordering in
    /// `start_recording` that calls it before the `RECORDING` store.
    /// `start_recording` needs a real `Recorder`, which needs live audio
    /// hardware to construct (`ensure_recorder` -> `Recorder::new`), so that
    /// ordering cannot be driven through this suite or CI, the same reason
    /// `spawn_safety_valve`'s body was pulled out to a named function so
    /// *that* could be tested directly. The ordering itself is enforced by
    /// reading `start_recording`'s source (`touch_activity(daemon);` is its
    /// first statement, before the `RECORDING` store), not by this test --
    /// hence the name naming only what this test actually covers.
    #[test]
    fn touch_activity_refreshes_the_idle_deadline() {
        let daemon = fake_daemon(IDLE);
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

        touch_activity(&daemon);

        assert!(
            lock_ignoring_poison(&daemon.last_activity).elapsed() < Duration::from_secs(1),
            "the deadline was not refreshed"
        );
    }

    #[test]
    fn an_utterance_ending_refreshes_the_idle_deadline_even_on_a_panic() {
        // `IdleOnExit` already recovers `state` when the utterance thread
        // unwinds; the deadline must ride along, or a panicking utterance leaves
        // a stale deadline and the models are unloaded early.
        let daemon = fake_daemon(TRANSCRIBING);
        let stale = Instant::now() - Duration::from_secs(3600);
        *lock_ignoring_poison(&daemon.last_activity) = stale;

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = IdleOnExit::new(&daemon.state, daemon.broadcaster(), &daemon.last_activity);
            panic!("pipeline blew up");
        }));

        assert!(result.is_err());
        assert_eq!(daemon.state.load(Ordering::SeqCst), IDLE);
        assert!(
            lock_ignoring_poison(&daemon.last_activity).elapsed() < Duration::from_secs(1),
            "the deadline was not refreshed on unwind"
        );
    }

    #[test]
    fn should_unload_waits_for_the_full_idle_timeout() {
        let now = Instant::now();
        let long_ago = now - Duration::from_secs(60);
        let recent = now - Duration::from_secs(59);

        assert!(should_unload(long_ago, now, 60, IDLE, true), "the boundary is inclusive");
        assert!(!should_unload(recent, now, 60, IDLE, true), "unloaded a second early");
    }

    #[test]
    fn should_unload_never_fires_with_zero_seconds_configured() {
        // 0 is the documented "never unload" setting, not "unload immediately".
        let now = Instant::now();
        assert!(!should_unload(now - Duration::from_secs(86_400), now, 0, IDLE, true));
    }

    #[test]
    fn should_unload_never_fires_when_nothing_is_loaded() {
        let now = Instant::now();
        assert!(!should_unload(now - Duration::from_secs(3600), now, 60, IDLE, false));
    }

    #[test]
    fn should_unload_accepts_idle_and_paused_but_no_busy_state() {
        let now = Instant::now();
        let stale = now - Duration::from_secs(3600);

        // Paused counts as idle deliberately: it is reached only by a CAS from
        // IDLE, so there is never an utterance to protect, and a user who paused
        // dictation is the clearest signal the models are not about to be needed.
        for s in [IDLE, PAUSED] {
            assert!(should_unload(stale, now, 60, s, true), "state {s} should unload");
        }
        for s in [WARMING, RECORDING, TRANSCRIBING, NORMALIZING, INJECTING, FAILED] {
            assert!(!should_unload(stale, now, 60, s, true), "state {s} must not unload");
        }
    }

    #[test]
    fn unloading_drops_the_pipeline_and_with_it_the_model() {
        let daemon = fake_daemon(IDLE);
        let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();
        daemon.normalize_available.store(true, Ordering::SeqCst);
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

        assert!(unload_models(&daemon));

        assert!(!daemon.models_loaded.load(Ordering::SeqCst));
        // Dropping the pipeline is what frees the ~480 MB: the engine is the
        // pipeline's normalizer, so there is nothing else left to assert on.
        assert!(lock_ignoring_poison(&daemon.pipeline).is_none());
        assert!(!daemon.normalize_available.load(Ordering::SeqCst));
    }

    #[test]
    fn unloading_declines_when_a_recording_started_after_the_decision() {
        // The race `unload_models`' re-check under `load_lock` exists to close:
        // the housekeeping thread decided to unload, then a press moved the
        // deadline before it got the lock. `last_activity` is set stale
        // *first*, so `touch_activity`'s refresh is load-bearing: `fake_daemon`
        // already starts `last_activity` fresh (`Instant::now()`), so without
        // this the deadline would already have been fresh regardless of
        // whether `touch_activity` ran at all.
        let daemon = fake_daemon(IDLE);
        let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);
        touch_activity(&daemon); // the press lands, moving the deadline

        assert!(!unload_models(&daemon), "unloaded despite a fresh deadline");
        assert!(daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
    }

    /// The only construction that actually distinguishes "re-check the
    /// deadline after taking `load_lock`" from "read it, then take the lock":
    /// a hoisted re-check would read the stale deadline *before* blocking on
    /// the lock and decide `true`, then perform the teardown unconditionally
    /// once the lock is free. Holding `load_lock` on this thread and moving
    /// the deadline while a second thread's `unload_models` call is blocked
    /// waiting for it is what forces the two placements to disagree -- the
    /// seven tests above this one all pass identically under either
    /// placement, which is exactly what this one exists to catch.
    ///
    /// `recv_timeout`, never a bare `recv`/`join`: if the re-check ever
    /// regresses to reading before the lock (or deadlocks some other way),
    /// this must fail, not hang the whole suite -- same discipline as
    /// `status_answers_promptly_while_an_utterance_holds_the_pipeline_lock`.
    #[test]
    fn unload_re_checks_the_deadline_after_taking_the_lock_not_before() {
        let daemon = fake_daemon(IDLE);
        *lock_ignoring_poison(&daemon.pipeline) = Some(stub_pipeline());
        daemon.models_loaded.store(true, Ordering::SeqCst);
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

        let held = lock_ignoring_poison(&daemon.load_lock);

        let d = Arc::clone(&daemon);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(unload_models(&d));
        });

        // Give the spawned thread time to actually run before the press
        // lands: `std::thread::spawn` returning is no guarantee the new
        // thread has been scheduled yet, and without this a hoisted, pre-lock
        // re-check could just as easily run *after* `touch_activity` below by
        // scheduling accident, which would pass this test either way and
        // prove nothing (100ms is the same margin
        // `wait_for_busy_to_clear_then_shutdown_with`'s own test above uses
        // for the identical "let the other thread get going" purpose).
        // Correct code is genuinely blocked on `load_lock` for the whole of
        // this sleep, so it never races; only the hoisted version is timing-
        // sensitive, and this sleep is what pins it deterministically.
        std::thread::sleep(Duration::from_millis(100));

        // The press lands here, while the spawned thread is blocked
        // acquiring `load_lock` -- a hoisted, pre-lock re-check would have
        // already read the stale deadline by now and missed this entirely.
        touch_activity(&daemon);
        drop(held);

        let unloaded = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("unload_models did not return promptly -- deadlock in the re-check?");
        assert!(!unloaded, "unload_models acted on the deadline as it stood before the lock, not after");
        assert!(daemon.models_loaded.load(Ordering::SeqCst));
        assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
    }

    #[test]
    fn unloading_declines_mid_utterance() {
        let daemon = fake_daemon(TRANSCRIBING);
        let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
        ensure_models_loaded_with(&daemon, &mut load, &NOT_SHUTTING_DOWN).unwrap();
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

        assert!(!unload_models(&daemon), "unloaded a pipeline an utterance is using");
        assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
    }

    #[test]
    fn there_is_no_unload_deadline_when_nothing_is_loaded() {
        let daemon = fake_daemon(IDLE);
        assert_eq!(time_until_unload(&daemon), None);
    }

    #[test]
    fn there_is_no_unload_deadline_when_unloading_is_disabled() {
        let daemon = fake_daemon(IDLE);
        daemon.models_loaded.store(true, Ordering::SeqCst);
        lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds = 0;
        assert_eq!(time_until_unload(&daemon), None);
    }

    #[test]
    fn the_unload_deadline_shrinks_as_the_idle_time_passes() {
        let daemon = fake_daemon(IDLE);
        daemon.models_loaded.store(true, Ordering::SeqCst);
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(50);

        let remaining = time_until_unload(&daemon).expect("a deadline");

        // 60 s configured, 50 s elapsed -- about ten left, and never more.
        assert!(remaining <= Duration::from_secs(10), "got {remaining:?}");
        assert!(remaining >= Duration::from_secs(9), "got {remaining:?}");
    }

    #[test]
    fn a_passed_deadline_never_shrinks_the_tick_below_the_floor() {
        // `unload_models` re-reads the state and the deadline under
        // `load_lock`, so it can still decline on a tick this returned
        // `Some` for -- a press landing in between, or a lazy load holding
        // that lock. Without the floor the loop would then spin at a zero
        // timeout until that resolved.
        let daemon = fake_daemon(IDLE);
        daemon.models_loaded.store(true, Ordering::SeqCst);
        *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

        // Against a literal, not just `MIN_UNLOAD_TICK` itself: comparing the
        // result to the same constant that produced it would stay green even
        // if `MIN_UNLOAD_TICK` were changed to `Duration::ZERO`, silently
        // reintroducing the busy-spin this test exists to catch.
        assert_eq!(time_until_unload(&daemon), Some(Duration::from_secs(1)));
        assert!(MIN_UNLOAD_TICK > Duration::ZERO, "the floor must not be zero");
    }

    /// The review finding this replaced the old `TRANSCRIBING` version of the
    /// test above with. `time_until_unload` used to answer
    /// `Some(MIN_UNLOAD_TICK)` for *any* state once the deadline had passed,
    /// and the housekeeping loop takes `wait.min(..)` of that answer
    /// unconditionally -- including on the arm carrying spec 15's
    /// 1 -> 2 -> 4 -> ... -> 30 s `llama-server` restart backoff. With
    /// `idle_unload_seconds = 10` and a `llama-server` that fails fast (a
    /// missing ggml backend, which `wait_healthy` catches via `try_wait` in
    /// milliseconds), a 40 s dictation drew roughly 30 fork/exec respawn
    /// attempts instead of about four, on the thread that shares this
    /// machine with live audio capture.
    ///
    /// Nothing is lost by returning `None`: `IdleOnExit`'s `Drop` refreshes
    /// `last_activity` before it stores `IDLE`, so the deadline is a full
    /// `idle_unload_seconds` away the moment a `Some` becomes possible again.
    #[test]
    fn a_busy_daemon_has_no_unload_deadline_to_wake_early_for() {
        for state in [WARMING, RECORDING, TRANSCRIBING, NORMALIZING, INJECTING, FAILED] {
            let daemon = fake_daemon(state);
            daemon.models_loaded.store(true, Ordering::SeqCst);
            *lock_ignoring_poison(&daemon.last_activity) =
                Instant::now() - Duration::from_secs(3600);

            assert_eq!(
                time_until_unload(&daemon),
                None,
                "state {state} shortened the housekeeping tick despite `unload_models` refusing it"
            );
        }

        // The other half: the two states an unload may actually act on must
        // still shorten the tick, or `idle_unload_seconds` goes back to
        // meaning "somewhere within `HEALTH_POLL_INTERVAL` of itself".
        for state in [IDLE, PAUSED] {
            let daemon = fake_daemon(state);
            daemon.models_loaded.store(true, Ordering::SeqCst);
            *lock_ignoring_poison(&daemon.last_activity) =
                Instant::now() - Duration::from_secs(3600);

            assert_eq!(
                time_until_unload(&daemon),
                Some(MIN_UNLOAD_TICK),
                "state {state} must still wake the housekeeping loop for its deadline"
            );
        }
    }

    /// The second reader `models_loaded` exists for: this runs on the thread
    /// that also reaps dead subscribers, so blocking it behind a
    /// transcription would stall that unrelated work for the whole utterance.
    ///
    /// The calls under test run on their own thread and are awaited with a
    /// timeout, the same shape as
    /// `status_answers_promptly_while_an_utterance_holds_the_pipeline_lock`:
    /// `std::sync::Mutex` is not reentrant, so calling these on the test's own
    /// thread while it holds `pipeline` would self-deadlock on a regression
    /// instead of merely running slow -- hanging the whole suite rather than
    /// failing this one test, and never reaching the timing assertion at all.
    #[test]
    fn the_housekeeping_tick_answers_while_an_utterance_holds_the_pipeline_lock() {
        let daemon = fake_daemon(TRANSCRIBING);
        daemon.models_loaded.store(true, Ordering::SeqCst);
        let held = lock_ignoring_poison(&daemon.pipeline);

        let d = Arc::clone(&daemon);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = time_until_unload(&d);
            let _ = tx.send(());
        });

        let answered = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        assert_eq!(answered, Ok(()), "the tick blocked on the pipeline lock");
    }

}
