//! The first-run setup wizard's backend: whether to open it, what to tell it,
//! and what to do when the user finishes it.
//!
//! The wizard replaces the Setup pane that used to live in the settings
//! window. `provision.rs` is untouched by that change -- `setup_status`,
//! `run_setup` and the `setup-progress` event stream are exactly what they
//! were, and this module reuses them rather than reimplementing provisioning.

use std::path::Path;

use yappr_core::desktop::{self, Desktop};

/// Whether the setup wizard opens: at startup, by itself, and as a
/// replacement for the settings form once the window is up. One question,
/// one answer, over a fact rather than a world to read -- has a human ever
/// reached the last step and clicked Fertig on this machine?
///
/// It had a second, sufficient reason until 2026-09-09: `provision::is_ready`
/// being false, so that a model deleted after setup surfaced as a wizard
/// rather than as a failed dictation three days later. That reason is gone,
/// and both halves of why are worth keeping:
///
/// - **It reopened forever over gaps it could not close.** `ready` is false
///   for a missing prerequisite binary and for a hash mismatch too, neither
///   of which the wizard has a button for -- and `ydotoold` not *running* is
///   not even expressible as a check. A window that opens itself on every
///   launch until someone fixes something it never names is not a warning,
///   it is a nag.
/// - **It could not explain itself.** The wizard's models step reports the
///   model list, so a `ready: false` caused by anything else reads as "Alle
///   Modelle sind vorhanden" *and* "Einrichtung unvollständig" at the same
///   time -- the exact contradiction that prompted this change.
///
/// The warning did not go away, it moved: `build_wizard_state` sends the
/// whole `setup_status` answer along, and `Settings.tsx` renders it as a
/// banner **naming what is missing**, with the wizard one click behind it.
/// Seeing it costs opening the settings window, which is what the tray is
/// for. See invariant 13.
fn should_open(marker_present: bool) -> bool {
    !marker_present
}

pub(crate) fn should_show_window_at(marker: &Path) -> bool {
    should_open(marker.exists())
}

/// [`should_show_window_at`], reading the world -- one `stat`, since the
/// answer no longer depends on the install's health. Still called off the
/// event-loop thread by its one caller in `lib.rs`, which also shows the
/// window from there.
pub(crate) fn should_show_settings_at_startup() -> bool {
    should_show_window_at(&yappr_core::paths::wizard_marker())
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

/// [`write_marker`] against the real path -- **the one thing that makes
/// [`should_open`] ever answer `false`**, and therefore the only reason the
/// wizard stops taking over the settings window on every single launch.
///
/// It exists as a named function because the bug it fixes was that only
/// *one* route out of the wizard wrote it: the last step's Fertig button. A
/// user who loaded the models, copied the shortcut, and then left by any
/// other door -- "Einstellungen öffnen" on the last step, "Einstellungen" on
/// the models step, or simply closing the window, which is what a finished
/// window invites -- got the whole wizard again at the next start, forever,
/// with nothing on screen explaining why. Reported exactly that way on
/// 2026-09-10, on a machine with every model present, the shortcut bound,
/// and no `wizard-done` next to its `config.toml`.
///
/// So every exit funnels through here now, and the marker means what its
/// path says: *this machine has been through setup*. The callers:
///
/// - [`wizard_finish`] -- Fertig, and it writes this **before** the backend
///   patch it also does, so a `set_config` that fails cannot take the marker
///   down with it (it used to `?` out one line above the write).
/// - [`wizard_dismiss`] -- the two "Einstellungen" buttons, via
///   `Settings.tsx`'s `onOpenSettings`.
/// - `lib.rs`'s close-request hook on the settings window -- the door with
///   no button, and the one a user who considers themselves done reaches for.
///
/// A failure is returned *and* logged: a state dir that cannot be written is
/// the one case where the wizard legitimately comes back, and then the user
/// gets told which path failed instead of watching it reappear in silence.
pub(crate) fn remember_setup_seen() -> Result<(), String> {
    let path = yappr_core::paths::wizard_marker();
    write_marker(&path).map_err(|e| {
        let msg =
            format!("Einrichtungsstatus konnte nicht gespeichert werden ({}): {e}", path.display());
        eprintln!("wizard: {msg}");
        msg
    })
}

/// Leaving the wizard without finishing it: the "Einstellungen" button on
/// the models step and "Einstellungen öffnen" on the last one. Both are the
/// user saying they are done with this window's wizard mode, which is
/// exactly what the marker records -- see [`remember_setup_seen`].
#[tauri::command]
pub fn wizard_dismiss() -> Result<(), String> {
    remember_setup_seen()
}

/// The injection backend that works on `d` out of the box.
///
/// GNOME is the exception and Mutter is the reason: it does not implement
/// the virtual-keyboard protocol `wtype` types through, so `wtype` silently
/// does nothing there. Everywhere else `wtype` is right *and* free -- no
/// daemon, no `/dev/uinput`.
///
/// GNOME's answer was `ydotool` until 2026-09-09, at the cost of a package,
/// a systemd unit and write access to `/dev/uinput`, and `clipboard` after
/// it: the transcript lands in the clipboard and the user pastes it.
///
/// `ydotool` is a selectable backend again since 2026-09-11, and this still
/// does not recommend it. A *recommendation* is what a first run applies
/// without asking, and this one cannot check the thing that decides whether
/// it works -- `ydotoold` running, reachable on the socket its client looks
/// at. Recommending it would hand a new GNOME user a backend that fails
/// silently into the clipboard fallback anyway, which is what `clipboard`
/// does honestly. Both it and `[inject] script` are a paragraph in the
/// wizard, pointing at the Verfahren dropdown, which is where a choice the
/// user makes for themselves belongs.
pub(crate) fn recommended_backend(d: &Desktop) -> &'static str {
    match d {
        Desktop::Gnome => "clipboard",
        _ => "wtype",
    }
}

