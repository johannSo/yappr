//! The paste chord, pressed through the XDG Desktop Portal instead of a
//! `ydotool` daemon.
//!
//! This is the transport behind [`InjectBackend::Libei`]. What it buys over
//! [`YdotoolInjector`] is everything that backend asks of the user and this
//! one does not: no `ydotoold`, no write access to `/dev/uinput`, no group
//! membership, and no socket path the client has to guess
//! (`$YDOTOOL_SOCKET`, else `$XDG_RUNTIME_DIR/.ydotool_socket` -- yappr
//! starts from the tray or a `.desktop` entry and inherits no shell export,
//! so a daemon started anywhere else breaks every paste and says so on
//! *stdout* with exit 2, which is the whole reason `inject::diagnostic`
//! exists). The portal is always there, needs no setup, and answers with a
//! real D-Bus error.
//!
//! It pastes rather than types, exactly as `ydotool` does: `wl-copy` the
//! transcript, then one Ctrl+V (Ctrl+Shift+V per `inject::wants_shift`).
//! Synthesizing the transcript key by key would reintroduce the layout
//! problem [`InjectBackend::Ydotool`]'s doc comment describes.
//!
//! ## Why `NotifyKeyboardKeysym` and not EIS
//!
//! The portal offers two ways in, and the obvious reading of them is
//! backwards. `ConnectToEIS` is where upstream keeps steering callers, but
//! it needs a 2023-or-later stack (GNOME 45+, Plasma 6+, xdg-desktop-portal
//! 1.18+); `NotifyKeyboardKeysym` reaches back years on GNOME and KDE alike
//! and is the *more* portable of the two. It is also ~100 lines instead of a
//! protocol implementation, and because the compositor resolves a **keysym**
//! against the current keymap it is layout-independent in a way `ydotool
//! key`'s raw evdev keycodes are not -- the one respect in which this
//! backend is better than the one it is modelled on rather than merely
//! cheaper.
//!
//! Measured here on 2026-09-12 (Fedora 44, GNOME Shell 50, mutter 50.0,
//! xdg-desktop-portal 1.21.1, xdg-desktop-portal-gnome 50.0): the portal
//! exports `NotifyKeyboardKeysym` *and* `ConnectToEIS` at `version 2`, with
//! `AvailableDeviceTypes 7` (keyboard, pointer, touchscreen). If a desktop
//! ever answers neither, every failure here is an `Err` and the clipboard
//! fallback carries the transcript (invariant 1).
//!
//! ## The session is opened around a paste, and approved once, ever
//!
//! Two facts pull in opposite directions here, and the shape of this module
//! is the reconciliation.
//!
//! **`Start` can raise an approval dialog, and that dialog takes keyboard
//! focus.** Raising it *during* a dictation would collide with invariant 2
//! head-on: the overlay is up and the pipeline is at `INJECTING`, which is
//! precisely when nothing else may hold focus. So the very first `Start` of
//! an install is paid at startup, by [`prewarm_if_selected`], where there is
//! no overlay and a human is sitting in front of a desktop they just logged
//! into.
//!
//! **A live session is what puts GNOME's orange screen-sharing indicator in
//! the top bar.** The first version of this module held one for the life of
//! the process, which left that indicator up permanently -- reported from
//! real use on 2026-09-12, and a fair complaint: yappr shares nothing and
//! presses one chord a few times an hour.
//!
//! So the session is *closed* again [`SESSION_LINGER`] after the last paste,
//! and re-opened for the next one. That only works because
//! [`PersistMode::ExplicitlyRevoked`] plus the restore token in
//! [`token_file`] makes every `Start` after the first silent -- and because
//! [`DialogStrikes`] catches the desktop where it is not, reverting to a
//! held session rather than asking for permission once per dictation.
//! That token file is the reason this backend is usable at all; it is
//! written `0600` because it is a capability, not a setting.
//!
//! The session lives in this module rather than on the injector for a
//! separate reason: `Request::SetConfig` rebuilds the injector on *every*
//! settings autosave (`server.rs`), and a session torn down and re-approved
//! each time the user typed in a text field would be worse than none.
//!
//! ## Two measured facts that are not guessable
//!
//! **The portal refuses to create a session while the screen is locked.**
//! `CreateSession` comes back `Session creation inhibited`
//! (`GDBus.Error:org.freedesktop.DBus.Error.Failed`, from
//! xdg-desktop-portal-gnome, with mutter inhibiting underneath) -- measured
//! on this desktop on 2026-09-12, with `LockedHint=yes` and
//! `org.gnome.ScreenSaver.GetActive` returning `true`. Nothing is wrong in
//! that case and nothing should be repaired: [`establish`] simply has no
//! session yet, the next press re-tries, and a press that arrives before it
//! succeeds falls back to the clipboard. It is also why a failed
//! establishment is never latched.
//!
//! **A press must be able to give up while the session cannot.** The worker
//! thread may be sitting inside `Start` waiting for a human to answer the
//! dialog, which can take a minute; the pipeline thread at `INJECTING` may
//! not wait that long for anything (invariant 6's rule, even though there is
//! no subprocess here for `procutil::run_with_timeout` to bound). So every
//! queued press carries a deadline and the worker drops the ones that have
//! passed -- without which a chord approved late would be pressed into
//! whatever window the user had moved on to, long after the transcript had
//! already gone to the clipboard.
//!
//! [`InjectBackend::Libei`]: crate::config::InjectBackend::Libei
//! [`InjectBackend::Ydotool`]: crate::config::InjectBackend::Ydotool
//! [`YdotoolInjector`]: crate::inject::YdotoolInjector

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;

