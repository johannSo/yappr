//! The yappr overlay: a small always-on-top window that renders
//! the dictation pipeline's state (spec 12). As of this app hosting the
//! server itself, this crate no longer talks to a daemon over a socket for
//! its own events -- `setup()` below starts `yappr_core::server` in-process
//! and forwards its broadcasts to the frontend directly as a Tauri event.
//! (`--replay <path>` (`replay.rs`) still drives the overlay from a
//! checked-in NDJSON fixture instead, with neither a pipeline nor a
//! socket.) All rendering decisions live in `src/`.

use std::sync::Arc;

mod bench;
pub mod cli;
pub mod client;
mod client_stream;
mod layer;
mod provision;
mod replay;
mod settings_cmds;
mod setup;
mod wizard;
mod tray;

use tauri::{Emitter, Manager, PhysicalPosition};

use yappr_core::proto::{OverlayEvent, Request};
use yappr_core::server::{dispatch, shutdown, Daemon, EventSink};

/// Must match the window `label` in `tauri.conf.json`.
const OVERLAY_LABEL: &str = "overlay";
/// Must match the settings window's `label` in `tauri.conf.json`.
pub(crate) const SETTINGS_LABEL: &str = "settings";
/// The transparent *canvas* the capsule is drawn on -- not the capsule's own
/// size. Spec 12 fixed both at 280 x 72 because the pill was a fixed-size
/// pill; it is now a surface that springs its width and height to whatever
/// state it is showing (a bare recording meter is narrow, a two-line "done"
/// preview is wide and tall), so the window has to be the largest box any
/// state can need rather than the size of any one of them. The capsule is
/// bottom-anchored inside it (`.stage` in `Overlay.css`), so growing upward
/// leaves its bottom edge -- the edge the user's eye tracks against the
/// screen edge -- exactly where it was.
const OVERLAY_WIDTH_LOGICAL: f64 = 420.0;
const OVERLAY_HEIGHT_LOGICAL: f64 = 120.0;
/// Default gap between the overlay's bottom edge and the screen's bottom
/// edge. Overridable with `OWF_OVERLAY_MARGIN` (logical px) -- spec 12
/// calls out "position is configurable"; this env var is the M2 mechanism
/// for that. Wiring it to `[overlay]` in `config.toml` (as the design's
/// table suggests) is future work: this crate now depends on `yappr-core`
/// for its wire types (see the module doc above), but does not yet use
/// `yappr-core`'s config loader.
///
/// This is the gap to the *window*, and the window is now a canvas larger
/// than the capsule drawn on it -- `.stage` in `Overlay.css` insets the
/// capsule another 12 px from the canvas's bottom edge. The two are split so
/// that 12 + 12 lands the capsule at the same 24 logical px off the screen
/// edge the old fixed 280x72 window sat at.
const DEFAULT_BOTTOM_MARGIN_LOGICAL: f64 = 12.0;

/// Re-applies the bottom-centre position (spec 12). A Tauri command rather
/// than a one-shot call in `setup()` because on Wayland a window has no
/// monitor to query until it has been mapped at least once --
/// `current_monitor()` returns `None` for a window that has never been
/// shown (GDK's Wayland backend has no "primary monitor" concept at all;
/// `monitor_at_window` is the only thing that works, and it needs a mapped
/// window). The frontend calls this once after every `show()` (cheap, and
/// self-correcting if the window migrates to a different output).
///
/// **Verified on this machine: this call is a no-op under Hyprland.**
/// `tao`'s Linux `set_outer_position` reaches GTK's `gtk_window_move`,
/// which upstream GTK documents as unsupported on Wayland -- toplevel
/// windows have no client-settable position in the `xdg_shell` protocol;
/// only the compositor can place them. `window.set_position()` here
/// returns `Ok(())` and the window does not move. Real bottom-centre
/// placement on Hyprland/Wayland therefore has to come from a
/// compositor-side window rule (e.g. a `move`/`center` `windowrulev2`),
/// which is Task 6's territory, not this crate's -- the M2 plan's Task 6
/// only lists float/focus/border rules, so this is a gap to add there.
/// This function is kept anyway: it is correct and will actually move the
/// window on platforms/backends where client-side positioning works
/// (X11/XWayland, macOS, Windows), and it's harmless dead weight where it
/// doesn't.
#[tauri::command]
fn position_overlay(window: tauri::WebviewWindow) {
    position_bottom_center(&window);
}

