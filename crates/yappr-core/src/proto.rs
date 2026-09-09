use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crate::paths;

/// Not `Copy` any more: `SetConfig` carries the whole config as JSON. Every
/// existing variant's wire form is unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    PttStart,
    PttStop,
    Cancel,
    Status,
    Reload,
    /// Press-to-start / press-to-stop. Resolved against the server's current
    /// state, never against a client-side memory of the last press -- the
    /// client is a fresh process every time and has no memory to consult.
    Toggle,
    /// Shut the whole app down. The tray's Beenden sends the same request.
    Quit,
    /// Shut the whole app down and start it again. Identical to
    /// [`Request::Quit`] up to the last instruction -- the same `quitting`
    /// latch, the same wait for an in-flight utterance, the same `shutdown`
    /// -- with the successor spawned in between `shutdown` and the exit.
    ///
    /// The ordering is the whole point and is not interchangeable with
    /// Tauri's own `AppHandle::restart`, which spawns the successor *before*
    /// the parent has released anything: `shutdown` is what removes
    /// `yappr.lock`, so a successor started ahead of it finds the exclusive
    /// `flock` still held, prints "yappr is already running" and exits 1 --
    /// and then the parent exits too, leaving nothing running at all. See
    /// `server::dispatch`'s arm and `EventSink::relaunch`.
    Restart,
    /// Show and focus the settings window.
    ShowSettings,
    /// Show the settings window with the first-run wizard on top of it
    /// (`yappr --wizard`, and the tray's Einrichtung item). Distinct from
    /// [`Request::ShowSettings`] because the two land the user in different
    /// places -- one in the settings form, one at the top of the setup flow.
    ShowWizard,
    /// Turns this connection into a long-lived `OverlayEvent` stream (spec
    /// 12) instead of the usual one-request-one-response exchange: after
    /// this line, the daemon writes one NDJSON `OverlayEvent` per line,
    /// starting with a snapshot of whatever state it's in right now, for as
    /// long as the connection stays open. There is no `Response` for this
    /// request -- see `owf-daemon.rs`'s `handle`, which special-cases it
    /// before ever reaching `dispatch`.
    Subscribe,
    /// The whole `Config` as JSON, for the settings GUI. Sent as JSON rather
    /// than TOML so the GUI never needs the Rust type -- there is no fourth
    /// hand-maintained copy of the config schema.
    GetConfig,
    /// Applies `config` as a *patch* to the config on disk -- it is routinely
    /// partial, and `wizard_finish` sends a single leaf -- then rewrites
    /// `config.toml` from the merged `Config` and applies whatever can take
    /// effect without a restart. Rejected outright if the result would not
    /// load, with the file untouched.
    SetConfig { config: serde_json::Value },
    /// The input devices `cpal` can see, for the device dropdown.
    ListInputDevices,
    /// Tray-driven. Pausing leaves the shortcut bound and the app running; it
    /// only makes `Toggle` refuse, so no microphone is opened.
    SetPaused { paused: bool },
}

/// One row of the settings GUI's microphone dropdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDevice {
    pub name: String,
    /// Whether this is the host's default input.
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Warming,
    Idle,
    Recording,
    Transcribing,
    Normalizing,
    Injecting,
    /// A fatal, unrecoverable warm-up failure (ASR/VAD failed to load) --
    /// distinct from the transient `OverlayEvent::Error` flash. This is a
    /// steady state a late-connecting subscriber's snapshot must be able to
    /// report instead of a permanent `Warming` spinner (spec 12; see
    /// `owf-daemon.rs`'s `FAILED` and `serve_subscriber`). Additive: every
    /// existing variant's wire form is unchanged.
    Error,
    /// Task 13: `Request::SetPaused { paused: true }`, accepted only from
    /// `Idle` -- see `owf-daemon.rs`'s `SetPaused` handler, a
    /// compare-and-exchange rather than a store, so pausing can never seize
    /// a state an utterance is already using. `Toggle`/`PttStart` refuse in
    /// this state exactly as they do in `Warming`/`Error`, opening no
    /// microphone, until `Request::SetPaused { paused: false }` returns the
    /// daemon to `Idle`.
    Paused,
}

