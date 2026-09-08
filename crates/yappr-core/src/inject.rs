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
    /// match on), or `None` off Hyprland / when nothing was focused. Only
    /// the ydotool backend reads it -- terminals paste with Ctrl+Shift+V --
    /// but it describes the *target*, so it travels with every injection.
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
        run_typer("wtype", wtype_argv(text, self.delay_ms), WTYPE_TIMEOUT)
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

/// How long the ydotool backend's single paste chord may take -- one key
/// event round trip to ydotoold, nowhere near what typing a whole
/// transcript would need.
const PASTE_KEY_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause between `wl-copy` returning and the paste chord being pressed.
/// `wl-copy` has forked and owns the selection when it exits, but the
/// compositor still has to hand the new offer to the focused client; paste
/// races that handoff without a small settle.
const PASTE_SETTLE: Duration = Duration::from_millis(100);
/// The paste chord for [`YdotoolInjector`], as raw `keycode:state` pairs
/// for `ydotool key`: LEFTCTRL (29) [+ LEFTSHIFT (42)] + V (47), pressed
/// and released in nested order. Raw keycodes on purpose -- key *positions*
/// are identical on QWERTZ and QWERTY, so this is the one ydotool
/// invocation a keyboard layout cannot mangle (`ydotool type`'s
/// char->keycode table is US-only, which is why this backend pastes instead
/// of typing). `terminal` selects Ctrl+Shift+V: terminals reserve plain
/// Ctrl+V for the application running inside them.
fn paste_key_argv(terminal: bool) -> Vec<String> {
    let mut argv = vec!["key".to_string(), "29:1".to_string()];
    if terminal {
        argv.push("42:1".to_string());
    }
    argv.push("47:1".to_string());
    argv.push("47:0".to_string());
    if terminal {
        argv.push("42:0".to_string());
    }
    argv.push("29:0".to_string());
    argv
}

/// Whether `class` names a terminal, per the configured
/// `inject.terminal_classes` list. Case-insensitive: Hyprland reports
/// "Alacritty" with a capital A, and nobody should have to know that.
fn is_terminal_class(class: Option<&str>, terminal_classes: &[String]) -> bool {
    let Some(class) = class else { return false };
    terminal_classes.iter().any(|t| t.eq_ignore_ascii_case(class))
}

/// Whether the paste chord carries Shift, given the configured
/// [`PasteChord`] and the target window's class.
///
/// The `Auto` arm is the class-based rule this backend has always used. The
/// two forced arms exist because `class` is `None` whenever `hyprctl` cannot
/// answer -- on GNOME/Mutter always, on Hyprland whenever the daemon's
/// environment lacks `HYPRLAND_INSTANCE_SIGNATURE` -- and an unknown class
/// under `Auto` means plain Ctrl+V, which no terminal accepts. See
/// [`PasteChord`] for the full account.
fn wants_shift(chord: PasteChord, class: Option<&str>, terminal_classes: &[String]) -> bool {
    match chord {
        PasteChord::CtrlV => false,
        PasteChord::CtrlShiftV => true,
        PasteChord::Auto => is_terminal_class(class, terminal_classes),
    }
}

/// Spec 10.3's second injector, for the surfaces `wtype` cannot reach
/// (GNOME/Mutter, XWayland, some Electron windows) -- since 2026-09-02 by
/// pasting rather than typing: `wl-copy` the transcript, then press one
/// Ctrl+V (Ctrl+Shift+V for terminals) via `ydotool key`. See
/// `InjectBackend::Ydotool` for why paste replaced `ydotool type` outright
/// (US-only keymap: z/y swapped, umlauts and ß dropped).
///
/// Unlike `wtype` this is not self-contained: it talks to a `ydotoold`
/// daemon over `$YDOTOOL_SOCKET`, and that daemon needs write access to
/// `/dev/uinput`. When either is missing, `ydotool` exits non-zero and the
/// clipboard fallback (spec 10.4) carries the transcript instead -- and
/// since the transcript was already copied here, that fallback amounts to
/// exactly the manual-paste story the clipboard backend offers (invariant 1
/// holds). Setting ydotoold up is the user's call, not the app's (same
/// reason `hypr.rs` prints a config block rather than applying one).
pub struct YdotoolInjector {
    terminal_classes: Vec<String>,
    paste_chord: PasteChord,
}

