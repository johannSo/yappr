//! Tauri commands for first-run model provisioning (spec 7).
//!
//! Before this module existed, provisioning lived entirely behind
//! `owf-ctl setup`/`--update-lock` -- a command that no longer exists on this
//! branch. `crates/owf-core/src/server.rs`'s warm-up *loads* models from disk
//! but never downloads them, so a machine with no models had no way to get
//! them at all. This is that way in: `setup_status` reports what's missing
//! (prerequisite binaries, separately from model files -- neither check
//! subsumes the other), and `run_setup` actually fetches the missing models.
//!
//! Both commands are `async fn` that hand their blocking work to
//! `tauri::async_runtime::spawn_blocking`, exactly as `settings_cmds.rs`'s
//! `call` does and for the same reason (see that module's doc comment):
//! `setup_status` hashes however many models are already on disk (up to
//! ~1.1 GB), and `run_setup` does real network I/O, and neither may run
//! inline on the Tauri event-loop thread without freezing the settings
//! window.
//!
//! `download_all`'s `update_lock` is always `false` here. Rewriting
//! `models.lock.toml` -- the file that pins every model by sha256 -- is a
//! developer action (`openwhisprflow --update-lock`, spec 14.2's "generated
//! once during M0"), never something a user's first run may do: that would
//! mean a first run silently re-pins whatever it happened to download,
//! defeating the point of pinning at all.

use owf_core::models::{self, Artifact, LockFile, ARTIFACTS};
use serde::Serialize;
use tauri::Emitter;

/// One artifact the Setup pane needs to name to the user: its stable key
/// (`Artifact::name` -- never itself shown) and its display name (which may
/// carry license-mandated capitalisation; see `Artifact::display`'s doc
/// comment on why that can't be derived from the key).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct MissingModel {
    name: String,
    display: String,
}

fn artifact_by_name(name: &str) -> Option<&'static Artifact> {
    ARTIFACTS.iter().find(|a| a.name == name)
}

fn artifact_by_url(url: &str) -> Option<&'static Artifact> {
    ARTIFACTS.iter().find(|a| a.url == url)
}

/// Turns the two independent check results into the JSON shape the Setup
/// pane renders. Split out from the `setup_status` command so the shaping
/// logic is testable without touching disk -- the prerequisite and model
/// lists are inputs here, computed for real (binaries on `PATH`, sha256 of
/// whatever is in `models_dir()`) only in the command below.
pub(crate) fn build_status(
    missing_prerequisites: Vec<&'static str>,
    missing_model_names: Vec<String>,
) -> serde_json::Value {
    let missing_models: Vec<MissingModel> = missing_model_names
        .into_iter()
        .map(|name| {
            // Every name `models::verify` returns is one of `ARTIFACTS`'
            // own keys, so this always resolves in practice; the fallback
            // just keeps this function total rather than panicking if that
            // ever stopped being true.
            let display = artifact_by_name(&name)
                .map(|a| a.display.to_string())
                .unwrap_or_else(|| name.clone());
            MissingModel { name, display }
        })
        .collect();
    let ready = missing_prerequisites.is_empty() && missing_models.is_empty();
    serde_json::json!({
        "ready": ready,
        "missing_prerequisites": missing_prerequisites,
        "missing_models": missing_models,
    })
}

/// The two checks behind every question this module answers: which
/// prerequisite binaries are missing, and which models are absent or
/// failing their pinned hash. Blocking (the model check hashes whatever is
/// on disk) -- every caller runs it off the Tauri event-loop thread. Pulled
/// out so `is_ready` and `setup_status` run the exact same two checks rather
/// than each re-deriving them, which is how the two could go out of sync.
fn check_missing() -> Result<(Vec<&'static str>, Vec<String>), String> {
    let missing_prerequisites = crate::setup::check_prerequisites();
    let lock = LockFile::load().map_err(|e| format!("models.lock.toml: {e:#}"))?;
    let missing_models = models::verify(&lock).map_err(|e| format!("Modelle prüfen: {e:#}"))?;
    Ok((missing_prerequisites, missing_models))
}

