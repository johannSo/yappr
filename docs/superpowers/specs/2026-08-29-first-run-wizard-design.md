# First-run setup wizard — design

Status: proposed, 2026-08-29. Supersedes the first-run half of
`2026-08-28-settings-gui-design.md` §7: the Setup pane described there is
**deleted** by this design and replaced by the wizard below. The provisioning
machinery underneath it (`provision.rs`'s `setup_status`, `run_setup`, the
`setup-progress` event stream) is unchanged — only its consumer moves.

## Purpose

A fresh install currently drops the user into the settings window with a Setup
pane that downloads models and lists missing binaries. Nothing tells them what
the app is, nothing binds a shortcut, and nothing sets the injection backend —
so a user who completes "setup" still cannot dictate, because the one thing
that starts a dictation is a shortcut only they can bind.

This replaces that with a four-step wizard that ends with a working install:
models on disk, injection backend matched to the desktop, and the two shortcut
lines in front of the user with a copy button.

Scope note: yappr supports **Hyprland and GNOME**. Every other desktop gets a
generic, non-dead-end variant of step 3 (§4.3) rather than a refusal.

## 1. The flow

Four steps, rendered in the existing settings window with its sidebar hidden
(§8). No new Tauri window and no third Vite entry point.

| # | Step | Content | Actions |
|---|---|---|---|
| 1 | **Willkommen** | One line on what yappr is and what will happen in the next three steps. | `Los geht's` |
| 2 | **Modelle** | Missing prerequisites (`llama-server`, `wtype`, `wl-copy`, ggml backend) and the three models, with per-model progress. "Möchtest du die Modelle jetzt laden? (~1,1 GB)" | `Jetzt laden` / `Später`, then `Weiter` |
| 3 | **Desktop & Kurzbefehle** | "Deine Arbeitsumgebung: **Hyprland**". The exact shortcut lines, their target file, a copy button, and which injection backend was set and why. | `Weiter` |
| 4 | **Fertig** | "Alles eingerichtet. Drücke `SUPER+D` zum Diktieren." | `Fertig` (hides the window) · `Einstellungen öffnen` |

`Fertig` hides the window rather than revealing the settings view; the app
stays in the tray, which is where it lives. `Einstellungen öffnen` is the
secondary path for a user who wants to keep configuring.

**A download never blocks the wizard.** Once `Jetzt laden` is pressed, the
primary button becomes `Weiter` and stays enabled: the `setup-progress`
listener is scoped to the window rather than to step 2 (§8), so the download
continues while the user reads steps 3 and 4, and step 4 shows its progress.
Nobody is held on one screen for 1,1 GB. `run_setup`'s single-flight guard
already makes a second press a no-op, so the button is simply hidden while a
download is in flight rather than needing its own disabled state.

**Prerequisites belong in step 2, not a step of their own.** A missing `wtype`
blocks dictation exactly as hard as a missing model, and both are answered by
the same question — "is everything this needs on disk?". Splitting them would
give the user two consecutive screens that each say "something is missing".

**~1,1 GB is the real figure**, verified against `models_dir()` on this
machine: 484 MB S1-mini, ~600 MB Parakeet (unpacked), 0.6 MB Silero. The
figure the wizard prints is a constant in the frontend next to the same string
the old Setup pane used; it is not computed, because the download sizes
(compressed `.tar.bz2` for Parakeet) and the on-disk sizes differ and the
number the user cares about is disk.

### 1.1 Where the wizard starts

`wizard_state()` (§9) returns `start_step`, and there are exactly two values:

- **`welcome`** — the marker file (§6) is absent. A genuine first run.
- **`models`** — the marker is present but `setup_status()` is not ready. The
  install was completed once and has since broken (a model deleted, a hash
  failing, a prerequisite uninstalled).

When the wizard is opened by hand (tray, `--wizard`) on a healthy install,
`should_open` is `false` and `start_step` is `welcome` — an explicit request
gets the whole flow, from the top.

