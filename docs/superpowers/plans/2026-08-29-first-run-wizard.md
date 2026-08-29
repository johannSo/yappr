# First-run setup wizard — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the settings window's Setup pane with a four-step first-run
wizard that leaves a fresh install actually usable — models on disk, injection
backend matched to the desktop, and the two shortcut lines in front of the user.

**Architecture:** Desktop detection and per-desktop shortcut text go into
`yappr-core` as a new `desktop.rs`, testable without a GUI. A marker file in
the state dir plus the existing `setup_status()` decide whether the wizard
opens. The wizard is a mode of the existing settings window, not a new window:
`Settings.tsx` renders `<Wizard>` full-bleed instead of sidebar-and-panes. The
provisioning machinery underneath (`setup_status`, `run_setup`, the
`setup-progress` events) is reused unchanged — only its consumer moves.

**Tech Stack:** Rust (`yappr-core`, Tauri 2 in `src-tauri`), React 19 +
TypeScript + `motion/react` in `src/`, Vite, bun.

**Spec:** `docs/superpowers/specs/2026-08-29-first-run-wizard-design.md`

## Global Constraints

- **Verification gate:** `cargo test --workspace && cargo clippy --workspace --all-targets`. There is no CI; this is the gate.
- **`gtk-layer-shell` must be installed** or nothing in `src-tauri` compiles at all (`sudo pacman -S gtk-layer-shell`).
- **All user-facing strings are German.** The settings window is German throughout; the wizard matches.
- **Never write the user's desktop config.** Not `~/.config/hypr/*`, not `gsettings`. The wizard emits text and the user pastes it. This is a standing project rule (CLAUDE.md), not a preference.
- **Never open the microphone.** No task here records audio.
- **Config writes go through `config_write`** (i.e. `Request::SetConfig`), never `toml::to_string` — invariant 9.
- **`#[serde(deny_unknown_fields)]` is on every config struct** — invariant 4. This feature adds **no** `config.toml` key; the marker is a state file. Do not add a config section.
- **Model size is ~1,1 GB**, written German-style with a comma. Not 1.4 GB.
- **Tests are named as full sentences** (`fn the_wizard_opens_when_the_marker_is_absent()`), matching the existing convention.
- **No new dependencies.** Scratch directories in tests use the house idiom (`std::env::temp_dir()` + pid + counter, see `crates/yappr-core/src/debug.rs:618`), not `tempfile`.
- **Pure function + thin wrapper.** Anything that reads the environment or the filesystem gets split: a pure function the tests drive, and a wrapper that touches the world. See `cli::route`, `provision::build_status`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/yappr-core/src/desktop.rs` | **new** — which desktop this is, and the shortcut text for it |
| `crates/yappr-core/src/hypr.rs` | modified — expose the Lua-vs-`.conf` probe so `desktop.rs` can name the target file |
| `crates/yappr-core/src/paths.rs` | modified — `wizard_marker()` |
| `crates/yappr-core/src/proto.rs` | modified — `Request::ShowWizard` |
| `crates/yappr-core/src/server.rs` | modified — `EventSink::show_wizard`, its dispatch arm |
| `crates/yappr-core/src/lib.rs` | modified — `pub mod desktop;` |
| `src-tauri/src/wizard.rs` | **new** — the gate, `wizard_state`, `wizard_finish` |
| `src-tauri/src/lib.rs` | modified — `show_wizard_window`, sink override, startup gate, command registration |
| `src-tauri/src/cli.rs` | modified — `--wizard` |
| `src-tauri/src/tray.rs` | modified — `Einrichtung…` menu item |
| `src-tauri/capabilities/settings.json` | **new** — event permissions for the settings window |
| `src/settings/wizard.tsx` | **new** — the whole wizard flow and its own setup state |
| `src/Settings.tsx` | modified — wizard branch; `SetupPane` deleted |
| `src/settings/schema.ts` | modified — `SETUP_CATEGORY` deleted |
| `src/Settings.css` | modified — `.wizard*` classes |

---

### Task 1: Desktop detection

**Files:**
- Create: `crates/yappr-core/src/desktop.rs`
- Modify: `crates/yappr-core/src/lib.rs` (module list, alphabetical — between `debug` and `finish`)

**Interfaces:**
- Consumes: nothing.
- Produces: `yappr_core::desktop::{Desktop, detect, detect_from}`. `Desktop` is
  `enum { Hyprland, Gnome, Other(String), Unknown }` with methods
  `display(&self) -> &str` and `key(&self) -> &'static str`.

- [ ] **Step 1: Write the failing test**

Create `crates/yappr-core/src/desktop.rs` containing *only* the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Hyprland sets `HYPRLAND_INSTANCE_SIGNATURE` for every client it
    /// launches, so it is evidence rather than configuration -- it wins over
    /// an `XDG_CURRENT_DESKTOP` that says something else, which is what a
    /// session started from a display manager with a stale profile looks
    /// like.
    #[test]
    fn the_hyprland_signature_outranks_a_conflicting_xdg_desktop() {
        assert_eq!(detect_from(Some("abc123"), Some("GNOME"), None), Desktop::Hyprland);
    }

    /// An exported-but-empty variable is not evidence of anything.
    #[test]
    fn an_empty_hyprland_signature_is_not_evidence() {
        assert_eq!(detect_from(Some(""), Some("GNOME"), None), Desktop::Gnome);
    }

    /// `XDG_CURRENT_DESKTOP` is a colon-separated *list*; `ubuntu:GNOME` is a
    /// real value on a real distribution, and a substring match would also
    /// accept `NOT-GNOME`, so the comparison is per component.
    #[test]
    fn xdg_current_desktop_is_matched_per_colon_separated_component() {
        assert_eq!(detect_from(None, Some("ubuntu:GNOME"), None), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("GNOME"), None), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("Hyprland"), None), Desktop::Hyprland);
    }

    #[test]
    fn desktop_names_are_matched_case_insensitively() {
        assert_eq!(detect_from(None, Some("gnome"), None), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("hyprland"), None), Desktop::Hyprland);
    }

    #[test]
    fn xdg_session_desktop_is_the_fallback_when_current_desktop_says_nothing() {
        assert_eq!(detect_from(None, None, Some("gnome")), Desktop::Gnome);
        assert_eq!(detect_from(None, Some(""), Some("Hyprland")), Desktop::Hyprland);
    }

    /// A desktop we do not support is reported *by name* rather than as
    /// `Unknown`: the wizard's unsupported-desktop step says which one it
    /// found, and "KDE" is more useful to a user than "unbekannt".
    #[test]
    fn an_unsupported_desktop_keeps_its_name() {
        assert_eq!(detect_from(None, Some("KDE"), None), Desktop::Other("KDE".into()));
        assert_eq!(detect_from(None, Some("sway"), None), Desktop::Other("sway".into()));
    }

    #[test]
    fn nothing_set_at_all_is_unknown() {
        assert_eq!(detect_from(None, None, None), Desktop::Unknown);
        assert_eq!(detect_from(None, Some(""), Some("")), Desktop::Unknown);
    }

    /// `key` is what crosses the wire to the wizard's TypeScript; `display`
    /// is what a human reads. They are different strings on purpose.
    #[test]
    fn every_desktop_has_a_wire_key_and_a_human_name() {
        assert_eq!(Desktop::Hyprland.key(), "hyprland");
        assert_eq!(Desktop::Gnome.key(), "gnome");
        assert_eq!(Desktop::Other("KDE".into()).key(), "other");
        assert_eq!(Desktop::Unknown.key(), "unknown");

        assert_eq!(Desktop::Hyprland.display(), "Hyprland");
        assert_eq!(Desktop::Gnome.display(), "GNOME");
        assert_eq!(Desktop::Other("KDE".into()).display(), "KDE");
        assert_eq!(Desktop::Unknown.display(), "unbekannt");
    }
}
```

Add the module to `crates/yappr-core/src/lib.rs`, keeping the list alphabetical:

```rust
pub mod debug;
pub mod desktop;
pub mod finish;
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yappr-core desktop::`
Expected: FAIL to compile — `cannot find type Desktop in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/yappr-core/src/desktop.rs`, above the test module:

```rust
//! Which desktop environment this session is, and what the user has to paste
//! into it to get a dictation shortcut.
//!
//! Lives here rather than in `src-tauri` for the same reason `hypr.rs` does:
//! it is knowledge about the surrounding desktop, and it is testable without
//! a GUI. The wizard (`src-tauri/src/wizard.rs`) is the only caller today.
//!
//! yappr supports **Hyprland and GNOME**. Everything else resolves to
//! [`Desktop::Other`], which is a degraded path -- generic instructions
//! naming the two commands to bind -- not a refusal.

/// The desktop this session is running under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desktop {
    Hyprland,
    Gnome,
    /// Recognised by name, but with no shortcut recipe of its own.
    Other(String),
    /// Nothing in the environment said anything at all.
    Unknown,
}

impl Desktop {
    /// The name a human reads, in the wizard's German UI.
    pub fn display(&self) -> &str {
        match self {
            Desktop::Hyprland => "Hyprland",
            Desktop::Gnome => "GNOME",
            Desktop::Other(name) => name,
            Desktop::Unknown => "unbekannt",
        }
    }

    /// The stable key that crosses the wire to the wizard's TypeScript.
    /// Deliberately not derived from [`display`]: `Other`'s display name is
    /// arbitrary user-environment text, and the frontend must switch on a
    /// closed set.
    pub fn key(&self) -> &'static str {
        match self {
            Desktop::Hyprland => "hyprland",
            Desktop::Gnome => "gnome",
            Desktop::Other(_) => "other",
            Desktop::Unknown => "unknown",
        }
    }
}

/// Whether a colon-separated desktop list names `want`, compared per
/// component and case-insensitively. A substring test would accept
/// `NOT-GNOME`; a whole-string test would reject `ubuntu:GNOME`.
fn names(list: Option<&str>, want: &str) -> bool {
    list.is_some_and(|s| s.split(':').any(|part| part.trim().eq_ignore_ascii_case(want)))
}

