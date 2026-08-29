//! Writing `config.toml` back from the settings GUI without destroying it.
//!
//! `config.toml` is a file the user is invited to edit by hand -- it ships
//! full of explanatory comments (see `config::DEFAULT_CONFIG_TOML`), and the
//! README tells people to open it. Serializing `Config` back with
//! `toml::to_string` would flatten every one of those comments and reorder
//! every section, so a single change made in the GUI would silently cost the
//! user their annotated file.
//!
//! So this edits the existing document in place with `toml_edit`: tables are
//! never replaced, only descended into, and a scalar's `decor` (the whitespace
//! and comments attached to it, including a trailing `# ...` on the same line)
//! is carried across when its value changes.
//!
//! The write itself is validate-then-rename: the rendered document must parse
//! as a `Config` before anything touches the real file, and the real file is
//! then replaced by an atomic rename from a temp file in the same directory. A
//! half-written `config.toml` is a daemon that will not start.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};

use crate::config::Config;

pub fn merge_json_into_toml(existing: &str, incoming: &Value) -> Result<String> {
    let mut doc: DocumentMut =
        existing.parse().context("the existing config.toml is not valid TOML")?;
    let Value::Object(_) = incoming else {
        bail!("a config update must be a JSON object");
    };
    // What the file currently *means*, defaults included. A leaf that already
    // has its incoming value is skipped, so a save writes only what the user
    // actually changed.
    //
    // Without this the GUI -- which always sends the whole config -- would
    // materialise every defaulted key on the first save: a hand-written
    // ten-line config.toml came back as forty, and a top-level
    // `style_rules = []` was hoisted *above* the file's own header comment,
    // because TOML requires bare keys to precede the first table. The user's
    // file has to come back looking like the user's file.
    let current = Config::from_str(existing).ok().and_then(|c| serde_json::to_value(c).ok());
    merge_into_table(doc.as_table_mut(), incoming, current.as_ref())?;
    Ok(doc.to_string())
}

fn merge_into_table(
    table: &mut dyn toml_edit::TableLike,
    json: &Value,
    current: Option<&Value>,
) -> Result<()> {
    let Value::Object(map) = json else {
        bail!("expected a JSON object");
    };
    for (key, value) in map {
        let current_value = current.and_then(|c| c.get(key));
        // Unchanged scalars and arrays: leave the document exactly as the user
        // wrote them, down to the byte.
        if current_value == Some(value) && !matches!(value, Value::Object(_)) {
            continue;
        }
        match value {
            // TOML has no null. An absent key *is* the representation of
            // `Option::None`, which is what a `StyleRule`'s unset axes
            // serialize to, so remove the key rather than refusing the write.
            Value::Null => {
                table.remove(key);
            }
            Value::Object(_) => {
                // A section whose every value is already current needs no
                // table: creating one would add an empty `[section]` header to
                // a file that never had it.
                if current_value == Some(value) && table.get(key).is_none() {
                    continue;
                }
                if table.get(key).and_then(Item::as_table_like).is_none() {
                    table.insert(key, Item::Table(Table::new()));
                }
                let entry = table.get_mut(key).expect("just inserted");
                let sub = entry.as_table_like_mut().expect("ensured above");
                merge_into_table(sub, value, current_value)?;
            }
            Value::Array(items) => {
                let replacement = build_array(table.get(key), items)?;
                set_preserving_decor(table, key, replacement);
            }
            scalar => {
                let v = scalar_to_toml(scalar)
                    .with_context(|| format!("unsupported value for `{key}`"))?;
                set_preserving_decor(table, key, Item::Value(v));
            }
        }
    }
    Ok(())
}