/// Shows and focuses the settings window, which `setup()` below creates
/// hidden. Best-effort -- the window is always declared in
/// `tauri.conf.json`, so `get_webview_window` returning `None` here would
/// mean that declaration was removed, not a transient failure worth
/// surfacing to whoever asked for Settings to be shown.
///
/// Shared by `TauriSink::show_settings` (`Request::ShowSettings`, the CLI's
/// `--settings` flag's path) and `tray.rs`'s `OwfTray` (left click and its
/// Settings menu item, which call this directly rather than going
/// through a `Daemon` -- see `tray.rs`'s module doc for why) so there is
/// one definition of "show Settings", not two independently-maintained
/// copies of the same two lines.
/// Shows the settings window with the first-run wizard on top of it.
///
/// Shared by `TauriSink::show_wizard` (`Request::ShowWizard`, the CLI's
/// `--wizard` flag's path) and `tray.rs`'s Setup menu item, for the
/// same reason `show_settings_window` is shared: one definition of "show the
/// wizard", not two that can drift.
///
/// The event is what puts the window into wizard mode. The window is shown
/// first, so a webview that has not mounted yet still ends up correct -- it
/// asks `wizard_state()` on mount regardless -- and an already-open one gets
/// the event.
pub(crate) fn show_wizard_window(app: &tauri::AppHandle) {
    show_settings_window(app);
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.emit("show-wizard", ());
    }
}

pub(crate) fn show_settings_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.show();
        let _ = w.set_focus();
        // This webview is hidden on close and never destroyed (see the
        // `hide_instead_of_close` calls in `setup`), so it reads the config
        // exactly once -- at app launch -- and its autosave posts that whole
        // snapshot back through `SetConfig`. Anything that writes
        // `config.toml` behind its back is therefore reverted by the next
        // unrelated toggle -- and the writer behind its back is in this
        // app, not outside it: `wizard::wizard_finish` patches
        // `inject.backend` through `set_config`, which on GNOME is the
        // difference between a working injector and a `wtype` that types
        // nothing at all. Re-reading on every reveal is the only moment an
        // already-mounted window can learn the file moved on.
        //
        // Emitted after `show` for the same reason `show_wizard_window`
        // emits after it: a webview that has not mounted yet loads on mount
        // anyway, and one that has needs telling. `show_wizard_window`
        // funnels through here and so fires this too -- harmless, because
        // the frontend's refresh is quiet (it raises no loading state) and
        // skips itself outright when the window has an edit of its own still
        // pending, in flight, or rejected.
        let _ = w.emit("show-settings", ());
    }
}

/// Forwards every broadcast from the in-process `yappr_core::server::Daemon`
/// to the frontend as a Tauri event, and to the tray icon -- the overlay's
/// `src/Overlay.tsx` already listens for `"overlay-event"`; it used to
/// arrive there via `connection.rs`'s socket client, and now arrives the
/// same way but without a socket in between. The tray (Task 12) needs the
/// same broadcasts the overlay does, so this is the one place both are
/// fed from, rather than a second subscription.
struct TauriSink {
    app: tauri::AppHandle,
    tray: tray::Handle,
}

/// How long injection is delayed after hiding an overlay that had keyboard
/// focus, giving the compositor time to unmap it and return focus to the
/// previously focused window -- the dictation's actual target. Mutter
/// refocuses well inside this on 2026 hardware; generous on purpose, since
/// the cost is a one-off pause before typing starts and the alternative is
/// the transcript landing in the overlay.
const FOCUS_RETURN_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Invariant 2's failure detector: true when the overlay holds keyboard
/// focus at the moment that matters -- `Injecting`, right before the
/// injector spawns. On a layer-shell compositor this can never be true
/// (`KeyboardMode::None` refuses focus at the protocol level); on
/// GNOME/Mutter it is routinely true, because `focusable: false` reaches
/// GTK's `accept_focus` -- an X11 mechanism with no Wayland counterpart --
/// and Mutter focuses the freshly mapped toplevel. A focus-following
/// injector (wtype, and any paste script that presses keys) would then type the
/// transcript into the overlay itself.
fn overlay_hijacks_injection(event: &OverlayEvent, overlay_focused: bool) -> bool {
    overlay_focused && matches!(event, OverlayEvent::Injecting)
}

