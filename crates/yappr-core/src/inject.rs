use std::io;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{InjectBackend, InjectConfig, PasteChord};
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
    run_prepared(backend, cmd, timeout)
}

/// [`run_backend`] for a caller that had to build the `Command` itself --
/// the ydotool backend, which sets `YDOTOOL_SOCKET` on it. Same timeout,
/// same error mapping, same logging; the only difference is who chose the
/// argv and the environment.
fn run_prepared(
    backend: &'static str,
    cmd: Command,
    timeout: Duration,
) -> Result<(), InjectError> {
    let argv: Vec<String> =
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
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
        // `ydotool` reports its own failures on **stdout**, not stderr --
        // "failed to connect socket `...': No such file or directory" arrives
        // there with stderr empty, exactly as `hyprctl` does (see
        // `hypr::window_class`). An error built from stderr alone is then a
        // bare exit code, which is what a real report of this backend failing
        // looked like: `ydotool exited with status exit status: 1: `. Prefer
        // stderr when there is any, fall back to stdout when there is not.
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(InjectError::Failed {
            backend,
            status: out.status.to_string(),
            stderr: detail,
        });
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

/// One key-event round trip to `ydotoold`; nowhere near what typing a whole
/// transcript would need.
const PASTE_KEY_TIMEOUT: Duration = Duration::from_secs(5);
/// Read the old clipboard before replacing it. Short on purpose: an empty
/// clipboard makes `wl-paste` block, and a missed restore is a far smaller
/// harm than a dictation that stalls.
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Between `wl-copy` returning and the chord. `wl-copy` has forked and owns
/// the selection when it exits, but the compositor still has to hand the new
/// offer to the focused client; the paste races that handoff without this.
const PASTE_SETTLE: Duration = Duration::from_millis(200);
/// Between the chord and restoring the previous clipboard, so the target has
/// actually read the offer before it is replaced again.
const RESTORE_DELAY: Duration = Duration::from_millis(300);

/// The paste chord as raw `keycode:state` pairs for `ydotool key`:
/// LEFTCTRL (29) [+ LEFTSHIFT (42)] + V (47), pressed and released in
/// nested order.
///
/// Raw keycodes on purpose -- key *positions* are identical on QWERTZ and
/// QWERTY, so this is the one ydotool invocation a keyboard layout cannot
/// mangle (`ydotool type`'s char->keycode table is US-only, which is why
/// this backend pastes instead of typing: on a German layout it swaps z/y
/// and drops umlauts and ß entirely).
fn paste_key_argv(shift: bool) -> Vec<String> {
    let mut v = vec!["-d".to_string(), "50".to_string(), "29:1".to_string()];
    if shift {
        v.push("42:1".to_string());
    }
    v.push("47:1".to_string());
    v.push("47:0".to_string());
    if shift {
        v.push("42:0".to_string());
    }
    v.push("29:0".to_string());
    v
}

/// Whether `class` names a terminal, per the configured
/// `inject.terminal_classes`. Case-insensitive: Hyprland reports
/// "Alacritty" with a capital A, and nobody should have to know that.
fn is_terminal_class(class: Option<&str>, terminal_classes: &[String]) -> bool {
    let Some(class) = class else { return false };
    terminal_classes.iter().any(|t| t.eq_ignore_ascii_case(class))
}

/// Whether the chord carries Shift.
///
/// **`Auto` treats an unknown class as a terminal**, and that inversion is
/// the entire reason this backend works where its predecessor did not. Every
/// window-class provider can answer `None` (see [`crate::winclass`]); the old
/// rule resolved `None` through the terminal list, came up `false`, and sent
/// a plain Ctrl+V that every terminal ignores -- from a `ydotool` that exits
/// 0, so nothing failed, no fallback fired and the user simply got no text.
///
/// Guessing shifted is the better bet in both directions: terminals *require*
/// Ctrl+Shift+V, while browsers and Electron apps read it as "paste as plain
/// text", which is what dictation wants anyway. A known non-terminal still
/// gets plain Ctrl+V, so the few apps that bind Ctrl+Shift+V to something
/// else (LibreOffice's Paste Special) are unaffected whenever the class is
/// actually available.
fn wants_shift(chord: PasteChord, class: Option<&str>, terminal_classes: &[String]) -> bool {
    match chord {
        PasteChord::CtrlV => false,
        PasteChord::CtrlShiftV => true,
        PasteChord::Auto => class.is_none() || is_terminal_class(class, terminal_classes),
    }
}

/// Where `ydotoold` might be listening, most likely first.
///
/// `$XDG_RUNTIME_DIR/.ydotool_socket` leads because it is both `ydotool`'s
/// own compiled-in default *and* what the usual user unit resolves to:
/// `--socket-path=%t/.ydotool_socket` expands `%t` to `$XDG_RUNTIME_DIR` for
/// a user manager (`man 5 systemd.unit`), which reads like `$HOME` and is
/// not. `~/.ydotool_socket` is second because some setups really do put it
/// there.
fn ydotool_socket_candidates() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        v.push(std::path::PathBuf::from(rt).join(".ydotool_socket"));
    }
    if let Some(home) = dirs::home_dir() {
        v.push(home.join(".ydotool_socket"));
    }
    v
}

