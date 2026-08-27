use std::process::Command;
use std::time::Duration;

use crate::procutil;

/// I3: `hyprctl` runs on the single-threaded accept loop's behalf at every
/// `ptt-start`; a hung `hyprctl` (compositor wedged, IPC socket stuck) must
/// not be able to block that loop -- and every command it serves, including
/// `status` -- forever.
const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(3);

/// Hyprland configuration for OpenWhisprFlow.
///
/// The three focus rules are load-bearing, not cosmetic: if the M2 overlay
/// takes keyboard focus, `wtype` delivers the dictation to the overlay instead
/// of the user's target window. See spec 5.3.
pub const HYPR_CONFIG: &str = r#"# OpenWhisprFlow
exec-once = owf-daemon

bind  = SUPER, D,      exec, owf-ctl ptt-start
bindr = SUPER, D,      exec, owf-ctl ptt-stop
bind  = SUPER, ESCAPE, exec, owf-ctl cancel

windowrulev2 = float,          class:^(openwhisprflow)$
windowrulev2 = nofocus,        class:^(openwhisprflow)$
windowrulev2 = noinitialfocus, class:^(openwhisprflow)$
windowrulev2 = pin,            class:^(openwhisprflow)$
windowrulev2 = noborder,       class:^(openwhisprflow)$
"#;

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
    fn the_generated_config_contains_every_load_bearing_rule() {
        for needle in [
            "exec-once = owf-daemon",
            "bind  = SUPER, D,      exec, owf-ctl ptt-start",
            "bindr = SUPER, D,      exec, owf-ctl ptt-stop",
            "owf-ctl cancel",
            "nofocus",
            "noinitialfocus",
        ] {
            assert!(HYPR_CONFIG.contains(needle), "missing from HYPR_CONFIG: {needle}");
        }
    }
}
