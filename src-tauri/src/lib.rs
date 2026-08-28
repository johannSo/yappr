//! The OpenWhisprFlow overlay: a small always-on-top window that renders
//! the dictation pipeline's state (spec 12). This crate holds no pipeline
//! logic -- it is a thin client of the daemon's `Subscribe` event stream
//! (`connection.rs`), or of a checked-in NDJSON fixture when driven with
//! `--replay <path>` (`replay.rs`), and forwards whatever it receives to
//! the frontend as a Tauri event. All rendering decisions live in `src/`.

pub mod cli;
mod connection;
mod replay;

use tauri::{Emitter, Manager, PhysicalPosition};

use owf_core::proto::OverlayEvent;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let replay_path = replay_arg();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![position_overlay])
        .setup(move |app| {
            let window = app
                .get_webview_window(OVERLAY_LABEL)
                .expect("the overlay window must be declared in tauri.conf.json");
            relax_webkitgtk_minimum_size(&window);

            let handle = app.handle().clone();
            let emit = move |event: OverlayEvent| {
                if let Err(e) = handle.emit("overlay-event", &event) {
                    eprintln!("overlay: failed to emit event to the frontend: {e}");
                }
            };

            // Never on the Tauri event-loop thread: a not-yet-warm daemon,
            // or a slow/looping replay file, must not delay first paint or
            // freeze window management.
            match replay_path.clone() {
                Some(path) => {
                    eprintln!("overlay: replaying {} (not connecting to the daemon)", path.display());
                    std::thread::spawn(move || replay::run(&path, emit));
                }
                None => {
                    std::thread::spawn(move || connection::run(emit));
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