/// One line of the NDJSON stream a `Request::Subscribe` connection turns
/// into (spec 12): every state the overlay needs to render, plus the two
/// terminal outcomes (`Done`, `Error`) and the busy-rejection flash that
/// aren't `State` transitions at all. `owf-daemon.rs` is the sole producer;
/// the overlay (and `owf-ctl subscribe`) are the consumers.
///
/// `#[serde(tag = "event", ...)]` makes each variant a self-describing JSON
/// object -- e.g. `{"event":"recording","level":0.02,"elapsed_ms":140}` --
/// so a bare `serde_json::to_string` plus a trailing newline is already
/// valid NDJSON with no wrapping needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OverlayEvent {
    Warming,
    Idle,
    /// Task 13: mirrors `State::Paused` -- the daemon is paused via the
    /// tray's "Diktat pausieren" and will accept no `ptt-start`/`toggle`
    /// until it is unchecked. Sent as the initial snapshot to a subscriber
    /// connecting while paused (`snapshot_event`), and broadcast the moment
    /// `Request::SetPaused` actually takes effect.
    Paused,
    /// `ptt-start` has been accepted but the microphone is not delivering
    /// samples yet: `ensure_recorder` may still have to build the recorder,
    /// and even after `Recorder::start` returns, ALSA/PipeWire hands over the
    /// first buffer tens of milliseconds later (measured ~55 ms through
    /// PipeWire on this hardware). Speech in that window is genuinely lost,
    /// so the overlay is expected to render this as "wait" rather than live
    /// bars over a microphone that is not capturing yet.
    ///
    /// The very next `Recording` event marks the moment real audio arrived --
    /// the first audio callback is never throttled (`should_emit_level`'s
    /// `last_emitted` starts `None`), so it is emitted as soon as capture is
    /// genuinely live, not up to `LEVEL_EMIT_INTERVAL` afterwards.
    Opening,
    /// Spec 7.1: emitted at roughly 50 ms cadence while recording, not per
    /// audio callback. `level` is the RMS of the most recent capture window;
    /// `elapsed_ms` is time since this recording started.
    Recording { level: f32, elapsed_ms: u64 },
    Transcribing,
    Normalizing,
    Injecting,
    /// The first ~60 characters of what was actually injected (spec 12's
    /// 800 ms preview flash). Truncation to that length is the overlay's
    /// job, not the wire format's.
    Done { preview: String },
    Error { reason: String },
    BusyRejected,
    /// Spec 15: normalization has stopped being available -- the supervised
    /// `llama-server` died, was wedged, or never came up. This is not a
    /// `State` transition: the pipeline itself is unaffected (raw text plus
    /// the rule-based fallback keeps working per spec 15's error matrix), so
    /// the overlay is expected to render this as a persistent badge
    /// alongside whatever `State`-driven view is already showing, not
    /// replace it. `reason` is a short, log-line-style explanation. See
    /// `owf-daemon.rs`'s `spawn_housekeeping`/`supervise_llama_once`.
    NormalizeDegraded { reason: String },
    /// Emitted once normalization becomes available again after a
    /// `NormalizeDegraded`.
    NormalizeRecovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<serde_json::Value>,
    /// Whether normalization is currently available -- populated only by the
    /// `Status` handler (mirroring how `warm` already works), and only when
    /// `[normalize].enabled = true`: `None` there means "not applicable"
    /// (normalization was never turned on), not "unknown". This is Task 3's
    /// "status must stop lying": before it existed, `status` kept reporting
    /// a daemon as fully fine while its supervised `llama-server` was dead,
    /// with no field anywhere reflecting the gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalize_available: Option<bool>,
    /// `GetConfig` only: the whole config as JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// `GetConfig` only: where that config lives. The settings window used to
    /// print it at the foot of the sidebar; that line is the running version
    /// now, so nothing in this tree renders it any more. It stays because it
    /// is part of `GetConfig`'s answer on the socket and is the key any other
    /// client reads the path by -- dropping it would be a wire change, not a
    /// GUI one. Pinned by `settings_cmds`'s
    /// `an_ok_response_becomes_ok_json_with_its_fields_intact` and `server`'s
    /// `get_config_returns_the_whole_config_and_the_path_it_came_from`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// `GetConfig` and `Status`: set when startup found a `config.toml` it
    /// could not load, moved it aside and came up on defaults instead (spec
    /// §3). German, because the settings window renders it verbatim in the
    /// notice banner it already has. `None` on every healthy run.
    ///
    /// Deliberately not `err`, and deliberately not `Daemon::fatal_error`: a
    /// quarantine is not a failure. The daemon is running and dictation works
    /// -- the user has simply lost their settings and needs telling where the
    /// old file went.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_notice: Option<String>,
    /// `GetConfig` only: `Config::default()` as JSON, shaped exactly like
    /// `config`. The settings GUI's per-row reset button restores a field to
    /// the value found here, and hides itself on a field that already matches.
    /// It travels on the wire for the same reason `config` does: the GUI has
    /// no copy of the config schema, so a defaults table maintained there
    /// would be a second one, free to drift from the Rust it claims to
    /// describe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defaults: Option<serde_json::Value>,
    /// `ListInputDevices` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<InputDevice>>,
    /// `SetConfig` only: whether the change needs a daemon restart to take
    /// effect. Computed by the daemon, never by the GUI -- the rule already
    /// exists in `Pipeline::update_reloadable` and must not exist twice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_required: Option<bool>,
    /// `SetConfig` only: which settings need it, in words the GUI can show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_reason: Option<String>,
}