/// Arrays are replaced wholesale -- there is no way to diff "the same" entry
/// across an edit, so any comment written *inside* an array is lost. Comments
/// around it survive, because the item's own decor is carried over.
///
/// The existing item decides the shape: a `[[vocabulary.replacements]]` block
/// stays a block, rather than being collapsed into one long inline array the
/// next time the GUI saves.
fn build_array(existing: Option<&Item>, items: &[Value]) -> Result<Item> {
    let wants_tables = items.iter().all(|i| matches!(i, Value::Object(_)));
    let was_tables = matches!(existing, Some(Item::ArrayOfTables(_)));

    if !items.is_empty() && wants_tables && (was_tables || existing.is_none()) {
        let mut aot = ArrayOfTables::new();
        for item in items {
            let mut t = Table::new();
            merge_into_table(&mut t, item, None)?;
            aot.push(t);
        }
        return Ok(Item::ArrayOfTables(aot));
    }

    let mut array = Array::new();
    for item in items {
        match item {
            Value::Object(_) => {
                let mut inline = InlineTable::new();
                merge_into_table(&mut inline, item, None)?;
                array.push(toml_edit::Value::InlineTable(inline));
            }
            scalar => array.push(scalar_to_toml(scalar)?),
        }
    }
    Ok(Item::Value(toml_edit::Value::Array(array)))
}

/// Replaces a value while keeping the whitespace and comments attached to it.
/// Without this, `max_error_ratio = 0.25   # two wrong characters in eight`
/// loses its explanation the first time the GUI touches it.
///
/// A key/value pair carries its comments in *two* places, and both have to be
/// carried across: the trailing `# ...` on the same line belongs to the value,
/// while comment lines written on their own line *above* the key belong to the
/// key. `toml_edit::TableLike::insert` reformats whichever key it lands on
/// (`Key::fmt()`, which resets the key's decor to its default), so restoring
/// the value's decor alone left every such comment deleted -- including
/// `DEFAULT_CONFIG_TOML`'s "# Cleanup runs on S1-mini by Superwhisper.", an
/// attribution the spec requires, wiped by the user's first toggle of the
/// `[normalize] enabled` switch printed directly beneath it.
fn set_preserving_decor(table: &mut dyn toml_edit::TableLike, key: &str, mut new: Item) {
    let old_value_decor = table.get(key).and_then(Item::as_value).map(|v| v.decor().clone());
    // Only when the key was already a plain key/value pair. A key that came
    // from an `[[array.of.tables]]` *header* carries header spacing -- no
    // trailing space, because a header has no `=` -- and pasting that onto an
    // inline value renders `replacements= []`.
    let old_key_decor = table
        .get(key)
        .and_then(Item::as_value)
        .and(table.key(key))
        .map(|k| (k.leaf_decor().clone(), k.dotted_decor().clone()));
    if let (Some(decor), Some(value)) = (old_value_decor, new.as_value_mut()) {
        *value.decor_mut() = decor;
    }
    table.insert(key, new);
    if let (Some((leaf, dotted)), Some(mut k)) = (old_key_decor, table.key_mut(key)) {
        *k.leaf_decor_mut() = leaf;
        *k.dotted_decor_mut() = dotted;
    }
}

fn scalar_to_toml(value: &Value) -> Result<toml_edit::Value> {
    Ok(match value {
        Value::Bool(b) => (*b).into(),
        Value::String(s) => s.as_str().into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                bail!("number out of range: {n}")
            }
        }
        Value::Null => bail!("null is not representable in TOML"),
        other => bail!("unsupported value: {other}"),
    })
}

/// Merges `incoming` into the config at `path`, validates the result, and only
/// then replaces the file.
///
/// Returns the rendered document that was written, so a caller can log or
/// diff it.
pub fn save_config(path: &Path, incoming: &Value) -> Result<String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::config::DEFAULT_CONFIG_TOML.to_string()
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let rendered = merge_json_into_toml(&existing, incoming)?;

    // The same validation the daemon boots with. A config that would not load
    // is rejected here, with the file still untouched -- the GUI must not be
    // able to produce a daemon that refuses to start.
    Config::from_str(&rendered).context("the resulting config is not valid")?;

    write_atomically(path, &rendered)?;
    Ok(rendered)
}

/// Temp file in the same directory, then rename. Same filesystem, so the
/// rename is atomic: readers see either the old file or the new one, never a
/// truncated one.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
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
    use serde_json::json;

    const ANNOTATED: &str = r#"# yappr configuration

[audio]
device = "default"
max_seconds = 120   # Sicherheitsnetz: beendet eine vergessene Aufnahme