/// What else has to be true for [`recommended_backend`] to actually work.
///
/// Empty on every desktop since 2026-09-09, and kept rather than deleted
/// because the *shape* of the question is still right. It existed for one
/// thing no package check could express -- `ydotoold` having to be running,
/// on top of `pacman -S ydotool` -- and it stays empty now that backend is
/// selectable again, because nothing *recommends* ydotool: the two
/// recommendations left need nothing beyond the fatal prerequisites
/// `setup.rs` already reports. The
/// wizard renders its card only when this is non-empty, so an empty list
/// means the card is simply absent.
pub(crate) fn backend_prereqs(_d: &Desktop) -> Vec<&'static str> {
    Vec::new()
}

/// Shapes the answer, given facts rather than a world to read -- split out
/// exactly as `provision::build_status` is, so every branch is testable
/// without a desktop session, a config file or a model directory.
///
/// `setup` is `provision::build_status`'s own answer, passed through whole
/// rather than reduced to a `ready` flag: the settings banner has to *name*
/// what is missing (a package is not a model), and the wizard's models step
/// already has a renderer for exactly this shape.
pub(crate) fn build_wizard_state(
    marker_present: bool,
    setup: serde_json::Value,
    d: &Desktop,
    current_backend: &str,
) -> serde_json::Value {
    let ready = setup["ready"].as_bool().unwrap_or(false);
    // The models step is where a returning user with a broken install needs
    // to land -- they do not need to be introduced to the app again. A first
    // run, and any hand-opened wizard, gets the whole flow from the top.
    let start_step = if marker_present && !ready { "models" } else { "welcome" };
    serde_json::json!({
        "should_open": should_open(marker_present),
        // The banner's condition and its contents, and the only way the
        // settings form learns that provisioning is incomplete: with the
        // wizard no longer opening itself, nothing else would say so.
        "setup": setup,
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
/// `provision.rs`'s module doc gives: the setup check hashes up to ~1,1 GB
/// of models the first time it runs, and must never run inline on
/// WebKitGTK's main loop.
#[tauri::command]
pub async fn wizard_state() -> Result<serde_json::Value, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let marker_present = yappr_core::paths::wizard_marker().exists();
        let setup = crate::provision::status_or_assume_incomplete("yappr");
        let d = desktop::detect();
        // A config that will not load is not a reason to withhold the whole
        // wizard -- it is a reason to show the default and let the user fix
        // things. A broken install is what the wizard is *for*.
        let current = yappr_core::config::Config::load()
            .map(|c| match c.inject.backend {
                yappr_core::config::InjectBackend::Ydotool => "ydotool",
                yappr_core::config::InjectBackend::Script => "script",
                yappr_core::config::InjectBackend::Clipboard => "clipboard",
                yappr_core::config::InjectBackend::Wtype => "wtype",
            })
            .unwrap_or("wtype");
        build_wizard_state(marker_present, setup, &d, current)
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
/// user who deliberately switched to `script` on Hyprland to reach an
/// XWayland window does not have that undone by a wizard they reopened for
/// another reason.
///
/// The window is hidden here rather than by the frontend because
/// `src-tauri/capabilities/` scopes `core:window:allow-hide` to the overlay;
/// doing it Rust-side needs no capability at all.
///
/// **The marker is written first, and unconditionally.** The two jobs used to
/// run the other way round, with the backend patch `?`-ing out one line above
/// the write -- so a `set_config` that failed for any reason at all (a
/// config the daemon rejects, no `Daemon` managed under `--replay`, a
/// read-only file) silently cost the user the one bit that stops the wizard
/// reopening, while the frontend closed the wizard anyway and showed nothing.
/// Neither half depends on the other, so neither may be able to lose the
/// other: both errors are collected and the marker's is reported first.
#[tauri::command]
pub async fn wizard_finish(
    app: tauri::AppHandle,
    server: tauri::State<'_, crate::settings_cmds::Server>,
    set_backend: Option<String>,
) -> Result<(), String> {
    let remembered = remember_setup_seen();

    let patched = match set_backend {
        // `app` so the save broadcasts the configured palette like any
        // other, which the wizard needs for a reason of its own: it takes
        // over the whole settings window, so this is the first save some
        // installs ever make.
        Some(backend) => {
            crate::settings_cmds::set_config(app.clone(), server, backend_patch(&backend))
                .await
                // The saved config comes back for the settings form's
                // benefit; this caller has no use for it, and `Settings.tsx`
                // re-reads through `load(true)` right after anyway.
                .map(|_| ())
        }
        None => Ok(()),
    };

    if let Some(w) = tauri::Manager::get_webview_window(&app, crate::SETTINGS_LABEL) {
        let _ = w.hide();
    }

    remembered.and(patched)
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

    /// A fresh install shows the window even with the models already on
    /// disk: nobody has been told what this app is, or which keys to press.
    /// A finished install stops asking, and stays stopped -- see the test
    /// below for the second half of that.
    #[test]
    fn the_window_opens_until_setup_is_finished() {
        let dir = scratch_dir("gate");
        let marker = dir.join("wizard-done");
        assert!(should_show_window_at(&marker));

        write_marker(&marker).unwrap();
        write_marker(&marker).unwrap(); // idempotent, and it created `dir`
        assert!(!should_show_window_at(&marker));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file from this repo, read relative to this crate rather than the
    /// cwd, so the test works from anywhere in the workspace. Same idiom as
    /// `settings_cmds`' theme tests and `proto.rs`'s fixture test.
    fn source(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} should be readable: {e}", path.display()))
    }

    /// **The bug this whole module was reopened for, on 2026-09-10.** The
    /// marker was written by exactly one thing -- the last step's Fertig
    /// button -- and the wizard has three other exits: two "Einstellungen"
    /// buttons and the window's own close control. Take any of them and the
    /// marker stays unwritten, so `should_open` keeps answering `true` and
    /// the wizard takes over the settings window on every launch, forever,
    /// on a machine with every model downloaded and the shortcut bound.
    ///
    /// Deliberately crude string scanning across two languages, for the same
    /// reason the theme tests do it: the wiring is the invariant, it spans
    /// Rust and TSX, and nothing else can notice one of these three calls
    /// being deleted. The Rust side of the close hook is `lib.rs`'s, inside
    /// a window-event callback no unit test can drive.
    #[test]
    fn every_exit_from_the_wizard_persists_the_marker() {
        let settings = source("src/Settings.tsx");
        for call in ["wizard_finish", "wizard_dismiss"] {
            assert!(
                settings.contains(&format!("invoke(\"{call}\"")),
                "Settings.tsx must still call {call}: every way out of the wizard has to \
                 write the marker, or it reopens on the next launch",
            );
        }
        // The wizard's own two exits, and the handlers they are wired to.
        let wizard_tsx = source("src/settings/wizard.tsx");
        assert_eq!(
            // The models step's "Einstellungen" and the last step's
            // "Einstellungen öffnen". A third button added without this
            // handler is a fourth unmarked exit.
            wizard_tsx.matches("onClick={onOpenSettings}").count(),
            2,
            "both 'Einstellungen' buttons must still go through onOpenSettings",
        );
        assert!(wizard_tsx.contains("onFinish(firstRun"), "Fertig must still call onFinish");

        // The door with no button. `hide_instead_of_close`'s callback cannot
        // be invoked from a test, so this asserts the wiring is present.
        let lib = source("src-tauri/src/lib.rs");
        assert!(
            lib.contains("wizard::remember_setup_seen"),
            "closing the settings window must still record that setup was seen",
        );
    }

    /// The other half of the same bug: `wizard_finish` used to `?` out of the
    /// backend patch *before* writing the marker, so anything that made
    /// `set_config` fail also cost the user the marker -- and the frontend
    /// closed the wizard regardless, which is a wizard that reappears next
    /// launch having reported nothing. A failed write is now returned, not
    /// dropped, and the settings window says so in its banner slot.
    ///
    /// The failure is provoked with a *file* where the state directory has to
    /// be, rather than a read-only directory: deterministic, and it stays a
    /// failure when the tests run as root.
    #[test]
    fn a_marker_that_cannot_be_written_is_reported_rather_than_dropped() {
        let dir = scratch_dir("unwritable");
        std::fs::create_dir_all(&dir).unwrap();
        let blocked = dir.join("state-dir-is-a-file");
        std::fs::write(&blocked, b"").unwrap();

        // The kind is deliberately not asserted: `create_dir_all` over a
        // path whose parent is a file reports `AlreadyExists` (EEXIST) here,
        // not `NotADirectory`. What matters is that it is an `Err` at all,
        // because the caller's whole job is to pass that on.
        let err = write_marker(&blocked.join("wizard-done")).unwrap_err();
        assert!(!err.to_string().is_empty(), "the failure has to be describable to the user");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The regression this gate was narrowed for (2026-09-09, second pass):
    /// once the wizard has been finished, *nothing* about the install's
    /// current health reopens it. `provision`'s `ready` used to be a second,
    /// sufficient reason, and it is a fact the wizard cannot always explain
    /// -- a missing prerequisite binary or a hash mismatch reads as "setup
    /// incomplete" while the models step says "Alle Modelle sind vorhanden".
    #[test]
    fn a_finished_setup_never_opens_the_wizard_by_itself_again() {
        let dir = scratch_dir("finished");
        let marker = dir.join("wizard-done");
        write_marker(&marker).unwrap();

        assert!(!should_show_window_at(&marker));
        // Health is not an input at all any more -- the type says so, and
        // this is the assertion that keeps it that way: an unusable install
        // is a banner in the settings window (`setup` on the wire), not a
        // window that opens itself on every launch.
        let broken = build_wizard_state(
            true,
            crate::provision::build_status(vec!["wl-clipboard"], vec!["parakeet".to_string()]),
            &Desktop::Hyprland,
            "wtype",
        );
        assert_eq!(broken["should_open"], false);
        assert_eq!(broken["setup"]["ready"], false);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The two questions -- open the window, and replace the settings form
    /// with the wizard -- are one question again, and the marker is its only
    /// input. They were briefly separate (marker-or-unready for the window,
    /// marker-only for the take-over); `should_open`'s doc comment records
    /// why the first half went away too.
    #[test]
    fn only_a_first_run_opens_the_wizard() {
        assert!(should_open(false), "a first run is the wizard's whole purpose");
        assert!(
            !should_open(true),
            "a finished install must reach its settings, missing model or not"
        );
    }

    /// GNOME recommends the one backend that needs nothing installed.
    ///
    /// Mutter implements neither the virtual-keyboard protocol `wtype` types
    /// through nor anything else yappr can drive itself, and the `ydotool`
    /// recommendation that used to fill that gap went with the backend on
    /// 2026-09-09. `clipboard` is honest: the transcript lands in the
    /// clipboard and the user presses Ctrl+V. Anyone who wants that
    /// automated writes a script and points `[inject] script` at it -- which
    /// no prerequisite check can verify, because the script does not exist
    /// until they write it.
    #[test]
    fn gnome_recommends_the_backend_that_needs_no_setup() {
        assert_eq!(recommended_backend(&Desktop::Gnome), "clipboard");
        assert!(backend_prereqs(&Desktop::Gnome).is_empty());
        assert_eq!(recommended_backend(&Desktop::Hyprland), "wtype");
        assert_eq!(recommended_backend(&Desktop::Unknown), "wtype");
    }

    #[test]
    fn no_desktop_asks_the_user_to_install_anything_for_its_backend() {
        // `backend_prereqs` exists to name what a `pacman -S` line cannot
        // finish -- it was `ydotoold` having to be *running*. That backend
        // is selectable again since 2026-09-11 but is not recommended by
        // any desktop, so nothing here qualifies, and the wizard's card must
        // not reappear for some other desktop by accident.
        for d in [
            Desktop::Gnome,
            Desktop::Hyprland,
            Desktop::Other("sway".to_string()),
            Desktop::Unknown,
        ] {
            assert!(backend_prereqs(&d).is_empty(), "{d:?} still lists prerequisites");
        }
    }

    #[test]
    fn the_start_step_is_the_models_step_only_for_a_returning_user_whose_install_broke() {
        let ready = || crate::provision::build_status(vec![], vec![]);
        let no_model = || crate::provision::build_status(vec![], vec!["parakeet".to_string()]);

        let fresh = build_wizard_state(false, no_model(), &Desktop::Hyprland, "wtype");
        assert_eq!(fresh["should_open"], true);
        assert_eq!(fresh["setup"]["ready"], false);
        assert_eq!(fresh["start_step"], "welcome");
        assert_eq!(fresh["desktop"], "hyprland");

        let broken = build_wizard_state(true, no_model(), &Desktop::Gnome, "clipboard");
        // Nothing opens by itself: the banner (`setup`) is what tells the
        // user, and the wizard is one click behind it, opened at the step
        // that matters.
        assert_eq!(broken["should_open"], false);
        assert_eq!(broken["setup"]["ready"], false);
        assert_eq!(broken["start_step"], "models");
        assert_eq!(broken["shortcut"]["kind"], "gnome");
        assert_eq!(broken["shortcut"]["bindings"][0]["command"], "yappr --toggle");
        assert!(
            broken["backend_prereqs"].as_array().unwrap().is_empty(),
            "no desktop has backend prerequisites since ydotool was retired"
        );

        // Opened by hand from the tray on a healthy install: an explicit
        // request gets the whole flow.
        let healthy = build_wizard_state(true, ready(), &Desktop::Hyprland, "wtype");
        assert_eq!(healthy["should_open"], false);
        assert_eq!(healthy["setup"]["ready"], true);
        assert_eq!(healthy["start_step"], "welcome");
    }

    /// The banner has to be able to say *which* gap it means, which is the
    /// whole reason `setup` travels whole rather than as a `ready` flag: a
    /// missing package is not a missing model, and the wording that blamed
    /// the model for both is what sent a user looking at a models step
    /// reporting everything present.
    #[test]
    fn the_wizard_state_carries_what_is_missing_not_just_that_something_is() {
        let state = build_wizard_state(
            true,
            crate::provision::build_status(vec!["wl-clipboard"], vec!["s1-mini".to_string()]),
            &Desktop::Hyprland,
            "wtype",
        );
        assert_eq!(state["setup"]["missing_prerequisites"][0], "wl-clipboard");
        assert_eq!(state["setup"]["missing_models"][0]["name"], "s1-mini");
        assert_eq!(
            state["setup"]["missing_models"][0]["display"],
            "S1-mini by Superwhisper (de-v3 Finetune)"
        );
    }

    /// `config_write` merges rather than replaces, so a patch carrying the
    /// whole `[inject]` table would also overwrite `trailing_space` and
    /// `keystroke_delay_ms` with whatever this process happened to think they
    /// were -- a different and much worse thing than setting a backend.
    #[test]
    fn the_backend_patch_names_only_the_one_key_it_changes() {
        assert_eq!(
            backend_patch("script"),
            serde_json::json!({ "inject": { "backend": "script" } })
        );
        assert_eq!(backend_patch("wtype")["inject"].as_object().unwrap().len(), 1);
    }
}
