use std::process::Command;
use std::time::Duration;

use crate::procutil;

/// I3: `hyprctl` runs on the single-threaded accept loop's behalf at every
/// `ptt-start`; a hung `hyprctl` (compositor wedged, IPC socket stuck) must
/// not be able to block that loop -- and every command it serves, including
/// `status` -- forever.
const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(3);

/// Hyprland keybindings and window rules, in Omarchy's Lua config format --
/// spec §3 (shortcuts), §6 (the overlay window), §10 (migration).
///
/// Hyprland 0.56+ configured through Lua rejects the legacy keyword parser
/// outright (`hyprctl keyword windowrule ...` answers "keyword can't work
/// with non-legacy parsers"), so the `.conf` block below is useless on such
/// a system and this is what those users need instead.
///
/// ## Migration (spec §10)
///
/// This has now broken twice, and the emitted text has to carry both
/// breaks, because a user's Hyprland config may be stale by either
/// generation. First, `owf-ctl` stopped existing: autostart became the
/// Settings window's "Beim Anmelden starten" toggle (spec §9) rather than a
/// Hyprland line, and hold-to-talk's release-edge binding (the paired
/// `{ release = true }` `o.bind` call; `bindr` in the classic format) lost
/// its replacement under press/press toggle (spec §3) -- there is nothing
/// to rewrite it *to*. Then the app itself was renamed from
/// `openwhisprflow` to `yappr`, which invalidates the *second* generation
/// of that block wholesale: its binds call a binary that is no longer
/// installed under that name, and its window rule matches a title
/// (`openwhisprflow overlay`) no window carries any more. Neither failure
/// is visible -- a bind to a missing binary is silent, and a window rule
/// that matches nothing is inert. So this text leads with what to delete,
/// per spec §10, naming both dead binaries rather than silently leaving
/// stale lines behind.
///
/// ## Window rules
///
/// Two requirements, unchanged since M2:
///
/// 1. **Never take keyboard focus** -- otherwise `wtype` types the dictation
///    into the overlay instead of the user's target window (invariant 2).
/// 2. **Be positioned bottom-centre** (invariant 5) -- Wayland's `xdg_shell`
///    has no client-settable window position, so placement can only come
///    from the compositor.
///
/// `src-tauri/src/layer.rs`'s `anchor_overlay` now satisfies both directly,
/// with no window rule at all, via `wlr-layer-shell` -- but only on a
/// compositor that implements that protocol (every wlroots compositor;
/// not Mutter). The rule below is what a GNOME/Mutter session falls back to
/// (spec §6), so it is emitted unconditionally: this text is generated once
/// by `--print-shortcuts`, ahead of time, with no way to know which of the
/// two a given run will land on. It costs nothing on the compositors that
/// don't need it.
///
/// It is now keyed on the overlay's **title**, not its class. Since the
/// settings window became a window of this same Tauri app (Task 9), it
/// shares the app's class -- a class-matched rule would make the settings
/// form unable to take a keystroke too, which is exactly the class-collision
/// bug the previous two-app split existed to avoid (spec §12 item 2). The
/// overlay's title, `"yappr overlay"`, comes from its window
/// declaration in `src-tauri/tauri.conf.json`; the settings window's title
/// there, `"yappr – Einstellungen"`, does not match the regex below,
/// so the rule reaches only the overlay.
///
/// Every rule *effect* below is the exact Lua spelling registered by the
/// *installed* Hyprland 0.56.2 binary, not a remembered one -- unchanged
/// from the class-matched version this replaces. The ground truth is
/// `WINDOW_RULE_EFFECT_DESCS` in
/// `/usr/include/hyprland/src/config/lua/bindings/LuaBindingsInternal.hpp`
/// (name string -> `eWindowRuleEffect`), which the `o.window` global
/// (`registerToplevelBindings`, present as a symbol in the `Hyprland` binary
/// itself) is built from:
///   - `{"float", ..., WE::WINDOW_RULE_EFFECT_FLOAT}` (line 50)
///   - `{"pin", ..., WE::WINDOW_RULE_EFFECT_PIN}` (line 57)
///   - `{"no_focus", ..., WE::WINDOW_RULE_EFFECT_NO_FOCUS}` (line 90)
///   - `{"no_dim", ..., WE::WINDOW_RULE_EFFECT_NO_DIM}` (line 89)
///   - `{"border_size", ..., WE::WINDOW_RULE_EFFECT_BORDER_SIZE}` (line 69)
///   - `{"move", ..., WE::WINDOW_RULE_EFFECT_MOVE}` (line 59), taking a
///     `CLuaConfigExpressionVec2` -- a `{x, y}` table of expression strings.
///
/// The **selector** -- `o.window({ title = ... }, { ... })` rather than a
/// bare class string -- is confirmed by real, currently-shipping Omarchy Lua
/// rules on this machine that key on `title` the same way:
///   - `/usr/share/omarchy/default/hypr/apps/pip.lua:2` --
///     `o.window({ title = "(Picture.?in.?[Pp]icture)" }, { tag = "+pip" })`
///   - `/usr/share/omarchy/default/hypr/apps/battlenet.lua:4` --
///     `o.window({ class = "^steam_app_battlenet$", title = "^Battle\\.net$" }, { ... })`
///   - `/usr/share/omarchy/default/hypr/apps/steam.lua:2` --
///     `o.window({ class = "steam", title = "Steam" }, { center = true, ... })`
///
/// `pin` is included because the overlay window is created once, hidden, at
/// startup and only shown/hidden after that -- without `pin` it would stay
/// bound to whatever workspace was active at that moment and silently fail
/// to appear if the user dictates from a different one. `no_dim` exists
/// because `no_focus` makes the window permanently "inactive", which would
/// make it a target for `decoration.dim_inactive` if a user ever turns that
/// on. `border_size = 0` matches the overlay's transparent, decoration-less
/// pill -- without it Hyprland would still draw its own compositor border
/// around the window regardless of the Tauri window's own
/// `decorations: false`.
pub const SHORTCUT_CONFIG_LUA: &str = r#"-- yappr dictation shortcuts and window rules.
--
-- Delete first, wherever they currently live (bindings.lua, autostart.lua,
-- windows.lua): every line naming `openwhisprflow` or `owf-ctl`. Neither
-- binary exists any more -- the app is now `yappr`. Concretely that is the
-- old owf-ctl bind pair (the "ptt-start" press binding and its paired
-- "ptt-stop" release binding), the later
-- o.bind(..., "openwhisprflow --toggle") / ("openwhisprflow --cancel")
-- pair, both o.launch_on_start(...) lines ("owf-ctl daemon" and
-- "openwhisprflow"), the o.window("openwhisprflow", { ... }) rule keyed on
-- the app's class, and its title-keyed successor
-- o.window({ title = "^openwhisprflow overlay$" }, { ... }).
-- Autostart is now the Settings window's "Beim Anmelden starten" toggle,
-- not a Hyprland line; and the class-matched rule also reaches the settings
-- window, which then cannot take a keystroke.
--
-- Add to ~/.config/hypr/bindings.lua. Press-only: there is no release-edge
-- counterpart to pair either bind with.
o.bind("SUPER + D", "Dictation: toggle", "yappr --toggle")
o.bind("SUPER + ALT + D", "Dictation: cancel", "yappr --cancel")

