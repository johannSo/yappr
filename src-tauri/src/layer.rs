//! Places the overlay without a compositor rule -- spec §6.
//!
//! CLAUDE.md invariant 5 records that client-side positioning is a no-op
//! under Hyprland: true for an `xdg_shell` toplevel, and not true for a
//! **layer-shell** surface, which anchors itself. `KeyboardMode::None` is
//! the same surface's answer to invariant 2, so the overlay refuses focus
//! at the protocol level instead of by a rule the user has to paste.
//!
//! `wlr-layer-shell` is a wlroots protocol; Mutter does not implement it,
//! and the spec deliberately keeps GNOME from being actively precluded by
//! that gap. `anchor_overlay` returns `Err` in that case rather than
//! half-initialising the window; the caller in `lib.rs` logs it and leaves
//! the window an ordinary toplevel. Invariant 2 still holds on that
//! fallback path -- see the call site's comment for how -- and
//! `position_overlay`'s existing no-op in `lib.rs` is that path's answer to
//! invariant 5.
//!
//! `gtk_layer_shell::is_supported()` asks the *current* Wayland display for
//! the `zwlr_layer_shell_v1` global, so this is a live runtime fact checked
//! fresh on every call, not a cached probe from elsewhere or a config
//! switch choosing between two modes -- there is exactly one code path
//! here, and it tells the truth about whether it worked.
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
pub fn anchor_overlay(window: &tauri::WebviewWindow) -> anyhow::Result<()> {
    use gtk_layer_shell::{is_supported, Edge, KeyboardMode, Layer, LayerShell};

    if !is_supported() {
        anyhow::bail!("the compositor does not implement wlr-layer-shell");
    }

    let gtk_win = window.gtk_window()?;
    // Must happen before the window is realized -- gtk-layer-shell's own
    // requirement. `setup()` calls this before any other window
    // manipulation, ahead of the first `show()`.
    gtk_win.init_layer_shell();
    gtk_win.set_layer(Layer::Overlay);
    // Invariant 2, at the protocol level: this surface can never be given
    // keyboard focus, so `wtype` cannot type the dictation into it.
    gtk_win.set_keyboard_mode(KeyboardMode::None);
    gtk_win.set_anchor(Edge::Bottom, true);
    gtk_win.set_layer_shell_margin(Edge::Bottom, crate::bottom_margin_logical() as i32);
    Ok(())
}

/// `wlr-layer-shell` is a Wayland protocol with a GTK3/Linux-family binding
/// only; every other target falls back to the ordinary-toplevel path
/// unconditionally, the same way `relax_webkitgtk_minimum_size` does for
/// its own GTK-specific fix.
#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
pub fn anchor_overlay(_window: &tauri::WebviewWindow) -> anyhow::Result<()> {
    anyhow::bail!("layer-shell is only implemented for Linux/BSD Wayland targets")
}