use crate::config::{InjectBackend, InjectConfig};
use crate::paths;

/// X11 keysyms, which is what the portal takes. `XK_Control_L`,
/// `XK_Shift_L` and `XK_v` -- Latin-1 keysyms are their own codepoints,
/// which is why `v` is simply `0x76`.
const XK_CONTROL_L: i32 = 0xffe3;
const XK_SHIFT_L: i32 = 0xffe1;
const XK_V: i32 = 0x76;

/// One portal round trip that involves no human: `CreateSession`,
/// `SelectDevices`, and each key event.
const PORTAL_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `Start` may take, which is how long the user has to answer the
/// approval dialog. Only ever paid when no restore token is in hand -- once
/// per install, in the ordinary case.
const PORTAL_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a press waits for the worker to answer before the clipboard
/// fallback takes the transcript instead. Deliberately far below
/// [`PORTAL_APPROVAL_TIMEOUT`]: a dictation must not block on a dialog.
const CHORD_REPLY_TIMEOUT: Duration = Duration::from_secs(8);

/// A `Start` that returns faster than this had no dialog in front of it.
/// The portal reports no such thing directly, so this is the only signal
/// there is -- see [`SessionReport::asked_a_human`], which says so.
const DIALOG_THRESHOLD: Duration = Duration::from_millis(400);

/// After a failed establishment, how long before the next press is allowed
/// to try again. Without it, a desktop with no RemoteDesktop portal at all
/// would pay a full round of timeouts on every single utterance.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(10);

/// How long an idle session is kept before it is closed again.
///
/// A live RemoteDesktop session is what puts GNOME's orange
/// screen-sharing indicator in the top bar, and holding one for the life of
/// the process left it there permanently -- reported from real use on
/// 2026-09-12, and a fair complaint: yappr is not sharing anything, it
/// presses one chord a few times an hour. So the session is opened for a
/// paste and closed again after this long.
///
/// This was two seconds, on the theory that back-to-back dictations should
/// not each pay a full `CreateSession`/`SelectDevices`/`Start`. Measurement
/// killed that theory -- a token-restored open is ~5,5 ms and a close ~1-9 ms
/// -- and two seconds is long enough to watch, which is what was reported.
///
/// **Not zero, and the reason is a race rather than a cost.** Mutter's
/// `NotifyKeyboardKeysym` completes its D-Bus call and *then* dispatches the
/// key to the input thread (`notify_keyval_in_impl` runs under a `GTask`, see
/// `meta-virtual-input-device-native.c`), so the portal answering does not
/// mean the compositor has emitted the event yet. Closing the session -- and
/// with it the virtual keyboard -- the instant the last release is
/// acknowledged could therefore drop it, and a dropped *release* leaves Ctrl
/// or Shift stuck down for everything the user does next, with nothing in
/// yappr reporting it. A quarter second is far more than that dispatch
/// needs and short enough that the indicator reads as a blink.
const SESSION_LINGER: Duration = Duration::from_millis(250);

/// Closing a session is best-effort; it must not hold up the worker.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many *consecutive* re-opens may cost an approval dialog before the
/// session stops being closed at all.
///
/// The open-per-paste scheme rests on one assumption: that
/// [`PersistMode::ExplicitlyRevoked`] plus a stored restore token makes
/// `Start` silent. Measured here on 2026-09-12, it holds comfortably -- four
/// close/re-open cycles 5 s, 45 s and 90 s apart each took ~5,5 ms and got
/// the *same* token back. But it held once and then did not: a re-open 42 s
/// after a silent one raised a dialog (3,4 s, confirmed against
/// xdg-desktop-portal-gnome's journal), for a token that had worked minutes
/// earlier and has worked in every deliberate test since. The cause is not
/// understood, which is precisely why the response is a count rather than a
/// latch.
///
/// **Two, not one.** The first version latched on a single dialog, and one
/// unexplained event then condemned the whole process to holding a session
/// open -- the permanent screen-sharing indicator this was written to
/// remove, reinstated by the guard meant to protect it. And not zero either:
/// a desktop that genuinely re-prompts every time would otherwise steal
/// focus once per dictation, which is invariant 2's problem and far worse
/// than an indicator. Two bounds the damage at two dialogs, ever.
const DIALOG_STRIKES_BEFORE_HOLDING: u32 = 2;

