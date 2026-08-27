use std::process::Command;
use std::time::Duration;

use crate::procutil;

/// I3: `hyprctl` runs on the single-threaded accept loop's behalf at every
/// `ptt-start`; a hung `hyprctl` (compositor wedged, IPC socket stuck) must
/// not be able to block that loop -- and every command it serves, including
/// `status` -- forever.
const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(3);

/// Hyprland keybindings and autostart, in Omarchy's Lua config format.
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
/// Window rules are deliberately absent. M1 renders no window, so there is
/// nothing to rule on. When the M2 overlay lands it MUST NOT take keyboard
/// focus -- otherwise `wtype` types the dictation into the overlay instead of
/// the user's target window (spec 5.3) -- and the verified key for that is
/// `o.window("openwhisprflow", { no_focus = true })`. Verify any additional
/// rule names against the running compositor's own stubs
/// (`/usr/share/hypr/stubs/hl.meta.lua`) and Omarchy's `default/hypr/windows.lua`
/// rather than from memory; this syntax changes between Hyprland versions.
pub const HYPR_CONFIG_LUA: &str = r#"-- OpenWhisprFlow: push-to-talk dictation.
-- Add to ~/.config/hypr/bindings.lua
o.bind("SUPER + D", "Dictate (hold to talk)", "owf-ctl ptt-start")
o.bind("SUPER + D", nil, "owf-ctl ptt-stop", { release = true })
o.bind("SUPER + ALT + D", "Dictation: cancel", "owf-ctl cancel")

-- Add to ~/.config/hypr/autostart.lua
o.launch_on_start("owf-daemon")
"#;

/// Hyprland keybindings and autostart, in the classic `.conf` format.
///
/// For installations that still use `hyprland.conf`. See [`HYPR_CONFIG_LUA`]
/// for the Lua equivalent, and for why window rules are not emitted here.
pub const HYPR_CONFIG_CONF: &str = r#"# OpenWhisprFlow
exec-once = owf-daemon

bind  = SUPER, D,       exec, owf-ctl ptt-start
bindr = SUPER, D,       exec, owf-ctl ptt-stop
bind  = SUPER ALT, D,   exec, owf-ctl cancel
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
        for dead in ["windowrule", "bindr", "exec-once", "="] {
            let leaked = HYPR_CONFIG_LUA
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .any(|l| l.contains(dead) && !l.contains("release = true"));
            assert!(!leaked, "conf-style `{dead}` leaked into the Lua config");
        }
    }

    #[test]
    fn neither_config_ships_unverified_window_rules() {
        // A previous version emitted five `windowrulev2` lines written from
        // memory. M1 renders no window, so the correct number of window rules
        // is zero; M2 must verify the syntax against the running compositor.
        for cfg in [HYPR_CONFIG_LUA, HYPR_CONFIG_CONF] {
            assert!(!cfg.contains("windowrule"), "unverified window rule syntax");
            assert!(!cfg.contains("nofocus"));
            assert!(!cfg.contains("noinitialfocus"));
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
