//! The OpenWhisprFlow overlay: a small always-on-top window that renders
//! the dictation pipeline's state (spec 12). As of this app hosting the
//! server itself, this crate no longer talks to a daemon over a socket for
//! its own events -- `setup()` below starts `owf_core::server` in-process
//! and forwards its broadcasts to the frontend directly as a Tauri event.
//! (`--replay <path>` (`replay.rs`) still drives the overlay from a
//! checked-in NDJSON fixture instead, with neither a pipeline nor a
//! socket.) All rendering decisions live in `src/`.

use std::sync::Arc;

mod bench;
pub mod cli;
pub mod client;
mod client_stream;
mod replay;
mod settings_cmds;
mod setup;

use tauri::{Emitter, Manager, PhysicalPosition};

use owf_core::proto::OverlayEvent;
use owf_core::server::{shutdown, Daemon, EventSink};

/// Must match the window `label` in `tauri.conf.json`.
const OVERLAY_LABEL: &str = "overlay";
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
/// table suggests) is future work: this crate now depends on `owf-core`
/// for its wire types (see the module doc above), but does not yet use
/// `owf-core`'s config loader.
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

/// Forwards every broadcast from the in-process `owf_core::server::Daemon`
/// to the frontend as a Tauri event -- the overlay's `src/Overlay.tsx`
/// already listens for `"overlay-event"`; it used to arrive there via
/// `connection.rs`'s socket client, and now arrives the same way but
/// without a socket in between.
struct TauriSink(tauri::AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: &OverlayEvent) {
        if let Err(e) = self.0.emit("overlay-event", event) {
            eprintln!("overlay: failed to emit event to the frontend: {e}");
        }
    }

    /// `Request::ShowSettings` (spec §8): shows and focuses the settings
    /// window, which `setup()` below creates hidden. Best-effort -- the
    /// window is always declared in `tauri.conf.json`, so `get_webview_window`
    /// returning `None` here would mean that declaration was removed, not a
    /// transient failure worth surfacing to the caller of `Request::ShowSettings`.
    fn show_settings(&self) {
        if let Some(w) = self.0.get_webview_window("settings") {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
}

/// Called once by the frontend, immediately after it starts listening for
/// `"overlay-event"` (`src/Overlay.tsx`) -- replays whatever state the
/// daemon is in *right now* as a fresh `"overlay-event"` emission, via
/// `Daemon::connect_snapshot`. That is the same snapshot
/// `owf_core::server::serve_subscriber` sends a freshly connected socket
/// subscriber; the overlay stopped being a socket client, but still needs
/// the same "what's happening right now" answer on attach.
///
/// Without this, nothing shows during the several-second warm-up (nothing
/// ever *broadcasts* `Warming` -- `owf_core::server::start` only stores it),
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
            settings_cmds::list_input_devices
        ])
        .setup(move |app| {
            let window = app
                .get_webview_window(OVERLAY_LABEL)
                .expect("the overlay window must be declared in tauri.conf.json");
            relax_webkitgtk_minimum_size(&window);

            // Spec §8: closing the settings window hides it; it does not
            // exit the app -- only Beenden/`--quit`/a real app exit does
            // that (see `run`'s `RunEvent::Exit` handler below). Without
            // this, Tauri's default `CloseRequested` behaviour destroys the
            // window outright, and the settings command handlers would then
            // find no "settings" window left to `show`/`set_focus` on the
            // next `Request::ShowSettings`.
            if let Some(settings) = app.get_webview_window("settings") {
                let w = settings.clone();
                settings.on_window_event(move |e| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = e {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            // Never on the Tauri event-loop thread: a not-yet-warm daemon,
            // or a slow/looping replay file, must not delay first paint or
            // freeze window management.
            match replay_path.clone() {
                Some(path) => {
                    // No `Daemon` exists in replay mode at all. Managed
                    // unconditionally (see both arms) so a settings command's
                    // `State<Server>` extraction never fails here either --
                    // it reports a German error instead.
                    app.manage(settings_cmds::Server(None));
                    let handle = app.handle().clone();
                    let emit = move |event: OverlayEvent| {
                        if let Err(e) = handle.emit("overlay-event", &event) {
                            eprintln!("overlay: failed to emit event to the frontend: {e}");
                        }
                    };
                    eprintln!("overlay: replaying {} (not connecting to the daemon)", path.display());
                    std::thread::spawn(move || replay::run(&path, emit));
                }
                None => {
                    let (daemon, listener) =
                        owf_core::server::start(Arc::new(TauriSink(app.handle().clone())))?;
                    app.manage(daemon.clone());
                    app.manage(settings_cmds::Server(Some(daemon.clone())));
                    // Never on the Tauri event-loop thread -- the accept
                    // loop blocks, and a blocked event loop is a frozen
                    // window and an unclickable tray.
                    std::thread::spawn(move || owf_core::server::serve(daemon, listener));
                }
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Task 10 / spec §8: every exit route must converge on
    // `owf_core::server::shutdown`, not just the signal handler inside
    // `owf_core::server::start`. Two different mechanisms now guarantee
    // that, and this hook is only one of them:
    //
    // - `Request::Quit` (Beenden, `--quit`, and eventually the tray) already
    //   calls `shutdown` directly, on its own background thread, before
    //   its `std::process::exit(0)` -- and `exit` tears the whole process
    //   down immediately, on every thread, without ever giving Tauri's
    //   event loop a chance to run this closure. So `RunEvent::Exit` never
    //   fires on that path, and it doesn't need to: `shutdown` already ran.
    // - Every *other* way this app's event loop can end -- most notably a
    //   compositor-issued close of the overlay window falling through
    //   Tauri's default `ExitRequested` -> `Exit` (the settings window's
    //   own close is intercepted above and never reaches this at all) --
    //   has no `Request` to dispatch through, so this closure is the only
    //   place left for it to reach `shutdown`. `RunEvent::Exit` fires
    //   exactly once, right before the process actually exits, covering
    //   all of those at once.
    //
    // Either way `shutdown` is idempotent (`SHUTTING_DOWN`), so there is no
    // harm if some future path ends up calling it from both. `try_state`,
    // not `state`: in `--replay` mode no `Arc<Daemon>` is ever `app.manage`d,
    // and this callback must not panic in that mode.
    app.run(|app_handle, event| {
        if let tauri::RunEvent::Exit = event {
            if let Some(daemon) = app_handle.try_state::<Arc<Daemon>>() {
                shutdown(&daemon);
            }
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
