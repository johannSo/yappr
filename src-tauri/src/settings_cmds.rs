//! The settings window's commands: the original three config/device calls,
//! plus (task 16) the pair backing the "Beim Anmelden starten" toggle.
//!
//! The first three were socket calls in `settings-tauri`; they are direct
//! calls now. The command names and the JSON they return are byte-identical
//! on purpose: `src/settings/` and `Settings.tsx` are unchanged by this move,
//! and CLAUDE.md invariant 9's autosave contract -- validate before writing,
//! atomic rename, a rejected save keeps the value on screen -- is
//! `config_write`'s, not the transport's.
//!
//! ## Why autostart is not a `config.toml` key
//!
//! Every other setting in this window is a key the daemon reads out of
//! `Config` and validates with `#[serde(deny_unknown_fields)]`. Autostart
//! doesn't fit that shape: the thing being toggled is *whether a file
//! exists* (`owf_core::paths::autostart_desktop_file()`), and that file can
//! be deleted or edited by something entirely outside this app -- the user
//! clearing `~/.config/autostart/` by hand, a distro migration, another
//! autostart manager. A `config.toml` key mirroring that ("autostart.enabled
//! = true") would be a second, independent copy of the same fact, free to
//! disagree with the filesystem the moment either one changes without the
//! other -- and there would be no event that tells this app to re-sync them.
//!
//! So there is no key. [`autostart_status`] answers by checking the file's
//! existence directly, every time it's asked, and [`set_autostart`] is the
//! only thing that ever writes or removes it. The filesystem is not
//! mirrored into config; it *is* the state, which is the only way for a
//! toggle here to be incapable of lying about it.
//!
//! Two things a socket call didn't have to worry about, that a Tauri command
//! does:
//!
//! - **Threading.** Verified against the vendored `tauri-2.11.5` and
//!   `tauri-macros-2.6.3` sources (`tauri::State`'s `CommandArg` impl in
//!   `state.rs`; the command-wrapper codegen in
//!   `tauri-macros/src/command/wrapper.rs`): a plain, non-`async`
//!   `#[tauri::command] fn` is expanded into `body_blocking`, which calls the
//!   function inline -- `let result = $path(...)` -- on whatever thread
//!   delivered the IPC message, with no dispatch to any other thread. On
//!   Linux that is WebKitGTK's main loop, the same thread every window's UI
//!   runs on and the thread `Webview::on_message` is invoked from. An
//!   `async fn` command instead goes through `body_async`, which calls
//!   `resolver.respond_async_serialized(async move { ... })`; that hands the
//!   whole call to `crate::async_runtime::spawn`, i.e. onto Tauri's own Tokio
//!   runtime (built with `rt-multi-thread`, per `tauri`'s own `Cargo.toml`) --
//!   genuinely different threads from the event loop, not just a different
//!   `async` block on the same one. `Request::ListInputDevices` alone can
//!   take `DEVICE_LIST_TIMEOUT` = 5 s enumerating ALSA/PipeWire devices
//!   (`owf_core::server`), so these three are `async fn`. `dispatch` itself
//!   is still a *blocking* call (mutex locks, a blocking device-enumeration
//!   thread join, blocking file I/O for `SetConfig`/`GetConfig`), so `call`
//!   below further hands it to `tauri::async_runtime::spawn_blocking` rather
//!   than running it inline on an async worker -- a slow enumeration then
//!   ties up a dedicated blocking thread, not a worker other invokes or
//!   timers might need.
//! - **Replay mode.** `src-tauri/src/lib.rs`'s `--replay` branch never starts
//!   a `Daemon` at all (`replay.rs` drives the overlay from a fixture file
//!   with no socket and no pipeline), so a command declared as
//!   `tauri::State<'_, Arc<Daemon>>` would fail its `State` extraction
//!   whenever replay is active -- unreachable today (no tray, no way to open
//!   this window in replay), but reachable the moment a tray's left click can
//!   open it regardless of mode. Both branches of `setup()` now `app.manage`
//!   a [`Server`] unconditionally, so a settings command's `State` extraction
//!   can never fail; a `None` inside it means "no daemon in this process"
//!   and turns into a stated German error instead.

use std::sync::Arc;

use owf_core::proto::{Request, Response};
use owf_core::server::{dispatch, Daemon};

/// What `setup()` in `lib.rs` manages in both the normal and `--replay`
/// branches, so a settings command's `tauri::State` extraction always
/// succeeds -- only the body's own German error varies with whether a
/// daemon actually exists in this process.
pub struct Server(pub Option<Arc<Daemon>>);

const NO_DAEMON: &str =
    "Kein Daemon in diesem Prozess (Replay-Modus) -- Einstellungen sind nicht verfügbar.";

impl Server {
    /// The daemon to dispatch against, or [`NO_DAEMON`] -- pulled out as its
    /// own method (rather than inlined in `call`) so it is testable without
    /// any `tauri::State`/IPC machinery: `Server` itself is a plain struct.
    fn daemon(&self) -> Result<Arc<Daemon>, String> {
        self.0.clone().ok_or_else(|| NO_DAEMON.to_string())
    }
}