/// The `YDOTOOL_SOCKET` to hand the child, or `None` to hand it nothing.
///
/// Three outcomes, and the third is the one that matters:
///
///   - the environment already names a socket -> `None`, leave it alone;
///   - it does not, and one of `candidates` **exists** -> use that, because a
///     GUI process started by the desktop inherits none of what the user's
///     shell exports;
///   - it does not, and none exists -> `None`. Guessing here is worse than
///     silence: `ydotool` has a sensible default of its own, and overriding
///     it with a path that is merely plausible turns "ydotoold is not
///     running" into "no such file", which is a different and more confusing
///     failure.
///
/// That last case is not hypothetical. This function used to return
/// `~/.ydotool_socket` unconditionally, lifted from a user paste script's
/// `${YDOTOOL_SOCKET:-$HOME/.ydotool_socket}`. A shell fallback for one
/// person's machine is not a default an app may impose: on a box whose
/// `ydotoold` ran from the ordinary user unit it pointed `ydotool` at a
/// socket that did not exist, and every dictation fell through to the
/// clipboard.
fn ydotool_socket_in(
    existing: Option<&str>,
    candidates: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    if existing.is_some_and(|v| !v.is_empty()) {
        return None;
    }
    candidates.iter().find(|p| p.exists()).cloned()
}

fn ydotool_socket(existing: Option<&str>) -> Option<std::path::PathBuf> {
    ydotool_socket_in(existing, &ydotool_socket_candidates())
}

/// Spec 10.3's second injector: **paste via `ydotool`**, rebuilt on
/// 2026-09-10 as the sequence a working user script arrived at.
///
/// The previous version of this backend was removed on 2026-09-09 because it
/// never worked for anyone; this one is that script's steps, in order, run
/// from Rust instead of `sh`:
///
///   1. read the current clipboard, so it can be put back (`wl-paste`);
///   2. put the transcript on the clipboard (`wl-copy`);
///   3. settle, so the compositor hands the offer over;
///   4. name the focused window -- [`crate::winclass`] first (`hyprctl`, then
///      the accessibility bus), which needs no GNOME extension;
///   5. press Ctrl+V, or **Ctrl+Shift+V when the class is a terminal *or
///      unknown*** -- see [`wants_shift`], this is the fix;
///   6. restore the previous clipboard, detached, after a delay.
///
/// Step 6 is deliberately fire-and-forget: it outlives this call, so the
/// pipeline is not held at `INJECTING` waiting for a clipboard the user is
/// no longer looking at. Its pipes are `/dev/null`, so unlike a shell
/// script's `nohup … &` it cannot hold a drain open at all (invariant 6).
///
/// A failure at any step is an `Err`, which `inject_with_recovery` turns into
/// the clipboard fallback -- and since step 2 already ran, that fallback is
/// exactly the manual-paste story the `clipboard` backend offers. Invariant 1
/// holds either way.
pub struct YdotoolInjector {
    terminal_classes: Vec<String>,
    paste_chord: PasteChord,
    restore_clipboard: bool,
}

