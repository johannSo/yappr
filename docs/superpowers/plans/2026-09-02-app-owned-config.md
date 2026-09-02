# App-owned configuration — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `config.toml` a file the app owns — written canonically, moved to the
state dir, and quarantined rather than fatal when it will not load.

**Architecture:** The `toml_edit` comment-preserving merge is replaced by a pure
renderer (`Config → String`) plus an explicit JSON patch step, because `SetConfig`
routinely receives partial objects. Startup gains a quarantine path so a bad file can
never abort Tauri setup, and a one-time migration moves the file out of `~/.config`.

**Tech Stack:** Rust (serde, `toml`, `serde_json`, `anyhow`), React/TypeScript, Tauri 2.

**Spec:** `docs/superpowers/specs/2026-09-02-app-owned-config-design.md`

## Global Constraints

- German for every user-facing string. The settings window and all banners are German.
- House comment style: explain *why*, cite `spec §N`, use `--` not em-dashes in Rust docs.
- Tests are named as full sentences. `known_limitation_` prefixes documented-but-unfixed behaviour.
- The gate is `cargo test --workspace && cargo test --workspace -- --ignored && cargo clippy --workspace --all-targets`, plus `bun run build`. There is no CI.
- Never point a test at `paths::config_file()` — `set-config` writes, and a test running against the real path overwrites the config of whoever ran the suite. Use the existing `scratch` / `scratch_config` helpers.
- `crates/yappr-core/src/config.rs`'s `DEFAULT_CONFIG_TOML` is `pub` but referenced in exactly two files (`config.rs`, `config_write.rs`) and nowhere in `src-tauri` or `src/`. Deleting it breaks no in-tree consumer.
- Historical records under `.superpowers/sdd/` and `docs/superpowers/{specs,plans}/` dated before today must **not** be rewritten — CLAUDE.md says the stored `review-*.diff` files have to keep matching the commits they record.

---

### Task 1: The canonical renderer

**Files:**
- Modify: `crates/yappr-core/src/config.rs` — add `render`, delete `DEFAULT_CONFIG_TOML` (683-779), fix `Config::load_from` (578-585), fix doc comments at 359, 565, 575
- Test: `crates/yappr-core/src/config.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub fn config::render(cfg: &Config) -> String` — the fixed header comment
  followed by `toml::to_string_pretty(cfg)`. Infallible: Task 1's round-trip test proves
  `Config` always serializes (verified by probe on 2026-09-02, no field reorder needed).
- Produces: `NormalizeConfig::port` and `NormalizeConfig::llama_server_path` carry
  `#[serde(skip_serializing)]` — read but never written.

- [ ] **Step 1: Write the failing tests**

In `config.rs`'s test module:

```rust
/// The renderer is a pure function of `Config`, which is what replaces
/// invariant 9's old byte-identical guarantee: the same config cannot
/// render two different files, so a save that changes nothing cannot
/// change the file. Free, where `toml_edit` had to work for it.
#[test]
fn rendering_the_same_config_twice_is_byte_identical() {
    let cfg = Config::default();
    assert_eq!(render(&cfg), render(&cfg));
}

/// The guard that derived `Default` agrees with the serde `default = "d_*"`
/// functions. This used to be `default_config_toml_round_trips_to_config_default`
/// against the hand-written annotated const; the const is gone, but the
/// property it protected is not, and it is the only whole-`Config` comparison
/// in the workspace. `server.rs`'s `GetConfig` ships `Config::default()` on
/// the wire as the GUI's reset targets (invariant 9), so a drift here is a
/// reset button that restores the wrong value.
#[test]
fn a_rendered_default_config_parses_back_as_the_default() {
    let rendered = render(&Config::default());
    assert_eq!(Config::from_str(&rendered).unwrap(), Config::default());
    // Every section must appear: a section missing from the render is a
    // group of settings the GUI would show as absent.
    for section in ["[audio]", "[asr]", "[models]", "[normalize]", "[guardrail]",
                    "[inject]", "[overlay]", "[style_default]", "[vocabulary]", "[debug]"] {
        assert!(rendered.contains(section), "{section} missing from the rendered default");
    }
}

/// A config carrying everything optional, so the array-of-tables and the
/// `Option` axes are exercised rather than assumed. `StyleRule`'s unset axes
/// must simply be absent -- TOML has no null.
#[test]
fn a_fully_populated_config_round_trips_through_the_renderer() {
    let cfg = Config::from_str(
        r#"
[[style_rules]]
match_class = "term.*"
styling = "technical"

[vocabulary]
terms = ["Kubernetes"]
[[vocabulary.replacements]]
from = "kdd"
to = "KDD"
"#,
    )
    .unwrap();
    assert_eq!(Config::from_str(&render(&cfg)).unwrap(), cfg);
}

/// Spec §2.1. Under the old comment-preserving merge these two survived only
/// in files that already had them; a canonical dump would write them into
/// every user's file, which is the opposite of retiring them. Reading one
/// must still work -- the section is `deny_unknown_fields`, so a
/// pre-existing file naming them would otherwise fail to start.
#[test]
fn the_two_obsolete_normalize_keys_are_read_but_never_written() {
    let rendered = render(&Config::default());
    assert!(!rendered.contains("port"), "normalize.port must not be written any more");
    assert!(!rendered.contains("llama_server_path"));

    let legacy = Config::from_str(
        "[normalize]\nport = 8730\nllama_server_path = \"llama-server\"\n",
    );
    assert!(legacy.is_ok(), "a pre-existing file naming them must still load");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p yappr-core config::tests:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function 'render' in this scope`.