#[tauri::command]
pub async fn get_config(server: tauri::State<'_, Server>) -> Result<serde_json::Value, String> {
    call(&server, Request::GetConfig).await
}

#[tauri::command]
pub async fn set_config(
    server: tauri::State<'_, Server>,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    call(&server, Request::SetConfig { config }).await
}

#[tauri::command]
pub async fn list_input_devices(
    server: tauri::State<'_, Server>,
) -> Result<serde_json::Value, String> {
    call(&server, Request::ListInputDevices).await
}

/// The `.desktop` entry `set_autostart_at` writes when enabling autostart --
/// mirrors the minimal working shape already on this machine at
/// `~/.config/autostart/Handy.desktop` (no `Hidden`, no `OnlyShowIn`/
/// `NotShowIn`, which is what lets `xdg-autostart-generator` pick it up
/// unconditionally). `Exec=openwhisprflow` names the binary bare, matching
/// how `crates/owf-core/src/hypr.rs` invokes it in the Hyprland config it
/// emits (`exec-once = openwhisprflow`) -- both rely on a `PATH` install
/// rather than an absolute path baked in.
const AUTOSTART_DESKTOP_ENTRY: &str = "\
[Desktop Entry]
Type=Application
Version=1.0
Name=OpenWhisprFlow
Comment=Startet das Diktat-Overlay im Hintergrund
Exec=openwhisprflow
StartupNotify=false
Terminal=false
";

/// Writes or removes the autostart `.desktop` entry at `path` (spec §9;
/// off by default, so this is only ever reached by an explicit toggle).
///
/// Idempotent in both directions: `enabled: true` is a plain overwrite of a
/// fixed single path, so writing it twice leaves exactly one file, not two;
/// `enabled: false` treats a second removal's `NotFound` as success rather
/// than an error, so disabling twice -- or disabling a toggle that was
/// already off -- never fails.
///
/// Takes `path` as a parameter rather than resolving
/// `owf_core::paths::autostart_desktop_file()` itself, purely so this is
/// testable against a scratch directory: see this module's tests, none of
/// which ever construct the real path. Only [`set_autostart`] below does.
fn set_autostart_at(path: &std::path::Path, enabled: bool) -> std::io::Result<()> {
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, AUTOSTART_DESKTOP_ENTRY)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Whether OpenWhisprFlow currently starts itself at login -- read straight
/// off the filesystem (see the module doc's "why not a config key"), so a
/// file removed behind this app's back is reported truthfully instead of
/// from stale state. A single `Path::exists()` stat; no `spawn_blocking`
/// needed for that, unlike [`call`]'s `dispatch` or `list_input_devices`.
#[tauri::command]
pub fn autostart_status() -> serde_json::Value {
    let enabled = owf_core::paths::autostart_desktop_file().exists();
    serde_json::json!({ "enabled": enabled })
}

/// Turns "Beim Anmelden starten" on or off by writing or removing the real
/// autostart entry. Same reasoning as [`autostart_status`] on why this is a
/// plain synchronous command: a single small write or remove, not the kind
/// of work `settings_cmds.rs`'s module doc reserves `spawn_blocking` for.
#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    set_autostart_at(&owf_core::paths::autostart_desktop_file(), enabled).map_err(|e| {
        format!("Autostart-Eintrag konnte nicht geschrieben werden: {e}")
    })
}

/// Every settings command funnels through here: pulls the `Arc<Daemon>` out
/// of `Server` (or reports [`NO_DAEMON`]), then runs the blocking `dispatch`
/// call on a dedicated blocking thread -- never inline on the async worker
/// this `async fn` itself is polled on. See the module doc for why both of
/// those matter.
async fn call(
    server: &tauri::State<'_, Server>,
    req: Request,
) -> Result<serde_json::Value, String> {
    let daemon = server.daemon()?;
    let resp = tauri::async_runtime::spawn_blocking(move || dispatch(&daemon, req))
        .await
        .map_err(|e| format!("interner Fehler: {e}"))?;
    to_json(resp)
}