The second case skips the welcome deliberately. A returning user does not need
to be introduced to the app again; they need the one screen that fixes what
broke. This is also what makes step 2's `Später` honest rather than a trap: a
user who defers the download is re-prompted on the next launch, but lands
directly on the download screen, one click from acting on it. The app cannot
dictate without models, so silently never mentioning it again would be worse.

## 2. What this deletes

| Deleted | Where |
|---|---|
| `SetupPane` | `src/Settings.tsx` |
| `SETUP_CATEGORY` and its conditional sidebar prepend | `src/settings/schema.ts`, `src/Settings.tsx` |
| The `setupStatus`-gated category list in `Settings.tsx` | `src/Settings.tsx` |

`setup_status`, `run_setup`, the `setup-progress` event stream, the
single-flight install guard and `missing_models_cached` are all **kept
unchanged**. This is a change of consumer, not of provisioning.

`schema.ts`'s comment about `SETUP_CATEGORY` being the one category with no
`config.toml` section behind it goes with it; after this change every category
in the sidebar is config-backed again, which restores the property that file's
doc comment claims.

## 3. Desktop detection — `crates/yappr-core/src/desktop.rs`

New module in the core crate, alongside `hypr.rs`, because it is the same kind
of thing: knowledge about the surrounding desktop, testable without a GUI.

```rust
pub enum Desktop { Hyprland, Gnome, Other(String), Unknown }

/// Reads the environment.
pub fn detect() -> Desktop;

/// The pure function `detect` delegates to — this is what the tests drive.
pub fn detect_from(
    hyprland_signature: Option<&str>,
    xdg_current_desktop: Option<&str>,
    xdg_session_desktop: Option<&str>,
) -> Desktop;
```

Resolution order:

1. `HYPRLAND_INSTANCE_SIGNATURE` non-empty → `Hyprland`. Hyprland sets this
   itself for every client it launches, so it is evidence rather than
   configuration.
2. `XDG_CURRENT_DESKTOP`, split on `:` (it is a colon-separated list —
   `ubuntu:GNOME` is a real value), compared case-insensitively: a component
   equal to `hyprland` → `Hyprland`, `gnome` → `Gnome`.
3. `XDG_SESSION_DESKTOP`, same comparison.
4. Otherwise `Other(name)` with the first non-empty value seen, or `Unknown`
   when nothing is set.

Splitting `detect_from` out of `detect` follows `provision.rs`'s `build_status`
and `cli.rs`'s `route()`: the decision is a pure function with a test per row,
and only the thin wrapper touches the environment.

## 4. Shortcut instructions per desktop

```rust
pub struct ShortcutInstructions {
    pub kind: ShortcutKind,           // HyprLua | HyprConf | Gnome | Generic
    pub target: Option<String>,       // file path or settings location
    pub snippet: String,              // the copyable block
    pub bindings: Vec<ShortcutBinding>, // GNOME's GUI triples; empty on Hyprland
}

pub struct ShortcutBinding { pub name: String, pub command: String, pub keys: String }

pub fn shortcut_instructions(d: &Desktop) -> ShortcutInstructions;
```

### 4.1 Hyprland

`snippet` is `hypr::shortcut_config()` verbatim — it already picks Lua vs
`.conf` by probing for `~/.config/hypr/hyprland.lua`, and already carries the
migration text naming the dead `openwhisprflow`/`owf-ctl` lines to delete.
`target` is `~/.config/hypr/bindings.lua` or `~/.config/hypr/hyprland.conf`
accordingly. Nothing in `hypr.rs` changes.

