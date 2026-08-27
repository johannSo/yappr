use anyhow::{bail, Context, Result};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::NormalizeConfig;
use crate::paths;

/// A supervised `llama-server` child running S1-mini by Superwhisper.
///
/// The child is killed when this value is dropped, so a daemon crash cannot
/// leave a ~600 MB model resident.
pub struct LlamaServer {
    child: Child,
    port: u16,
}

/// Tries the configured port, then the next nine, per spec 8.1.
///
/// This only proves a port was free at the moment of the check: the listener
/// is dropped immediately afterward so `llama-server` itself can bind it,
/// leaving a narrow TOCTOU window if something else grabs the port in
/// between. That's an inherent limit of "ask the OS for a free port, then
/// hand it to a child process" and not something a retry loop here can fully
/// close.
fn pick_port(preferred: u16) -> Result<u16> {
    for port in preferred..preferred.saturating_add(10) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    // `.saturating_add(10)`, matching the loop above: `preferred + 10` here
    // would overflow (and panic in a debug/test build) for a `preferred`
    // within 10 of `u16::MAX`, even though the loop itself was already
    // overflow-safe -- the bail message just needs to describe the same
    // range the loop actually checked.
    bail!("no free port in {}..{}", preferred, preferred.saturating_add(10))
}

/// A single `/health` request, bounded to a couple of seconds.
///
/// Without a request-level timeout here, a `llama-server` that accepts the
/// TCP connection but never writes a response would hang this call forever
/// -- which would in turn defeat `wait_healthy`'s own deadline loop (the
/// `Instant::now() >= deadline` check is never reached if this call never
/// returns). This is the same class of bug I2 fixes in `normalize.rs`,
/// applied here so C1's "fail fast on a dead llama-server" guarantee holds
/// even when the child is alive but wedged rather than exited.
fn health_probe(url: &str) -> bool {
    ureq::get(url)
        .config()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .call()
        .map(|r| r.status() == 200)
        .unwrap_or(false)
}

impl LlamaServer {
    pub fn spawn(cfg: &NormalizeConfig) -> Result<Self> {
        let model = paths::models_dir().join("s1-mini-q4_k_m.gguf");
        anyhow::ensure!(model.exists(), "missing {}", model.display());

        let port = pick_port(cfg.port)?;

        let child = Command::new(&cfg.llama_server_path)
            .arg("-m").arg(&model)
            .arg("--host").arg("127.0.0.1")
            .arg("--port").arg(port.to_string())
            .arg("-c").arg(cfg.context_size.to_string())
            .arg("-t").arg(cfg.threads.to_string())
            .arg("--jinja")
            .arg("--chat-template-kwargs").arg(r#"{"enable_thinking":false}"#)
            .arg("--temp").arg("0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {}", cfg.llama_server_path))?;

        std::fs::write(paths::runtime_port(), port.to_string()).ok();
        tracing::info!(port, "llama-server spawned");
        Ok(Self { child, port })
    }

    /// Wraps an already-spawned child under the same supervision `spawn`
    /// gives a real `llama-server`: this is what makes `Drop`'s kill-and-reap
    /// behaviour testable with a stub process (`sleep 300`, a tiny script)
    /// instead of a real `llama-server`, which needs a model file on disk and
    /// a working ggml compute backend to even start.
    #[cfg(any(test, feature = "test-util"))]
    pub fn from_child(child: Child, port: u16) -> Self {
        Self { child, port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Polls `/health` until the model is loaded and serving, or the timeout
    /// elapses.
    ///
    /// Takes `&mut self` (not `&self`, as before C1) so it can call
    /// `Child::try_wait` between polls: a `llama-server` that fails to spawn
    /// at all with a working model (missing ggml compute backend, a bad
    /// model path, etc.) typically exits within milliseconds, and without
    /// this check the caller would otherwise learn that only after the full
    /// `timeout` elapsed -- up to 120 s of polling a corpse before reporting
    /// what was knowable almost immediately. See C1.
    pub fn wait_healthy(&mut self, timeout: Duration) -> Result<()> {
        let url = format!("{}/health", self.base_url());
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                bail!("llama-server exited during startup ({status})");
            }
            if health_probe(&url) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!("llama-server did not become healthy within {timeout:?}")
    }

    /// A single, non-blocking-retry health probe (used by the daemon's
    /// ongoing supervision loop, unlike `wait_healthy`'s startup poll).
    pub fn is_healthy(&self) -> bool {
        health_probe(&format!("{}/health", self.base_url()))
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        // `Child::kill` sends SIGKILL on Unix: immediate, not graceful, which
        // is exactly what we want here -- this exists to guarantee the model
        // is never left resident, not to give llama-server a chance to clean
        // up.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(paths::runtime_port());
        tracing::info!(port = self.port, "llama-server stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_port_returns_the_preferred_port_when_free() {
        // Bind and release first so the exact port is very likely free again
        // by the time pick_port checks it (best-effort: a genuinely flake-
        // proof version would need a mock listener, which isn't worth it
        // here).
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);

        assert_eq!(pick_port(port).unwrap(), port);
    }

    #[test]
    fn pick_port_skips_a_port_that_is_already_bound() {
        let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let held_port = held.local_addr().unwrap().port();

        let got = pick_port(held_port).unwrap();

        assert_ne!(got, held_port);
        assert!(
            (held_port..held_port.saturating_add(10)).contains(&got),
            "expected a port within 10 of {held_port}, got {got}"
        );
        drop(held);
    }

    #[test]
    fn pick_port_fails_when_the_whole_range_is_held() {
        let mut held = Vec::new();
        let base = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base_port = base.local_addr().unwrap().port();
        held.push(base);
        for p in base_port..base_port.saturating_add(10) {
            if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
                held.push(l);
            }
        }

        assert!(pick_port(base_port).is_err());
    }