impl EventSink for TauriSink {
    fn emit(&self, event: &OverlayEvent) {
        if let Err(e) = self.app.emit("overlay-event", event) {
            eprintln!("overlay: failed to emit event to the frontend: {e}");
        }
        self.refresh_tray_icon(event);
        // Last, so the frontend has already been told about `Injecting` --
        // its handler deliberately stops calling `show()` for that state
        // (see `Overlay.tsx`), so nothing re-maps the window while this
        // blocks the pipeline thread.
        self.hide_overlay_if_it_hijacks_injection(event);
    }

    /// `Request::ShowSettings` (spec §8, the CLI's `--settings` flag).
    fn show_settings(&self) {
        show_settings_window(&self.app);
    }

    /// `Request::ShowWizard` (the CLI's `--wizard` flag, and the tray).
    fn show_wizard(&self) {
        show_wizard_window(&self.app);
    }

    /// Spec §8 step 3.
    fn unregister_tray(&self) {
        self.tray.unregister();
    }

    /// `Request::Restart`'s last step, called from that arm's thread after
    /// `shutdown` and immediately before `std::process::exit(0)`.
    ///
    /// Started with **no arguments**, deliberately, rather than forwarding
    /// this process's own. Two reasons: the only argv that reaches a running
    /// app is the no-args start (every flag in `cli::route` is a client call
    /// that exits before Tauri initialises), and `--replay` is the one
    /// exception -- a replay session must not resurrect itself as a second
    /// replay, which is what forwarding argv (Tauri's own `restart` does
    /// exactly that) would do.
    ///
    /// A spawn failure is logged and swallowed: the caller exits either way,
    /// and there is nothing left to fall back to -- `shutdown` has already
    /// run. The user sees the app close, which is a bad outcome but a
    /// legible one, and `yappr` starts normally afterwards.
    fn relaunch(&self) {
        let path = match successor_binary() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("restart: cannot resolve this binary's path, not relaunching: {e}");
                return;
            }
        };
        match std::process::Command::new(&path).spawn() {
            Ok(child) => {
                tracing::info!(pid = child.id(), path = %path.display(), "restart: successor started")
            }
            Err(e) => eprintln!("restart: failed to start {}: {e}", path.display()),
        }
    }
}

/// Which file [`TauriSink::relaunch`] should start.
///
/// **Not `tauri::process::current_binary`.** That was the first
/// implementation and it is actively wrong here, which a live restart on this
/// machine demonstrated the first time it ran: it returns `$APPIMAGE`
/// whenever that variable is merely *set*, without ever checking that this
/// process is the AppImage in question. `$APPIMAGE` is inherited like any
/// other environment variable, so a yappr launched from a terminal that is
/// itself packaged as an AppImage sees its *ancestor's* path -- and the
/// restart dutifully started a copy of the unrelated editor the developer
/// happened to be running, while yappr itself stayed down.
///
/// The variable is still the right answer when it really is ours, and that
/// case has to be handled: under an AppImage `current_exe` names a path
/// inside the mount (`/tmp/.mount_yapprXXXX/usr/bin/yappr`), and the runtime
/// unmounts it as this process exits, so a successor started from there would
/// be running out of a directory being pulled out from under it.
///
/// The discriminator is `$APPDIR`, which the AppImage runtime sets to the
/// mount root of *the AppImage whose payload is running*. If this executable
/// lives inside it, `$APPIMAGE` names the outer file and is correct; if it
/// does not, both variables belong to somebody else's AppImage and
/// `current_exe` is correct.
fn successor_binary() -> std::io::Result<std::path::PathBuf> {
    Ok(pick_successor_binary(
        std::env::current_exe()?,
        std::env::var_os("APPIMAGE").map(std::path::PathBuf::from),
        std::env::var_os("APPDIR").map(std::path::PathBuf::from),
    ))
}

/// [`successor_binary`]'s decision, with the environment passed in so the
/// wrong answer that shipped first is a test rather than a story.
fn pick_successor_binary(
    current_exe: std::path::PathBuf,
    appimage: Option<std::path::PathBuf>,
    appdir: Option<std::path::PathBuf>,
) -> std::path::PathBuf {
    match (appimage, appdir) {
        (Some(appimage), Some(appdir))
            if !appimage.as_os_str().is_empty()
                && !appdir.as_os_str().is_empty()
                && current_exe.starts_with(&appdir) =>
        {
            appimage
        }
        _ => current_exe,
    }
}

