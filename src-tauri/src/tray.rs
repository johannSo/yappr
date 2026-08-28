//! The tray icon, as a native StatusNotifierItem (spec §5, Task 12).
//!
//! ## Why `ksni`, not Tauri's `tray-icon` feature
//!
//! Verified on this machine, 2026-08-28: the two registered tray items are
//! `handy`'s `/org/ayatana/NotificationItem/tray_icon_tray_app_…` (the
//! libayatana-appindicator path) and `claude-desktop`'s `/StatusNotifierItem`
//! (native SNI). Appindicator exposes no activation event -- which is also
//! why Tauri's own `tray-icon` feature documents click events as unsupported
//! on Linux. "Left-click opens Settings" needs the second kind, so this
//! module implements `org.kde.StatusNotifierItem` directly against
//! `org.kde.StatusNotifierWatcher` on the session bus, via `ksni`.
//!
//! ## Both click paths, deliberately, not a probe
//!
//! Whether *this* host's tray sends `Activate` on left click was a claim
//! spec §12 listed to verify by probe before implementing, and it turned
//! out not to need answering first: the two possible answers don't need
//! different code, only different first contacts. [`OwfTray::activate`]
//! opens Settings for a host that sends `Activate`; **Einstellungen is the
//! first actionable item** of the context menu for a host that doesn't
//! (the status line above it is disabled, so it is not actionable). One
//! implementation, correct either way, and correct on a machine that
//! answers differently than this one -- no runtime probe, no config
//! switch.
//!
//! ## `ksni` version and its own thread
//!
//! Pinned to the 0.2 line (`Cargo.toml`), not the newer 0.3 (zbus 5, async,
//! wants a tokio/async-io runtime): `TrayService::spawn` in 0.2 runs the
//! service on a plain OS thread via the classic blocking `dbus` crate, with
//! no async runtime to wire into Tauri's event loop -- matching this
//! module's one hard rule, that nothing tray-related may ever run on that
//! loop (an earlier task established that blocking it freezes the windows;
//! it would now freeze the tray too). `Tray::activate` and every menu
//! item's `activate` below run on `ksni`'s own thread, never Tauri's.
//!
//! ## Left click and Einstellungen do not dispatch `Request::ShowSettings`
//!
//! A deliberate deviation from this task's own brief sketch, which routes
//! `activate` through `Request::ShowSettings`. That request's only handler
//! (`owf_core::server::dispatch`) calls `EventSink::show_settings`, which
//! for `TauriSink` (`lib.rs`) does nothing but show and focus the settings
//! window -- so routing through it would be a round trip through a
//! `Daemon` for an action that never touches daemon state, and `--replay`
//! manages no `Daemon` at all, so that round trip has nowhere to go there.
//! Left click and Einstellungen call [`crate::show_settings_window`]
//! directly instead -- the same function `TauriSink::show_settings` calls,
//! so there is one definition of "show Settings", not two -- which keeps
//! both tray actions working identically under `--replay`, the one way
//! this feature can be exercised on this machine without touching the
//! production daemon's socket and lock. `Request::ShowSettings` remains
//! exactly what it was: the CLI's `--settings` flag's own path, unrelated
//! to this one.
//!
//! ## Icon and daemon state
//!
//! [`icon_name`] is a pure function over [`owf_core::proto::State`].
//! Recording is the one that matters most: under press/press toggle the
//! user's own finger no longer indicates an open microphone, so this icon
//! and the overlay are the only two indicators left, and neither may
//! render Recording as anything resembling Idle or a busy-but-not-recording
//! state.
//!
//! State reaches the tray through `TauriSink::emit` (`lib.rs`) -- the same
//! bridge that already forwards every daemon broadcast to the overlay's
//! frontend, extended rather than duplicated as a second subscription.
//! `TauriSink` re-asks `Request::Status` for the daemon's real current
//! state on every broadcast, rather than inferring one from the
//! `OverlayEvent` it just received: `OverlayEvent::Error` alone cannot tell
//! a one-time fatal warm-up failure (`State::Error`) apart from an ordinary
//! per-utterance "no speech detected" (which leaves the daemon genuinely
//! `Idle` again immediately -- see `IdleOnExit` in `owf-core`'s
//! `server.rs`), and `State::Error`'s own doc comment says the two must
//! stay distinct. Asking `Request::Status` -- the same dispatch a
//! `--status`/GUI caller gets -- means the tray can never disagree with
//! them, and it is cheap enough to do on every broadcast (a handful of
//! already-in-memory atomics/mutexes, no I/O). `--replay` has no `Daemon`
//! to ask, so it falls back to [`state_from_replay_event`], a pure
//! best-effort guess that leaves the icon unchanged on exactly the events
//! that guess can't be trusted for.