/// The first non-empty component of a colon-separated desktop list.
fn first_name(list: Option<&str>) -> Option<String> {
    list.and_then(|s| s.split(':').map(str::trim).find(|p| !p.is_empty()))
        .map(str::to_string)
}

/// The decision, as a pure function over three environment variables -- this
/// is what the tests drive. See [`detect`] for the wrapper that reads them.
pub fn detect_from(
    hyprland_signature: Option<&str>,
    xdg_current_desktop: Option<&str>,
    xdg_session_desktop: Option<&str>,
) -> Desktop {
    if hyprland_signature.is_some_and(|s| !s.is_empty()) {
        return Desktop::Hyprland;
    }
    for list in [xdg_current_desktop, xdg_session_desktop] {
        if names(list, "hyprland") {
            return Desktop::Hyprland;
        }
        if names(list, "gnome") {
            return Desktop::Gnome;
        }
    }
    first_name(xdg_current_desktop)
        .or_else(|| first_name(xdg_session_desktop))
        .map(Desktop::Other)
        .unwrap_or(Desktop::Unknown)
}

/// [`detect_from`], reading the environment.
pub fn detect() -> Desktop {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
    let current = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let session = std::env::var("XDG_SESSION_DESKTOP").ok();
    detect_from(sig.as_deref(), current.as_deref(), session.as_deref())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr-core desktop::`
Expected: PASS, 8 tests.

Run: `cargo clippy -p yappr-core --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/yappr-core/src/desktop.rs crates/yappr-core/src/lib.rs
git commit -m "feat(desktop): detect Hyprland and GNOME from the session environment"
```

---

### Task 2: Per-desktop shortcut instructions

**Files:**
- Modify: `crates/yappr-core/src/hypr.rs` (`shortcut_config`, around line 223)
- Modify: `crates/yappr-core/src/desktop.rs`

**Interfaces:**
- Consumes: `Desktop` from Task 1; `hypr::shortcut_config()`.
- Produces:
  - `hypr::uses_lua_config_here() -> bool`
  - `desktop::{ShortcutKind, ShortcutBinding, ShortcutInstructions, shortcut_instructions}`
  - `ShortcutInstructions { kind: ShortcutKind, target: Option<String>, snippet: String, bindings: Vec<ShortcutBinding> }`, all `Serialize`
  - `ShortcutBinding { name: String, command: String, keys: String }`

> **Note — refines spec §9.** The spec sketched this as
> `fields: [{label, value}]`. A flat label/value list renders two shortcuts as
> six rows with repeated labels; `bindings` groups them into the two cards the
> GNOME step actually needs. Same information, better shape.

- [ ] **Step 1: Write the failing test**

Append to `crates/yappr-core/src/desktop.rs`'s `mod tests`:

```rust
    /// Hyprland's snippet is `hypr::shortcut_config()` verbatim -- the text
    /// that already carries the migration lines naming the dead
    /// `openwhisprflow`/`owf-ctl` binds to delete. This asserts the wiring,
    /// not the text: `hypr.rs` owns the text and tests it.
    #[test]
    fn the_hyprland_snippet_is_the_config_hypr_already_emits() {
        let it = shortcut_instructions(&Desktop::Hyprland);
        assert_eq!(it.snippet, crate::hypr::shortcut_config());
        assert!(it.snippet.contains("yappr --toggle"));
        assert!(it.snippet.contains("yappr --cancel"));
        assert!(it.bindings.is_empty(), "the snippet is authoritative on Hyprland");
    }

    /// The kind and the target file must agree: naming `bindings.lua` while
    /// emitting `.conf` syntax is worse than naming neither.
    #[test]
    fn the_hyprland_target_file_matches_the_snippets_syntax() {
        let it = shortcut_instructions(&Desktop::Hyprland);
        let target = it.target.expect("Hyprland always names a file to paste into");
        match it.kind {
            ShortcutKind::HyprLua => assert!(target.ends_with("bindings.lua"), "got {target}"),
            ShortcutKind::HyprConf => assert!(target.ends_with("hyprland.conf"), "got {target}"),
            other => panic!("Hyprland must not resolve to {other:?}"),
        }
    }

    #[test]
    fn gnome_gets_both_bindings_and_a_gsettings_snippet() {
        let it = shortcut_instructions(&Desktop::Gnome);
        assert_eq!(it.kind, ShortcutKind::Gnome);
        assert_eq!(it.bindings.len(), 2);
        assert_eq!(it.bindings[0].command, "yappr --toggle");
        assert_eq!(it.bindings[0].keys, "Super+D");
        assert_eq!(it.bindings[1].command, "yappr --cancel");
        assert_eq!(it.bindings[1].keys, "Super+Alt+D");
        assert!(it.snippet.contains("<Super>d"));
        assert!(it.snippet.contains("<Super><Alt>d"));
        assert!(it.target.is_some(), "GNOME names where in Settings to go");
    }

    /// The one thing this snippet must never do is replace the user's
    /// existing custom keybindings. The `case` that appends instead of
    /// overwriting is the whole reason the snippet is more than one line --
    /// see spec §4.2.
    #[test]
    fn the_gnome_snippet_appends_to_custom_keybindings_rather_than_replacing_them() {
        let it = shortcut_instructions(&Desktop::Gnome);
        assert!(it.snippet.contains("CUR=$(gsettings get"), "must read the current list first");
        assert!(it.snippet.contains("${CUR%]}"), "must append to it");
        assert!(it.snippet.contains("*yappr-toggle*"), "must be idempotent when re-run");
    }

    /// An unsupported desktop is a degraded path, not a dead end: it still
    /// learns the two commands to bind.
    #[test]
    fn an_unsupported_desktop_still_gets_the_two_commands() {
        for d in [Desktop::Other("KDE".into()), Desktop::Unknown] {
            let it = shortcut_instructions(&d);
            assert_eq!(it.kind, ShortcutKind::Generic);
            assert_eq!(it.bindings.len(), 2);
            assert!(it.snippet.contains("yappr --toggle"));
            assert!(it.snippet.contains("yappr --cancel"));
            assert!(it.target.is_none(), "there is no file we can name for an unknown desktop");
        }
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yappr-core desktop::`
Expected: FAIL to compile — `cannot find function shortcut_instructions`.

- [ ] **Step 3a: Expose the Lua-vs-`.conf` probe from `hypr.rs`**

In `crates/yappr-core/src/hypr.rs`, replace the body of `shortcut_config` (it
currently computes the config dir inline) with:

```rust
/// `~/.config/hypr`, or a path that cannot exist when there is no config dir
/// at all -- in which case [`uses_lua_config`] answers `false` and the
/// classic `.conf` block is emitted, which is the safer default (Lua is the
/// newer, opt-in format).
fn hypr_config_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .map(|d| d.join("hypr"))
        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent"))
}

/// Whether *this machine* configures Hyprland in Lua. Public because
/// `desktop.rs` has to name the file the user pastes into
/// (`bindings.lua` vs `hyprland.conf`), and that answer must come from the
/// same probe [`shortcut_config`] branches on -- two probes could disagree,
/// and then the wizard would name a file whose syntax does not match the
/// text above it.
pub fn uses_lua_config_here() -> bool {
    uses_lua_config(&hypr_config_dir())
}

/// The shortcut/window-rule snippet appropriate to this machine (spec §3,
/// §10) -- what `yappr --print-shortcuts` prints. Renamed from
/// `hypr_config()`: this crate no longer emits anything Hyprland-specific
/// beyond the shortcut bindings and the overlay's fallback window rule, so
/// the name should say what the function actually produces.
///
/// Picks Lua when `~/.config/hypr/hyprland.lua` exists, `.conf` otherwise.
pub fn shortcut_config() -> &'static str {
    if uses_lua_config_here() {
        SHORTCUT_CONFIG_LUA
    } else {
        SHORTCUT_CONFIG_CONF
    }
}
```

- [ ] **Step 3b: Write `shortcut_instructions`**

Add to `crates/yappr-core/src/desktop.rs`, above the test module:

```rust
use serde::Serialize;

/// Which recipe the wizard is showing. The frontend switches on this, so it
/// is a closed set with a stable wire spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ShortcutKind {
    HyprLua,
    HyprConf,
    Gnome,
    Generic,
}

/// One shortcut, as three fields a user copies into a form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShortcutBinding {
    pub name: String,
    pub command: String,
    pub keys: String,
}

/// Everything the wizard's shortcut step renders for one desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShortcutInstructions {
    pub kind: ShortcutKind,
    /// Where the snippet goes: a file path on Hyprland, a menu path on
    /// GNOME, `None` where we cannot know.
    pub target: Option<String>,
    /// The copyable block.
    pub snippet: String,
    /// The two shortcuts as form fields. Empty on Hyprland, where the
    /// snippet is the authoritative thing to paste.
    pub bindings: Vec<ShortcutBinding>,
}

fn bindings() -> Vec<ShortcutBinding> {
    vec![
        ShortcutBinding {
            name: "yappr: Diktat starten/stoppen".into(),
            command: "yappr --toggle".into(),
            keys: "Super+D".into(),
        },
        ShortcutBinding {
            name: "yappr: Diktat abbrechen".into(),
            command: "yappr --cancel".into(),
            keys: "Super+Alt+D".into(),
        },
    ]
}

/// The `gsettings` route for GNOME, offered *behind* the GUI instructions --
/// see this function's doc comment and spec §4.2.
///
/// The obvious one-liner (`gsettings set ... custom-keybindings "[...]"`)
/// **overwrites** the list and silently destroys every custom shortcut the
/// user already had. The `case` below reads the current value, leaves it
/// alone if ours is already in it, and otherwise appends -- so it is safe to
/// paste on a machine that has custom shortcuts, and safe to run twice.
/// `@as []` is what `gsettings get` prints for an empty list; the plain `[]`
/// arm covers older versions that print that instead.
const GNOME_GSETTINGS_SNIPPET: &str = r#"BASE=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings
SCHEMA=org.gnome.settings-daemon.plugins.media-keys.custom-keybinding

gsettings set $SCHEMA:$BASE/yappr-toggle/ name    'yappr: Diktat starten/stoppen'
gsettings set $SCHEMA:$BASE/yappr-toggle/ command 'yappr --toggle'
gsettings set $SCHEMA:$BASE/yappr-toggle/ binding '<Super>d'

