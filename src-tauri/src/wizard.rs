//! The first-run setup wizard's backend: whether to open it, what to tell it,
//! and what to do when the user finishes it.
//!
//! The wizard replaces the Setup pane that used to live in the settings
//! window. `provision.rs` is untouched by that change -- `setup_status`,
//! `run_setup` and the `setup-progress` event stream are exactly what they
//! were, and this module reuses them rather than reimplementing provisioning.

use std::path::Path;

use yappr_core::desktop::{self, Desktop};

/// The rule, over facts rather than a world to read -- one definition, two
/// call sites, so `lib.rs`'s startup thread and the settings window can never
/// disagree about whether setup is finished.
///
/// Two independent reasons to open, either sufficient: the wizard was never
/// finished, or the install is not usable. The second is what turns a deleted
/// model into a wizard rather than into a failed dictation three days later.
fn should_open(marker_present: bool, ready: bool) -> bool {
    !marker_present || !ready
}

pub(crate) fn should_open_wizard_at(marker: &Path, ready: bool) -> bool {
    should_open(marker.exists(), ready)
}

/// [`should_open_wizard_at`], reading the world. Blocking:
/// `is_ready_or_assume_not` hashes whatever models are on disk, so this must
/// not run on the Tauri event-loop thread -- see its one caller in `lib.rs`,
/// which is a plain `std::thread::spawn`.
pub(crate) fn should_open_wizard() -> bool {
    should_open_wizard_at(
        &yappr_core::paths::wizard_marker(),
        crate::provision::is_ready_or_assume_not("yappr"),
    )
}

/// Writes the marker, creating the state directory if this is a brand-new
/// machine that has never written anything there. Idempotent: a plain
/// overwrite of a fixed path, so clicking Fertig twice leaves one file.
pub(crate) fn write_marker(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"")
}

/// The injection backend that works on `d`.
///
/// GNOME is the exception and Mutter is the reason: it does not implement the
/// virtual-keyboard protocol `wtype` types through, so `wtype` silently does
/// nothing there. Everywhere else `wtype` is right *and* free -- no daemon,
/// no `/dev/uinput`.
pub(crate) fn recommended_backend(d: &Desktop) -> &'static str {
    match d {
        Desktop::Gnome => "ydotool",
        _ => "wtype",
    }
}

/// What else has to be true for [`recommended_backend`] to actually type.
///
/// Not folded into `setup.rs`'s `check_prerequisites`. That check knows the
/// desktop too (it has to: `wtype` is inapplicable on GNOME, not missing),
/// but it reports a missing `ydotool` as *optional* everywhere, including
/// here -- a fatal gap keeps `provision::is_ready` false, which would reopen
/// this wizard on every launch over a requirement no `pacman -S` line can
/// satisfy on its own. `ydotoold` has to be running as well, and that is
/// only sayable in prose: this list, shown in the wizard's shortcut step.
pub(crate) fn backend_prereqs(d: &Desktop) -> Vec<&'static str> {
    match d {
        Desktop::Gnome => vec!["ydotool", "ydotoold"],
        _ => Vec::new(),
    }
}

/// Shapes the answer, given facts rather than a world to read -- split out
/// exactly as `provision::build_status` is, so every branch is testable
/// without a desktop session, a config file or a model directory.
pub(crate) fn build_wizard_state(
    marker_present: bool,
    ready: bool,
    d: &Desktop,
    current_backend: &str,
) -> serde_json::Value {
    // The models step is where a returning user with a broken install needs
    // to land -- they do not need to be introduced to the app again. A first
    // run, and any hand-opened wizard, gets the whole flow from the top.
    let start_step = if marker_present && !ready { "models" } else { "welcome" };
    serde_json::json!({
        "should_open": should_open(marker_present, ready),
        "start_step": start_step,
        "desktop": d.key(),
        "desktop_name": d.display(),
        "recommended_backend": recommended_backend(d),
        "current_backend": current_backend,
        "backend_prereqs": backend_prereqs(d),
        "shortcut": desktop::shortcut_instructions(d),
    })
}

/// Everything the wizard needs to render, in one round trip.
///
/// `async fn` handing its blocking work to `spawn_blocking` for the reason
/// `provision.rs`'s module doc gives: `is_ready_or_assume_not` hashes up to
/// ~1,1 GB of models the first time it runs, and must never run inline on
/// WebKitGTK's main loop.
#[tauri::command]
pub async fn wizard_state() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let marker_present = yappr_core::paths::wizard_marker().exists();
        let ready = crate::provision::is_ready_or_assume_not("yappr");
        let d = desktop::detect();
        // A config that will not load is not a reason to withhold the whole
        // wizard -- it is a reason to show the default and let the user fix
        // things. A broken install is what the wizard is *for*.
        let current = yappr_core::config::Config::load()
            .map(|c| match c.inject.backend {
                yappr_core::config::InjectBackend::Ydotool => "ydotool",
                yappr_core::config::InjectBackend::Clipboard => "clipboard",
                yappr_core::config::InjectBackend::Wtype => "wtype",
            })
            .unwrap_or("wtype");
        build_wizard_state(marker_present, ready, &d, current)
    })
    .await
    .map_err(|e| format!("interner Fehler: {e}"))
}

