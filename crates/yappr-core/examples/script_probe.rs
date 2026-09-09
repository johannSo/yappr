//! Runs the configured paste script for one line of text, printing what it
//! was given and what it did -- without a microphone, an ASR model, or a
//! running daemon.
//!
//! This replaced `paste_probe` on 2026-09-09, and answers a narrower
//! question. The old probe existed because yappr chose the paste chord
//! itself and got it wrong silently; it printed the window class and the
//! chord. The script chooses now, so what is worth showing is the contract:
//! which program ran, with which argument, and what it exited with. The
//! window class is printed too, but only as a courtesy -- nothing on this
//! path reads it.
//!
//! **It runs your real paste script, which presses real keys into whatever
//! window is focused** and will very likely replace your clipboard. Focus a
//! scratch window first.
//!
//! ```bash
//! cargo run -p yappr-core --example script_probe -- "hallo"
//! cargo run -p yappr-core --example script_probe -- "hallo" ~/bin/paste.sh  # override the path
//! ```
use yappr_core::config::{Config, InjectBackend};
use yappr_core::{inject, winclass};

/// Poll until the focus tracker has an answer, or give up.
fn wait_for_class() -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(class) = winclass::active_window_class() {
            return Some(class);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn main() {
    let mut cfg = Config::load().unwrap_or_default();
    cfg.inject.backend = InjectBackend::Script;
    if let Some(path) = std::env::args().nth(2) {
        cfg.inject.script = path;
    }
    if cfg.inject.script.is_empty() {
        eprintln!(
            "no script configured: set [inject] script in config.toml, \
             or pass a path as the second argument"
        );
        std::process::exit(2);
    }
    // The GNOME provider answers from a thread that tracks focus, so a
    // short-lived process has to let it connect and sweep first. The daemon
    // has been tracking since startup and never waits like this.
    winclass::start_tracking();
    let class = wait_for_class();
    let text = std::env::args().nth(1).unwrap_or_else(|| "script_probe".to_string());
    println!("active_window_class() = {class:?}   (informational -- the script is not told)");
    println!("script                = {}", cfg.inject.script);
    println!("argv[1]               = {text:?}");
    match inject::build(&cfg.inject).inject(&text, class.as_deref()) {
        Ok(()) => println!("inject()              = Ok"),
        Err(e) => println!("inject()              = Err: {e}"),
    }
}