impl YdotoolInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        Self { terminal_classes: cfg.terminal_classes.clone(), paste_chord: cfg.paste_chord }
    }
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        ClipboardInjector.inject(text, target_class)?;
        std::thread::sleep(PASTE_SETTLE);
        let shift = wants_shift(self.paste_chord, target_class, &self.terminal_classes);
        if target_class.is_none() && self.paste_chord == PasteChord::Auto {
            // Not a failure -- `ydotool` will exit 0 and this function will
            // return `Ok` -- which is exactly why it has to be said out loud.
            // A terminal ignores the plain Ctrl+V that is about to be sent,
            // so the user gets no text and no error. Naming the override
            // here is the only warning they will ever see.
            tracing::warn!(
                "no target window class (no provider could name the focused window: \
                 no hyprctl, and no accessibility bus or an app not on it); \
                 pasting with plain Ctrl+V, which terminals ignore -- \
                 set [inject] paste_chord = \"ctrl_shift_v\" if you dictate into a terminal"
            );
        }
        run_typer("ydotool", paste_key_argv(shift), PASTE_KEY_TIMEOUT)
    }
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
    /// attributed to a specific backend (e.g. the ydotool environment
    /// report, which keys on the failing primary's name).
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
        cfg.backend = InjectBackend::Ydotool;
        assert_eq!(build(&cfg).name(), "ydotool");
        cfg.backend = InjectBackend::Clipboard;
        assert_eq!(build(&cfg).name(), "clipboard");
    }

    #[test]
    fn the_paste_chord_for_a_normal_window_is_ctrl_v_in_nested_order() {
        // 29 = KEY_LEFTCTRL, 47 = KEY_V -- raw keycodes, deliberately: key
        // POSITIONS are the same on QWERTZ and QWERTY, which is the whole
        // point of pasting (ydotool's own char->keycode table is US-only,
        // so `type` mangles German text; a single Ctrl+V does not).
        let argv = paste_key_argv(false);
        assert_eq!(argv, vec!["key", "29:1", "47:1", "47:0", "29:0"]);
    }

    #[test]
    fn the_paste_chord_for_a_terminal_adds_shift_in_nested_order() {
        // 42 = KEY_LEFTSHIFT. Terminals reserve plain Ctrl+V for the
        // application running inside them; their paste is Ctrl+Shift+V.
        let argv = paste_key_argv(true);
        assert_eq!(argv, vec!["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]);
    }

    #[test]
    fn an_unknown_window_class_falls_back_to_plain_ctrl_v_under_auto() {
        // The bug this pins: `hyprctl` is the only source of the target
        // class, so on GNOME/Mutter (where it does not exist -- and which is
        // the very desktop this backend exists for), on Hyprland whenever
        // `HYPRLAND_INSTANCE_SIGNATURE` is missing from the daemon's
        // environment (`hyprctl` then reports that on stdout and exits 1),
        // and on Hyprland with nothing focused (`{}`, exit 0), the class is
        // `None`. Under `Auto` that means "not a terminal", so every
        // terminal gets a plain Ctrl+V it ignores.
        let list = vec!["kitty".to_string()];
        assert!(!wants_shift(PasteChord::Auto, None, &list));
    }

    #[test]
    fn a_forced_ctrl_shift_v_pastes_into_a_terminal_whose_class_is_unknown() {
        // The fix: a user whose compositor cannot report a window class can
        // still say "I dictate into terminals", and get the chord terminals
        // actually accept.
        let list = vec!["kitty".to_string()];
        assert!(wants_shift(PasteChord::CtrlShiftV, None, &list));
        assert!(wants_shift(PasteChord::CtrlShiftV, Some("firefox"), &list));
    }

    #[test]
    fn a_forced_ctrl_v_never_adds_shift_even_for_a_known_terminal() {
        let list = vec!["kitty".to_string()];
        assert!(!wants_shift(PasteChord::CtrlV, Some("kitty"), &list));
        assert!(!wants_shift(PasteChord::CtrlV, None, &list));
    }

    #[test]
    fn auto_still_reads_the_class_when_one_is_available() {
        let list = vec!["kitty".to_string()];
        assert!(wants_shift(PasteChord::Auto, Some("kitty"), &list));
        assert!(!wants_shift(PasteChord::Auto, Some("firefox"), &list));
    }

    #[test]
    fn terminal_detection_matches_the_configured_classes_case_insensitively() {
        // Hyprland reports "Alacritty" with a capital A; the configured list
        // is lowercase. Nobody should have to know which spelling wins.
        let list = vec!["alacritty".to_string(), "org.wezfurlong.wezterm".to_string()];
        assert!(is_terminal_class(Some("Alacritty"), &list));
        assert!(is_terminal_class(Some("org.wezfurlong.wezterm"), &list));
        assert!(!is_terminal_class(Some("firefox"), &list));
        assert!(!is_terminal_class(None, &list), "no class means no shift");
    }

    #[test]
    fn the_target_window_class_travels_to_both_injectors() {
        // The class captured at ptt-start decides the ydotool backend's
        // paste chord, so inject_with_recovery must hand it to the primary
        // -- and to the fallback, which is an injector like any other.
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
}
