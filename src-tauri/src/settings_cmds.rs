//! The settings window's commands: the original three config/device calls,
//! the pair backing the "Beim Anmelden starten" toggle (task 16), and the
//! version string its sidebar foot shows.
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
//! exists* (`yappr_core::paths::autostart_desktop_file()`), and that file can
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
//!   (`yappr_core::server`), so these three are `async fn`. `dispatch` itself
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

use tauri::Emitter as _;

use yappr_core::proto::{Request, Response};
use yappr_core::server::{dispatch, Daemon};

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
    app: tauri::AppHandle,
    server: tauri::State<'_, Server>,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let res = call(&server, Request::SetConfig { config }).await?;
    // Both windows repaint from one broadcast, and the overlay is the reason
    // it is a broadcast rather than a return value: it is a second webview
    // with no part in this call, and a themed settings window next to a HUD
    // still wearing the old palette is the one bug this feature can most
    // obviously ship with.
    //
    // Read back off disk rather than out of `config`, because `config` is
    // routinely partial -- `Request::SetConfig` carries the settings window's
    // whole snapshot, but `wizard_finish`'s patch is a single leaf, and
    // neither is required to mention `[ui]`. Fired on every accepted save,
    // theme or not: `applyTheme` is idempotent, and a change-detecting
    // version would need a before-image it has no reason to hold.
    let _ = app.emit("theme-changed", configured_theme());
    Ok(res)
}

/// The palette `[ui] theme` currently asks for, as the string the DOM and
/// `palettes.css` both spell it with.
///
/// Deliberately lenient where every other reader of this file is strict: an
/// unreadable config gives back the shipped pair rather than an error,
/// because the caller is a repaint and the alternative is an unpainted
/// window (see `resolve` in `src/theme.ts`). Nothing is hidden by that --
/// invariant 4's `load_or_quarantine` has already run in `server::start`, so
/// a config bad enough to fail here is one the settings window is already
/// showing a notice about.
fn configured_theme() -> String {
    yappr_core::config::Config::load_from(&yappr_core::paths::config_file())
        .map(|c| c.ui.theme)
        .unwrap_or(yappr_core::config::Theme::System)
        .to_string()
}

/// What the overlay asks on mount. It reads no other config, so this is a
/// command of its own rather than a second `get_config` -- which would hand
/// a click-through HUD the whole settings tree, and would fail outright in
/// `--replay` mode, where there is no daemon but there is still a capsule on
/// screen.
#[tauri::command]
pub fn theme() -> String {
    configured_theme()
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
/// unconditionally). `Exec=yappr` names the binary bare, matching
/// how `crates/yappr-core/src/hypr.rs` invokes it in the Hyprland config it
/// emits (`exec-once = yappr`) -- both rely on a `PATH` install
/// rather than an absolute path baked in.
const AUTOSTART_DESKTOP_ENTRY: &str = "\
[Desktop Entry]
Type=Application
Version=1.0
Name=yappr
Comment=Startet das Diktat-Overlay im Hintergrund
Exec=yappr
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
/// `yappr_core::paths::autostart_desktop_file()` itself, purely so this is
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

/// What the settings window's sidebar foot shows -- the only thing in that
/// window naming the running *binary* rather than one of its settings.
///
/// `CARGO_PKG_VERSION` is this crate's own version, baked in at compile time,
/// so it is true of the binary actually executing. `tauri.conf.json`'s
/// `version` would not be: that one describes the *bundle*, and the only
/// thing holding the two together is someone remembering to edit both --
/// which is what `the_binary_and_the_bundle_agree_on_the_version_number`
/// below exists to notice.
///
/// The `-debug` suffix is half the reason to show this at all. A
/// `cargo build` with no `--release` and the shipped AppImage report the same
/// three numbers and are not the same program (one of them runs the
/// `debug_assertions` paths, at a fraction of the ASR speed), and nothing
/// else on screen separates them -- least of all in the situation this line
/// gets read out loud, which is a bug report.
const APP_VERSION: &str = if cfg!(debug_assertions) {
    concat!(env!("CARGO_PKG_VERSION"), "-debug")
} else {
    env!("CARGO_PKG_VERSION")
};

/// The version string for the sidebar foot. Plain and synchronous for the
/// same reason [`autostart_status`] is: it returns a compile-time constant,
/// which is not work worth handing to a blocking thread.
#[tauri::command]
pub fn app_version() -> &'static str {
    APP_VERSION
}

