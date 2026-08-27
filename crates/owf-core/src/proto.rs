use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    PttStart,
    PttStop,
    Cancel,
    Status,
    Reload,
    /// Turns this connection into a long-lived `OverlayEvent` stream (spec
    /// 12) instead of the usual one-request-one-response exchange: after
    /// this line, the daemon writes one NDJSON `OverlayEvent` per line,
    /// starting with a snapshot of whatever state it's in right now, for as
    /// long as the connection stays open. There is no `Response` for this
    /// request -- see `owf-daemon.rs`'s `handle`, which special-cases it
    /// before ever reaching `dispatch`.
    Subscribe,
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
}

impl Response {
    pub fn ok(state: State) -> Self {
        Self { ok: true, state: Some(state), err: None, warm: None, last_ms: None }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, state: None, err: Some(msg.into()), warm: None, last_ms: None }
    }
}

/// Client side: one connection, one request line, one response line.
pub fn send(req: &Request) -> Result<Response> {
    let sock = paths::runtime_socket();
    let stream = UnixStream::connect(&sock).with_context(|| {
        format!("cannot reach the daemon at {} — is owf-daemon running?", sock.display())
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
        assert_eq!(
            serde_json::to_string(&Request::Subscribe).unwrap(),
            r#"{"cmd":"subscribe"}"#
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
            Request::Subscribe,
        ] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn an_unknown_command_fails_to_parse() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"launch-missiles"}"#).is_err());
    }

    #[test]
    fn states_serialise_lowercase() {
        assert_eq!(serde_json::to_string(&State::Warming).unwrap(), r#""warming""#);
        assert_eq!(serde_json::to_string(&State::Recording).unwrap(), r#""recording""#);
        assert_eq!(serde_json::to_string(&State::Transcribing).unwrap(), r#""transcribing""#);
        assert_eq!(serde_json::to_string(&State::Normalizing).unwrap(), r#""normalizing""#);
        assert_eq!(serde_json::to_string(&State::Injecting).unwrap(), r#""injecting""#);
        assert_eq!(serde_json::to_string(&State::Idle).unwrap(), r#""idle""#);
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

    /// Every `OverlayEvent` variant round-trips through serde -- the overlay
    /// frontend and `owf-ctl subscribe` are both written against this exact
    /// wire form, so a variant that fails to round-trip here would silently
    /// break both.
    #[test]
    fn every_overlay_event_round_trips_through_json() {
        let events = [
            OverlayEvent::Warming,
            OverlayEvent::Idle,
            OverlayEvent::Recording { level: 0.42, elapsed_ms: 1_234 },
            OverlayEvent::Transcribing,
            OverlayEvent::Normalizing,
            OverlayEvent::Injecting,
            OverlayEvent::Done { preview: "Hello there".to_string() },
            OverlayEvent::Error { reason: "no speech detected".to_string() },
            OverlayEvent::BusyRejected,
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
    }
}
