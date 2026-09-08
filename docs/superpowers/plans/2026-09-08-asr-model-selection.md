# Selectable ASR Models Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user pick which speech-recognition model yappr uses, from a dropdown in the settings window and in the first-run wizard, with only the selected model downloaded.

**Architecture:** `models.rs` gains a catalogue of ASR models, each owning an `Artifact` and a flavour (`Offline` = sherpa-onnx `OfflineRecognizer`, as today; `CacheAwareStreaming` = `OnlineRecognizer`, fed the whole utterance at once). Provisioning stops treating every artifact as required and instead requires Silero + S1-mini + *the selected* ASR model. `asr::build` resolves the config selection to a `Box<dyn Transcriber>`, so `Pipeline` and everything downstream is untouched.

**Tech Stack:** Rust (`yappr-core`, `src-tauri`), `sherpa-onnx` 1.13.6 crate, serde/TOML config, React + TypeScript settings frontend, `bun` for the frontend build.

**Spec:** `docs/superpowers/specs/2026-09-08-asr-model-selection-design.md`

## Global Constraints

- **Never change `Artifact::name` for the existing Parakeet entry.** It stays the bare string `"parakeet"`. It is the key in the committed `crates/yappr-core/models.lock.toml`; renaming it invalidates the pin on every machine that already has the model.
- **Config structs are `#[serde(deny_unknown_fields)]` (invariant 4).** Every new key needs `#[serde(default = "…")]` so configs written before this change still load.
- **Once ASR has produced text, the user gets text (invariant 1).** No new code path may lose a transcribed utterance.
- **The default ASR model is `parakeet-tdt-v3`.** Fresh installs and upgrades must behave exactly as they do today.
- **The nemotron chunk size is fixed at 560 ms.** It is not a setting.
- **Test names are full sentences** (repo convention). Documented-but-unfixed behaviour is prefixed `known_limitation_`.
- **The gate is** `cargo test --workspace && cargo test --workspace -- --ignored && cargo clippy --workspace --all-targets`. There is no CI.
- **German is the UI language.** All user-facing strings in `schema.ts` and `wizard.tsx` are German.
- Model URLs, all under `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/`:
  - `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2`
  - `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2`
  - `sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2`

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/yappr-core/src/config.rs` | `AsrModel` enum, `[asr] model` / `[asr] language`, validation | Modify |
| `crates/yappr-core/src/models.rs` | Catalogue, `all_artifacts`, `required_artifacts`, selection-driven `verify`/`looks_present`/`download_all` | Modify |
| `crates/yappr-core/src/asr.rs` | `build()`, parameterised `SherpaTranscriber`, new `SherpaStreamingTranscriber` | Modify |
| `crates/yappr-core/src/server.rs` | `load_models` calls `asr::build` | Modify (1 line + import) |
| `crates/yappr-core/models.lock.toml` | Pins for every catalogue entry | Modify |
| `crates/yappr-core/tests/asr_fixture.rs` | `#[ignore]`d real-model tests, incl. the streaming flavour | Modify |
| `src-tauri/src/provision.rs` | Config-aware missing-model cache, catalogue-wide artifact lookup | Modify |
| `src-tauri/src/bench.rs` | Uses `asr::build` instead of `SherpaTranscriber::new` | Modify |
| `crates/yappr-core/examples/transcribe_file.rs` | Same | Modify |
| `src/settings/schema.ts` | `ENUMS`/`LABELS`/`HELP` entries for the two new keys | Modify |
| `src/settings/model-download.tsx` | Download affordance for a model chosen in Settings | Create |
| `src/Settings.tsx` | Save-revision counter, the section slot the affordance renders in | Modify |
| `src/settings/wizard.tsx` | Model dropdown on the Modelle step; exports `useSetup` | Modify |
| `CLAUDE.md`, `docs/HANDOVER.md` | Stop naming Parakeet TDT as *the* engine | Modify |

`primeline-parakeet` is deliberately the **last** task: it needs an ONNX export produced outside this repository (spec §9), and adding its catalogue entry before it is pinned would break the pin-completeness test.

---

### Task 1: `[asr] model` and `[asr] language` in the config

**Files:**
- Modify: `crates/yappr-core/src/config.rs` (`AsrConfig` at :137-150, `validate` at :665-690, tests module)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub enum AsrModel { ParakeetTdtV3, ParakeetUnifiedEn, Nemotron35 }` with `Copy + Clone + Debug + PartialEq + Eq + Serialize + Deserialize`; `AsrConfig { model: AsrModel, language: String, num_threads: i32 }`; `fn d_asr_model() -> AsrModel`.

- [ ] **Step 1: Write the failing tests**

Add to `config.rs`'s `mod tests`, following the existing `inject.backend` spelling test at :1344:

```rust
#[test]
fn every_asr_model_spelling_round_trips_from_toml() {
    for (spelling, expected) in [
        ("parakeet-tdt-v3", AsrModel::ParakeetTdtV3),
        ("parakeet-unified-en", AsrModel::ParakeetUnifiedEn),
        ("nemotron-3.5", AsrModel::Nemotron35),
    ] {
        let c = Config::from_str(&format!("[asr]\nmodel = \"{spelling}\"\n")).unwrap();
        assert_eq!(c.asr.model, expected, "for {spelling}");
    }
}

#[test]
fn a_config_written_before_model_selection_existed_still_loads() {
    // Invariant 4: [asr] is deny_unknown_fields, and every pre-existing
    // config.toml has num_threads and nothing else in this section.
    let c = Config::from_str("[asr]\nnum_threads = 7\n").unwrap();
    assert_eq!(c.asr.num_threads, 7);
    assert_eq!(c.asr.model, AsrModel::ParakeetTdtV3, "the default must not move");
    assert_eq!(c.asr.language, "auto");
}

#[test]
fn an_unknown_asr_model_spelling_is_rejected_rather_than_silently_defaulted() {
    let err = Config::from_str("[asr]\nmodel = \"whisper\"\n").unwrap_err();
    assert!(format!("{err:#}").contains("model"), "unhelpful error: {err:#}");
}