-- Add to ~/.config/hypr/windows.lua. Keyed on the overlay's title, not its
-- class -- see this module's doc comment for why, and for why this rule is
-- emitted even when the layer-shell surface (which needs no window rule at
-- all) is available.
o.window({ title = "^yappr overlay$" }, {
  float = true,
  pin = true,
  no_focus = true,
  no_dim = true,
  border_size = 0,
  move = { "(monitor_w/2-window_w/2)", "(monitor_h-window_h-40)" },
})
"#;

/// Hyprland keybindings and window rules, in the classic `.conf` format.
///
/// For installations that still use `hyprland.conf` (Hyprland 0.56.2 still
/// depends on `libhyprlang`, so this parser is retained even though the
/// upstream wiki has moved its documentation to the Lua syntax). See
/// [`SHORTCUT_CONFIG_LUA`] for the Lua equivalent and the full reasoning
/// behind every line below.
///
/// This machine configures Hyprland in Lua, so these classic `windowrulev2`
/// lines can't be exercised here the way the Lua ones were checked against
/// this compositor's own installed binary. The rule *effects* are unchanged
/// from the class-matched version this replaces, so the same verification
/// stands (real, currently-published classic configs rather than memory):
///   - `nofocus` / `noinitialfocus`: a GitHub Hyprland discussion
///     (hyprwm/Hyprland#13141) and issue (#8136) both show
///     `windowrulev2 = noinitialfocus, class:^(jetbrains-.)$` and
///     `windowrulev2 = nofocus, class:^(.*jetbrains.*)$, title:^(win.*)$`
///     used together on a never-should-focus window -- the second of these
///     is also the standing evidence that `title:` is a real, independent
///     selector key the classic parser accepts, which is what the rule
///     below relies on (with no `class:` at all -- Hyprland's windowrulev2
///     selector keys, unlike this project's own rule effects, are
///     documented as freely combinable/omittable, not a fixed set that
///     needs one-by-one verification the way an *effect* keyword does).
///   - `float` / `pin` / `move <x> <y>`: end-4/dots-hyprland's shipped
///     `rules.conf` (commit `41520aebc6f0bd5fe4e10e32e08f4817bce321c0`) pins
///     and floats a picture-in-picture window with
///     `windowrulev2 = move 73% 72%,title:...`,
///     `windowrulev2 = float,title:...`, `windowrulev2 = pin,title:...` --
///     already title-matched in the source it was cited from.
///   - `bordersize 0`: cited from a real `hyprland.conf` snippet,
///     `windowrule = bordersize 0, floating:0, onworkspace:w[tv1]`.
///   - the `(monitor_w-1000)`-style parenthesised expression form of `move`
///     (used below for centring) is confirmed by a separate real example,
///     `windowrulev2 = move (monitor_w-1000) (monitor_h-1000), ...`.
///
/// Every rule-effect keyword also matches, one-to-one modulo underscores,
/// the same internal `eWindowRuleEffect` table the Lua bindings use
/// (`WINDOW_RULE_EFFECT_NO_FOCUS`, `_NOINITIALFOCUS`, `_FLOAT`, `_PIN`,
/// `_BORDER_SIZE`, `_MOVE` in
/// `/usr/include/hyprland/src/desktop/rule/windowRule/WindowRuleEffectContainer.hpp`),
/// which is the strongest available cross-check without a classic-conf
/// Hyprland session on hand to test against directly. `no_dim` is left out of
/// this block because it has no independently confirmed classic spelling.
pub const SHORTCUT_CONFIG_CONF: &str = r#"# yappr dictation shortcuts and window rules.
#
# Delete first: every line naming `openwhisprflow` or `owf-ctl`. Neither
# binary exists any more -- the app is now `yappr`. Concretely that is the
# old exec-once lines (owf-ctl daemon, openwhisprflow), the old owf-ctl
# bind/bindr pair (ptt-start / ptt-stop), the later
# `exec, openwhisprflow --toggle` / `--cancel` binds, and every
# windowrulev2 line matched on class:^(openwhisprflow)$ or on
# title:^(openwhisprflow overlay)$. Autostart is now the Settings window's
# "Beim Anmelden starten" toggle, not a Hyprland line; and the
# class-matched rule also reaches the settings window, which then cannot
# take a keystroke.
#
# Press-only: there is no release-edge counterpart to pair either bind with.
bind  = SUPER, D,     exec, yappr --toggle
bind  = SUPER ALT, D, exec, yappr --cancel