The wizard **does not write this file**. That is CLAUDE.md's standing rule
("never apply Hyprland config on the user's behalf — emit it and let them
paste it"), and it holds here unchanged.

### 4.2 GNOME

The primary instruction is the GUI path, with the two triples rendered as
copyable fields:

> **Einstellungen → Tastatur → Tastenkürzel anpassen → Eigene Tastenkürzel → +**

| Name | Befehl | Tastenkürzel |
|---|---|---|
| `yappr: Diktat starten/stoppen` | `yappr --toggle` | `Super+D` |
| `yappr: Diktat abbrechen` | `yappr --cancel` | `Super+Alt+D` |

Those three columns are one `ShortcutBinding` each — grouped rather than
flattened into label/value pairs, so the step renders two cards instead of six
repeated-label rows.

The `gsettings` equivalent sits behind a secondary "oder per Terminal"
disclosure:

```sh
BASE=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings
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
```

**Why the GUI path is primary and not the snippet.** The obvious one-liner —
`gsettings set … custom-keybindings "['…/yappr-toggle/', '…/yappr-cancel/']"` —
*overwrites* the list, silently destroying every custom shortcut the user
already had. A copy-paste block that eats a user's existing bindings is the
same class of mistake as writing their compositor config, and the rule against
one is a rule against the other. The `case` above exists to append instead of
replace, and to be idempotent when re-run; the GUI path cannot clobber
anything at all, which is why it leads.

### 4.3 Unsupported desktops

`Other`/`Unknown` gets a step 3 that names what was detected, states plainly
that only Hyprland and GNOME are supported, and gives the two commands to bind
however that desktop does it:

```
yappr --toggle    # Diktat starten/stoppen
yappr --cancel    # Diktat abbrechen
```

`recommended_backend` for these is `wtype` (the default; it needs no daemon and
no `/dev/uinput`). This is a degraded path, not a dead end — a sway or river
user has a working install after it.

## 5. Injection backend

| Desktop | `[inject] backend` | Why |
|---|---|---|
| Hyprland | `wtype` | wlroots; `wtype` is self-contained |
| GNOME | `ydotool` | Mutter does not implement the virtual-keyboard protocol `wtype` needs |
| Other/Unknown | `wtype` | The existing default |

Written through the existing `set_config` → `config_write` path, so invariant 9
holds unchanged: merged with `toml_edit`, unchanged leaves skipped, the
rendered result validated by `Config::from_str` before the atomic rename. When
the recommendation already matches what is in the file, `config_write` writes
nothing and the file stays byte-identical.

**Only written on a genuine first run** (`start_step == "welcome"`, i.e. the
marker was absent). On a re-run the step shows the recommendation with a button
to apply it instead of writing silently — a user who deliberately switched to
`ydotool` on Hyprland to reach an XWayland window must not have that undone by
a wizard they reopened to fix something else. Making this conditional on the
marker needs no new machinery: `wizard_state()` already reports it.

### 5.1 GNOME needs more than the config key

`ydotool` is not self-contained. It needs the `ydotool` binary, a running
`ydotoold`, and access to `/dev/uinput`. Step 3 on GNOME therefore carries its
own prerequisite block with copyable commands:

```sh
sudo pacman -S ydotool
systemctl --user enable --now ydotoold
```

plus a line naming `/dev/uinput` permissions as the thing to check if typing
stays silent. Consistent with §4: shown, never run.

`setup.rs`'s `check_prerequisites` deliberately reports a missing `ydotool` as
`absent (optional)` rather than MISSING, because `wtype` is the default and
most machines never need it. That stays true — this block is desktop-specific
and lives in step 3, not in step 2's prerequisite list.

## 6. The first-run gate

New in `crates/yappr-core/src/paths.rs`:

```rust
pub fn wizard_marker() -> PathBuf   // state_dir().join("wizard-done")
```

An empty file. It records one fact — "a human reached step 4 and clicked
Fertig" — and nothing else, so there is no format to version.

New in `src-tauri/src/wizard.rs` (`use std::path::Path`):

```rust
/// The decision, as a pure function — this is what the tests drive.
pub(crate) fn should_open_wizard_at(marker: &Path, ready: bool) -> bool {
    !marker.exists() || !ready
}

/// The thin wrapper that touches the filesystem.
pub(crate) fn should_open_wizard() -> bool {
    should_open_wizard_at(
        &yappr_core::paths::wizard_marker(),
        provision::is_ready_or_assume_not("yappr"),
    )
}
```

Split for the same reason `detect_from` and `build_status` are: the decision is
a pure function with a test per row, and only the wrapper reads the world.

`lib.rs`'s existing startup thread — the one that today calls
`is_ready_or_assume_not` and shows the settings window — calls this instead. It
stays on a background thread for the reason it already does: `is_ready` hashes
whatever models are on disk, and that must not run on the Tauri event-loop
thread.

The marker is written **only** by `wizard_finish` (§9). Closing the window
mid-wizard leaves it absent, and the wizard returns on the next launch, which
is correct: setup was not finished.

## 7. Reachability

The wizard must be reachable after it has been dismissed, for the same reason
the Setup pane was reachable: an install can break later.

- **`proto::Request::ShowWizard`**, wire name `"show-wizard"`, added to
  `proto.rs` next to `ShowSettings` and to that module's round-trip test list.
- **`lib.rs::show_wizard_window(app)`**, a shared helper next to the existing
  `show_settings_window`: show and focus the settings window, then emit a
  `show-wizard` Tauri event to it. It exists for the reason
  `show_settings_window`'s doc comment gives — two callers (the sink below and
  the tray) must not maintain two copies of the same two lines.