impl YdotoolInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        Self {
            terminal_classes: cfg.terminal_classes.clone(),
            paste_chord: cfg.paste_chord,
            restore_clipboard: cfg.restore_clipboard,
        }
    }

    /// Step 1. Best-effort: a clipboard we could not read is one we simply
    /// do not restore, which must never fail the dictation.
    fn read_clipboard() -> Option<Vec<u8>> {
        let mut cmd = Command::new("wl-paste");
        cmd.arg("--no-newline");
        let out = procutil::run_with_timeout(cmd, CLIPBOARD_READ_TIMEOUT, None).ok()?;
        (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
    }

    /// Step 6. Detached, silent, and after `RESTORE_DELAY`.
    ///
    /// A thread rather than a `nohup`-style grandchild, so nothing can hold
    /// a drained pipe open (invariant 6). The trade is that it dies with the
    /// process: quit yappr inside `RESTORE_DELAY` of a dictation and the
    /// clipboard simply keeps the transcript, which is the harmless
    /// direction to fail in.
    fn restore_clipboard_later(previous: Vec<u8>) {
        std::thread::spawn(move || {
            std::thread::sleep(RESTORE_DELAY);
            let mut cmd = Command::new("wl-copy");
            procutil::unbundle(&mut cmd);
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let Ok(mut child) = cmd.spawn() else { return };
            if let Some(mut si) = child.stdin.take() {
                use std::io::Write as _;
                let _ = si.write_all(&previous);
            }
            // `wl-copy` forks a daemon to own the selection and the parent
            // exits at once; reap that parent so it does not linger as a
            // zombie for the life of the app.
            let _ = child.wait();
        });
    }
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        let previous = self.restore_clipboard.then(Self::read_clipboard).flatten();

        ClipboardInjector.inject(text, target_class)?;
        std::thread::sleep(PASTE_SETTLE);

        // `target_class` is the class captured at `ptt-start`, and it is the
        // only one worth having. Asking `winclass` again *here* looks like a
        // free improvement and is actively harmful: by this point the overlay
        // may hold keyboard focus itself (invariant 2), so the answer can be
        // yappr's own window -- which is not in `terminal_classes`, so a
        // dictation into a terminal would resolve to plain Ctrl+V and vanish.
        // That is precisely the failure this backend was rebuilt to end, so
        // an absent class stays absent and `wants_shift` shifts it.
        let shift = wants_shift(self.paste_chord, target_class, &self.terminal_classes);
        tracing::debug!(class = ?target_class, shift, "ydotool paste chord chosen");

        let mut cmd = Command::new("ydotool");
        cmd.args(paste_key_argv(shift));
        if let Some(sock) = ydotool_socket(std::env::var("YDOTOOL_SOCKET").ok().as_deref()) {
            cmd.env("YDOTOOL_SOCKET", sock);
        }
        let res = run_prepared("ydotool", cmd, PASTE_KEY_TIMEOUT);

        if let Some(previous) = previous {
            Self::restore_clipboard_later(previous);
        }
        res
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
        InjectBackend::Ydotool => Box::new(YdotoolInjector::new(cfg)),
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

    /// `ScriptInjector::inject`, retrying only on `ETXTBSY`.
    ///
    /// Exec'ing a script this process wrote moments ago races every *other*
    /// thread in the same process that is spawning a child: between a
    /// sibling's `fork` and its `exec`, that child holds an inherited
    /// (`O_CLOEXEC`, but not yet closed) write descriptor on our fresh file,
    /// and the kernel answers our exec with `ETXTBSY`. It is transient,
    /// microseconds wide, and entirely an artifact of writing the fixture
    /// in-process -- `cargo test --workspace` runs enough subprocess tests
    /// beside these to hit it perhaps one run in ten.
    ///
    /// Retried here rather than in `ScriptInjector` on purpose: a real user's
    /// paste script is not being rewritten while yappr runs it, so absorbing
    /// this in production code would only hide a genuinely broken script
    /// behind a delay. `ENOENT` and every other spawn failure are returned
    /// untouched, which is what
    /// `a_missing_script_file_is_a_spawn_error_not_a_panic` pins.
    fn inject_script(
        cfg: &InjectConfig,
        text: &str,
        class: Option<&str>,
    ) -> Result<(), InjectError> {
        /// `ETXTBSY` on Linux. Spelled out rather than pulled from `libc`,
        /// which this crate does not depend on.
        const ETXTBSY: i32 = 26;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let out = ScriptInjector::new(cfg).inject(text, class);
            let busy = matches!(
                &out,
                Err(InjectError::Spawn { source, .. }) if source.raw_os_error() == Some(ETXTBSY)
            );
            if !busy || std::time::Instant::now() >= deadline {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
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

        inject_script(&script_cfg(&path), "hallo welt", Some("kitty")).unwrap();

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

        inject_script(&script_cfg(&path), "-n --version", None).unwrap();

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

        let err = inject_script(&script_cfg(&path), "hallo", None).unwrap_err();

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
        inject_script(&script_cfg(&path), "hallo", None).unwrap();

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

    // ---- the built-in ydotool backend -----------------------------------

    #[test]
    fn an_unknown_window_class_pastes_with_ctrl_shift_v() {
        // THE bug that made the previous ydotool backend useless, pinned so
        // it cannot come back. `wants_shift` used to resolve `None` through
        // the terminal list, which does not contain `None`, so an unknown
        // class meant plain Ctrl+V -- which every terminal ignores, from a
        // `ydotool` that exits 0. Nothing failed, nothing was logged, and
        // the user got no text.
        //
        // Unknown is now the *shifted* case. Ctrl+Shift+V is what the
        // reference script sends when its class lookup comes up empty, and
        // it is the safer guess in both directions: terminals require it,
        // and browsers and Electron read it as "paste as plain text", which
        // is what dictation wants anyway.
        assert!(wants_shift(PasteChord::Auto, None, &["kitty".to_string()]));
    }

    #[test]
    fn a_known_non_terminal_still_pastes_with_plain_ctrl_v() {
        // The shifted default must not swallow the case we *can* answer:
        // a named window that is not a terminal gets Ctrl+V, so an app
        // where Ctrl+Shift+V means something else (LibreOffice's Paste
        // Special) is unaffected whenever the class is actually known.
        assert!(!wants_shift(PasteChord::Auto, Some("firefox"), &["kitty".to_string()]));
    }

    #[test]
    fn a_known_terminal_pastes_with_ctrl_shift_v() {
        assert!(wants_shift(PasteChord::Auto, Some("kitty"), &["kitty".to_string()]));
    }

    #[test]
    fn terminal_detection_matches_the_configured_classes_case_insensitively() {
        // Hyprland reports "Alacritty" with a capital A and nobody should
        // have to know that.
        let classes = ["alacritty".to_string(), "org.gnome.Ptyxis".to_string()];
        assert!(is_terminal_class(Some("Alacritty"), &classes));
        assert!(is_terminal_class(Some("org.gnome.ptyxis"), &classes));
        assert!(!is_terminal_class(Some("firefox"), &classes));
    }

    #[test]
    fn a_forced_chord_overrides_the_class_in_both_directions() {
        assert!(wants_shift(PasteChord::CtrlShiftV, Some("firefox"), &[]));
        assert!(!wants_shift(PasteChord::CtrlV, Some("kitty"), &["kitty".to_string()]));
    }

    #[test]
    fn the_paste_chord_is_pressed_and_released_in_nested_order() {
        // Raw keycodes, because key *positions* are layout-independent:
        // this is the one ydotool invocation a German keyboard cannot
        // mangle. LEFTCTRL 29, LEFTSHIFT 42, V 47. Nested, not sequential --
        // releasing Ctrl before V would be a different chord.
        assert_eq!(paste_key_argv(false), vec!["-d", "50", "29:1", "47:1", "47:0", "29:0"]);
        assert_eq!(
            paste_key_argv(true),
            vec!["-d", "50", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]
        );
    }

    #[test]
    fn the_default_terminal_list_covers_the_terminals_the_reference_script_named() {
        // The built-in backend replaced a shell script whose terminal regex
        // is the list a real user arrived at; dropping a name from it is a
        // regression for whoever dictates into that terminal.
        let d = crate::config::InjectConfig::default().terminal_classes;
        for class in ["ptyxis", "kitty", "alacritty", "foot", "ghostty", "konsole", "wezterm"] {
            assert!(d.iter().any(|t| t == class), "{class} missing from {d:?}");
        }
    }

    #[test]
    fn an_explicit_ydotool_socket_is_never_overridden() {
        let (dir, _) = script_fixture("sock-explicit", "true");
        let existing = dir.join(".ydotool_socket");
        std::fs::write(&existing, b"").unwrap();
        assert_eq!(ydotool_socket_in(Some("/somewhere/else"), &[existing]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_socket_that_actually_exists_is_chosen() {
        // Order matters: `$XDG_RUNTIME_DIR` first, because that is both
        // ydotool's own default and what a user unit's `%t` resolves to.
        let (dir, _) = script_fixture("sock-pick", "true");
        let missing = dir.join("missing.sock");
        let present = dir.join("present.sock");
        std::fs::write(&present, b"").unwrap();
        assert_eq!(
            ydotool_socket_in(None, &[missing.clone(), present.clone()]),
            Some(present)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_socket_anywhere_means_we_impose_nothing() {
        // The regression this replaced: returning `~/.ydotool_socket` on a
        // machine whose ydotoold listens in `$XDG_RUNTIME_DIR` pointed
        // `ydotool` at a path that did not exist, so every dictation fell
        // through to the clipboard with "exit status: 1" and no reason.
        // Handing over nothing lets ydotool use its own default and report
        // its own, accurate error.
        let (dir, _) = script_fixture("sock-none", "true");
        let a = dir.join("nope-a.sock");
        let b = dir.join("nope-b.sock");
        assert_eq!(ydotool_socket_in(None, &[a, b]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backend_that_reports_its_failure_on_stdout_still_gets_a_readable_error() {
        // ydotool and hyprctl both do this. stderr-only error building turned
        // "failed to connect socket ...: No such file or directory" into a
        // bare "exited with status exit status: 1: ".
        let (dir, path) = script_fixture(
            "stdout-err",
            r#"echo "failed to connect socket: No such file or directory"
exit 1"#,
        );
        let err = inject_script(&script_cfg(&path), "hallo", None).unwrap_err();
        assert!(
            err.to_string().contains("failed to connect socket"),
            "stdout must reach the error when stderr is empty: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
