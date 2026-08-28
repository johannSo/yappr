use std::process::Command;
use std::time::Duration;

use crate::procutil;

/// I3: `hyprctl` runs on the single-threaded accept loop's behalf at every
/// `ptt-start`; a hung `hyprctl` (compositor wedged, IPC socket stuck) must
/// not be able to block that loop -- and every command it serves, including
/// `status` -- forever.
const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(3);

/// Hyprland keybindings, autostart and window rules, in Omarchy's Lua config
/// format.
///
/// Hyprland 0.56+ configured through Lua rejects the legacy keyword parser
/// outright (`hyprctl keyword windowrule ...` answers "keyword can't work with
/// non-legacy parsers"), so the `.conf` block below is useless on such a
/// system and this is what those users need instead.
///
/// The `release = true` binding is what makes this push-to-talk rather than a
/// toggle: the same key starts recording on press and stops it on release.
/// Both halves must stay bound to the same key.
///
/// ## Window rules
///
/// M1 rendered no window, so M1 shipped none. The M2 overlay exists now, and
/// it needs two things a previous version got wrong by writing
/// `windowrulev2` lines from memory (they didn't even apply on a
/// Lua-configured compositor):
///
/// 1. **Never take keyboard focus** -- otherwise `wtype` types the dictation
///    into the overlay instead of the user's target window (spec 5.3). The
///    Tauri window already asks for `focused:false`/`focusable:false`; this
///    is belt-and-braces at the compositor level.
/// 2. **Be positioned bottom-centre** (spec 12) -- Wayland's `xdg_shell` has
///    no client-settable window position (`tao`'s `set_outer_position` calls
///    `gtk_window_move`, which GTK documents as a no-op under Wayland), so
///    placement can only come from the compositor.
///
/// Every field below is the exact Lua spelling registered by the *installed*
/// Hyprland 0.56.2 binary, not a remembered one. The ground truth is
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
/// That table is cross-checked against real, currently-shipping Omarchy Lua
/// rules on this machine that do the same kind of thing (a floating, pinned,
/// never-focused HUD-like window positioned with monitor-relative math):
///   - `/usr/share/omarchy/default/hypr/windows.lua:18` -- `no_focus = true`
///   - `/usr/share/omarchy/default/hypr/apps/webcam-overlay.lua:20-24` --
///     `float = true, pin = true, no_initial_focus = true, no_dim = true`
///     on a small always-on-top overlay, positioned via `size`/`move`
///   - `/usr/share/omarchy/default/hypr/apps/pip.lua:3-12` -- `float = true`,
///     `pin = true`, `border_size = 0`,
///     `move = { "(monitor_w-window_w-40)", "(monitor_h*0.04)" }` --
///     confirms both the field spellings and the `monitor_w`/`window_w`/
///     `monitor_h`/`window_h` expression variables used below
///
/// `pin` is included because the overlay window is created once, hidden, at
/// daemon start (spec 12) and only shown/hidden after that -- without `pin`
/// it would stay bound to whatever workspace was active at that moment and
/// silently fail to appear if the user dictates from a different one.
/// `no_dim` exists because `no_focus` makes the window permanently
/// "inactive", which would make it a target for `decoration.dim_inactive` if
/// a user ever turns that on (off by default here, but not universally).
/// `border_size = 0` matches the spec's transparent, decoration-less pill --
/// without it Hyprland would still draw its own compositor border around the
/// window regardless of the Tauri window's own `decorations: false`.
pub const HYPR_CONFIG_LUA: &str = r#"-- OpenWhisprFlow: push-to-talk dictation.
-- Add to ~/.config/hypr/bindings.lua
o.bind("SUPER + D", "Dictate (hold to talk)", "owf-ctl ptt-start")
o.bind("SUPER + D", nil, "owf-ctl ptt-stop", { release = true })
o.bind("SUPER + ALT + D", "Dictation: cancel", "owf-ctl cancel")

-- Add to ~/.config/hypr/autostart.lua
o.launch_on_start("owf-daemon")

-- Add to ~/.config/hypr/windows.lua
-- Never focus (spec 5.3), always floating and on every workspace, no
-- compositor border, bottom-centre (spec 12).
o.window("openwhisprflow", {
  float = true,
  pin = true,
  no_focus = true,
  no_dim = true,
  border_size = 0,
  move = { "(monitor_w/2-window_w/2)", "(monitor_h-window_h-40)" },
})
"#;

