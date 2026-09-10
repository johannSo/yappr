// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Argument dispatch runs here, above everything else: before `tauri::Builder`,
// before any GTK/WebKit initialisation, before any model is loaded. A
// shortcut press runs this binary, and everything it initialises before
// answering is latency the user pays -- see `client.rs`'s module doc.
fn main() {
    disable_webkit_sandbox();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let route = yappr_lib::cli::route(&args);
    match yappr_lib::client::dispatch(route) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!("yappr: {e:#}");
            yappr_lib::client::notify_failure(&e);
            std::process::exit(1);
        }
    }
    yappr_lib::run();
}

/// Turns WebKitGTK's bubblewrap sandbox off before the webview exists.
///
/// This is the `--no-sandbox` of a Tauri app: WebKitGTK stopped honouring
/// `WEBKIT_FORCE_SANDBOX=0` and says so itself ("no longer allows disabling
/// the sandbox. Use WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1 instead"), so
/// this variable is the only remaining switch.
///
/// Justified here in a way it would not be in a browser: both windows load
/// *bundled local assets only* -- `frontendDist`, embedded in the binary by
/// the `custom-protocol` feature. There is no untrusted content for the
/// sandbox to contain, and the thing it reliably does contain is the app
/// itself, which inside an AppImage mount is where it breaks: the sandbox
/// re-executes through `bwrap` and the bundle's paths do not survive.
///
/// Set only when the user has not: `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=0
/// yappr` puts it back. Must run before any GTK/WebKit initialisation, which
/// is why it is the first line of `main` rather than part of `setup()`.
fn disable_webkit_sandbox() {
    if std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_none() {
        // SAFETY: single-threaded here -- this is the first statement of
        // `main`, before `cli::route`, Tauri, GTK or any thread we spawn.
        unsafe { std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1") };
    }
}
