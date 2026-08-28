// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Argument dispatch runs here, above everything else: before `tauri::Builder`,
// before any GTK/WebKit initialisation, before any model is loaded. A
// shortcut press runs this binary, and everything it initialises before
// answering is latency the user pays -- see `client.rs`'s module doc.
fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let route = openwhisprflow_lib::cli::route(&args);
    match openwhisprflow_lib::client::dispatch(route) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!("openwhisprflow: {e:#}");
            openwhisprflow_lib::client::notify_failure(&e);
            std::process::exit(1);
        }
    }
    openwhisprflow_lib::run();
}
