//! Writing `config.toml`.
//!
//! `config.toml` is the app's file, not the user's (spec §1): the settings GUI
//! writes it, nothing invites anyone to edit it, and it is rendered whole from
//! `Config` every time rather than edited in place. That replaced ~400 lines of
//! `toml_edit` merging whose entire purpose was preserving a hand-written
//! file's comments and byte layout.
//!
//! Invariant 9's no-op guarantee survived that deletion for free.
//! `config::render` is a pure function of `Config`, so an unchanged config
//! cannot render two different files -- a stronger property than the old
//! writer's careful leaf-skipping bought, and one nothing here has to work for.
//!
//! What this module still has to be careful about is that **`incoming` is
//! routinely partial**. `Request::SetConfig` passes its JSON straight through,
//! and `wizard_finish`'s `backend_patch` is a single leaf. The old writer met
//! that requirement implicitly, by editing a document that already held every
//! other key; a canonical dump has no such document, so the patch is applied to
//! a loaded `Config` explicitly. Render `incoming` directly and the first
//! wizard-driven save blanks everything it does not name.
//!
//! The write itself is validate-then-rename, unchanged: the rendered document
//! must parse as a `Config` before anything touches the real file, and the real
//! file is then replaced by an atomic rename from a temp file in the same
//! directory. That property is about crash-safety, not hand-editing -- a
//! half-written `config.toml` is still a daemon that will not start -- which is
//! why it outlived everything around it.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;

use crate::config::Config;

/// Deep-merges a JSON patch into a JSON value (spec §2, step 2).
///
/// Three rules, of which the last two are the ones worth stating:
///
/// - objects merge recursively;
/// - arrays replace **wholesale**, because an array is a value, and an
///   element-wise merge would make a shortened list impossible to express;
/// - `null` **removes** the key, because TOML has no null and an absent key
///   *is* how `Option::None` is represented. The settings GUI posts
///   `"styling": null` for a `StyleRule`'s unset axes, so this is a live path,
///   not a defensive one.
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