/// Restarts the whole app, for the settings window's restart dialog.
///
/// Goes through [`call`] like every other command here, which means it
/// returns as soon as `dispatch` has *accepted* the restart -- not when the
/// app comes back. That is deliberate and is what the arm is built for:
/// `Request::Restart` latches `quitting` synchronously and does the waiting,
/// the teardown and the relaunch on its own thread, so the frontend gets its
/// `Ok` and the webview then dies underneath it mid-promise. The dialog must
/// therefore treat this call resolving as "the restart is under way", never
/// as "the restart finished", and must not try to render anything afterwards.
///
/// An `Err` is genuinely worth showing, though, and there is exactly one:
/// [`NO_DAEMON`], in `--replay` mode, where there is no daemon to restart.
#[tauri::command]
pub async fn restart_app(server: tauri::State<'_, Server>) -> Result<serde_json::Value, String> {
    call(&server, Request::Restart).await
}

/// Whether yappr currently starts itself at login -- read straight
/// off the filesystem (see the module doc's "why not a config key"), so a
/// file removed behind this app's back is reported truthfully instead of
/// from stale state. A single `Path::exists()` stat; no `spawn_blocking`
/// needed for that, unlike [`call`]'s `dispatch` or `list_input_devices`.
#[tauri::command]
pub fn autostart_status() -> serde_json::Value {
    let enabled = yappr_core::paths::autostart_desktop_file().exists();
    serde_json::json!({ "enabled": enabled })
}