#[test]
fn asr_language_must_be_auto_or_a_two_letter_code() {
    assert!(Config::from_str("[asr]\nlanguage = \"de\"\n").unwrap().validate().is_ok());
    assert!(Config::from_str("[asr]\nlanguage = \"auto\"\n").unwrap().validate().is_ok());
    let err = Config::from_str("[asr]\nlanguage = \"Deutsch\"\n")
        .unwrap()
        .validate()
        .unwrap_err();
    assert!(format!("{err:#}").contains("asr.language"), "unhelpful error: {err:#}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core config::tests::every_asr_model_spelling_round_trips_from_toml`
Expected: FAIL — `cannot find type AsrModel in this scope`.

- [ ] **Step 3: Add the enum and the two fields**

Replace `AsrConfig` and its `Default` in `config.rs`:

```rust
/// Which speech-recognition model the pipeline loads.
///
/// The spelling is the stable, user-visible name in `config.toml` and the
/// key in `models::ASR_MODELS`. It is deliberately *not* the lock-file key:
/// the Parakeet v3 entry is pinned under the bare name `parakeet`, which can
/// never change (see `models::Artifact::name`).
///
/// See `docs/superpowers/specs/2026-09-08-asr-model-selection-design.md` §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AsrModel {
    /// Parakeet TDT 0.6b v3 — multilingual, the default and the only model
    /// that existed before model selection did.
    #[serde(rename = "parakeet-tdt-v3")]
    ParakeetTdtV3,
    /// Parakeet Unified 0.6b — English only.
    #[serde(rename = "parakeet-unified-en")]
    ParakeetUnifiedEn,
    /// Nemotron 3.5 ASR 0.6b, 560 ms cache-aware streaming export —
    /// multilingual, and the only entry that is not an `OfflineRecognizer`.
    #[serde(rename = "nemotron-3.5")]
    Nemotron35,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsrConfig {
    #[serde(default = "d_asr_model")]
    pub model: AsrModel,
    /// `"auto"`, or a two-letter code such as `"de"`. Only a
    /// `CacheAwareStreaming` model reads this; the offline models are either
    /// single-language or detect it themselves.
    #[serde(default = "d_asr_language")]
    pub language: String,
    #[serde(default = "d_threads")]
    pub num_threads: i32,
}

fn d_asr_model() -> AsrModel {
    AsrModel::ParakeetTdtV3
}
fn d_asr_language() -> String {
    "auto".to_string()
}
fn d_threads() -> i32 {
    4
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model: d_asr_model(),
            language: d_asr_language(),
            num_threads: d_threads(),
        }
    }
}
```

Note: the existing `AsrConfig` declaration already carries `#[derive(...)]` and `#[serde(deny_unknown_fields)]` above line 137 — replace the whole block, do not duplicate the attributes.

- [ ] **Step 4: Add the validation**

In `Config::validate`, directly after the `asr.num_threads` check:

```rust
        let lang_ok = self.asr.language == "auto"
            || (self.asr.language.len() == 2
                && self.asr.language.bytes().all(|b| b.is_ascii_lowercase()));
        if !lang_ok {
            bail!(
                "asr.language must be \"auto\" or a two-letter lowercase code such as \"de\", got {:?}",
                self.asr.language
            );
        }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p yappr-core config::`
Expected: PASS, including the pre-existing `empty_toml_yields_documented_defaults`.

- [ ] **Step 6: Commit**

```bash
git add crates/yappr-core/src/config.rs
git commit -m "feat(config): [asr] model and [asr] language"
```

---

### Task 2: The model catalogue, and provisioning driven by the selection

**Files:**
- Modify: `crates/yappr-core/src/models.rs` (`ARTIFACTS` at :27-56, `hash_target`, `verify`, `looks_present`, `download_all`, tests module)

**Interfaces:**
- Consumes: `config::AsrModel` (Task 1).
- Produces:
  - `pub enum AsrFlavor { Offline, CacheAwareStreaming }`
  - `pub struct AsrModelSpec { pub key: AsrModel, pub artifact: Artifact, pub flavor: AsrFlavor, pub display: &'static str }`
  - `pub static ASR_MODELS: [AsrModelSpec; 3]`
  - `pub static SUPPORT_ARTIFACTS: [Artifact; 2]`
  - `pub fn all_artifacts() -> Vec<&'static Artifact>`
  - `pub fn spec_for(model: AsrModel) -> &'static AsrModelSpec`
  - `pub fn required_artifacts(model: AsrModel) -> Vec<&'static Artifact>`
  - `pub fn verify(lock: &LockFile, required: &[&'static Artifact]) -> Result<Vec<String>>`
  - `pub fn looks_present(required: &[&'static Artifact]) -> bool`
  - `pub fn download_all(required: &[&'static Artifact], update_lock: bool, progress: &mut dyn FnMut(&str, u64, Option<u64>)) -> Result<()>`

`ARTIFACTS` is removed; `all_artifacts()` replaces it everywhere.

- [ ] **Step 1: Write the failing tests**

Add to `models.rs`'s `mod tests`:

```rust
#[test]
fn required_artifacts_is_the_support_pair_plus_exactly_one_asr_model() {
    for model in [
        AsrModel::ParakeetTdtV3,
        AsrModel::ParakeetUnifiedEn,
        AsrModel::Nemotron35,
    ] {
        let required = required_artifacts(model);
        assert_eq!(required.len(), 3, "for {model:?}");
        assert!(required.iter().any(|a| a.name == "silero"), "for {model:?}");
        assert!(required.iter().any(|a| a.name == "s1-mini"), "for {model:?}");
        assert!(
            required.iter().any(|a| a.name == spec_for(model).artifact.name),
            "the selected model itself is missing, for {model:?}"
        );
    }
}

#[test]
fn selecting_one_asr_model_never_requires_another() {
    let required = required_artifacts(AsrModel::Nemotron35);
    assert!(
        !required.iter().any(|a| a.name == "parakeet"),
        "Parakeet v3 must not be required when it is not selected: {:?}",
        required.iter().map(|a| a.name).collect::<Vec<_>>()
    );
}

#[test]
fn the_parakeet_v3_lock_key_is_still_the_bare_name_parakeet() {
    // Renaming this key invalidates models.lock.toml on every machine that
    // already downloaded the model. See Artifact::name's doc comment.
    assert_eq!(spec_for(AsrModel::ParakeetTdtV3).artifact.name, "parakeet");
}

#[test]
fn every_artifact_name_is_unique_across_the_whole_catalogue() {
    let mut names: Vec<&str> = all_artifacts().iter().map(|a| a.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate lock key in the catalogue: {names:?}");
}

#[test]
fn every_asr_model_enum_variant_has_a_catalogue_entry() {
    // spec_for panics on a variant with no entry, which is the failure this
    // asserts against: adding a variant without an artifact must not compile
    // its way into a runtime panic on someone's first dictation.
    for model in [
        AsrModel::ParakeetTdtV3,
        AsrModel::ParakeetUnifiedEn,
        AsrModel::Nemotron35,
    ] {
        let _ = spec_for(model);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core models::tests::required_artifacts_is_the_support_pair_plus_exactly_one_asr_model`
Expected: FAIL — `cannot find function required_artifacts`.

- [ ] **Step 3: Replace `ARTIFACTS` with the catalogue**

In `models.rs`, add `use crate::config::AsrModel;` to the imports and replace the whole `pub static ARTIFACTS: [Artifact; 3] = [...]` block with:

```rust
/// How a model is decoded. Not a property of the file layout — all four
/// entries are encoder/decoder/joiner + tokens.txt — but of which
/// sherpa-onnx recognizer can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrFlavor {
    /// `OfflineRecognizer`, as every version before model selection used.
    Offline,
    /// `OnlineRecognizer`. Exported for cache-aware streaming; yappr feeds it
    /// the whole VAD-trimmed utterance at once and takes the final result.
    /// See spec asr-model §3.
    CacheAwareStreaming,
}

/// One selectable speech-recognition model.
pub struct AsrModelSpec {
    /// The `[asr] model` spelling. Distinct from `artifact.name`, which is
    /// the lock key and can never change.
    pub key: AsrModel,
    pub artifact: Artifact,
    pub flavor: AsrFlavor,
    /// German dropdown label. Names the language, because that is what
    /// actually decides the choice.
    pub display: &'static str,
}

pub static ASR_MODELS: [AsrModelSpec; 3] = [
    AsrModelSpec {
        key: AsrModel::ParakeetTdtV3,
        artifact: Artifact {
            // Never rename: this is the committed lock key.
            name: "parakeet",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
            display: "Parakeet TDT 0.6b v3 (int8)",
            rel_path: "parakeet-tdt-0.6b-v3-int8",
            archive: true,
        },
        flavor: AsrFlavor::Offline,
        display: "Parakeet TDT 0.6b v3 — mehrsprachig",
    },
    AsrModelSpec {
        key: AsrModel::ParakeetUnifiedEn,
        artifact: Artifact {
            name: "parakeet-unified-en",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2",
            display: "Parakeet Unified EN 0.6b (int8)",
            rel_path: "parakeet-unified-en-0.6b-int8",
            archive: true,
        },
        flavor: AsrFlavor::Offline,
        display: "Parakeet Unified 0.6b — nur Englisch",
    },
    AsrModelSpec {
        key: AsrModel::Nemotron35,
        artifact: Artifact {
            name: "nemotron-3.5-560ms",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2",
            display: "Nemotron 3.5 ASR 0.6b (560 ms, int8)",
            rel_path: "nemotron-3.5-asr-streaming-0.6b-560ms-int8",
            archive: true,
        },
        flavor: AsrFlavor::CacheAwareStreaming,
        display: "Nemotron 3.5 ASR 0.6b — mehrsprachig",
    },
];

/// Everything that is needed no matter which ASR model is selected.
pub static SUPPORT_ARTIFACTS: [Artifact; 2] = [
    Artifact {
        name: "silero",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        display: "Silero VAD",
        rel_path: "silero_vad.onnx",
        archive: false,
    },
    Artifact {
        name: "s1-mini",
        // The German de-v3 finetune of upstream S1-mini
        // (https://huggingface.co/superwhisper/s1-mini-GGUF), published from
        // this repo's own training pipeline. Same qwen3 architecture and
        // chat template as upstream, so `normalize::render_chat_prompt`
        // needed no change.
        url: "https://huggingface.co/Joni000000000/s1-mini-de-v3/resolve/main/s1-mini-q4_k_m-de-v3.gguf",
        // License-mandated capitalization: "S1-mini" by "Superwhisper", exactly.
        display: "S1-mini by Superwhisper (de-v3 Finetune)",
        rel_path: "s1-mini-q4_k_m-de-v3.gguf",
        archive: false,
    },
];

/// Every artifact this build knows about, selected or not. Used for
/// name/URL lookups (a download-progress event names a URL, and the Setup
/// pane has to turn that back into a display name) and for the lock-file
/// completeness test — never as "what must be present".
pub fn all_artifacts() -> Vec<&'static Artifact> {
    SUPPORT_ARTIFACTS
        .iter()
        .chain(ASR_MODELS.iter().map(|m| &m.artifact))
        .collect()
}

/// The catalogue entry for `model`.
///
/// Panics only if a variant were added to `AsrModel` without an entry here,
/// which `every_asr_model_enum_variant_has_a_catalogue_entry` prevents from
/// reaching a release.
pub fn spec_for(model: AsrModel) -> &'static AsrModelSpec {
    ASR_MODELS
        .iter()
        .find(|m| m.key == model)
        .unwrap_or_else(|| panic!("no catalogue entry for {model:?}"))
}

/// What provisioning may treat as required: the support pair plus the one
/// selected ASR model. Models left on disk from an earlier selection are
/// deliberately absent — they are not verified, not redownloaded, and never
/// deleted, so switching back is instant and offline. See spec asr-model §4.
pub fn required_artifacts(model: AsrModel) -> Vec<&'static Artifact> {
    SUPPORT_ARTIFACTS
        .iter()
        .chain(std::iter::once(&spec_for(model).artifact))
        .collect()
}
```

The URLs are spelled out in full rather than composed from a shared base const: `Artifact::url` is a `&'static str` inside a `static`, and `concat!` cannot interpolate a `const` there.

- [ ] **Step 4: Make the three provisioning functions take the required list**

Change the three signatures and their bodies' loop headers:

```rust
pub fn verify(lock: &LockFile, required: &[&'static Artifact]) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    for a in required.iter().copied() {
```

```rust
pub fn looks_present(required: &[&'static Artifact]) -> bool {
    let targets: Vec<PathBuf> = required.iter().map(|a| hash_target(a)).collect();
    all_targets_look_present(&targets)
}
```

```rust
pub fn download_all(
    required: &[&'static Artifact],
    update_lock: bool,
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<()> {
    let mut lock = LockFile::load()?;
    for a in required.iter().copied() {
```

The bodies are otherwise unchanged.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p yappr-core models::`
Expected: PASS. `cargo build -p yappr-core` will still fail at this point — `src-tauri` callers are updated in Task 5. That is expected; `yappr-core`'s own tests must pass.

- [ ] **Step 6: Commit**

```bash
git add crates/yappr-core/src/models.rs
git commit -m "feat(models): ASR model catalogue, provisioning follows the selection"
```

---

### Task 3: `asr::build` and a parameterised offline transcriber

**Files:**
- Modify: `crates/yappr-core/src/asr.rs`
- Modify: `crates/yappr-core/src/server.rs:996`
- Modify: `src-tauri/src/bench.rs:77`, `crates/yappr-core/examples/transcribe_file.rs:21`, `crates/yappr-core/tests/asr_fixture.rs`

**Interfaces:**
- Consumes: `config::{AsrConfig, AsrModel}` (Task 1); `models::{spec_for, AsrFlavor}` (Task 2).
- Produces: `pub fn build(models_dir: &Path, cfg: &AsrConfig) -> Result<Box<dyn Transcriber>>`; `SherpaTranscriber::new(models_dir: &Path, rel_path: &str, num_threads: i32)`.

- [ ] **Step 1: Write the failing test**

Add to `asr.rs` a `mod tests` (the file currently has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AsrConfig, AsrModel};

    /// `build` must fail cleanly, not panic, when the selected model is not
    /// on disk. This is an ordinary state now that only the *selected* model
    /// is downloaded (spec asr-model §4), and it reaches the user as
    /// `run_utterance`'s retryable error (invariant 12), not as a crash.
    #[test]
    fn building_a_transcriber_for_an_absent_model_is_an_error_not_a_panic() {
        let empty = std::env::temp_dir().join(format!(
            "yappr-asr-build-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&empty).unwrap();
        let cfg = AsrConfig {
            model: AsrModel::Nemotron35,
            ..AsrConfig::default()
        };
        assert!(build(&empty, &cfg).is_err());
        std::fs::remove_dir_all(&empty).ok();
    }

    #[test]
    fn the_default_config_selects_the_parakeet_v3_directory() {
        let spec = crate::models::spec_for(AsrConfig::default().model);
        assert_eq!(spec.artifact.rel_path, "parakeet-tdt-0.6b-v3-int8");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yappr-core asr::tests`
Expected: FAIL — `cannot find function build in this scope`.

- [ ] **Step 3: Parameterise `SherpaTranscriber` and add `build`**

In `asr.rs`, change the `new` signature and body:

```rust
impl SherpaTranscriber {
    /// `rel_path` is the model directory's name under `models_dir`, from the
    /// catalogue (`models::AsrModelSpec::artifact.rel_path`) rather than
    /// hardcoded — there is more than one offline model now.
    pub fn new(models_dir: &Path, rel_path: &str, num_threads: i32) -> Result<Self> {
        let dir = models_dir.join(rel_path);
```

The rest of `new` is unchanged. Then append to the file:

```rust
/// The whole utterance at once, through sherpa-onnx's *online* recognizer.
///
/// The nemotron export is published only as a cache-aware streaming model,
/// so this is the only way to read it — but yappr has no streaming UI yet
/// (spec asr-model §10), and the VAD has already decided where the utterance
/// ends. So the buffer goes in in one go, `input_finished` closes it, and the
/// single final result is what gets injected. Endpointing stays off for the
/// same reason: it exists to cut a live stream into utterances, and that
/// decision has already been made upstream.
pub struct SherpaStreamingTranscriber {
    recognizer: OnlineRecognizer,
    language: String,
}

impl Transcriber for SherpaStreamingTranscriber {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let stream = self.recognizer.create_stream();
        stream.set_option("language", &self.language);
        stream.accept_waveform(SAMPLE_RATE, samples);
        stream.input_finished();
        while self.recognizer.is_ready(&stream) {
            self.recognizer.decode(&stream);
        }
        let text = self
            .recognizer
            .get_result(&stream)
            .map(|r| r.text)
            .unwrap_or_default();
        Ok(text.trim().to_string())
    }
}

/// Resolves `[asr] model` to a loaded transcriber.
///
/// The one place that knows a model can have more than one flavour;
/// everything downstream sees `Box<dyn Transcriber>` and cannot tell the
/// difference (spec §17.1).
pub fn build(models_dir: &Path, cfg: &AsrConfig) -> Result<Box<dyn Transcriber>> {
    let spec = models::spec_for(cfg.model);
    match spec.flavor {
        AsrFlavor::Offline => Ok(Box::new(SherpaTranscriber::new(
            models_dir,
            spec.artifact.rel_path,
            cfg.num_threads,
        )?)),
        AsrFlavor::CacheAwareStreaming => Ok(Box::new(SherpaStreamingTranscriber::new(
            models_dir,
            spec.artifact.rel_path,
            cfg.num_threads,
            &cfg.language,
        )?)),
    }
}
```

Add to the imports at the top of `asr.rs`:

```rust
use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig, OnlineModelConfig,
    OnlineRecognizer, OnlineRecognizerConfig, OnlineTransducerModelConfig,
};

use crate::config::AsrConfig;
use crate::models::{self, AsrFlavor};
```

`SherpaStreamingTranscriber::new` is written in Task 4 — for this task, stub it so the crate compiles and the two tests above pass:

```rust
impl SherpaStreamingTranscriber {
    pub fn new(
        _models_dir: &Path,
        _rel_path: &str,
        _num_threads: i32,
        _language: &str,
    ) -> Result<Self> {
        anyhow::bail!("the streaming ASR flavour is not implemented yet")
    }
}
```

- [ ] **Step 4: Update the four call sites**

`server.rs:996`:

```rust
    let asr = asr::build(&models, &cfg.asr)?;
```

and change the `Pipeline::new` call at :1044 from `Box::new(asr)` to `asr` (it is already boxed). Adjust `server.rs`'s import from `SherpaTranscriber` to `asr`.

`src-tauri/src/bench.rs:77`:

```rust
    let asr = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )?;
```

`crates/yappr-core/examples/transcribe_file.rs:21`:

```rust
    let t = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )
    .expect("build transcriber");
```

`crates/yappr-core/tests/asr_fixture.rs` — both tests, replacing `SherpaTranscriber::new(&…models_dir(), 4)`:

```rust
    let t = yappr_core::asr::build(
        &yappr_core::paths::models_dir(),
        &yappr_core::config::AsrConfig::default(),
    )
    .expect("build transcriber");
```

and change the file's first line to `use yappr_core::asr::Transcriber;`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p yappr-core asr:: && cargo test -p yappr-core --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/yappr-core/src/asr.rs crates/yappr-core/src/server.rs \
        crates/yappr-core/examples/transcribe_file.rs \
        crates/yappr-core/tests/asr_fixture.rs src-tauri/src/bench.rs
git commit -m "feat(asr): build() resolves [asr] model to a transcriber"
```

---

### Task 4: The cache-aware streaming transcriber

**Files:**
- Modify: `crates/yappr-core/src/asr.rs` (replace the Task 3 stub)

**Interfaces:**
- Consumes: `SherpaStreamingTranscriber` stub (Task 3).
- Produces: a working `SherpaStreamingTranscriber::new(models_dir, rel_path, num_threads, language)`.

The three unknowns this task settles are listed in spec asr-model §8. They fail as *wrong text*, not as an error, so they are settled by the `#[ignore]`d test in Task 10 — not by reasoning here.

- [ ] **Step 1: Implement `new`**

Replace the stub:

```rust
impl SherpaStreamingTranscriber {
    pub fn new(
        models_dir: &Path,
        rel_path: &str,
        num_threads: i32,
        language: &str,
    ) -> Result<Self> {
        let dir = models_dir.join(rel_path);
        let p = |f: &str| -> Option<String> { Some(dir.join(f).to_string_lossy().into_owned()) };

        let mut config = OnlineRecognizerConfig::default();
        // The crate's default is 80; every NeMo FastConformer export in the
        // catalogue is 128. `OfflineRecognizer` reads this from the encoder's
        // ONNX metadata and the offline path never had to set it; the online
        // one takes it from here, and a wrong value produces plausible-looking
        // wrong text rather than an error. See spec asr-model §8.
        config.feat_config.feature_dim = 128;
        config.model_config = OnlineModelConfig {
            transducer: OnlineTransducerModelConfig {
                encoder: p("encoder.int8.onnx"),
                decoder: p("decoder.int8.onnx"),
                joiner: p("joiner.int8.onnx"),
            },
            tokens: p("tokens.txt"),
            num_threads,
            ..OnlineModelConfig::default()
        };
        // Endpointing cuts a live stream into utterances. The VAD already did
        // that, and this transcriber is handed one finished utterance.
        config.enable_endpoint = false;

        let recognizer = OnlineRecognizer::create(&config).context(
            "OnlineRecognizer::create returned None — check model paths and feature_dim",
        )?;

        Ok(Self {
            recognizer,
            language: language.to_string(),
        })
    }
}
```

- [ ] **Step 2: Run the build and the offline tests**

Run: `cargo test -p yappr-core --lib && cargo clippy -p yappr-core --all-targets`
Expected: PASS, clippy clean. The streaming path is not exercised without the model — Task 10 does that.

- [ ] **Step 3: Commit**

```bash
git add crates/yappr-core/src/asr.rs
git commit -m "feat(asr): cache-aware streaming transcriber, whole utterance at once"
```

---

### Task 5: Provisioning callers follow the selection

**Files:**
- Modify: `src-tauri/src/provision.rs` (:43 imports, :57-63 lookups, :95-143 cache, :109-112 `compute_missing_models`)

**Interfaces:**
- Consumes: `models::{all_artifacts, required_artifacts}` (Task 2), `config::AsrModel` (Task 1).
- Produces: no new public API; `missing_models_cached` becomes keyed on the selected model.

- [ ] **Step 1: Write the failing test**

Add to `provision.rs`'s test module, whose `use super::*;` brings in the
`AsrModel` imported at :43:

```rust
    /// The cache used to be "computed once per process", which was correct
    /// when the answer could not change. It can now: selecting a model in
    /// Settings changes which artifacts are required, and a stale `true`
    /// here reports an entirely absent model as ready — surfacing as a failed
    /// dictation rather than a missing-model message. See spec asr-model §4.
    #[test]
    fn the_missing_models_cache_is_keyed_on_the_selected_model() {
        set_cache_for_test(AsrModel::ParakeetTdtV3, Vec::new());
        assert_eq!(
            cached_for_test(AsrModel::ParakeetTdtV3),
            Some(Vec::new()),
            "the model it was computed for must hit"
        );
        assert_eq!(
            cached_for_test(AsrModel::Nemotron35),
            None,
            "a different selection must miss, not reuse the other model's answer"
        );
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p yappr provision::tests::the_missing_models_cache_is_keyed_on_the_selected_model`
Expected: FAIL — `cannot find function set_cache_for_test`.

- [ ] **Step 3: Key the cache on the selection**

Change the import at :43:

```rust
use yappr_core::config::AsrModel;
use yappr_core::models::{self, Artifact, LockFile};
```

Replace the two lookup helpers at :57-63 so they see the whole catalogue, not just what is required:

```rust
fn artifact_by_name(name: &str) -> Option<&'static Artifact> {
    models::all_artifacts().into_iter().find(|a| a.name == name)
}

fn artifact_by_url(url: &str) -> Option<&'static Artifact> {
    models::all_artifacts().into_iter().find(|a| a.url == url)
}
```

Replace the cache statics and accessors at :95-143:

```rust
/// The one cache behind [`missing_models_cached`], stored together with the
/// `[asr] model` it was computed for. `None` means "not computed yet, or
/// invalidated"; a stored entry for a *different* model is a miss, not a hit.
static MISSING_MODELS_CACHE: Mutex<Option<(AsrModel, Vec<String>)>> = Mutex::new(None);

fn missing_models_cache_lock() -> std::sync::MutexGuard<'static, Option<(AsrModel, Vec<String>)>> {
    MISSING_MODELS_CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
fn set_cache_for_test(model: AsrModel, missing: Vec<String>) {
    *missing_models_cache_lock() = Some((model, missing));
}

#[cfg(test)]
fn cached_for_test(model: AsrModel) -> Option<Vec<String>> {
    match missing_models_cache_lock().clone() {
        Some((cached_model, missing)) if cached_model == model => Some(missing),
        _ => None,
    }
}

/// Which of the *required* models are missing. `required` follows
/// `[asr] model`, so an unselected model left on disk is neither verified nor
/// reported (spec asr-model §4).
fn compute_missing_models(model: AsrModel) -> Result<Vec<String>, String> {
    let required = models::required_artifacts(model);
    if models::looks_present(&required) {
        return Ok(Vec::new());
    }
    let lock = LockFile::load().map_err(|e| format!("models.lock.toml: {e:#}"))?;
    models::verify(&lock, &required).map_err(|e| format!("Modelle prüfen: {e:#}"))
}

fn missing_models_cached(model: AsrModel) -> Result<Vec<String>, String> {
    if let Some((cached_model, missing)) = missing_models_cache_lock().clone() {
        if cached_model == model {
            return Ok(missing);
        }
    }
    let computed = compute_missing_models(model)?;
    *missing_models_cache_lock() = Some((model, computed.clone()));
    Ok(computed)
}
```

Add a helper and change `check_missing` to use it:

```rust
/// The selected model, read from disk on every call.
///
/// Deliberately not cached: the Settings dropdown writes `config.toml` the
/// moment it changes (invariant 9), and re-reading here is how that becomes
/// visible without a restart. A config that will not load is not this
/// module's problem to report — `load_or_quarantine` handles that at startup
/// (invariant 4) — so fall back to the default selection and still give the
/// Setup pane something useful to render.
fn selected_model() -> AsrModel {
    match yappr_core::config::Config::load() {
        Ok(c) => c.asr.model,
        Err(_) => yappr_core::config::AsrConfig::default().model,
    }
}

fn check_missing() -> Result<(Vec<&'static str>, Vec<String>), String> {
    let missing_prerequisites = crate::setup::check_prerequisites();
    let missing_models = missing_models_cached(selected_model())?;
    Ok((missing_prerequisites, missing_models))
}
```

- [ ] **Step 4: Update `run_setup`'s `download_all` call**

Find the `models::download_all(false, …)` call in `run_setup` and pass the required list, reading the selection the same way:

```rust
        let required = models::required_artifacts(selected_model());
        models::download_all(&required, false, &mut progress)
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p yappr && cargo clippy --workspace --all-targets`
Expected: PASS, clippy clean. The whole workspace now builds again.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/provision.rs
git commit -m "fix(provision): missing-model cache follows the selected ASR model"
```

---

### Task 6: The dropdown in the settings window

**Files:**
- Modify: `src/settings/schema.ts` (`LABELS` at :72, `HELP` at :137, `ENUMS` at :207, `FIELD_ORDER` at :269)

**Interfaces:**
- Consumes: the `[asr] model` / `[asr] language` keys (Task 1).
- Produces: nothing consumed by later tasks.

`schema.ts` only decides how a key is *shown*. A key with no entry here still renders by its JSON type, so this task cannot make a setting unreachable — only unlabelled.

- [ ] **Step 1: Add the enum choices**

In `ENUMS`, above the `inject.backend` line:

```ts
  "asr.model": ["parakeet-tdt-v3", "parakeet-unified-en", "nemotron-3.5"],
```

- [ ] **Step 2: Add the labels**

In `LABELS`, replacing the single `"asr.num_threads"` line with three:

```ts
  "asr.model": "Modell",
  "asr.language": "Sprache",
  "asr.num_threads": "Threads",
```

- [ ] **Step 3: Add the help text**

In `HELP`, above the `"asr.num_threads"` entry:

```ts
  "asr.model":
    "Welches Modell den gesprochenen Text erkennt. Parakeet TDT v3 ist mehrsprachig und die Voreinstellung. Parakeet Unified versteht nur Englisch, erkennt es aber genauer. Nemotron 3.5 ist mehrsprachig und die Grundlage für die spätere Live-Erkennung. Ein Wechsel lädt einmalig rund 500 MB herunter; bereits geladene Modelle bleiben liegen, ein Zurückwechseln geht also ohne Download.",
  "asr.language":
    "Nur für mehrsprachige Modelle. „auto“ lässt das Modell die Sprache selbst erkennen; ein Kürzel wie „de“ legt sie fest, was bei kurzen Diktaten zuverlässiger ist. Parakeet TDT v3 und Parakeet Unified ignorieren diese Einstellung.",
```

- [ ] **Step 4: Put the model first in the section**

In `FIELD_ORDER`, replace `asr: ["num_threads"],` with:

```ts
  asr: ["model", "language", "num_threads"],
```

- [ ] **Step 5: Build the frontend and check the pane**

Run: `bun run build`
Expected: `tsc` clean, `vite build` writes both entries to `dist/`.

Then run the app and open Einstellungen → Sprache:

```bash
cargo run --release -p yappr --features custom-protocol -- --settings
```

Expected: a **Modell** dropdown with the three spellings, a **Sprache** field, then **Threads**. Changing the dropdown writes `config.toml` immediately (no Save button — invariant 9). Confirm with `grep model ~/.local/state/yappr/config.toml`.

- [ ] **Step 6: Commit**

```bash
git add src/settings/schema.ts
git commit -m "feat(settings): ASR model dropdown in the Sprache pane"
```

---

### Task 7: Downloading a model chosen in Settings

**Files:**
- Create: `src/settings/model-download.tsx`
- Modify: `src/settings/wizard.tsx` (export `useSetup` and its two types)
- Modify: `src/Settings.tsx` (`flush` at :242, `SectionCard` at :750, the pane's section loop)

**Interfaces:**
- Consumes: the dropdown (Task 6); `setup_status` / `run_setup` / the `setup-progress` event, all unchanged.
- Produces: `export function AsrModelDownload({ model, revision }: { model: string; revision: number })`; `useSetup`, `SetupStatus` and `MissingModel` become exports of `wizard.tsx`.

Selecting a model autosaves immediately — a dropdown gets no debounce (invariant 9) — so the pane can be left showing a selected model that is not on disk. Without this task the only symptom is a failed dictation later.

- [ ] **Step 1: Export the setup hook from the wizard**

In `src/settings/wizard.tsx`, change three declarations to exports. Nothing else moves — the hook already owns the `setup-progress` listener and the single-flight `install`, and it is written to be independent of the step it renders in:

```tsx
export type MissingModel = { name: string; display: string };
export type SetupStatus = { ready: boolean; missing_prerequisites: string[]; missing_models: MissingModel[] };
export function useSetup() {
```

- [ ] **Step 2: Write the component**

Create `src/settings/model-download.tsx`:

```tsx
/// The download affordance for a model chosen in Settings rather than in the
/// wizard.
///
/// A dropdown autosaves the moment it changes (invariant 9), so the pane can
/// be left naming a model that is not on disk. Only the *selected* model is
/// ever downloaded (spec asr-model §4), which makes that an ordinary state
/// rather than a broken install — but the user has to be told, here, rather
/// than finding out when a dictation fails.
///
/// Reuses the wizard's `useSetup` wholesale: same `setup_status`, same
/// `run_setup`, same `setup-progress` listener, same single-flight guard.
/// A second, parallel implementation of the download UI is exactly how the
/// two would drift.
import { useEffect } from "react";

import { useSetup } from "./wizard";
import { Icon } from "./icons";

export function AsrModelDownload({ model, revision }: { model: string; revision: number }) {
  const setup = useSetup();
  const { check } = setup;

  // `revision` is bumped by a *successful* save, not by the local dropdown
  // value. `setup_status` answers from `config.toml` on disk, so re-checking
  // on `model` alone would race the save that is still in flight and report
  // the previous selection.
  useEffect(() => {
    void check();
  }, [check, model, revision]);

  const missing = setup.status?.missing_models ?? [];
  if (setup.checkError || missing.length === 0) return null;

  return (
    <div className="banner notice">
      <Icon name="warn" className="icon-sm" />
      <div className="setup-row__body">
        <span>
          {missing.map((m) => m.display).join(", ")} — noch nicht heruntergeladen.
        </span>
        {missing.map((m) => {
          const progress = setup.downloads[m.name];
          if (!progress) return null;
          const pct =
            progress.total === null
              ? null
              : Math.round((progress.done / progress.total) * 100);
          return (
            <span key={m.name} className="setup-row__head">
              {m.display}: {pct === null ? "lädt…" : `${pct} %`}
            </span>
          );
        })}
        {setup.installError && <span className="error">{setup.installError}</span>}
      </div>
      <button
        type="button"
        className="add"
        disabled={setup.installing}
        onClick={() => setup.install()}
      >
        {setup.installing ? "Lädt…" : "Jetzt laden"}
      </button>
    </div>
  );
}
```

`DownloadProgress` is already the type of `setup.downloads`' values in `wizard.tsx`; it does not need exporting because it is only read structurally here.

- [ ] **Step 3: Bump a revision on every successful save**

In `src/Settings.tsx`, add the state next to `saveState`:

```tsx
  const [savedRevision, setSavedRevision] = useState(0);
```

and in `flush`'s success branch, directly after `setSaveError(null);`:

```tsx
      setSavedRevision((n) => n + 1);
```

- [ ] **Step 4: Render it under the Spracherkennung section**

Give `SectionCard` an optional slot, rendered after its card:

```tsx
  extra,
```

in the destructured props, `extra?: React.ReactNode;` in the type, and immediately before the closing `</section>`:

```tsx
      {extra}
```

Then, where the pane maps its sections to `<SectionCard …/>`, pass the slot for `asr` only:

```tsx
            extra={
              name === "asr" ? (
                <AsrModelDownload
                  model={String((config as Record<string, any>)?.asr?.model ?? "")}
                  revision={savedRevision}
                />
              ) : undefined
            }
```

with `import { AsrModelDownload } from "./settings/model-download";` at the top.

- [ ] **Step 5: Build and exercise it**

Run: `bun run build`
Expected: `tsc` clean.

```bash
cargo run --release -p yappr --features custom-protocol -- --settings
```

In Einstellungen → Sprache, switch **Modell** to `parakeet-unified-en`. Expected: within a moment the banner appears naming the model as not downloaded, with a **Jetzt laden** button; pressing it shows a percentage and the banner disappears when the download finishes. Switching back to `parakeet-tdt-v3` makes the banner disappear immediately, with no download — the model is still on disk.

- [ ] **Step 6: Commit**

```bash
git add src/settings/model-download.tsx src/settings/wizard.tsx src/Settings.tsx
git commit -m "feat(settings): download a model chosen from the dropdown"
```

---

### Task 8: The dropdown in the first-run wizard

**Files:**
- Modify: `src/settings/wizard.tsx` (the `step === "models"` branch at :246-300)

**Interfaces:**
- Consumes: `setup_status` / `run_setup` / the `setup-progress` listener already owned by `Wizard` (unchanged); the `set_config` Tauri command.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Add the model choice above the download card**

Inside the `step === "models"` branch, directly after the `<p className="wizard-lead">…</p>` block, insert:

```tsx
              <div className="card">
                <label className="setup-row" htmlFor="wizard-asr-model">
                  <span>Spracherkennungs-Modell</span>
                  <select
                    id="wizard-asr-model"
                    value={asrModel}
                    disabled={setup.installing}
                    onChange={(e) => void chooseModel(e.target.value)}
                  >
                    <option value="parakeet-tdt-v3">Parakeet TDT v3 — mehrsprachig</option>
                    <option value="parakeet-unified-en">Parakeet Unified — nur Englisch</option>
                    <option value="nemotron-3.5">Nemotron 3.5 — mehrsprachig</option>
                  </select>
                </label>
                <p className="setup-command">
                  Es wird nur das ausgewählte Modell geladen. Ein Wechsel später in den
                  Einstellungen lädt das neue Modell nach.
                </p>
              </div>
```

- [ ] **Step 2: Add the state and the handler**

In the `Wizard` component, next to the existing `step` state:

```tsx
  const [asrModel, setAsrModel] = useState("parakeet-tdt-v3");

  // Read the current selection once, so the dropdown reflects config.toml
  // rather than assuming the default — the wizard reopens on a machine that
  // already chose a model when that model is missing (invariant 13).
  useEffect(() => {
    void (async () => {
      try {
        const res = (await invoke("get_config")) as { config?: { asr?: { model?: string } } };
        const current = res.config?.asr?.model;
        if (current) setAsrModel(current);
      } catch {
        // Leave the default showing; the Modelle step still works.
      }
    })();
  }, []);

  /// Writes the selection, then re-runs the status check so the missing-model
  /// list below is about the model the user just picked. A single-leaf patch,
  /// which `config_write::save_config` merges rather than renders — the same
  /// path `wizard_finish`'s backend patch takes (invariant 9).
  const chooseModel = async (model: string) => {
    setAsrModel(model);
    await invoke("set_config", { config: { asr: { model } } });
    await setup.check();
  };
```

- [ ] **Step 3: Build and walk the wizard**

Run: `bun run build`
Expected: `tsc` clean.

Then:

```bash
mv ~/.local/state/yappr/wizard-done ~/.local/state/yappr/wizard-done.bak
cargo run --release -p yappr --features custom-protocol -- --wizard
```

Expected: on the Modelle step, changing the dropdown to `nemotron-3.5` makes the missing-model list switch to naming the Nemotron artifact instead of Parakeet, without a restart. Restore with `mv ~/.local/state/yappr/wizard-done.bak ~/.local/state/yappr/wizard-done`.

- [ ] **Step 4: Commit**

```bash
git add src/settings/wizard.tsx
git commit -m "feat(wizard): choose the ASR model before downloading it"
```

---

### Task 9: Pin the new models

**Files:**
- Modify: `crates/yappr-core/models.lock.toml`
- Modify: `crates/yappr-core/src/models.rs` (tests module)

**Interfaces:**
- Consumes: the catalogue (Task 2).
- Produces: a `hashes` entry for every `all_artifacts()` name.

`download_all` refuses an unpinned artifact unless `--update-lock` was given, so until this task lands, the two new models are entries no user can download.

- [ ] **Step 1: Write the failing test**

Add to `models.rs`'s `mod tests`:

```rust
#[test]
fn every_catalogue_artifact_is_pinned_in_the_committed_lock_file() {
    // download_all refuses an unpinned artifact unless --update-lock was
    // given, so an unpinned entry is one no user can ever install — and the
    // failure appears only for whoever selects it. Catch it here instead.
    let lock = LockFile::compiled_in().expect("the committed lock file must parse");
    for a in all_artifacts() {
        assert!(
            lock.hashes.contains_key(a.name),
            "{} ({}) has no pin in models.lock.toml",
            a.name,
            a.display
        );
    }
}
```

`LockFile::compiled_in` is currently private; change it to `pub(crate) fn compiled_in`.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p yappr-core models::tests::every_catalogue_artifact_is_pinned_in_the_committed_lock_file`
Expected: FAIL — `parakeet-unified-en (…) has no pin in models.lock.toml`.

- [ ] **Step 3: Compute the two hashes**

The pin is the sha256 of the *extracted* `encoder.int8.onnx` (see `hash_target`). Download to a scratch directory so `~/.local/share/yappr/models` is not filled with ~1 GB of models the user did not choose:

```bash
scratch=$(mktemp -d)
base=https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models
for f in sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming \
         sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11; do
  curl -SL --output "$scratch/$f.tar.bz2" "$base/$f.tar.bz2"
  tar xjf "$scratch/$f.tar.bz2" -C "$scratch"
  echo "$f  $(sha256sum "$scratch/$f/encoder.int8.onnx" | cut -d' ' -f1)"
done
```

Expected: two 64-character hex digests. Keep `$scratch` — Task 10 needs the extracted nemotron directory.

- [ ] **Step 4: Write the pins**

Add to `crates/yappr-core/models.lock.toml`, keeping the keys sorted (the file is written by `toml::to_string_pretty` over a `BTreeMap`, so committed order should match):

```toml
[hashes]
nemotron-3.5-560ms = "<digest from step 3>"
parakeet = "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247"
parakeet-unified-en = "<digest from step 3>"
s1-mini = "1cc4f7e0bad193ed98da7267f26be074f90ad42f2a5bdaecf5521772f0c227a3"
silero = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p yappr-core models::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/yappr-core/models.lock.toml crates/yappr-core/src/models.rs
git commit -m "chore(models): pin parakeet-unified-en and nemotron-3.5-560ms"
```

---

### Task 10: Prove the streaming flavour actually transcribes

**Files:**
- Modify: `crates/yappr-core/tests/asr_fixture.rs`

**Interfaces:**
- Consumes: `asr::build` (Task 3), `SherpaStreamingTranscriber` (Task 4), the pins (Task 9).
- Produces: nothing consumed by later tasks.

This is where spec asr-model §8's three unknowns are settled — `feature_dim`, `model_type`, and the language option's key. All three fail as plausible-looking wrong text, so only a real transcription catches them.

- [ ] **Step 1: Install the nemotron model**

```bash
dest=~/.local/share/yappr/models/nemotron-3.5-asr-streaming-0.6b-560ms-int8
mv "$scratch/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11" "$dest"
ls "$dest"   # encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt
```

- [ ] **Step 2: Write the failing test**

Append to `crates/yappr-core/tests/asr_fixture.rs`:

```rust
#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn the_streaming_model_transcribes_the_german_fixture_in_one_pass() {
    // The whole point of the CacheAwareStreaming flavour as yappr uses it:
    // one buffer in, one final result out, no partials. If feature_dim,
    // model_type or the language option are wrong, this returns confident
    // nonsense rather than an error — which is why the assertion is on
    // content, not on Ok-ness. See spec asr-model §8.
    let samples = read_wav_16k_mono("fixtures/hallo_german.wav");
    let cfg = yappr_core::config::AsrConfig {
        model: yappr_core::config::AsrModel::Nemotron35,
        language: "de".to_string(),
        ..Default::default()
    };
    let t = yappr_core::asr::build(&yappr_core::paths::models_dir(), &cfg)
        .expect("build streaming transcriber");
    let text = t.transcribe(&samples).expect("transcribe");

    assert!(!text.trim().is_empty(), "got empty transcript");
    // Real words, not token soup: at least two runs of three or more
    // letters. A wrong feature_dim reliably produces neither.
    let wordish = text
        .split_whitespace()
        .filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3)
        .count();
    assert!(wordish >= 2, "does not look like German words: {text}");
}

#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn the_streaming_transcriber_returns_empty_for_empty_input() {
    let cfg = yappr_core::config::AsrConfig {
        model: yappr_core::config::AsrModel::Nemotron35,
        ..Default::default()
    };
    let t = yappr_core::asr::build(&yappr_core::paths::models_dir(), &cfg)
        .expect("build streaming transcriber");
    assert_eq!(t.transcribe(&[]).expect("transcribe"), "");
}
```

- [ ] **Step 3: Run it**

Run: `cargo test -p yappr-core --test asr_fixture -- --ignored --nocapture`
Expected: PASS.

If `the_streaming_model_transcribes_the_german_fixture_in_one_pass` fails, work the three §8 unknowns in this order, re-running after each:

1. `OnlineRecognizer::create` returned `None` → the paths or `feature_dim` are wrong. Try `config.model_config.debug = true` to get sherpa's own diagnostics on stderr.
2. Creation succeeded but the text is empty → the decode loop. `input_finished()` must be called before the `is_ready` loop, not after.
3. Creation succeeded and the text is nonsense → `feature_dim`. Read the real value with `python3 -c "import onnx;print([(p.key,p.value) for p in onnx.load('$dest/encoder.int8.onnx').metadata_props])"` and use what it reports.
4. Text is right but the language is wrong on a multilingual clip → the `set_option` key. Check `stream.get_option("language")` round-trips; if it does not, the key name is wrong.

Record whatever the answer turns out to be as a comment in `SherpaStreamingTranscriber::new` — that is the durable output of this task.

- [ ] **Step 4: Run the whole gate**

Run: `cargo test --workspace && cargo test --workspace -- --ignored && cargo clippy --workspace --all-targets`
Expected: all PASS, clippy clean. The `--ignored` half is not optional — it is the only thing that catches a C++ ABI mismatch between sherpa-onnx and llama.cpp (see CLAUDE.md's environment gotchas).

- [ ] **Step 5: Commit**

```bash
git add crates/yappr-core/tests/asr_fixture.rs crates/yappr-core/src/asr.rs
git commit -m "test(asr): the streaming flavour transcribes in one pass"
```

---

### Task 11: Documentation

**Files:**
- Modify: `CLAUDE.md` (opening paragraph, Architecture section)
- Modify: `docs/HANDOVER.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing.

- [ ] **Step 1: Update `CLAUDE.md`'s opening paragraph**

It currently reads "transcribed (Parakeet TDT via `sherpa-onnx`)". Replace with:

> transcribed (one of several `sherpa-onnx` models, chosen by `[asr] model` — Parakeet TDT 0.6b v3 by default)

- [ ] **Step 2: Add a paragraph to the Architecture section**

After the `[models]` paragraph:

> `[asr] model` picks which speech-recognition model runs, from the catalogue in
> `models.rs` (`ASR_MODELS`). Provisioning follows the selection rather than
> requiring every artifact: `required_artifacts` is the support pair (Silero,
> S1-mini) plus the one selected model, and `verify`/`looks_present`/`download_all`
> all take that list. A model left on disk by an earlier selection is never
> verified, redownloaded or deleted. Two flavours exist behind
> `asr::build`: `Offline` (sherpa's `OfflineRecognizer`, as always) and
> `CacheAwareStreaming` (`OnlineRecognizer`, fed the whole utterance at once —
> yappr has no streaming UI, and the nemotron export is published no other way).
> See `docs/superpowers/specs/2026-09-08-asr-model-selection-design.md`.

- [ ] **Step 3: Update `docs/HANDOVER.md`**

Add to its state-of-play list, under what has been verified on real hardware, whatever Task 10 actually established — including which of the §8 unknowns needed a non-default value.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md docs/HANDOVER.md
git commit -m "docs: selectable ASR models"
```

---

### Task 12: The primeline entry (blocked on an out-of-band export)

**Files:**
- Modify: `crates/yappr-core/src/config.rs` (`AsrModel`), `crates/yappr-core/src/models.rs` (`ASR_MODELS`, array length), `crates/yappr-core/models.lock.toml`, `src/settings/schema.ts` (`ENUMS`, `HELP`), `src/settings/wizard.tsx` (the `<select>`)

**Interfaces:**
- Consumes: everything above.
- Produces: `AsrModel::ParakeetPrimelineDe`, spelled `parakeet-primeline-de`.

**This task cannot start until the export exists.** Spec asr-model §9 has the procedure; it runs outside this repository:

```bash
pip install nemo_toolkit['asr'] "numpy<2" onnx==1.17.0 onnxruntime==1.17.1 \
            kaldi-native-fbank librosa soundfile
curl -SL -O https://huggingface.co/primeline/parakeet-primeline/resolve/main/2_95_WER.nemo
mv 2_95_WER.nemo parakeet-tdt-0.6b-v3.nemo   # what export_onnx.py looks for
curl -SL -O https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/scripts/nemo/parakeet-tdt-0.6b-v3/export_onnx.py
curl -SL -O https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/scripts/nemo/generate_bpe_vocab.py
python3 ./export_onnx.py
mkdir parakeet-primeline-de-int8
mv encoder.int8.onnx decoder.int8.onnx joiner.int8.onnx tokens.txt parakeet-primeline-de-int8/
tar cjf parakeet-primeline-de-int8.tar.bz2 parakeet-primeline-de-int8
```

`extract_tar_bz2` flattens exactly one top-level directory and fails otherwise, which is why the `mkdir` matters. Upload the tarball beside `s1-mini-de-v3` on the project's Hugging Face account.

- [ ] **Step 1: Add the enum variant**

In `config.rs`'s `AsrModel`:

```rust
    /// primeline-parakeet — a German finetune of Parakeet TDT v3, and the
    /// most accurate German model in the catalogue (2.95 % vs 3.64 % average
    /// WER). Not the default: see spec asr-model §2.
    #[serde(rename = "parakeet-primeline-de")]
    ParakeetPrimelineDe,
```

Add `("parakeet-primeline-de", AsrModel::ParakeetPrimelineDe)` to `every_asr_model_spelling_round_trips_from_toml`, and `AsrModel::ParakeetPrimelineDe` to both loops in `models.rs`'s tests.

- [ ] **Step 2: Add the catalogue entry**

Change `pub static ASR_MODELS: [AsrModelSpec; 3]` to `[AsrModelSpec; 4]` and append:

```rust
    AsrModelSpec {
        key: AsrModel::ParakeetPrimelineDe,
        artifact: Artifact {
            name: "parakeet-primeline-de",
            url: "<the uploaded tarball's resolve/main URL>",
            display: "primeline Parakeet 0.6b (int8)",
            rel_path: "parakeet-primeline-de-int8",
            archive: true,
        },
        flavor: AsrFlavor::Offline,
        display: "primeline Parakeet 0.6b — nur Deutsch, genaueste deutsche Erkennung",
    },
```

- [ ] **Step 3: Pin it**

```bash
sha256sum parakeet-primeline-de-int8/encoder.int8.onnx
```

Add `parakeet-primeline-de = "<digest>"` to `models.lock.toml`.

- [ ] **Step 4: Add it to both UIs**

`schema.ts`'s `ENUMS`:

```ts
  "asr.model": [
    "parakeet-tdt-v3",
    "parakeet-primeline-de",
    "parakeet-unified-en",
    "nemotron-3.5",
  ],
```

Extend the `"asr.model"` `HELP` string with a sentence naming primeline as the most accurate German option. Add the matching `<option value="parakeet-primeline-de">primeline Parakeet — nur Deutsch</option>` to `wizard.tsx`.

- [ ] **Step 5: Verify against the third-party export**

`x-ian/sherpa-onnx-parakeet-primeline-de-int8` is an independent export of the same checkpoint. Transcribe `fixtures/hallo_german.wav` with both and compare — the texts should agree. A disagreement means our export is wrong, not that theirs is authoritative.

- [ ] **Step 6: Run the gate and commit**

```bash
cargo test --workspace && cargo test --workspace -- --ignored && cargo clippy --workspace --all-targets
git add -A && git commit -m "feat(asr): primeline-parakeet, the German model"
```