impl Response {
    fn blank(ok: bool) -> Self {
        Self {
            ok,
            state: None,
            err: None,
            warm: None,
            last_ms: None,
            normalize_available: None,
            config: None,
            config_path: None,
            config_notice: None,
            defaults: None,
            devices: None,
            restart_required: None,
            restart_reason: None,
        }
    }

    pub fn ok(state: State) -> Self {
        Self { ok: true, state: Some(state), ..Self::blank(true) }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { err: Some(msg.into()), ..Self::blank(false) }
    }
}

/// Client side: one connection, one request line, one response line.
pub fn send(req: &Request) -> Result<Response> {
    let sock = paths::runtime_socket();
    let stream = UnixStream::connect(&sock).with_context(|| {
        format!("cannot reach the daemon at {} — is yappr running?", sock.display())
    })?;
    let mut w = stream.try_clone()?;
    writeln!(w, "{}", serde_json::to_string(req)?)?;
    w.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_to_the_documented_wire_form() {
        assert_eq!(
            serde_json::to_string(&Request::PttStart).unwrap(),
            r#"{"cmd":"ptt-start"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::PttStop).unwrap(),
            r#"{"cmd":"ptt-stop"}"#
        );
        assert_eq!(serde_json::to_string(&Request::Cancel).unwrap(), r#"{"cmd":"cancel"}"#);
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        assert_eq!(serde_json::to_string(&Request::Reload).unwrap(), r#"{"cmd":"reload"}"#);
        assert_eq!(serde_json::to_string(&Request::Toggle).unwrap(), r#"{"cmd":"toggle"}"#);
        assert_eq!(serde_json::to_string(&Request::Quit).unwrap(), r#"{"cmd":"quit"}"#);
        assert_eq!(serde_json::to_string(&Request::Restart).unwrap(), r#"{"cmd":"restart"}"#);
        assert_eq!(
            serde_json::to_string(&Request::ShowSettings).unwrap(),
            r#"{"cmd":"show-settings"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ShowWizard).unwrap(),
            r#"{"cmd":"show-wizard"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Subscribe).unwrap(),
            r#"{"cmd":"subscribe"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::GetConfig).unwrap(),
            r#"{"cmd":"get-config"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ListInputDevices).unwrap(),
            r#"{"cmd":"list-input-devices"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetPaused { paused: true }).unwrap(),
            r#"{"cmd":"set-paused","paused":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetConfig {
                config: serde_json::json!({"audio": {"device": "default"}})
            })
            .unwrap(),
            r#"{"cmd":"set-config","config":{"audio":{"device":"default"}}}"#
        );
    }

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::PttStart,
            Request::PttStop,
            Request::Cancel,
            Request::Status,
            Request::Reload,
            Request::Toggle,
            Request::Quit,
            Request::ShowSettings,
            Request::ShowWizard,
            Request::Subscribe,
            Request::GetConfig,
            Request::ListInputDevices,
            Request::SetConfig { config: serde_json::json!({"asr": {"num_threads": 2}}) },
            Request::SetPaused { paused: true },
            Request::SetPaused { paused: false },
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn an_unknown_command_fails_to_parse() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"launch-missiles"}"#).is_err());
    }

    /// A malformed or unknown line must not panic the parser -- both
    /// `src-tauri/src/connection.rs` and `replay.rs`'s `load()` rely on this
    /// to drop an unrecognised event line and keep listening, rather than
    /// crashing the overlay. Restored after `src-tauri/src/wire.rs` (whose
    /// own copy of this assertion, `unknown_event_tag_fails_to_parse_rather_than_panicking`,
    /// was deleted along with the file) was found to have no equivalent left
    /// anywhere in the workspace.
    #[test]
    fn an_unknown_overlay_event_tag_fails_to_parse() {
        assert!(serde_json::from_str::<OverlayEvent>(r#"{"event":"levitating"}"#).is_err());
    }

    #[test]
    fn states_serialise_lowercase() {
        assert_eq!(serde_json::to_string(&State::Warming).unwrap(), r#""warming""#);
        assert_eq!(serde_json::to_string(&State::Recording).unwrap(), r#""recording""#);
        assert_eq!(serde_json::to_string(&State::Transcribing).unwrap(), r#""transcribing""#);
        assert_eq!(serde_json::to_string(&State::Normalizing).unwrap(), r#""normalizing""#);
        assert_eq!(serde_json::to_string(&State::Injecting).unwrap(), r#""injecting""#);
        assert_eq!(serde_json::to_string(&State::Idle).unwrap(), r#""idle""#);
        assert_eq!(serde_json::to_string(&State::Error).unwrap(), r#""error""#);
        assert_eq!(serde_json::to_string(&State::Paused).unwrap(), r#""paused""#);
    }

    #[test]
    fn an_error_response_carries_ok_false_and_a_reason() {
        let r = Response::err("busy");
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["err"], serde_json::json!("busy"));
    }

    #[test]
    fn an_ok_response_carries_the_state() {
        let r = Response::ok(State::Recording);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["state"], serde_json::json!("recording"));
    }

    /// `defaults` is what the settings GUI's per-row reset button restores to.
    /// It is absent from every response that is not a `GetConfig` reply, the
    /// same contract `config` and `config_path` already keep -- a GUI that
    /// sees it on a `Status` reply would be reading a field nobody populates.
    #[test]
    fn a_response_omits_defaults_unless_something_puts_them_there() {
        let v: serde_json::Value = serde_json::to_value(Response::ok(State::Idle)).unwrap();
        assert!(v.get("defaults").is_none(), "defaults leaked into a plain ok response");

        let mut r = Response::ok(State::Idle);
        r.defaults = Some(serde_json::json!({"audio": {"device": "default"}}));
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["defaults"]["audio"]["device"], serde_json::json!("default"));
    }

    /// Every `OverlayEvent` variant round-trips through serde -- the overlay
    /// frontend and `owf-ctl subscribe` are both written against this exact
    /// wire form, so a variant that fails to round-trip here would silently
    /// break both.
    #[test]
    fn every_overlay_event_round_trips_through_json() {
        let events = [
            OverlayEvent::Warming,
            OverlayEvent::Idle,
            OverlayEvent::Paused,
            OverlayEvent::Recording { level: 0.42, elapsed_ms: 1_234 },
            OverlayEvent::Transcribing,
            OverlayEvent::Normalizing,
            OverlayEvent::Injecting,
            OverlayEvent::Done { preview: "Hello there".to_string() },
            OverlayEvent::Error { reason: "no speech detected".to_string() },
            OverlayEvent::BusyRejected,
            OverlayEvent::NormalizeDegraded { reason: "llama-server is down".to_string() },
            OverlayEvent::NormalizeRecovered,
        ];
        for event in events {
            let s = serde_json::to_string(&event).unwrap();
            assert_eq!(
                serde_json::from_str::<OverlayEvent>(&s).unwrap(),
                event,
                "round trip failed for {s}"
            );
        }
    }

    /// Pins the exact wire form each variant produces -- the overlay
    /// frontend (a separate, not-yet-written codebase) will be implemented
    /// against this shape, so an accidental rename here must fail loudly.
    #[test]
    fn overlay_events_serialise_to_the_documented_wire_form() {
        assert_eq!(serde_json::to_string(&OverlayEvent::Warming).unwrap(), r#"{"event":"warming"}"#);
        assert_eq!(serde_json::to_string(&OverlayEvent::Idle).unwrap(), r#"{"event":"idle"}"#);
        assert_eq!(serde_json::to_string(&OverlayEvent::Paused).unwrap(), r#"{"event":"paused"}"#);
        assert_eq!(serde_json::to_string(&OverlayEvent::Opening).unwrap(), r#"{"event":"opening"}"#);
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Recording { level: 0.5, elapsed_ms: 100 }).unwrap(),
            r#"{"event":"recording","level":0.5,"elapsed_ms":100}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Transcribing).unwrap(),
            r#"{"event":"transcribing"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Normalizing).unwrap(),
            r#"{"event":"normalizing"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Injecting).unwrap(),
            r#"{"event":"injecting"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Done { preview: "hi".to_string() }).unwrap(),
            r#"{"event":"done","preview":"hi"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::Error { reason: "boom".to_string() }).unwrap(),
            r#"{"event":"error","reason":"boom"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::BusyRejected).unwrap(),
            r#"{"event":"busy_rejected"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::NormalizeDegraded { reason: "boom".to_string() })
                .unwrap(),
            r#"{"event":"normalize_degraded","reason":"boom"}"#
        );
        assert_eq!(
            serde_json::to_string(&OverlayEvent::NormalizeRecovered).unwrap(),
            r#"{"event":"normalize_recovered"}"#
        );
    }

    /// Task 3: `status` must stop lying about normalization availability --
    /// the field is additive (`skip_serializing_if`) so an old client that
    /// never looks for it is unaffected, and it must not appear at all
    /// unless something actually populates it.
    #[test]
    fn normalize_available_is_absent_by_default_and_present_when_set() {
        let mut r = Response::ok(State::Idle);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert!(v.get("normalize_available").is_none());

        r.normalize_available = Some(false);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["normalize_available"], serde_json::json!(false));
    }

    /// `src-tauri/src/wire.rs` deliberately hand-duplicates this enum's wire
    /// format rather than depending on `yappr-core` (see that module's doc
    /// comment for why), which means the two can silently drift: a variant
    /// added or reshaped here with no matching change there would only be
    /// caught by someone remembering to update `wire.rs`'s own hand-written
    /// pinned-string test by hand. This turns that manual promise into a
    /// mechanical one from this side: every line of the checked-in overlay
    /// replay fixture -- itself asserted elsewhere
    /// (`src-tauri/src/replay.rs`'s `checked_in_fixture_covers_every_event_kind`)
    /// to cover every `wire::OverlayEvent` variant -- must also parse as
    /// this crate's own `OverlayEvent`. If the two wire formats ever
    /// disagree, one of these two tests fails.
    #[test]
    fn the_overlay_replay_fixture_parses_as_this_crates_overlay_event() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src-tauri/fixtures/replay-full.ndjson");
        let contents = std::fs::read_to_string(&fixture)
            .unwrap_or_else(|e| panic!("reading {}: {e}", fixture.display()));

        let mut events = Vec::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let event: OverlayEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("parsing fixture line {line:?}: {e}"));
            events.push(event);
        }

        assert!(!events.is_empty(), "fixture must contain events");
        for variant in [
            "warming",
            "idle",
            "paused",
            "opening",
            "transcribing",
            "normalizing",
            "injecting",
            "done",
            "error",
            "busy_rejected",
            "normalize_degraded",
            "normalize_recovered",
        ] {
            assert!(
                events.iter().any(|e| serde_json::to_value(e).unwrap()["event"] == variant),
                "fixture is missing a {variant} line"
            );
        }
        assert!(events.iter().any(|e| matches!(e, OverlayEvent::Recording { .. })));
    }
}