/// Turns "Beim Anmelden starten" on or off by writing or removing the real
/// autostart entry. Same reasoning as [`autostart_status`] on why this is a
/// plain synchronous command: a single small write or remove, not the kind
/// of work `settings_cmds.rs`'s module doc reserves `spawn_blocking` for.
#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    set_autostart_at(&yappr_core::paths::autostart_desktop_file(), enabled).map_err(|e| {
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
    use yappr_core::proto::State as WireState;
    use std::collections::{HashMap, HashSet};
    use yappr_core::config::Theme;

    /// A file from the frontend, read relative to this crate rather than the
    /// cwd, so the test works from anywhere in the workspace.
    fn frontend(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} should be readable: {e}", path.display()))
    }

    /// `--p-*` slots declared per selector block in `palettes.css`, and the
    /// slots each stylesheet reads. Deliberately crude string scanning: the
    /// file is machine-shaped, and a CSS parser would be a dependency whose
    /// whole job is this one test.
    fn declared_blocks(css: &str) -> HashMap<String, HashSet<String>> {
        let mut out = HashMap::new();
        for block in css.split('}') {
            let Some((head, body)) = block.split_once('{') else { continue };
            let selector = head.rsplit("*/").next().unwrap_or(head).trim().to_string();
            if selector.is_empty() {
                continue;
            }
            let slots: HashSet<String> = body
                .lines()
                .filter_map(|l| l.trim().strip_prefix("--p-"))
                .filter_map(|l| l.split_once(':'))
                .map(|(name, _)| name.trim().to_string())
                .collect();
            out.entry(selector).or_insert_with(HashSet::new).extend(slots);
        }
        out
    }

    /// Slots a stylesheet reads with **no** fallback, i.e. the ones a palette
    /// must actually declare. `var(--p-hud, var(--p-base))` is excluded on
    /// purpose: that fallback is the mechanism by which a themed palette gets
    /// a themed capsule without declaring a single HUD slot.
    fn required_slots(css: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        for (i, _) in css.match_indices("var(--p-") {
            let rest = &css[i + "var(--p-".len()..];
            let end = rest.find([')', ',']).expect("a var() must be closed");
            if rest.as_bytes()[end] == b',' {
                continue; // has a fallback
            }
            out.insert(rest[..end].trim().to_string());
        }
        out
    }

    /// Which appearance each palette is, read out of `theme.ts`'s
    /// `APPEARANCE` table -- the copy of that fact the frontend actually uses.
    fn appearances(ts: &str) -> HashMap<String, String> {
        let body = ts
            .split_once("export const APPEARANCE = {")
            .expect("theme.ts should declare APPEARANCE")
            .1
            .split_once("} as const;")
            .expect("APPEARANCE should be closed")
            .0;
        body.lines()
            .filter_map(|l| l.trim().strip_suffix(','))
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| {
                (
                    k.trim().trim_matches('"').to_string(),
                    v.trim().trim_matches('"').to_string(),
                )
            })
            .collect()
    }

    /// The list of themes exists in four places and the compiler sees one of
    /// them: `Theme::ALL` here, the `[data-theme="..."]` blocks in
    /// `palettes.css`, `APPEARANCE` in `src/theme.ts`, and
    /// `ENUMS["ui.theme"]` plus `ENUM_LABELS` in `src/settings/schema.ts`.
    ///
    /// Every way they can drift produces a *silent* failure, which is why
    /// this is a test and not a comment: a theme missing from `palettes.css`
    /// selects fine and paints nothing, one missing from `schema.ts` cannot
    /// be chosen at all, and one missing from `theme.ts` gets no
    /// `data-appearance` and so loses its grain blend and its high-contrast
    /// palette. Same reasoning as the overlay-event fixture tests in
    /// `proto.rs` and `replay.rs`.
    #[test]
    fn themes_are_declared_everywhere_they_have_to_be() {
        let css = frontend("src/palettes.css");
        let ts = frontend("src/theme.ts");
        let schema = frontend("src/settings/schema.ts");
        let appearance = appearances(&ts);

        for theme in Theme::ALL {
            let name = theme.to_string();
            let selector = format!(r#":root[data-theme="{name}"]"#);

            if theme == Theme::System {
                // `system` is a configuration value, not a palette. It must
                // stay resolved in `theme.ts`, because a CSS block for it
                // would be the shipped palette written a second time.
                assert!(
                    !css.contains(&selector),
                    "`system` must not have a palette block: it is resolved to \
                     the shipped pair in theme.ts"
                );
                assert!(!appearance.contains_key(&name), "`system` has no fixed appearance");
            } else {
                assert!(css.contains(&selector), "palettes.css has no block for {name}");
                assert!(
                    appearance.contains_key(&name),
                    "theme.ts's APPEARANCE does not say whether {name} is light or dark"
                );
            }

            assert!(
                schema.contains(&format!("\"{name}\"")),
                "schema.ts never mentions {name}, so it cannot be chosen"
            );
        }

        // And nothing extra: a palette or an appearance entry the enum does
        // not have is a theme the GUI can never offer and Rust would reject.
        let known: HashSet<String> = Theme::ALL.iter().map(|t| t.to_string()).collect();
        for name in appearance.keys() {
            assert!(known.contains(name), "theme.ts declares an unknown theme: {name}");
        }
        for selector in declared_blocks(&css).keys() {
            if let Some(name) = selector
                .strip_prefix(r#":root[data-theme=""#)
                .and_then(|r| r.strip_suffix(r#""]"#))
            {
                assert!(known.contains(name), "palettes.css declares an unknown theme: {name}");
            }
        }
    }

    /// Every palette declares every slot the two windows actually read.
    ///
    /// This is the test that makes the ramp indirection safe to add a theme
    /// to. A slot a palette forgets is not a wrong colour -- `var(--p-x)`
    /// with nothing behind it is an *invalid* value, so the property drops
    /// out and the affected surface renders with no background or no text
    /// colour at all. Nothing in a build or a type-check notices that.
    #[test]
    fn every_palette_declares_every_slot_the_windows_read() {
        let css = frontend("src/palettes.css");
        let blocks = declared_blocks(&css);
        let appearance = appearances(&frontend("src/theme.ts"));

        let mut required = required_slots(&frontend("src/Settings.css"));
        required.extend(required_slots(&frontend("src/Overlay.css")));
        assert!(
            required.len() > 20,
            "the scan found only {} slots, which means it stopped working rather \
             than that the windows got simpler",
            required.len()
        );

        for (name, appear) in &appearance {
            let theme_block = blocks
                .get(&format!(r#":root[data-theme="{name}"]"#))
                .unwrap_or_else(|| panic!("no palette block for {name}"));
            let appearance_block = blocks
                .get(&format!(r#":root[data-appearance="{appear}"]"#))
                .unwrap_or_else(|| panic!("no appearance block for {appear}"));

            let missing: Vec<&String> = required
                .iter()
                .filter(|s| !theme_block.contains(*s) && !appearance_block.contains(*s))
                .collect();
            assert!(
                missing.is_empty(),
                "{name} declares no {missing:?} -- every surface using those \
                 slots would render with no colour at all"
            );
        }
    }

    /// `schema.ts`'s `DEPENDENT_FIELDS`, as
    /// `(section, key, dependency key, trigger values)`.
    fn dependent_fields(ts: &str) -> Vec<(String, String, String, Vec<String>)> {
        let body = ts
            .split_once("export const DEPENDENT_FIELDS: Record<string, { on: string; is: Json[] }> = {")
            .expect("schema.ts should declare DEPENDENT_FIELDS")
            .1
            .split_once("};")
            .expect("DEPENDENT_FIELDS should be closed")
            .0;
        body.lines()
            .filter_map(|l| l.trim().strip_prefix('"'))
            .filter_map(|l| l.split_once("\":"))
            .map(|(path, rule)| {
                let (section, key) = path.split_once('.').expect("a path is section.key");
                let on = rule
                    .split_once("on:")
                    .expect("a rule has an `on`")
                    .1
                    .split(',')
                    .next()
                    .unwrap()
                    .trim()
                    .trim_matches('"')
                    .to_string();
                let is = rule
                    .split_once("is: [")
                    .expect("a rule has an `is`")
                    .1
                    .split_once(']')
                    .expect("`is` should be closed")
                    .0
                    .split(',')
                    .map(|v| v.trim().trim_matches('"').to_string())
                    .filter(|v| !v.is_empty())
                    .collect();
                (section.to_string(), key.to_string(), on, is)
            })
            .collect()
    }

    /// A conditionally shown row must depend on a key that exists and on
    /// values Rust would actually accept for it.
    ///
    /// `DEPENDENT_FIELDS` is the one thing in `schema.ts` that hides a *real*
    /// setting, and every way it can drift is silent. A trigger value spelled
    /// the way Rust does not (`"Script"`, or a variant renamed in
    /// `InjectBackend`) hides the row for *every* value of the dependency,
    /// which is a setting the GUI can no longer reach; a dependency key that
    /// no longer exists goes the other way and shows the row always. Neither
    /// is visible to `tsc` or to `vite build`, because the frontend keeps no
    /// copy of the schema on purpose.
    #[test]
    fn a_dependent_row_names_a_real_key_and_values_rust_accepts() {
        use yappr_core::config::Config;

        let rules = dependent_fields(&frontend("src/settings/schema.ts"));
        assert!(!rules.is_empty(), "the scan found nothing, so it stopped working");

        let defaults = serde_json::to_value(Config::default()).expect("a config is JSON");
        for (section, key, on, is) in rules {
            let table = defaults
                .get(&section)
                .and_then(|s| s.as_object())
                .unwrap_or_else(|| panic!("no [{section}] section, so {section}.{key} is unreachable"));
            assert!(table.contains_key(&key), "[{section}] has no `{key}` to hide");
            assert!(
                table.contains_key(&on),
                "{section}.{key} depends on `{on}`, which [{section}] does not have"
            );
            assert!(!is.is_empty(), "{section}.{key} would be hidden for every value");

            for want in &is {
                let mut patched = defaults.clone();
                patched[&section][&on] = serde_json::Value::String(want.clone());
                serde_json::from_value::<Config>(patched).unwrap_or_else(|e| {
                    panic!("{section}.{on} = {want:?} is not a value Rust accepts: {e}")
                });
            }
        }
    }

    /// A fresh, collision-free scratch directory for a single test. Not a
    /// dependency: `tempfile` isn't in `[dev-dependencies]` here either (see
    /// the identical helper in `yappr-core`'s `inject.rs` and `owf-cli`'s --
    /// now this crate's -- `setup.rs` tests). Load-bearing for this file in
    /// particular: this is what keeps every autostart test off the real
    /// `~/.config/autostart/`, which on this machine holds five files this
    /// project must never touch, one of them another dictation app's own
    /// autostart entry.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-settings-cmds-test-{tag}-{}-{n}", std::process::id()))
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
        let p = dir.join("yappr.desktop");

        set_autostart_at(&p, true).unwrap();
        set_autostart_at(&p, true).unwrap();
        assert!(p.exists());
        assert!(std::fs::read_to_string(&p).unwrap().contains("Exec=yappr"));

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
        let p = dir.join("yappr.desktop");
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
        let p = dir.join("yappr.desktop");

        set_autostart_at(&p, true).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.starts_with("[Desktop Entry]"));
        assert!(content.contains("Type=Application"));
        assert!(content.contains("Exec=yappr"));
        assert!(!content.contains("Hidden"));
        assert!(!content.contains("OnlyShowIn"));
        assert!(!content.contains("NotShowIn"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The version line must name the binary that renders it, not a number
    /// typed beside it: this pins that [`APP_VERSION`] is built out of this
    /// crate's own `CARGO_PKG_VERSION`, and that the `-debug` marker tracks
    /// the build profile rather than being pasted on for good.
    #[test]
    fn the_reported_version_is_this_crates_own_and_says_when_it_is_a_debug_build() {
        assert!(APP_VERSION.starts_with(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            APP_VERSION.ends_with("-debug"),
            cfg!(debug_assertions),
            "the -debug suffix must follow the build profile and nothing else"
        );
    }

    /// [`APP_VERSION`] comes from `Cargo.toml`; the AppImage's filename, the
    /// `.desktop` entry and every other bundle artefact come from
    /// `tauri.conf.json`. Nothing makes those two agree on their own, so a
    /// release that bumps one and forgets the other ships a binary reporting
    /// a different version from the file it arrived in -- precisely the
    /// confusion the sidebar line is there to end.
    #[test]
    fn the_binary_and_the_bundle_agree_on_the_version_number() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json must be valid JSON");
        assert_eq!(conf["version"], serde_json::json!(env!("CARGO_PKG_VERSION")));
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
    /// including `config_path` staying snake_case, since `Response` has no
    /// `rename_all` attribute. The settings window stopped rendering that
    /// field when its sidebar foot became the version line, but the key is
    /// still part of the socket's answer to `GetConfig` and still the shape
    /// any other client reads it by -- dropping it would be a wire change,
    /// not a GUI one.
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