/// Hyprland keybindings, autostart and window rules, in the classic `.conf`
/// format.
///
/// For installations that still use `hyprland.conf` (Hyprland 0.56.2 still
/// depends on `libhyprlang`, so this parser is retained even though the
/// upstream wiki has moved its documentation to the Lua syntax). See
/// [`HYPR_CONFIG_LUA`] for the Lua equivalent, the two requirements the
/// window rules satisfy, and why `pin`/`no_dim`/`border_size` are included.
///
/// This machine configures Hyprland in Lua, so these classic `windowrulev2`
/// lines can't be exercised here the way the Lua ones were checked against
/// this compositor's own installed binary. Instead each keyword is verified
/// against real, currently-published classic configs rather than memory:
///   - `nofocus` / `noinitialfocus`: a GitHub Hyprland discussion
///     (hyprwm/Hyprland#13141) and issue (#8136) both show
///     `windowrulev2 = noinitialfocus, class:^(jetbrains-.)$` and
///     `windowrulev2 = nofocus, class:^(.*jetbrains.*)$, title:^(win.*)$`
///     used together on a never-should-focus window, exactly this case.
///   - `float` / `pin` / `move <x> <y>`: end-4/dots-hyprland's shipped
///     `rules.conf` (commit `41520aebc6f0bd5fe4e10e32e08f4817bce321c0`) pins
///     and floats a picture-in-picture window with
///     `windowrulev2 = move 73% 72%,title:...`,
///     `windowrulev2 = float,title:...`, `windowrulev2 = pin,title:...`.
///   - `bordersize 0`: cited from a real `hyprland.conf` snippet,
///     `windowrule = bordersize 0, floating:0, onworkspace:w[tv1]`.
///   - the `(monitor_w-1000)`-style parenthesised expression form of `move`
///     (used below for centring) is confirmed by a separate real example,
///     `windowrulev2 = move (monitor_w-1000) (monitor_h-1000), ...`.
///
/// Every one of these keyword strings also matches, one-to-one modulo
/// underscores, the same internal `eWindowRuleEffect` table the Lua bindings
/// use (`WINDOW_RULE_EFFECT_NO_FOCUS`, `_NOINITIALFOCUS`, `_FLOAT`, `_PIN`,
/// `_BORDER_SIZE`, `_MOVE` in
/// `/usr/include/hyprland/src/desktop/rule/windowRule/WindowRuleEffectContainer.hpp`),
/// which is the strongest available cross-check without a classic-conf
/// Hyprland session on hand to test against directly. `no_dim` is left out of
/// this block because it has no independently confirmed classic spelling.
pub const HYPR_CONFIG_CONF: &str = r#"# OpenWhisprFlow
exec-once = owf-daemon

bind  = SUPER, D,       exec, owf-ctl ptt-start
bindr = SUPER, D,       exec, owf-ctl ptt-stop
bind  = SUPER ALT, D,   exec, owf-ctl cancel

# Never focus (spec 5.3), always floating and on every workspace, no
# compositor border, bottom-centre (spec 12).
windowrulev2 = float,class:^(openwhisprflow)$
windowrulev2 = pin,class:^(openwhisprflow)$
windowrulev2 = nofocus,class:^(openwhisprflow)$
windowrulev2 = noinitialfocus,class:^(openwhisprflow)$
windowrulev2 = bordersize 0,class:^(openwhisprflow)$
windowrulev2 = move (monitor_w/2-window_w/2) (monitor_h-window_h-40),class:^(openwhisprflow)$
"#;

/// True when this machine configures Hyprland in Lua rather than `.conf`.
fn uses_lua_config(config_dir: &std::path::Path) -> bool {
    config_dir.join("hyprland.lua").exists()
}

