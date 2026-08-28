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
//! [`state_from_event`] derives the tray's `State` directly from the
//! `OverlayEvent` just broadcast, for every event except `Error`.
//!
//! **Review round 1 first tried the opposite** -- re-asking
//! `Request::Status` on every broadcast, on the theory that the tray should
//! never disagree with the daemon's own idea of its state. That was wrong:
//! `daemon.state` and its broadcasts are not consistently ordered. `Done` is
//! broadcast while `daemon.state` is still `INJECTING` (`server.rs`'s
//! stage-events closure stores `NORMALIZING`/`INJECTING` there *before*
//! `process_utterance` broadcasts `Done`); the transient "no speech
//! detected" `Error` is broadcast while `daemon.state` is still
//! `TRANSCRIBING`. `IdleOnExit`'s later reset to `IDLE` (`server.rs`)
//! broadcasts nothing at all. Asking `Status` at broadcast time therefore
//! read the *pre-transition* state on every completed dictation, leaving
//! the tray stuck on the busy/loading icon for the entire idle period after
//! -- i.e. essentially always. Deriving straight from the event (`Done` ->
//! `Idle` immediately, not "whatever `state_of` says right now") is exact
//! where re-asking `Status` was not, and it is cheaper too: no
//! `tauri::State` lookup, no extra `Mutex` lock, no `serde_json`
//! allocation, on what is otherwise a 20 Hz path while recording.
//!
//! The one event [`state_from_event`] cannot resolve on its own is `Error`:
//! it is broadcast identically for the one fatal, permanent warm-up failure
//! (`State::Error`) and for an ordinary per-utterance failure (which leaves
//! the daemon genuinely `Idle` again immediately), and `State::Error`'s own
//! doc comment says the two must stay distinct. Only for that one case,
//! `TauriSink::refresh_tray_icon` (`lib.rs`) still asks `Request::Status`:
//! `State::Error` is set only on the warm-up path (`server.rs`'s `warm_up`
//! `Err` arm, before it ever broadcasts), so "`Status` reports anything
//! other than `State::Error`" unambiguously means this was the transient
//! case, mapped to `Idle`. `--replay` has no `Daemon` to ask, so its
//! `Error` broadcasts are simply left unmapped by [`state_from_event`] and
//! do not move the tray's icon.
//!
//! ## Tray callbacks must not call `dispatch` inline
//!
//! `ksni` 0.2.2 (`service.rs`) runs both `Tray::activate` and every menu
//! item's `activate` inside `update_immediately`, which holds this tray's
//! model `Mutex` for the callback's entire duration. `Handle::update` --
//! what [`Handle::set_state`] calls -- locks that exact same, non-reentrant
//! `Mutex`. So a tray callback that synchronously triggers
//! `Daemon::broadcast` (-> `TauriSink::emit` -> `refresh_tray_icon` ->
//! `Handle::set_state`) on `ksni`'s own thread would try to lock a `Mutex`
//! it is already holding, on the same thread -- a guaranteed, permanent
//! self-deadlock: frozen icon, dead menu, dead Beenden, recoverable only by
//! killing the process from outside.
//!
//! Not reachable today: `Request::Quit` spawns its wait-then-shutdown work
//! on a fresh thread and broadcasts nothing on the calling one, and
//! `show_settings_window` broadcasts nothing at all. But it is a real trap
//! for the next `dispatch` call a tray callback adds -- "Diktat pausieren"
//! is exactly such a case -- so every `dispatch` call reachable from a tray
//! callback runs on its own freshly spawned thread ([`OwfTray::quit`] is
//! the current example), never inline in the callback, on principle rather
//! than because today's one call happens to be safe.

use std::sync::{Arc, Mutex, PoisonError};

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

