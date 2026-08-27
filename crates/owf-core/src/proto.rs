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
    }

    #[test]
    fn requests_round_trip() {
        for r in [Request::PttStart, Request::PttStop, Request::Cancel, Request::Status, Request::Reload] {
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
}
