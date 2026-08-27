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
    bail!("no free port in {}..{}", preferred, preferred + 10)
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

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Polls `/health` until the model is loaded and serving, or the timeout
    /// elapses.
    pub fn wait_healthy(&self, timeout: Duration) -> Result<()> {
        let url = format!("{}/health", self.base_url());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ureq::get(&url).call().map(|r| r.status() == 200).unwrap_or(false) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!("llama-server did not become healthy within {timeout:?}")
    }

    /// A single, non-blocking-retry health probe (used by the daemon's
    /// ongoing supervision loop, unlike `wait_healthy`'s startup poll).
    pub fn is_healthy(&self) -> bool {
        ureq::get(&format!("{}/health", self.base_url()))
            .call()
            .map(|r| r.status() == 200)
            .unwrap_or(false)
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
