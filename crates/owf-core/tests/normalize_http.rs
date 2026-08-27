use httpmock::prelude::*;
use owf_core::normalize::{Normalizer, S1MiniClient, SYSTEM_PROMPT};

fn ok_body(content: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [ { "message": { "role": "assistant", "content": content } } ]
    })
}

#[test]
fn sends_the_verbatim_system_prompt_and_required_flags() {
    let server = MockServer::start();
    let m = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .json_body_includes(
                serde_json::json!({
                    "temperature": 0,
                    "top_k": 1,
                    "stream": false,
                    "chat_template_kwargs": { "enable_thinking": false }
                })
                .to_string(),
            );
        then.status(200).json_body(ok_body("Hello there."));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    let out = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hello there")
        .unwrap();

    assert_eq!(out, "Hello there.");
    m.assert();
}

#[test]
fn the_user_turn_is_the_control_line_then_a_newline_then_the_transcript() {
    let server = MockServer::start();
    let control = "[Styling: formal] [Structure: lists] [Context: email]";
    let m = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes(SYSTEM_PROMPT)
            .body_includes(format!("{control}\\nhello there"));
        then.status(200).json_body(ok_body("Hello there."));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    c.normalize(control, "hello there").unwrap();
    m.assert();
}

#[test]
fn a_500_is_an_error_not_a_panic() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(500).body("upstream exploded");
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_malformed_body_is_an_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).body("{ not json");
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_response_with_no_choices_is_an_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(serde_json::json!({ "choices": [] }));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_slow_server_times_out_within_the_configured_budget() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .delay(std::time::Duration::from_millis(1_500))
            .json_body(ok_body("too late"));
    });

    let c = S1MiniClient::new(server.base_url(), 300);
    let t0 = std::time::Instant::now();
    let err = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there")
        .unwrap_err();
    let elapsed = t0.elapsed();

    assert!(err.to_string().to_lowercase().contains("timeout"), "got: {err}");
    assert!(elapsed < std::time::Duration::from_millis(1_200), "took {elapsed:?}");
}

/// I2: a `llama-server` that accepts the connection and then never replies
/// must not leak the worker thread `normalize()` races against
/// `recv_timeout`. Before this fix, `ureq` was given no timeout of its own
/// (`ureq::Timeouts::default()` is all `None`), so the thread -- and its
/// socket -- would still be blocked in I/O well after this function's
/// `recv_timeout` gave up on waiting for it.
///
/// Live thread count is Linux-specific (`/proc/self/status`), which matches
/// the rest of this project's scope (Hyprland on Linux only).
#[test]
fn a_permanently_silent_server_does_not_leak_the_worker_thread() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .delay(std::time::Duration::from_secs(5))
            .json_body(ok_body("too late"));
    });

    // ureq's own timeout is roughly double the caller's budget, so it fires
    // at ~300ms here -- well before the mock's 5 s delay, but comfortably
    // after this call's own 150ms `recv_timeout` has already returned.
    let c = S1MiniClient::new(server.base_url(), 150);

    let threads_before = live_thread_count();
    let err = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there")
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("timeout"), "got: {err}");

    // Give the worker thread comfortable headroom past ureq's own timeout to
    // actually unblock and exit.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let threads_after = live_thread_count();

    assert!(
        threads_after <= threads_before,
        "a worker thread is still alive well past ureq's own timeout: before={threads_before}, after={threads_after}"
    );
}

fn live_thread_count() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

#[test]
fn output_is_trimmed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(ok_body("  Hello there.\n\n"));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    let out = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hello there")
        .unwrap();
    assert_eq!(out, "Hello there.");
}