# Cleanup runs on S1-mini.
[normalize]
enabled = true
port = 8730
"#;

    #[test]
    fn changing_a_value_keeps_the_comment_above_its_section() {
        let out = merge_json_into_toml(ANNOTATED, &json!({"normalize": {"port": 9000}})).unwrap();
        assert!(out.contains("# Cleanup runs on S1-mini."), "section comment lost:\n{out}");
        assert!(out.contains("port = 9000"), "value not written:\n{out}");
    }

    #[test]
    fn changing_a_value_keeps_the_trailing_comment_on_its_own_line() {
        let out = merge_json_into_toml(ANNOTATED, &json!({"audio": {"max_seconds": 60}})).unwrap();
        assert!(
            out.contains("max_seconds = 60   # Sicherheitsnetz: beendet eine vergessene Aufnahme"),
            "trailing comment lost:\n{out}"
        );
    }

    #[test]
    fn untouched_keys_are_left_exactly_as_they_were() {
        let out = merge_json_into_toml(ANNOTATED, &json!({"audio": {"max_seconds": 60}})).unwrap();
        assert!(out.contains(r#"device = "default""#), "unrelated key changed:\n{out}");
        assert!(out.contains("port = 8730"));
    }

    #[test]
    fn a_missing_section_is_created() {
        let out = merge_json_into_toml(ANNOTATED, &json!({"vocabulary": {"enabled": false}}))
            .unwrap();
        let parsed = Config::from_str(&out).unwrap();
        assert!(!parsed.vocabulary.enabled);
    }

    #[test]
    fn a_string_array_is_replaced_wholesale() {
        let out = merge_json_into_toml(
            ANNOTATED,
            &json!({"vocabulary": {"terms": ["Hyprland", "Parakeet"]}}),
        )
        .unwrap();
        let parsed = Config::from_str(&out).unwrap();
        assert_eq!(parsed.vocabulary.terms, ["Hyprland", "Parakeet"]);
    }

    /// `[[vocabulary.replacements]]` must not collapse into a single inline
    /// array the first time the GUI saves -- the user still has to read this
    /// file.
    #[test]
    fn an_existing_array_of_tables_stays_an_array_of_tables() {
        let existing = "[[vocabulary.replacements]]\nfrom = \"a\"\nto = \"b\"\n";
        let out = merge_json_into_toml(
            existing,
            &json!({"vocabulary": {"replacements": [{"from": "SQUI", "to": "GUI"}]}}),
        )
        .unwrap();
        assert!(out.contains("[[vocabulary.replacements]]"), "collapsed to inline:\n{out}");
        let parsed = Config::from_str(&out).unwrap();
        assert_eq!(parsed.vocabulary.replacements[0].from, "SQUI");
    }

    /// The assertion on the *rendered file* is the point, not just on the
    /// parsed result: dropping the key entirely would also parse as an empty
    /// list (`#[serde(default)]`), so a test that only checked the parse would
    /// pass for an implementation that silently deleted the section. Mutation
    /// testing caught exactly that.
    #[test]
    fn an_emptied_array_of_tables_is_written_as_an_empty_array() {
        let existing = "[[vocabulary.replacements]]\nfrom = \"a\"\nto = \"b\"\n";
        let out =
            merge_json_into_toml(existing, &json!({"vocabulary": {"replacements": []}})).unwrap();
        assert!(
            !out.contains("[[vocabulary.replacements]]"),
            "the old entry survived:\n{out}"
        );
        assert!(out.contains("replacements = []"), "not written as an empty array:\n{out}");
        let parsed = Config::from_str(&out).unwrap();
        assert!(parsed.vocabulary.replacements.is_empty(), "not cleared:\n{out}");
    }

    #[test]
    fn changing_an_inject_setting_keeps_the_comments_documenting_the_section() {
        // DEFAULT_CONFIG_TOML annotates `backend` with the three spellings it
        // accepts. Invariant 9: a save that changes one setting must produce a
        // one-line diff -- it must not strip the annotation the user relies on
        // to know what else they could have written there.
        let out = merge_json_into_toml(
            crate::config::DEFAULT_CONFIG_TOML,
            // Both decors at once: the choices comment trails `backend` on
            // its own line, the ydotoold note sits between `backend` and
            // `trailing_space`, and each is changed here.
            &json!({"inject": {"backend": "ydotool", "trailing_space": false}}),
        )
        .unwrap();
        assert!(out.contains(r#"backend = "ydotool""#), "not written:\n{out}");
        assert!(
            out.contains(r#"# "wtype" | "ydotool" | "clipboard""#),
            "annotation lost:\n{out}"
        );
        assert!(out.contains("# ydotool needs a running ydotoold"), "note lost:\n{out}");
        assert_eq!(
            Config::from_str(&out).unwrap().inject.backend,
            crate::config::InjectBackend::Ydotool
        );
    }

    #[test]
    fn changing_a_value_keeps_the_comment_lines_above_it() {
        // `set_preserving_decor` used to carry only the *value*'s decor across
        // -- the trailing `# ...` on the same line. A comment written on its
        // own line above a key belongs to the *key*'s decor, and `Table::insert`
        // replaces the whole key/value pair, so every such comment was dropped
        // the first time the GUI touched the key beneath it. In the shipped
        // DEFAULT_CONFIG_TOML that included "# Cleanup runs on S1-mini by
        // Superwhisper." above `normalize.enabled` -- an attribution the
        // README and spec require, deleted by the user's first toggle.
        let out = merge_json_into_toml(
            crate::config::DEFAULT_CONFIG_TOML,
            &json!({"normalize": {"enabled": false}}),
        )
        .unwrap();
        assert!(out.contains("enabled = false"), "not written:\n{out}");
        assert!(
            out.contains("# Cleanup runs on S1-mini by Superwhisper."),
            "attribution lost:\n{out}"
        );
    }

    #[test]
    fn a_full_config_round_trips_through_the_writer() {
        let original = Config::from_str(crate::config::DEFAULT_CONFIG_TOML).unwrap();
        let as_json = serde_json::to_value(&original).unwrap();
        let out = merge_json_into_toml(crate::config::DEFAULT_CONFIG_TOML, &as_json).unwrap();
        assert_eq!(Config::from_str(&out).unwrap(), original);
    }

    /// `StyleRule`'s axes are `Option`s, so a rule that sets only `context`
    /// serializes with `"styling": null`. TOML has no null: the key must be
    /// absent, not present-and-empty. Without this, a user with a single
    /// style rule could not save anything at all from the GUI -- their whole
    /// config would be unwritable because of one unset field.
    #[test]
    fn a_null_is_written_as_an_absent_key_rather_than_rejected() {
        let existing = "[[style_rules]]\nmatch_class = \"old\"\nstyling = \"casual\"\n";
        let out = merge_json_into_toml(
            existing,
            &json!({"style_rules": [
                {"match_class": "(?i)thunderbird", "styling": null, "structure": null, "context": "email"}
            ]}),
        )
        .unwrap();
        let parsed = Config::from_str(&out).unwrap();
        assert_eq!(parsed.style_rules[0].match_class, "(?i)thunderbird");
        assert_eq!(parsed.style_rules[0].styling, None);
        assert_eq!(parsed.style_rules[0].context, Some(crate::config::Context::Email));
    }

    /// The round trip that matters for the GUI: read the config, send it
    /// straight back unchanged, get the same config. A `null` mishandled
    /// anywhere breaks this for every user who has a style rule.
    #[test]
    fn a_config_containing_a_partial_style_rule_round_trips() {
        let source = r#"
[[style_rules]]
match_class = "(?i)thunderbird"
context = "email"
"#;
        let original = Config::from_str(source).unwrap();
        let as_json = serde_json::to_value(&original).unwrap();
        let out = merge_json_into_toml(source, &as_json).unwrap();
        assert_eq!(Config::from_str(&out).unwrap(), original);
    }

    /// The settings GUI always sends the *whole* config, so this is what
    /// pressing Save without changing anything must do: nothing.
    #[test]
    fn saving_an_unchanged_config_leaves_the_file_byte_identical() {
        let whole = serde_json::to_value(Config::from_str(ANNOTATED).unwrap()).unwrap();
        let out = merge_json_into_toml(ANNOTATED, &whole).unwrap();
        assert_eq!(out, ANNOTATED);
    }

    /// And this is what changing one setting must do: touch one line. The
    /// user's hand-written file must not be expanded into a full dump of every
    /// default just because the GUI knows them all.
    #[test]
    fn saving_one_change_adds_only_that_one_setting() {
        let mut whole = serde_json::to_value(Config::from_str(ANNOTATED).unwrap()).unwrap();
        whole["normalize"]["port"] = serde_json::json!(9000);

        let out = merge_json_into_toml(ANNOTATED, &whole).unwrap();

        assert_eq!(out, ANNOTATED.replace("port = 8730", "port = 9000"));
    }

    /// The specific damage seen end-to-end before the minimal-diff rule: a
    /// top-level array has to precede the first table in TOML, so writing an
    /// empty `style_rules` the user never had displaced the file's own header
    /// comment to the second line.
    #[test]
    fn an_unset_top_level_array_is_not_materialised_above_the_header_comment() {
        let whole = serde_json::to_value(Config::from_str(ANNOTATED).unwrap()).unwrap();
        let out = merge_json_into_toml(ANNOTATED, &whole).unwrap();
        assert!(!out.contains("style_rules"), "materialised an unset array:\n{out}");
        assert!(
            out.starts_with("# yappr configuration"),
            "the header comment was displaced:\n{out}"
        );
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("yappr-config-write-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn a_config_that_would_not_load_is_rejected_and_the_file_is_untouched() {
        let path = scratch("reject");
        std::fs::write(&path, ANNOTATED).unwrap();

        // max_error_ratio above 1.0 is a `Config::validate` failure.
        let err = save_config(&path, &json!({"vocabulary": {"max_error_ratio": 5.0}})).unwrap_err();
        assert!(err.to_string().contains("not valid"), "got: {err:#}");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), ANNOTATED, "the file was modified");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_rejected_write_leaves_no_temp_file_behind() {
        let path = scratch("no-temp");
        std::fs::write(&path, ANNOTATED).unwrap();
        let _ = save_config(&path, &json!({"vocabulary": {"max_error_ratio": 5.0}}));

        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_valid_write_lands_on_disk_and_reloads() {
        let path = scratch("ok");
        std::fs::write(&path, ANNOTATED).unwrap();

        save_config(&path, &json!({"audio": {"device": "hw:1,0"}})).unwrap();

        let reloaded = Config::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.audio.device, "hw:1,0");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_config_without_a_models_section_gains_one_when_the_gui_saves() {
        // Every existing user's file looks like this. The GUI must be able to
        // write a setting whose whole section is missing from the document.
        let original = "[audio]\ndevice = \"default\"\n";
        let mut cfg = Config::from_str(original).unwrap();
        cfg.models.idle_unload_seconds = 30;
        let as_json = serde_json::to_value(&cfg).unwrap();

        let out = merge_json_into_toml(original, &as_json).unwrap();

        assert!(out.contains("[models]"), "section not created:\n{out}");
        assert!(out.contains("idle_unload_seconds = 30"), "value not written:\n{out}");
        // The rendered result must still be a config the daemon accepts.
        assert_eq!(Config::from_str(&out).unwrap().models.idle_unload_seconds, 30);
    }

    #[test]
    fn changing_the_idle_timeout_produces_a_one_line_diff() {
        // Invariant 9: `config.toml` is a file the user is invited to edit by
        // hand, so a save that changes one setting must not rewrite the document.
        let original = crate::config::DEFAULT_CONFIG_TOML;
        let mut cfg = Config::from_str(original).unwrap();
        cfg.models.idle_unload_seconds = 300;
        let as_json = serde_json::to_value(&cfg).unwrap();

        let out = merge_json_into_toml(original, &as_json).unwrap();

        let changed: Vec<_> = original
            .lines()
            .zip(out.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed.len(), 1, "expected exactly one changed line, got {changed:?}");
        assert_eq!(changed[0].1.trim(), "idle_unload_seconds = 300");
    }
}