/// Applies `incoming` to the config at `path`, validates the result, and only
/// then replaces the file.
///
/// Returns the rendered document that was written, so a caller can log or diff
/// it.
pub fn save_config(path: &Path, incoming: &Value) -> Result<String> {
    // The old `toml_edit` writer rejected a non-object update explicitly and
    // this must too. Without it `merge_json`'s fall-through replaces the whole
    // base with the patch, and because every `Config` field is
    // `#[serde(default)]`, a JSON array deserializes cleanly into
    // `Config::default()` -- every setting silently reset, reported as `ok`.
    let Value::Object(_) = incoming else {
        bail!("a config update must be a JSON object");
    };

    // `exists`, not a bare `Config::load_from`: that one *creates* the file
    // when it is absent, and a writer must not create a file as a side effect
    // of deciding what to base a patch on.
    let base = if path.exists() {
        Config::load_from(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        Config::default()
    };

    let mut merged = serde_json::to_value(&base).context("serializing the current config")?;
    merge_json(&mut merged, incoming);

    // Where `deny_unknown_fields` and `validate` run on the change itself, with
    // the file still untouched: the GUI must not be able to produce a daemon
    // that refuses to start.
    let next: Config =
        serde_json::from_value(merged).context("the resulting config is not valid")?;
    next.validate().context("the resulting config is not valid")?;

    let rendered = crate::config::render(&next);

    // Not redundant with the check above. That one rejects a bad patch; this
    // one rejects a renderer that emitted something it cannot read back --
    // a different failure, and the only one that would otherwise reach disk.
    Config::from_str(&rendered).context("the rendered config is not valid")?;

    write_atomically(path, &rendered)?;
    Ok(rendered)
}

/// Temp file in the same directory, then rename. Same filesystem, so the
/// rename is atomic: readers see either the old file or the new one, never a
/// truncated one.
///
/// `pub(crate)` for `config::migrate_from_legacy`, which needs it more than
/// this module does: a torn migration write is unrecoverable, because the
/// half-file makes the new path exist and the migration never runs again.
pub(crate) fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::render;
    use serde_json::json;

    /// A fresh, collision-free scratch directory for a single test. Never
    /// `paths::config_file()`: `save_config` writes, and a test running against
    /// the real path would overwrite the config of whoever ran the suite.
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("yappr-config-write-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Seeds a scratch config file from a `Config`, the way the app itself
    /// would have written it.
    fn seeded(tag: &str, cfg: &Config) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = scratch(tag);
        let path = dir.join("config.toml");
        std::fs::write(&path, render(cfg)).unwrap();
        (dir, path)
    }

    /// The regression a canonical dump makes easy to introduce, and the reason
    /// `save_config` has an explicit patch step at all. `Request::SetConfig`
    /// passes its JSON straight through and `wizard_finish`'s `backend_patch`
    /// is a single leaf -- so a writer that rendered `incoming` directly would
    /// blank every key the patch did not name. On GNOME that is the entire
    /// config, destroyed by the wizard's own last action.
    #[test]
    fn a_partial_patch_leaves_every_key_it_does_not_name_at_its_on_disk_value() {
        let mut seed = Config::default();
        seed.audio.max_seconds = 90;
        seed.asr.num_threads = 7;
        let (dir, path) = seeded("partial-patch", &seed);

        save_config(&path, &json!({"inject": {"backend": "script"}})).unwrap();

        let after = Config::load_from(&path).unwrap();
        assert_eq!(after.inject.backend, crate::config::InjectBackend::Script);
        assert_eq!(after.audio.max_seconds, 90, "an unnamed key keeps its on-disk value");
        assert_eq!(after.asr.num_threads, 7, "it must not fall back to the default");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TOML has no null, and the settings GUI posts `"styling": null` for a
    /// `StyleRule`'s unset axes. Removing the key is what makes it deserialize
    /// back as `Option::None`; writing it would be a value the file cannot
    /// hold. This used to be `merge_into_table`'s explicit `Value::Null` arm.
    #[test]
    fn a_null_in_a_patch_removes_the_key_rather_than_being_rejected() {
        let (dir, path) = seeded("null-removes", &Config::default());

        save_config(
            &path,
            &json!({"style_rules": [{"match_class": "foo", "styling": null}]}),
        )
        .unwrap();

        let after = Config::load_from(&path).unwrap();
        assert_eq!(after.style_rules.len(), 1);
        assert_eq!(after.style_rules[0].styling, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Arrays are values, not documents: a patch naming a shorter list means
    /// the shorter list, never an element-wise merge with what was there.
    #[test]
    fn an_array_in_a_patch_replaces_the_existing_one_wholesale() {
        let mut seed = Config::default();
        seed.vocabulary.terms = vec!["Kubernetes".into(), "Wayland".into()];
        let (dir, path) = seeded("array-replace", &seed);

        save_config(&path, &json!({"vocabulary": {"terms": ["Wayland"]}})).unwrap();

        assert_eq!(Config::load_from(&path).unwrap().vocabulary.terms, vec!["Wayland"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Mutation-testing point that outlived the writer it was written for: an
    /// emptied list must be written as `replacements = []`, not dropped.
    /// `#[serde(default)]` would hide the deletion and the entry would come
    /// back on the next load.
    #[test]
    fn an_emptied_array_of_tables_is_written_as_an_empty_array() {
        let mut seed = Config::default();
        seed.vocabulary.replacements = vec![crate::config::Replacement {
            from: "kdd".into(),
            to: "KDD".into(),
        }];
        let (dir, path) = seeded("emptied-aot", &seed);

        let written = save_config(&path, &json!({"vocabulary": {"replacements": []}})).unwrap();

        // Both halves: the emptied list is *written* as an explicit empty
        // array, and it survives a reload. Asserting only the reload would
        // pass just as happily if the key were dropped from the file, because
        // `#[serde(default)]` would hand back an empty Vec either way.
        assert!(written.contains("replacements = []"), "written:\n{written}");
        assert!(Config::load_from(&path).unwrap().vocabulary.replacements.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A patch naming a section the file happens not to spell out still
    /// applies. Tautological under a canonical dump -- every section is always
    /// emitted -- but it is the shape the settings GUI actually sends, so it
    /// stays pinned.
    #[test]
    fn a_patch_naming_any_section_applies_to_it() {
        let (dir, path) = seeded("any-section", &Config::default());

        save_config(&path, &json!({"models": {"idle_unload_seconds": 30}})).unwrap();

        assert_eq!(Config::load_from(&path).unwrap().models.idle_unload_seconds, 30);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated file has to say so on its first line: it is the only
    /// warning a person who opens it ever gets that their edits will not last.
    #[test]
    fn a_written_config_starts_with_the_generated_by_header() {
        let (dir, path) = seeded("header", &Config::default());
        save_config(&path, &json!({"audio": {"max_seconds": 90}})).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("# Automatisch erzeugt von yappr"), "raw:\n{raw}");
        assert!(raw.contains("Handedits gehen verloren"), "raw:\n{raw}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Invariant 9's no-op guarantee, at the file level rather than the
    /// renderer's: saving a config that changes nothing must leave the file
    /// exactly as it was. A property of `render`'s purity now, not of careful
    /// leaf-skipping.
    #[test]
    fn saving_an_unchanged_config_leaves_the_file_byte_identical() {
        let mut seed = Config::default();
        seed.audio.max_seconds = 90;
        let (dir, path) = seeded("no-op-save", &seed);
        let before = std::fs::read_to_string(&path).unwrap();

        save_config(&path, &serde_json::to_value(&seed).unwrap()).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `StyleRule` with only some axes set is the shape the GUI's table
    /// editor produces, and the one where an `Option` round trip can quietly
    /// go wrong.
    #[test]
    fn a_config_containing_a_partial_style_rule_round_trips() {
        let cfg = Config::from_str(
            "[[style_rules]]\nmatch_class = \"mail.*\"\ncontext = \"email\"\n",
        )
        .unwrap();
        let (dir, path) = seeded("partial-rule", &cfg);

        save_config(&path, &serde_json::to_value(&cfg).unwrap()).unwrap();

        let after = Config::load_from(&path).unwrap();
        assert_eq!(after.style_rules.len(), 1);
        assert_eq!(after.style_rules[0].context, Some(crate::config::Context::Email));
        assert_eq!(after.style_rules[0].styling, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The old `toml_edit` writer rejected a non-object update explicitly, and
    /// dropping that guard with the rest of it was not free: `merge_json`
    /// replaces the base wholesale with anything that is not an object, and
    /// because every `Config` field is `#[serde(default)]` a JSON array
    /// deserializes cleanly into `Config::default()` -- every setting reset,
    /// reported as success.
    #[test]
    fn a_config_update_that_is_not_a_json_object_is_rejected() {
        let mut seed = Config::default();
        seed.audio.max_seconds = 90;
        let (dir, path) = seeded("non-object", &seed);

        for bad in [json!([]), json!([{"audio": {"max_seconds": 1}}]), json!(5), json!(null)] {
            let err = save_config(&path, &bad).unwrap_err();
            assert!(format!("{err:#}").contains("must be a JSON object"), "for {bad}: {err:#}");
        }
        assert_eq!(Config::load_from(&path).unwrap().audio.max_seconds, 90);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Validate-then-rename, the property that outlived the `toml_edit`
    /// machinery around it: a config that would not load is rejected with the
    /// file still untouched. The GUI must not be able to produce a daemon that
    /// refuses to start.
    #[test]
    fn a_config_that_would_not_load_is_rejected_and_the_file_is_untouched() {
        let (dir, path) = seeded("rejected", &Config::default());
        let before = std::fs::read_to_string(&path).unwrap();

        // `max_seconds = 0` fails `validate`, not the parser -- invariant 11
        // makes it the sole terminator of a forgotten recording.
        let err = save_config(&path, &json!({"audio": {"max_seconds": 0}})).unwrap_err();

        assert!(format!("{err:#}").contains("not valid"), "got: {err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown key is a rejection too, not a silently-dropped field: the
    /// config structs are `deny_unknown_fields` (invariant 4) and the writer
    /// runs that check on the merged patch before rendering anything.
    #[test]
    fn a_patch_naming_an_unknown_key_is_rejected() {
        let (dir, path) = seeded("unknown-key", &Config::default());

        let err = save_config(&path, &json!({"audio": {"no_such_key": 1}})).unwrap_err();
        assert!(format!("{err:#}").contains("not valid"), "got: {err:#}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rejected_write_leaves_no_temp_file_behind() {
        let (dir, path) = seeded("no-temp", &Config::default());

        let _ = save_config(&path, &json!({"audio": {"max_seconds": 0}}));

        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_valid_write_lands_on_disk_and_reloads() {
        let (dir, path) = seeded("valid-write", &Config::default());

        save_config(&path, &json!({"audio": {"device": "hw:1,0"}})).unwrap();

        assert_eq!(Config::load_from(&path).unwrap().audio.device, "hw:1,0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file need not exist yet: a first save from the settings GUI on a
    /// fresh install lands on nothing at all, and must produce a complete
    /// config rather than a one-key fragment.
    #[test]
    fn saving_against_a_missing_file_writes_a_complete_config() {
        let dir = scratch("missing-file");
        let path = dir.join("config.toml");
        assert!(!path.exists());

        save_config(&path, &json!({"audio": {"device": "hw:2,0"}})).unwrap();

        let after = Config::load_from(&path).unwrap();
        assert_eq!(after.audio.device, "hw:2,0");
        assert_eq!(after.asr.num_threads, Config::default().asr.num_threads);
        assert_eq!(after.guardrail, Config::default().guardrail);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