/// Whether setup is complete right now. `lib.rs`'s `setup()` runs this on a
/// plain background thread at startup, which is currently the *only* way a
/// first-run user ever sees the Setup pane at all: there is no tray yet
/// (Task 12 is blocked behind a compositor probe), so nothing else would
/// ever show the settings window on a fresh install.
fn is_ready() -> Result<bool, String> {
    let (missing_prerequisites, missing_models) = check_missing()?;
    Ok(missing_prerequisites.is_empty() && missing_models.is_empty())
}

/// Blocking, best-effort convenience over [`is_ready`] for a plain
/// background thread (not an async context) that just needs a yes/no and
/// would rather treat "couldn't tell" as "not ready" than propagate the
/// error further -- see `lib.rs`'s `setup()`, the one caller.
pub(crate) fn is_ready_or_assume_not(app_ctx: &str) -> bool {
    match is_ready() {
        Ok(ready) => ready,
        Err(e) => {
            eprintln!("{app_ctx}: could not determine setup status, assuming incomplete: {e}");
            false
        }
    }
}

/// What the Setup pane renders: every prerequisite binary `check_prerequisites`
/// finds missing, and every model `owf_core::models::verify` finds absent or
/// failing its pinned hash -- reported separately (see the module doc) so the
/// pane can tell the user "install `wtype`" apart from "downloading the ASR
/// model".
#[tauri::command]
pub async fn setup_status() -> Result<serde_json::Value, String> {
    let result = tauri::async_runtime::spawn_blocking(|| -> Result<serde_json::Value, String> {
        let (missing_prerequisites, missing_model_names) = check_missing()?;
        Ok(build_status(missing_prerequisites, missing_model_names))
    })
    .await
    .map_err(|e| format!("interner Fehler: {e}"))?;
    result
}

/// One update the Setup pane receives as a `"setup-progress"` Tauri event
/// while `run_setup` is in flight. `#[serde(tag = "kind")]` rather than a
/// bare enum so the TypeScript side can switch on a `kind` field the way it
/// already does nowhere else in this window -- this is the one pane whose
/// state does not come from `config.toml`, so it earns its own small wire
/// shape rather than borrowing `OverlayEvent`'s.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SetupProgress {
    /// Progress within one artifact's download, throttled through
    /// `setup::crosses_report_threshold` -- the same underflow-safe decision
    /// `openwhisprflow --update-lock`'s own progress line uses.
    Downloading {
        name: String,
        display: String,
        done: u64,
        total: Option<u64>,
    },
    /// `download_all` finished every artifact successfully. The daemon's own
    /// warm-up already ran (or failed) at process start, so models placed
    /// just now are not yet loaded -- the pane tells the user to restart.
    Finished,
    /// `download_all` aborted before finishing every artifact. `message` is
    /// `download_all`'s own error, unmodified: it names the artifact and the
    /// underlying cause (network failure, checksum mismatch, ...), which a
    /// translated wrapper would only obscure.
    Failed { message: String },
}