impl TauriSink {
    /// Pushes the tray icon's next `State`, if any -- the decision itself
    /// is [`tray::icon_state_for`], a pure function this method's only job
    /// is to feed. See that function's doc comment (and `tray.rs`'s module
    /// doc, "Icon and daemon state") for the full reasoning; in short,
    /// deriving from `event` beats re-asking `Request::Status` on every
    /// broadcast (review round 1's bug), and `Request::Status` is asked at
    /// all only for `OverlayEvent::Error` (review round 2: this call
    /// pattern, not `tray::state_from_event`'s match body, was where that
    /// bug actually lived and was invisible to every test).
    ///
    /// `try_state` rather than `state` for that lookup: it can in principle
    /// run before `setup()` has finished calling `app.manage(Server(..))`
    /// (warm-up is spawned from inside `yappr_core::server::start`, before it
    /// returns to `setup()`), in which case `status` below is `None` for
    /// that one `Error` -- harmless: `icon_state_for` treats a `None`
    /// status as "nothing to resolve `Error` with", same as `--replay`.
    fn refresh_tray_icon(&self, event: &OverlayEvent) {
        // Only `Error` needs `Request::Status` at all (see
        // `tray::icon_state_for`'s doc comment) -- every other event is
        // resolved by `tray::state_from_event` alone, so this must not
        // dispatch on the 20 Hz `Recording` path.
        let status = if matches!(event, OverlayEvent::Error { .. }) {
            self.app
                .try_state::<settings_cmds::Server>()
                .and_then(|server| server.0.clone())
                .and_then(|daemon| dispatch(&daemon, Request::Status).state)
        } else {
            None
        };
        if let Some(state) = tray::icon_state_for(event, status) {
            self.tray.set_state(state);
        }
    }

    /// Invariant 2's last line of defense, for compositors where neither
    /// layer-shell (`layer.rs`) nor a pasted window rule (`hypr.rs`) exists
    /// to keep the overlay unfocused -- GNOME/Mutter in practice. If the
    /// overlay holds keyboard focus when `Injecting` is broadcast, it is
    /// hidden natively and the broadcast blocks [`FOCUS_RETURN_DELAY`] so
    /// the compositor can hand focus back to the target window before the
    /// injector spawns. The pipeline calls this sink synchronously right
    /// before `inject_with_recovery`, which is exactly what makes blocking
    /// here effective -- and safe: `Injecting` is only ever broadcast from
    /// the pipeline's own worker thread, never the GTK main thread this
    /// would otherwise freeze.
    ///
    /// The early `matches!` keeps the per-event cost at one enum check for
    /// the ~20 Hz `Recording` stream; `is_focused` (one IPC round trip to
    /// the main loop) runs once per dictation at most.
    fn hide_overlay_if_it_hijacks_injection(&self, event: &OverlayEvent) {
        if !matches!(event, OverlayEvent::Injecting) {
            return;
        }
        let Some(window) = self.app.get_webview_window(OVERLAY_LABEL) else {
            return;
        };
        let focused = window.is_focused().unwrap_or(false);
        if !overlay_hijacks_injection(event, focused) {
            return;
        }
        tracing::warn!(
            delay_ms = FOCUS_RETURN_DELAY.as_millis() as u64,
            "invariant 2 failed: the overlay holds keyboard focus at injection time \
             (compositor without layer-shell?); hiding it and waiting for focus to \
             return to the target window"
        );
        if let Err(e) = window.hide() {
            tracing::error!(error = %e, "could not hide the focused overlay before injection");
            return;
        }
        std::thread::sleep(FOCUS_RETURN_DELAY);
    }
}

