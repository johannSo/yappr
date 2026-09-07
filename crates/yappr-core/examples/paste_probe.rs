//! Exercises the real injection path for one line of text, printing every
//! decision on the way -- without a microphone, an ASR model, or a running
//! daemon.
//!
//! This exists because the ydotool backend's Ctrl+V/Ctrl+Shift+V choice
//! depends on `hypr::active_window_class()`, which is unavailable on
//! GNOME/Mutter and on any Hyprland session whose environment lacks
//! `HYPRLAND_INSTANCE_SIGNATURE` (see that function, and `config::PasteChord`).
//! The failure is silent -- `ydotool` exits 0 either way -- so the only way to
//! tell which chord was pressed used to be to read `/dev/input/event*`.
//!
//! **It presses real keys into whatever window is focused**, and leaves the
//! text in the clipboard. Focus a scratch window first.
//!
//! ```bash
//! cargo run -p yappr-core --example paste_probe -- "hello"               # config's chord
//! cargo run -p yappr-core --example paste_probe -- "hello" ctrl_shift_v  # forced
//! ```
use yappr_core::config::{Config, InjectBackend, PasteChord};
use yappr_core::{hypr, inject};

fn main() {
    let mut cfg = Config::load().unwrap_or_default();
    // The wtype and clipboard backends have no chord to probe.
    cfg.inject.backend = InjectBackend::Ydotool;
    if let Some(c) = std::env::args().nth(2) {
        cfg.inject.paste_chord = match c.as_str() {
            "ctrl_v" => PasteChord::CtrlV,
            "ctrl_shift_v" => PasteChord::CtrlShiftV,
            "auto" => PasteChord::Auto,
            other => {
                eprintln!("unknown chord {other:?}; expected auto, ctrl_v or ctrl_shift_v");
                std::process::exit(2);
            }
        };
    }
    let class = hypr::active_window_class();
    println!("active_window_class() = {class:?}");
    println!("paste_chord           = {:?}", cfg.inject.paste_chord);
    let shift = match cfg.inject.paste_chord {
        PasteChord::CtrlV => false,
        PasteChord::CtrlShiftV => true,
        PasteChord::Auto => class
            .as_deref()
            .map(|c| cfg.inject.terminal_classes.iter().any(|t| t.eq_ignore_ascii_case(c)))
            .unwrap_or(false),
    };
    println!("chord sent            = {}", if shift { "Ctrl+Shift+V" } else { "Ctrl+V" });
    if class.is_none() && cfg.inject.paste_chord == PasteChord::Auto {
        println!("NOTE: no window class -- a terminal will ignore this chord.");
    }
    let text = std::env::args().nth(1).unwrap_or_else(|| "paste_probe".to_string());
    match inject::build(&cfg.inject).inject(&text, class.as_deref()) {
        Ok(()) => println!("inject()              = Ok"),
        Err(e) => println!("inject()              = Err: {e}"),
    }
}