/// Counts consecutive re-opens that cost a dialog, and says when to give up
/// on closing the session. Lives on the worker thread, which is the only
/// place either question is asked.
#[derive(Default)]
struct DialogStrikes(u32);

impl DialogStrikes {
    /// Folds one establishment in. Returns `true` once the session should
    /// stop being closed. A silent re-open clears the count, so the strikes
    /// have to be consecutive -- an isolated dialog months apart never adds
    /// up to holding a session forever.
    fn record(&mut self, report: &SessionReport) -> bool {
        if reopening_costs_a_dialog(report) {
            self.0 += 1;
        } else {
            self.0 = 0;
        }
        self.0 >= DIALOG_STRIKES_BEFORE_HOLDING
    }
}

/// Everything that can go wrong on this path, in the two shapes
/// `inject.rs` has to tell apart: a deadline, and everything else.
#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("{0}")]
    Failed(String),
    #[error("the desktop portal did not answer within {0:?}")]
    Timeout(Duration),
}

/// What the session that carried (or failed to carry) the last chord
/// actually did, for `examples/libei_probe.rs` to print.
///
/// Nothing in the pipeline reads this. It exists because the two questions
/// a user debugging this backend has -- "which way in did it find?" and "did
/// it have to ask me?" -- are answerable only from inside the worker.
#[derive(Debug, Clone)]
pub struct SessionReport {
    /// The portal method the chord goes through. Always
    /// `"RemoteDesktop.NotifyKeyboardKeysym"` today; the field exists so the
    /// probe reports the path rather than asserting it.
    pub path: &'static str,
    /// Whether a stored restore token was offered to the portal.
    pub token_offered: bool,
    /// Whether the portal handed one back to store for next time.
    pub token_issued: bool,
    /// Whether the token handed back is the *same* one that was offered.
    ///
    /// Measured 2026-09-12: a healthy restore answers with the identical
    /// token every time, so `false` here with `token_offered` true means the
    /// permission was granted afresh -- which is to say a dialog was shown,
    /// independently of how long `Start` took. The one unexplained dialog
    /// this module has seen would have been a one-line diagnosis with this
    /// field, which is why it is recorded rather than inferred.
    pub token_reused: bool,
    /// How long `Start` took.
    pub start_elapsed: Duration,
}

impl SessionReport {
    /// Whether a human almost certainly had to click something.
    ///
    /// Inferred, not reported: the portal's `Start` gives no indication of
    /// whether it showed a dialog. A restored session comes back in
    /// milliseconds and an approval takes as long as a person takes, so
    /// [`DIALOG_THRESHOLD`] separates them with room to spare -- but it is a
    /// heuristic and the probe prints it as one.
    pub fn asked_a_human(&self) -> bool {
        self.start_elapsed >= DIALOG_THRESHOLD
    }
}

/// Where the portal's restore token is kept.
///
/// A state file rather than a `config.toml` key, for the reason
/// [`paths::wizard_marker`] is one: it carries no setting anybody should
/// edit, and a `deny_unknown_fields` struct (invariant 4) would put it in
/// the settings GUI as a row that must not be touched. Deleting it costs
/// exactly one approval dialog.
pub fn token_file() -> std::path::PathBuf {
    paths::state_dir().join("libei-restore-token")
}

/// The chord as `(keysym, pressed)` pairs, in nested order: modifiers down,
/// `v` down, `v` up, modifiers up.
///
/// Keysyms, not the raw evdev keycodes `inject::paste_key_argv` hands
/// `ydotool`: the compositor resolves these against the keymap the
/// user actually has, so this is correct on a layout where `v` is not where
/// a US keyboard puts it, which the keycode form is not.
pub(crate) fn paste_keysyms(shift: bool) -> Vec<(i32, bool)> {
    let mut seq = vec![(XK_CONTROL_L, true)];
    if shift {
        seq.push((XK_SHIFT_L, true));
    }
    seq.push((XK_V, true));
    seq.push((XK_V, false));
    if shift {
        seq.push((XK_SHIFT_L, false));
    }
    seq.push((XK_CONTROL_L, false));
    seq
}

/// One press, with the moment after which it is no longer worth pressing.
struct Press {
    shift: bool,
    deadline: Instant,
    reply: SyncSender<Result<(), PortalError>>,
}

static WORKER: OnceLock<SyncSender<Press>> = OnceLock::new();
static LAST_OUTCOME: OnceLock<Mutex<Option<Result<SessionReport, String>>>> = OnceLock::new();

fn outcome_slot() -> &'static Mutex<Option<Result<SessionReport, String>>> {
    LAST_OUTCOME.get_or_init(|| Mutex::new(None))
}