/// Called once by the frontend, immediately after it starts listening for
/// `"overlay-event"` (`src/Overlay.tsx`) -- replays whatever state the
/// daemon is in *right now* as a fresh `"overlay-event"` emission, via
/// `Daemon::connect_snapshot`. That is the same snapshot
/// `yappr_core::server::serve_subscriber` sends a freshly connected socket
/// subscriber; the overlay stopped being a socket client, but still needs
/// the same "what's happening right now" answer on attach.
///
/// Without this, nothing shows during the several-second warm-up (nothing
/// ever *broadcasts* `Warming` -- `yappr_core::server::start` only stores it),
/// and a warm-up failure broadcast from a background thread that happened to
/// outrun this window's `listen()` registration would be lost to the
/// overlay forever. Calling this only after `listen()`'s own promise has
/// resolved (rather than e.g. on page load, which races React's effect-based
/// registration) is what removes that race by construction: the listener
/// this replays into already exists by the time it's asked to replay.
///
/// A no-op the frontend ignores in `--replay` mode: no `Daemon` is ever
/// `app.manage`d there (replay drives the overlay from the fixture file
/// directly), so this command's `State` extraction itself fails before the
/// body below ever runs, and Tauri reports that as a rejected promise
/// rather than a panic.
#[tauri::command]
fn overlay_ready(app: tauri::AppHandle, daemon: tauri::State<'_, Arc<Daemon>>) {
    for event in daemon.connect_snapshot() {
        if let Err(e) = app.emit("overlay-event", &event) {
            eprintln!("overlay: failed to emit event to the frontend: {e}");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let replay_path = replay_arg();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            position_overlay,
            overlay_ready,
            settings_cmds::get_config,
            settings_cmds::set_config,
            settings_cmds::list_input_devices,
            settings_cmds::autostart_status,
            settings_cmds::set_autostart,
            settings_cmds::restart_app,
            settings_cmds::app_version,
            settings_cmds::theme,
            provision::setup_status,
            provision::run_setup,
            wizard::wizard_state,
            wizard::wizard_finish,
            wizard::wizard_dismiss
        ])
        .setup(move |app| {
            let window = app
                .get_webview_window(OVERLAY_LABEL)
                .expect("the overlay window must be declared in tauri.conf.json");

            // Spec §6: layer-shell must be applied before the window is
            // realized, so this runs before anything else touches `window`.
            // `anchor_overlay` returns `Err` when the compositor doesn't
            // implement wlr-layer-shell (Mutter) -- the window then stays
            // an ordinary toplevel, and invariant 2 (never take keyboard
            // focus) is NOT enforceable up front on that path, despite what
            // this comment used to claim: `tauri.conf.json`'s
            // `focusable: false` reaches GTK's `accept_focus`, an X11
            // mechanism with no Wayland counterpart (xdg_shell offers a
            // toplevel no way to refuse focus -- that gap is the whole
            // reason layer-shell's KeyboardMode exists), and
            // `yappr_core::hypr::shortcut_config`'s title-matched
            // `nofocus` rule is Hyprland config, which does not exist on
            // the very compositors that lack layer-shell. Verified on
            // Fedora/GNOME: Mutter focuses the overlay the moment it maps,
            // and the injector then types the transcript into it. The
            // enforcement on this path is therefore at injection time
            // instead: `TauriSink::hide_overlay_if_it_hijacks_injection`
            // hides a focused overlay on `Injecting` and delays the
            // injector until focus has returned to the target window.
            if let Err(e) = layer::anchor_overlay(&window) {
                eprintln!("overlay: layer-shell unavailable ({e}); falling back to a toplevel");
            }
            relax_webkitgtk_minimum_size(&window);

            // Neither window is ever destroyed by a close request -- only
            // hidden. For `settings` this is spec §8 in so many words
            // ("closing settings hides it; it does not exit the app"). For
            // `overlay` it is load-bearing for a different reason: a
            // destroyed `overlay` leaves `TauriSink::emit` failing to stderr
            // forever (there is no window left to receive "overlay-event"),
            // with no way to get it back short of restarting the app -- and,
            // per `run`'s `RunEvent::Exit` comment below, does *not* cause
            // the app to exit either, since `settings` (hidden, not
            // destroyed) keeps Tauri's window map non-empty. The app quits
            // only via Quit or `--quit` -- both `Request::Quit` -- never
            // by closing a window.
            hide_instead_of_close(&window, || {});
            if let Some(settings) = app.get_webview_window(SETTINGS_LABEL) {
                // Closing the settings window is the wizard's exit with no
                // button, and the one a user who has finished setup actually
                // reaches for -- the wizard *is* this window while it is
                // active, so its window controls are the wizard's window
                // controls. Nothing wrote the marker on this path, which is
                // how an install with every model present and the shortcut
                // bound still got the whole wizard on every launch
                // (`wizard::remember_setup_seen` records the report).
                //
                // Unconditional, not "only while the wizard is active": this
                // hook cannot see the webview's mode, and it does not need
                // to. A settings window that has been opened and closed is a
                // machine where yappr has introduced itself, which is all the
                // marker claims. One `write` of an empty file, on a thread
                // that is about to hide a window anyway.
                hide_instead_of_close(&settings, || {
                    let _ = wizard::remember_setup_seen();
                });
            }

            // Task 12: registers with `org.kde.StatusNotifierWatcher` and
            // serves on `ksni`'s own OS thread -- never this one, and never
            // the Tauri event loop once it starts (see `tray.rs`'s module
            // doc). Spawned once, unconditionally, before the branch below:
            // both arms need a `tray::Handle`, and Quit must exist as a
            // menu item under `--replay` too (see `tray::OwfTray::quit`).
            let tray_handle = tray::spawn(app.handle().clone());

            // Never on the Tauri event-loop thread: a not-yet-warm daemon,
            // or a slow/looping replay file, must not delay first paint or
            // freeze window management.
            match replay_path.clone() {
                Some(path) => {
                    // No `Daemon` exists in replay mode at all. Managed
                    // unconditionally (see both arms) so a settings command's
                    // `State<Server>` extraction never fails here either --
                    // it reports a stated error instead.
                    app.manage(settings_cmds::Server(None));
                    let handle = app.handle().clone();
                    let tray_for_replay = tray_handle.clone();
                    let emit = move |event: OverlayEvent| {
                        if let Err(e) = handle.emit("overlay-event", &event) {
                            eprintln!("overlay: failed to emit event to the frontend: {e}");
                        }
                        // No `Daemon` to ask `Request::Status` of for the
                        // `Error` case, unlike `TauriSink::refresh_tray_icon`
                        // -- see `tray::state_from_event`'s doc comment: an
                        // `Error` broadcast here just leaves the icon as it
                        // is, since this closure has no way to tell a fatal
                        // failure from a transient one.
                        if let Some(state) = tray::state_from_event(&event) {
                            tray_for_replay.set_state(state);
                        }
                    };
                    eprintln!("overlay: replaying {} (not connecting to the daemon)", path.display());
                    std::thread::spawn(move || replay::run(&path, emit));
                }
                None => {
                    let sink =
                        Arc::new(TauriSink { app: app.handle().clone(), tray: tray_handle.clone() });
                    let (daemon, listener) = yappr_core::server::start(sink)?;
                    app.manage(daemon.clone());
                    app.manage(settings_cmds::Server(Some(daemon.clone())));
                    // Never on the Tauri event-loop thread -- the accept
                    // loop blocks, and a blocked event loop is a frozen
                    // window and an unclickable tray.
                    std::thread::spawn(move || yappr_core::server::serve(daemon, listener));

                    // Now that the tray (Task 12) exists, Settings is
                    // always reachable -- this is a redundant safety net,
                    // not the only way in, for a first-run user who has not
                    // yet noticed the tray icon. One `stat` of the wizard
                    // marker, off the event-loop thread because the window
                    // is shown from here too: it opens Settings on a machine
                    // that has never finished setup, and does nothing on
                    // every other run, however healthy or broken the install
                    // is (invariant 13).
                    let setup_check_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        if wizard::should_show_settings_at_startup() {
                            if let Some(w) = setup_check_handle.get_webview_window(SETTINGS_LABEL) {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    });
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Task 10 / spec §8: every exit route must converge on
    // `yappr_core::server::shutdown`, not just the signal handler inside
    // `yappr_core::server::start`. `Request::Quit` (Quit -- Task 12's
    // `tray::OwfTray::quit` dispatches this and nothing else, deliberately,
    // see that function's doc comment -- and `--quit`) already calls
    // `shutdown` directly, on its own background thread, before its
    // `std::process::exit(0)` -- and `exit` tears the whole process down
    // immediately, on every thread, without ever giving Tauri's event loop a
    // chance to run this closure. So `RunEvent::Exit` never fires on that
    // path, and it doesn't need to: `shutdown` already ran.
    //
    // This hook exists for every *other* way the event loop could end.
    // Tauri's own default reaction, `RunEvent::ExitRequested` -> `Exit`,
    // fires when its window map becomes empty (every window has reached
    // `Destroyed`) -- checked against `tauri-runtime-wry` 2.11.4's source,
    // not assumed. As of the `hide_instead_of_close` calls above, *neither*
    // window this app manages is ever destroyed by a close request, so that
    // path cannot currently fire at all; a compositor-issued close of either
    // window is a no-op for the app's lifetime by design (see the comment on
    // those calls). What this hook actually guards is a *programmatic* exit
    // -- `AppHandle::exit`/`restart`, which nothing in this codebase calls
    // today but which would also raise `RunEvent::Exit` with no `Request`
    // for `shutdown` to hang off. Kept for that case: `shutdown`
    // is idempotent (`SHUTTING_DOWN`), so a hook that never fires today costs
    // nothing, and one that starts firing tomorrow (e.g. if a window is ever
    // allowed to actually close) is exactly the safety net Task 10 exists
    // to provide. `try_state`, not `state`: in `--replay` mode no
    // `Arc<Daemon>` is ever `app.manage`d, and this callback must not panic
    // in that mode.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(daemon) = app_handle.try_state::<Arc<Daemon>>() {
                shutdown(&daemon);
            }
        }
    });
}

