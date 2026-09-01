use std::io;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{InjectBackend, InjectConfig};
use crate::paths;
use crate::procutil;

/// I3: none of these subprocess calls may block the daemon forever. `wtype`
/// gets the most generous budget because `-d <ms>` (see `wtype_argv`) makes
/// its own runtime proportional to the text length -- even a full 120 s
/// utterance typed at a brisk pace is a few thousand characters, comfortably
/// inside this bound at the default 2 ms/keystroke delay.
const WTYPE_TIMEOUT: Duration = Duration::from_secs(15);
/// Twice `WTYPE_TIMEOUT` for the same text, because the two spell "delay"
/// differently: `wtype -d` waits once per character, `ydotool --key-delay`
/// waits once per *key event* -- press and release both -- so the same
/// configured `inject.keystroke_delay_ms` buys ydotool half the throughput.
const YDOTOOL_TIMEOUT: Duration = Duration::from_secs(30);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("{backend} exited with status {status}: {stderr}")]
    Failed { backend: &'static str, status: String, stderr: String },
    #[error("could not run {backend}: {source}")]
    Spawn { backend: &'static str, source: std::io::Error },
    #[error("{backend} did not respond within {timeout:?}")]
    Timeout { backend: &'static str, timeout: Duration },
    #[error("mock injector configured to fail")]
    Mock,
}

/// Maps a [`procutil::run_with_timeout`] failure onto the right
/// [`InjectError`] variant: a timeout is a distinct, recognizable failure
/// mode from "the OS couldn't even start the process" (I3), even though both
/// currently degrade the same way (fall through to the clipboard fallback).
fn map_proc_error(backend: &'static str, timeout: Duration, e: io::Error) -> InjectError {
    if e.kind() == io::ErrorKind::TimedOut {
        InjectError::Timeout { backend, timeout }
    } else {
        InjectError::Spawn { backend, source: e }
    }
}

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<(), InjectError>;
    fn name(&self) -> &'static str;
}

/// Builds the wtype argument vector.
///
/// `--` terminates option parsing so a transcript starting with `-` is typed
/// rather than misread as flags (confirmed against `wtype`'s own man page,
/// which documents `wtype [OPTION_OR_TEXT]... -- [TEXT]...`); `-d` inserts
/// an inter-keystroke delay that some Electron and XWayland surfaces need to
/// avoid dropping characters.
fn wtype_argv(text: &str, delay_ms: u32) -> Vec<String> {
    vec!["-d".to_string(), delay_ms.to_string(), "--".to_string(), text.to_string()]
}

pub struct WtypeInjector {
    delay_ms: u32,
}

impl WtypeInjector {
    pub fn new(delay_ms: u32) -> Self {
        Self { delay_ms }
    }
}

impl TextInjector for WtypeInjector {
    fn name(&self) -> &'static str {
        "wtype"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        run_typer("wtype", wtype_argv(text, self.delay_ms), WTYPE_TIMEOUT)
    }
}

/// Builds the ydotool argument vector.
///
/// `type` is a subcommand and owns its own `getopt_long` parser, so it has to
/// come before the options rather than after them. `--key-delay` is the same
/// idea as `wtype -d` but counted per key event rather than per character
/// (see [`YDOTOOL_TIMEOUT`]), and `--` terminates option parsing so a
/// transcript beginning with `-` is typed rather than misread as flags --
/// `getopt_long`'s own guarantee, not something ydotool documents.
///
/// Only the three options ydotool's man page actually documents for `type`
/// are used (`-d/--key-delay`, `-D/--next-delay`, `-f/--file`). In
/// particular `--escape`, which some builds accept and others do not, is
/// left alone: an unknown flag would fail every injection outright, and the
/// escaping it controls is not something spec 10.2 wants invented anyway.
fn ydotool_argv(text: &str, delay_ms: u32) -> Vec<String> {
    vec![
        "type".to_string(),
        "--key-delay".to_string(),
        delay_ms.to_string(),
        "--".to_string(),
        text.to_string(),
    ]
}

