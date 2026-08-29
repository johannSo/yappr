//! Bounds a subprocess call to a timeout, killing the child if it hangs.
//!
//! `std::process::Command::output()` (and `Child::wait()`) block forever if
//! the child never exits -- exactly the `wtype`-on-XWayland hang spec 17.3
//! warns about. Before I3, every subprocess call in `inject.rs` and
//! `hypr.rs` used `.output()` directly: a hung `wtype` wedged the daemon at
//! `BUSY` forever and lost the in-flight transcript, and a hung `hyprctl`
//! wedged the whole single-threaded accept loop so even `status` stopped
//! answering. Both now go through [`run_with_timeout`] instead.
//!
//! This polls `Child::try_wait` on a short interval rather than racing a
//! worker thread against `recv_timeout` (contrast `normalize.rs`'s HTTP
//! call, which blocks inside `ureq` where nothing but `ureq`'s own timeout
//! can interrupt it -- see I2): a child process, unlike a blocked network
//! read, can always be killed outright from the outside, so there is no
//! thread left to leak here.

use std::io::{self, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Moved here from `inject.rs` alongside [`notify_send`]: how long a hung
/// `notify-send` (a stuck or absent notification daemon) is given before
/// this module kills it.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Runs `command` to completion, or kills it after `timeout`.
///
/// `stdin`, when given, is written and the pipe closed *before* polling
/// begins -- a child blocked reading stdin forever is exactly the kind of
/// hang this function exists to bound, so the write happens up front rather
/// than racing the poll loop's own timeout accounting.
pub fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    stdin: Option<&[u8]>,
) -> io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(data) = stdin {
        if let Some(mut si) = child.stdin.take() {
            si.write_all(data)?;
            // `si` drops here, closing the pipe so the child sees EOF.
        }
    }
    wait_with_timeout(child, timeout)
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> io::Result<Output> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut o) = child.stdout.take() {
                let _ = o.read_to_end(&mut stdout);
            }
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_end(&mut stderr);
            }
            return Ok(Output { status, stdout, stderr });
        }
        if start.elapsed() >= timeout {
            // Best-effort: kill and reap so the child never outlives this
            // call as a zombie, but a failure here doesn't change the
            // outcome -- the caller is told about the timeout either way.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process did not exit within {timeout:?}"),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Fires a desktop notification via the `notify-send` subprocess.
///
/// A courtesy, not part of the contract: a missing or failing `notify-send`
/// (no notification daemon running, binary not installed, etc.) must never
/// surface as an error, so any failure — spawn or exit status — is silently
/// discarded.
///
/// The one place in this workspace that actually spawns `notify-send`:
/// `inject.rs`'s clipboard-fallback notice and `client.rs`'s client-failure
/// notice both call through here rather than each owning its own
/// invocation, so a future change to the timeout, icon or urgency only has
/// one call site to make it in.
pub fn notify_send(summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.arg(summary).arg(body);
    let _ = run_with_timeout(cmd, NOTIFY_TIMEOUT, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fast_command_completes_normally() {
        let cmd = Command::new("true");
        let out = run_with_timeout(cmd, Duration::from_secs(2), None).unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn stdout_is_captured() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        let out = run_with_timeout(cmd, Duration::from_secs(2), None).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }

    #[test]
    fn stdin_is_forwarded_and_closed_so_the_child_sees_eof() {
        let cmd = Command::new("cat");
        let out = run_with_timeout(cmd, Duration::from_secs(2), Some(b"through stdin")).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "through stdin");
    }

    #[test]
    fn a_hung_command_is_killed_and_reported_as_timed_out() {
        let mut cmd = Command::new("sleep");
        cmd.arg("300");

        let t0 = Instant::now();
        let err = run_with_timeout(cmd, Duration::from_millis(200), None).unwrap_err();
        let elapsed = t0.elapsed();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(2),
            "should return promptly after the timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn a_nonzero_exit_status_is_still_reported_rather_than_erroring() {
        let cmd = Command::new("false");
        let out = run_with_timeout(cmd, Duration::from_secs(2), None).unwrap();
        assert!(!out.status.success());
    }
}
