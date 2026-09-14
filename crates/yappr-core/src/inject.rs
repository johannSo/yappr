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
    #[error("{backend} exited with status {status}: {output}")]
    Failed { backend: &'static str, status: String, output: String },
    #[error("could not run {backend}: {source}")]
    Spawn { backend: &'static str, source: std::io::Error },
    #[error("{backend} did not respond within {timeout:?}")]
    Timeout { backend: &'static str, timeout: Duration },
    /// A backend that talks to a service rather than spawning a program, so
    /// there is no exit status to report and no stream the diagnosis came in
    /// on -- only what the service said. The `libei` backend's whole failure
    /// surface (see [`crate::libei::PortalError`]).
    #[error("{backend} could not use the desktop portal: {detail}")]
    Portal { backend: &'static str, detail: String },
    #[error("mock injector configured to fail")]
    Mock,
    #[error("no paste script configured -- set [inject] script to an executable path")]
    NotConfigured,
}

/// What a failed backend actually said, from whichever stream it said it on.
///
/// Reading stderr alone is not enough: `ydotool` reports "failed to connect
/// socket `...`: No such file or directory / Please check if ydotoold is
/// running." on **stdout** and exits 2, exactly the habit `hyprctl` has
/// (see `hypr::window_class`, which reads stdout before the status check
/// for the same reason). That left the debug record's `primary_error` as
/// `ydotool exited with status exit status: 2: ` -- a failure naming
/// neither its cause nor anything to fix, measured on Fedora 44 against a
/// `ydotoold` listening on a non-default socket path.
///
/// stderr comes first, so a backend that reports failures the usual way
/// reads exactly as it always did.
fn diagnostic(stdout: &str, stderr: &str) -> String {
    [stderr, stdout].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("; ")
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
    /// match on), or `None` when no provider could name it. The ydotool
    /// backend reads it to pick its paste chord (terminals paste with
    /// Ctrl+Shift+V); the others do not, but it describes the *target*, the
    /// style rules resolve against it, `MockInjector` records it and the
    /// debug record writes it, so it travels with every injection.
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
        return Err(InjectError::Failed {
            backend,
            status: out.status.to_string(),
            output: diagnostic(&stdout, &stderr),
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
                output: diagnostic(
                    String::from_utf8_lossy(&out.stdout).trim(),
                    String::from_utf8_lossy(&out.stderr).trim(),
                ),
            });
        }
        Ok(())
    }
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

/// Pause between the paste chord being delivered and the previous clipboard
/// contents being written back (`[inject] restore_clipboard`).
///
/// The chord returns once the key events have been *sent*; the target
/// application still has to receive them, ask the compositor for the
/// selection and read it. Restoring inside that window hands it the old text
/// instead of the dictation, which is the one way this feature can go wrong
/// and is invisible from here -- nothing reports which bytes a client read.
/// 300 ms is what `handy-paste.sh`, the reference paste script this
/// behaviour is borrowed from, sleeps before doing the same thing;
/// `restore_clipboard = false` is the way out for an application slower
/// than that.
const CLIPBOARD_RESTORE_SETTLE: Duration = Duration::from_millis(300);

/// Above this the previous clipboard is left alone rather than carried
/// through memory and pushed back down a pipe. A dictation is not the moment
/// to copy someone's 200 MB screenshot twice, and the cost of skipping is
/// one manual re-copy of something they still have open.
const CLIPBOARD_SNAPSHOT_MAX: usize = 4 * 1024 * 1024;

/// What the clipboard held before a transcript was staged there, in the one
/// MIME type it will be handed back as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardSnapshot {
    mime: String,
    bytes: Vec<u8>,
}

/// Whether `mime` names text, for the two places the answer changes what is
/// done with it: `wl-paste` appends a newline to text output (and only text
/// output), and a text snapshot is the one that has to survive a round trip
/// byte-for-byte. The bare X11 atom names are in the list because
/// `wl-paste --list-types` reports them for anything an XWayland client
/// copied.
fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || ["utf8_string", "string", "text"].contains(&mime.to_ascii_lowercase().as_str())
}