/// How the worker's last attempt at a session went, or `None` if it has not
/// finished one yet. The failure arm carries the portal's own words, which
/// on a locked screen is `Session creation inhibited` -- see the module doc.
pub fn session_outcome() -> Option<Result<SessionReport, String>> {
    outcome_slot().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Blocks until the worker has finished an attempt, or `limit` passes.
///
/// For `examples/libei_probe.rs` and nothing else: the pipeline never waits
/// for a session, it presses and falls back. A probe does have to wait,
/// because the first attempt of a fresh install is a human answering a
/// dialog and there would be nothing to report before then.
pub fn wait_for_session(limit: Duration) -> Option<Result<SessionReport, String>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(outcome) = session_outcome() {
            return Some(outcome);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Starts the portal worker and lets it establish its session before any
/// dictation does. Idempotent; later calls do nothing.
///
/// Never fails outwardly: a desktop with no RemoteDesktop portal, or a
/// locked screen, leaves the worker with no session, and the first press
/// then fails into the clipboard fallback (invariant 1).
///
/// **Deliberately not called by `LibeiInjector::new`.** That constructor
/// runs in `inject::build`, which `cargo test` reaches through
/// `build_selects_the_configured_backend` -- and a unit test that raises an
/// approval dialog on whoever's desktop is running it is not a unit test.
/// The two callers that do want it are the two places a *user* has chosen
/// this backend: `server::start`, and the `SetConfig`/`Reload` handlers.
pub fn start() {
    let _ = worker();
}

/// [`start`], but only when the user has actually chosen this backend.
///
/// Called from `server::start` so the approval dialog -- if there is one to
/// answer -- happens while the user is sitting in front of a desktop they
/// just logged into, not while an overlay is up and the pipeline is waiting
/// to inject; and from the `SetConfig` and `Reload` handlers, which is the
/// other moment the backend can come to be selected and the other moment
/// there is a human at the keyboard to answer with.
pub fn prewarm_if_selected(cfg: &InjectConfig) {
    if cfg.backend == InjectBackend::Libei {
        start();
    }
}

fn worker() -> &'static SyncSender<Press> {
    WORKER.get_or_init(|| {
        // Bounded at one, and `press_chord` only ever `try_send`s onto it.
        // Both halves of that matter. A press that is still queued is a
        // press the worker has not reached -- it is inside `Start`, waiting
        // on a human -- and the honest answer to a second one is "no", not a
        // queue. A blocking `send` would instead park the pipeline thread at
        // INJECTING for as long as the dialog stayed unanswered, which is
        // the one thing `CHORD_REPLY_TIMEOUT` exists to prevent and would
        // have bypassed it entirely.
        let (tx, rx) = mpsc::sync_channel::<Press>(1);
        let spawned = std::thread::Builder::new()
            .name("libei-portal".to_string())
            .spawn(move || serve(rx));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not spawn the portal worker; \
                 the libei backend will fall back to the clipboard");
        }
        tx
    })
}