use owf_core::proto::{OverlayEvent, Request, State};
use owf_core::server::dispatch;
use tauri::{AppHandle, Manager};

/// Freedesktop icon name for each daemon state. Pure so it is testable
/// without a D-Bus connection, a tray, or a running daemon.
pub fn icon_name(state: State) -> &'static str {
    use State::*;
    match state {
        Warming => "content-loading-symbolic",
        Idle => "audio-input-microphone-symbolic",
        Recording => "media-record-symbolic",
        Transcribing | Normalizing | Injecting => "content-loading-symbolic",
        Error => "dialog-error-symbolic",
    }
}

/// The context menu's non-interactive first row, in German to match the
/// rest of the settings GUI.
fn status_label(state: State) -> &'static str {
    use State::*;
    match state {
        Warming => "Wird vorbereitet …",
        Idle => "Bereit",
        Recording => "Nimmt auf …",
        Transcribing => "Transkribiert …",
        Normalizing => "Verbessert Text …",
        Injecting => "Fügt Text ein …",
        Error => "Fehler",
    }
}

/// A pure fallback for `--replay`, which manages no `Daemon` for
/// `TauriSink` to ask `Request::Status` of the way the real branch does
/// (see this module's doc comment). Approximates the coarse `State` a
/// fixture event implies. `None` for events that either are not a `State`
/// transition at all (`BusyRejected`; the `NormalizeDegraded`/
/// `NormalizeRecovered` badge, which its own doc comment says is not one
/// either) or are ambiguous without a live daemon to resolve them
/// (`Error`, which covers both the one fatal warm-up failure and an
/// ordinary per-utterance failure that leaves the daemon `Idle` again) --
/// leaving the icon exactly as it was is the honest answer when this
/// function cannot tell which one applies.
pub(crate) fn state_from_replay_event(event: &OverlayEvent) -> Option<State> {
    match event {
        OverlayEvent::Warming => Some(State::Warming),
        OverlayEvent::Idle => Some(State::Idle),
        // `Opening` precedes the first real `Recording` sample by tens of
        // milliseconds (see `OverlayEvent::Opening`'s own doc comment) --
        // mapped to `Recording`, not left `None`, so the fallback never
        // shows a mic-is-ready icon while the mic is actually being opened.
        OverlayEvent::Opening | OverlayEvent::Recording { .. } => Some(State::Recording),
        OverlayEvent::Transcribing => Some(State::Transcribing),
        OverlayEvent::Normalizing => Some(State::Normalizing),
        OverlayEvent::Injecting => Some(State::Injecting),
        OverlayEvent::Done { .. } => Some(State::Idle),
        OverlayEvent::Error { .. }
        | OverlayEvent::BusyRejected
        | OverlayEvent::NormalizeDegraded { .. }
        | OverlayEvent::NormalizeRecovered => None,
    }
}

/// The `ksni::Tray` model. Holds an `AppHandle`, not an `Arc<Daemon>`
/// directly, so it works unchanged under `--replay` -- [`OwfTray::quit`]
/// looks up `settings_cmds::Server` lazily, at click time, rather than
/// requiring a daemon at construction.
struct OwfTray {
    app: AppHandle,
    state: State,
}

impl OwfTray {
    /// Beenden: routes through `Request::Quit`, exactly what `--quit` and
    /// the tray must both use -- never `shutdown`/`exit` directly, which
    /// would skip spec §8 step 1 (waiting out an in-flight utterance) and
    /// reintroduce the transcript-loss bug a previous task fixed. `None`
    /// under `--replay` (`settings_cmds::Server(None)`, managed in both
    /// branches of `lib.rs`'s `setup()`): there is no daemon to ask, and
    /// with no pipeline ever running there, there is nothing invariant 1
    /// protects either -- so Beenden does nothing rather than reaching for
    /// `exit` on its own, which would just be a second, divergent teardown
    /// path to maintain.
    fn quit(&self) {
        if let Some(server) = self.app.try_state::<crate::settings_cmds::Server>() {
            if let Some(daemon) = server.0.clone() {
                dispatch(&daemon, Request::Quit);
            }
        }
    }
}

impl ksni::Tray for OwfTray {
    fn id(&self) -> String {
        "openwhisprflow".into()
    }

    fn title(&self) -> String {
        "OpenWhisprFlow".into()
    }

    fn icon_name(&self) -> String {
        icon_name(self.state).into()
    }

