//! The class of the focused window, however this desktop can be asked.
//!
//! Split out of `hypr.rs` on 2026-09-07. That module had grown two unrelated
//! halves: Hyprland's *configuration* text, which only a Hyprland user ever
//! sees, and the focused-window lookup, which two features read on every
//! utterance -- `inject`'s paste chord and `style`'s per-window rules. The
//! second half lived there only because `hyprctl` happened to be the one
//! tool that could answer it, never because the question was Hyprland's.
//! GNOME can answer it too now, via [`crate::gnome`], so the question is
//! nobody's compositor in particular and lives here instead.
//!
//! Providers are tried in order; the first real answer wins:
//!
//! 1. [`crate::hypr::window_class`] -- `hyprctl -j activewindow`. Off
//!    Hyprland the binary does not exist and the spawn fails immediately
//!    (`ENOENT`), which costs nothing worth measuring.
//! 2. [`crate::gnome::window_class`] -- the accessibility bus, read from a
//!    value a background thread keeps polled rather than queried on the
//!    spot. See that module for why it polls: the events that would make a
//!    query unnecessary never arrive on GNOME.
//!
//! `None` from all of them keeps the historical meaning exactly: an unknown
//! window, which `inject::wants_shift` reads as "not a terminal" and
//! `style::resolve` reads as "use the default axes".

/// The focused window's class, or `None` when no provider can say.
///
/// Called from `start_recording` (`server.rs`) when a recording begins, off
/// the latency-critical path.
pub fn active_window_class() -> Option<String> {
    if let Some(class) = crate::hypr::window_class() {
        return Some(class);
    }
    crate::gnome::window_class()
}

/// Start any provider that has to watch the desktop over time rather than
/// answer on demand.
///
/// Called once from `server::start`. It must run at daemon startup and not
/// lazily at the first lookup: by the time a lookup happens the overlay is
/// already up and holds focus itself (invariant 2), so the honest answer is
/// already gone -- which is why [`crate::gnome`] keeps a polled value.
pub fn start_tracking() {
    crate::gnome::start_tracking();
}
