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
//! read, can always be killed outright from the outside. The only threads
//! here drain the child's pipes; they end when the last holder of a pipe
//! closes it, and a grandchild that keeps one open is what
//! [`PIPE_DRAIN_GRACE`] exists for.

use std::io::{self, Read, Write};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Moved here from `inject.rs` alongside [`notify_send`]: how long a hung
/// `notify-send` (a stuck or absent notification daemon) is given before
/// this module kills it.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// The variables an AppImage's startup hooks export to point this process at
/// the libraries, GTK modules, GSettings schemas and icon themes *inside the
/// mount* -- every one of which is wrong for a child process that is a host
/// binary.
///
/// Read `linuxdeploy-plugin-gtk.sh` in any bundle for the full list; these
/// are the ones that reach a child and change what it does.
const BUNDLED_ENV: &[&str] = &[
    // Loader-level. yappr's own bundle uses `RUNPATH $ORIGIN/../lib` rather
    // than these, but other AppImages export them and a user may launch
    // yappr from one; a child resolving the wrong libc or glib dies in ways
    // that are very hard to read from the outside.
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    // GLib/GTK-level. These are the ones that actually bite: a `gdbus` or
    // `wl-copy` from the host that inherits a *different* GLib's module
    // path, schema directory or pixbuf cache either fails to start or fails
    // to find an interface, and reports it as something unrelated.
    "GIO_EXTRA_MODULES",
    "GSETTINGS_SCHEMA_DIR",
    "GTK_PATH",
    "GTK_DATA_PREFIX",
    "GTK_EXE_PREFIX",
    "GTK_IM_MODULE_FILE",
    "GTK_THEME",
    "GDK_PIXBUF_MODULE_FILE",
    "GDK_BACKEND",
    "XDG_DATA_DIRS",
    "APPDIR",
];

/// Strips the running bundle's environment off `command` so a child runs the
/// way it would from a shell.
///
/// This is why a paste script does not need its own `unset LD_LIBRARY_PATH`
/// line: the reference script this backend was built against carries one,
/// with a comment recording that an AppImage's bundled glib made `gdbus`
/// die on `undefined symbol: g_variant_builder_init_static`. That is a bug
/// in the *caller*, not something every script author should have to know.
///
/// Applied unconditionally rather than only under `$APPIMAGE`: outside a
/// bundle these are either unset (removing them is a no-op) or the user's
/// own, and a `GTK_THEME` or `XDG_DATA_DIRS` meant for this GUI process is
/// not something a `ydotool` invocation needs either. `PATH`, `HOME`,
/// `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and `YDOTOOL_SOCKET` are untouched --
/// children genuinely need those.
pub fn unbundle(command: &mut Command) {
    for key in BUNDLED_ENV {
        command.env_remove(key);
    }
}


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
    unbundle(&mut command);
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

/// Drains one of the child's output pipes on its own thread. Bytes are
/// appended under the mutex as they arrive, so the caller can take what has
/// been read so far without waiting for EOF -- which never comes while a
/// grandchild that inherited the pipe is alive.
fn drain_on_thread(pipe: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });
    buf
}

/// After the child has exited, everything it wrote is already in the pipe
/// buffers and the drain threads pick it up in microseconds; this is how
/// long they are given before the output is taken as-is. It is a bound on
/// the *drain*, not a wait for EOF: `wl-copy` (and anything else that forks
/// a daemon) leaves a grandchild holding the pipes open for as long as it
/// lives, and `read_to_end` on that pipe used to block the pipeline thread
/// at `INJECTING` until the clipboard was next replaced.
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(50);

fn wait_with_timeout(mut child: Child, timeout: Duration) -> io::Result<Output> {
    let stdout = child.stdout.take().map(drain_on_thread);
    let stderr = child.stderr.take().map(drain_on_thread);
    let take = |buf: &Option<Arc<Mutex<Vec<u8>>>>| {
        buf.as_ref().map(|b| b.lock().unwrap().clone()).unwrap_or_default()
    };
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            std::thread::sleep(PIPE_DRAIN_GRACE);
            return Ok(Output { status, stdout: take(&stdout), stderr: take(&stderr) });
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
    fn a_child_that_exits_but_leaves_a_grandchild_holding_its_pipes_does_not_block() {
        // wl-copy's shape: the process exits 0 at once, but a background
        // fork it left behind inherits stdout/stderr and keeps them open
        // (for the clipboard's lifetime, in wl-copy's case). Reading the
        // pipes to EOF after the exit would block until that grandchild
        // dies -- with no timeout, since the timeout only bounds the exit.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf out; printf err >&2; sleep 5 & exit 0");

        let t0 = Instant::now();
        let out = run_with_timeout(cmd, Duration::from_secs(3), None).unwrap();
        let elapsed = t0.elapsed();

        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err");
        assert!(
            elapsed < Duration::from_secs(1),
            "must return once the child itself has exited, took {elapsed:?}"
        );
    }

    #[test]
    fn a_nonzero_exit_status_is_still_reported_rather_than_erroring() {
        let cmd = Command::new("false");
        let out = run_with_timeout(cmd, Duration::from_secs(2), None).unwrap();
        assert!(!out.status.success());
    }
}