/// Runs provisioning to completion, off the event-loop thread, reporting
/// progress as `"setup-progress"` events as it goes. Returns the same shape
/// `setup_status` does, freshly recomputed, so a caller that only awaits the
/// promise (rather than also listening for the `Finished` event) still learns
/// whether the install actually closed every gap.
#[tauri::command]
pub async fn run_setup(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let emit_handle = app.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let mut state = crate::setup::ProgressState::default();
        models::download_all(false, &mut |url: &str, done: u64, total: Option<u64>| {
            let (report, new_state) = crate::setup::crosses_report_threshold(
                url,
                done,
                total,
                std::mem::take(&mut state),
            );
            state = new_state;
            if report {
                if let Some(a) = artifact_by_url(url) {
                    let _ = emit_handle.emit(
                        "setup-progress",
                        &SetupProgress::Downloading {
                            name: a.name.to_string(),
                            display: a.display.to_string(),
                            done,
                            total,
                        },
                    );
                }
            }
        })
    })
    .await
    .map_err(|e| format!("interner Fehler: {e}"))?;

    match outcome {
        Ok(()) => {
            let _ = app.emit("setup-progress", &SetupProgress::Finished);
            setup_status().await
        }
        Err(e) => {
            let message = format!("{e:#}");
            let _ = app.emit(
                "setup-progress",
                &SetupProgress::Failed { message: message.clone() },
            );
            Err(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No missing prerequisites and no missing models is the only
    /// combination `build_status` calls ready -- the Setup pane's own
    /// disappearance-from-the-sidebar condition.
    #[test]
    fn ready_is_true_only_when_nothing_is_missing() {
        let status = build_status(vec![], vec![]);
        assert_eq!(status["ready"], true);
        assert_eq!(status["missing_prerequisites"], serde_json::json!([]));
        assert_eq!(status["missing_models"], serde_json::json!([]));
    }

    #[test]
    fn a_missing_prerequisite_alone_makes_it_not_ready() {
        let status = build_status(vec!["wtype"], vec![]);
        assert_eq!(status["ready"], false);
        assert_eq!(status["missing_prerequisites"], serde_json::json!(["wtype"]));
    }

    /// The whole reason this function exists rather than handing the raw
    /// artifact keys to the frontend: `silero` is not a name to show anyone,
    /// and `s1-mini`'s display carries license-mandated capitalisation
    /// (`Artifact::display`'s doc comment) that must not be reconstructed by
    /// guessing from the key.
    #[test]
    fn missing_models_are_reported_with_their_display_names() {
        let status = build_status(vec![], vec!["silero".to_string(), "s1-mini".to_string()]);
        assert_eq!(status["ready"], false);
        let models = status["missing_models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["name"], "silero");
        assert_eq!(models[0]["display"], "Silero VAD");
        assert_eq!(models[1]["name"], "s1-mini");
        assert_eq!(models[1]["display"], "S1-mini by Superwhisper");
    }

    /// A name `models::verify` never actually returns in practice (every one
    /// of its outputs is one of `ARTIFACTS`' own keys) -- pinned so this
    /// function stays total rather than panicking if that ever changed.
    #[test]
    fn an_unrecognised_model_name_falls_back_to_showing_itself() {
        let status = build_status(vec![], vec!["mystery-model".to_string()]);
        let models = status["missing_models"].as_array().unwrap();
        assert_eq!(models[0]["name"], "mystery-model");
        assert_eq!(models[0]["display"], "mystery-model");
    }

    #[test]
    fn every_missing_kind_is_reported_together_when_both_are_missing() {
        let status = build_status(vec!["llama-cpp", "ggml-cpu"], vec!["parakeet".to_string()]);
        assert_eq!(status["ready"], false);
        assert_eq!(
            status["missing_prerequisites"],
            serde_json::json!(["llama-cpp", "ggml-cpu"])
        );
        assert_eq!(status["missing_models"][0]["name"], "parakeet");
    }

    #[test]
    fn artifact_by_url_finds_the_matching_artifact() {
        let parakeet = artifact_by_url(
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        );
        assert_eq!(parakeet.map(|a| a.name), Some("parakeet"));
        assert!(artifact_by_url("https://example.com/not-a-real-artifact").is_none());
    }

    /// `SetupProgress`'s wire shape: `#[serde(tag = "kind")]` is what lets
    /// the frontend switch on a `kind` field, so it is pinned directly
    /// rather than trusted to stay whatever `#[derive(Serialize)]` happens
    /// to produce.
    #[test]
    fn setup_progress_variants_serialize_with_a_kind_tag() {
        let downloading = SetupProgress::Downloading {
            name: "parakeet".to_string(),
            display: "Parakeet TDT 0.6b v3 (int8)".to_string(),
            done: 1024,
            total: Some(2048),
        };
        let v = serde_json::to_value(&downloading).unwrap();
        assert_eq!(v["kind"], "downloading");
        assert_eq!(v["name"], "parakeet");
        assert_eq!(v["done"], 1024);
        assert_eq!(v["total"], 2048);

        assert_eq!(serde_json::to_value(SetupProgress::Finished).unwrap()["kind"], "finished");

        let failed = SetupProgress::Failed { message: "network error".to_string() };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["kind"], "failed");
        assert_eq!(v["message"], "network error");
    }
}