/// The tray's primary state derivation, used by both the real branch
/// (`TauriSink::refresh_tray_icon`, `lib.rs`) and `--replay`
/// (`lib.rs`'s replay `emit` closure) -- see this module's doc comment
/// ("Icon and daemon state") for why deriving from the event beats asking
/// `Request::Status` at broadcast time. `None` for events that either are
/// not a `State` transition at all (`BusyRejected`; the
/// `NormalizeDegraded`/`NormalizeRecovered` badge, which its own doc
/// comment says is not one either) or are ambiguous without a live daemon
/// to resolve (`Error`, which covers both the one fatal warm-up failure and
/// an ordinary per-utterance failure that leaves the daemon `Idle` again --
/// only `TauriSink::refresh_tray_icon` can tell those apart, by asking
/// `Request::Status`; `--replay` has no daemon to ask, so its `Error`
/// broadcasts leave the icon exactly as it was).
pub(crate) fn state_from_event(event: &OverlayEvent) -> Option<State> {
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
    ///
    /// Dispatched on a freshly spawned thread, never inline here -- see
    /// this module's doc comment ("Tray callbacks must not call `dispatch`
    /// inline") for the self-deadlock a synchronously-broadcasting
    /// `dispatch` call would cause on `ksni`'s own thread. `Request::Quit`
    /// itself broadcasts nothing synchronously today, but the thread hop
    /// costs nothing and keeps this call site correct regardless.
    fn quit(&self) {
        if let Some(server) = self.app.try_state::<crate::settings_cmds::Server>() {
            if let Some(daemon) = server.0.clone() {
                std::thread::spawn(move || {
                    dispatch(&daemon, Request::Quit);
                });
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
pub struct Handle {
    inner: ksni::Handle<OwfTray>,
    /// The `State` last pushed to `inner`, held outside `OwfTray`'s own
    /// model `Mutex` so [`Handle::set_state`] can decide whether there is
    /// anything to push at all without taking that lock.
    ///
    /// Review round 1 finding: `set_state` used to call `inner.update`
    /// unconditionally, and `ksni::Handle::update` (`lib.rs` in the vendored
    /// crate) flags "needs update" unconditionally too, regardless of
    /// whether the closure actually changed anything. During a whole
    /// recording, ~20 identical `State::Recording` values a second were
    /// each triggering a full `update_properties`/`menu()` rebuild on
    /// `ksni`'s D-Bus thread, and each taking the same model `Mutex` the
    /// audio-capture thread was calling `set_state` through, roughly every
    /// 50 ms, for as long as recording lasted.
    last: Arc<Mutex<State>>,
}

impl Handle {
    /// Pushes a new icon (and, next time the menu is opened, a new status
    /// line) to the tray -- but only if `state` differs from what was last
    /// pushed (see [`Handle`]'s doc comment on `last`); otherwise this is
    /// one uncontended `Mutex` lock and nothing else; `ksni`'s own model
    /// lock is never touched. When it does differ, `ksni::Handle::update`
    /// locks that model `Mutex`, mutates it, and flips an in-memory "needs
    /// update" flag for the service thread to notice; it does not itself
    /// talk to D-Bus.
    pub fn set_state(&self, state: State) {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if *last == state {
            return;
        }
        *last = state;
        drop(last);
        self.inner.update(|tray| tray.state = state);
    }

    /// Spec §8 step 3: unregisters the tray item. Sets a stop flag `ksni`'s
    /// own thread checks on its next iteration; does not block waiting for
    /// that thread to act on it.
    pub fn unregister(&self) {
        self.inner.shutdown();
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
    let inner = service.handle();
    service.spawn();
    Handle { inner, last: Arc::new(Mutex::new(State::Warming)) }
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

    /// `Opening` precedes real capture by tens of milliseconds -- this must
    /// show it as Recording, never Idle, for the same reason `icon_name`
    /// must never make Recording look like Idle.
    #[test]
    fn state_from_event_maps_opening_to_recording_not_idle() {
        assert_eq!(state_from_event(&OverlayEvent::Opening), Some(State::Recording));
        assert_ne!(state_from_event(&OverlayEvent::Opening), Some(State::Idle));
    }

    #[test]
    fn state_from_event_maps_recording_samples_to_the_recording_state() {
        assert_eq!(
            state_from_event(&OverlayEvent::Recording { level: 0.1, elapsed_ms: 40 }),
            Some(State::Recording)
        );
    }

    /// `Done` means the daemon is back to `Idle` -- immediately, from the
    /// event alone, not from re-asking `Request::Status` (review round 1:
    /// `daemon.state` is still `INJECTING` at the moment `Done` is
    /// broadcast, so asking `Status` here would read the wrong side of the
    /// transition).
    #[test]
    fn state_from_event_maps_done_back_to_idle_without_asking_status() {
        assert_eq!(
            state_from_event(&OverlayEvent::Done { preview: "hallo".to_string() }),
            Some(State::Idle)
        );
    }

    /// These four are either not a `State` transition at all, or ambiguous
    /// without a live daemon to resolve them -- `state_from_event` must
    /// leave the icon exactly where it was (`None`) rather than guess.
    /// `TauriSink::refresh_tray_icon` (`lib.rs`) is what resolves `Error`
    /// the rest of the way, by asking `Request::Status`.
    #[test]
    fn state_from_event_leaves_error_and_non_state_badges_unmapped() {
        assert_eq!(state_from_event(&OverlayEvent::Error { reason: "x".to_string() }), None);
        assert_eq!(state_from_event(&OverlayEvent::BusyRejected), None);
        assert_eq!(
            state_from_event(&OverlayEvent::NormalizeDegraded { reason: "x".to_string() }),
            None
        );
        assert_eq!(state_from_event(&OverlayEvent::NormalizeRecovered), None);
    }
}