/// Which of the offered MIME types to snapshot, given `wl-paste
/// --list-types`' output.
///
/// Text first, then whatever was listed first -- which is `wl-paste`'s own
/// rule when it is given no `--type`, so the snapshot is exactly what a
/// manual Ctrl+V would have produced. Only one type is taken: an offer
/// carries several (an HTML selection lists `text/html` *and* `text/plain`)
/// and yappr has no way to re-offer all of them, so the one a paste would
/// have used is the honest reconstruction.
fn preferred_mime(list_types: &str) -> Option<String> {
    let types: Vec<&str> =
        list_types.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let exact = |want: &str| types.iter().find(|t| t.eq_ignore_ascii_case(want)).copied();
    exact("text/plain;charset=utf-8")
        .or_else(|| exact("text/plain"))
        .or_else(|| types.iter().find(|t| is_text_mime(t)).copied())
        .or_else(|| types.first().copied())
        .map(str::to_string)
}

/// Runs `wl-paste` under I3's timeout and returns its stdout, or `None` for
/// every way it can decline to answer.
///
/// All of those ways are ordinary: an empty clipboard exits non-zero with
/// "No selection", and a desktop without `wl-clipboard` installed cannot
/// spawn it at all. Neither is a dictation failure -- the transcript is
/// already in the clipboard by the time this matters -- so they are logged
/// at debug and the restore is simply skipped. A warning here would fire on
/// every dictation for a user who has nothing copied.
fn wl_paste(args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("wl-paste");
    cmd.args(args);
    let out = match procutil::run_with_timeout(cmd, CLIPBOARD_TIMEOUT, None) {
        Ok(out) => out,
        Err(e) => {
            tracing::debug!(?args, error = %e, "wl-paste did not run; clipboard not restored");
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            ?args,
            status = %out.status,
            output = %diagnostic(
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            "wl-paste declined; clipboard not restored"
        );
        return None;
    }
    Some(out.stdout)
}

/// Reads the clipboard as it stands, before a transcript is staged over it.
///
/// `None` means "restore nothing", which leaves the transcript in the
/// clipboard -- the behaviour every one of these backends had before the
/// restore existed.
fn clipboard_snapshot() -> Option<ClipboardSnapshot> {
    let listed = wl_paste(&["--list-types"])?;
    let mime = preferred_mime(&String::from_utf8_lossy(&listed))?;
    // `--no-newline` for text only: `wl-paste` appends one to text output,
    // and without this every dictation would grow the restored entry by a
    // newline. Binary data is taken as it comes.
    let mut args = vec!["--type", mime.as_str()];
    if is_text_mime(&mime) {
        args.push("--no-newline");
    }
    let bytes = wl_paste(&args)?;
    if bytes.is_empty() {
        return None;
    }
    if bytes.len() > CLIPBOARD_SNAPSHOT_MAX {
        tracing::debug!(
            bytes = bytes.len(),
            limit = CLIPBOARD_SNAPSHOT_MAX,
            %mime,
            "clipboard contents too large to put back; leaving the transcript in place"
        );
        return None;
    }
    Some(ClipboardSnapshot { mime, bytes })
}

/// Writes a snapshot back, which is what pushes the user's own entry to the
/// top of their clipboard history again and leaves the transcript one step
/// down in it.
///
/// The type is passed explicitly rather than letting `wl-copy` infer one
/// from the bytes: inference reads the *content*, so a restored shell
/// snippet or XML document would come back under a different MIME type than
/// it went in with.
///
/// Failure is logged and swallowed. The paste has already happened by the
/// time this runs -- the user has their dictation -- so nothing here may
/// turn a successful injection into a clipboard fallback.
fn clipboard_restore(snapshot: &ClipboardSnapshot) {
    let mut cmd = Command::new("wl-copy");
    cmd.arg("--type").arg(&snapshot.mime);
    match procutil::run_with_timeout(cmd, CLIPBOARD_TIMEOUT, Some(&snapshot.bytes)) {
        Ok(out) if out.status.success() => tracing::debug!(
            mime = %snapshot.mime,
            bytes = snapshot.bytes.len(),
            "previous clipboard contents restored"
        ),
        Ok(out) => tracing::warn!(
            mime = %snapshot.mime,
            status = %out.status,
            output = %diagnostic(
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            "could not put the previous clipboard contents back; the transcript is still there"
        ),
        Err(e) => tracing::warn!(
            mime = %snapshot.mime,
            error = %e,
            "could not put the previous clipboard contents back; the transcript is still there"
        ),
    }
}

/// The three clipboard operations a chord-pressing backend performs, behind
/// a trait for exactly one reason: the ordering below is the feature, and a
/// test of it must not swap out what the developer running `cargo test` has
/// copied.
trait Clipboard {
    fn snapshot(&self) -> Option<ClipboardSnapshot>;
    fn stage(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError>;
    fn restore(&self, snapshot: &ClipboardSnapshot);
}

/// The real one: `wl-paste` to read, [`ClipboardInjector`] to write.
struct WlClipboard;

impl Clipboard for WlClipboard {
    fn snapshot(&self) -> Option<ClipboardSnapshot> {
        clipboard_snapshot()
    }

    fn stage(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        ClipboardInjector.inject(text, target_class)
    }

    fn restore(&self, snapshot: &ClipboardSnapshot) {
        clipboard_restore(snapshot);
    }
}

/// The shared body of the two backends that paste rather than type: read
/// what the clipboard holds, stage the transcript over it, let the
/// compositor hand the new offer to the focused client, press `press`, and
/// then put the old contents back.
///
/// The order is the whole of it, which is why this is one function rather
/// than a copy in each backend. Three parts of it are decisions:
///
///   - **The snapshot is taken before staging**, because staging is what
///     destroys it. A restore-capable paste script has to do the same thing
///     in the same order (see [`ScriptInjector`], which is why yappr does
///     not pre-copy for it).
///   - **A failed press restores nothing.** The transcript staying in the
///     clipboard *is* the fallback the user is about to be notified about
///     (invariant 1); handing the old contents back would take it away.
///   - **An empty clipboard restores nothing either** -- `snapshot` answers
///     `None` -- because the alternative is `wl-copy --clear`, which would
///     leave a user with no clipboard-history manager nothing to paste.
///
/// `settle` and `restore_settle` are parameters rather than the two consts
/// directly so the ordering test does not have to sleep through them.
fn paste_through_clipboard(
    clipboard: &dyn Clipboard,
    text: &str,
    target_class: Option<&str>,
    restore_previous: bool,
    settle: Duration,
    restore_settle: Duration,
    press: &dyn Fn() -> Result<(), InjectError>,
) -> Result<(), InjectError> {
    let previous = if restore_previous { clipboard.snapshot() } else { None };
    clipboard.stage(text, target_class)?;
    std::thread::sleep(settle);
    let pressed = press();
    if pressed.is_ok() {
        if let Some(previous) = previous {
            std::thread::sleep(restore_settle);
            clipboard.restore(&previous);
        }
    }
    pressed
}

/// The warning both chord-pressing backends emit when they are about to
/// press a chord chosen from a window class nobody could name.
///
/// Verbatim in both because it is the same hole with the same consequence:
/// `ydotool` exits 0 and the portal reports success, so the plain Ctrl+V a
/// terminal ignores produces no text and no error. This warning is the only
/// signal there is, and it is what retired the ydotool backend once already.
fn warn_if_the_chord_was_guessed(chord: PasteChord, target_class: Option<&str>) {
    if target_class.is_none() && chord == PasteChord::Auto {
        tracing::warn!(
            "no target window class (no provider could name the focused window: \
             no hyprctl, and no accessibility bus or an app not on it); \
             pasting with plain Ctrl+V, which terminals ignore -- \
             set [inject] paste_chord = \"ctrl_shift_v\" if you dictate into a terminal"
        );
    }
}

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
/// two forced arms exist because `class` is `None` whenever no provider can
/// name the focused window -- see [`crate::winclass`] for the four ways
/// that happens -- and an unknown class under `Auto` means plain Ctrl+V,
/// which no terminal accepts. See [`PasteChord`] for the full account.
fn wants_shift(chord: PasteChord, class: Option<&str>, terminal_classes: &[String]) -> bool {
    match chord {
        PasteChord::CtrlV => false,
        PasteChord::CtrlShiftV => true,
        PasteChord::Auto => is_terminal_class(class, terminal_classes),
    }
}

/// Spec 10.3's paste injector, for the surfaces `wtype` cannot reach
/// (GNOME/Mutter, XWayland, some Electron windows): `wl-copy` the
/// transcript, then press one Ctrl+V (Ctrl+Shift+V for terminals) via
/// `ydotool key`. See `InjectBackend::Ydotool` for why it pastes rather
/// than running `ydotool type` (US-only keymap: z/y swapped, umlauts and ß
/// dropped), and for the 2026-09-09 retirement this restores.
///
/// Unlike `wtype` this is not self-contained: it talks to a `ydotoold`
/// daemon over `$YDOTOOL_SOCKET` (else `$XDG_RUNTIME_DIR/.ydotool_socket`),
/// and that daemon needs write access to `/dev/uinput`. When either is
/// missing, `ydotool` exits non-zero and the clipboard fallback (spec 10.4)
/// carries the transcript instead -- and since the transcript was already
/// copied here, that fallback amounts to exactly the manual-paste story the
/// clipboard backend offers (invariant 1 holds). Setting ydotoold up is the
/// user's call, not the app's (same reason `hypr.rs` prints a config block
/// rather than applying one).
///
/// Unless `[inject] restore_clipboard` is off, what the clipboard held
/// before the dictation is read first and written back once the paste has
/// landed -- see [`paste_through_clipboard`], which is the shared body of
/// this backend and [`LibeiInjector`].
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
}

impl TextInjector for YdotoolInjector {
    fn name(&self) -> &'static str {
        "ydotool"
    }

    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        let shift = wants_shift(self.paste_chord, target_class, &self.terminal_classes);
        warn_if_the_chord_was_guessed(self.paste_chord, target_class);
        paste_through_clipboard(
            &WlClipboard,
            text,
            target_class,
            self.restore_clipboard,
            PASTE_SETTLE,
            CLIPBOARD_RESTORE_SETTLE,
            &|| {
                run_backend(
                    "ydotool",
                    std::ffi::OsStr::new("ydotool"),
                    paste_key_argv(shift),
                    PASTE_KEY_TIMEOUT,
                )
            },
        )
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


/// The same paste [`YdotoolInjector`] performs, pressed through the XDG
/// Desktop Portal's RemoteDesktop interface rather than a `ydotool` daemon:
/// `wl-copy` the transcript, settle, then one Ctrl+V -- Ctrl+Shift+V when
/// [`wants_shift`] says the target is a terminal.
///
/// Everything interesting about it is in [`crate::libei`]: the session is
/// created once for the whole process, before any dictation, and held there
/// rather than here, because `Request::SetConfig` rebuilds this struct on
/// every settings autosave.
///
/// This is a thin thing on purpose. It reads `paste_chord` and
/// `terminal_classes` with the same [`wants_shift`] the ydotool backend
/// uses, and it inherits that backend's one real hole with them -- see the
/// warning in [`LibeiInjector::inject`].
pub struct LibeiInjector {
    terminal_classes: Vec<String>,
    paste_chord: PasteChord,
    restore_clipboard: bool,
}

impl LibeiInjector {
    /// Builds the injector and **starts no portal session**. `inject::build`
    /// runs in `cargo test` (`build_selects_the_configured_backend`), and a
    /// unit test that raises an approval dialog on the developer's desktop
    /// is not one. Establishing is `libei::prewarm_if_selected`'s job, from
    /// the places a user has actually chosen this backend -- see
    /// [`crate::libei::start`].
    pub fn new(cfg: &InjectConfig) -> Self {
        Self {
            terminal_classes: cfg.terminal_classes.clone(),
            paste_chord: cfg.paste_chord,
            restore_clipboard: cfg.restore_clipboard,
        }
    }
}

impl TextInjector for LibeiInjector {
    fn name(&self) -> &'static str {
        // What `debug.rs`, `InjectOutcome` and the fallback notification
        // record, and the spelling `config.toml` uses. The transport is the
        // RemoteDesktop portal rather than libei proper; the name is the
        // setting's, so a bug report and a config file say the same word.
        "libei"
    }

    fn inject(&self, text: &str, target_class: Option<&str>) -> Result<(), InjectError> {
        let shift = wants_shift(self.paste_chord, target_class, &self.terminal_classes);
        // The same hole `YdotoolInjector` warns about, for the same reason
        // and with the same consequence: the portal delivers the plain
        // Ctrl+V and reports success, a terminal ignores it, and the user
        // gets no text and no error.
        warn_if_the_chord_was_guessed(self.paste_chord, target_class);
        paste_through_clipboard(
            &WlClipboard,
            text,
            target_class,
            self.restore_clipboard,
            PASTE_SETTLE,
            CLIPBOARD_RESTORE_SETTLE,
            &|| {
                crate::libei::press_chord(shift).map_err(|e| match e {
                    crate::libei::PortalError::Timeout(timeout) => {
                        InjectError::Timeout { backend: "libei", timeout }
                    }
                    crate::libei::PortalError::Failed(detail) => {
                        InjectError::Portal { backend: "libei", detail }
                    }
                })
            },
        )
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
        InjectBackend::Libei => Box::new(LibeiInjector::new(cfg)),
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
    let mut f =
        std::fs::OpenOptions::new().create(true).append(true).open(state_dir.join("unsent.txt"))?;
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
    use std::ffi::OsStr;

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
    fn a_backend_that_reports_its_failure_on_stdout_still_names_the_cause() {
        // What a paste script's `ydotool` does when it cannot reach
        // `ydotoold`: the message goes to stdout, exit is 2, stderr is
        // empty. Reading stderr alone recorded `script exited with status
        // exit status: 2: ` and named no cause at all.
        let argv = vec![
            "-c".to_string(),
            "echo 'Please check if ydotoold is running.'; exit 2".to_string(),
        ];
        let err =
            run_backend("script", OsStr::new("sh"), argv, Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("ydotoold is running"), "{err}");
    }

    #[test]
    fn a_backend_that_reports_its_failure_on_stderr_reads_as_it_always_did() {
        let argv = vec!["-c".to_string(), "echo 'compositor said no' >&2; exit 1".to_string()];
        let err =
            run_backend("script", OsStr::new("sh"), argv, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.to_string(), "script exited with status exit status: 1: compositor said no");
    }

    #[test]
    fn a_backend_that_uses_both_streams_loses_neither() {
        assert_eq!(diagnostic("on stdout", "on stderr"), "on stderr; on stdout");
        assert_eq!(diagnostic("on stdout", ""), "on stdout");
        assert_eq!(diagnostic("", "on stderr"), "on stderr");
        assert_eq!(diagnostic("", ""), "");
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
        cfg.backend = InjectBackend::Ydotool;
        assert_eq!(build(&cfg).name(), "ydotool");
        cfg.backend = InjectBackend::Script;
        assert_eq!(build(&cfg).name(), "script");
        cfg.backend = InjectBackend::Libei;
        assert_eq!(build(&cfg).name(), "libei");
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
        // The hole this backend was retired over, pinned rather than fixed:
        // `winclass` can answer `None` (no hyprctl and no accessibility
        // bus, a Hyprland session without HYPRLAND_INSTANCE_SIGNATURE,
        // nothing focused, an Electron/Qt app on no bus), and under `Auto`
        // that reads as "not a terminal", so a terminal gets a plain Ctrl+V
        // it ignores. `YdotoolInjector::inject` warns on exactly this
        // combination and `paste_chord` is how a user closes it.
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
    fn the_libei_backend_decides_its_chord_with_the_same_rule_as_ydotool() {
        // Two backends, two wire encodings of the same chord -- evdev
        // keycodes for `ydotool key`, X11 keysyms for the portal -- and one
        // decision, `wants_shift`. Nothing would notice them drifting apart
        // except a user whose terminal silently stopped receiving pastes
        // after switching backend, so the agreement is pinned rather than
        // assumed.
        let list = vec!["kitty".to_string()];
        for chord in [PasteChord::Auto, PasteChord::CtrlV, PasteChord::CtrlShiftV] {
            for class in [None, Some("kitty"), Some("firefox")] {
                let shift = wants_shift(chord, class, &list);
                let ydotool_has_shift = paste_key_argv(shift).contains(&"42:1".to_string());
                let libei_has_shift = crate::libei::paste_keysyms(shift)
                    .iter()
                    .any(|(keysym, pressed)| *keysym == 0xffe1 && *pressed);
                assert_eq!(
                    ydotool_has_shift, libei_has_shift,
                    "chord {chord:?} into {class:?} must press the same modifiers on both backends"
                );
                assert_eq!(libei_has_shift, shift);
            }
        }
    }

    #[test]
    fn a_portal_failure_names_the_backend_and_what_the_portal_said() {
        // The `libei` analogue of `diagnostic`: there is no exit status and
        // no stream to read, so the D-Bus error text is the entire account
        // of what went wrong and has to survive into the debug record.
        let err = InjectError::Portal {
            backend: "libei",
            detail: "Session creation inhibited".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("libei"), "{msg}");
        assert!(msg.contains("Session creation inhibited"), "{msg}");
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
    fn the_shipped_terminal_list_covers_the_terminals_the_class_providers_report() {
        // The default matters more than it looks: under `Auto` a terminal
        // missing from this list gets a plain Ctrl+V it ignores, which is a
        // dictation that produces no text and no error. `ptyxis` is the
        // AT-SPI name GNOME's default terminal arrives under (see
        // `gnome.rs`), `ghostty` the one its pid lookup resolves to.
        let list = InjectConfig::default().terminal_classes;
        for t in ["kitty", "foot", "alacritty", "ghostty", "ptyxis", "org.gnome.terminal"] {
            assert!(is_terminal_class(Some(t), &list), "{t} must be a known terminal");
        }
        assert!(!is_terminal_class(Some("firefox"), &list));
    }

    #[test]
    fn the_target_window_class_travels_to_both_injectors() {
        // The class captured at ptt-start decides the ydotool backend's
        // paste chord and the per-window style rules resolve against it, so
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
        assert_eq!(
            out.backend, "fallback-mock",
            "must report that the fallback, not the primary, ran"
        );
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

    /// A [`Clipboard`] that touches nothing and writes down what it was
    /// asked to do, in order. The real one runs `wl-paste`/`wl-copy`, and a
    /// test that ran those would swap out whatever the developer had copied.
    #[derive(Default)]
    struct RecordingClipboard {
        /// `"snapshot"`, `"stage:<text>"`, `"restore:<bytes>"`, in call order.
        log: Mutex<Vec<String>>,
        /// What `snapshot` answers. `None` is an empty clipboard.
        held: Option<ClipboardSnapshot>,
    }

    impl RecordingClipboard {
        fn holding(text: &str) -> Self {
            Self {
                held: Some(ClipboardSnapshot {
                    mime: "text/plain;charset=utf-8".to_string(),
                    bytes: text.as_bytes().to_vec(),
                }),
                ..Self::default()
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Clipboard for RecordingClipboard {
        fn snapshot(&self) -> Option<ClipboardSnapshot> {
            self.log.lock().unwrap().push("snapshot".to_string());
            self.held.clone()
        }

        fn stage(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
            self.log.lock().unwrap().push(format!("stage:{text}"));
            Ok(())
        }

        fn restore(&self, snapshot: &ClipboardSnapshot) {
            self.log
                .lock()
                .unwrap()
                .push(format!("restore:{}", String::from_utf8_lossy(&snapshot.bytes)));
        }
    }

    /// `paste_through_clipboard` with both settles skipped and a press that
    /// records itself into the same log.
    fn paste_with(
        clipboard: &RecordingClipboard,
        restore: bool,
        press: Result<(), InjectError>,
    ) -> Result<(), InjectError> {
        let pressed = Mutex::new(Some(press));
        paste_through_clipboard(
            clipboard,
            "diktat",
            Some("kitty"),
            restore,
            Duration::ZERO,
            Duration::ZERO,
            &|| {
                clipboard.log.lock().unwrap().push("press".to_string());
                pressed.lock().unwrap().take().expect("the chord is pressed once")
            },
        )
    }

    #[test]
    fn the_previous_clipboard_is_read_before_staging_and_put_back_after_the_paste() {
        // The whole feature, and it is entirely an ordering: read first
        // (staging destroys it), write back last (the target has to have
        // read the transcript). The user's own entry ends up current again
        // and the transcript one step down in their clipboard history.
        let clipboard = RecordingClipboard::holding("was der nutzer kopiert hatte");
        paste_with(&clipboard, true, Ok(())).unwrap();
        assert_eq!(
            clipboard.log(),
            vec![
                "snapshot",
                "stage:diktat",
                "press",
                "restore:was der nutzer kopiert hatte",
            ]
        );
    }

    #[test]
    fn a_failed_paste_leaves_the_transcript_on_the_clipboard() {
        // Invariant 1 in the shape this feature meets it: the press failed,
        // so `inject_with_recovery` is about to notify "transcript copied to
        // clipboard" -- and the transcript has to still be there for that
        // notification to be true. Restoring here would take it away.
        let clipboard = RecordingClipboard::holding("was der nutzer kopiert hatte");
        let err = paste_with(&clipboard, true, Err(InjectError::Mock)).unwrap_err();
        assert!(matches!(err, InjectError::Mock), "{err:?}");
        assert_eq!(clipboard.log(), vec!["snapshot", "stage:diktat", "press"]);
    }

    #[test]
    fn an_empty_clipboard_is_not_restored_over_the_transcript() {
        // `wl-copy --clear` would be the alternative, and it would leave a
        // user without a clipboard-history manager nothing at all to paste.
        let clipboard = RecordingClipboard::default();
        paste_with(&clipboard, true, Ok(())).unwrap();
        assert_eq!(clipboard.log(), vec!["snapshot", "stage:diktat", "press"]);
    }

    #[test]
    fn the_clipboard_is_not_even_read_when_the_setting_is_off() {
        // `restore_clipboard = false` is the escape hatch for an application
        // that reads the selection after the restore has already happened,
        // so it must cost nothing at all -- not a `wl-paste` that runs and
        // is then ignored.
        let clipboard = RecordingClipboard::holding("was der nutzer kopiert hatte");
        paste_with(&clipboard, false, Ok(())).unwrap();
        assert_eq!(clipboard.log(), vec!["stage:diktat", "press"]);
    }

    #[test]
    fn both_chord_backends_restore_the_clipboard_by_default() {
        // The setting a user never touches. Both backends that stage the
        // transcript themselves read it; nothing else does.
        let cfg = InjectConfig::default();
        assert!(cfg.restore_clipboard);
        assert!(YdotoolInjector::new(&cfg).restore_clipboard);
        assert!(LibeiInjector::new(&cfg).restore_clipboard);
        let off = InjectConfig { restore_clipboard: false, ..InjectConfig::default() };
        assert!(!YdotoolInjector::new(&off).restore_clipboard);
        assert!(!LibeiInjector::new(&off).restore_clipboard);
    }

    #[test]
    fn the_snapshotted_type_is_the_one_a_manual_paste_would_have_used() {
        // An offer carries several types and only one can be handed back.
        // Text wins, because that is what `wl-paste` itself picks with no
        // `--type` -- so the restored entry is what Ctrl+V would have given.
        assert_eq!(
            preferred_mime("text/html\ntext/plain;charset=utf-8\ntext/plain\nSTRING\n").as_deref(),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(preferred_mime("text/html\ntext/plain\n").as_deref(), Some("text/plain"));
        assert_eq!(preferred_mime("text/html\n").as_deref(), Some("text/html"));
    }

    #[test]
    fn a_clipboard_holding_only_an_image_is_snapshotted_under_its_own_type() {
        // Nothing about the restore is text-specific: a copied screenshot
        // has to survive a dictation too, which is why the type travels
        // with the bytes instead of being assumed.
        assert_eq!(preferred_mime("image/png\n").as_deref(), Some("image/png"));
        assert!(!is_text_mime("image/png"));
        assert_eq!(preferred_mime("").as_deref(), None, "an empty offer restores nothing");
    }

    #[test]
    fn the_x11_atom_spellings_of_text_count_as_text() {
        // `wl-paste --list-types` reports these for anything an XWayland
        // client copied, and they decide whether `--no-newline` is passed --
        // without it every dictation would grow the restored entry by a
        // newline.
        for atom in ["UTF8_STRING", "STRING", "TEXT", "text/plain", "text/html"] {
            assert!(is_text_mime(atom), "{atom} is text");
        }
        assert!(!is_text_mime("application/octet-stream"));
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
