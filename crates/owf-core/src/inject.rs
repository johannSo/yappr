use std::process::Command;
use std::sync::Mutex;

use crate::config::{InjectBackend, InjectConfig};

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("{backend} exited with status {status}: {stderr}")]
    Failed { backend: &'static str, status: String, stderr: String },
    #[error("could not run {backend}: {source}")]
    Spawn { backend: &'static str, source: std::io::Error },
    #[error("mock injector configured to fail")]
    Mock,
}

pub trait TextInjector: Send + Sync {
    fn inject(&self, text: &str) -> Result<(), InjectError>;
    fn name(&self) -> &'static str;
}

/// Builds the wtype argument vector.
///
/// `--` terminates option parsing so a transcript starting with `-` is typed
/// rather than misread as flags (confirmed against `wtype`'s own man page,
/// which documents `wtype [OPTION_OR_TEXT]... -- [TEXT]...`); `-d` inserts
/// an inter-keystroke delay that some Electron and XWayland surfaces need to
/// avoid dropping characters.
fn wtype_argv(text: &str, delay_ms: u32) -> Vec<String> {
    vec!["-d".to_string(), delay_ms.to_string(), "--".to_string(), text.to_string()]
}

pub struct WtypeInjector {
    delay_ms: u32,
}

impl WtypeInjector {
    pub fn new(delay_ms: u32) -> Self {
        Self { delay_ms }
    }
}

impl TextInjector for WtypeInjector {
    fn name(&self) -> &'static str {
        "wtype"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        let out = Command::new("wtype")
            .args(wtype_argv(text, self.delay_ms))
            .output()
            .map_err(|source| InjectError::Spawn { backend: "wtype", source })?;
        if !out.status.success() {
            return Err(InjectError::Failed {
                backend: "wtype",
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

pub struct ClipboardInjector;

impl TextInjector for ClipboardInjector {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        use std::io::Write;
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|source| InjectError::Spawn { backend: "wl-copy", source })?;
        child
            .stdin
            .as_mut()
            .expect("piped stdin")
            .write_all(text.as_bytes())
            .map_err(|source| InjectError::Spawn { backend: "wl-copy", source })?;
        let status = child
            .wait()
            .map_err(|source| InjectError::Spawn { backend: "wl-copy", source })?;
        if !status.success() {
            return Err(InjectError::Failed {
                backend: "clipboard",
                status: status.to_string(),
                stderr: String::new(),
            });
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MockInjector {
    calls: Mutex<Vec<String>>,
    fail: bool,
}

impl MockInjector {
    pub fn failing() -> Self {
        Self { calls: Mutex::new(Vec::new()), fail: true }
    }

    pub fn injected(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl TextInjector for MockInjector {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn inject(&self, text: &str) -> Result<(), InjectError> {
        if self.fail {
            return Err(InjectError::Mock);
        }
        self.calls.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

pub fn build(cfg: &InjectConfig) -> Box<dyn TextInjector> {
    match cfg.backend {
        InjectBackend::Wtype => Box::new(WtypeInjector::new(cfg.keystroke_delay_ms)),
        InjectBackend::Clipboard => Box::new(ClipboardInjector),
    }
}

/// Injects via `primary`; on failure copies to the clipboard and notifies.
///
/// A transcript is never silently lost — see spec 10.4.
pub fn inject_with_fallback(
    primary: &dyn TextInjector,
    text: &str,
) -> anyhow::Result<&'static str> {
    match primary.inject(text) {
        Ok(()) => Ok(primary.name()),
        Err(e) => {
            tracing::warn!(error = %e, "primary injector failed; falling back to clipboard");
            ClipboardInjector.inject(text)?;
            let _ = notify_rust::Notification::new()
                .summary("OpenWhisprFlow")
                .body("Typing failed — transcript copied to clipboard")
                .timeout(notify_rust::Timeout::Milliseconds(4_000))
                .show();
            Ok("clipboard")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InjectBackend, InjectConfig};

    #[test]
    fn mock_injector_records_what_it_was_given() {
        let m = MockInjector::default();
        m.inject("hello").unwrap();
        m.inject("world").unwrap();
        assert_eq!(m.injected(), vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn mock_injector_can_be_told_to_fail() {
        let m = MockInjector::failing();
        assert!(m.inject("hello").is_err());
    }

    #[test]
    fn wtype_argv_ends_option_parsing_before_the_text() {
        // A transcript beginning with '-' must be typed, not parsed as flags.
        let argv = wtype_argv("-- not a flag", 2);
        let dashdash = argv.iter().position(|a| a == "--").expect("needs a --");
        assert_eq!(argv.last().unwrap(), "-- not a flag");
        assert!(dashdash < argv.len() - 1, "-- must precede the text");
        assert!(argv.contains(&"-d".to_string()));
        assert!(argv.contains(&"2".to_string()));
    }

    #[test]
    fn wtype_argv_passes_the_text_as_a_single_argument() {
        let argv = wtype_argv("hello there friend", 2);
        assert_eq!(argv.iter().filter(|a| a.contains(' ')).count(), 1);
    }

    #[test]
    fn build_selects_the_configured_backend() {
        let mut cfg = InjectConfig::default();
        assert_eq!(build(&cfg).name(), "wtype");
        cfg.backend = InjectBackend::Clipboard;
        assert_eq!(build(&cfg).name(), "clipboard");
    }

    #[test]
    fn fallback_reports_the_primary_when_it_succeeds() {
        let m = MockInjector::default();
        assert_eq!(inject_with_fallback(&m, "hello").unwrap(), "mock");
        assert_eq!(m.injected(), vec!["hello".to_string()]);
    }
}
