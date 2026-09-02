use std::path::PathBuf;

const APP: &str = "yappr";

fn xdg_runtime() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("no config dir").join(APP)
}

/// Spec §1: the config is written by the app and read by the app, so it lives
/// with the app's other owned state (`wizard-done`, `rejections.jsonl`) rather
/// than in `~/.config`, which is the directory a user is invited to edit.
///
/// The accepted consequence: dotfile-sync setups that cover `~/.config` and not
/// `~/.local/state` stop carrying settings between machines.
pub fn config_file() -> PathBuf {
    state_dir().join("config.toml")
}

/// Where the config lived until 2026-09-02. Read exactly once per start, by
/// `config::migrate_from_legacy`, and never written. [`config_dir`] itself
/// stays -- `hypr.rs` and [`autostart_desktop_file`] still need it.
pub fn legacy_config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn models_dir() -> PathBuf {
    dirs::data_local_dir().expect("no data dir").join(APP).join("models")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| dirs::data_local_dir().expect("no data dir").join("state"))
        .join(APP)
}

pub fn log_file() -> PathBuf {
    state_dir().join("yappr.log")
}

pub fn rejections_file() -> PathBuf {
    state_dir().join("rejections.jsonl")
}

/// Records that a human reached the last step of the setup wizard and clicked
/// Fertig. An empty file: it carries one bit and no format, so there is
/// nothing to version. Deliberately not a `config.toml` key -- that would
/// have to join a `deny_unknown_fields` struct and then show up in the
/// settings GUI as a setting nobody should touch.
pub fn wizard_marker() -> PathBuf {
    state_dir().join("wizard-done")
}

pub fn runtime_socket() -> PathBuf {
    xdg_runtime().join("yappr.sock")
}

pub fn runtime_lock() -> PathBuf {
    xdg_runtime().join("yappr.lock")
}

/// The XDG autostart entry (design doc §9, task 16). Deliberately **not**
/// built on [`config_dir`]: `~/.config/autostart/` is a directory shared by
/// every autostart-capable app on the system (on this machine it already
/// holds, among others, `Handy.desktop` and `claude-desktop.desktop`), not a
/// subdirectory namespaced under this app the way `config_file()`'s parent
/// is. `xdg-autostart-generator` turns whatever `.desktop` files live here
/// into systemd user units automatically -- this project authors no unit of
/// its own.
pub fn autostart_desktop_file() -> PathBuf {
    dirs::config_dir()
        .expect("no config dir")
        .join("autostart")
        .join("yappr.desktop")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_namespaced_under_the_app_name() {
        assert!(config_file().ends_with("yappr/config.toml"));
        assert!(models_dir().ends_with("yappr/models"));
        assert!(rejections_file().ends_with("yappr/rejections.jsonl"));
    }

    /// Spec §1. The two assertions are the whole of the move: the config sits
    /// with the state this app owns, and the path it came *from* is still
    /// computable so `config::migrate_from_legacy` can find a pre-2026-09-02
    /// file exactly once.
    #[test]
    fn the_config_lives_in_the_state_dir_and_its_old_home_is_still_reachable() {
        assert_eq!(config_file().parent(), wizard_marker().parent());
        assert!(legacy_config_file().starts_with(config_dir()));
        assert_ne!(config_file(), legacy_config_file());
    }

    /// `~/.config/autostart/` is a shared directory, not this app's own
    /// namespaced subdirectory -- unlike every other path in this file, it
    /// must sit as a sibling of [`config_dir`], not inside it. Computing the
    /// path is safe to test directly (it is a pure function of
    /// `dirs::config_dir()`); nothing here reads, writes, or creates it.
    #[test]
    fn the_autostart_entry_lives_beside_this_apps_config_dir_not_inside_it() {
        let p = autostart_desktop_file();
        assert!(p.ends_with("autostart/yappr.desktop"));
        assert!(!p.starts_with(config_dir()));
    }

    #[test]
    fn runtime_paths_follow_xdg_runtime_dir() {
        // Not asserting the prefix (it varies by machine); assert the file names,
        // which the daemon and ctl must agree on exactly.
        assert_eq!(runtime_socket().file_name().unwrap(), "yappr.sock");
        assert_eq!(runtime_lock().file_name().unwrap(), "yappr.lock");
    }
}