/// A `{"ok": false}` becomes an `Err`, exactly as `settings-tauri`'s socket
/// client did, so the frontend's existing rejection handling keeps working
/// unchanged.
fn to_json(resp: Response) -> Result<serde_json::Value, String> {
    let v = serde_json::to_value(&resp).map_err(|e| e.to_string())?;
    if resp.ok {
        Ok(v)
    } else {
        Err(resp.err.unwrap_or_else(|| "Unbekannter Fehler".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owf_core::proto::State as WireState;

    /// A fresh, collision-free scratch directory for a single test. Not a
    /// dependency: `tempfile` isn't in `[dev-dependencies]` here either (see
    /// the identical helper in `owf-core`'s `inject.rs` and `owf-cli`'s --
    /// now this crate's -- `setup.rs` tests). Load-bearing for this file in
    /// particular: this is what keeps every autostart test off the real
    /// `~/.config/autostart/`, which on this machine holds five files this
    /// project must never touch, one of them another dictation app's own
    /// autostart entry.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("owf-settings-cmds-test-{tag}-{}-{n}", std::process::id()))
    }

    /// Writing the file is what enables autostart; removing it is what
    /// disables it. Both directions must be idempotent -- a user who toggles
    /// twice must not end up with two entries or a stale one. Runs entirely
    /// inside a scratch directory (never the real autostart directory) --
    /// see [`set_autostart_at`]'s doc comment for why the path is a
    /// parameter rather than resolved internally.
    #[test]
    fn the_autostart_desktop_file_is_written_and_removed_idempotently() {
        let dir = scratch_dir("autostart");
        let p = dir.join("openwhisprflow.desktop");

        set_autostart_at(&p, true).unwrap();
        set_autostart_at(&p, true).unwrap();
        assert!(p.exists());
        assert!(std::fs::read_to_string(&p).unwrap().contains("Exec=openwhisprflow"));

        set_autostart_at(&p, false).unwrap();
        set_autostart_at(&p, false).unwrap();
        assert!(!p.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Disabling something that was never enabled -- the directory itself
    /// doesn't even exist yet -- must be a no-op, not an error: a fresh
    /// install's very first `set_config`-equivalent call for this toggle is
    /// exactly this shape (default off, nothing on disk).
    #[test]
    fn disabling_autostart_when_nothing_was_ever_enabled_is_not_an_error() {
        let dir = scratch_dir("autostart-never-enabled");
        let p = dir.join("openwhisprflow.desktop");
        assert!(!dir.exists());

        set_autostart_at(&p, false).unwrap();
        assert!(!p.exists());
    }

    /// Pins the fields `xdg-autostart-generator` cares about, checked
    /// against `~/.config/autostart/Handy.desktop`'s known-working shape on
    /// this machine: a bare `Type=Application`/`Exec=`, and critically
    /// *no* `Hidden=true` or `OnlyShowIn`/`NotShowIn` -- any of those would
    /// make the generator skip the file rather than turn it into a systemd
    /// user unit.
    #[test]
    fn the_written_entry_has_the_shape_xdg_autostart_generator_requires() {
        let dir = scratch_dir("autostart-shape");
        let p = dir.join("openwhisprflow.desktop");

        set_autostart_at(&p, true).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.starts_with("[Desktop Entry]"));
        assert!(content.contains("Type=Application"));
        assert!(content.contains("Exec=openwhisprflow"));
        assert!(!content.contains("Hidden"));
        assert!(!content.contains("OnlyShowIn"));
        assert!(!content.contains("NotShowIn"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact scenario Task 9 exists to close off: `--replay` mode
    /// manages `Server(None)` (see `lib.rs`'s `setup()`), and a settings
    /// command must turn that into a stated German error rather than
    /// panicking or failing Tauri's own generic "state not managed" way --
    /// which is what a bare `tauri::State<'_, Arc<Daemon>>` parameter would
    /// have done the moment a tray's left click could reach this window in
    /// replay mode.
    #[test]
    fn a_missing_daemon_reports_a_stated_german_error_rather_than_panicking() {
        let server = Server(None);
        assert_eq!(server.daemon().err(), Some(NO_DAEMON.to_string()));
    }

    /// The JSON-shape half of the contract: the frontend's existing
    /// rejection handling depends on a successful `Response` staying an
    /// `Ok`, with every field (not just `ok`) carried through untouched --
    /// including `config_path` staying snake_case, which is the literal key
    /// `src/Settings.tsx` reads (`res.config_path`), since `Response` has no
    /// `rename_all` attribute.
    #[test]
    fn an_ok_response_becomes_ok_json_with_its_fields_intact() {
        let mut resp = Response::ok(WireState::Idle);
        resp.config = Some(serde_json::json!({"asr": {"num_threads": 4}}));
        resp.config_path = Some("/tmp/config.toml".to_string());
        let value = to_json(resp).expect("an ok response must not become an Err");
        assert_eq!(value["ok"], true);
        assert_eq!(value["config"]["asr"]["num_threads"], 4);
        assert_eq!(value["config_path"], "/tmp/config.toml");
    }

    /// Mirrors `settings-tauri`'s old socket client exactly: `{"ok": false}`
    /// becomes an `Err` carrying the daemon's own reason, which is what the
    /// frontend's rejection handling (`src/Settings.tsx`) is written against.
    #[test]
    fn a_rejected_response_becomes_an_err_carrying_the_daemons_reason() {
        let resp = Response::err("changing settings requires idle");
        assert_eq!(
            to_json(resp).unwrap_err(),
            "changing settings requires idle"
        );
    }

    /// `Response::err` always sets a reason in production, but `to_json`'s
    /// fallback exists for the case it doesn't -- pinned directly since nothing
    /// else exercises that branch.
    #[test]
    fn a_rejection_with_no_reason_falls_back_to_a_generic_german_message() {
        let mut resp = Response::err("placeholder");
        resp.err = None;
        assert_eq!(to_json(resp).unwrap_err(), "Unbekannter Fehler");
    }
}