- **`EventSink::show_wizard`**, a no-op default method exactly like
  `show_settings`, so every existing sink (`NoExtraSink` and each test sink)
  stays valid without edits. `TauriSink` overrides it to call
  `show_wizard_window`.
- **`Settings.tsx`** listens for `show-wizard` and enters wizard mode. It
  already holds a `listen` for `setup-progress`, so this is the same pattern.
- **`cli.rs`**: `["--wizard"] => Route::Send(Request::ShowWizard)`, with a row
  in the existing per-route test table. `--help` gains the flag.
- **`tray.rs`**: an `Einrichtung…` `StandardItem` directly under
  `Einstellungen`, calling `show_wizard_window` the way the existing item calls
  `show_settings_window` — directly, not through a `Daemon` dispatch, per that
  module's doc comment on why the tray must never block on the accept loop.

## 8. Frontend structure

**New file `src/settings/wizard.tsx`** holds the whole flow — the step machine,
the four step components, and the copy button. `Settings.tsx` is already 966
lines; the wizard does not go inside it.

`Settings.tsx` gains one piece of state (`wizardActive`) and one branch: when
active, render `<Wizard>` full-bleed in place of the sidebar and panes.
`Settings.css` gains `.wizard*` classes in the existing frosted-tile idiom.

Motion follows the file convention: named spring consts declared at the top of
`wizard.tsx` and nothing else used. Step transitions are a horizontal
slide-and-fade with `bounce: 0` — a step change is a discrete navigation with
no momentum behind it, and the codebase reserves overshoot for motion that had
some.

The step-2 download UI is a direct port of `SetupPane`'s: the same
`setup-progress` subscription, the same `downloadStatusText`, the same progress
bars. That listener stays mounted for the life of the window, not just while
step 2 is on screen — the existing comment in `Settings.tsx` explains why, and
the reason survives the move.

## 9. Rust command surface

Two new Tauri commands, in `src-tauri/src/wizard.rs`, registered in `lib.rs`'s
`invoke_handler` alongside `setup_status`/`run_setup`. Both are `async fn` that
hand blocking work to `spawn_blocking`, for the reason `provision.rs`'s module
doc gives.

