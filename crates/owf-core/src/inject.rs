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
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

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
        let mut cmd = Command::new("wtype");
        cmd.args(wtype_argv(text, self.delay_ms));
        let out = procutil::run_with_timeout(cmd, WTYPE_TIMEOUT, None)
            .map_err(|e| map_proc_error("wtype", WTYPE_TIMEOUT, e))?;
        if !out.status.success() {
            return Err(InjectError::Failed {
                backend: "wtype",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
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
        InjectBackend::Clipboard => Box::new(ClipboardInjector),
    }
}

/// Fires a desktop notification via the `notify-send` subprocess.
///
/// A courtesy, not part of the contract: a missing or failing `notify-send`
/// (no notification daemon running, binary not installed, etc.) must never
/// surface as an error, so any failure — spawn or exit status — is silently
/// discarded.
fn notify_send(summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.arg(summary).arg(body);
    let _ = procutil::run_with_timeout(cmd, NOTIFY_TIMEOUT, None);
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
) -> anyhow::Result<&'static str> {
    match primary.inject(text) {
        Ok(()) => Ok(primary.name()),
        Err(primary_err) => {
            tracing::warn!(error = %primary_err, "primary injector failed; falling back to clipboard");
            match fallback.inject(text) {
                Ok(()) => {
                    notify_send(
                        "OpenWhisprFlow",
                        "Typing failed — transcript copied to clipboard",
                    );
                    Ok(fallback.name())
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
    fn build_selects_the_configured_backend() {
        let mut cfg = InjectConfig::default();
        assert_eq!(build(&cfg).name(), "wtype");
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
        std::env::temp_dir().join(format!("owf-core-test-{tag}-{}-{n}", std::process::id()))
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
    fn a_working_fallback_never_touches_the_recovery_file() {
        let dir = scratch_dir("no-recovery");
        let primary = MockInjector::failing();
        // Named distinctly from the primary (fix: both used to be plain
        // `MockInjector::default()`, both reporting the same "mock" name, so
        // asserting on the returned backend could not actually tell which
        // one ran).
        let fallback = MockInjector::named("fallback-mock");

        let backend = inject_with_recovery(&primary, &fallback, "hello", &dir).unwrap();
        assert_eq!(backend, "fallback-mock", "must report that the fallback, not the primary, ran");
        assert_eq!(fallback.injected(), vec!["hello".to_string()]);
        assert!(!dir.join("unsent.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
