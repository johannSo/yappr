use std::path::PathBuf;

const APP: &str = "openwhisprflow";

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
    state_dir().join("openwhisprflow.log")
}

pub fn rejections_file() -> PathBuf {
    state_dir().join("rejections.jsonl")
}

pub fn runtime_socket() -> PathBuf {
    xdg_runtime().join("openwhisprflow.sock")
}

pub fn runtime_lock() -> PathBuf {
    xdg_runtime().join("openwhisprflow.lock")
}

pub fn runtime_port() -> PathBuf {
    xdg_runtime().join("openwhisprflow.port")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_namespaced_under_the_app_name() {
        assert!(config_file().ends_with("openwhisprflow/config.toml"));
        assert!(models_dir().ends_with("openwhisprflow/models"));
        assert!(rejections_file().ends_with("openwhisprflow/rejections.jsonl"));
    }

    #[test]
    fn runtime_paths_follow_xdg_runtime_dir() {
        // Not asserting the prefix (it varies by machine); assert the file names,
        // which the daemon and ctl must agree on exactly.
        assert_eq!(runtime_socket().file_name().unwrap(), "openwhisprflow.sock");
        assert_eq!(runtime_lock().file_name().unwrap(), "openwhisprflow.lock");
        assert_eq!(runtime_port().file_name().unwrap(), "openwhisprflow.port");
    }
}
