//! The focused window on GNOME, polled from the accessibility bus.
//!
//! GNOME exposes no supported way for an ordinary application to ask which
//! window has focus. Measured on GNOME Shell 50.0 / Fedora 44, 2026-09-08:
//!
//! - `org.gnome.Shell.Eval` answers `(false, '')` -- disabled since GNOME 41
//!   outside unsafe-mode, which is not a thing to ask a user to turn on.
//! - `org.gnome.Shell.Introspect`'s `GetWindows` and `GetRunningApplications`
//!   both answer `AccessDenied`, behind an allowlist compiled into the shell.
//!   There is no gsetting that opens it.
//! - `xprop` sees XWayland windows only, so a native Wayland client -- Ptyxis,
//!   GNOME's own terminal since Fedora 41 -- is invisible to it.
//!
//! The accessibility bus does answer, needs no extension, and is already
//! running in every GNOME session: GTK exposes its widget tree there whenever
//! the bus is present, independent of
//! `org.gnome.desktop.interface toolkit-accessibility`.
//!
//! ## Why this polls rather than listening
//!
//! Because the events do not arrive. Registering for `window:activate` here
//! succeeds and then yields nothing at all -- verified twice over, with this
//! crate and with an independent `pyatspi` listener, opening and focusing
//! windows throughout, with `toolkit-accessibility` both `false` and `true`.
//! Only *queries* against the tree work on this desktop; broadcasts do not.
//!
//! That was worth measuring rather than assuming, because an event-driven
//! tracker fails in a particularly nasty way: the single sweep it does at
//! startup succeeds, so the class is correct for whatever window was focused
//! then and silently frozen from that moment on. Dictation keeps working in
//! that one window and stops working everywhere else, with no error at all --
//! a paste script exits 0 whichever chord it pressed.
//!
//! A full sweep costs about 7 ms here (median of 10, same session), and its
//! cost grows with the number of running applications, since it walks them.
//! So the wait between sweeps is whichever is longer: [`POLL_INTERVAL`], or
//! [`DUTY_DIVISOR`] times how long the last sweep actually took. That bounds
//! this thread to roughly 1/`DUTY_DIVISOR` of one core on any desktop,
//! busy or idle, instead of leaving it to scale with what the user happens
//! to have open. Measured at a fixed 500 ms it drew 2.3-2.6% of a core here,
//! which is what prompted the cap.
//!
//! The trade this module makes is that small bounded cost for an answer that
//! cannot go stale, against at most `POLL_INTERVAL` of lag after a window
//! switch -- and the value is read once per utterance, seconds after the
//! user focused whatever they are dictating into.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::{ObjectRefOwned, State};

/// Our own windows are never an answer. The overlay takes focus on Mutter
/// (invariant 2) at the exact moment the class is wanted, and the settings
/// window is a window of this same app -- either would otherwise overwrite
/// the very value it is covering up.
const SELF_APP: &str = "yappr";

/// Nor is the shell itself. `gnome-shell` owns focus during the overview,
/// the app grid, the lock screen and every focus hand-off in between --
/// states nobody dictates into, which would otherwise evict a real answer.
const SHELL_APP: &str = "gnome-shell";

/// The shortest wait between sweeps, and so how stale the answer may get.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Sweeps wait at least this many times their own duration, which holds the
/// poller near 1/50th of one core however many applications are running.
const DUTY_DIVISOR: u32 = 50;

/// How long to wait before reconnecting after the bus goes away. The a11y
/// bus can start *after* the daemon, so a first failure is not final.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Consecutive failed sweeps that mean the bus is gone rather than one
/// application having vanished mid-walk.
const ERRORS_BEFORE_RECONNECT: u32 = 5;

static LAST_FOCUS: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static STARTED: OnceLock<()> = OnceLock::new();

fn slot() -> &'static Mutex<Option<String>> {
    LAST_FOCUS.get_or_init(|| Mutex::new(None))
}

/// The last window seen focused that was not ours, or `None` before the
/// poller has seen one. Cheap: a mutex read, no bus traffic.
pub fn window_class() -> Option<String> {
    slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

fn record(name: &str) {
    if name.is_empty() || name == SELF_APP || name == SHELL_APP {
        return;
    }
    let mut slot = slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_deref() != Some(name) {
        tracing::debug!(class = name, "focused window changed");
        *slot = Some(name.to_string());
    }
}

/// Start the poller thread. Idempotent; later calls do nothing.
///
/// Never fails outwardly: off GNOME, or with no accessibility bus, the
/// thread finds nothing to poll and [`window_class`] keeps answering
/// `None` -- the same "unknown window" every caller already handles.
pub fn start_tracking() {
    if STARTED.set(()).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("winclass-atspi".to_string())
        .spawn(|| futures_lite::future::block_on(poll_forever()));
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not spawn the focus poller; \
             per-application style rules will fall back to their defaults");
    }
}

