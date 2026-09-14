//! Which desktop environment this session is, and what the user has to paste
//! into it to get a dictation shortcut.
//!
//! Lives here rather than in `src-tauri` for the same reason `hypr.rs` does:
//! it is knowledge about the surrounding desktop, and it is testable without
//! a GUI. The setup wizard (`src-tauri/src/wizard.rs`) is the only caller.
//!
//! yappr supports **Hyprland and GNOME**. Everything else resolves to
//! [`Desktop::Other`], which is a degraded path -- generic instructions
//! naming the two commands to bind -- not a refusal.

use serde::Serialize;

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
    /// The name a human reads, in the wizard's UI.
    pub fn display(&self) -> &str {
        match self {
            Desktop::Hyprland => "Hyprland",
            Desktop::Gnome => "GNOME",
            Desktop::Other(name) => name,
            Desktop::Unknown => "unknown",
        }
    }

    /// The stable key that crosses the wire to the wizard's TypeScript.
    /// Deliberately not derived from [`Desktop::display`]: `Other`'s display
    /// name is arbitrary user-environment text, and the frontend must switch
    /// on a closed set.
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
    // Hyprland sets this for every client it launches, so it is evidence
    // rather than configuration -- it outranks an `XDG_CURRENT_DESKTOP` that
    // says something else, which is what a stale display-manager profile
    // looks like.
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
            name: "yappr: start/stop dictation".into(),
            command: "yappr --toggle".into(),
            keys: "Super+D".into(),
        },
        ShortcutBinding {
            name: "yappr: cancel dictation".into(),
            command: "yappr --cancel".into(),
            keys: "Super+Alt+D".into(),
        },
    ]
}

/// The `gsettings` route for GNOME, offered *behind* the GUI instructions.
///
/// The obvious one-liner (`gsettings set ... custom-keybindings "[...]"`)
/// **overwrites** the list and silently destroys every custom shortcut the
/// user already had. The `case` below reads the current value, leaves it
/// alone if ours is already in it, and otherwise appends -- so it is safe to
/// paste on a machine that has custom shortcuts, and safe to run twice.
/// `@as []` is what `gsettings get` prints for an empty list; the bare `[]`
/// arm covers versions that print that instead.
const GNOME_GSETTINGS_SNIPPET: &str = r#"BASE=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings
SCHEMA=org.gnome.settings-daemon.plugins.media-keys.custom-keybinding

gsettings set $SCHEMA:$BASE/yappr-toggle/ name    'yappr: start/stop dictation'
gsettings set $SCHEMA:$BASE/yappr-toggle/ command 'yappr --toggle'
gsettings set $SCHEMA:$BASE/yappr-toggle/ binding '<Super>d'

gsettings set $SCHEMA:$BASE/yappr-cancel/ name    'yappr: cancel dictation'
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
yappr --toggle    # start/stop dictation
yappr --cancel    # cancel dictation
";

/// What the wizard's shortcut step shows for `d`.
///
/// Nothing here writes anything. Every desktop gets text to paste, never an
/// action taken on the user's behalf -- a standing project rule for Hyprland
/// (a bad window rule breaks their desktop) and, for the reason above, for
/// GNOME too.
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
                "Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `XDG_CURRENT_DESKTOP` is a colon-separated *list* (`ubuntu:GNOME` is a
    /// real value), the Hyprland signature outranks it, and an exported-but-
    /// empty variable is not evidence of anything.
    #[test]
    fn the_session_environment_resolves_to_the_right_desktop() {
        assert_eq!(detect_from(Some("abc123"), Some("GNOME"), None), Desktop::Hyprland);
        assert_eq!(detect_from(Some(""), Some("GNOME"), None), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("ubuntu:GNOME"), None), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("hyprland"), None), Desktop::Hyprland);
        assert_eq!(detect_from(None, None, Some("gnome")), Desktop::Gnome);
        assert_eq!(detect_from(None, Some("KDE"), None), Desktop::Other("KDE".into()));
        assert_eq!(detect_from(None, None, None), Desktop::Unknown);
    }

    /// The kind and the target file must agree: naming `bindings.lua` while
    /// emitting `.conf` syntax is worse than naming neither.
    #[test]
    fn hyprland_gets_the_snippet_hypr_already_emits_and_a_matching_target() {
        let it = shortcut_instructions(&Desktop::Hyprland);
        assert_eq!(it.snippet, crate::hypr::shortcut_config());
        assert!(it.bindings.is_empty(), "the snippet is authoritative on Hyprland");
        let target = it.target.expect("Hyprland always names a file to paste into");
        match it.kind {
            ShortcutKind::HyprLua => assert!(target.ends_with("bindings.lua"), "got {target}"),
            ShortcutKind::HyprConf => assert!(target.ends_with("hyprland.conf"), "got {target}"),
            other => panic!("Hyprland must not resolve to {other:?}"),
        }
    }

    /// The one thing the GNOME snippet must never do is replace the user's
    /// existing custom keybindings.
    #[test]
    fn the_gnome_snippet_appends_to_custom_keybindings_rather_than_replacing_them() {
        let it = shortcut_instructions(&Desktop::Gnome);
        assert_eq!(it.kind, ShortcutKind::Gnome);
        assert_eq!(it.bindings.len(), 2);
        assert!(it.snippet.contains("CUR=$(gsettings get"), "must read the current list first");
        assert!(it.snippet.contains("${CUR%]}"), "must append to it");
        assert!(it.snippet.contains("*yappr-toggle*"), "must be idempotent when re-run");
    }

    /// An unsupported desktop is a degraded path, not a dead end.
    #[test]
    fn an_unsupported_desktop_still_gets_the_two_commands() {
        for d in [Desktop::Other("KDE".into()), Desktop::Unknown] {
            let it = shortcut_instructions(&d);
            assert_eq!(it.kind, ShortcutKind::Generic);
            assert_eq!(it.bindings.len(), 2);
            assert!(it.snippet.contains("yappr --toggle"));
            assert!(it.target.is_none());
        }
    }
}
