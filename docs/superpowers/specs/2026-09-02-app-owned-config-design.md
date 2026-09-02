# App-owned configuration

*Design, 2026-09-02. Supersedes the hand-editable-`config.toml` contract described
in CLAUDE.md invariants 4 and 9 and in README's `config.toml` section.*

## The decision

`config.toml` stops being a documented human interface. The settings GUI becomes
the way configuration is changed; the file becomes an implementation detail the
app owns, writes canonically, and repairs by itself.

Nothing here *prevents* hand-editing — the file lives in the user's own home
directory and always will. What changes is what the file is, where it lives, and
what the app does when it finds one it cannot read.

### Why

Four reasons, all of them accepted at design time:

1. **Two writers on one file.** The GUI's autosave posts a whole in-memory
   snapshot; anything that changed underneath is reverted by the next unrelated
   toggle. This is not hypothetical and not limited to hand edits — `wizard_finish`
   writes `inject.backend` through `set_config` (`src-tauri/src/wizard.rs:157`),
   and on GNOME (`recommended_backend(Gnome) == "ydotool"`) a stale snapshot
   silently puts `wtype` back, which under Mutter types nothing at all. A
   reveal-time resync landed on 2026-09-02 (`show-settings`, see
   `src-tauri/src/lib.rs`'s `show_settings_window`) and closes the reachable case;
   it does not make the file safe to edit while the window is open.
2. **A bad file is fatal.** `server::start` loads the config eagerly
   (`crates/yappr-core/src/server.rs:608`) and `?`s it; `setup()` calls that at
   `src-tauri/src/lib.rs:418`. Every section is `deny_unknown_fields`, so one
   typo'd key aborts Tauri setup — no tray, no settings window, no way to repair
   it from inside the app. Hand-editing is currently the *only* recovery path,
   which is precisely the dependency this design has to remove.
3. **The machinery costs more than it returns.** `crates/yappr-core/src/config_write.rs`
   is 535 lines of `toml_edit` merging, `decor` carrying and no-op leaf skipping
   whose entire purpose is preserving a hand-written file's comments and byte
   layout.
4. **One setting, one place.** A product stance, not a bug report.

## 1. Location and format

`paths::config_file()` (`crates/yappr-core/src/paths.rs:15`) moves from
`~/.config/yappr/config.toml` to `~/.local/state/yappr/config.toml`, beside
`wizard-done` and `rejections.jsonl`. `paths::state_dir()` already exists
(`paths.rs:23`). The format stays TOML.

**Stated consequence, accepted:** XDG calls the state directory a place for data
that persists between restarts but is not important enough to back up, and most
dotfile-sync setups cover `~/.config` and not `~/.local/state`. Settings will
therefore stop travelling between machines for users who sync that way. This was
chosen deliberately for the "the app owns this" signal it sends; it is recorded
here rather than re-argued.

`paths.rs:75`'s assertion (`config_file().ends_with("yappr/config.toml")`) still
holds and needs no change; a new assertion that the parent is the state dir
should join it.

## 2. The writer

`config_write::save_config` (`config_write.rs:190`) becomes three steps:

1. **Render** — a fixed header comment followed by `toml::to_string_pretty(&Config)`.
2. **Validate** — `Config::from_str` on the rendered text, exactly as today.
3. **Replace** — `write_atomically` (`config_write.rs:213`), unchanged.

Validate-then-rename **stays**. That property is about crash-safety, not
hand-editing: a half-written config is still a daemon that will not start.

**Deleted:** `merge_json_into_toml` (`config_write.rs:27`), `merge_into_table`
(`:48`), `build_array` (`:105`), `set_preserving_decor` (`:146`), `scalar_to_toml`
(`:167`), `config::DEFAULT_CONFIG_TOML` (`config.rs:683`), and the test suite
pinning comment survival and byte-identical no-op saves.

**Invariant 9's no-op guarantee survives for free.** A canonical dump is a pure
function of `Config`, so an unchanged config renders identical bytes by
construction. That is a stronger property than `toml_edit` was buying, with none
of the machinery. What is lost is only the *reason* it mattered — a user's
comments — and that is the point of the change.

`Config::load_from` (`config.rs:578`) currently seeds `DEFAULT_CONFIG_TOML` when
the file is absent. It seeds the canonical rendering of `Config::default()`
instead.

### 2.1 Two obsolete keys finally retire

`NormalizeConfig` carries `port` and `llama_server_path`, accepted and ignored
since the `llama-server` child was removed, kept only because `deny_unknown_fields`
would reject a pre-existing file without them (`config.rs:201`). Under a
comment-preserving merge they survived only in files that already had them. Under
a canonical dump they would be written into **every** user's file — the opposite
of retiring them.

Both fields get `#[serde(skip_serializing)]`: still accepted on read, never
written back. After one save they are gone from the file, and the struct fields
can be deleted outright in a later version once no file on disk still names them.
`schema.ts`'s `OBSOLETE_FIELDS` is unaffected.

### 2.2 Canonical rendering: verified, not assumed

`Config` (`config.rs:525`) declares `style_rules` after eight table fields, which
raised the question of whether `toml::to_string_pretty` would fail with
`ValueAfterTable`. It does not — probed directly against this tree on 2026-09-02
with a throwaway integration test, since guessing here would have sent the
implementation plan after a problem that does not exist:

- `toml::to_string_pretty(&Config::default())` renders 1234 bytes cleanly. The
  serializer hoists values above tables by itself, so declaration order in the
  struct does not matter and **no field reorder is needed**.
- `Config → render → Config` compared equal for the default config.
- A config carrying a `[[style_rules]]` entry renders it as an array-of-tables
  between `[style_default]` and `[vocabulary]`, with a `StyleRule`'s unset
  `Option` axes correctly omitted.

Two shape notes for whoever implements the header comment:

- An **empty** `style_rules` renders as a bare `style_rules = []` hoisted to the
  very first line, above every section. A prepended `#` header comment still sits
  above it, so the header is safe — but the file's first line changes shape
  depending on whether any style rule exists. This is the same hoisting the
  current `config_write.rs` comment complains about; under a canonical dump it is
  correct rather than a defect.
- The rendered default **does** contain `port = 8730` and
  `llama_server_path = "llama-server"`. This confirms §2.1 concretely: without
  `skip_serializing`, the first save would write both obsolete keys into every
  user's file.

The header comment is a fixed two-line block, prepended before rendering:

```toml
# Automatisch erzeugt von yappr. Änderungen über die Einstellungen
# (Tray-Symbol anklicken oder `yappr --settings`) -- Handedits gehen verloren.
```

## 3. Boot behaviour — invariant 4 rewritten

`deny_unknown_fields` **stays**. It is still how a typo, a downgrade, or a
half-written file is detected. Only the consequence changes.

A new `Config::load_or_quarantine(path) -> (Config, Option<Quarantine>)`:

1. renames the offending file to `config.toml.broken-<unix-timestamp>`, adding a
   `-<n>` counter if that name is somehow taken, so a second quarantine in the
   same second cannot destroy the first one's evidence,
2. writes a fresh canonical default in its place,
3. returns `Quarantine { moved_to: PathBuf, error: String }` alongside the
   defaults — the path so the GUI can name the file, the error string because
   `anyhow::Error` is not `Clone` and this has to sit behind a `Mutex` for the
   life of the process,
4. lets startup continue.

`server::start` stores the `Quarantine` on the `Daemon` and boots normally.

**Only startup quarantines.** `Config::load_from` stays strict, because its other
callers must keep reporting errors rather than silently resetting the user's
settings:

| Call site | Behaviour |
|---|---|
| `server::start` (`server.rs:608`) | `load_or_quarantine` — never fails |
| `Request::Reload` (`server.rs:1847`) | strict; "your file is broken" is the honest answer |
| `Request::GetConfig` (`server.rs:1891`) | strict |
| `Request::SetConfig` (`server.rs:1931`, `:1940`) | strict |
| `ensure_models_loaded` (`server.rs:959`) | strict |
| `setup.rs:17`, `wizard.rs:115` | strict |

Note that `validate()` (`config.rs:585`) failures — `max_seconds = 0`,
`context_size < 512` and the rest — are equally fatal at startup and quarantine on
the same path. Quarantine triggers on any `load_from` error, not only on parse
errors.

### 3.1 Surfacing it

The `Daemon` already has `fatal_error: Mutex<Option<String>>` (`server.rs:303`)
for a stored startup reason, read by `Request::Status` (`server.rs:1701`). A
quarantine is *not* fatal, so it needs its own field rather than reusing that one
— overloading `fatal_error` would make `Status` report a booted, working daemon
as `FAILED`.

`Response` gains `config_notice: Option<String>` with `skip_serializing_if`,
populated on `GetConfig` and `Status`. `Settings.tsx` renders it in the existing
`banner notice` slot, naming the quarantined file and the error. This is a
`Response` field, not an `OverlayEvent` variant, so CLAUDE.md invariant 3's
two-copies rule does not apply and `src/Overlay.tsx` needs no change.

## 4. Migration

A new `config::migrate_from_legacy(legacy, current) -> Result<Option<PathBuf>>`,
called once by `server::start` before the load:

- new file absent, old `~/.config/yappr/config.toml` present → load the old
  strictly, render it canonically to the new path, rename the old to
  `config.toml.migrated`. Never delete.
- both present → the state-dir file wins; the legacy file is ignored, with one
  log line.
- old file present but unloadable → leave it alone entirely and let §3's
  quarantine handle the new path. Migrating a broken file into the new location
  would only move the problem.

Migration runs before `Config::load()`, so `setup.rs:17` and `wizard.rs:115`
(Tauri commands, which cannot run until `setup()` has returned) always see the
new path. Under `--replay` no daemon starts and no migration runs; `setup_status`
would seed a default at the new path, which is correct for a mode that has no
pipeline anyway.

## 5. `--reload` stays

`SetConfig` already does a best-effort live apply through `update_reloadable`
(`server.rs:1950`), so `--reload`'s documented purpose — "apply the edit you just
made by hand" — disappears with this change. It is kept anyway: it is already
written and tested, and it remains the affordance for a config restored from a
backup or written by another invocation. Removing it buys nothing.

## 6. Documentation

- **README** — the `config.toml` section keeps its table of sections (a useful
  map of what exists), but "You can edit it by hand; the GUI is careful not to
  trample it" is replaced by the opposite, plus the new path and the quarantine
  behaviour. The "Unknown keys are a hard error" callout is rewritten: unknown
  keys are still detected, but they quarantine rather than brick.
- **CLAUDE.md** — invariants 4 and 9 rewritten; the paths bullet under
  Conventions updated.
- **`config_write.rs`'s module doc** — currently opens "`config.toml` is a file
  the user is invited to edit by hand". Rewritten to state the opposite and to
  say why validate-then-rename survives the change.

## 7. Testing

New:

- `Config → render → Config` is the identity, for `Config::default()` and for a
  fully-populated config with `style_rules`, vocabulary entries and replacements.
- Rendering the same `Config` twice is byte-identical (invariant 9's no-op-save
  property, now a property of the renderer).
- A file with an unknown key boots with defaults, is renamed to
  `config.toml.broken-<ts>`, and the reason reaches `Status` and `GetConfig`.
- A file failing `validate()` semantically (`max_seconds = 0`) quarantines on the
  same path.
- `Reload` on a broken file still reports an error and does *not* quarantine.
- Migration preserves every value, renames the legacy file, and is a no-op when
  the new file already exists.
- A rendered default config contains neither `port` nor `llama_server_path`
  (§2.1), and a legacy file containing both still loads.

Removed: the comment-preservation and byte-identical-merge suites in
`config_write.rs`.

## 8. Known gap, deliberately not closed here

With the GUI as the only interface, `schema.ts`'s guarantee — a key added in Rust
always renders somewhere, unlabelled in the `Weitere` pane if no category claims
its section — stops being a nicety and becomes load-bearing: a key the GUI cannot
render is a key nobody can set. Nothing enforces it mechanically today, and this
repo has no TypeScript test infrastructure to enforce it with.

Recorded as a gap rather than solved in the same change. Standing up a TS test
harness is its own piece of work, and bundling it here would hide a config
redesign inside a tooling change.

## 9. Out of scope

- Patch-shaped saves (posting only changed leaves instead of the whole snapshot).
  It is the class-wide fix for §"Why" item 1 and `SetConfig` already accepts
  partial objects — `wizard_finish`'s `backend_patch` proves it — but the
  reveal-time resync closes the reachable case, and stacking both changes into one
  spec makes neither reviewable.
- Deleting `NormalizeConfig::port` and `llama_server_path` outright. §2.1 stops
  writing them; removing the fields is a follow-up once no file on disk has them.
- Any change to `OverlayEvent`, the socket wire format, or the overlay.