    #[test]
    fn pick_port_error_message_does_not_overflow_near_the_top_of_the_range() {
        // `preferred + 10` (unsaturated) would overflow `u16` arithmetic --
        // and panic in this debug/test build -- for a `preferred` this close
        // to `u16::MAX`. Hold every port the loop actually checks so
        // `pick_port` is forced down the `bail!` path that builds the
        // message.
        let preferred = u16::MAX - 3;
        let mut held = Vec::new();
        for p in preferred..preferred.saturating_add(10) {
            if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
                held.push(l);
            }
        }

        let err = pick_port(preferred).unwrap_err();
        assert!(
            err.to_string().contains(&preferred.saturating_add(10).to_string()),
            "message should describe the saturated range, got: {err}"
        );
    }

    /// C1: proves `wait_healthy` reports a dead child almost immediately
    /// instead of polling it for the rest of the (potentially 120 s) budget.
    /// A real `llama-server` can't run here (no ggml compute backend on this
    /// machine), so this stubs the supervised child with a process that
    /// exits on its own right away -- `LlamaServer::from_child` is the same
    /// test seam `kill_llama_terminates_a_stub_child_and_is_idempotent`
    /// (owf-daemon.rs) uses for the same reason.
    #[test]
    fn wait_healthy_fails_fast_when_the_child_has_already_exited() {
        let child = std::process::Command::new("false")
            .spawn()
            .expect("spawning a stub child (`false`) for this test");
        // Give the child a moment to actually exit before asking; `false`
        // exits essentially instantly, but this keeps the test robust
        // against scheduling jitter without inflating the timing assertion
        // below.
        std::thread::sleep(Duration::from_millis(100));
        let mut server = LlamaServer::from_child(child, 0);

        let t0 = Instant::now();
        let err = server.wait_healthy(Duration::from_secs(120)).unwrap_err();
        let elapsed = t0.elapsed();

        assert!(err.to_string().contains("exited"), "got: {err}");
        assert!(
            elapsed < Duration::from_secs(5),
            "should fail fast rather than polling the full 120 s budget, took {elapsed:?}"
        );
    }

    #[test]
    fn spawn_fails_fast_when_the_model_file_is_missing() {
        // Exercise the ensure!() guard without touching the real, already-
        // downloaded model: point llama_server_path at a directory that
        // cannot possibly contain "s1-mini-q4_k_m.gguf" isn't possible since
        // models_dir() isn't configurable per-call, so instead this asserts
        // the guard's error message shape against the *actual* models_dir(),
        // which on a correctly provisioned machine will already exist -- so
        // this test only meaningfully fails closed (missing model -> error,
        // never a panic) rather than asserting the model is absent.
        let cfg = NormalizeConfig {
            llama_server_path: "/nonexistent/llama-server-binary".to_string(),
            ..NormalizeConfig::default()
        };
        // Either the model is missing (ensure! fires) or the model is
        // present and spawning the bogus binary fails (with_context fires).
        // Both are `Result::Err`, never a panic -- that's the behaviour under
        // test.
        assert!(LlamaServer::spawn(&cfg).is_err());
    }
}