- [ ] **Step 3: Implement**

In `config.rs`, replacing `DEFAULT_CONFIG_TOML` (lines 683-779):

```rust
/// What every written `config.toml` starts with. Two lines, fixed: the file
/// is the app's now (spec §1), and the one thing a human opening it needs to
/// know is that editing it is pointless.
const HEADER: &str = "\
# Automatisch erzeugt von yappr. Änderungen über die Einstellungen
# (Tray-Symbol anklicken oder `yappr --settings`) -- Handedits gehen verloren.
";

/// The config as it is written to disk: a pure function of [`Config`], which
/// is the whole of invariant 9's no-op-save guarantee now (spec §2).
///
/// `to_string_pretty` cannot fail for this type -- `Config` is a plain tree of
/// structs, `Vec`s and scalars with no map keys that could be non-strings, and
/// the value-before-table ordering TOML requires is handled by the serializer
/// itself rather than by field order here (verified against this tree, spec
/// §2.2). An `expect` rather than a `Result` keeps every caller from carrying
/// an error case that cannot occur.
pub fn render(cfg: &Config) -> String {
    let body = toml::to_string_pretty(cfg).expect("Config is always serializable to TOML");
    format!("{HEADER}{body}")
}
```

In `NormalizeConfig` (around `config.rs:193`), on both obsolete fields:

```rust
    /// Accepted and ignored: there is no `llama-server` to point a port at
    /// since the engine moved in-process. Kept because the section is
    /// `deny_unknown_fields` (invariant 4) and deleting the field would turn
    /// every pre-existing `config.toml` into a hard startup failure --
    /// `skip_serializing` is what finally retires it, by never writing it
    /// back (spec §2.1). Once no file on disk still names it, the field goes.
    #[serde(default = "d_port", skip_serializing)]
    pub port: u16,
```

(the same `skip_serializing` addition on `llama_server_path`, keeping each field's
existing `default = "d_*"`.)

In `Config::load_from` (`config.rs:581`):

```rust
            std::fs::write(p, render(&Config::default()))?;
```

- [ ] **Step 4: Fix the doc comments the const's deletion strands**

- `config.rs:359` — delete the whole "Kept in sync with `DEFAULT_CONFIG_TOML` by …"
  sentence from `d_terminal_classes`. It names a test that does not exist under that
  name anyway, and under a canonical render the list is emitted *from* this function,
  so there is no second copy to keep in sync.
- `config.rs:565` (`Config::load`) and `config.rs:575` (`load_from`) — both say
  "writing a **commented** default file if none exists". Drop "commented".
- `config.rs:947` — the comment on
  `every_inject_backend_is_spelled_the_way_config_toml_spells_it` cites the
  `# "wtype" | "ydotool" | "clipboard"` annotation in the const. Keep the test (its
  body never touched the const), and rewrite the comment to name the two surviving
  copies: `ENUMS["inject.backend"]` (`src/settings/schema.ts:205`) and
  `HELP["inject.backend"]` (`schema.ts:170`).

- [ ] **Step 5: Delete the superseded tests**

- `config.rs:936` `default_config_toml_round_trips_to_config_default` — replaced by
  `a_rendered_default_config_parses_back_as_the_default` in Step 1.
