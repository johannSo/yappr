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
    #[error("no paste script configured -- set [inject] script to an executable path")]
    NotConfigured,
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
    /// Injects `text` into the window that was focused when the recording
    /// started. `target_class` is that window's class as captured at
    /// `ptt-start` (`daemon.window_class`, the same value the style rules
    /// match on), or `None` when no provider could name it. No shipped
    /// injector reads it any more -- the ydotool backend that did was
    /// retired on 2026-09-09 -- but it describes the *target*, `MockInjector`
    /// records it, and the debug record writes it, so it still travels with
    /// every injection.
    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError>;
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

    fn inject(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
        run_backend(
            "wtype",
            std::ffi::OsStr::new("wtype"),
            wtype_argv(text, self.delay_ms),
            WTYPE_TIMEOUT,
        )
    }
}

/// Runs one argv-driven injection backend under I3's timeout and maps the
/// outcome onto [`InjectError`].
///
/// `backend` is the error label and `program` the thing actually spawned.
/// For `wtype` these are the same string; for the script backend `program`
/// is the user's path and `backend` stays the stable `"script"` that
/// `InjectOutcome`, the debug record and the settings GUI all name.
fn run_backend(
    backend: &'static str,
    program: &std::ffi::OsStr,
    argv: Vec<String>,
    timeout: Duration,
) -> Result<(), InjectError> {
    tracing::debug!(backend, ?program, ?argv, ?timeout, "spawning injection backend");
    let mut cmd = Command::new(program);
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
        // A user's paste script is the likely source here, and its output is
        // the only account of what it did -- surfaced at info rather than
        // debug for that reason, even on the success path.
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

pub struct ClipboardInjector;

impl TextInjector for ClipboardInjector {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn inject(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
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

/// How long a user's paste script may take before I3's timeout claims it.
/// Generous on purpose: `handy-paste.sh`, the script this backend was built
/// against, waits on `wl-paste` to save the old clipboard, sleeps 200 ms for
/// the compositor to hand over the new offer, presses six key events at
/// 50 ms, and sleeps another 300 ms -- ~1,5 s all told. The bound exists so
/// a *hung* script cannot wedge the pipeline thread, not to police a slow
/// one.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Spec 10.3's second injector, since 2026-09-09 a **user-supplied script**
/// rather than `ydotool`.
///
/// It runs `[inject] script` with the finished transcript as its single
/// argument and does nothing else -- no `wl-copy` first, no environment of
/// its own, no paste chord. The script owns the entire injection: how the
/// text reaches the clipboard, which chord it presses, how it names the
/// focused window, whether it restores what was in the clipboard before.
///
/// Each of those omissions is deliberate:
///
///   - **No pre-copy.** The `ydotool` backend this replaced staged the text
///     with [`ClipboardInjector`] before pressing a chord. A script that
///     saves and restores the previous clipboard (as the reference one does)
///     would then "restore" yappr's own transcript over the user's.
///   - **No chord.** Choosing Ctrl+V vs Ctrl+Shift+V needed the focused
///     window's class, and every provider for it can answer `None` (see
///     [`crate::winclass`]) -- which produced a plain Ctrl+V that terminals
///     ignore, from a `ydotool` that exited 0, so nothing failed and nothing
///     was logged. A script asks its own desktop, its own way.
///   - **No `$2`, no `YAPPR_*` environment.** `$1` is the whole contract, so
///     a script written for another dictation tool works unchanged.
///
/// Failure of any kind -- unconfigured, missing, non-executable, non-zero
/// exit, timeout -- is an `Err`, which `inject_with_recovery` turns into the
/// clipboard fallback and its notification. Invariant 1 holds.
pub struct ScriptInjector {
    path: std::path::PathBuf,
}

impl ScriptInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        // `Command` performs no expansion -- only a shell does, and there is
        // no shell on this path -- so `~/bin/paste.sh`, which is exactly what
        // a user types into the settings field, has to be expanded here.
        Self { path: crate::debug::expand_tilde(&cfg.script) }
    }
}

impl TextInjector for ScriptInjector {
    fn name(&self) -> &'static str {
        "script"
    }

    fn inject(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
        if self.path.as_os_str().is_empty() {
            return Err(InjectError::NotConfigured);
        }
        run_backend("script", self.path.as_os_str(), vec![text.to_string()], SCRIPT_TIMEOUT)
    }
}


#[derive(Default)]
pub struct MockInjector {
    calls: Mutex<Vec<String>>,
    /// The `target_class` each call carried, parallel to `calls`.
    classes: Mutex<Vec<Option<String>>>,
    fail: bool,
    /// `None` reports the default `"mock"` name. See [`MockInjector::named`].
    name: Option<&'static str>,
}

impl MockInjector {
    pub fn failing() -> Self {
        Self { fail: true, ..Self::default() }
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
        Self { name: Some(name), ..Self::default() }
    }

    /// A failing mock that reports `name` -- for tests that need a failure
    /// attributed to a specific backend rather than the generic `"mock"`.
    pub fn failing_named(name: &'static str) -> Self {
        Self { fail: true, name: Some(name), ..Self::default() }
    }