/// Intercepts `window`'s `CloseRequested` so a close request hides it
/// instead of destroying it. Applied to both `overlay` and `settings` in
/// `run`'s `setup` -- see the comment on those two calls for why neither
/// window may ever actually be destroyed, and `run`'s `RunEvent::Exit`
/// comment for the exit-convergence consequence of that.
///
/// `after_hide` runs once the window is away, for the one caller that has
/// something to record about the close itself: `settings` remembers that
/// setup has been seen, because closing that window is the setup wizard's
/// unbuttoned exit (see the call site). The overlay passes `|| {}`.
fn hide_instead_of_close(window: &tauri::WebviewWindow, after_hide: impl Fn() + Send + 'static) {
    let w = window.clone();
    window.on_window_event(move |e| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = e {
            api.prevent_close();
            let _ = w.hide();
            after_hide();
        }
    });
}

/// `--replay <path>` switches the overlay from subscribing to the daemon
/// socket to replaying a checked-in NDJSON `OverlayEvent` log -- see
/// `replay.rs` and `fixtures/replay-full.ndjson`. This is the acceptance
/// evidence for every overlay state without a microphone (M2 plan, Task 5).
fn replay_arg() -> Option<std::path::PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let idx = args.iter().position(|a| a == "--replay")?;
    args.get(idx + 1).map(std::path::PathBuf::from)
}