/// Spec 10.3's second injector, for the XWayland and Electron surfaces
/// `wtype` cannot reach (spec 17.3).
///
/// Unlike `wtype` this is not self-contained: it talks to a `ydotoold`
/// daemon over `$YDOTOOL_SOCKET`, and that daemon needs write access to
/// `/dev/uinput`. When either is missing, `ydotool` exits non-zero and the
/// clipboard fallback (spec 10.4) carries the transcript instead -- the user
/// still gets their text, per invariant 1. Setting that up is the user's
/// call, not the app's (same reason `hypr.rs` prints a config block rather
/// than applying one).
pub struct YdotoolInjector {
    delay_ms: u32,
}

impl YdotoolInjector {
    pub fn new(delay_ms: u32) -> Self {
        Self { delay_ms }
    }
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        run_typer("ydotool", ydotool_argv(text, self.delay_ms), YDOTOOL_TIMEOUT)
    }
}

/// Runs one argv-driven typing backend under I3's timeout and maps the
/// outcome onto [`InjectError`].
///
/// `backend` is both the error label and the program name -- true of `wtype`
/// and `ydotool` alike, which differ only in the argv they build and the
/// budget they are given.
fn run_typer(
    backend: &'static str,
    argv: Vec<String>,
    timeout: Duration,
) -> Result<(), InjectError> {
    tracing::debug!(backend, ?argv, ?timeout, "spawning injection backend");
    let mut cmd = Command::new(backend);
    cmd.args(&argv);
    let started = std::time::Instant::now();
    let out = procutil::run_with_timeout(cmd, timeout, None).map_err(|e| {
        let mapped = map_proc_error(backend, timeout, e);
        tracing::warn!(
            backend,
            ?argv,
            elapsed_ms = started.elapsed().as_millis() as u64,
            error = %mapped,
            "injection backend did not run"
        );
        mapped
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        // `ExitStatus`'s Display spells out death-by-signal, so a SIGSEGV in
        // the backend is distinguishable from a nonzero exit in the log.
        tracing::warn!(
            backend,
            ?argv,
            status = %out.status,
            elapsed_ms = started.elapsed().as_millis() as u64,
            stdout = %stdout,
            stderr = %stderr,
            "injection backend failed"
        );
        return Err(InjectError::Failed { backend, status: out.status.to_string(), stderr });
    }
    if !stdout.is_empty() || !stderr.is_empty() {
        // ydotool in particular prints notices to stderr while still exiting
        // 0 (e.g. socket-permission grumbles) -- that is real diagnostic
        // signal, not noise, so it is surfaced at info rather than debug.
        tracing::info!(backend, stdout = %stdout, stderr = %stderr,
            "injection backend succeeded but printed output");
    }
    tracing::debug!(
        backend,
        status = %out.status,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "injection backend succeeded"
    );
    Ok(())
}

/// One `path`'s existence, permissions and ownership, for
/// [`ydotool_env_report_at`] -- the three fields that decide whether the
/// ydotool client (this uid) may open a socket / device node.
fn stat_line(path: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    match std::fs::symlink_metadata(path) {
        Ok(m) => {
            format!("exists, mode {:04o}, uid {}, gid {}", m.mode() & 0o7777, m.uid(), m.gid())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => "missing".to_string(),
        Err(e) => format!("stat failed: {e}"),
    }
}

/// Scans `proc_dir` (i.e. `/proc`) for a process whose `comm` is `name`.
/// Reading `/proc` directly instead of shelling out to `pgrep` keeps the
/// probe subprocess-free -- nothing here can hang, so I3 needs no timeout.
fn find_process(proc_dir: &std::path::Path, name: &str) -> Option<u32> {
    let entries = std::fs::read_dir(proc_dir).ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
        if comm.trim() == name {
            return Some(pid);
        }
    }
    None
}

/// The testable core of [`ydotool_env_report`]: every input the report
/// depends on is a parameter, so tests can fabricate a socket, a uinput
/// node, a `/proc` table and a `$PATH` in a scratch directory.
fn ydotool_env_report_at(
    socket_env: Option<&str>,
    xdg_runtime_dir: Option<&str>,
    uinput_path: &std::path::Path,
    proc_dir: &std::path::Path,
    path_env: Option<&str>,
) -> String {
    let mut lines = Vec::new();
    match socket_env {
        Some(v) => {
            lines.push(format!("YDOTOOL_SOCKET={v}: {}", stat_line(std::path::Path::new(v))))
        }
        None => {
            // Which default the client uses varies by ydotool version
            // ($XDG_RUNTIME_DIR/.ydotool_socket on current builds,
            // /tmp/.ydotool_socket on older ones), so report both.
            lines.push("YDOTOOL_SOCKET unset; default socket candidates:".to_string());
            let mut candidates = Vec::new();
            if let Some(xdg) = xdg_runtime_dir {
                candidates.push(std::path::PathBuf::from(xdg).join(".ydotool_socket"));
            }
            candidates.push(std::path::PathBuf::from("/tmp/.ydotool_socket"));
            for c in candidates {
                lines.push(format!("  {}: {}", c.display(), stat_line(&c)));
            }
        }
    }
    lines.push(format!("uinput device {}: {}", uinput_path.display(), stat_line(uinput_path)));
    lines.push(match find_process(proc_dir, "ydotoold") {
        Some(pid) => format!("ydotoold: running (pid {pid})"),
        None => "ydotoold: not found in the process table".to_string(),
    });
    let binary = path_env.and_then(|p| {
        std::env::split_paths(p).map(|d| d.join("ydotool")).find(|c| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(c)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
    });
    lines.push(match binary {
        Some(b) => format!("ydotool binary: {}", b.display()),
        None => "ydotool binary: not found on PATH".to_string(),
    });
    lines.join("\n")
}

/// Fingerprints the three classic ydotool failure modes -- no `ydotoold`
/// running, a socket the client may not open, `/dev/uinput` the daemon may
/// not open -- from the live environment. Called (and logged) only when a
/// ydotool injection has just failed; every probe is a read-only stat or
/// `/proc` read, never a subprocess.
pub(crate) fn ydotool_env_report() -> String {
    ydotool_env_report_at(
        std::env::var("YDOTOOL_SOCKET").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        std::path::Path::new("/dev/uinput"),
        std::path::Path::new("/proc"),
        std::env::var("PATH").ok().as_deref(),
    )
}

pub struct ClipboardInjector;

impl TextInjector for ClipboardInjector {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        let cmd = Command::new("wl-copy");
        let out = procutil::run_with_timeout(cmd, CLIPBOARD_TIMEOUT, Some(text.as_bytes()))
            .map_err(|e| map_proc_error("wl-copy", CLIPBOARD_TIMEOUT, e))?;
        if !out.status.success() {
            return Err(InjectError::Failed {
                backend: "clipboard",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MockInjector {
    calls: Mutex<Vec<String>>,
    fail: bool,
    /// `None` reports the default `"mock"` name. See [`MockInjector::named`].
    name: Option<&'static str>,
}

impl MockInjector {
    pub fn failing() -> Self {
        Self { calls: Mutex::new(Vec::new()), fail: true, name: None }
    }

    /// A non-failing mock identified by `name` instead of the default
    /// `"mock"`.
    ///
    /// Lets a test tell two `MockInjector`s apart by the backend name
    /// `inject_with_fallback`/`inject_with_recovery` return -- e.g. a
    /// primary and a fallback injector in the same test. Without this,
    /// `a_working_fallback_never_touches_the_recovery_file` constructed both
    /// as plain `MockInjector`s (both named `"mock"`), so its
    /// `assert_eq!(backend, "mock")` passed identically whether the primary
    /// or the fallback had actually run.
    pub fn named(name: &'static str) -> Self {
        Self { calls: Mutex::new(Vec::new()), fail: false, name: Some(name) }
    }

    /// A failing mock that reports `name` -- for tests that need a failure
    /// attributed to a specific backend (e.g. the ydotool environment
    /// report, which keys on the failing primary's name).
    pub fn failing_named(name: &'static str) -> Self {
        Self { calls: Mutex::new(Vec::new()), fail: true, name: Some(name) }
    }

    pub fn injected(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl TextInjector for MockInjector {
    fn name(&self) -> &'static str {
        self.name.unwrap_or("mock")
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        if self.fail {
            return Err(InjectError::Mock);
        }
        self.calls.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

pub fn build(cfg: &InjectConfig) -> Box<dyn TextInjector> {
    match cfg.backend {
        InjectBackend::Wtype => Box::new(WtypeInjector::new(cfg.keystroke_delay_ms)),
        InjectBackend::Ydotool => Box::new(YdotoolInjector::new(cfg.keystroke_delay_ms)),
        InjectBackend::Clipboard => Box::new(ClipboardInjector),
    }
}

/// Appends one timestamped line to `<state_dir>/unsent.txt`.
///
/// This is the last resort when both the primary injector and the clipboard
/// fallback have failed: the transcript has already cost the user real
/// speech and ASR time, so it must not simply vanish — see spec 10.4.
/// Failing to write this file must never mask the original injection error,
/// so failures here are logged and swallowed, not propagated.
fn write_recovery_file(state_dir: &std::path::Path, text: &str) {
    if let Err(e) = write_recovery_file_inner(state_dir, text) {
        tracing::error!(error = %e, "failed to write recovery file; transcript may be lost");
    }
}

fn write_recovery_file_inner(state_dir: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(state_dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("unsent.txt"))?;
    writeln!(f, "[{ts}] {text}")
}

/// What [`inject_with_recovery`] actually did: which backend carried the
/// text, and -- when that was the fallback -- why the primary failed and (for
/// ydotool) what its environment looked like at that moment. The failure
/// detail exists so the per-utterance debug record can answer "why did this
/// machine fall back?" after the fact, instead of only the daemon log.
#[derive(Debug)]
pub(crate) struct InjectOutcome {
    pub backend: &'static str,
    pub primary_backend: &'static str,
    /// `Some` iff the fallback ran.
    pub primary_error: Option<String>,
    /// `Some` iff the failing primary was ydotool -- see [`ydotool_env_report`].
    pub env_report: Option<String>,
}

/// Injects via `primary`; on failure copies to the clipboard and notifies.
///
/// A transcript is never silently lost — see spec 10.4. If the clipboard
/// fallback also fails, the transcript is appended to a recovery file under
/// `paths::state_dir()` before the error is returned.
pub fn inject_with_fallback(
    primary: &dyn TextInjector,
    text: &str,
) -> anyhow::Result<&'static str> {
    inject_with_recovery(primary, &ClipboardInjector, text, &paths::state_dir())
        .map(|out| out.backend)
}

/// The testable core of [`inject_with_fallback`]: the clipboard fallback and
/// the recovery-file directory are both parameters, so tests can substitute
/// a failing fallback and a scratch directory without touching real system
/// state or the public signature `inject_with_fallback` is required to keep.
///
/// `pub(crate)` rather than private: `pipeline::Pipeline` calls this directly
/// with its own configurable fallback injector and recovery directory (see
/// `Pipeline::with_fallback_injector` / `with_recovery_dir`), so that a test
/// can force *both* injectors to fail deterministically -- `inject_with_fallback`
/// alone can't do that, since it hard-codes the real `ClipboardInjector` and
/// `paths::state_dir()`.
pub(crate) fn inject_with_recovery(
    primary: &dyn TextInjector,
    fallback: &dyn TextInjector,
    text: &str,
    state_dir: &std::path::Path,
) -> anyhow::Result<InjectOutcome> {
    match primary.inject(text) {
        Ok(()) => Ok(InjectOutcome {
            backend: primary.name(),
            primary_backend: primary.name(),
            primary_error: None,
            env_report: None,
        }),
        Err(primary_err) => {
            tracing::warn!(error = %primary_err, backend = primary.name(),
                "primary injector failed; falling back to clipboard");
            // Fingerprint the environment the moment ydotool fails: whether
            // ydotoold runs, and who may open the socket and /dev/uinput --
            // the questions that separate "works on this distro" from
            // "fails on that one".
            let env_report = (primary.name() == "ydotool").then(|| {
                let report = ydotool_env_report();
                tracing::warn!("ydotool environment at time of failure:\n{report}");
                report
            });
            match fallback.inject(text) {
                Ok(()) => {
                    procutil::notify_send(
                        "yappr",
                        "Typing failed — transcript copied to clipboard",
                    );
                    Ok(InjectOutcome {
                        backend: fallback.name(),
                        primary_backend: primary.name(),
                        primary_error: Some(primary_err.to_string()),
                        env_report,
                    })
                }
                Err(fallback_err) => {
                    // Spec 10.4: once transcribed, the user gets the text --
                    // if it can't be typed or copied, it must at least be
                    // saved to disk rather than discarded.
                    write_recovery_file(state_dir, text);
                    Err(anyhow::anyhow!(
                        "primary injector '{}' failed ({primary_err}); {} fallback also failed ({fallback_err}); transcript saved to {}",
                        primary.name(),
                        fallback.name(),
                        state_dir.join("unsent.txt").display(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InjectBackend, InjectConfig};

    #[test]
    fn mock_injector_records_what_it_was_given() {
        let m = MockInjector::default();
        m.inject("hello").unwrap();
        m.inject("world").unwrap();
        assert_eq!(m.injected(), vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn mock_injector_can_be_told_to_fail() {
        let m = MockInjector::failing();
        assert!(m.inject("hello").is_err());
    }

    #[test]
    fn wtype_argv_ends_option_parsing_before_the_text() {
        // A transcript beginning with '-' must be typed, not parsed as flags.
        let argv = wtype_argv("-- not a flag", 2);
        let dashdash = argv.iter().position(|a| a == "--").expect("needs a --");
        assert_eq!(argv.last().unwrap(), "-- not a flag");
        assert!(dashdash < argv.len() - 1, "-- must precede the text");
        assert!(argv.contains(&"-d".to_string()));
        assert!(argv.contains(&"2".to_string()));
    }

    #[test]
    fn wtype_argv_passes_the_text_as_a_single_argument() {
        let argv = wtype_argv("hello there friend", 2);
        assert_eq!(argv.iter().filter(|a| a.contains(' ')).count(), 1);
    }

    #[test]
    fn ydotool_argv_ends_option_parsing_before_the_text() {
        // Same hazard as wtype: a transcript beginning with '-' must be
        // typed, not parsed as flags. `ydotool type` parses with
        // getopt_long, so `--` terminates option scanning.
        let argv = ydotool_argv("-- not a flag", 2);
        let dashdash = argv.iter().position(|a| a == "--").expect("needs a --");
        assert_eq!(argv.last().unwrap(), "-- not a flag");
        assert!(dashdash < argv.len() - 1, "-- must precede the text");
        assert_eq!(argv.first().unwrap(), "type", "type is a subcommand, not a flag");
        assert!(argv.contains(&"--key-delay".to_string()));
        assert!(argv.contains(&"2".to_string()));
    }

    #[test]
    fn ydotool_argv_passes_the_text_as_a_single_argument() {
        let argv = ydotool_argv("hello there friend", 2);
        assert_eq!(argv.iter().filter(|a| a.contains(' ')).count(), 1);
    }

    #[test]
    fn ydotool_argv_puts_the_subcommand_before_its_options() {
        // `ydotool --key-delay 2 type ...` is not a thing: the subcommand
        // owns the option parser, so it has to come first.
        let argv = ydotool_argv("hello", 2);
        let sub = argv.iter().position(|a| a == "type").unwrap();
        let delay = argv.iter().position(|a| a == "--key-delay").unwrap();
        assert!(sub < delay, "got: {argv:?}");
    }

    #[test]
    fn build_selects_the_configured_backend() {
        let mut cfg = InjectConfig::default();
        assert_eq!(build(&cfg).name(), "wtype");
        cfg.backend = InjectBackend::Ydotool;
        assert_eq!(build(&cfg).name(), "ydotool");
        cfg.backend = InjectBackend::Clipboard;
        assert_eq!(build(&cfg).name(), "clipboard");
    }

    #[test]
    fn fallback_reports_the_primary_when_it_succeeds() {
        let m = MockInjector::default();
        assert_eq!(inject_with_fallback(&m, "hello").unwrap(), "mock");
        assert_eq!(m.injected(), vec!["hello".to_string()]);
    }

    /// A fresh, collision-free scratch directory for a single test. Not a
    /// dependency: `tempfile` isn't in `[dev-dependencies]`, and this is the
    /// whole of what's needed here.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-core-test-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn when_both_injectors_fail_the_transcript_is_saved_to_the_recovery_file() {
        let dir = scratch_dir("recovery");
        let primary = MockInjector::failing();
        let fallback = MockInjector::failing();

        let err = inject_with_recovery(&primary, &fallback, "please do not lose this", &dir)
            .unwrap_err();
        assert!(err.to_string().contains("unsent.txt"), "got: {err}");

        let saved = std::fs::read_to_string(dir.join("unsent.txt")).unwrap();
        assert!(
            saved.contains("please do not lose this"),
            "recovery file should contain the transcript, got: {saved:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_primary_records_its_error_and_the_fallback_backend_in_the_outcome() {
        let dir = scratch_dir("outcome-error");
        let primary = MockInjector::failing();
        let fallback = MockInjector::named("fallback-mock");

        let out = inject_with_recovery(&primary, &fallback, "hello", &dir).unwrap();
        assert_eq!(out.backend, "fallback-mock");
        assert_eq!(out.primary_backend, "mock");
        let err = out.primary_error.expect("must record why the primary failed");
        assert!(err.contains("mock"), "the error should name its cause, got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_primary_leaves_no_failure_detail_in_the_outcome() {
        let dir = scratch_dir("outcome-clean");
        let primary = MockInjector::default();
        let fallback = MockInjector::named("fallback-mock");

        let out = inject_with_recovery(&primary, &fallback, "hello", &dir).unwrap();
        assert_eq!(out.backend, "mock");
        assert_eq!(out.primary_backend, "mock");
        assert!(out.primary_error.is_none());
        assert!(out.env_report.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_env_report_is_attached_only_when_the_failing_primary_is_ydotool() {
        let dir = scratch_dir("outcome-env");
        let fallback = MockInjector::named("fallback-mock");

        let ydotool_like = MockInjector::failing_named("ydotool");
        let out = inject_with_recovery(&ydotool_like, &fallback, "hello", &dir).unwrap();
        let report = out.env_report.expect("a ydotool failure must carry the environment report");
        assert!(report.contains("ydotoold"), "got: {report}");

        let wtype_like = MockInjector::failing_named("wtype");
        let out = inject_with_recovery(&wtype_like, &fallback, "hello", &dir).unwrap();
        assert!(
            out.env_report.is_none(),
            "wtype needs no ydotoold/uinput diagnosis; the report is ydotool-specific"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_report_shows_the_socket_env_value_and_whether_that_socket_exists() {
        let dir = scratch_dir("env-sock");
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("custom.sock");
        let sock_str = sock.to_str().unwrap();

        let report = ydotool_env_report_at(
            Some(sock_str),
            None,
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            None,
        );
        assert!(report.contains("YDOTOOL_SOCKET"), "got: {report}");
        assert!(report.contains(sock_str), "got: {report}");
        assert!(report.contains("missing"), "an absent socket must be called out, got: {report}");

        std::fs::write(&sock, b"").unwrap();
        let report = ydotool_env_report_at(
            Some(sock_str),
            None,
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            None,
        );
        assert!(
            report.contains("mode"),
            "an existing socket must report its permissions, got: {report}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_report_names_the_default_socket_candidates_when_the_env_var_is_unset() {
        let dir = scratch_dir("env-sock-unset");
        std::fs::create_dir_all(&dir).unwrap();

        let report = ydotool_env_report_at(
            None,
            Some("/run/user/12345"),
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            None,
        );
        assert!(report.contains("unset"), "got: {report}");
        assert!(report.contains("/run/user/12345/.ydotool_socket"), "got: {report}");
        assert!(report.contains("/tmp/.ydotool_socket"), "got: {report}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_report_reports_uinput_permissions_or_its_absence() {
        let dir = scratch_dir("env-uinput");
        std::fs::create_dir_all(&dir).unwrap();

        let missing = ydotool_env_report_at(
            None,
            None,
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            None,
        );
        assert!(missing.contains("uinput"), "got: {missing}");
        assert!(missing.contains("missing"), "got: {missing}");

        let uinput = dir.join("uinput");
        std::fs::write(&uinput, b"").unwrap();
        let present = ydotool_env_report_at(None, None, &uinput, &dir.join("no-proc"), None);
        assert!(
            present.contains("mode"),
            "an existing uinput must report its permissions, got: {present}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_report_finds_a_running_ydotoold_in_the_proc_table() {
        let dir = scratch_dir("env-proc");
        let proc_dir = dir.join("proc");
        std::fs::create_dir_all(proc_dir.join("1234")).unwrap();
        std::fs::write(proc_dir.join("1234").join("comm"), "ydotoold\n").unwrap();
        std::fs::create_dir_all(proc_dir.join("99")).unwrap();
        std::fs::write(proc_dir.join("99").join("comm"), "bash\n").unwrap();

        let report =
            ydotool_env_report_at(None, None, &dir.join("no-uinput"), &proc_dir, None);
        assert!(report.contains("ydotoold: running (pid 1234)"), "got: {report}");

        let empty_proc = dir.join("empty-proc");
        std::fs::create_dir_all(&empty_proc).unwrap();
        let report =
            ydotool_env_report_at(None, None, &dir.join("no-uinput"), &empty_proc, None);
        assert!(report.contains("ydotoold: not found"), "got: {report}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_report_resolves_the_ydotool_binary_from_path() {
        let dir = scratch_dir("env-bin");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("ydotool");
        std::fs::write(&exe, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let report = ydotool_env_report_at(
            None,
            None,
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            Some(bin_dir.to_str().unwrap()),
        );
        assert!(report.contains(exe.to_str().unwrap()), "got: {report}");

        let report = ydotool_env_report_at(
            None,
            None,
            &dir.join("no-uinput"),
            &dir.join("no-proc"),
            Some(dir.join("empty-bin").to_str().unwrap()),
        );
        assert!(report.contains("not found on PATH"), "got: {report}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_working_fallback_never_touches_the_recovery_file() {
        let dir = scratch_dir("no-recovery");
        let primary = MockInjector::failing();
        // Named distinctly from the primary (fix: both used to be plain
        // `MockInjector::default()`, both reporting the same "mock" name, so
        // asserting on the returned backend could not actually tell which
        // one ran).
        let fallback = MockInjector::named("fallback-mock");

        let out = inject_with_recovery(&primary, &fallback, "hello", &dir).unwrap();
        assert_eq!(out.backend, "fallback-mock", "must report that the fallback, not the primary, ran");
        assert_eq!(fallback.injected(), vec!["hello".to_string()]);
        assert!(!dir.join("unsent.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