    pub fn injected(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The `target_class` each successful call carried, parallel to
    /// [`MockInjector::injected`].
    pub fn target_classes(&self) -> Vec<Option<String>> {
        self.classes.lock().unwrap().clone()
    }
}

impl TextInjector for MockInjector {
    fn name(&self) -> &'static str {
        self.name.unwrap_or("mock")
    }

    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        if self.fail {
            return Err(InjectError::Mock);
        }
        self.calls.lock().unwrap().push(text.to_string());
        self.classes.lock().unwrap().push(target_class.map(str::to_string));
        Ok(())
    }
}

pub fn build(cfg: &InjectConfig) -> Box<dyn TextInjector> {
    match cfg.backend {
        InjectBackend::Wtype => Box::new(WtypeInjector::new(cfg.keystroke_delay_ms)),
        InjectBackend::Script => Box::new(ScriptInjector::new(cfg)),
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
/// text, and -- when that was the fallback -- why the primary failed. The
/// failure detail exists so the per-utterance debug record can answer "why
/// did this machine fall back?" after the fact, instead of only the daemon
/// log.
#[derive(Debug)]
pub(crate) struct InjectOutcome {
    pub backend: &'static str,
    pub primary_backend: &'static str,
    /// `Some` iff the fallback ran.
    pub primary_error: Option<String>,
}

/// Injects via `primary`; on failure copies to the clipboard and notifies.
///
/// A transcript is never silently lost — see spec 10.4. If the clipboard
/// fallback also fails, the transcript is appended to a recovery file under
/// `paths::state_dir()` before the error is returned.
pub fn inject_with_fallback(
    primary: &dyn TextInjector,
    text: &str,
    target_class: Option<&str>,
) -> anyhow::Result<&'static str> {
    inject_with_recovery(primary, &ClipboardInjector, text, target_class, &paths::state_dir())
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
    target_class: Option<&str>,
    state_dir: &std::path::Path,
) -> anyhow::Result<InjectOutcome> {
    match primary.inject(text, target_class) {
        Ok(()) => Ok(InjectOutcome {
            backend: primary.name(),
            primary_backend: primary.name(),
            primary_error: None,
        }),
        Err(primary_err) => {
            tracing::warn!(error = %primary_err, backend = primary.name(),
                "primary injector failed; falling back to clipboard");
            match fallback.inject(text, target_class) {
                Ok(()) => {
                    procutil::notify_send(
                        "yappr",
                        "Typing failed — transcript copied to clipboard",
                    );
                    Ok(InjectOutcome {
                        backend: fallback.name(),
                        primary_backend: primary.name(),
                        primary_error: Some(primary_err.to_string()),
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

    /// Writes an executable shell script into a fresh temp directory and
    /// returns `(dir, script_path)`. The caller removes `dir`.
    ///
    /// No `tempfile` dependency: this crate has none, and the tests that
    /// need scratch space elsewhere build their paths the same way.
    fn script_fixture(tag: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir =
            std::env::temp_dir().join(format!("yappr-script-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paste.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    /// An `InjectConfig` whose script backend points at `path`.
    fn script_cfg(path: &std::path::Path) -> InjectConfig {
        InjectConfig { script: path.display().to_string(), ..InjectConfig::default() }
    }

    #[test]
    fn mock_injector_records_what_it_was_given() {
        let m = MockInjector::default();
        m.inject("hello", None).unwrap();
        m.inject("world", Some("kitty")).unwrap();
        assert_eq!(m.injected(), vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(m.target_classes(), vec![None, Some("kitty".to_string())]);
    }

    #[test]
    fn mock_injector_can_be_told_to_fail() {
        let m = MockInjector::failing();
        assert!(m.inject("hello", None).is_err());
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
        cfg.backend = InjectBackend::Script;
        assert_eq!(build(&cfg).name(), "script");
        cfg.backend = InjectBackend::Clipboard;
        assert_eq!(build(&cfg).name(), "clipboard");
    }

    #[test]
    fn the_target_window_class_travels_to_both_injectors() {
        // The class captured at ptt-start describes the target window, and
        // the per-window style rules resolve against it, so
        // inject_with_recovery must hand it to the primary -- and to the
        // fallback, which is an injector like any other.
        let dir = scratch_dir("class-through");
        let primary = MockInjector::default();
        let out = inject_with_recovery(
            &primary,
            &MockInjector::named("fallback-mock"),
            "hello",
            Some("kitty"),
            &dir,
        )
        .unwrap();
        assert_eq!(out.backend, "mock");
        assert_eq!(primary.target_classes(), vec![Some("kitty".to_string())]);

        let failing = MockInjector::failing();
        let fallback = MockInjector::named("fallback-mock");
        inject_with_recovery(&failing, &fallback, "hello", Some("kitty"), &dir).unwrap();
        assert_eq!(fallback.target_classes(), vec![Some("kitty".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_reports_the_primary_when_it_succeeds() {
        let m = MockInjector::default();
        assert_eq!(inject_with_fallback(&m, "hello", None).unwrap(), "mock");
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

        let err = inject_with_recovery(&primary, &fallback, "please do not lose this", None, &dir)
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

        let out = inject_with_recovery(&primary, &fallback, "hello", None, &dir).unwrap();
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

        let out = inject_with_recovery(&primary, &fallback, "hello", None, &dir).unwrap();
        assert_eq!(out.backend, "mock");
        assert_eq!(out.primary_backend, "mock");
        assert!(out.primary_error.is_none());

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

        let out = inject_with_recovery(&primary, &fallback, "hello", None, &dir).unwrap();
        assert_eq!(out.backend, "fallback-mock", "must report that the fallback, not the primary, ran");
        assert_eq!(fallback.injected(), vec!["hello".to_string()]);
        assert!(!dir.join("unsent.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_receives_the_transcript_as_its_only_argument() {
        // The whole contract with a user's paste script: `$1` is the text,
        // and there is no `$2`. `handy-paste.sh` -- the script this backend
        // was built against -- reads nothing else, so anything extra here
        // would be a promise no script has to keep.
        let (dir, path) = script_fixture(
            "argv",
            r#"printf '%s' "$#" > "$(dirname "$0")/count"
printf '%s' "$1" > "$(dirname "$0")/arg1""#,
        );

        ScriptInjector::new(&script_cfg(&path)).inject("hallo welt", Some("kitty")).unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("count")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(dir.join("arg1")).unwrap(), "hallo welt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transcript_that_begins_with_a_dash_reaches_the_script_intact() {
        // Same hazard `wtype_argv`'s `--` guards against, answered
        // differently: there is no shell and no option parsing between here
        // and the script, so the text is passed as one argv element and
        // arrives whole. Pinned because "just run it through sh -c" is the
        // obvious wrong turn, and it would break on this input.
        let (dir, path) = script_fixture("dash", r#"printf '%s' "$1" > "$(dirname "$0")/arg1""#);

        ScriptInjector::new(&script_cfg(&path)).inject("-n --version", None).unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("arg1")).unwrap(), "-n --version");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unconfigured_script_path_fails_without_spawning_anything() {
        // `[inject] script` defaults to empty, so this is the state of every
        // user who selects the backend before writing a script. It must be a
        // clean, named error -- which `inject_with_recovery` turns into the
        // clipboard fallback (invariant 1) -- not a spawn of "".
        let err =
            ScriptInjector::new(&InjectConfig::default()).inject("hallo", None).unwrap_err();
        assert!(
            matches!(err, InjectError::NotConfigured),
            "expected NotConfigured, got {err:?}"
        );
        assert!(
            err.to_string().contains("[inject] script"),
            "the error must name the setting: {err}"
        );
    }

    #[test]
    fn a_script_that_exits_nonzero_is_reported_as_a_failure() {
        // The user's own error path: ydotoold not running, no /dev/uinput,
        // wl-copy missing. The backend must report it so the clipboard
        // fallback runs and the notification fires.
        let (dir, path) = script_fixture(
            "fails",
            r#"echo "no uinput" >&2
exit 3"#,
        );

        let err = ScriptInjector::new(&script_cfg(&path)).inject("hallo", None).unwrap_err();

        assert!(matches!(err, InjectError::Failed { .. }), "expected Failed, got {err:?}");
        assert!(err.to_string().contains("no uinput"), "stderr must survive into the error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_script_file_is_a_spawn_error_not_a_panic() {
        let cfg = InjectConfig {
            script: "/nonexistent/yappr/paste.sh".to_string(),
            ..InjectConfig::default()
        };
        let err = ScriptInjector::new(&cfg).inject("hallo", None).unwrap_err();
        assert!(matches!(err, InjectError::Spawn { .. }), "expected Spawn, got {err:?}");
    }

    #[test]
    fn a_paste_script_that_backgrounds_its_clipboard_restore_does_not_block() {
        // Invariant 6, in the exact shape this backend meets it.
        // `handy-paste.sh` ends with `nohup bash -c '... | wl-copy' &`, so a
        // grandchild outlives the script holding its stdout and stderr open.
        // Reading those pipes to EOF would wedge the pipeline thread at
        // INJECTING until that grandchild died -- which is the bug
        // `PIPE_DRAIN_GRACE` exists for. 5 s is far under the grandchild's
        // 30 s, so a regression here fails loudly rather than slowly.
        let (dir, path) = script_fixture("grandchild", "sleep 30 &\nexit 0");

        let started = std::time::Instant::now();
        ScriptInjector::new(&script_cfg(&path)).inject("hallo", None).unwrap();

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}; the grandchild's pipes blocked the drain",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_path_beginning_with_a_tilde_is_expanded() {
        // `~/bin/paste.sh` is what a user types into the settings field, and
        // `Command` does no expansion -- only a shell does, and there is no
        // shell here. Without this the field silently never works.
        let cfg = InjectConfig { script: "~/bin/paste.sh".to_string(), ..InjectConfig::default() };
        let err = ScriptInjector::new(&cfg).inject("hallo", None).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains('~'), "the tilde must be gone by the time we spawn: {msg}");
    }
}