fn bottom_margin_logical() -> f64 {
    std::env::var("OWF_OVERLAY_MARGIN")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_BOTTOM_MARGIN_LOGICAL)
}

/// WebKitGTK's `WebKitWebView` widget reports its own internal minimum
/// natural size to GTK's layout system, and GTK's box packing refuses to
/// shrink a window below its child's minimum -- regardless of
/// `tauri.conf.json`'s `width`/`height`/`minHeight`. Confirmed empirically
/// on this machine: with every size field in config set to 72px tall, the
/// window still mapped at ~200 logical px tall; the *width* fields were
/// honoured exactly, only height was floored. `gtk_widget_set_size_request`
/// with `(-1, -1)` tells the widget "use whatever the parent allocates,
/// don't request a natural size of your own", which removes the floor.
/// No-op on non-Linux targets, where this widget-level minimum doesn't
/// apply (`PlatformWebview::inner()` isn't even compiled there).
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn relax_webkitgtk_minimum_size(window: &tauri::WebviewWindow) {
    use gtk::prelude::{GtkWindowExt, WidgetExt};
    let result = window.with_webview(|webview| {
        webview.inner().set_size_request(-1, -1);
    });
    if let Err(e) = result {
        eprintln!("overlay: could not relax the WebKitGTK webview minimum size: {e}");
    }
    match window.gtk_window() {
        Ok(gtk_window) => {
            let w = OVERLAY_WIDTH_LOGICAL as i32;
            let h = OVERLAY_HEIGHT_LOGICAL as i32;
            gtk_window.set_size_request(w, h);
            gtk_window.resize(w, h);
        }
        Err(e) => eprintln!("overlay: could not access the raw GTK window: {e}"),
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn relax_webkitgtk_minimum_size(_window: &tauri::WebviewWindow) {}

/// Positions the overlay at the bottom-centre of its current monitor's work
/// area (spec 12) -- the work area rather than the full monitor size so the
/// overlay doesn't sit under a Waybar-style reserved strut. Best-effort: if
/// monitor info isn't available (the window hasn't been mapped yet, or is
/// running headless), it's a no-op and the window keeps whatever position
/// it already had.
fn position_bottom_center(window: &tauri::WebviewWindow) {
    let Ok(Some(monitor)) = window.current_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let work_area = monitor.work_area();
    let win_w = (OVERLAY_WIDTH_LOGICAL * scale).round() as i32;
    let win_h = (OVERLAY_HEIGHT_LOGICAL * scale).round() as i32;
    let margin = (bottom_margin_logical() * scale).round() as i32;

    let x = work_area.position.x + (work_area.size.width as i32 - win_w) / 2;
    let y = work_area.position.y + work_area.size.height as i32 - win_h - margin;
    // NB: on Wayland this is best-effort and, on this machine's Hyprland
    // session, empirically a no-op -- see the module doc on
    // `position_overlay` and the report for what actually controls
    // placement there (a compositor-side window rule, Task 6's territory).
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The bug this function exists to prevent, reproduced. The first
    /// implementation used `tauri::process::current_binary`, which returns
    /// `$APPIMAGE` whenever that variable is set at all -- and it is an
    /// ordinary inherited variable, so a yappr started from a terminal that
    /// is itself an AppImage sees the terminal's path. On the first live
    /// restart on the developer's machine that is exactly what happened: the
    /// restart started a second copy of an unrelated editor and left yappr
    /// down. `$APPDIR` is what distinguishes the two cases, because the
    /// runtime points it at the mount of the AppImage whose payload is
    /// actually running.
    #[test]
    fn an_inherited_appimage_variable_from_some_other_app_is_ignored() {
        let exe = PathBuf::from("/home/joni/yappr/target/debug/yappr");
        assert_eq!(
            pick_successor_binary(
                exe.clone(),
                Some(PathBuf::from("/home/joni/AppImages/some-editor.appimage")),
                Some(PathBuf::from("/tmp/.mount_some-editorAbC123")),
            ),
            exe,
            "this executable is not inside that AppImage's mount, so the variables are not ours"
        );
    }

    /// The case the variable is genuinely for: running as the AppImage's own
    /// payload, where `current_exe` is inside a mount the runtime unmounts as
    /// this process exits -- so a successor started from it would be running
    /// out of a directory being pulled out from under it.
    #[test]
    fn our_own_appimage_is_relaunched_by_its_outer_path_not_its_mount_path() {
        assert_eq!(
            pick_successor_binary(
                PathBuf::from("/tmp/.mount_yapprXy9z/usr/bin/yappr"),
                Some(PathBuf::from("/home/joni/AppImages/yappr.appimage")),
                Some(PathBuf::from("/tmp/.mount_yapprXy9z")),
            ),
            PathBuf::from("/home/joni/AppImages/yappr.appimage"),
        );
    }

    /// The ordinary case, and the two half-set ones. An empty variable is
    /// treated as unset because that is how a shell exports a cleared one.
    #[test]
    fn without_a_matching_appimage_pair_the_current_executable_is_used() {
        let exe = PathBuf::from("/usr/bin/yappr");
        for (appimage, appdir) in [
            (None, None),
            (Some("/home/joni/AppImages/yappr.appimage"), None),
            (None, Some("/tmp/.mount_yapprXy9z")),
            (Some(""), Some("/tmp/.mount_yapprXy9z")),
            (Some("/home/joni/AppImages/yappr.appimage"), Some("")),
        ] {
            assert_eq!(
                pick_successor_binary(
                    exe.clone(),
                    appimage.map(PathBuf::from),
                    appdir.map(PathBuf::from),
                ),
                exe,
                "appimage={appimage:?} appdir={appdir:?}"
            );
        }
    }

    #[test]
    fn a_focused_overlay_hijacks_injection_only_at_the_injecting_event() {
        // Invariant 2's failure mode on GNOME/Mutter: no layer-shell, so the
        // overlay toplevel takes keyboard focus and a focus-following
        // injector (wtype, and any paste script that presses keys) types the transcript
        // into the overlay itself. The moment that matters is Injecting.
        assert!(overlay_hijacks_injection(&OverlayEvent::Injecting, true));
        assert!(!overlay_hijacks_injection(&OverlayEvent::Injecting, false));
        assert!(!overlay_hijacks_injection(&OverlayEvent::Idle, true));
        assert!(!overlay_hijacks_injection(
            &OverlayEvent::Error { reason: "x".to_string() },
            true
        ));
    }
}