# Keyed on the overlay's title, not its class -- see SHORTCUT_CONFIG_LUA's
# doc comment for why, and for why this rule is emitted even when the
# layer-shell surface (which needs no window rule at all) is available.
windowrulev2 = float,title:^(yappr overlay)$
windowrulev2 = pin,title:^(yappr overlay)$
windowrulev2 = nofocus,title:^(yappr overlay)$
windowrulev2 = noinitialfocus,title:^(yappr overlay)$
windowrulev2 = bordersize 0,title:^(yappr overlay)$
windowrulev2 = move (monitor_w/2-window_w/2) (monitor_h-window_h-40),title:^(yappr overlay)$
"#;

/// True when this machine configures Hyprland in Lua rather than `.conf`.
fn uses_lua_config(config_dir: &std::path::Path) -> bool {
    config_dir.join("hyprland.lua").exists()
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

/// Whether *this machine* configures Hyprland in Lua. Public because
/// `desktop.rs` has to name the file the user pastes into (`bindings.lua` vs
/// `hyprland.conf`), and that answer must come from the same probe
/// [`shortcut_config`] branches on -- two probes could disagree, and then the
/// wizard would name a file whose syntax does not match the text above it.
pub fn uses_lua_config_here() -> bool {
    let dir = dirs::config_dir()
        .map(|d| d.join("hypr"))
        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent"));
    uses_lua_config(&dir)
}

fn parse_class(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let class = v.get("class")?.as_str()?;
    if class.is_empty() {
        None
    } else {
        Some(class.to_string())
    }
}

/// The class of the currently focused window, or `None` when Hyprland is
/// unavailable or nothing is focused.
///
/// Called from `start_recording` (`server.rs`) when a recording begins, off
/// the latency-critical path. `Request::PttStart` still exists internally
/// and is still what triggers this call -- but there is no longer an
/// `owf-ctl ptt-start` route that sends it directly: `Request::Toggle`
/// resolves to it when the daemon is `IDLE` (see spec §2, "Toggle
/// semantics"), which is the only way a user-facing action reaches here now.
pub fn active_window_class() -> Option<String> {
    let mut cmd = Command::new("hyprctl");
    cmd.args(["-j", "activewindow"]);
    let out = procutil::run_with_timeout(cmd, HYPRCTL_TIMEOUT, None).ok()?;
    if !out.status.success() {
        tracing::debug!("hyprctl activewindow failed; using default style");
        return None;
    }
    parse_class(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of `config` that are executable directives rather than
    /// comments. Both migration headers deliberately *quote* dead syntax
    /// (`owf-ctl ptt-stop`, `launch_on_start`, the old class-matched
    /// selector) so a user can find and delete it -- spec §10. A test for
    /// "this dead thing is truly gone" must look only at the lines that
    /// would actually run, not at prose telling the user to remove it.
    fn active_lines<'a>(config: &'a str, comment_prefix: &str) -> Vec<&'a str> {
        config
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.is_empty() && !l.starts_with(comment_prefix))
            .collect()
    }

    #[test]
    fn parses_the_class_from_hyprctl_json() {
        let json = r#"{"address":"0x1","class":"thunderbird","title":"Inbox"}"#;
        assert_eq!(parse_class(json).as_deref(), Some("thunderbird"));
    }

    #[test]
    fn an_empty_class_is_treated_as_absent() {
        assert_eq!(parse_class(r#"{"class":""}"#), None);
    }

    #[test]
    fn missing_or_malformed_json_yields_none() {
        assert_eq!(parse_class("{}"), None);
        assert_eq!(parse_class("not json"), None);
        // hyprctl prints this when nothing is focused.
        assert_eq!(parse_class("Invalid"), None);
    }

    #[test]
    fn the_lua_config_binds_toggle_and_cancel_with_no_release_edge() {
        // Rev 3 dropped hold-to-talk (spec §3): both binds are press-only,
        // so a `{ release = true }` counterpart must never reappear here.
        assert!(SHORTCUT_CONFIG_LUA
            .contains(r#"o.bind("SUPER + D", "Dictation: toggle", "yappr --toggle")"#));
        assert!(SHORTCUT_CONFIG_LUA.contains(
            r#"o.bind("SUPER + ALT + D", "Dictation: cancel", "yappr --cancel")"#
        ));
        for line in active_lines(SHORTCUT_CONFIG_LUA, "--") {
            assert!(!line.contains("release"), "a release-edge binding leaked in: {line:?}");
        }
    }

    #[test]
    fn neither_config_names_a_deleted_binary_on_a_line_it_tells_the_user_to_add() {
        // Two binaries have now been retired: `owf-ctl`, and
        // `openwhisprflow` itself (renamed to `yappr`). Both still appear
        // legitimately in the "delete this" migration comments, which is
        // exactly why this checks the *active* lines -- the ones the user
        // pastes into a live config -- rather than the whole text. A bind
        // to a missing binary fails silently, so a leak here is invisible
        // in use.
        for (config, comment) in
            [(SHORTCUT_CONFIG_LUA, "--"), (SHORTCUT_CONFIG_CONF, "#")]
        {
            assert!(config.contains("yappr --toggle"));
            assert!(config.contains("yappr --cancel"));
            for line in active_lines(config, comment) {
                assert!(!line.contains("owf-ctl"), "a dead binary leaked in: {line:?}");
                assert!(
                    !line.contains("openwhisprflow"),
                    "the pre-rename binary leaked in: {line:?}"
                );
            }
        }
    }

    /// Spec §10: this breaks in the worst way available -- a shortcut that
    /// silently does nothing. So the emitted block leads with what to
    /// delete, before it says what to add.
    ///
    /// Tested against both format constants directly rather than through
    /// `shortcut_config()`: which one that returns depends on whether
    /// *this* machine has `~/.config/hypr/hyprland.lua` (see
    /// `lua_is_selected_only_when_hyprland_lua_is_present` below), and the
    /// Lua constant never mentions `bindr` at all -- only the classic
    /// format's migration comment does, since `bindr` is that format's own
    /// spelling for hold-to-talk's release-edge bind. A test that called
    /// `shortcut_config()` would therefore pass or fail depending on which
    /// config format happens to be installed on whoever runs the suite,
    /// which is exactly the kind of environment-dependent test this
    /// project's own test discipline (full-sentence names, no hidden
    /// coupling to the runner's machine) exists to avoid.
    #[test]
    fn the_emitted_block_names_the_lines_to_delete_before_the_ones_to_add() {
        for config in [SHORTCUT_CONFIG_LUA, SHORTCUT_CONFIG_CONF] {
            let ctl_at = config.find("owf-ctl").expect("must name the owf-ctl-era command");
            let old_at =
                config.find("openwhisprflow").expect("must name the pre-rename command");
            // `"yappr --toggle"`, not bare `"--toggle"`: the delete comment
            // itself quotes `openwhisprflow --toggle` as a line to remove,
            // and in the Lua block `--` is also the comment marker.
            let add_at = config.find("yappr --toggle").expect("must name the new one");
            assert!(ctl_at < add_at, "must name what to delete before what to add");
            assert!(old_at < add_at, "must name what to delete before what to add");
        }
        assert!(
            SHORTCUT_CONFIG_CONF.contains("bindr"),
            "the bindr line has no replacement and must be named"
        );
    }

    #[test]
    fn neither_config_autostarts_anything() {
        // Autostart is now the Settings window's toggle (spec §9), not a
        // Hyprland line -- a previous version's `o.launch_on_start` /
        // `exec-once` lines must not reappear as live directives (the
        // migration header names them so the user can delete the old ones).
        for line in active_lines(SHORTCUT_CONFIG_LUA, "--") {
            assert!(!line.contains("launch_on_start"), "autostart leaked in: {line:?}");
        }
        for line in active_lines(SHORTCUT_CONFIG_CONF, "#") {
            assert!(!line.contains("exec-once"), "autostart leaked in: {line:?}");
        }
    }

    #[test]
    fn the_lua_config_uses_lua_syntax_not_the_dead_keyword_parser() {
        // Hyprland 0.56+ under Lua rejects the legacy keyword parser, so any
        // conf-style directive leaking into the Lua output is inert text.
        //
        // Lua rule tables legitimately use `=` for their own fields
        // (`float = true` etc.), so a blanket "no bare `=` anywhere" check
        // can't tell those apart from a leaked classic directive. What's
        // actually distinctive about a classic directive is that it's a
        // *complete statement* starting with the bare keyword itself
        // (`bind = ...`, `bindr = ...`) -- every legitimate statement in
        // this file starts with `o.` or `--` instead. Match on that instead
        // of on `=`.
        for line in SHORTCUT_CONFIG_LUA.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("--") {
                continue;
            }
            assert!(
                !trimmed.starts_with("bind ") && !trimmed.starts_with("bind="),
                "conf-style `bind =` leaked into the Lua config: {line:?}"
            );
            for dead in ["windowrulev2", "bindr", "exec-once"] {
                assert!(!line.contains(dead), "conf-style `{dead}` leaked into the Lua config");
            }
        }
    }

    #[test]
    fn window_rules_are_title_matched_not_class_matched_in_either_config() {
        // The settings window shares the overlay's class (Task 9); a
        // class-matched rule reaches it too and its form could not take a
        // keystroke. This is the specific regression this task exists to
        // fix, so it is pinned directly rather than only inferred from the
        // presence of a title selector.
        assert!(SHORTCUT_CONFIG_LUA.contains(r#"o.window({ title = "^yappr overlay$" }"#));
        for line in active_lines(SHORTCUT_CONFIG_LUA, "--") {
            assert!(!line.contains("o.window(\"yappr\""), "class-keyed rule leaked in: {line:?}");
        }

        assert!(SHORTCUT_CONFIG_CONF.contains(",title:^(yappr overlay)$"));
        for line in active_lines(SHORTCUT_CONFIG_CONF, "#") {
            assert!(!line.contains(",class:^(yappr)$"), "class-keyed rule leaked in: {line:?}");
        }
    }

    #[test]
    fn neither_config_ships_unverified_window_rule_effects() {
        // A previous version emitted `windowrulev2` lines written from
        // memory that didn't even apply on a Lua-configured compositor. The
        // rule *effects* here must be exactly the fields verified against
        // the installed Hyprland 0.56.2 Lua binding table
        // (`LuaBindingsInternal.hpp`'s `WINDOW_RULE_EFFECT_DESCS`, see this
        // module's doc comment) for the Lua side, and against real
        // published classic configs for the `.conf` side.
        for verified_field in
            ["float = true", "pin = true", "no_focus = true", "no_dim = true", "border_size = 0", "move = {"]
        {
            assert!(
                SHORTCUT_CONFIG_LUA.contains(verified_field),
                "SHORTCUT_CONFIG_LUA is missing the verified field `{verified_field}`"
            );
        }
        // The classic keyword spellings (no underscores) are Lua-syntax
        // errors, not valid Lua field names -- if either leaks into the Lua
        // config it's a sign the two formats got mixed up.
        assert!(!SHORTCUT_CONFIG_LUA.contains("nofocus"));
        assert!(!SHORTCUT_CONFIG_LUA.contains("noinitialfocus"));

        for verified_effect in
            ["float,title:", "pin,title:", "nofocus,title:", "noinitialfocus,title:", "bordersize 0,title:", "move (monitor_w/2-window_w/2) (monitor_h-window_h-40),title:"]
        {
            assert!(
                SHORTCUT_CONFIG_CONF.contains(verified_effect),
                "SHORTCUT_CONFIG_CONF is missing the verified rule: {verified_effect}"
            );
        }
    }

    #[test]
    fn the_conf_config_still_serves_classic_installations() {
        assert!(SHORTCUT_CONFIG_CONF.contains("bind  = SUPER, D,     exec, yappr --toggle"));
        assert!(SHORTCUT_CONFIG_CONF.contains("bind  = SUPER ALT, D, exec, yappr --cancel"));
        for line in active_lines(SHORTCUT_CONFIG_CONF, "#") {
            assert!(!line.contains("bindr"), "a release-edge bindr leaked in: {line:?}");
        }
    }

    /// Mechanical drift check, the same shape as `proto.rs`'s
    /// `the_overlay_replay_fixture_parses_as_this_crates_overlay_event`
    /// (invariant 3): a title-matched rule that doesn't match
    /// `src-tauri/tauri.conf.json`'s *actual* window titles silently rules
    /// nothing, which is exactly the failure mode a previous version of
    /// this file already shipped once (unverified `windowrulev2` lines).
    #[test]
    fn the_title_selectors_match_tauri_conf_jsons_actual_window_titles() {
        let conf_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/tauri.conf.json");
        let raw = std::fs::read_to_string(&conf_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", conf_path.display()));
        let json: serde_json::Value = serde_json::from_str(&raw).expect("tauri.conf.json is valid JSON");
        let windows = json["app"]["windows"].as_array().expect("app.windows is an array");
        let title_of = |label: &str| -> String {
            windows
                .iter()
                .find(|w| w["label"] == label)
                .and_then(|w| w["title"].as_str())
                .unwrap_or_else(|| panic!("no window labelled {label:?} in tauri.conf.json"))
                .to_string()
        };
        let overlay_title = title_of("overlay");
        let settings_title = title_of("settings");

        assert_eq!(overlay_title, "yappr overlay");

        // Both emitted regexes must match the real overlay title...
        assert!(SHORTCUT_CONFIG_LUA.contains(&overlay_title));
        assert!(SHORTCUT_CONFIG_CONF.contains(&overlay_title));
        // ...and neither may match the settings window's title, which is
        // the entire point of matching on title instead of class.
        assert_ne!(overlay_title, settings_title);
        assert!(!SHORTCUT_CONFIG_LUA.contains(&settings_title));
        assert!(!SHORTCUT_CONFIG_CONF.contains(&settings_title));
    }

    #[test]
    fn lua_is_selected_only_when_hyprland_lua_is_present() {
        let base = std::env::temp_dir().join("yappr-hypr-detect-test");
        let lua_dir = base.join("with-lua");
        let conf_dir = base.join("without-lua");
        std::fs::create_dir_all(&lua_dir).unwrap();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(lua_dir.join("hyprland.lua"), "-- test").unwrap();

        assert!(uses_lua_config(&lua_dir));
        assert!(!uses_lua_config(&conf_dir));

        std::fs::remove_dir_all(&base).ok();
    }
}