/// The worker thread: owns the portal session, and is the only place any of
/// this crate's `async` is driven besides `gnome.rs`.
///
/// One `block_on` per call rather than one around the whole loop, which is
/// safe because zbus drives its own connection on an internal executor
/// thread of its own -- nothing here has to keep ticking it between presses.
fn serve(rx: Receiver<Press>) {
    let mut portal: Option<Portal> = None;
    let mut next_attempt = Instant::now();
    let mut strikes = DialogStrikes::default();
    // Set once the session stops being closed. Local to this thread because
    // nothing else asks: the worker is the only thing that opens or closes a
    // session.
    let mut hold_open = false;

    // Eagerly, before the first press: this is the call that may raise the
    // approval dialog, and the whole point of the module is that it happens
    // here rather than at `INJECTING`. Then closed straight away -- the
    // first run of a fresh install is the one `Start` that legitimately
    // shows a dialog, and paying it at startup is exactly what this call is
    // for; *holding* the session afterwards is what put a permanent
    // screen-sharing indicator in the top bar.
    //
    // On every later start this same call is also the measurement that
    // decides the whole scheme: it offers the stored token, so if `Start` is
    // slow here it is slow because a dialog appeared, and `attempt` latches
    // that it is holding the session before any dictation is surprised.
    attempt(&mut portal, &mut next_attempt, &mut strikes, &mut hold_open);
    close(&mut portal, hold_open);

    loop {
        // A session that is open waits only `SESSION_LINGER` for more work
        // before giving the indicator back; with none open there is nothing
        // to time out, so block.
        let press = if portal.is_some() {
            match rx.recv_timeout(SESSION_LINGER) {
                Ok(press) => press,
                Err(RecvTimeoutError::Timeout) => {
                    close(&mut portal, hold_open);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(press) => press,
                Err(_) => return,
            }
        };

        if Instant::now() >= press.deadline {
            // The sender has already given up and the clipboard has the
            // transcript. Pressing now would paste into whatever the user
            // moved on to.
            tracing::warn!("dropping a paste chord whose deadline passed while the \
                 portal session was still being established");
            let _ = press.reply.send(Err(PortalError::Timeout(CHORD_REPLY_TIMEOUT)));
            continue;
        }
        if portal.is_none() && Instant::now() >= next_attempt {
            attempt(&mut portal, &mut next_attempt, &mut strikes, &mut hold_open);
        }
        let result = match portal.as_ref() {
            Some(p) => {
                let first = futures_lite::future::block_on(p.press(press.shift));
                match first {
                    Ok(()) => Ok(()),
                    // A session the compositor has closed under us -- the
                    // screen was locked, the portal restarted, the user
                    // revoked and re-granted. Re-establishing is silent when
                    // a restore token is in hand, so retry once rather than
                    // spending the user's dictation on a diagnosis.
                    Err(e) => {
                        tracing::info!(error = %e, "portal session stopped answering; \
                             re-establishing and retrying the chord once");
                        portal = None;
                        next_attempt = Instant::now();
                        attempt(&mut portal, &mut next_attempt, &mut strikes, &mut hold_open);
                        match portal.as_ref() {
                            // Re-checked, not assumed: re-establishing can
                            // take as long as an approval dialog, and the
                            // caller gave up after `CHORD_REPLY_TIMEOUT`.
                            // Pressing here without looking would paste into
                            // whatever the user moved on to, minutes later --
                            // the same stray chord the queue guard above
                            // exists to prevent.
                            Some(_) if Instant::now() >= press.deadline => {
                                tracing::warn!("dropping the chord retry: re-establishing \
                                     the portal session outlasted the press");
                                Err(PortalError::Timeout(CHORD_REPLY_TIMEOUT))
                            }
                            Some(p) => futures_lite::future::block_on(p.press(press.shift)),
                            None => Err(e),
                        }
                    }
                }
            }
            None => Err(PortalError::Failed(
                "no RemoteDesktop portal session (is the screen locked, or does this \
                 desktop's portal not implement RemoteDesktop?)"
                    .to_string(),
            )),
        };
        let _ = press.reply.send(result);
    }
}

/// Ends the session, which is what takes GNOME's screen-sharing indicator
/// out of the top bar.
///
/// Best-effort and bounded: a portal that will not answer a `Close` must not
/// wedge the worker, and the session is being dropped either way -- the
/// worst case of a failed close is the indicator lingering until the portal
/// notices our peer is gone.
fn close(portal: &mut Option<Portal>, hold_open: bool) {
    let Some(p) = portal.take() else { return };
    if hold_open {
        // Put it back: re-opening costs a dialog on this desktop, so the
        // indicator is the lesser evil. See `DIALOG_STRIKES_BEFORE_HOLDING`.
        *portal = Some(p);
        return;
    }
    let closed = futures_lite::future::block_on(deadline(CLOSE_TIMEOUT, p.session.close()));
    match closed {
        Ok(()) => tracing::debug!("closed the RemoteDesktop portal session"),
        Err(e) => tracing::debug!(error = %e, "could not close the portal session cleanly"),
    }
}

/// Whether this establishment says that re-opening a session costs an
/// approval dialog, and so that the session must stop being closed.
///
/// Both halves matter. A slow `Start` with **no** token offered is the
/// first run of a fresh install: the dialog is expected there, it is the
/// whole point of prewarming at startup, and reading it as a failure would
/// leave every install holding a session open forever. A slow `Start`
/// *with* a token offered is the real signal -- the permission yappr
/// already has did not open a session by itself.
fn reopening_costs_a_dialog(report: &SessionReport) -> bool {
    report.token_offered && report.asked_a_human()
}

/// One establishment attempt, recording when the next one is allowed.
fn attempt(
    portal: &mut Option<Portal>,
    next_attempt: &mut Instant,
    strikes: &mut DialogStrikes,
    hold_open: &mut bool,
) {
    match futures_lite::future::block_on(establish()) {
        Ok((p, report)) => {
            tracing::info!(
                token_offered = report.token_offered,
                token_issued = report.token_issued,
                start_ms = report.start_elapsed.as_millis() as u64,
                // Whether the portal handed back the very token it was
                // given. Logged because it is the one field that separates
                // "the permission is intact" from "the permission was
                // re-granted", and the 2026-09-12 dialog nobody could
                // explain would have been diagnosable in one line with it.
                token_reused = report.token_reused,
                "RemoteDesktop portal session established"
            );
            if strikes.record(&report) && !*hold_open {
                *hold_open = true;
                tracing::warn!(
                    start_ms = report.start_elapsed.as_millis() as u64,
                    "the stored portal permission has now failed to open a session \
                     silently {DIALOG_STRIKES_BEFORE_HOLDING} times running, so yappr \
                     will hold one open instead of re-asking for every dictation -- \
                     your desktop will show its screen-sharing indicator while yappr runs"
                );
            }
            *outcome_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(Ok(report));
            *portal = Some(p);
        }
        Err(e) => {
            // Not latched, and not an error the user has to act on: a locked
            // screen is the ordinary case here (see the module doc), and the
            // next press tries again.
            tracing::warn!(error = %e, "could not establish a RemoteDesktop portal session; \
                 the libei backend will fall back to the clipboard until it can");
            *outcome_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(Err(e.to_string()));
            *portal = None;
            *next_attempt = Instant::now() + RETRY_AFTER_FAILURE;
        }
    }
}

/// A live, approved keyboard session on the RemoteDesktop portal.
struct Portal {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
}

impl Portal {
    async fn press(&self, shift: bool) -> Result<(), PortalError> {
        for (keysym, pressed) in paste_keysyms(shift) {
            let state = if pressed { KeyState::Pressed } else { KeyState::Released };
            deadline(
                PORTAL_CALL_TIMEOUT,
                self.proxy.notify_keyboard_keysym(
                    &self.session,
                    keysym,
                    state,
                    Default::default(),
                ),
            )
            .await
            .map_err(|e| {
                PortalError::Failed(format!("NotifyKeyboardKeysym(0x{keysym:x}, {state:?}): {e}"))
            })?;
        }
        Ok(())
    }
}

/// Runs `fut` under a deadline, mapping both the timeout and the portal's
/// own errors into [`PortalError`].
///
/// Invariant 6's rule with no subprocess to apply it to: every round trip
/// gets a bound, because a portal that never answers must degrade to the
/// clipboard rather than wedge the pipeline thread.
async fn deadline<T>(
    limit: Duration,
    fut: impl std::future::Future<Output = Result<T, ashpd::Error>>,
) -> Result<T, PortalError> {
    let timer = async {
        async_io::Timer::after(limit).await;
        Err(PortalError::Timeout(limit))
    };
    let work = async { fut.await.map_err(|e| PortalError::Failed(e.to_string())) };
    futures_lite::future::or(work, timer).await
}

/// Creates, configures and starts one keyboard-only portal session.
///
/// The restore token is offered when there is one and the result is written
/// back when the portal issues one. A `Start` that fails *with* a token in
/// hand is retried once without it: a token can go stale (the portal's
/// permission store was cleared, the session was revoked), and a stale token
/// that permanently poisoned every start would leave the user with no way
/// back short of finding this file.
async fn establish() -> Result<(Portal, SessionReport), PortalError> {
    let stored = read_token();
    match start_session(stored.as_deref()).await {
        Ok(v) => Ok(v),
        Err(e) if stored.is_some() => {
            tracing::warn!(error = %e, "the stored restore token did not work; \
                 discarding it and asking for approval again");
            let _ = std::fs::remove_file(token_file());
            start_session(None).await
        }
        Err(e) => Err(e),
    }
}

async fn start_session(token: Option<&str>) -> Result<(Portal, SessionReport), PortalError> {
    let proxy = deadline(PORTAL_CALL_TIMEOUT, RemoteDesktop::new()).await?;
    let session =
        deadline(PORTAL_CALL_TIMEOUT, proxy.create_session(Default::default())).await?;
    deadline(
        PORTAL_CALL_TIMEOUT,
        proxy.select_devices(
            &session,
            SelectDevicesOptions::default()
                // A keyboard and nothing else. The portal would hand over a
                // pointer and a touchscreen too (`AvailableDeviceTypes 7`
                // here), and the approval dialog names what was asked for.
                .set_devices(BitFlags::from(DeviceType::Keyboard))
                // The token survives a restart; the grant survives until the
                // user revokes it. `DoNot` would mean a dialog per start.
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token),
        ),
    )
    .await?;

    let started = Instant::now();
    let request = deadline(
        PORTAL_APPROVAL_TIMEOUT,
        proxy.start(&session, None, Default::default()),
    )
    .await?;
    let devices = request
        .response()
        .map_err(|e| PortalError::Failed(format!("the portal refused the session: {e}")))?;
    let start_elapsed = started.elapsed();

    if !devices.devices().contains(DeviceType::Keyboard) {
        return Err(PortalError::Failed(format!(
            "the portal granted {:?}, which does not include a keyboard",
            devices.devices()
        )));
    }

    let issued = devices.restore_token();
    let token_reused = matches!((token, issued), (Some(offered), Some(new)) if offered == new);
    if let Some(t) = issued {
        write_token(t);
    }

    Ok((
        Portal { proxy, session },
        SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: token.is_some(),
            token_issued: issued.is_some(),
            token_reused,
            start_elapsed,
        },
    ))
}