async fn poll_forever() {
    loop {
        match AccessibilityConnection::new().await {
            Ok(conn) => {
                tracing::debug!("polling focus on the accessibility bus");
                poll_with(&conn).await;
            }
            Err(e) => tracing::debug!(error = %e, "no accessibility bus to poll"),
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

/// Poll until the connection stops answering, then hand back so it can be
/// remade.
async fn poll_with(conn: &AccessibilityConnection) {
    let mut consecutive_errors = 0u32;
    loop {
        let started = std::time::Instant::now();
        match active_app(conn).await {
            Ok(Some(name)) => {
                consecutive_errors = 0;
                record(&name);
            }
            // Nothing focused that counts -- the overview, the lock screen,
            // or our own overlay. Keep the last real answer.
            Ok(None) => consecutive_errors = 0,
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= ERRORS_BEFORE_RECONNECT {
                    tracing::debug!(error = %e, "accessibility bus stopped answering");
                    return;
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL.max(started.elapsed() * DUTY_DIVISOR));
    }
}

/// An `AccessibleProxy` for one object reference, or `None` for a null one
/// (which names no bus peer, so there is nothing to ask).
async fn accessible(
    conn: &AccessibilityConnection,
    obj: &ObjectRefOwned,
) -> Result<Option<AccessibleProxy<'static>>, atspi::AtspiError> {
    let Some(dest) = obj.name_as_str().map(str::to_owned) else {
        return Ok(None);
    };
    Ok(Some(
        AccessibleProxy::builder(conn.connection())
            .destination(dest)?
            .path(obj.path_as_str().to_owned())?
            .build()
            .await?,
    ))
}

/// The application owning whichever window currently holds focus.
///
/// Walks applications, then their windows, stopping at the first whose state
/// carries `Active`. The *application's* name is what a paste script's own
/// terminal list is matched against -- `ptyxis`, `gnome-text-editor` -- not
/// the window's own name, which is its title.
async fn active_app(conn: &AccessibilityConnection) -> Result<Option<String>, atspi::AtspiError> {
    let root = AccessibleProxy::builder(conn.connection())
        .destination("org.a11y.atspi.Registry")?
        .path("/org/a11y/atspi/accessible/root")?
        .build()
        .await?;

    for app_ref in root.get_children().await? {
        let Ok(Some(app)) = accessible(conn, &app_ref).await else {
            continue;
        };
        // Read the name before the children: it rules an application out in
        // one round trip instead of one per window it happens to own.
        let Ok(name) = app.name().await else {
            continue;
        };
        if name.is_empty() || name == SELF_APP || name == SHELL_APP {
            continue;
        }
        let Ok(windows) = app.get_children().await else {
            continue;
        };
        for window_ref in windows {
            let Ok(Some(window)) = accessible(conn, &window_ref).await else {
                continue;
            };
            if window.get_state().await.is_ok_and(|s| s.contains(State::Active)) {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        *slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    #[test]
    fn our_own_windows_are_never_recorded() {
        // The overlay takes focus on Mutter at the exact moment the answer
        // matters, so letting it through would overwrite the real window
        // with `yappr` on every single utterance.
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        record("ptyxis");
        record(SELF_APP);
        assert_eq!(window_class().as_deref(), Some("ptyxis"));
    }

    #[test]
    fn the_shell_is_never_recorded() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        record("gnome-text-editor");
        record(SHELL_APP);
        assert_eq!(window_class().as_deref(), Some("gnome-text-editor"));
    }

    #[test]
    fn an_empty_name_is_not_a_window() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        record("kitty");
        record("");
        assert_eq!(window_class().as_deref(), Some("kitty"));
    }

    #[test]
    fn a_switch_between_real_windows_is_followed() {
        // The whole reason this polls: an event-driven tracker froze at the
        // first window here, and dictation silently kept its chord.
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset();
        record("ptyxis");
        record("gnome-text-editor");
        assert_eq!(window_class().as_deref(), Some("gnome-text-editor"));
        record("ptyxis");
        assert_eq!(window_class().as_deref(), Some("ptyxis"));
    }
}