- `config.rs:1062` `the_shipped_default_config_declares_the_models_section` — its
  rationale ("a section that exists in Rust but not there is a setting no hand-editor
  discovers") is the contract this design retires, and the `[models]` assertion is
  folded into the new round-trip test.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p yappr-core config:: 2>&1 | tail -20`
Expected: PASS. `cargo clippy -p yappr-core --all-targets` clean.

- [ ] **Step 7: Commit**

```bash
git add crates/yappr-core/src/config.rs
git commit -m "feat: render config.toml canonically instead of shipping an annotated template"
```

---

### Task 2: The writer

**Files:**
- Modify: `crates/yappr-core/src/config_write.rs` — rewrite module doc (1-18), delete `merge_json_into_toml` (27), `merge_into_table` (48), `build_array` (105), `set_preserving_decor` (146), `scalar_to_toml` (167); rewrite `save_config` (190)
- Modify: `crates/yappr-core/src/server.rs:3223` — drop one comment-preservation assertion
- Test: `crates/yappr-core/src/config_write.rs` `#[cfg(test)] mod tests` (228+)

**Interfaces:**
- Consumes: `config::render` from Task 1.
- Produces: `save_config(path: &Path, incoming: &Value) -> Result<String>` — unchanged
  signature, so `server.rs:1937` needs no edit.
- Produces: `fn merge_json(base: &mut Value, patch: &Value)` — private.

- [ ] **Step 1: Write the failing test that matters most**

```rust
/// The regression a canonical dump makes easy to introduce, and the reason
/// spec §2 has an explicit patch step at all. `Request::SetConfig` passes its
/// JSON straight through (`server.rs`), and `wizard_finish`'s `backend_patch`
/// is a single leaf -- so a writer that rendered `incoming` directly would
/// blank every key the patch did not name. On GNOME that is the whole config,
/// destroyed by the wizard's own last action.
#[test]
fn a_partial_patch_leaves_every_key_it_does_not_name_at_its_on_disk_value() {
    let dir = scratch("partial-patch");
    let path = dir.join("config.toml");
    let mut seed = Config::default();
    seed.audio.max_seconds = 90;
    seed.asr.num_threads = 7;
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, crate::config::render(&seed)).unwrap();

    save_config(&path, &serde_json::json!({"inject": {"backend": "ydotool"}})).unwrap();

    let after = Config::load_from(&path).unwrap();
    assert_eq!(after.inject.backend, "ydotool", "the patch must apply");
    assert_eq!(after.audio.max_seconds, 90, "an unnamed key must keep its on-disk value");
    assert_eq!(after.asr.num_threads, 7, "not fall back to the default");

    let _ = std::fs::remove_dir_all(&dir);
}

/// TOML has no null, and the settings GUI posts `"styling": null` for a
/// `StyleRule`'s unset axes. Removing the key is what makes it deserialize
/// back as `Option::None`; keeping it would be a write the file cannot hold.
#[test]
fn a_null_in_a_patch_removes_the_key_rather_than_being_rejected() {
    let dir = scratch("null-removes");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, crate::config::render(&Config::default())).unwrap();

    save_config(
        &path,
        &serde_json::json!({"style_rules": [{"match_class": "foo", "styling": null}]}),
    )
    .unwrap();

    let after = Config::load_from(&path).unwrap();
    assert_eq!(after.style_rules.len(), 1);
    assert_eq!(after.style_rules[0].styling, None);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Arrays are values, not documents: a patch naming a shorter list means the
/// shorter list, never an element-wise merge with what was there.
#[test]
fn an_array_in_a_patch_replaces_the_existing_one_wholesale() {
    let dir = scratch("array-replace");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    let mut seed = Config::default();
    seed.vocabulary.terms = vec!["Kubernetes".into(), "Wayland".into()];
    std::fs::write(&path, crate::config::render(&seed)).unwrap();

    save_config(&path, &serde_json::json!({"vocabulary": {"terms": ["Wayland"]}})).unwrap();

    assert_eq!(Config::load_from(&path).unwrap().vocabulary.terms, vec!["Wayland"]);

    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p yappr-core config_write::tests::a_partial_patch -- --exact --nocapture`
Expected: FAIL (these exercise the new writer against the old merge; the partial-patch
one may pass today by accident — that is fine, it must pass *after* the rewrite too,
which is what it is here for).

- [ ] **Step 3: Implement the writer**

Replace `save_config` and delete the five merge functions:

```rust
/// Deep-merges a JSON patch into a JSON value (spec §2, step 2).
///
/// Three rules, and the second two are the ones worth stating: objects merge
/// recursively; arrays replace wholesale, because an array is a value and an
/// element-wise merge would make a shortened list impossible to express; and
/// `null` removes the key, because TOML has no null and an absent key *is*
/// how `Option::None` is represented.
fn merge_json(base: &mut Value, patch: &Value) {
    let (Value::Object(base_map), Value::Object(patch_map)) = (&mut *base, patch) else {
        *base = patch.clone();
        return;
    };
    for (key, value) in patch_map {
        match value {
            Value::Null => {
                base_map.remove(key);
            }
            Value::Object(_) => match base_map.get_mut(key) {
                Some(existing) => merge_json(existing, value),
                None => {
                    base_map.insert(key.clone(), value.clone());
                }
            },
            _ => {
                base_map.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Applies `incoming` to the config at `path` and writes the result.
///
/// `incoming` is routinely *partial*: `Request::SetConfig` passes its JSON
/// through untouched and `wizard_finish`'s `backend_patch` is a single leaf.
/// The old `toml_edit` writer satisfied that implicitly, by editing a document
/// that already held every other key. A canonical dump has no such document,
/// so the base is loaded as a `Config` and the patch applied to it explicitly
/// (spec §2) -- rendering `incoming` directly would blank everything it does
/// not name.
///
/// Returns the rendered document that was written, so a caller can log or diff
/// it.
pub fn save_config(path: &Path, incoming: &Value) -> Result<String> {
    // `exists`, not `Config::load_from`: that one *creates* the file when it
    // is absent, and a writer must not do that as a side effect of deciding
    // what to base a patch on.
    let base = if path.exists() {
        Config::load_from(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Config::default()
    };

    let mut merged = serde_json::to_value(&base).context("serializing the current config")?;
    merge_json(&mut merged, incoming);

    // Where `deny_unknown_fields` and `validate()` run on the change itself.
    // The context string is load-bearing: a test matches on "not valid".
    let next: Config =
        serde_json::from_value(merged).context("the resulting config is not valid")?;
    next.validate_public().context("the resulting config is not valid")?;

    let rendered = crate::config::render(&next);

    // Not redundant with the check above: that one rejects a bad patch, this
    // one rejects a renderer that emitted something it cannot read back.
    Config::from_str(&rendered).context("the rendered config is not valid")?;

    write_atomically(path, &rendered)?;
    Ok(rendered)
}
```

Note: `Config::validate` is private (`config.rs:585`). Either make it `pub(crate)` and
call it as above, or drop the explicit call and rely on step 5's `Config::from_str`,
which validates. **Prefer making it `pub(crate)`** — rejecting a bad patch before
rendering gives the better error message, and the test matching `"not valid"` needs to
hit on the patch, not on the render.

- [ ] **Step 4: Delete the superseded tests**

Delete outright (each pins comment/decor/byte preservation, and most call the deleted
`merge_json_into_toml`):

| Line | Test |
|---|---|
| 246 | `changing_a_value_keeps_the_comment_above_its_section` |
| 253 | `changing_a_value_keeps_the_trailing_comment_on_its_own_line` |
| 262 | `untouched_keys_are_left_exactly_as_they_were` |
| 291 | `an_existing_array_of_tables_stays_an_array_of_tables` |
| 323 | `changing_an_inject_setting_keeps_the_comments_documenting_the_section` |
| 349 | `changing_a_value_keeps_the_comment_lines_above_it` |
| 428 | `saving_one_change_adds_only_that_one_setting` |
| 442 | `an_unset_top_level_array_is_not_materialised_above_the_header_comment` |
| 517 | `changing_the_idle_timeout_produces_a_one_line_diff` |

Two of these are safe to lose for a non-obvious reason worth recording in the commit
message: `changing_a_value_keeps_the_comment_lines_above_it` was the only test asserting
the license-mandated `S1-mini by Superwhisper` attribution *in the config comments*, but
that attribution is independently pinned at `models.rs:602-603` and
`src-tauri/src/provision.rs:373`, neither of which this change touches.

- [ ] **Step 5: Rewrite the salvageable tests**

Each of these keeps its point and swaps `merge_json_into_toml(...)` for a `save_config`
call against a scratch file, asserting on the parsed `Config` rather than on bytes:

| Line | Test | What survives |
|---|---|---|
| 269 | `a_missing_section_is_created` | a patch naming an absent section still applies (rename it: nothing is "created" under a full dump) |
| 277 | `a_string_array_is_replaced_wholesale` | already asserts on parsed `Config` — entrypoint swap only (may be subsumed by Step 1's array test; delete if so) |
| 309 | `an_emptied_array_of_tables_is_written_as_an_empty_array` | an emptied list must not be silently dropped — `#[serde(default)]` would hide it |
| 371 | `a_full_config_round_trips_through_the_writer` | seed from `Config::default()` instead of the deleted const |
| 384 | `a_null_is_written_as_an_absent_key_rather_than_rejected` | subsumed by Step 1's null test — delete this one |
| 403 | `a_config_containing_a_partial_style_rule_round_trips` | entrypoint swap only |
| 418 | `saving_an_unchanged_config_leaves_the_file_byte_identical` | rewrite: seed with `render(&cfg)`, save the same config, assert the file is unchanged |
| 500 | `a_config_without_a_models_section_gains_one_when_the_gui_saves` | keep only "the value survives and reparses" |

Keep untouched: `scratch` (452),
`a_config_that_would_not_load_is_rejected_and_the_file_is_untouched` (459),
`a_rejected_write_leaves_no_temp_file_behind` (472),
`a_valid_write_lands_on_disk_and_reloads` (488).

The `ANNOTATED` fixture (233) survives only as *input* — no test may assert on its bytes
any more. It also carries `port = 8730`, which Task 1 stopped writing.

- [ ] **Step 6: Fix the assertion outside this file**

`crates/yappr-core/src/server.rs:3223`, in
`set_config_writes_the_change_and_reports_that_nothing_needs_a_restart`:

```rust
    assert!(raw.contains("# kommentiert"), "comment lost");
```

Delete that line. The `# kommentiert` marker in the `scratch_config` fixture
(`server.rs:3166`) exists only to feed it; leave the fixture but stop calling it a
preservation fixture in its comment. The rest of the test (a partial patch lands on
disk, `restart_required == false`) is unaffected and is now one of the better
partial-patch regression guards in the suite.

- [ ] **Step 7: Rewrite the module doc**

`config_write.rs:1-18`. Lines 3-13 (the hand-editing premise, `DEFAULT_CONFIG_TOML`
reference, `toml_edit` decor description) go; lines 15-18 (validate-then-rename) stay
and gain a sentence saying that property is about crash-safety, not hand-editing, which
is why it survived the change.

- [ ] **Step 8: Run the gate**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|FAILED"` then
`cargo clippy --workspace --all-targets`
Expected: all pass, clippy clean.

- [ ] **Step 9: Commit**

```bash
git add crates/yappr-core/src/config_write.rs crates/yappr-core/src/server.rs
git commit -m "feat: the config writer applies a patch and renders, instead of merging TOML"
```

---

### Task 3: Path move and migration

**Files:**
- Modify: `crates/yappr-core/src/paths.rs:15` — `config_file()` to the state dir; add `legacy_config_file()`
- Modify: `crates/yappr-core/src/paths.rs:73-78` — the namespacing test
- Modify: `crates/yappr-core/src/config.rs` — add `migrate_from_legacy`
- Test: both files' test modules

**Interfaces:**
- Consumes: `config::render` (Task 1).
- Produces: `paths::legacy_config_file() -> PathBuf` — `~/.config/yappr/config.toml`.
- Produces: `config::migrate_from_legacy(legacy: &Path, current: &Path) -> Result<Option<PathBuf>>`
  — `Ok(Some(renamed_legacy_path))` when a migration happened, `Ok(None)` otherwise.
  Both paths are parameters so tests never touch the real ones.

- [ ] **Step 1: Write the failing tests**

```rust
/// Spec §4. A user upgrading has a real config in the old place; losing it
/// silently would be the worst possible first impression of this change.
#[test]
fn a_legacy_config_is_migrated_and_the_old_file_is_kept_under_a_new_name() {
    let dir = scratch_dir("migrate");
    let legacy = dir.join("old/config.toml");
    let current = dir.join("new/config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "[audio]\nmax_seconds = 45\n").unwrap();

    let moved = migrate_from_legacy(&legacy, &current).unwrap();

    assert!(moved.is_some(), "a migration must be reported");
    assert_eq!(Config::load_from(&current).unwrap().audio.max_seconds, 45);
    assert!(!legacy.exists(), "the legacy file must be renamed out of the way");
    assert!(dir.join("old/config.toml.migrated").exists(), "and never deleted");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The state-dir file is the truth once it exists. A legacy file left behind
/// by a downgrade-and-upgrade must not silently overwrite newer settings.
#[test]
fn migration_is_a_no_op_when_the_new_file_already_exists() {
    let dir = scratch_dir("migrate-noop");
    let legacy = dir.join("old/config.toml");
    let current = dir.join("new/config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::create_dir_all(current.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "[audio]\nmax_seconds = 45\n").unwrap();
    std::fs::write(&current, "[audio]\nmax_seconds = 99\n").unwrap();

    assert!(migrate_from_legacy(&legacy, &current).unwrap().is_none());
    assert_eq!(Config::load_from(&current).unwrap().audio.max_seconds, 99);
    assert!(legacy.exists(), "an ignored legacy file is left exactly as it was");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Spec §4: migrating a broken file would only move the problem. Leave it,
/// and let startup's quarantine (Task 4) handle the new path instead.
#[test]
fn an_unloadable_legacy_config_is_left_alone_rather_than_migrated() {
    let dir = scratch_dir("migrate-broken");
    let legacy = dir.join("old/config.toml");
    let current = dir.join("new/config.toml");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, "[audio]\nnope = 1\n").unwrap();

    assert!(migrate_from_legacy(&legacy, &current).unwrap().is_none());
    assert!(!current.exists(), "nothing may be written from a file that will not load");
    assert!(legacy.exists());

    let _ = std::fs::remove_dir_all(&dir);
}
```

And in `paths.rs`:

```rust
    /// Spec §1: the config is the app's now, so it lives with the app's other
    /// owned state rather than in the directory a user is invited to edit.
    #[test]
    fn the_config_lives_in_the_state_dir_beside_the_wizard_marker() {
        assert!(config_file().ends_with("yappr/config.toml"));
        assert_eq!(config_file().parent(), wizard_marker().parent());
        assert!(legacy_config_file().starts_with(config_dir()));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p yappr-core -- migrate the_config_lives 2>&1 | tail -20`
Expected: FAIL — `cannot find function 'migrate_from_legacy'` / `'legacy_config_file'`.

- [ ] **Step 3: Implement**

`paths.rs`:

```rust
/// Spec §1. The config is written by the app and read by the app; it sits with
/// `wizard-done` and `rejections.jsonl` rather than in `~/.config`, which is
/// the directory a user is invited to edit.
///
/// The consequence, accepted at design time: dotfile-sync setups that cover
/// `~/.config` stop carrying settings between machines.
pub fn config_file() -> PathBuf {
    state_dir().join("config.toml")
}

/// Where the config lived until 2026-09-02. Read exactly once, by
/// `config::migrate_from_legacy` at startup, and never written.
pub fn legacy_config_file() -> PathBuf {
    config_dir().join("config.toml")
}
```

`config_dir()` stays — `hypr.rs` and `autostart_desktop_file` still need it.

`config.rs`:

```rust
/// Moves a pre-2026-09-02 config from `~/.config` into the state dir (spec §4).
///
/// Returns the path the legacy file was renamed to, or `None` when there was
/// nothing to do. Both paths are parameters rather than `paths::` calls so a
/// test can never run this against the real config of whoever ran the suite.
///
/// A legacy file that will not load is deliberately left untouched: migrating
/// it would only move the problem into the new location, where startup's
/// quarantine has to deal with it anyway.
pub fn migrate_from_legacy(legacy: &Path, current: &Path) -> Result<Option<PathBuf>> {
    if current.exists() || !legacy.exists() {
        return Ok(None);
    }
    let Ok(cfg) = Config::load_from(legacy) else {
        return Ok(None);
    };
    if let Some(parent) = current.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(current, render(&cfg))
        .with_context(|| format!("writing {}", current.display()))?;
    let renamed = legacy.with_extension("toml.migrated");
    std::fs::rename(legacy, &renamed)
        .with_context(|| format!("renaming {}", legacy.display()))?;
    Ok(Some(renamed))
}
```

Careful with `with_extension`: on `config.toml` it yields `config.toml.migrated` only if
written as `with_extension("toml.migrated")`. Verify in the test — if it produces
`config.toml.migrated` as expected, keep it; otherwise build the name from
`file_name()`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p yappr-core -- migrate the_config_lives`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yappr-core/src/paths.rs crates/yappr-core/src/config.rs
git commit -m "feat: the config moves to the state dir, with a one-time migration"
```

---

### Task 4: Startup quarantine and the wire

**Files:**
- Modify: `crates/yappr-core/src/config.rs` — add `Quarantine`, `load_or_quarantine`
- Modify: `crates/yappr-core/src/proto.rs:149-230` — `Response.config_notice` + `blank`
- Modify: `crates/yappr-core/src/server.rs` — `Daemon` field (~303), `start()` (608), every `Daemon` literal (655, 3031, 3124), `Status` (1701) and `GetConfig` (1891) handlers
- Test: `config.rs` and `server.rs` test modules

**Interfaces:**
- Consumes: `config::render`, `config::migrate_from_legacy`.
- Produces: `pub struct Quarantine { pub moved_to: PathBuf, pub error: String }`.
- Produces: `pub fn config::load_or_quarantine(path: &Path) -> (Config, Option<Quarantine>)` — infallible.
- Produces: `Response.config_notice: Option<String>`.

- [ ] **Step 1: Write the failing tests**

```rust
/// Invariant 4 rewritten (spec §3). `server::start` loads the config eagerly
/// and `?`s it, and `setup()` in `src-tauri/src/lib.rs` calls that -- so
/// before this, one typo'd key aborted Tauri setup and left the user with no
/// tray, no settings window, and nothing but a text editor to repair it with.
/// That was defensible while the file was a human interface. It is not now.
#[test]
fn an_unloadable_config_is_quarantined_and_startup_gets_defaults() {
    let dir = scratch_dir("quarantine");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "[audio]\nthis_key_does_not_exist = 1\n").unwrap();

    let (cfg, quarantine) = load_or_quarantine(&path);

    assert_eq!(cfg, Config::default(), "startup continues on defaults");
    let q = quarantine.expect("the failure must be reported, not swallowed");
    assert!(q.moved_to.exists(), "the user's file must survive for inspection");
    assert!(q.error.contains("this_key_does_not_exist"), "and name what was wrong");
    assert_eq!(Config::load_from(&path).unwrap(), Config::default(), "a fresh file replaces it");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `validate()` failures are as fatal at startup as parse failures, so they
/// take the same path. `max_seconds = 0` is the case worth naming: invariant
/// 11 makes it the sole terminator of a forgotten recording.
#[test]
fn a_semantically_invalid_config_is_quarantined_too() {
    let dir = scratch_dir("quarantine-validate");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "[audio]\nmax_seconds = 0\n").unwrap();

    let (cfg, quarantine) = load_or_quarantine(&path);
    assert_eq!(cfg.audio.max_seconds, Config::default().audio.max_seconds);
    assert!(quarantine.is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A good config must not be touched, and must not report a notice.
#[test]
fn a_loadable_config_is_returned_untouched_with_no_notice() {
    let dir = scratch_dir("quarantine-clean");
    let path = dir.join("config.toml");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "[audio]\nmax_seconds = 45\n").unwrap();

    let (cfg, quarantine) = load_or_quarantine(&path);
    assert_eq!(cfg.audio.max_seconds, 45);
    assert!(quarantine.is_none());

    let _ = std::fs::remove_dir_all(&dir);
}
```

Plus, in `server.rs`'s test module, a test that `Request::Reload` against a broken file
still reports an error and does **not** quarantine — spec §3's per-call-site table makes
that the point of keeping `load_from` strict.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p yappr-core -- quarantine 2>&1 | tail -20`
Expected: FAIL — `cannot find function 'load_or_quarantine'`.

- [ ] **Step 3: Implement `load_or_quarantine`**

```rust
/// What [`load_or_quarantine`] moved aside, and why.
#[derive(Debug, Clone)]
pub struct Quarantine {
    pub moved_to: PathBuf,
    /// The load error as text. A `String` rather than the `anyhow::Error`
    /// because this is stored behind a `Mutex` for the life of the process and
    /// `anyhow::Error` is not `Clone`.
    pub error: String,
}

/// [`Config::load_from`] for the one caller that must never fail: startup
/// (spec §3).
///
/// Every other caller stays strict on purpose. `Request::Reload` answering
/// "your file is broken" is the honest answer; `Reload` silently resetting a
/// user's settings would not be.
pub fn load_or_quarantine(path: &Path) -> (Config, Option<Quarantine>) {
    match Config::load_from(path) {
        Ok(cfg) => (cfg, None),
        Err(e) => {
            let error = format!("{e:#}");
            let moved_to = match quarantine_file(path) {
                Ok(p) => p,
                // Nothing left to do but carry on with defaults: refusing to
                // start is the failure mode this function exists to remove.
                Err(rename_err) => {
                    return (
                        Config::default(),
                        Some(Quarantine {
                            moved_to: path.to_path_buf(),
                            error: format!("{error} (Datei konnte nicht beiseitegelegt werden: {rename_err})"),
                        }),
                    )
                }
            };
            let _ = std::fs::write(path, render(&Config::default()));
            (Config::default(), Some(Quarantine { moved_to, error }))
        }
    }
}

/// Renames `path` to `<name>.broken-<unix seconds>`, adding a counter if that
/// is somehow taken, so a second quarantine in the same second cannot destroy
/// the first one's evidence.
fn quarantine_file(path: &Path) -> std::io::Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut candidate = dir.join(format!("{name}.broken-{secs}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{name}.broken-{secs}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &candidate)?;
    Ok(candidate)
}
```

- [ ] **Step 4: Wire it into startup**

`server.rs:608`, replacing `let cfg = Config::load().context("loading config")?;`:

```rust
    // Spec §4: a pre-2026-09-02 config in `~/.config` moves here once, before
    // anything reads the new path. Best-effort -- a migration that fails must
    // not stop the app, it just means the user starts from defaults and their
    // old file is still sitting where they left it.
    let migrated = config::migrate_from_legacy(
        &paths::legacy_config_file(),
        &paths::config_file(),
    )
    .unwrap_or(None);

    // Spec §3: never fatal. This is the only caller that quarantines; every
    // other `Config::load_from` stays strict.
    let (cfg, quarantine) = config::load_or_quarantine(&paths::config_file());
```

Then store `quarantine` on the `Daemon` (a new
`config_quarantine: Mutex<Option<Quarantine>>` beside `fatal_error` at `server.rs:303`),
and log `migrated` once after `init_tracing`.

**A new `Daemon` field forces every literal to be updated.** There are three:
`server.rs:655` (production), and the test helpers at `server.rs:3031` and
`server.rs:3124`. Miss one and the crate does not compile, so this is self-checking.

- [ ] **Step 5: Add the wire field**

`proto.rs`, in `Response` after `config_path`:

```rust
    /// `GetConfig` and `Status`: set when startup found a `config.toml` it
    /// could not load, moved it aside and continued on defaults (spec §3).
    /// German, because the settings window renders it verbatim in its notice
    /// banner. `None` on every healthy run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_notice: Option<String>,
```

and `config_notice: None,` in `Response::blank` (`proto.rs:203`). `blank` is the single
construction point, so nothing else needs touching.

Populate it in the `Status` (`server.rs:1701`) and `GetConfig` (`server.rs:1891`)
handlers from `daemon.config_quarantine`, formatted as, e.g.:

```rust
format!(
    "Die Konfigurationsdatei war beschädigt und wurde nach {} verschoben. \
     yappr läuft mit Standardwerten. Grund: {}",
    q.moved_to.display(),
    q.error
)
```

Do **not** reuse `fatal_error`: a quarantine is not fatal, and `Status` reads
`fatal_error` only when the state is `FAILED`. Overloading it would report a working
daemon as broken.

- [ ] **Step 6: Run the gate and commit**

```bash
cargo test --workspace && cargo clippy --workspace --all-targets
git add crates/yappr-core/src/{config.rs,proto.rs,server.rs}
git commit -m "feat: a config that will not load is quarantined, not fatal"
```

---

### Task 5: The settings window shows the notice

**Files:**
- Modify: `src/Settings.tsx:86-96` (the `get_config` response type and `load`), `:61` (state)

**Interfaces:**
- Consumes: `Response.config_notice` from Task 4, arriving through the existing
  `get_config` Tauri command — `settings_cmds::call` passes the whole `Response` JSON
  through, so no Rust-side change is needed here.

- [ ] **Step 1: Implement**

In `load()`, widen the response type and feed the banner that already exists:

```tsx
      const res = (await invoke("get_config")) as {
        config: Section;
        defaults?: Section;
        config_notice?: string;
      };
      setConfig(res.config);
      configRef.current = res.config;
      setDefaults(res.defaults ?? null);
      // Startup found a config.toml it could not load, moved it aside and
      // came up on defaults (spec §3). The user has to be told: their
      // settings are gone from this window's point of view, and the file
      // holding them is somewhere they would never think to look.
      if (res.config_notice) setNotice(res.config_notice);
```

`notice` (`Settings.tsx:61`) and its `banner notice` block (`:496`) already exist and
already carry a "Verstanden" dismiss button. Nothing else is needed.

- [ ] **Step 2: Verify**

Run: `bun run build`
Expected: clean. `tsc` catches the response-type change if it is wrong.

- [ ] **Step 3: Commit**

```bash
git add src/Settings.tsx
git commit -m "feat: the settings window reports a quarantined config"
```

---

### Task 6: Documentation

**Files:**
- Modify: `README.md:203-241` (the Settings and `config.toml` sections), `:220`, `:268`
- Modify: `CLAUDE.md:25` (XDG namespace), invariants 4 and 9, `:379` (paths bullet)

- [ ] **Step 1: README**

- `:220` — "Lives at `~/.config/yappr/config.toml`, created with commented defaults on
  first run. You can edit it by hand; the GUI is careful not to trample it." becomes the
  new path, canonical generation, and the fact that hand edits are overwritten by the
  next save. Keep the section table: it is a useful map of what exists.
- The "Unknown keys are a hard error" callout — rewrite: unknown keys are still
  detected, but the file is moved aside and the app starts on defaults, with the
  settings window saying so.
- `:209` — "Saving rewrites `config.toml` in place: your comments and formatting
  survive" is now false. Replace with the canonical-render behaviour and the byte-stable
  no-op property, which does survive.
- Add a line about migration, so an upgrading user can find their old file.

- [ ] **Step 2: CLAUDE.md**

- Invariant 4 — rewrite: `deny_unknown_fields` stays, consequence is quarantine.
- Invariant 9 — rewrite: the GUI is the only writer; `config_write` renders canonically;
  the byte-identical no-op guarantee survives as a property of the renderer's purity.
- `:25` — the XDG namespace list still says `~/.config/yappr`; add the state dir for the config.
- `:379` — the paths bullet names `config ~/.config/yappr/config.toml`; update.

- [ ] **Step 3: Commit**

```bash
git add README.md CLAUDE.md
git commit -m "docs: config.toml is the app's file now"
```

---

## Self-Review

**Spec coverage:** §1 → Task 3. §2 → Tasks 1 and 2. §2.1 → Task 1. §2.2 → Task 1
(no work needed; recorded as verified). §3 → Task 4. §3.1 → Tasks 4 and 5. §4 → Task 3.
§5 (`--reload` stays) → no work, verified by the existing `Reload` tests plus Task 4's
new "Reload does not quarantine" test. §6 → Task 6. §7 → distributed across Tasks 1-4.
§8 (known gap) → deliberately no task. §9 (out of scope) → deliberately no task.

**Type consistency:** `render(&Config) -> String` is used identically in Tasks 1, 2, 3
and 4. `migrate_from_legacy(&Path, &Path) -> Result<Option<PathBuf>>` matches its call
in Task 4 Step 4. `load_or_quarantine(&Path) -> (Config, Option<Quarantine>)` matches.
`Quarantine { moved_to, error }` field names match between definition and use.
`save_config(&Path, &Value) -> Result<String>` is unchanged, so `server.rs:1937` needs
no edit — stated in Task 2's interfaces and relied on in Task 2 Step 3.

**Open item carried into Task 2:** `Config::validate` is private today. Task 2 Step 3
requires it to be `pub(crate)` and says so inline, with the fallback if that turns out
to be unwanted.

**Open item carried into Task 3:** `Path::with_extension("toml.migrated")` on
`config.toml` needs verifying against the test rather than assumed; the step says so.