/// The config snippet appropriate to this machine.
///
/// Picks Lua when `~/.config/hypr/hyprland.lua` exists, `.conf` otherwise.
pub fn hypr_config() -> &'static str {
    let dir = dirs::config_dir()
        .map(|d| d.join("hypr"))
        .unwrap_or_else(|| std::path::PathBuf::from("/nonexistent"));
    if uses_lua_config(&dir) {
        HYPR_CONFIG_LUA
    } else {
        HYPR_CONFIG_CONF
    }
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
/// Called at ptt-start, off the latency-critical path.
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
    fn the_lua_config_binds_press_and_release_to_the_same_key() {
        // The release binding is what makes this push-to-talk instead of a
        // toggle. Losing it would leave the microphone hot after one tap.
        assert!(HYPR_CONFIG_LUA.contains(r#"o.bind("SUPER + D", "Dictate (hold to talk)", "owf-ctl ptt-start")"#));
        assert!(HYPR_CONFIG_LUA
            .contains(r#"o.bind("SUPER + D", nil, "owf-ctl ptt-stop", { release = true })"#));
        assert!(HYPR_CONFIG_LUA.contains("o.launch_on_start(\"owf-daemon\")"));
        assert!(HYPR_CONFIG_LUA.contains("owf-ctl cancel"));
    }

    #[test]
    fn the_lua_config_uses_lua_syntax_not_the_dead_keyword_parser() {
        // Hyprland 0.56+ under Lua rejects the legacy keyword parser, so any
        // conf-style directive leaking into the Lua output is inert text.
        //
        // Lua rule tables legitimately use `=` for their own fields
        // (`{ release = true }`, the window rule's `float = true` etc.), so a
        // blanket "no bare `=` anywhere" check can't tell those apart from a
        // leaked classic directive. What's actually distinctive about a
        // classic directive is that it's a *complete statement* starting with
        // the bare keyword itself (`bind = ...`, `bindr = ...`) -- every
        // legitimate statement in this file starts with `o.` instead. Match
        // on that instead of on `=`.
        for line in HYPR_CONFIG_LUA.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("--") {
                continue;
            }
            assert!(
                !trimmed.starts_with("bind ") && !trimmed.starts_with("bind="),
                "conf-style `bind =` leaked into the Lua config: {line:?}"
            );
            for dead in ["windowrule", "bindr", "exec-once"] {
                assert!(!line.contains(dead), "conf-style `{dead}` leaked into the Lua config");
            }
        }
    }

    #[test]
    fn neither_config_ships_unverified_window_rules() {
        // A previous version emitted five `windowrulev2` lines written from
        // memory that didn't even apply on a Lua-configured compositor. M2's
        // rules must instead be exactly the fields verified against the
        // installed Hyprland 0.56.2 Lua binding table
        // (`LuaBindingsInternal.hpp`'s `WINDOW_RULE_EFFECT_DESCS`, see
        // `HYPR_CONFIG_LUA`'s doc comment) for the Lua side, and against real
        // published classic configs for the `.conf` side -- so this test
        // pins the verified forms rather than merely asserting "no rules",
        // and still fails if unverified syntax reappears.
        assert!(HYPR_CONFIG_LUA.contains(r#"o.window("openwhisprflow", {"#));
        for verified_field in
            ["float = true", "pin = true", "no_focus = true", "no_dim = true", "border_size = 0", "move = {"]
        {
            assert!(
                HYPR_CONFIG_LUA.contains(verified_field),
                "HYPR_CONFIG_LUA is missing the verified field `{verified_field}`"
            );
        }
        // The classic keyword spellings (no underscores) are Lua-syntax
        // errors, not valid Lua field names -- if either leaks into the Lua
        // config it's a sign the two formats got mixed up.
        assert!(!HYPR_CONFIG_LUA.contains("nofocus"));
        assert!(!HYPR_CONFIG_LUA.contains("noinitialfocus"));

        for verified_line in [
            "windowrulev2 = float,class:^(openwhisprflow)$",
            "windowrulev2 = pin,class:^(openwhisprflow)$",
            "windowrulev2 = nofocus,class:^(openwhisprflow)$",
            "windowrulev2 = noinitialfocus,class:^(openwhisprflow)$",
            "windowrulev2 = bordersize 0,class:^(openwhisprflow)$",
            "windowrulev2 = move (monitor_w/2-window_w/2) (monitor_h-window_h-40),class:^(openwhisprflow)$",
        ] {
            assert!(
                HYPR_CONFIG_CONF.contains(verified_line),
                "HYPR_CONFIG_CONF is missing the verified rule: {verified_line}"
            );
        }
    }

    #[test]
    fn the_conf_config_still_serves_classic_installations() {
        assert!(HYPR_CONFIG_CONF.contains("bind  = SUPER, D,       exec, owf-ctl ptt-start"));
        assert!(HYPR_CONFIG_CONF.contains("bindr = SUPER, D,       exec, owf-ctl ptt-stop"));
        assert!(HYPR_CONFIG_CONF.contains("exec-once = owf-daemon"));
    }

    #[test]
    fn lua_is_selected_only_when_hyprland_lua_is_present() {
        let base = std::env::temp_dir().join("owf-hypr-detect-test");
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