gsettings set $SCHEMA:$BASE/yappr-cancel/ name    'yappr: Diktat abbrechen'
gsettings set $SCHEMA:$BASE/yappr-cancel/ command 'yappr --cancel'
gsettings set $SCHEMA:$BASE/yappr-cancel/ binding '<Super><Alt>d'

CUR=$(gsettings get org.gnome.settings-daemon.plugins.media-keys custom-keybindings)
case "$CUR" in
  "@as []"|"[]")   NEW="['$BASE/yappr-toggle/', '$BASE/yappr-cancel/']" ;;
  *yappr-toggle*)  NEW="$CUR" ;;
  *)               NEW="${CUR%]}, '$BASE/yappr-toggle/', '$BASE/yappr-cancel/']" ;;
esac
gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings "$NEW"
"#;

const GENERIC_SNIPPET: &str = "\
yappr --toggle    # Diktat starten/stoppen
yappr --cancel    # Diktat abbrechen
";

/// What the wizard's shortcut step shows for `d`.
///
/// Nothing here writes anything. Every desktop gets text to paste, never an
/// action taken on the user's behalf -- a standing project rule for Hyprland
/// (a bad window rule breaks their desktop) and, per spec §4.2, for GNOME
/// too (the obvious `gsettings` call eats their existing shortcuts).
pub fn shortcut_instructions(d: &Desktop) -> ShortcutInstructions {
    match d {
        Desktop::Hyprland => {
            let lua = crate::hypr::uses_lua_config_here();
            ShortcutInstructions {
                kind: if lua { ShortcutKind::HyprLua } else { ShortcutKind::HyprConf },
                target: Some(
                    if lua { "~/.config/hypr/bindings.lua" } else { "~/.config/hypr/hyprland.conf" }
                        .to_string(),
                ),
                snippet: crate::hypr::shortcut_config().to_string(),
                bindings: Vec::new(),
            }
        }
        Desktop::Gnome => ShortcutInstructions {
            kind: ShortcutKind::Gnome,
            target: Some(
                "Einstellungen → Tastatur → Tastenkürzel anpassen → Eigene Tastenkürzel".into(),
            ),
            snippet: GNOME_GSETTINGS_SNIPPET.to_string(),
            bindings: bindings(),
        },
        Desktop::Other(_) | Desktop::Unknown => ShortcutInstructions {
            kind: ShortcutKind::Generic,
            target: None,
            snippet: GENERIC_SNIPPET.to_string(),
            bindings: bindings(),
        },
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr-core -- desktop:: hypr::`
Expected: PASS. The `hypr::` tests must still pass unchanged — the refactor in
step 3a moved code without changing behaviour.

Run: `cargo clippy -p yappr-core --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/yappr-core/src/desktop.rs crates/yappr-core/src/hypr.rs
git commit -m "feat(desktop): per-desktop shortcut instructions for the wizard"
```

---

### Task 3: `Request::ShowWizard` and `EventSink::show_wizard`

**Files:**
- Modify: `crates/yappr-core/src/proto.rs` (enum around line 25; tests around line 255 and 293)
- Modify: `crates/yappr-core/src/server.rs` (`EventSink` around line 197; `dispatch`'s `ShowSettings` arm around line 2281)

**Interfaces:**
- Consumes: nothing.
- Produces: `proto::Request::ShowWizard` (wire form `{"cmd":"show-wizard"}`), `server::EventSink::show_wizard(&self)` with a no-op default.

- [ ] **Step 1: Write the failing tests**

In `crates/yappr-core/src/proto.rs`, add to the serialisation test beside the
existing `ShowSettings` assertion:

```rust
        assert_eq!(
            serde_json::to_string(&Request::ShowWizard).unwrap(),
            r#"{"cmd":"show-wizard"}"#
        );
```

and add `Request::ShowWizard,` to the round-trip test's array, directly after
`Request::ShowSettings,`.

In `crates/yappr-core/src/server.rs`, add a test to `mod tests`, next to the
`Request::ShowSettings` tests (search for `-- Task 10:`):

```rust
    /// The tray's Einrichtung item and `yappr --wizard` both land here. A
    /// sink that does not override `show_wizard` must stay valid, which is
    /// what the no-op default is for -- so this test's sink overrides only
    /// the one method it cares about.
    #[test]
    fn show_wizard_reaches_the_sink_and_leaves_the_state_alone() {
        struct WizardSink(Arc<AtomicUsize>);
        impl EventSink for WizardSink {
            fn emit(&self, _event: &OverlayEvent) {}
            fn show_wizard(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let d = fake_daemon_with_sink(IDLE, Arc::new(WizardSink(Arc::clone(&calls))));

        let response = dispatch(&d, Request::ShowWizard);

        assert!(response.ok);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(d.state.load(Ordering::SeqCst), IDLE, "showing a window is not a state change");
    }
```

If `AtomicUsize` is not already imported in that test module, add it to the
existing `use std::sync::atomic::...` line.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core -- proto:: server::tests::show_wizard`
Expected: FAIL to compile — `no variant named ShowWizard`.

- [ ] **Step 3: Write the implementation**

In `crates/yappr-core/src/proto.rs`, directly after the `ShowSettings` variant:

```rust
    /// Show the settings window with the first-run wizard on top of it
    /// (`yappr --wizard`, and the tray's Einrichtung item). Distinct from
    /// [`Request::ShowSettings`] because the two land the user in different
    /// places -- one in the settings form, one at the top of the setup flow.
    ShowWizard,
```

In `crates/yappr-core/src/server.rs`, in `trait EventSink`, directly after
`show_settings`:

```rust
    /// Shows the settings window with the wizard on top (`Request::ShowWizard`).
    /// A no-op default for the same reason `show_settings`'s is: only
    /// `TauriSink` owns a window to show.
    fn show_wizard(&self) {}
```

and in `dispatch`, directly after the `Request::ShowSettings` arm:

```rust
        Request::ShowWizard => {
            daemon.sink.show_wizard();
            Response::ok(state_of(current))
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr-core`
Expected: PASS. Every pre-existing test still passes — the new variant is
additive and every existing sink keeps compiling because the trait method has
a default body.

Run: `cargo clippy -p yappr-core --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/yappr-core/src/proto.rs crates/yappr-core/src/server.rs
git commit -m "feat(proto): add Request::ShowWizard and EventSink::show_wizard"
```

---

### Task 4: The marker file and the first-run gate

**Files:**
- Modify: `crates/yappr-core/src/paths.rs`
- Create: `src-tauri/src/wizard.rs`
- Modify: `src-tauri/src/lib.rs` (module list near line 20; the startup thread at lines 313–320)

**Interfaces:**
- Consumes: `provision::is_ready_or_assume_not` (already `pub(crate)`).
- Produces: `yappr_core::paths::wizard_marker() -> PathBuf`;
  `crate::wizard::{should_open_wizard_at, should_open_wizard, write_marker}`.
  `write_marker(path: &Path) -> std::io::Result<()>` creates parent dirs and
  writes an empty file; Task 6 calls it.

- [ ] **Step 1: Write the failing tests**

In `crates/yappr-core/src/paths.rs`, add to `mod tests`:

```rust
    /// The marker records one fact -- "a human finished the wizard" -- and
    /// belongs with the other things this app remembers between runs, beside
    /// `rejections.jsonl`, not in `config.toml`: it is not a setting, and a
    /// new config key would have to be added to a `deny_unknown_fields`
    /// struct and then shown in the settings GUI.
    #[test]
    fn the_wizard_marker_lives_in_the_state_dir() {
        assert!(wizard_marker().ends_with("yappr/wizard-done"));
        assert_eq!(wizard_marker().parent(), rejections_file().parent());
    }
```

Create `src-tauri/src/wizard.rs` with only its test module for now:

```rust
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

    #[test]
    fn a_fresh_install_opens_the_wizard() {
        let marker = scratch_dir("fresh").join("wizard-done");
        assert!(should_open_wizard_at(&marker, false));
        // Even with everything already downloaded: nobody has been told what
        // this app is, or which keys to press.
        assert!(should_open_wizard_at(&marker, true));
    }

    /// The whole point of the marker: a finished install stops asking.
    #[test]
    fn a_finished_install_leaves_the_wizard_closed() {
        let dir = scratch_dir("finished");
        let marker = dir.join("wizard-done");
        write_marker(&marker).unwrap();
        assert!(!should_open_wizard_at(&marker, true));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A model deleted after setup brings the wizard back rather than
    /// surfacing as a failed dictation later.
    #[test]
    fn a_broken_install_reopens_the_wizard_even_with_the_marker_present() {
        let dir = scratch_dir("broken");
        let marker = dir.join("wizard-done");
        write_marker(&marker).unwrap();
        assert!(should_open_wizard_at(&marker, false));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `Fertig` clicked twice, or a marker written into a state dir that does
    /// not exist yet on a brand-new machine, must both just work.
    #[test]
    fn writing_the_marker_creates_its_directory_and_is_idempotent() {
        let dir = scratch_dir("idempotent");
        let marker = dir.join("nested").join("wizard-done");
        write_marker(&marker).unwrap();
        write_marker(&marker).unwrap();
        assert!(marker.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core paths::` — FAIL: `cannot find function wizard_marker`.
Then add `mod wizard;` to `src-tauri/src/lib.rs`'s module list (alphabetical, after `mod tray;`) and run
`cargo test -p yappr wizard::` — FAIL: `cannot find function should_open_wizard_at`.

- [ ] **Step 3: Write the implementation**

In `crates/yappr-core/src/paths.rs`, after `rejections_file`:

```rust
/// Records that a human reached the last step of the setup wizard and clicked
/// Fertig. An empty file: it carries one bit and no format, so there is
/// nothing to version. Deliberately not a `config.toml` key -- see the test
/// below, and invariant 4.
pub fn wizard_marker() -> PathBuf {
    state_dir().join("wizard-done")
}
```

Prepend to `src-tauri/src/wizard.rs`, above its test module:

```rust
//! The first-run setup wizard's backend: whether to open it, what to tell it,
//! and what to do when the user finishes it.
//!
//! The wizard replaces the Setup pane that used to live in the settings
//! window. `provision.rs` is untouched by that change -- `setup_status`,
//! `run_setup` and the `setup-progress` event stream are exactly what they
//! were, and this module reuses them rather than reimplementing provisioning.

use std::path::Path;

/// The decision, as a pure function -- this is what the tests drive.
///
/// Two independent reasons to open, either sufficient: the wizard was never
/// finished, or the install is not usable. The second is what turns a deleted
/// model into a wizard rather than into a failed dictation three days later.
pub(crate) fn should_open_wizard_at(marker: &Path, ready: bool) -> bool {
    !marker.exists() || !ready
}

/// [`should_open_wizard_at`], reading the world. Blocking: `is_ready_or_assume_not`
/// hashes whatever models are on disk, so this must not run on the Tauri
/// event-loop thread -- see its one caller in `lib.rs`, which is a plain
/// `std::thread::spawn`.
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr-core paths:: && cargo test -p yappr wizard::`
Expected: PASS, 1 + 4 tests.

- [ ] **Step 5: Wire the gate into startup**

In `src-tauri/src/lib.rs`, replace the startup check (currently
`if !provision::is_ready_or_assume_not("yappr")`) with:

```rust
                    // Now that the tray (Task 12) exists, Einstellungen is
                    // always reachable -- this is a redundant safety net,
                    // not the only way in, for a first-run user who has not
                    // yet noticed the tray icon: a one-time check, off the
                    // event-loop thread (it hashes whatever models are
                    // already on disk), that opens the window for exactly
                    // the machines that need it and does nothing on every
                    // other run.
                    //
                    // The window it opens comes up in wizard mode: the
                    // frontend asks `wizard_state()` on mount and decides
                    // for itself, so there is no ordering hazard between
                    // this thread and the webview's first paint.
                    let setup_check_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        if wizard::should_open_wizard() {
                            if let Some(w) = setup_check_handle.get_webview_window(SETTINGS_LABEL) {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    });
```

- [ ] **Step 6: Verify the workspace still builds clean**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/yappr-core/src/paths.rs src-tauri/src/wizard.rs src-tauri/src/lib.rs
git commit -m "feat(wizard): gate first-run on a marker file plus setup readiness"
```

---

### Task 5: The `wizard_state` command

**Files:**
- Modify: `src-tauri/src/wizard.rs`
- Modify: `src-tauri/src/lib.rs` (`invoke_handler` list, around line 210)

**Interfaces:**
- Consumes: `desktop::{detect, shortcut_instructions, Desktop}` (Tasks 1–2), `provision::is_ready_or_assume_not`.
- Produces: the `wizard_state` Tauri command, plus the pure
  `recommended_backend(&Desktop) -> &'static str`,
  `backend_prereqs(&Desktop) -> Vec<&'static str>` and
  `build_wizard_state(...) -> serde_json::Value` behind it.

The JSON it answers with (spec §9):

```json
{
  "should_open": true,
  "start_step": "welcome",
  "desktop": "hyprland",
  "desktop_name": "Hyprland",
  "recommended_backend": "wtype",
  "current_backend": "wtype",
  "backend_prereqs": [],
  "shortcut": { "kind": "hypr-lua", "target": "~/.config/hypr/bindings.lua",
                "snippet": "...", "bindings": [] }
}
```

- [ ] **Step 1: Write the failing tests**

Append to `src-tauri/src/wizard.rs`'s `mod tests`:

```rust
    /// Mutter does not implement the virtual-keyboard protocol `wtype` needs,
    /// so GNOME is the one desktop that has to pay `ydotool`'s setup cost.
    /// Everything else gets the default, which needs none.
    #[test]
    fn gnome_is_the_only_desktop_that_recommends_ydotool() {
        assert_eq!(recommended_backend(&Desktop::Gnome), "ydotool");
        assert_eq!(recommended_backend(&Desktop::Hyprland), "wtype");
        assert_eq!(recommended_backend(&Desktop::Other("KDE".into())), "wtype");
        assert_eq!(recommended_backend(&Desktop::Unknown), "wtype");
    }

    /// `ydotool` is not self-contained: it needs a running `ydotoold` and
    /// access to `/dev/uinput`. `wtype` needs neither, so recommending it
    /// carries no extra prerequisites at all.
    #[test]
    fn only_the_ydotool_recommendation_carries_extra_prerequisites() {
        assert_eq!(backend_prereqs(&Desktop::Gnome), vec!["ydotool", "ydotoold"]);
        assert!(backend_prereqs(&Desktop::Hyprland).is_empty());
    }

    #[test]
    fn a_fresh_install_starts_the_wizard_at_the_welcome_step() {
        let v = build_wizard_state(false, false, &Desktop::Hyprland, "wtype");
        assert_eq!(v["should_open"], true);
        assert_eq!(v["start_step"], "welcome");
        assert_eq!(v["desktop"], "hyprland");
        assert_eq!(v["desktop_name"], "Hyprland");
        assert_eq!(v["recommended_backend"], "wtype");
        assert_eq!(v["current_backend"], "wtype");
    }

    /// A returning user whose install broke does not need to be introduced to
    /// the app again -- they need the one screen that fixes what broke.
    #[test]
    fn a_broken_install_starts_the_wizard_at_the_models_step() {
        let v = build_wizard_state(true, false, &Desktop::Gnome, "ydotool");
        assert_eq!(v["should_open"], true);
        assert_eq!(v["start_step"], "models");
    }

    /// Opened by hand from the tray on a healthy install: `should_open` is
    /// false, and an explicit request gets the whole flow from the top.
    #[test]
    fn a_healthy_install_reopened_by_hand_starts_at_the_welcome_step() {
        let v = build_wizard_state(true, true, &Desktop::Hyprland, "wtype");
        assert_eq!(v["should_open"], false);
        assert_eq!(v["start_step"], "welcome");
    }

    /// The shortcut block travels inside the same answer, so the wizard's
    /// third step needs no second round trip.
    #[test]
    fn the_state_carries_the_shortcut_instructions_for_the_detected_desktop() {
        let v = build_wizard_state(false, false, &Desktop::Gnome, "wtype");
        assert_eq!(v["shortcut"]["kind"], "gnome");
        assert_eq!(v["shortcut"]["bindings"][0]["command"], "yappr --toggle");
        assert_eq!(v["backend_prereqs"][0], "ydotool");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr wizard::`
Expected: FAIL to compile — `cannot find function recommended_backend`.

- [ ] **Step 3: Write the implementation**

Add to `src-tauri/src/wizard.rs`, above the test module:

```rust
use yappr_core::desktop::{self, Desktop};

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
/// Not folded into `setup.rs`'s `check_prerequisites`: that list is
/// desktop-independent and deliberately reports a missing `ydotool` as
/// optional, because on the default `wtype` backend it is. This list is the
/// desktop-specific half, and it is shown in the wizard's shortcut step
/// rather than its model step for that reason.
pub(crate) fn backend_prereqs(d: &Desktop) -> Vec<&'static str> {
    match d {
        Desktop::Gnome => vec!["ydotool", "ydotoold"],
        _ => Vec::new(),
    }
}

/// Shapes the answer, given facts rather than a world to read -- split out
/// exactly as `provision::build_status` is, so every branch above is testable
/// without a desktop session, a config file or a model directory.
pub(crate) fn build_wizard_state(
    marker_present: bool,
    ready: bool,
    d: &Desktop,
    current_backend: &str,
) -> serde_json::Value {
    let should_open = should_open(marker_present, ready);
    // The models step is where a returning user with a broken install needs
    // to land; a first run, and any hand-opened wizard, gets the whole flow.
    let start_step = if marker_present && !ready { "models" } else { "welcome" };
    serde_json::json!({
        "should_open": should_open,
        "start_step": start_step,
        "desktop": d.key(),
        "desktop_name": d.display(),
        "recommended_backend": recommended_backend(d),
        "current_backend": current_backend,
        "backend_prereqs": backend_prereqs(d),
        "shortcut": desktop::shortcut_instructions(d),
    })
}

/// The same decision [`should_open_wizard_at`] makes, over facts instead of a
/// path -- one definition, two call sites, so the startup thread and the
/// window can never disagree about whether setup is finished.
fn should_open(marker_present: bool, ready: bool) -> bool {
    !marker_present || !ready
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
        // things. The wizard is what a broken install is *for*.
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
```

Rewrite `should_open_wizard_at` from Task 4 to delegate, so there is one rule:

```rust
pub(crate) fn should_open_wizard_at(marker: &Path, ready: bool) -> bool {
    should_open(marker.exists(), ready)
}
```

Register the command in `src-tauri/src/lib.rs`'s `invoke_handler`, after
`provision::run_setup`:

```rust
            provision::run_setup,
            wizard::wizard_state
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr wizard::`
Expected: PASS, 10 tests.

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/wizard.rs src-tauri/src/lib.rs
git commit -m "feat(wizard): add the wizard_state command"
```

---

### Task 6: The `wizard_finish` command

**Files:**
- Modify: `src-tauri/src/wizard.rs`
- Modify: `src-tauri/src/lib.rs` (`invoke_handler`)

**Interfaces:**
- Consumes: `write_marker` (Task 4), `settings_cmds::Server`, `yappr_core::server::dispatch`, `SETTINGS_LABEL`.
- Produces: the `wizard_finish` command, taking `set_backend: Option<String>`, plus the pure `backend_patch(&str) -> serde_json::Value`.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/src/wizard.rs`'s `mod tests`:

```rust
    /// The patch must name exactly one leaf. `config_write` merges into the
    /// existing document and skips unchanged leaves, so a patch carrying the
    /// whole `[inject]` table would still produce a one-line diff -- but it
    /// would also overwrite `trailing_space` and `keystroke_delay_ms` with
    /// whatever this process happened to think they were, which is a
    /// different and much worse thing than setting a backend.
    #[test]
    fn the_backend_patch_names_only_the_one_key_it_changes() {
        let patch = backend_patch("ydotool");
        assert_eq!(patch, serde_json::json!({ "inject": { "backend": "ydotool" } }));
        assert_eq!(patch["inject"].as_object().unwrap().len(), 1);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yappr wizard::the_backend_patch`
Expected: FAIL to compile — `cannot find function backend_patch`.

- [ ] **Step 3: Write the implementation**

Add to `src-tauri/src/wizard.rs`:

```rust
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
/// `set_backend` is `Some` only on a genuine first run (the frontend decides,
/// from `wizard_state`'s `start_step`, and passes it explicitly rather than
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

    let marker = yappr_core::paths::wizard_marker();
    write_marker(&marker)
        .map_err(|e| format!("Einrichtungsstatus konnte nicht gespeichert werden: {e}"))?;

    if let Some(w) = tauri::Manager::get_webview_window(&app, crate::SETTINGS_LABEL) {
        let _ = w.hide();
    }
    Ok(())
}
```

`settings_cmds::set_config` and `SETTINGS_LABEL` must be reachable from here.
Check their current visibility and widen to `pub(crate)` if needed — do not
duplicate either.

Register the command in `lib.rs`'s `invoke_handler`:

```rust
            wizard::wizard_state,
            wizard::wizard_finish
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr wizard::`
Expected: PASS, 11 tests.

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/wizard.rs src-tauri/src/lib.rs
git commit -m "feat(wizard): add the wizard_finish command"
```

---

### Task 7: The `--wizard` flag

**Files:**
- Modify: `src-tauri/src/cli.rs` (`USAGE` line 12, `route` line 40, tests line 76)

**Interfaces:**
- Consumes: `Request::ShowWizard` (Task 3).
- Produces: `route(&["--wizard"]) == Route::Send(Request::ShowWizard)`.

- [ ] **Step 1: Write the failing test**

In `src-tauri/src/cli.rs`, add to `fn the_lifecycle_and_inspection_flags_route`:

```rust
        assert_eq!(route(&["--wizard"]), Route::Send(Request::ShowWizard));
```

and add a test pinning that it stays distinct from `--settings`:

```rust
    /// Two flags, two destinations: `--settings` lands in the settings form,
    /// `--wizard` at the top of the setup flow. Collapsing them would make
    /// the tray's Einrichtung item indistinguishable from Einstellungen.
    #[test]
    fn the_wizard_flag_is_not_an_alias_for_settings() {
        assert_ne!(route(&["--wizard"]), route(&["--settings"]));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yappr cli::`
Expected: FAIL — `--wizard` routes to `Route::Usage`, not `Route::Send(...)`.

- [ ] **Step 3: Write the implementation**

Add the route, directly after the `--settings` row:

```rust
        ["--wizard"] => Route::Send(Request::ShowWizard),
```

and update `USAGE`:

```rust
pub const USAGE: &str = "\
usage: yappr                  start the app (tray, no window)
       yappr --toggle         start or stop dictating
       yappr --cancel         discard the current utterance
       yappr --settings       show the settings window
       yappr --wizard         show the setup wizard
       yappr --quit           shut everything down
       yappr --status|--debug|--subscribe|--reload
       yappr --bench|--print-shortcuts|--purge-logs|--update-lock
       yappr --replay <path>";
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr cli::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/cli.rs
git commit -m "feat(cli): add --wizard"
```

---

### Task 8: Showing the wizard — window helper, sink, tray, capability

**Files:**
- Modify: `src-tauri/src/lib.rs` (`show_settings_window` around line 98; `TauriSink` around line 126)
- Modify: `src-tauri/src/tray.rs` (`menu()` around line 333)
- Create: `src-tauri/capabilities/settings.json`

**Interfaces:**
- Consumes: `EventSink::show_wizard` (Task 3).
- Produces: `crate::show_wizard_window(app: &tauri::AppHandle)`, which shows
  the settings window and emits the `show-wizard` event to it. Task 9's
  frontend listens for that event name.

- [ ] **Step 1: Add the window helper and the sink override**

In `src-tauri/src/lib.rs`, directly after `show_settings_window`:

```rust
/// Shows the settings window with the first-run wizard on top of it.
///
/// Shared by `TauriSink::show_wizard` (`Request::ShowWizard`, the CLI's
/// `--wizard` flag's path) and `tray.rs`'s Einrichtung menu item, for the
/// same reason `show_settings_window` is shared: one definition of "show the
/// wizard", not two that can drift.
///
/// The event is what puts the window into wizard mode. The window is shown
/// first so that a webview which has not mounted yet still ends up correct --
/// it asks `wizard_state()` on mount regardless, and an already-open window
/// that missed nothing gets the event.
pub(crate) fn show_wizard_window(app: &tauri::AppHandle) {
    show_settings_window(app);
    if let Some(w) = app.get_webview_window(SETTINGS_LABEL) {
        let _ = w.emit("show-wizard", ());
    }
}
```

`w.emit` needs `tauri::Emitter` in scope; add the import if it is not already
there (`provision.rs` already imports it, `lib.rs` may not).

In `impl EventSink for TauriSink`, directly after `show_settings`:

```rust
    /// `Request::ShowWizard` (the CLI's `--wizard` flag, and the tray).
    fn show_wizard(&self) {
        show_wizard_window(&self.app);
    }
```

- [ ] **Step 2: Add the tray menu item**

In `src-tauri/src/tray.rs`'s `menu()`, insert directly after the
`Einstellungen` item and before the `Diktat pausieren` checkmark:

```rust
            StandardItem {
                label: "Einrichtung…".into(),
                activate: Box::new(|this: &mut Self| crate::show_wizard_window(&this.app)),
                ..Default::default()
            }
            .into(),
```

Calling `show_wizard_window` directly rather than dispatching
`Request::ShowWizard` is deliberate and matches the existing Einstellungen
item — see this module's doc comment on why a menu activation must never
block on the accept loop.

- [ ] **Step 3: Give the settings window event permissions**

Create `src-tauri/capabilities/settings.json`:

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "settings",
  "description": "Capability for the settings window, which hosts the setup wizard",
  "windows": ["settings"],
  "permissions": ["core:default", "opener:default"]
}
```

The existing `default.json` scopes its permissions to `"windows": ["overlay"]`,
so the settings window is covered by no capability at all. The window works
today because app commands declared in `generate_handler!` need no permission
— but `listen`/`emit` are core-plugin commands and do. Task 9 adds a second
`listen` there, so the window gets its own capability rather than relying on
that distinction holding.

- [ ] **Step 4: Verify the build and the existing tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 5: Verify the tray item exists at runtime**

The tray's menu is built by `menu()`, which has no test coverage for item
*labels*. Confirm by inspection that the new item sits between Einstellungen
and Diktat pausieren, and that `status_label` is still the first, disabled row.

Run: `cargo build --release -p yappr --features custom-protocol`
Expected: builds.

Do **not** start the app to check the tray unless the user asks — a second
instance would contend for `$XDG_RUNTIME_DIR/yappr.sock`.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/tray.rs src-tauri/capabilities/settings.json
git commit -m "feat(wizard): reach the wizard from the tray, --wizard and the sink"
```

---

### Task 9: Wizard shell — Willkommen and Fertig

**Files:**
- Create: `src/settings/wizard.tsx`
- Modify: `src/Settings.tsx`
- Modify: `src/Settings.css`
- Create (temporary, deleted in step 6): `wizard-preview.html` at the project root

**Interfaces:**
- Consumes: `wizard_state`, `wizard_finish` (Tasks 5–6); the `show-wizard` event (Task 8); `Icon` from `./icons`.
- Produces: `export function Wizard({ state, onFinish, onOpenSettings })`, and
  the exported types `WizardState`, `WizardShortcut`, `WizardShortcutBinding`,
  `Step`. Tasks 10 and 11 add steps inside this component.

- [ ] **Step 1: Write the wizard shell**

Create `src/settings/wizard.tsx`:

```tsx
import { useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { Icon } from "./icons";

/// The first-run setup wizard (spec §1). It takes over the settings window
/// rather than opening one of its own: the window already exists, already has
/// every command wired, and already comes up on a fresh install.
///
/// Steps live here rather than in `Settings.tsx` because that file is a
/// 900-line shell already, and because the wizard shares almost nothing with
/// it — no sidebar, no autosave, no config rows.

/// The house spring, in the two shapes this flow uses. `bounce: 0` throughout:
/// a step change is a discrete navigation with no momentum behind it, and the
/// codebase reserves overshoot for motion that had some.
const STEP = { type: "spring", bounce: 0, duration: 0.36 } as const;
const FADE = { type: "spring", bounce: 0, duration: 0.24 } as const;

export const STEPS = ["welcome", "models", "shortcuts", "done"] as const;
export type Step = (typeof STEPS)[number];

/// `yappr_core::desktop::ShortcutBinding`, unchanged across the wire.
export type WizardShortcutBinding = { name: string; command: string; keys: string };

/// `yappr_core::desktop::ShortcutInstructions`.
export type WizardShortcut = {
  kind: "hypr-lua" | "hypr-conf" | "gnome" | "generic";
  target: string | null;
  snippet: string;
  bindings: WizardShortcutBinding[];
};

/// `wizard::build_wizard_state`'s response shape.
export type WizardState = {
  should_open: boolean;
  start_step: Step;
  desktop: "hyprland" | "gnome" | "other" | "unknown";
  desktop_name: string;
  recommended_backend: string;
  current_backend: string;
  backend_prereqs: string[];
  shortcut: WizardShortcut;
};

/// The step rail. Not clickable: it reports where you are, it is not a way to
/// jump ahead of a download.
function Dots({ index }: { index: number }) {
  return (
    <div className="wizard-dots" aria-hidden="true">
      {STEPS.map((s, i) => (
        <span key={s} className={`wizard-dot${i === index ? " is-current" : ""}${i < index ? " is-done" : ""}`} />
      ))}
    </div>
  );
}

export function Wizard({
  state,
  onFinish,
  onOpenSettings,
}: {
  state: WizardState;
  /** Passes the backend to write, or `null` to leave `config.toml` alone. */
  onFinish: (setBackend: string | null) => void;
  onOpenSettings: () => void;
}) {
  const [step, setStep] = useState<Step>(state.start_step);
  const index = STEPS.indexOf(step);

  // Only a genuine first run writes the injection backend — see
  // `wizard_finish`'s doc comment. `start_step` is the frontend's evidence
  // for that, captured at mount so a marker written by this very click
  // cannot change the answer.
  const firstRun = state.start_step === "welcome";

  return (
    <div className="wizard">
      <Dots index={index} />
      <AnimatePresence mode="wait">
        <motion.div
          key={step}
          className="wizard-step"
          initial={{ opacity: 0, x: 24 }}
          animate={{ opacity: 1, x: 0 }}
          exit={{ opacity: 0, x: -24 }}
          transition={STEP}
        >
          {step === "welcome" && (
            <section className="wizard-body">
              <img className="wizard-mark" src="/yappr.png" alt="" aria-hidden="true" />
              <h1>Willkommen bei yappr</h1>
              <p className="wizard-lead">
                Diktieren in jedes Fenster — vollständig lokal, ohne Cloud. In den nächsten
                drei Schritten lädst du die Sprachmodelle, richtest deinen Kurzbefehl ein
                und bist fertig.
              </p>
              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("models")}>
                  Los geht’s
                </button>
              </div>
            </section>
          )}

          {step === "models" && (
            <section className="wizard-body">
              {/* Task 10 fills this in. */}
              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("shortcuts")}>
                  Weiter
                </button>
              </div>
            </section>
          )}

          {step === "shortcuts" && (
            <section className="wizard-body">
              {/* Task 11 fills this in. */}
              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("done")}>
                  Weiter
                </button>
              </div>
            </section>
          )}

          {step === "done" && (
            <section className="wizard-body">
              <motion.div
                className="wizard-tick"
                initial={{ opacity: 0, scale: 0.8 }}
                animate={{ opacity: 1, scale: 1 }}
                transition={FADE}
              >
                <Icon name="check" className="icon" />
              </motion.div>
              <h1>Alles eingerichtet</h1>
              <p className="wizard-lead">
                Drücke <kbd>Super</kbd>+<kbd>D</kbd>, sprich, und drücke noch einmal.
                yappr läuft weiter im Systemtray.
              </p>
              <div className="wizard-actions">
                <button
                  type="button"
                  className="add"
                  onClick={() => onFinish(firstRun ? state.recommended_backend : null)}
                >
                  Fertig
                </button>
                <button type="button" className="ghost" onClick={onOpenSettings}>
                  Einstellungen öffnen
                </button>
              </div>
            </section>
          )}
        </motion.div>
      </AnimatePresence>
    </div>
  );
}
```

- [ ] **Step 2: Wire it into `Settings.tsx`**

Add the import beside the existing `./settings/*` imports:

```tsx
import { Wizard, WizardState } from "./settings/wizard";
```

Add state, next to the existing `setupStatus` block:

```tsx
  // The wizard takes over this whole window when it is active (spec §8).
  // `wizardState` is null until `wizard_state()` answers, so nothing flashes
  // on screen before the very first answer arrives.
  const [wizardState, setWizardState] = useState<WizardState | null>(null);
  const [wizardActive, setWizardActive] = useState(false);
```

Add the mount effect and the event listener, after the existing
`setup-progress` effect:

```tsx
  // The window decides its own mode: `should_open` is computed from the same
  // marker-plus-readiness rule `lib.rs`'s startup thread uses, so there is no
  // ordering hazard between that thread and this webview's first paint.
  useEffect(() => {
    void (async () => {
      try {
        const res = (await invoke("wizard_state")) as WizardState;
        setWizardState(res);
        setWizardActive(res.should_open);
      } catch (e) {
        // A wizard that cannot describe itself must not replace the settings
        // form with a blank screen — the settings window still works, and a
        // user who reached it from the tray gets what they asked for.
        console.error("wizard_state failed", e);
      }
    })();
  }, []);

  // `yappr --wizard` and the tray's Einrichtung item. The state is re-read
  // rather than reused: the desktop or the backend may have changed since
  // this window mounted.
  useEffect(() => {
    const unlistenPromise = listen("show-wizard", () => {
      void (async () => {
        try {
          const res = (await invoke("wizard_state")) as WizardState;
          setWizardState(res);
        } catch (e) {
          console.error("wizard_state failed", e);
        }
        setWizardActive(true);
      })();
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);
```

Add the branch at the top of the returned JSX, inside `<MotionConfig>` and
before `<main className="shell">`:

```tsx
  if (wizardActive && wizardState) {
    return (
      <MotionConfig reducedMotion="user">
        <main className="shell shell--wizard">
          <Wizard
            state={wizardState}
            onFinish={(setBackend) => {
              // Errors are not swallowed silently, but they also must not
              // trap the user in the wizard: the marker is a convenience,
              // and a wizard that will not close is worse than one that
              // reappears next launch.
              invoke("wizard_finish", { setBackend }).catch((e) => {
                console.error("wizard_finish failed", e);
              });
              setWizardActive(false);
            }}
            onOpenSettings={() => setWizardActive(false)}
          />
        </main>
      </MotionConfig>
    );
  }
```

Note: Tauri converts the Rust parameter `set_backend` to the JS key
`setBackend`; pass it camelCase.

- [ ] **Step 3: Add the CSS**

Append to `src/Settings.css`, matching the existing frosted-tile idiom (reuse
the file's own custom properties rather than hard-coding colours):

```css
/* ---- Setup wizard ------------------------------------------------------ */

/* The wizard replaces the sidebar-and-pane layout entirely, so the shell
   stops being a two-column grid for as long as it is up. */
.shell--wizard {
  display: block;
  overflow: auto;
}

.wizard {
  max-width: 44rem;
  margin: 0 auto;
  padding: 3rem 2rem 2.5rem;
  display: flex;
  flex-direction: column;
  gap: 2rem;
  min-height: 100%;
}

.wizard-dots {
  display: flex;
  gap: 0.5rem;
  justify-content: center;
}

.wizard-dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: currentColor;
  opacity: 0.22;
  transition: opacity 0.2s ease, transform 0.2s ease;
}

.wizard-dot.is-done { opacity: 0.45; }
.wizard-dot.is-current { opacity: 1; transform: scale(1.25); }

.wizard-step { flex: 1; }

.wizard-body {
  display: flex;
  flex-direction: column;
  gap: 1rem;
}

.wizard-body h1 {
  margin: 0;
  font-size: 1.6rem;
  font-weight: 650;
  letter-spacing: -0.01em;
}

.wizard-lead {
  margin: 0;
  max-width: 34rem;
  line-height: 1.5;
  opacity: 0.75;
}

.wizard-mark { width: 4rem; height: 4rem; }

.wizard-tick { width: 2.5rem; height: 2.5rem; }

.wizard-actions {
  display: flex;
  align-items: center;
  gap: 0.75rem;
  margin-top: auto;
  padding-top: 1.5rem;
}

.wizard kbd {
  font: inherit;
  font-size: 0.85em;
  padding: 0.1em 0.4em;
  border-radius: 0.35em;
  border: 1px solid currentColor;
  opacity: 0.8;
}
```

- [ ] **Step 4: Typecheck and build**

Run: `bun run build`
Expected: `tsc` clean, both entries emitted to `dist/`.

- [ ] **Step 5: Verify the flow in a browser**

`hyprctl focuswindow` does not work on this machine's Lua config, so the
settings window cannot be driven from a real app run. Preview it in a plain
browser instead, by stubbing Tauri's IPC.

Create `wizard-preview.html` **at the project root** (Vite serves root HTML
files in dev; it will not work from the scratchpad):

```html
<!doctype html>
<html lang="de">
  <head>
    <meta charset="UTF-8" />
    <title>wizard preview</title>
  </head>
  <body>
    <div id="root"></div>
    <script>
      // Enough of Tauri's IPC for the settings window to mount in a plain
      // browser. `listen` is itself an `invoke` of `plugin:event|listen`, so
      // it resolves to a handler id that nothing ever fires.
      let nextId = 1;
      window.__TAURI_INTERNALS__ = {
        transformCallback: (cb) => {
          const id = nextId++;
          window[`_${id}`] = cb;
          return id;
        },
        invoke: async (cmd) => {
          switch (cmd) {
            case "plugin:event|listen":
              return nextId++;
            case "plugin:event|unlisten":
              return null;
            case "get_config":
              return {
                config: { inject: { backend: "wtype", trailing_space: true, keystroke_delay_ms: 2 } },
                defaults: { inject: { backend: "wtype", trailing_space: true, keystroke_delay_ms: 2 } },
                config_path: "~/.config/yappr/config.toml",
              };
            case "list_input_devices":
              return { devices: [] };
            case "autostart_status":
              return { enabled: false };
            case "setup_status":
              return {
                ready: false,
                missing_prerequisites: ["ggml-cpu"],
                missing_models: [
                  { name: "parakeet", display: "Parakeet TDT 0.6b v3 (int8)" },
                  { name: "s1-mini", display: "S1-mini by Superwhisper" },
                ],
              };
            case "wizard_state":
              return {
                should_open: true,
                start_step: "welcome",
                desktop: "hyprland",
                desktop_name: "Hyprland",
                recommended_backend: "wtype",
                current_backend: "wtype",
                backend_prereqs: [],
                shortcut: {
                  kind: "hypr-lua",
                  target: "~/.config/hypr/bindings.lua",
                  snippet: 'o.bind("SUPER", "D", "yappr --toggle")\no.bind("SUPER ALT", "D", "yappr --cancel")',
                  bindings: [],
                },
              };
            default:
              return null;
          }
        },
      };
    </script>
    <script type="module" src="/src/settings-main.tsx"></script>
  </body>
</html>
```

Run: `bun run dev` and open `http://localhost:1420/wizard-preview.html`.

Check: the Willkommen step is on screen, the first dot is filled, `Los geht’s`
advances through the (still empty) middle steps to Fertig, and the tick
animates in. Then edit the stub's `wizard_state` to return
`start_step: "models"` and reload: it must open on the second step with the
second dot filled.

Keep this file — Tasks 10 and 11 use it — and delete it in Task 12.

- [ ] **Step 6: Commit**

```bash
git add src/settings/wizard.tsx src/Settings.tsx src/Settings.css
git commit -m "feat(wizard): add the wizard shell with its welcome and done steps"
```

Do **not** commit `wizard-preview.html`.

---

### Task 10: The Modelle step, and deleting the Setup pane

**Files:**
- Modify: `src/settings/wizard.tsx`
- Modify: `src/Settings.tsx` (delete `SetupPane`, its state, its render branch)
- Modify: `src/settings/schema.ts` (delete `SETUP_CATEGORY`)

**Interfaces:**
- Consumes: `setup_status`, `run_setup`, the `setup-progress` event.
- Produces: nothing new for later tasks.

- [ ] **Step 1: Move the setup state into the wizard**

In `src/settings/wizard.tsx`, add the types and the state the deleted pane
used. Add these imports: `useCallback`, `useEffect` from `react`, `invoke`
from `@tauri-apps/api/core`, `listen` from `@tauri-apps/api/event`.

```tsx
/// `provision::MissingModel`, unchanged across the wire.
type MissingModel = { name: string; display: string };

/// `provision::setup_status`'s response shape.
type SetupStatus = {
  ready: boolean;
  missing_prerequisites: string[];
  missing_models: MissingModel[];
};

/// One artifact's live download progress, keyed by `MissingModel.name` — kept
/// only for artifacts a `"setup-progress"` event has actually mentioned, so a
/// model nothing has reported on yet renders as "fehlt" rather than a bar
/// stuck at 0%.
type DownloadProgress = { display: string; done: number; total: number | null };

/// `provision::SetupProgress`, unchanged across the wire.
type SetupProgressEvent =
  | { kind: "downloading"; name: string; display: string; done: number; total: number | null }
  | { kind: "finished" }
  | { kind: "failed"; message: string };

function downloadStatusText(progress: DownloadProgress | undefined, installing: boolean): string {
  if (!progress) return installing ? "wartet…" : "fehlt";
  if (progress.total !== null) {
    const pct = Math.min(100, Math.round((progress.done / progress.total) * 100));
    return `${pct} %`;
  }
  return `${Math.round(progress.done / (1 << 20))} MB`;
}

/// Everything the Modelle step needs, owned by `Wizard` rather than by the
/// step itself: the `setup-progress` listener has to outlive the step, so a
/// user who walks on to the shortcut step mid-download does not lose the
/// running total — the same reason it used to be scoped to the whole window.
function useSetup() {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);
  const [downloads, setDownloads] = useState<Record<string, DownloadProgress>>({});

  /// Fails *closed*: `ready: false` with the error carried separately, never
  /// a silent `ready: true`. A fresh install where `setup_status` itself is
  /// broken is exactly when this step needs to be seen.
  const check = useCallback(async () => {
    try {
      const res = (await invoke("setup_status")) as SetupStatus;
      setStatus(res);
      setCheckError(null);
    } catch (e) {
      setStatus({ ready: false, missing_prerequisites: [], missing_models: [] });
      setCheckError(String(e));
    }
  }, []);

  useEffect(() => {
    void check();
  }, [check]);

  useEffect(() => {
    const unlistenPromise = listen<SetupProgressEvent>("setup-progress", (event) => {
      const payload = event.payload;
      if (payload.kind === "downloading") {
        setDownloads((prev) => ({
          ...prev,
          [payload.name]: { display: payload.display, done: payload.done, total: payload.total },
        }));
      } else if (payload.kind === "finished") {
        setInstalling(false);
        setDownloads({});
        void check();
      } else if (payload.kind === "failed") {
        setInstalling(false);
        setInstallError(payload.message);
        // An artifact failing partway through does not undo the ones already
        // promoted before it (`download_all`), so what is still missing may
        // be a shorter list than it was.
        void check();
      }
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, [check]);

  const install = useCallback(() => {
    setInstalling(true);
    setInstallError(null);
    setDownloads({});
    invoke("run_setup").catch((e) => {
      // A failure inside `download_all` also arrives as a "failed" event.
      // The reentrancy guard rejects *before* `download_all` runs, though,
      // so no event fires for that one at all — without this the button
      // would stay on "Installation läuft…" forever. The functional update
      // keeps a more specific error the event already reported.
      setInstalling(false);
      setInstallError((prev) => prev ?? String(e));
    });
  }, []);

  return { status, checkError, check, installing, installError, downloads, install };
}
```

- [ ] **Step 2: Render the step**

Call `const setup = useSetup();` inside `Wizard`, and replace the `models`
step's placeholder body with:

```tsx
          {step === "models" && (
            <section className="wizard-body">
              <h1>Modelle laden</h1>
              <p className="wizard-lead">
                Spracherkennung, Sprachpausen-Erkennung und Nachbearbeitung laufen
                vollständig auf diesem Rechner. Dafür braucht yappr einmalig etwa
                1,1 GB an Modellen.
              </p>

              {setup.checkError && (
                <div className="banner error">
                  <Icon name="warn" className="icon-sm" />
                  <span>Setup-Status konnte nicht ermittelt werden: {setup.checkError}</span>
                  <button type="button" className="ghost" onClick={() => void setup.check()}>
                    Erneut versuchen
                  </button>
                </div>
              )}

              {setup.status && !setup.checkError && (
                <>
                  {setup.status.missing_prerequisites.length > 0 && (
                    <div className="card">
                      {setup.status.missing_prerequisites.map((pkg) => (
                        <div className="setup-row missing" key={pkg}>
                          <Icon name="warn" className="icon-sm" />
                          <span>{pkg} fehlt.</span>
                        </div>
                      ))}
                      <p className="setup-command">
                        Installieren mit:{" "}
                        <code>sudo pacman -S {setup.status.missing_prerequisites.join(" ")}</code>
                      </p>
                    </div>
                  )}

                  <div className="card">
                    {setup.status.missing_models.length === 0 ? (
                      <div className="setup-row ok">
                        <Icon name="check" className="icon-sm" />
                        <span>Alle Modelle sind vorhanden.</span>
                      </div>
                    ) : (
                      setup.status.missing_models.map((m) => {
                        const progress = setup.downloads[m.name];
                        const done =
                          !!progress && progress.total !== null && progress.done >= progress.total;
                        return (
                          <div className={`setup-row${done ? " ok" : " missing"}`} key={m.name}>
                            <Icon name={done ? "check" : "warn"} className="icon-sm" />
                            <div className="setup-row__body">
                              <div className="setup-row__head">
                                <span>{m.display}</span>
                                <span className="setup-row__status">
                                  {downloadStatusText(progress, setup.installing)}
                                </span>
                              </div>
                              {progress && !done && (
                                <div className="progressbar">
                                  <div
                                    className="progressbar__fill"
                                    style={{
                                      width:
                                        progress.total !== null
                                          ? `${Math.min(100, (progress.done / progress.total) * 100)}%`
                                          : "35%",
                                    }}
                                  />
                                </div>
                              )}
                            </div>
                          </div>
                        );
                      })
                    )}
                  </div>
                </>
              )}

              {setup.installError && (
                <div className="banner error">
                  <Icon name="warn" className="icon-sm" />
                  <span>{setup.installError}</span>
                </div>
              )}

              <div className="wizard-actions">
                {/* The download never blocks the wizard: once it is running,
                    the only button is Weiter, and the transfer continues
                    while the user reads the next two steps (spec §1). */}
                {!setup.installing && setup.status && setup.status.missing_models.length > 0 && (
                  <button type="button" className="add" onClick={setup.install}>
                    Jetzt laden
                  </button>
                )}
                <button
                  type="button"
                  className={setup.installing || setup.status?.missing_models.length === 0 ? "add" : "ghost"}
                  onClick={() => setStep("shortcuts")}
                >
                  {setup.installing || setup.status?.missing_models.length === 0 ? "Weiter" : "Später"}
                </button>
              </div>
            </section>
          )}
```

- [ ] **Step 3: Delete the Setup pane**

In `src/Settings.tsx`, delete:
- the whole `function SetupPane(...)` and the `downloadStatusText` helper above it,
- the `MissingModel` / `SetupStatus` / `DownloadProgress` / `SetupProgressEvent` type aliases,
- the `setupStatus`, `setupCheckError`, `installing`, `installError`, `downloads` state,
- the `checkSetup` callback, its mount effect, the `setup-progress` effect and `startInstall`,
- the `<SetupPane ... />` block in the render and the comment above it,
- `SETUP_CATEGORY` from the `./settings/schema` import list.

Simplify `categories` to:

```tsx
  const categories = useMemo(() => (config ? categorize(Object.keys(config)) : []), [config]);
```

In `src/settings/schema.ts`, delete the `SETUP_CATEGORY` const and its doc
comment.

**Do not touch `src/Settings.css`.** The wizard's model step reuses
`.setup-row`, `.setup-row__body`, `.setup-row__head`, `.setup-row__status`,
`.setup-command`, `.progressbar`, `.progressbar__fill`, `.card` and
`.banner.error` verbatim. The markup moved files; the styles did not, and
deleting them because "the Setup pane is gone" would strip the step this task
just built.

- [ ] **Step 4: Typecheck**

Run: `bun run build`
Expected: `tsc` clean. Any "declared but never read" error names something the
deletion missed — remove it rather than silencing it.

Run: `grep -rn "SETUP_CATEGORY\|SetupPane" src/`
Expected: no output.

- [ ] **Step 5: Verify in the browser**

Run `bun run dev`, open `http://localhost:1420/wizard-preview.html`, advance to
step 2. Check: two missing models listed, the `ggml-cpu` prerequisite shown
with its pacman line, `Jetzt laden` and `Später` both present. Then edit the
stub's `setup_status` to return `ready: true, missing_models: []` and reload:
the step must show "Alle Modelle sind vorhanden" with a single `Weiter`.

Then set `wizard_state`'s `should_open` to `false` and reload: the normal
settings window must appear, with **no** Einrichtung entry in the sidebar.

- [ ] **Step 6: Commit**

```bash
git add src/settings/wizard.tsx src/Settings.tsx src/settings/schema.ts
git commit -m "feat(wizard): move model provisioning into the wizard, delete the Setup pane"
```

---

### Task 11: The Desktop & Kurzbefehle step

**Files:**
- Modify: `src/settings/wizard.tsx`
- Modify: `src/Settings.css`

**Interfaces:**
- Consumes: `WizardState.shortcut`, `.desktop`, `.desktop_name`, `.recommended_backend`, `.current_backend`, `.backend_prereqs`.
- Produces: nothing for later tasks.

- [ ] **Step 1: Add a copy button**

Add to `src/settings/wizard.tsx`:

```tsx
/// Copies `text` and says so for a moment. `navigator.clipboard` is available
/// in the WebKitGTK webview; the fallback is the text itself, which is
/// already on screen and selectable, so a failure costs the user a keystroke
/// rather than the step.
function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="ghost"
      onClick={() => {
        void navigator.clipboard
          .writeText(text)
          .then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1600);
          })
          .catch(() => setCopied(false));
      }}
    >
      <Icon name={copied ? "check" : "copy"} className="icon-sm" />
      <span>{copied ? "Kopiert" : "Kopieren"}</span>
    </button>
  );
}
```

This step uses two glyphs the settings window may not have yet. Check
`src/settings/icons.tsx` for `copy` and `arrow`, and add whichever is missing,
drawn at the same 24×24 viewBox and stroke weight as the file's existing
icons — `copy` as two offset rounded rectangles, `arrow` as a short
right-pointing chevron. Run `grep -n '"copy"\|"arrow"' src/settings/icons.tsx`
first; `Icon` renders nothing for a name it does not know, so a missing glyph
fails silently rather than loudly.

- [ ] **Step 2: Render the step**

Replace the `shortcuts` step's placeholder body:

```tsx
          {step === "shortcuts" && (
            <section className="wizard-body">
              <h1>Kurzbefehl einrichten</h1>
              <p className="wizard-lead">
                Deine Arbeitsumgebung: <strong>{state.desktop_name}</strong>.{" "}
                {state.desktop === "other" || state.desktop === "unknown"
                  ? "Offiziell unterstützt sind Hyprland und GNOME — die beiden Befehle unten funktionieren trotzdem, du musst sie nur selbst auf eine Taste legen."
                  : "Wayland kennt keinen globalen Tastatur-Grab, deshalb legt yappr den Kurzbefehl nicht selbst an — du fügst ihn dort ein, wo dein Desktop ihn erwartet."}
              </p>

              {state.shortcut.target && (
                <p className="wizard-target">
                  <Icon name="arrow" className="icon-sm" />
                  <code>{state.shortcut.target}</code>
                </p>
              )}

              {state.shortcut.bindings.length > 0 && (
                <div className="card">
                  {state.shortcut.bindings.map((b) => (
                    <div className="setup-row" key={b.command}>
                      <div className="setup-row__body">
                        <div className="setup-row__head">
                          <span>{b.name}</span>
                          <span className="setup-row__status">
                            <kbd>{b.keys}</kbd>
                          </span>
                        </div>
                        <code className="wizard-command">{b.command}</code>
                      </div>
                    </div>
                  ))}
                </div>
              )}

              <details className="wizard-snippet" open={state.shortcut.bindings.length === 0}>
                <summary>
                  {state.desktop === "gnome" ? "oder per Terminal" : "Diese Zeilen einfügen"}
                </summary>
                <pre className="wizard-pre">{state.shortcut.snippet}</pre>
                <CopyButton text={state.shortcut.snippet} />
              </details>

              <p className="note">
                Texteingabe: <code>{state.recommended_backend}</code>
                {state.recommended_backend === "ydotool"
                  ? " — GNOME (Mutter) unterstützt das Protokoll nicht, über das wtype tippt."
                  : " — braucht keine weitere Einrichtung."}
              </p>

              {state.backend_prereqs.length > 0 && (
                <div className="card">
                  <p className="setup-command">
                    Dafür noch nötig: <code>sudo pacman -S ydotool</code> und{" "}
                    <code>systemctl --user enable --now ydotoold</code>. Tippt yappr danach
                    nichts, fehlt meist der Zugriff auf <code>/dev/uinput</code>.
                  </p>
                </div>
              )}

              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("done")}>
                  Weiter
                </button>
              </div>
            </section>
          )}
```

The step never applies anything — no `gsettings`, no file write. That is the
standing rule (spec §4), and on GNOME specifically the reason is in
`desktop.rs`'s `GNOME_GSETTINGS_SNIPPET` doc comment.

- [ ] **Step 3: Add the CSS**

Append to `src/Settings.css`:

```css
.wizard-target {
  display: flex;
  align-items: center;
  gap: 0.4rem;
  margin: 0;
  opacity: 0.75;
}

.wizard-command {
  font-size: 0.85em;
  opacity: 0.8;
}

.wizard-snippet summary {
  cursor: pointer;
  opacity: 0.75;
  user-select: none;
}

.wizard-pre {
  margin: 0.75rem 0;
  padding: 0.9rem 1rem;
  border-radius: 0.6rem;
  background: rgb(0 0 0 / 0.22);
  overflow-x: auto;
  font-size: 0.8rem;
  line-height: 1.5;
  white-space: pre;
}
```

- [ ] **Step 4: Typecheck**

Run: `bun run build`
Expected: `tsc` clean.

- [ ] **Step 5: Verify all four desktop shapes in the browser**

With `bun run dev` and `wizard-preview.html` open, edit the stub's
`wizard_state` return value and reload for each of these, checking step 3 each
time:

1. **Hyprland Lua** (as written): the `bindings.lua` target, the snippet open
   by default, no bindings cards, `wtype` named as needing no setup.
2. **GNOME** — `desktop: "gnome"`, `desktop_name: "GNOME"`,
   `recommended_backend: "ydotool"`, `backend_prereqs: ["ydotool","ydotoold"]`,
   and `shortcut: { kind: "gnome", target: "Einstellungen → Tastatur → …",
   snippet: "BASE=/org/gnome/…", bindings: [{name:"yappr: Diktat starten/stoppen",
   command:"yappr --toggle", keys:"Super+D"},{name:"yappr: Diktat abbrechen",
   command:"yappr --cancel", keys:"Super+Alt+D"}] }`. Expect: two binding cards
   with their keys, the terminal snippet collapsed behind the disclosure, and
   the `ydotoold` prerequisite card.
3. **Unsupported** — `desktop: "other"`, `desktop_name: "KDE"`,
   `shortcut.kind: "generic"`, `shortcut.target: null`. Expect: the
   "offiziell unterstützt" wording, no target line, the two commands, and no
   prerequisite card.
4. **Copy button** — press it on any shape; it must flip to "Kopiert" and back.

- [ ] **Step 6: Commit**

```bash
git add src/settings/wizard.tsx src/Settings.css src/settings/icons.tsx
git commit -m "feat(wizard): add the desktop and shortcut step"
```

---

### Task 12: Documentation and final verification

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Modify: `HANDOVER.md`
- Delete: `wizard-preview.html`

- [ ] **Step 1: Delete the preview stub**

```bash
rm -f wizard-preview.html
git status --porcelain
```

Expected: `wizard-preview.html` does not appear (it was never committed).

- [ ] **Step 2: Update `CLAUDE.md`**

- In **Commands**, add `yappr --wizard` to the runtime-control list beside `--settings`.
- In **Architecture**, under `src/`, replace the Setup-pane sentence with the wizard: `src/settings/wizard.tsx` is the first-run flow, it takes over the settings window, and `provision.rs`'s commands are unchanged beneath it.
- In **Architecture**, note `crates/yappr-core/src/desktop.rs` as the desktop-detection module beside `hypr.rs`.
- Add an invariant, numbered 13:

```markdown
13. **The wizard never writes desktop config, on either desktop.** Hyprland is
    the standing rule (a bad window rule breaks the desktop). GNOME is the
    same rule for a different reason: `gsettings set ... custom-keybindings`
    *replaces* the list, so the obvious one-liner destroys every custom
    shortcut the user already had. `desktop.rs`'s snippet reads the current
    value and appends, and the wizard shows it rather than running it. The
    first-run gate is `~/.local/state/yappr/wizard-done` plus
    `setup_status().ready` — a state file, not a config key, because a config
    key would have to join a `deny_unknown_fields` struct and then appear in
    the settings GUI as a setting nobody should touch.
```

- Fix the inaccuracy found while designing this: invariant 2 claims *every*
  window carries `focus: false`/`focusable: false`. The settings window is
  `focus: true`/`focusable: true` in `tauri.conf.json` — it has to be, or its
  form could not take a keystroke. Reword to say the **overlay** always
  carries them.

- [ ] **Step 3: Update `README.md`**

Replace the first-run instructions: starting `yappr` on a machine with no
models opens the setup wizard, which downloads them (~1,1 GB), names the
shortcut lines for the detected desktop, and sets the injection backend. Add
`yappr --wizard` to the flag list. Note that the wizard reopens by itself if a
model later goes missing.

- [ ] **Step 4: Update `HANDOVER.md`**

Add a section recording:
- what the wizard is and that it replaced the Setup pane;
- that the **Hyprland path is verified** on this machine (detection, Lua target file, snippet) and the **GNOME path is not** — no GNOME session exists here, so `gsettings` snippet, `ydotool`/`ydotoold` prerequisites and GNOME detection are written-not-executed, in the same sense the audio path is;
- that `hyprctl binds -j` cannot verify a Lua-configured bind (all binds report `dispatcher: "__lua"` with an opaque numeric `arg`), which is why the wizard trusts the user rather than checking;
- that nothing in this work started the app, bound the socket, opened the microphone or edited anything under `~/.config/hypr/`.

- [ ] **Step 5: Run the full gate**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
bun run build
```

Expected: all tests pass (the pre-existing count of 415 plus this plan's new
tests), no clippy warnings, both frontend entries built.

Report the actual numbers. If anything fails, fix it before the commit — do
not report a passing gate you did not see pass.

- [ ] **Step 6: Verify the release build**

```bash
cargo build --release -p yappr --features custom-protocol
```

Expected: builds. `custom-protocol` is not a default feature; without it the
binary embeds `devUrl` and every window is blank unless Vite is running.

- [ ] **Step 7: Commit**

```bash
git add CLAUDE.md README.md HANDOVER.md
git commit -m "docs: describe the first-run wizard and what is unverified"
```