```
wizard_state() -> {
  "should_open":         bool,
  "start_step":          "welcome" | "models",
  "desktop":             "hyprland" | "gnome" | "other" | "unknown",
  "desktop_name":        "Hyprland",
  "recommended_backend": "wtype" | "ydotool",
  "current_backend":     "wtype" | "ydotool",
  "backend_prereqs":     ["ydotool", "ydotoold"],
  "shortcut": {
    "kind":     "hypr-lua" | "hypr-conf" | "gnome" | "generic",
    "target":   "~/.config/hypr/bindings.lua" | null,
    "snippet":  "...",
    "bindings": [ { "name": "yappr: Diktat starten/stoppen",
                    "command": "yappr --toggle",
                    "keys": "Super+D" }, ... ]
  }
}

wizard_finish({ set_backend: "wtype" | "ydotool" | null }) -> ()
```

`wizard_finish` writes the marker file and, when `set_backend` is non-null,
writes `[inject] backend` through the same `Request::SetConfig` path the
settings form uses. The frontend passes the recommendation on a first run and
`null` on a re-run unless the user pressed the apply button (§5) — the decision
is explicit in the call rather than re-derived server-side, so it cannot race
against a marker written moments earlier in the same click.

The frontend learns its mode from `wizard_state()` on mount rather than from a
startup event, so there is no ordering hazard between `setup()`'s background
thread and the webview finishing its first paint.

## 10. Testing

`cargo test --workspace && cargo clippy --workspace --all-targets` stays the
gate. Tests are named as full sentences, per the existing convention.

| Unit | Test |
|---|---|
| `desktop::detect_from` | one row per environment shape: `HYPRLAND_INSTANCE_SIGNATURE` set with a conflicting `XDG_CURRENT_DESKTOP`; `GNOME`; `ubuntu:GNOME`; `KDE` → `Other`; all unset → `Unknown`; case-insensitivity |
| `desktop::shortcut_instructions` | Hyprland Lua and `.conf` produce the matching `target` and a snippet containing `yappr --toggle`; GNOME produces both `<Super>d` and `yappr --cancel` and a non-empty `fields`; `Other` produces the generic pair |
| `paths::wizard_marker` | lands in the state dir, next to `rejections.jsonl` |
| `wizard::should_open_wizard` | the four combinations of marker present/absent × ready/not, with the marker path injected |
| `cli::route` | `--wizard` resolves to `Route::Send(Request::ShowWizard)` |
| `proto` | `ShowWizard` round-trips and serialises to `{"cmd":"show-wizard"}` |

There is no frontend test framework in this repo and this design does not add
one. The wizard UI is verified by stubbing `__TAURI_INTERNALS__` in a throwaway
HTML entry point and driving all four steps in a browser — the same technique
the settings window is developed with, and the only one available here, since
`hyprctl focuswindow` fails on this machine's Lua config.

## 11. Deliberately excluded

- **Shortcut verification.** Considered and rejected. Note for anyone who
  revisits it: `hyprctl binds -j` **cannot** see a Lua-defined bind's command.
  On this machine all 227 binds report `"dispatcher": "__lua"` with an opaque
  numeric `arg`; it only exposes `exec` + the command text on a classic `.conf`
  config. Verification would have to grep `~/.config/hypr/**` instead.
- **A live `SUPER+D` test press** in step 4. It opens the microphone, forces a
  ~1,1 GB lazy model load, and types the result into whichever window has focus
  — which during the wizard is the wizard.
- **Writing any desktop config**, on either desktop (§4).
- **A dedicated wizard window** and a third Vite entry point.

## 12. Unverified

The GNOME path — detection via `XDG_CURRENT_DESKTOP`, the `gsettings` snippet,
the `ydotool`/`ydotoold` prerequisites — **cannot be verified on this machine**,
which runs Hyprland. It ships marked unverified in `HANDOVER.md`, in the same
sense the audio path is: written carefully against documented behaviour, never
executed. The Hyprland path and everything in §3 and §6 are testable here and
are covered by §10.
