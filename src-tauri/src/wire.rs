//! Minimal mirror of `owf_core::proto::OverlayEvent`'s wire format --
//! see `crates/owf-core/src/proto.rs`, the source of truth.
//!
//! This is a deliberate duplication, not an oversight. `owf-core` links
//! `sherpa-onnx`, `cpal`, and `rubato` for the model/audio stack; depending
//! on it from this crate would drag all of that into the overlay's build
//! graph for the sake of one small enum. `owf-ctl` avoids the same crates
//! for the same reason (see its module doc: "it links none of the model
//! crates"). The overlay is a socket client exactly like `owf-ctl`, so it
//! gets the same treatment.
//!
//! The risk of duplication is silent drift between this copy and the
//! daemon's wire format. The pinned-string test below is the guard: it
//! asserts byte-for-byte the same JSON that
//! `owf-core::proto::overlay_events_serialise_to_the_documented_wire_form`
//! asserts on the daemon side. If one changes without the other, both
//! tests still pass individually but the two processes would disagree on
//! the wire -- so this comment is the actual guard; keep the two test
//! functions in sync by hand when `proto.rs` changes.

use serde::{Deserialize, Serialize};

/// Every state the overlay renders, plus the two terminal outcomes and the
/// busy-rejection flash (spec 12). Field shapes and the `snake_case` /
/// `tag = "event"` wire form must match `owf_core::proto::OverlayEvent`
/// exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OverlayEvent {
    Warming,
    Idle,
    /// `level` is the RMS of the most recent ~50ms capture window;
    /// `elapsed_ms` is time since this recording started.
    Recording { level: f32, elapsed_ms: u64 },
    Transcribing,
    Normalizing,
    Injecting,
    /// The first ~60 characters of what was actually injected. Truncation
    /// to that length is the overlay's job (done in the frontend), not the
    /// wire format's -- matching the comment on the daemon-side type.
    Done { preview: String },
    Error { reason: String },
    BusyRejected,
}

/// The exact NDJSON line `owf_core::proto::Request::Subscribe` serialises
/// to. Sent verbatim as a constant rather than round-tripped through a
/// local `Request` enum, since the overlay never sends any other command --
/// unlike `owf-ctl`, it has no need for `ptt-start`/`ptt-stop`/etc.
pub const SUBSCRIBE_LINE: &str = r#"{"cmd":"subscribe"}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_events_round_trip() {
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

    /// Pins this mirror to the exact strings asserted in
    /// `crates/owf-core/src/proto.rs`'s
    /// `overlay_events_serialise_to_the_documented_wire_form` test. Keep
    /// these two lists identical by hand.
    #[test]
    fn wire_form_matches_owf_core_proto() {
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

    #[test]
    fn subscribe_line_matches_owf_core_proto_request_wire_form() {
        assert_eq!(SUBSCRIBE_LINE, r#"{"cmd":"subscribe"}"#);
    }

    /// A malformed or unknown line must not panic the parser -- the overlay
    /// treats it as "drop and keep listening" (see `connection.rs` and
    /// `replay.rs`), never as a crash.
    #[test]
    fn unknown_event_tag_fails_to_parse_rather_than_panicking() {
        assert!(serde_json::from_str::<OverlayEvent>(r#"{"event":"levitating"}"#).is_err());
    }
}