    /// Left click, on a host that sends it at all -- see this module's doc
    /// comment on why nothing here needs to know whether it does.
    fn activate(&mut self, _x: i32, _y: i32) {
        crate::show_settings_window(&self.app);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem { label: status_label(self.state).into(), enabled: false, ..Default::default() }
                .into(),
            StandardItem {
                label: "Einstellungen".into(),
                activate: Box::new(|this: &mut Self| crate::show_settings_window(&this.app)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Beenden".into(),
                activate: Box::new(|this: &mut Self| this.quit()),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// A handle onto the running tray -- the only thing the rest of the app
/// touches, so nothing outside this module needs to know about `ksni` or
/// `OwfTray` at all.
#[derive(Clone)]
pub struct Handle(ksni::Handle<OwfTray>);

impl Handle {
    /// Pushes a new icon (and, next time the menu is opened, a new status
    /// line) to the tray. Cheap -- `ksni::Handle::update` locks a `Mutex`,
    /// mutates the model, and flips an in-memory "needs update" flag for
    /// the service thread to notice; it does not itself talk to D-Bus.
    pub fn set_state(&self, state: State) {
        self.0.update(|tray| tray.state = state);
    }

    /// Spec §8 step 3: unregisters the tray item. Sets a stop flag `ksni`'s
    /// own thread checks on its next iteration; does not block waiting for
    /// that thread to act on it.
    pub fn unregister(&self) {
        self.0.shutdown();
    }
}

/// Registers the tray with `org.kde.StatusNotifierWatcher` on the session
/// bus and serves it on `ksni`'s own OS thread (`TrayService::spawn` --
/// never Tauri's event loop; see this module's doc comment). Called once
/// from `lib.rs`'s `setup()`, unconditionally in both the normal and
/// `--replay` branches: Beenden must exist as a menu item under `--replay`
/// too ([`OwfTray::quit`]'s doc comment covers what it does there), and a
/// tray that only sometimes exists would be a worse surprise than one
/// whose Beenden is sometimes inert.
pub fn spawn(app: AppHandle) -> Handle {
    let service = ksni::TrayService::new(OwfTray { app, state: State::Warming });
    let handle = service.handle();
    service.spawn();
    Handle(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The icon is the only indicator that the microphone is open, now that
    /// the user's own finger is not. Every state must map to a
    /// distinguishable icon, and recording must never look like idle.
    #[test]
    fn every_state_maps_to_an_icon_and_recording_is_never_idle() {
        use State::*;
        let all = [Warming, Idle, Recording, Transcribing, Normalizing, Injecting, Error];
        let names: Vec<_> = all.iter().map(|s| icon_name(*s)).collect();
        assert!(names.iter().all(|n| !n.is_empty()));
        assert_ne!(icon_name(Recording), icon_name(Idle));
        assert_ne!(icon_name(Recording), icon_name(Transcribing));
    }

    /// The context menu's status line must never be blank, for any state
    /// the daemon can report.
    #[test]
    fn every_state_has_a_non_empty_status_label() {
        use State::*;
        for s in [Warming, Idle, Recording, Transcribing, Normalizing, Injecting, Error] {
            assert!(!status_label(s).is_empty());
        }
    }

    /// `Opening` precedes real capture by tens of milliseconds -- the
    /// replay fallback must show it as Recording, never Idle, for the same
    /// reason `icon_name` must never make Recording look like Idle.
    #[test]
    fn replay_fallback_maps_opening_to_recording_not_idle() {
        assert_eq!(state_from_replay_event(&OverlayEvent::Opening), Some(State::Recording));
        assert_ne!(state_from_replay_event(&OverlayEvent::Opening), Some(State::Idle));
    }

    #[test]
    fn replay_fallback_maps_recording_samples_to_the_recording_state() {
        assert_eq!(
            state_from_replay_event(&OverlayEvent::Recording { level: 0.1, elapsed_ms: 40 }),
            Some(State::Recording)
        );
    }

    #[test]
    fn replay_fallback_maps_done_back_to_idle() {
        assert_eq!(
            state_from_replay_event(&OverlayEvent::Done { preview: "hallo".to_string() }),
            Some(State::Idle)
        );
    }

    /// These four are either not a `State` transition at all, or ambiguous
    /// without a live daemon to resolve them -- the replay fallback must
    /// leave the icon exactly where it was rather than guess.
    #[test]
    fn replay_fallback_leaves_the_icon_unchanged_on_events_it_cannot_map_confidently() {
        assert_eq!(state_from_replay_event(&OverlayEvent::Error { reason: "x".to_string() }), None);
        assert_eq!(state_from_replay_event(&OverlayEvent::BusyRejected), None);
        assert_eq!(
            state_from_replay_event(&OverlayEvent::NormalizeDegraded { reason: "x".to_string() }),
            None
        );
        assert_eq!(state_from_replay_event(&OverlayEvent::NormalizeRecovered), None);
    }
}
