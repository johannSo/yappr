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

pub fn config_file() -> PathBuf {
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

pub fn runtime_socket() -> PathBuf {
    xdg_runtime().join("yappr.sock")
}

pub fn runtime_lock() -> PathBuf {
    xdg_runtime().join("yappr.lock")
}

pub fn runtime_port() -> PathBuf {
    xdg_runtime().join("yappr.port")
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
        assert_eq!(runtime_port().file_name().unwrap(), "yappr.port");
    }
}