fn read_token() -> Option<String> {
    let raw = std::fs::read_to_string(token_file()).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Writes the restore token `0600`.
///
/// A capability, not a setting: anything that can read it can ask the portal
/// to resume this grant without a dialog. Failing to write it costs one
/// approval dialog on the next start and nothing else, so it is logged
/// rather than propagated.
fn write_token(token: &str) {
    if let Err(e) = write_token_inner(token) {
        tracing::warn!(error = %e, "could not store the portal restore token; \
             the approval dialog will appear again on the next start");
    }
}

fn write_token_inner(token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = token_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(token.as_bytes())
}

/// Presses the paste chord, blocking the caller until the worker answers or
/// [`CHORD_REPLY_TIMEOUT`] passes.
///
/// The bound is the point: the worker may be inside the approval dialog,
/// which a dictation may not wait on. A timeout here is an `Err`, which
/// `inject_with_recovery` turns into the clipboard fallback and its
/// notification -- and the transcript is already on the clipboard by then,
/// because [`crate::inject::LibeiInjector`] copies before it presses.
pub(crate) fn press_chord(shift: bool) -> Result<(), PortalError> {
    let (reply, answer) = mpsc::sync_channel(1);
    let press = Press { shift, deadline: Instant::now() + CHORD_REPLY_TIMEOUT, reply };
    worker().try_send(press).map_err(|e| match e {
        TrySendError::Full(_) => PortalError::Failed(
            "a previous paste chord is still waiting on the portal (an unanswered \
             approval dialog is the usual reason)"
                .to_string(),
        ),
        TrySendError::Disconnected(_) => {
            PortalError::Failed("the portal worker thread is gone".to_string())
        }
    })?;
    match answer.recv_timeout(CHORD_REPLY_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(PortalError::Timeout(CHORD_REPLY_TIMEOUT)),
        Err(RecvTimeoutError::Disconnected) => {
            Err(PortalError::Failed("the portal worker thread stopped".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paste_chord_for_a_normal_window_is_ctrl_v_in_nested_order() {
        // Keysyms rather than `paste_key_argv`'s evdev keycodes: the
        // compositor resolves these against the user's own keymap, which is
        // the one thing this backend does better than the one it copies.
        assert_eq!(
            paste_keysyms(false),
            vec![(XK_CONTROL_L, true), (XK_V, true), (XK_V, false), (XK_CONTROL_L, false)]
        );
    }

    #[test]
    fn the_paste_chord_for_a_terminal_adds_shift_in_nested_order() {
        assert_eq!(
            paste_keysyms(true),
            vec![
                (XK_CONTROL_L, true),
                (XK_SHIFT_L, true),
                (XK_V, true),
                (XK_V, false),
                (XK_SHIFT_L, false),
                (XK_CONTROL_L, false),
            ]
        );
    }

    #[test]
    fn every_key_the_chord_presses_is_released_again() {
        // A modifier left down is not a failed paste, it is a desktop that
        // has silently entered Ctrl-held mode for everything the user does
        // next -- and nothing in the pipeline would report it.
        for shift in [false, true] {
            let seq = paste_keysyms(shift);
            for (keysym, _) in &seq {
                let downs = seq.iter().filter(|(k, p)| k == keysym && *p).count();
                let ups = seq.iter().filter(|(k, p)| k == keysym && !*p).count();
                assert_eq!(downs, ups, "keysym 0x{keysym:x} with shift={shift}");
            }
        }
    }

    #[test]
    fn the_restore_token_lives_in_the_state_dir_beside_the_wizard_marker() {
        // Deliberately not a `config.toml` key -- see `token_file`.
        assert_eq!(token_file().parent(), paths::wizard_marker().parent());
        assert_eq!(token_file().file_name().unwrap(), "libei-restore-token");
    }

    #[test]
    fn a_start_that_returned_instantly_is_not_reported_as_having_asked_a_human() {
        let restored = SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: true,
            token_issued: false,
            token_reused: false,
            start_elapsed: Duration::from_millis(12),
        };
        assert!(!restored.asked_a_human());
        let approved = SessionReport { start_elapsed: Duration::from_secs(4), ..restored };
        assert!(approved.asked_a_human());
    }

    /// The one thing about this backend that a live desktop can answer and
    /// a unit test cannot: whether the portal in front of it implements
    /// `RemoteDesktop` at all, and whether it will hand over a keyboard.
    ///
    /// Deliberately stops short of `CreateSession`. Everything past this
    /// point raises the approval dialog on a machine with no stored token,
    /// and a test run is not a moment to ask a human for a permission --
    /// which is also why `cargo run -p yappr-core --example libei_probe` is
    /// the thing that exercises the rest, once, on purpose.
    ///
    /// `#[ignore]`d because it needs a session bus and a desktop portal, the
    /// same reason the ASR fixtures need downloaded models. On
    /// `xdg-desktop-portal-wlr` (sway, river, Wayfire) it fails, correctly:
    /// there is no `RemoteDesktop` implementation there and this backend
    /// cannot work.
    #[test]
    #[ignore = "needs a session bus and a desktop portal"]
    fn the_portal_on_this_desktop_hands_out_keyboards() {
        let (version, devices) = futures_lite::future::block_on(async {
            let proxy = RemoteDesktop::new().await.expect("no RemoteDesktop portal on this desktop");
            let devices = proxy.available_device_types().await.expect("AvailableDeviceTypes");
            (proxy.version(), devices)
        });
        eprintln!("RemoteDesktop portal version {version}, device types {devices:?}");
        assert!(
            devices.contains(DeviceType::Keyboard),
            "the portal offers {devices:?}, which has no keyboard in it"
        );
    }

    #[test]
    fn a_first_run_dialog_is_not_read_as_a_reason_to_hold_the_session_open() {
        // The distinction the whole open-per-paste scheme rests on. Reading
        // the first install's expected dialog as "tokens do not work here"
        // would leave every machine holding a session -- and so showing a
        // screen-sharing indicator -- forever, which is the bug this was
        // written to fix.
        let first_run = SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: false,
            token_issued: true,
            token_reused: false,
            start_elapsed: Duration::from_secs(4),
        };
        assert!(!reopening_costs_a_dialog(&first_run));
    }

    #[test]
    fn a_restored_session_that_came_up_instantly_leaves_the_session_closable() {
        let restored = SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: true,
            token_issued: true,
            token_reused: true,
            start_elapsed: Duration::from_millis(30),
        };
        assert!(!reopening_costs_a_dialog(&restored));
    }

    #[test]
    fn a_stored_permission_that_still_asks_makes_the_session_stay_open() {
        // The self-correcting arm: one surprise dialog, then never again.
        // Closing the session on such a desktop would trade a tidy top bar
        // for a focus-stealing dialog per dictation, which is much worse.
        let still_asks = SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: true,
            token_issued: true,
            token_reused: false,
            start_elapsed: Duration::from_secs(6),
        };
        assert!(reopening_costs_a_dialog(&still_asks));
    }

    /// A `SessionReport` that came back silently from a stored token.
    fn silent() -> SessionReport {
        SessionReport {
            path: "RemoteDesktop.NotifyKeyboardKeysym",
            token_offered: true,
            token_issued: true,
            token_reused: true,
            start_elapsed: Duration::from_millis(6),
        }
    }

    /// The same, but slow enough to have shown a dialog.
    fn asked() -> SessionReport {
        SessionReport { start_elapsed: Duration::from_secs(3), token_reused: false, ..silent() }
    }

    #[test]
    fn one_unexplained_dialog_does_not_condemn_the_process_to_holding_a_session() {
        // The regression this counter exists for. A single dialog latched
        // the first version permanently, so one unexplained event -- and
        // exactly one has ever been observed -- put the screen-sharing
        // indicator back up for good, which is the thing the whole
        // open-per-paste scheme was written to remove.
        let mut strikes = DialogStrikes::default();
        assert!(!strikes.record(&asked()), "one dialog must not be enough");
    }

    #[test]
    fn a_desktop_that_asks_every_time_gets_the_session_held_after_two() {
        // The other side: never holding would mean a focus-stealing dialog
        // per dictation on a desktop whose permission does not restore.
        let mut strikes = DialogStrikes::default();
        assert!(!strikes.record(&asked()));
        assert!(strikes.record(&asked()), "two in a row must be enough");
    }

    #[test]
    fn a_silent_reopen_clears_the_strikes_so_they_have_to_be_consecutive() {
        // Without this an isolated dialog in the morning and another in the
        // afternoon would add up to holding the session forever.
        let mut strikes = DialogStrikes::default();
        assert!(!strikes.record(&asked()));
        assert!(!strikes.record(&silent()));
        assert!(!strikes.record(&asked()), "the silent one in between must have reset the count");
    }

    #[test]
    fn an_idle_session_is_given_back_before_the_indicator_can_be_read() {
        // Two seconds was reported as visibly long. The upper bound is what
        // that report turned into a rule; the lower bound is the mutter
        // dispatch race in `SESSION_LINGER`'s doc -- a key release dropped
        // by closing too eagerly leaves a modifier stuck down, which is
        // worse than any indicator.
        assert!(SESSION_LINGER >= Duration::from_millis(100), "too eager to be safe");
        assert!(SESSION_LINGER <= Duration::from_millis(500), "long enough to read");
        // And closing must never be able to outlast the press budget that
        // the pipeline thread is actually waiting on.
        assert!(CLOSE_TIMEOUT < CHORD_REPLY_TIMEOUT);
    }

    #[test]
    fn a_press_gives_up_long_before_the_approval_dialog_does() {
        // The property the deadline exists for: a dictation must not wait on
        // a human answering a dialog, and the worker must be allowed to.
        assert!(CHORD_REPLY_TIMEOUT < PORTAL_APPROVAL_TIMEOUT);
        assert!(PORTAL_CALL_TIMEOUT < CHORD_REPLY_TIMEOUT);
    }
}
