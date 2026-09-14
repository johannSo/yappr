//! Presses the `libei` backend's paste chord for one line of text, printing
//! which way into the compositor it found and whether it had to ask you --
//! without a microphone, an ASR model, or a running daemon.
//!
//! It exists for the reason `script_probe` does: the only two things that
//! can go wrong on this path are invisible from the outside. A portal
//! session that could not be established, and a chord that was delivered but
//! ignored, both end as a dictation that produced no text. So this prints
//! the session outcome (including the portal's own words when it refused),
//! whether a stored restore token was offered and whether the dialog
//! appeared anyway, the window class the chord was chosen from, and which
//! chord that turned out to be.
//!
//! **It presses real keys into whatever window is focused, and it puts the
//! transcript on your clipboard.** Focus a scratch window first. Whatever
//! you had copied is handed back afterwards, exactly as a dictation does it,
//! unless you have `[inject] restore_clipboard = false`. The very first run also
//! raises the portal's approval dialog, which takes focus itself -- answer
//! it, then focus the scratch window again before the chord goes out.
//!
//! ```bash
//! cargo run -p yappr-core --example libei_probe -- "hallo welt"
//! ```
use std::time::Duration;

use yappr_core::config::{Config, InjectBackend};
use yappr_core::{inject, libei, winclass};

/// How long to wait for the portal session before giving up on it. Long
/// enough for a human to read a dialog and click Allow, which the pipeline
/// itself never waits for -- see `libei::CHORD_REPLY_TIMEOUT`.
const SESSION_WAIT: Duration = Duration::from_secs(120);

/// Poll until the focus tracker has an answer, or give up. Same as
/// `script_probe`'s: a short-lived process has to let the GNOME provider
/// connect and sweep, where the daemon has been tracking since startup.
fn wait_for_class() -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(class) = winclass::active_window_class() {
            return Some(class);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn main() {
    let mut cfg = Config::load().unwrap_or_default();
    cfg.inject.backend = InjectBackend::Libei;
    let text = std::env::args().nth(1).unwrap_or_else(|| "libei_probe".to_string());

    winclass::start_tracking();
    // Before the class sweep, so the two waits overlap -- and because the
    // approval dialog, if it comes, takes focus and would otherwise be the
    // window whose class we read.
    libei::start();

    println!("restore token file    = {}", libei::token_file().display());
    println!(
        "  present before this run: {}",
        if libei::token_file().exists() { "yes" } else { "no" }
    );

    match libei::wait_for_session(SESSION_WAIT) {
        Some(Ok(report)) => {
            println!("session               = Ok");
            println!("  path                = {}", report.path);
            println!("  restore token sent  = {}", report.token_offered);
            println!("  restore token saved = {}", report.token_issued);
            println!(
                "  Start took          = {:?}  ({})",
                report.start_elapsed,
                if report.asked_a_human() {
                    "slow enough that a dialog was almost certainly shown -- inferred, \
                     the portal does not say"
                } else {
                    "fast enough that no dialog was shown -- the stored token was honoured"
                }
            );
        }
        Some(Err(e)) => {
            println!("session               = Err: {e}");
            println!(
                "  (a locked screen answers `Session creation inhibited`; a desktop whose \
                 portal has no RemoteDesktop implementation answers nothing at all)"
            );
        }
        None => println!("session               = still being established after {SESSION_WAIT:?}"),
    }

    let class = wait_for_class();
    let shift = match cfg.inject.paste_chord {
        yappr_core::config::PasteChord::CtrlV => false,
        yappr_core::config::PasteChord::CtrlShiftV => true,
        yappr_core::config::PasteChord::Auto => class
            .as_deref()
            .is_some_and(|c| cfg.inject.terminal_classes.iter().any(|t| t.eq_ignore_ascii_case(c))),
    };
    println!("active_window_class() = {class:?}");
    println!("paste_chord           = {:?}", cfg.inject.paste_chord);
    println!("chord to be pressed   = {}", if shift { "Ctrl+Shift+V" } else { "Ctrl+V" });
    if class.is_none() && cfg.inject.paste_chord == yappr_core::config::PasteChord::Auto {
        println!(
            "  WARNING: no window class, so `auto` picks plain Ctrl+V -- which terminals \
             ignore, silently. Set [inject] paste_chord = \"ctrl_shift_v\" if that is where \
             you dictate."
        );
    }
    println!("text                  = {text:?}");
    println!(
        "restore_clipboard     = {}  ({})",
        cfg.inject.restore_clipboard,
        if cfg.inject.restore_clipboard {
            "what you had copied is put back after the paste"
        } else {
            "the transcript stays in the clipboard"
        }
    );

    match inject::build(&cfg.inject).inject(&text, class.as_deref()) {
        Ok(()) => println!("inject()              = Ok"),
        Err(e) => println!("inject()              = Err: {e}"),
    }
}
