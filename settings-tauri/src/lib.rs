//! The OpenWhisprFlow settings window.
//!
//! A second Tauri app rather than a second window of the overlay app, because
//! every window of a Tauri app shares one class, and the overlay's class is
//! matched by a Hyprland `no_focus` rule (see `owf-core/src/hypr.rs`). A
//! settings form inheriting `no_focus` could not accept a single keystroke.
//! Keeping this a separate app with its own class means the user's compositor
//! config needs no change at all.
//!
//! Like the overlay, this crate does **not** depend on `owf-core`: it would
//! inherit `sherpa-onnx`, `cpal` and `rubato` for the sake of rendering a
//! form. It is a socket client, and the config it edits crosses that socket as
//! plain JSON, so there is no copy of the config schema here either.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Bounded so a wedged daemon leaves the window usable rather than frozen.
/// Comfortably above the daemon's own 5 s device-enumeration timeout, which is
/// the slowest thing any of these calls can wait on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Mirrors `owf_core::paths::runtime_socket()` without depending on
/// `owf-core` — the same trade the overlay's `connection.rs` makes.
fn runtime_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("openwhisprflow.sock")
}

/// One connection, one request line, one response line — the exchange
/// documented in `owf-core/src/proto.rs`.
///
/// A `{"ok": false}` response is turned into an `Err`, so the frontend can
/// treat daemon-side rejections (an invalid config, a busy daemon) exactly
/// like transport failures: both are something to show the user, neither is a
/// reason to leave the form in a half-saved state.
fn request(req: &serde_json::Value) -> Result<serde_json::Value, String> {
    let sock = runtime_socket();
    let stream = UnixStream::connect(&sock).map_err(|e| {
        format!(
            "Der Daemon ist nicht erreichbar ({}): {e}\nLäuft `owf-ctl daemon`?",
            sock.display()
        )
    })?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).map_err(|e| e.to_string())?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(writer, "{req}").map_err(|e| format!("Senden fehlgeschlagen: {e}"))?;
    writer.flush().map_err(|e| format!("Senden fehlgeschlagen: {e}"))?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("Keine Antwort vom Daemon: {e}"))?;
    if line.trim().is_empty() {
        return Err("Der Daemon hat die Verbindung ohne Antwort geschlossen.".into());
    }

    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("Unlesbare Antwort: {e}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let err = value
            .get("err")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unbekannter Fehler");
        return Err(err.to_string());
    }
    Ok(value)
}

#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    request(&serde_json::json!({ "cmd": "get-config" }))
}

#[tauri::command]
fn set_config(config: serde_json::Value) -> Result<serde_json::Value, String> {
    request(&serde_json::json!({ "cmd": "set-config", "config": config }))
}

#[tauri::command]
fn list_input_devices() -> Result<serde_json::Value, String> {
    request(&serde_json::json!({ "cmd": "list-input-devices" }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_config, set_config, list_input_devices])
        .run(tauri::generate_context!())
        .expect("error while running the settings window");
}