/// The `SetConfig` payload that sets the injection backend and nothing else.
///
/// One leaf, deliberately: `config_write` merges rather than replaces, so
/// sending a fuller `[inject]` table would silently rewrite the two settings
/// beside it as well.
pub(crate) fn backend_patch(backend: &str) -> serde_json::Value {
    serde_json::json!({ "inject": { "backend": backend } })
}

/// The last step's Fertig button: remember that setup is done, optionally set
/// the injection backend, and put the window away.
///
/// `set_backend` is `Some` only on a genuine first run (the frontend decides
/// from `wizard_state`'s `start_step` and passes it explicitly, rather than
/// having this re-derive it -- a marker written moments earlier in the same
/// click would otherwise change the answer). On a re-run it is `None`, so a
/// user who deliberately switched to `ydotool` on Hyprland to reach an
/// XWayland window does not have that undone by a wizard they reopened for
/// another reason.
///
/// The window is hidden here rather than by the frontend because
/// `src-tauri/capabilities/` scopes `core:window:allow-hide` to the overlay;
/// doing it Rust-side needs no capability at all.
#[tauri::command]
pub async fn wizard_finish(
    app: tauri::AppHandle,
    server: tauri::State<'_, crate::settings_cmds::Server>,
    set_backend: Option<String>,
) -> Result<(), String> {
    if let Some(backend) = set_backend {
        crate::settings_cmds::set_config(server, backend_patch(&backend)).await?;
    }

    write_marker(&yappr_core::paths::wizard_marker())
        .map_err(|e| format!("Einrichtungsstatus konnte nicht gespeichert werden: {e}"))?;

    if let Some(w) = tauri::Manager::get_webview_window(&app, crate::SETTINGS_LABEL) {
        let _ = w.hide();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, collision-free scratch directory for a single test. The house
    /// idiom (see `yappr-core`'s `debug.rs`) -- no `tempfile` dependency.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("yappr-wizard-test-{tag}-{}-{n}", std::process::id()))
    }

    /// A fresh install opens the wizard even with the models already on disk:
    /// nobody has been told what this app is, or which keys to press. A
    /// finished install stops asking. A model deleted afterwards brings it
    /// back rather than surfacing as a failed dictation later.
    #[test]
    fn the_wizard_opens_until_it_is_finished_and_again_if_the_install_breaks() {
        let dir = scratch_dir("gate");
        let marker = dir.join("wizard-done");
        assert!(should_open_wizard_at(&marker, true));
        assert!(should_open_wizard_at(&marker, false));

        write_marker(&marker).unwrap();
        write_marker(&marker).unwrap(); // idempotent, and it created `dir`
        assert!(!should_open_wizard_at(&marker, true));
        assert!(should_open_wizard_at(&marker, false));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Mutter does not implement the virtual-keyboard protocol `wtype` needs,
    /// so GNOME is the one desktop that has to pay `ydotool`'s setup cost.
    #[test]
    fn gnome_is_the_only_desktop_that_recommends_ydotool() {
        assert_eq!(recommended_backend(&Desktop::Gnome), "ydotool");
        assert_eq!(backend_prereqs(&Desktop::Gnome), vec!["ydotool", "ydotoold"]);
        assert_eq!(recommended_backend(&Desktop::Hyprland), "wtype");
        assert!(backend_prereqs(&Desktop::Hyprland).is_empty());
        assert_eq!(recommended_backend(&Desktop::Unknown), "wtype");
    }

    #[test]
    fn the_start_step_is_the_models_step_only_for_a_returning_user_whose_install_broke() {
        let fresh = build_wizard_state(false, false, &Desktop::Hyprland, "wtype");
        assert_eq!(fresh["should_open"], true);
        assert_eq!(fresh["start_step"], "welcome");
        assert_eq!(fresh["desktop"], "hyprland");

        let broken = build_wizard_state(true, false, &Desktop::Gnome, "ydotool");
        assert_eq!(broken["start_step"], "models");
        assert_eq!(broken["shortcut"]["kind"], "gnome");
        assert_eq!(broken["shortcut"]["bindings"][0]["command"], "yappr --toggle");
        assert_eq!(broken["backend_prereqs"][0], "ydotool");

        // Opened by hand from the tray on a healthy install: an explicit
        // request gets the whole flow.
        let healthy = build_wizard_state(true, true, &Desktop::Hyprland, "wtype");
        assert_eq!(healthy["should_open"], false);
        assert_eq!(healthy["start_step"], "welcome");
    }

    /// `config_write` merges rather than replaces, so a patch carrying the
    /// whole `[inject]` table would also overwrite `trailing_space` and
    /// `keystroke_delay_ms` with whatever this process happened to think they
    /// were -- a different and much worse thing than setting a backend.
    #[test]
    fn the_backend_patch_names_only_the_one_key_it_changes() {
        assert_eq!(backend_patch("ydotool"), serde_json::json!({ "inject": { "backend": "ydotool" } }));
        assert_eq!(backend_patch("wtype")["inject"].as_object().unwrap().len(), 1);
    }
}
