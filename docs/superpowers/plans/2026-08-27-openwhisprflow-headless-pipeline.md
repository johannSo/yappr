# OpenWhisprFlow Headless Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A working push-to-talk dictation tool — hold a Hyprland keybind, speak, release, and cleaned text is typed into the focused window — with no GUI.

**Architecture:** A long-lived `owf-daemon` process owns the audio device and both models, and listens on a Unix socket. A tiny `owf-ctl` binary, invoked by Hyprland keybinds, sends it one-line NDJSON commands. Speech runs through Silero VAD trimming, Parakeet TDT 0.6b v3 (in-process via `sherpa-onnx`), language detection, and s1-mini normalization (over HTTP to a supervised `llama-server` child), then through a guardrail that falls back to raw ASR text before `wtype` types it.

**Tech Stack:** Rust 2021, `sherpa-onnx` 1.13.6, `cpal` 0.18, `rubato` 5.0, `whatlang` 0.18, `ureq` 3.4, `httpmock` 0.8, `hound` 3.5, `sha2` 0.11, `regex` 1.13, `toml` 1.1, `serde` 1, `anyhow` 1, `thiserror` 2, `tracing` 0.1, `notify-rust` 4.18, `dirs` 6. External binaries: `llama-server`, `wtype`, `wl-copy`, `hyprctl`.

**Spec:** `docs/superpowers/specs/2026-08-27-openwhisprflow-design.md`

This plan covers spec milestones **M0 and M1**. M2 (Tauri overlay) and M3 (style tuning) are a separate plan written after M1 lands, because the overlay's design should be informed by the real latency numbers Task 3 produces.

## Global Constraints

Copied verbatim from the spec. Every task's requirements implicitly include these.

- **No async runtime.** All HTTP is blocking (`ureq`). Do not add `tokio`, `async-std`, or `wiremock`. The pipeline is batch and synchronous by design. (Deviation from spec §16, which named `wiremock`; `httpmock` is the blocking equivalent.)
- **s1-mini system prompt is verbatim and must never be paraphrased:**
  `You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.`
- **s1-mini requires `enable_thinking: false` and `temperature: 0`.** Both are passed on the `llama-server` command line *and* per-request. Omitting the thinking flag produces blank output.
- **s1-mini's license carries a naming clause:** the model must be referred to as "S1-mini" by "Superwhisper", with that exact capitalization, in any user-facing text or documentation.
- **Control-line axes are closed enums.** Styling ∈ {`casual`, `semi-casual`, `semi-formal`, `formal`}; Structure ∈ {`prose`, `lists`}; Context ∈ {`general`, `email`}. Invalid values are config-load errors, never runtime errors.
- **Threading: `num_threads = 4`** everywhere (physical cores; hyperthreads make both ONNX Runtime and llama.cpp slower on this CPU).
- **Audio is always 16 kHz, mono, `f32`** by the time it leaves `capture.rs`.
- **Invariant: once audio has been transcribed, the user gets text.** Every failure downstream of ASR degrades to raw output. Never discard a transcript.
- **No network calls except model downloads** in `owf-ctl setup` and localhost requests to `llama-server`.
- **Target platform is Linux + Wayland + Hyprland only.** Do not add cross-platform branches.

## File Structure

New Cargo workspace at the repo root. The existing `src-tauri` Tauri crate becomes a workspace member but is not touched in this plan.

```
Cargo.toml                          # [workspace] members
crates/owf-core/
  Cargo.toml
  src/
    lib.rs                          # re-exports; no logic
    paths.rs                        # XDG path resolution, single source of truth
    config.rs                       # Config structs, load, validate
    style.rs                        # Styling/Structure/Context enums, StyleAxes, control_line()
    guardrail.rs                    # tokenize, overlap, ngram loop, verdict, rule_based_fallback
    lang.rs                         # LanguageDetector trait, WhatlangDetector, Lang enum
    asr.rs                          # Transcriber trait, SherpaTranscriber
    vad.rs                          # Trimmer trait, SileroTrimmer
    capture.rs                      # Recorder (cpal + rubato), RMS events
    normalize.rs                    # Normalizer trait, S1MiniClient, max_tokens_for
    llama.rs                        # LlamaServer supervisor (spawn, health, backoff, port)
    inject.rs                       # TextInjector trait, WtypeInjector, ClipboardInjector, MockInjector
    hypr.rs                         # active_window_class(), print_hypr_config()
    models.rs                       # ModelSet, download, sha256, models.lock.toml
    proto.rs                        # Request/Response NDJSON types, State enum
    pipeline.rs                     # Pipeline::run_utterance, orchestration
  tests/
    guardrail_table.rs
    normalize_http.rs
    asr_fixture.rs
    pipeline_e2e.rs
  fixtures/
    hello_english.wav
    hallo_german.wav
crates/owf-cli/
  Cargo.toml
  src/
    bin/owf-daemon.rs               # socket server, state machine, owns Pipeline
    bin/owf-ctl.rs                   # socket client + local subcommands
    bin/owf-bench.rs                 # M0 latency measurement
src-tauri/                          # existing, untouched by this plan
docs/superpowers/specs/2026-08-27-openwhisprflow-design.md
```

Files are split by responsibility, not layer. `guardrail.rs`, `style.rs`, `lang.rs`, and the `max_tokens_for` half of `normalize.rs` are pure functions with no I/O — that is where the correctness risk lives and where every test-first task below is aimed.

---

### Task 1: Workspace scaffold and prerequisites

**Files:**
- Create: `Cargo.toml`, `crates/owf-core/Cargo.toml`, `crates/owf-core/src/lib.rs`, `crates/owf-core/src/paths.rs`, `crates/owf-cli/Cargo.toml`, `crates/owf-cli/src/bin/owf-ctl.rs`
- Modify: `.gitignore`, `src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces: `owf_core::paths::{config_file, models_dir, state_dir, runtime_socket, runtime_lock, runtime_port, log_file, rejections_file}`, each `-> PathBuf`.

- [ ] **Step 1: Install prerequisites**

These are not installed on the target machine. Run:

```bash
sudo pacman -S --needed rustup llama-cpp
rustup default stable
```

Verify:

```bash
cargo --version && llama-server --version
```

Expected: a cargo version line, and a llama-server version banner. If `llama-server` is not on PATH after installing `llama-cpp`, find it with `pacman -Ql llama-cpp | grep 'bin/'` and note the absolute path — it goes in config later.

- [ ] **Step 2: Create the workspace root**

Create `Cargo.toml` at the repo root:

```toml
[workspace]
resolver = "2"
members = ["crates/owf-core", "crates/owf-cli", "src-tauri"]

[workspace.package]
edition = "2021"
version = "0.1.0"

[workspace.dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "1.1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
dirs = "6"
regex = "1.13"
ureq = { version = "3.4", features = ["json"] }
```

- [ ] **Step 3: Create `owf-core`**

`crates/owf-core/Cargo.toml`:

```toml
[package]
name = "owf-core"
edition.workspace = true
version.workspace = true

[dependencies]
anyhow.workspace = true
thiserror.workspace = true
serde.workspace = true
serde_json.workspace = true
toml.workspace = true
tracing.workspace = true
dirs.workspace = true
regex.workspace = true
ureq.workspace = true
```

`crates/owf-core/src/lib.rs`:

```rust
pub mod paths;
```

`crates/owf-core/src/paths.rs`:

```rust
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
```

- [ ] **Step 4: Create `owf-cli` with a stub binary**

`crates/owf-cli/Cargo.toml`:

```toml
[package]
name = "owf-cli"
edition.workspace = true
version.workspace = true

[dependencies]
owf-core = { path = "../owf-core" }
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true

[[bin]]
name = "owf-ctl"
path = "src/bin/owf-ctl.rs"
```

`crates/owf-cli/src/bin/owf-ctl.rs`:

```rust
fn main() {
    println!("socket: {}", owf_core::paths::runtime_socket().display());
}
```

- [ ] **Step 5: Make `src-tauri` a workspace member**

Add to `src-tauri/Cargo.toml`, immediately after the `[package]` block's `edition` line:

```toml
[package]
# ... existing keys unchanged ...

[lints]
workspace = false
```

Then edit `.gitignore` — a workspace shares one `target/` at the root:

```
# Rust
/target
```

Verify `src-tauri/.gitignore` still ignores its own `target` (harmless if it does).

- [ ] **Step 6: Verify the workspace builds and tests**

Run: `cargo test -p owf-core`
Expected: PASS, 2 tests.

Run: `cargo build -p owf-cli`
Expected: builds; `./target/debug/owf-ctl` prints a socket path.

Note: `cargo build` with no `-p` will also build the Tauri crate, which is slow and unnecessary in this plan. Always pass `-p owf-core` or `-p owf-cli`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates .gitignore src-tauri/Cargo.toml
git commit -m "feat: cargo workspace with owf-core and owf-cli crates

Adds XDG path resolution as the single source of truth for every
file location the daemon and ctl must agree on."
```

---

### Task 2: Model provisioning (`owf-ctl setup`)

Downloads ~1.1 GB of weights and pins their hashes. This must land before Task 3, which needs the models to exist.

**Files:**
- Create: `crates/owf-core/src/models.rs`
- Modify: `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`, `crates/owf-cli/src/bin/owf-ctl.rs`
- Test: inline `#[cfg(test)]` in `models.rs`

**Interfaces:**
- Consumes: `owf_core::paths::models_dir`.
- Produces:
  - `owf_core::models::Artifact { pub name: &'static str, pub url: &'static str, pub rel_path: &'static str, pub archive: bool }`
  - `owf_core::models::ARTIFACTS: [Artifact; 3]`
  - `owf_core::models::sha256_file(path: &Path) -> anyhow::Result<String>`
  - `owf_core::models::verify(lock: &LockFile) -> anyhow::Result<Vec<String>>` (returns names of missing/mismatched artifacts)
  - `owf_core::models::LockFile { pub hashes: BTreeMap<String, String> }` with `load()/save()`
  - `owf_core::models::download_all(update_lock: bool, progress: &mut dyn FnMut(&str, u64, Option<u64>)) -> anyhow::Result<()>`

- [ ] **Step 1: Add dependencies**

In `crates/owf-core/Cargo.toml` add:

```toml
sha2 = "0.11"
tar = "0.4"
bzip2 = "0.5"
```

- [ ] **Step 2: Write the failing tests**

Create the test module at the bottom of `crates/owf-core/src/models.rs` (write this before the implementation):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sha256_of_known_content_matches() {
        let dir = std::env::temp_dir().join("owf-test-sha");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.txt");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"abc").unwrap();
        drop(f);
        // Well-known SHA-256 of the three bytes "abc".
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn every_artifact_has_a_distinct_name_and_https_url() {
        let mut names = std::collections::HashSet::new();
        for a in ARTIFACTS.iter() {
            assert!(names.insert(a.name), "duplicate artifact name: {}", a.name);
            assert!(a.url.starts_with("https://"), "{} is not https", a.name);
        }
        assert_eq!(ARTIFACTS.len(), 3);
    }

    #[test]
    fn lockfile_roundtrips() {
        let mut lock = LockFile::default();
        lock.hashes.insert("silero".into(), "deadbeef".into());
        let s = toml::to_string(&lock).unwrap();
        let back: LockFile = toml::from_str(&s).unwrap();
        assert_eq!(back.hashes.get("silero").map(String::as_str), Some("deadbeef"));
    }

    #[test]
    fn verify_reports_missing_artifacts_rather_than_erroring() {
        let lock = LockFile::default(); // no hashes recorded
        let missing = verify(&lock).unwrap();
        // With an empty lock, every artifact counts as unverified.
        assert_eq!(missing.len(), ARTIFACTS.len());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core models`
Expected: FAIL — `models` module does not exist / unresolved imports.

- [ ] **Step 4: Implement `models.rs`**

```rust
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::paths;

pub struct Artifact {
    pub name: &'static str,
    pub url: &'static str,
    /// Path relative to models_dir() once provisioned.
    pub rel_path: &'static str,
    /// True when the download is a .tar.bz2 that must be extracted.
    pub archive: bool,
}

pub static ARTIFACTS: [Artifact; 3] = [
    Artifact {
        name: "parakeet",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        rel_path: "parakeet-tdt-0.6b-v3-int8",
        archive: true,
    },
    Artifact {
        name: "silero",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        rel_path: "silero_vad.onnx",
        archive: false,
    },
    Artifact {
        name: "s1-mini",
        url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf",
        rel_path: "s1-mini-q4_k_m.gguf",
        archive: false,
    },
];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
}

impl LockFile {
    pub fn path() -> PathBuf {
        paths::models_dir().join("models.lock.toml")
    }

    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(&p)?;
        Ok(toml::from_str(&s)?)
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// For an archive artifact the hash is taken over the extracted encoder file,
/// which is the piece that actually matters and the only one large enough for
/// corruption to be plausible.
fn hash_target(a: &Artifact) -> PathBuf {
    let base = paths::models_dir().join(a.rel_path);
    if a.archive {
        base.join("encoder.int8.onnx")
    } else {
        base
    }
}

/// Returns the names of artifacts that are absent, or present with a hash that
/// disagrees with the lock file.
pub fn verify(lock: &LockFile) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    for a in ARTIFACTS.iter() {
        let target = hash_target(a);
        if !target.exists() {
            bad.push(a.name.to_string());
            continue;
        }
        match lock.hashes.get(a.name) {
            None => bad.push(a.name.to_string()),
            Some(expected) => {
                if &sha256_file(&target)? != expected {
                    bad.push(a.name.to_string());
                }
            }
        }
    }
    Ok(bad)
}

fn download_to(url: &str, dest: &Path, progress: &mut dyn FnMut(&str, u64, Option<u64>)) -> Result<()> {
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let tmp = dest.with_extension("part");
    let resp = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut out, &buf[..n])?;
        done += n as u64;
        progress(url, done, total);
    }
    drop(out);
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

fn extract_tar_bz2(archive: &Path, into: &Path) -> Result<()> {
    let f = std::fs::File::open(archive)?;
    let dec = bzip2::read::BzDecoder::new(f);
    let mut tar = tar::Archive::new(dec);
    // The upstream tarball contains one top-level directory; flatten it into
    // `into` so paths are stable regardless of upstream naming.
    let staging = into.with_extension("staging");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    tar.unpack(&staging)?;
    let top = std::fs::read_dir(&staging)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.path())
        .context("archive had no top-level directory")?;
    if into.exists() {
        std::fs::remove_dir_all(into)?;
    }
    std::fs::rename(&top, into)?;
    std::fs::remove_dir_all(&staging).ok();
    Ok(())
}

pub fn download_all(
    update_lock: bool,
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<()> {
    let mut lock = LockFile::load()?;
    for a in ARTIFACTS.iter() {
        let target = hash_target(a);
        if target.exists() {
            if let Some(expected) = lock.hashes.get(a.name) {
                if &sha256_file(&target)? == expected {
                    continue; // already good
                }
            } else if !update_lock {
                bail!(
                    "{} is present but not pinned in models.lock.toml; \
                     re-run with --update-lock to pin it",
                    a.name
                );
            }
        }

        let dest = paths::models_dir().join(a.rel_path);
        if a.archive {
            let tmp = paths::models_dir().join(format!("{}.tar.bz2", a.name));
            download_to(a.url, &tmp, progress)?;
            extract_tar_bz2(&tmp, &dest)?;
            std::fs::remove_file(&tmp).ok();
        } else {
            download_to(a.url, &dest, progress)?;
        }

        let got = sha256_file(&hash_target(a))?;
        match lock.hashes.get(a.name) {
            Some(expected) if expected != &got => {
                bail!("checksum mismatch for {}: expected {expected}, got {got}", a.name)
            }
            Some(_) => {}
            None => {
                if update_lock {
                    lock.hashes.insert(a.name.to_string(), got);
                } else {
                    bail!("{} has no pinned hash; re-run with --update-lock", a.name);
                }
            }
        }
    }
    if update_lock {
        lock.save()?;
    }
    Ok(())
}
```

Add `pub mod models;` to `crates/owf-core/src/lib.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core models`
Expected: PASS, 4 tests.

- [ ] **Step 6: Wire `owf-ctl setup`**

Replace `crates/owf-cli/src/bin/owf-ctl.rs`:

```rust
use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match refs.as_slice() {
        ["setup"] => setup(false),
        ["setup", "--update-lock"] => setup(true),
        _ => {
            eprintln!("usage: owf-ctl setup [--update-lock]");
            std::process::exit(2);
        }
    }
}

fn setup(update_lock: bool) -> Result<()> {
    let mut last = 0u64;
    owf_core::models::download_all(update_lock, &mut |url, done, total| {
        // Report roughly every 8 MB so a 622 MB download does not spam the terminal.
        if done - last > 8 << 20 || Some(done) == total {
            last = done;
            match total {
                Some(t) => eprintln!("  {:>5.1}%  {}", 100.0 * done as f64 / t as f64, url),
                None => eprintln!("  {} MB  {}", done >> 20, url),
            }
        }
    })?;
    println!("models ready in {}", owf_core::paths::models_dir().display());
    Ok(())
}
```

- [ ] **Step 7: Download the models for real and pin them**

Run: `cargo run -p owf-cli --bin owf-ctl -- setup --update-lock`
Expected: ~1.1 GB downloaded, then `models ready in /home/<user>/.local/share/openwhisprflow/models`.

Verify the layout:

```bash
ls ~/.local/share/openwhisprflow/models/parakeet-tdt-0.6b-v3-int8/
cat ~/.local/share/openwhisprflow/models/models.lock.toml
```

Expected: `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`; and a lock file with three hashes.

- [ ] **Step 8: Commit the lock file**

The lock file is the pinned record every future install verifies against, so it belongs in the repo.

```bash
cp ~/.local/share/openwhisprflow/models/models.lock.toml crates/owf-core/models.lock.toml
git add crates/owf-core/src/models.rs crates/owf-core/src/lib.rs \
        crates/owf-core/Cargo.toml crates/owf-core/models.lock.toml \
        crates/owf-cli/src/bin/owf-ctl.rs
git commit -m "feat: model provisioning with pinned SHA-256 checksums

owf-ctl setup downloads Parakeet TDT 0.6b v3 int8, Silero VAD, and
S1-mini by Superwhisper, verifying each against models.lock.toml."
```

Note: `download_all` reads the lock from `models_dir()`. Add a follow-up in Task 4's config work only if the copy in `crates/owf-core/models.lock.toml` needs to seed a fresh machine; for now it is a committed record.

---

### Task 3: ASR spike and latency measurement (M0 gate)

**This is the risk-retiring task. Timebox the spike to one hour.** If `sherpa-onnx` cannot load Parakeet v3 in that time, stop and escalate: the spec's §17.1 fallback is a Python ASR sidecar behind the same `Transcriber` trait, and only `asr.rs` changes.

**Files:**
- Create: `crates/owf-core/src/asr.rs`, `crates/owf-cli/src/bin/owf-bench.rs`, `crates/owf-core/tests/asr_fixture.rs`, `crates/owf-core/fixtures/hello_english.wav`
- Modify: `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`, `crates/owf-cli/Cargo.toml`

**Interfaces:**
- Consumes: `owf_core::paths::models_dir`.
- Produces:
  - `owf_core::asr::Transcriber` trait: `fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String>`
  - `owf_core::asr::SherpaTranscriber::new(models_dir: &Path, num_threads: i32) -> anyhow::Result<Self>`
  - `owf_core::asr::SAMPLE_RATE: i32 = 16_000`

- [ ] **Step 1: Add dependencies**

`crates/owf-core/Cargo.toml`:

```toml
sherpa-onnx = "1.13.6"
hound = "3.5"
```

`crates/owf-cli/Cargo.toml` — add a second binary:

```toml
[[bin]]
name = "owf-bench"
path = "src/bin/owf-bench.rs"
```

- [ ] **Step 2: Record a fixture WAV**

The fixture must be 16 kHz mono. Record roughly ten seconds of English speech:

```bash
mkdir -p crates/owf-core/fixtures
pw-record --rate 16000 --channels 1 --format s16 \
  crates/owf-core/fixtures/hello_english.wav
# speak for ~10 seconds, then Ctrl-C
```

Say something with numbers and a self-correction, because that is what exercises S1-mini later — for example: *"um so the meeting is at uh three thirty no sorry four thirty on tuesday and you can reach me at jay at example dot com"*.

Verify: `ffprobe crates/owf-core/fixtures/hello_english.wav 2>&1 | grep Audio`
Expected: `pcm_s16le, 16000 Hz, mono`.

- [ ] **Step 3: Write the failing integration test**

Create `crates/owf-core/tests/asr_fixture.rs`:

```rust
use owf_core::asr::{SherpaTranscriber, Transcriber};

fn read_wav_16k_mono(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open fixture");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16_000, "fixture must be 16 kHz");
    assert_eq!(spec.channels, 1, "fixture must be mono");
    r.samples::<i16>()
        .map(|s| s.expect("sample") as f32 / 32768.0)
        .collect()
}

#[test]
#[ignore = "requires downloaded models; run with --ignored"]
fn transcribes_the_english_fixture() {
    let samples = read_wav_16k_mono("fixtures/hello_english.wav");
    let t = SherpaTranscriber::new(&owf_core::paths::models_dir(), 4)
        .expect("build transcriber");
    let text = t.transcribe(&samples).expect("transcribe");

    // Assert on content that must survive any reasonable ASR, not exact wording.
    let lower = text.to_lowercase();
    assert!(!lower.trim().is_empty(), "got empty transcript");
    assert!(
        lower.contains("meeting"),
        "expected the word 'meeting' in: {text}"
    );
}
```

Add `hound` as a dev-dependency too (it is already a normal dependency, so nothing extra is needed — integration tests see normal dependencies of the crate only via `owf_core`, so add `hound = "3.5"` under `[dev-dependencies]` as well).

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p owf-core --test asr_fixture -- --ignored`
Expected: FAIL — `owf_core::asr` does not exist.

- [ ] **Step 5: Implement `asr.rs`**

Every signature below was verified against the `sherpa-onnx` 1.13.6 docs.

```rust
use anyhow::{Context, Result};
use std::path::Path;

use sherpa_onnx::{
    OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig,
};

pub const SAMPLE_RATE: i32 = 16_000;

/// Anything that turns 16 kHz mono f32 samples into text.
///
/// This trait exists so the ASR engine can be swapped without touching the
/// pipeline — see spec §17.1.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, samples: &[f32]) -> Result<String>;
}

pub struct SherpaTranscriber {
    recognizer: OfflineRecognizer,
}

impl SherpaTranscriber {
    pub fn new(models_dir: &Path, num_threads: i32) -> Result<Self> {
        let dir = models_dir.join("parakeet-tdt-0.6b-v3-int8");
        let p = |f: &str| -> Option<String> {
            Some(dir.join(f).to_string_lossy().into_owned())
        };

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: p("encoder.int8.onnx"),
            decoder: p("decoder.int8.onnx"),
            joiner: p("joiner.int8.onnx"),
        };
        config.model_config.tokens = p("tokens.txt");
        config.model_config.model_type = Some("nemo_transducer".into());
        config.model_config.num_threads = num_threads;
        config.model_config.debug = false;

        let recognizer = OfflineRecognizer::create(&config)
            .context("OfflineRecognizer::create returned None — check model paths and model_type")?;

        Ok(Self { recognizer })
    }
}

impl Transcriber for SherpaTranscriber {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, samples);
        self.recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|r| r.text)
            .unwrap_or_default();
        Ok(text.trim().to_string())
    }
}
```

Add `pub mod asr;` to `lib.rs`.

Note on the first build: `sherpa-onnx` downloads a prebuilt native archive from GitHub releases rather than compiling with cmake. If that download fails behind a proxy, set `SHERPA_ONNX_LIB_DIR` to a manually-extracted library directory.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p owf-core --test asr_fixture -- --ignored --nocapture`
Expected: PASS.

If `OfflineRecognizer::create` returns `None`, the cause is almost always a wrong path or a missing `model_type`. Set `config.model_config.debug = true` and re-run — sherpa prints the config it received.

- [ ] **Step 7: Write the latency benchmark**

Create `crates/owf-cli/src/bin/owf-bench.rs`:

```rust
use anyhow::Result;
use owf_core::asr::{SherpaTranscriber, Transcriber};
use std::time::Instant;

fn main() -> Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "crates/owf-core/fixtures/hello_english.wav".to_string()
    });

    let mut r = hound::WavReader::open(&path)?;
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16_000, "bench input must be 16 kHz mono");
    let samples: Vec<f32> = r
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    let audio_secs = samples.len() as f64 / 16_000.0;

    let t0 = Instant::now();
    let asr = SherpaTranscriber::new(&owf_core::paths::models_dir(), 4)?;
    let load_ms = t0.elapsed().as_millis();

    // First pass warms ONNX Runtime's internal allocations; report the second.
    let _ = asr.transcribe(&samples)?;
    let t1 = Instant::now();
    let text = asr.transcribe(&samples)?;
    let asr_ms = t1.elapsed().as_millis();

    println!("audio        {audio_secs:.2} s");
    println!("model load   {load_ms} ms  (once, at daemon start)");
    println!("asr (warm)   {asr_ms} ms");
    println!("asr RTF      {:.3}", asr_ms as f64 / 1000.0 / audio_secs);
    println!("transcript   {text}");
    Ok(())
}
```

Add `hound = "3.5"` to `crates/owf-cli/Cargo.toml` dependencies.

- [ ] **Step 8: Measure and record the M0 numbers**

Run: `cargo run --release -p owf-cli --bin owf-bench`

Use `--release`. A debug build of ONNX Runtime inference is not representative and will look alarming.

Record the output in the commit message. **This is the M0 exit criterion.** Per spec §17.2, if warm ASR RTF is materially worse than ~0.3 (i.e. over ~3 s for a 10 s clip), stop and report before continuing — the remedies there (skip normalization for short utterances, smaller ASR model, opt-in normalization) are design decisions, not implementation details.

- [ ] **Step 9: Commit**

```bash
git add crates/owf-core/src/asr.rs crates/owf-core/src/lib.rs \
        crates/owf-core/Cargo.toml crates/owf-core/tests/asr_fixture.rs \
        crates/owf-core/fixtures crates/owf-cli/src/bin/owf-bench.rs \
        crates/owf-cli/Cargo.toml
git commit -m "feat: Parakeet TDT 0.6b v3 transcription via sherpa-onnx

Transcriber trait keeps the engine swappable per spec 17.1.

Measured on i7-10510U, 4 threads, release build:
  <paste the owf-bench output here>"
```

---

### Task 4: Configuration

**Files:**
- Create: `crates/owf-core/src/config.rs`
- Modify: `crates/owf-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `config.rs`

**Interfaces:**
- Consumes: `owf_core::paths::config_file`, and the `style` types from Task 5 are *not* yet available — `config.rs` defines the axis enums itself and Task 5 moves them to `style.rs`. To avoid that churn, **Task 5's enums are defined here** and `style.rs` imports them.
- Produces:
  - `owf_core::config::{Config, AudioConfig, AsrConfig, NormalizeConfig, GuardrailConfig, InjectConfig, StyleAxes, StyleRule, Styling, Structure, Context, InjectBackend}`
  - `Config::load() -> anyhow::Result<Config>` (writes defaults if absent)
  - `Config::from_str(&str) -> anyhow::Result<Config>`

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/owf-core/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_documented_defaults() {
        let c = Config::from_str("").unwrap();
        assert_eq!(c.audio.max_seconds, 120);
        assert_eq!(c.audio.vad_padding_ms, 200);
        assert_eq!(c.asr.num_threads, 4);
        assert!(c.normalize.enabled);
        assert_eq!(c.normalize.port, 8730);
        assert_eq!(c.normalize.timeout_ms, 6000);
        assert_eq!(c.normalize.context_size, 2048);
        assert_eq!(c.guardrail.min_word_ratio, 0.55);
        assert_eq!(c.guardrail.max_word_ratio, 1.80);
        assert_eq!(c.guardrail.min_overlap_english, 0.55);
        assert_eq!(c.guardrail.min_overlap_other, 0.70);
        assert_eq!(c.guardrail.short_input_words, 4);
        assert_eq!(c.guardrail.ngram_size, 6);
        assert_eq!(c.guardrail.ngram_max_repeats, 3);
        assert!(c.inject.trailing_space);
        assert_eq!(c.style_default.styling, Styling::SemiCasual);
        assert_eq!(c.style_default.structure, Structure::Prose);
        assert_eq!(c.style_default.context, Context::General);
    }

    #[test]
    fn axis_enums_deserialize_from_their_wire_spellings() {
        let c = Config::from_str(
            r#"
            [style_default]
            styling = "semi-formal"
            structure = "lists"
            context = "email"
            "#,
        )
        .unwrap();
        assert_eq!(c.style_default.styling, Styling::SemiFormal);
        assert_eq!(c.style_default.structure, Structure::Lists);
        assert_eq!(c.style_default.context, Context::Email);
    }

    #[test]
    fn invalid_axis_value_is_a_load_error() {
        let err = Config::from_str(
            r#"
            [style_default]
            styling = "shouty"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shouty") || err.to_string().contains("styling"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn unknown_key_is_a_load_error() {
        let err = Config::from_str("[audio]\nmax_secondz = 5\n").unwrap_err();
        assert!(err.to_string().contains("max_secondz"), "got: {err}");
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for bad in [
            "[audio]\nmax_seconds = 0\n",
            "[normalize]\ntimeout_ms = 0\n",
            "[guardrail]\nmin_overlap_english = 1.5\n",
            "[guardrail]\nmin_word_ratio = 2.0\nmax_word_ratio = 1.0\n",
            "[asr]\nnum_threads = 0\n",
        ] {
            assert!(Config::from_str(bad).is_err(), "should have rejected: {bad}");
        }
    }

    #[test]
    fn style_rules_parse_with_partial_axes() {
        let c = Config::from_str(
            r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
            "#,
        )
        .unwrap();
        assert_eq!(c.style_rules.len(), 1);
        assert_eq!(c.style_rules[0].context, Some(Context::Email));
        assert_eq!(c.style_rules[0].styling, None);
    }

    #[test]
    fn invalid_regex_in_a_style_rule_is_a_load_error() {
        let err = Config::from_str(
            r#"
            [[style_rules]]
            match_class = "([unclosed"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("regex"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core config`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `config.rs`**

```rust
use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Styling {
    Casual,
    SemiCasual,
    SemiFormal,
    Formal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Structure {
    Prose,
    Lists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Context {
    General,
    Email,
}

impl fmt::Display for Styling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Styling::Casual => "casual",
            Styling::SemiCasual => "semi-casual",
            Styling::SemiFormal => "semi-formal",
            Styling::Formal => "formal",
        };
        f.write_str(s)
    }
}

impl fmt::Display for Structure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Structure::Prose => "prose",
            Structure::Lists => "lists",
        })
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Context::General => "general",
            Context::Email => "email",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleAxes {
    #[serde(default = "default_styling")]
    pub styling: Styling,
    #[serde(default = "default_structure")]
    pub structure: Structure,
    #[serde(default = "default_context")]
    pub context: Context,
}

fn default_styling() -> Styling { Styling::SemiCasual }
fn default_structure() -> Structure { Structure::Prose }
fn default_context() -> Context { Context::General }

impl Default for StyleAxes {
    fn default() -> Self {
        Self {
            styling: default_styling(),
            structure: default_structure(),
            context: default_context(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleRule {
    pub match_class: String,
    #[serde(default)]
    pub styling: Option<Styling>,
    #[serde(default)]
    pub structure: Option<Structure>,
    #[serde(default)]
    pub context: Option<Context>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "d_device")]
    pub device: String,
    #[serde(default = "d_max_seconds")]
    pub max_seconds: u32,
    #[serde(default = "d_vad_padding")]
    pub vad_padding_ms: u32,
}

fn d_device() -> String { "default".into() }
fn d_max_seconds() -> u32 { 120 }
fn d_vad_padding() -> u32 { 200 }

impl Default for AudioConfig {
    fn default() -> Self {
        Self { device: d_device(), max_seconds: d_max_seconds(), vad_padding_ms: d_vad_padding() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsrConfig {
    #[serde(default = "d_threads")]
    pub num_threads: i32,
}

fn d_threads() -> i32 { 4 }

impl Default for AsrConfig {
    fn default() -> Self { Self { num_threads: d_threads() } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "d_llama_path")]
    pub llama_server_path: String,
    #[serde(default = "d_ctx")]
    pub context_size: u32,
    #[serde(default = "d_threads_u32")]
    pub threads: u32,
}

fn d_true() -> bool { true }
fn d_port() -> u16 { 8730 }
fn d_timeout() -> u64 { 6000 }
fn d_llama_path() -> String { "llama-server".into() }
fn d_ctx() -> u32 { 2048 }
fn d_threads_u32() -> u32 { 4 }

impl Default for NormalizeConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(), port: d_port(), timeout_ms: d_timeout(),
            llama_server_path: d_llama_path(), context_size: d_ctx(), threads: d_threads_u32(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardrailConfig {
    #[serde(default = "d_min_wr")]  pub min_word_ratio: f64,
    #[serde(default = "d_max_wr")]  pub max_word_ratio: f64,
    #[serde(default = "d_ov_en")]   pub min_overlap_english: f64,
    #[serde(default = "d_ov_other")]pub min_overlap_other: f64,
    #[serde(default = "d_short")]   pub short_input_words: usize,
    #[serde(default = "d_ngram")]   pub ngram_size: usize,
    #[serde(default = "d_ngram_rep")] pub ngram_max_repeats: usize,
}

fn d_min_wr() -> f64 { 0.55 }
fn d_max_wr() -> f64 { 1.80 }
fn d_ov_en() -> f64 { 0.55 }
fn d_ov_other() -> f64 { 0.70 }
fn d_short() -> usize { 4 }
fn d_ngram() -> usize { 6 }
fn d_ngram_rep() -> usize { 3 }

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            min_word_ratio: d_min_wr(), max_word_ratio: d_max_wr(),
            min_overlap_english: d_ov_en(), min_overlap_other: d_ov_other(),
            short_input_words: d_short(), ngram_size: d_ngram(),
            ngram_max_repeats: d_ngram_rep(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectBackend {
    Wtype,
    Clipboard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectConfig {
    #[serde(default = "d_backend")]
    pub backend: InjectBackend,
    #[serde(default = "d_true")]
    pub trailing_space: bool,
    #[serde(default = "d_keydelay")]
    pub keystroke_delay_ms: u32,
}

fn d_backend() -> InjectBackend { InjectBackend::Wtype }
fn d_keydelay() -> u32 { 2 }

impl Default for InjectConfig {
    fn default() -> Self {
        Self { backend: d_backend(), trailing_space: true, keystroke_delay_ms: d_keydelay() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)] pub audio: AudioConfig,
    #[serde(default)] pub asr: AsrConfig,
    #[serde(default)] pub normalize: NormalizeConfig,
    #[serde(default)] pub guardrail: GuardrailConfig,
    #[serde(default)] pub inject: InjectConfig,
    #[serde(default)] pub style_default: StyleAxes,
    #[serde(default)] pub style_rules: Vec<StyleRule>,
}

impl Config {
    pub fn from_str(s: &str) -> Result<Self> {
        let c: Config = toml::from_str(s).context("parsing config.toml")?;
        c.validate()?;
        Ok(c)
    }

    /// Loads the config, writing a commented default file if none exists.
    pub fn load() -> Result<Self> {
        let p = paths::config_file();
        if !p.exists() {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, DEFAULT_CONFIG_TOML)?;
        }
        let s = std::fs::read_to_string(&p)
            .with_context(|| format!("reading {}", p.display()))?;
        Self::from_str(&s)
    }

    fn validate(&self) -> Result<()> {
        if self.audio.max_seconds == 0 {
            bail!("audio.max_seconds must be greater than 0");
        }
        if self.asr.num_threads < 1 {
            bail!("asr.num_threads must be at least 1");
        }
        if self.normalize.timeout_ms == 0 {
            bail!("normalize.timeout_ms must be greater than 0");
        }
        if self.normalize.context_size < 512 {
            bail!("normalize.context_size must be at least 512");
        }
        for (name, v) in [
            ("guardrail.min_overlap_english", self.guardrail.min_overlap_english),
            ("guardrail.min_overlap_other", self.guardrail.min_overlap_other),
        ] {
            if !(0.0..=1.0).contains(&v) {
                bail!("{name} must be between 0.0 and 1.0, got {v}");
            }
        }
        if self.guardrail.min_word_ratio >= self.guardrail.max_word_ratio {
            bail!(
                "guardrail.min_word_ratio ({}) must be less than max_word_ratio ({})",
                self.guardrail.min_word_ratio, self.guardrail.max_word_ratio
            );
        }
        if self.guardrail.ngram_size < 2 {
            bail!("guardrail.ngram_size must be at least 2");
        }
        if self.guardrail.ngram_max_repeats < 2 {
            bail!("guardrail.ngram_max_repeats must be at least 2");
        }
        for rule in &self.style_rules {
            regex::Regex::new(&rule.match_class).with_context(|| {
                format!("invalid regex in style_rules match_class: {}", rule.match_class)
            })?;
        }
        Ok(())
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# OpenWhisprFlow configuration

[audio]
device = "default"
max_seconds = 120
vad_padding_ms = 200

[asr]
num_threads = 4

[normalize]
# Cleanup runs on S1-mini by Superwhisper.
enabled = true
port = 8730
timeout_ms = 6000
llama_server_path = "llama-server"
context_size = 2048
threads = 4

[guardrail]
min_word_ratio = 0.55
max_word_ratio = 1.80
min_overlap_english = 0.55
min_overlap_other = 0.70
short_input_words = 4
ngram_size = 6
ngram_max_repeats = 3

[inject]
backend = "wtype"
trailing_space = true
keystroke_delay_ms = 2

[style_default]
styling = "semi-casual"    # casual | semi-casual | semi-formal | formal
structure = "prose"        # prose | lists
context = "general"        # general | email

# First matching rule wins; unset axes inherit from [style_default].
# [[style_rules]]
# match_class = "(?i)thunderbird|^Mail$"
# styling = "semi-formal"
# context = "email"
"#;
```

Add `pub mod config;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core config`
Expected: PASS, 7 tests.

The `invalid_regex` test relies on `validate()` running inside `from_str`; if it fails, check that `from_str` calls `validate`.

- [ ] **Step 5: Commit**

```bash
git add crates/owf-core/src/config.rs crates/owf-core/src/lib.rs
git commit -m "feat: configuration with closed axis enums and load-time validation

Invalid control-line axis values, unknown keys, out-of-range numbers,
and malformed style-rule regexes all fail at load rather than at runtime."
```

---

### Task 5: Control line and style resolution

**Files:**
- Create: `crates/owf-core/src/style.rs`
- Modify: `crates/owf-core/src/lib.rs`
- Test: inline `#[cfg(test)]` in `style.rs`

**Interfaces:**
- Consumes: `owf_core::config::{Config, StyleAxes, StyleRule, Styling, Structure, Context}` (Task 4).
- Produces:
  - `owf_core::style::control_line(axes: &StyleAxes) -> String`
  - `owf_core::style::resolve(cfg: &Config, window_class: Option<&str>) -> StyleAxes`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Context, StyleAxes, Structure, Styling};

    #[test]
    fn control_line_covers_the_full_axis_matrix() {
        let stylings = [
            (Styling::Casual, "casual"),
            (Styling::SemiCasual, "semi-casual"),
            (Styling::SemiFormal, "semi-formal"),
            (Styling::Formal, "formal"),
        ];
        let structures = [(Structure::Prose, "prose"), (Structure::Lists, "lists")];
        let contexts = [(Context::General, "general"), (Context::Email, "email")];

        let mut seen = 0;
        for (sty, sty_s) in stylings {
            for (str_, str_s) in structures {
                for (ctx, ctx_s) in contexts {
                    let axes = StyleAxes { styling: sty, structure: str_, context: ctx };
                    assert_eq!(
                        control_line(&axes),
                        format!("[Styling: {sty_s}] [Structure: {str_s}] [Context: {ctx_s}]")
                    );
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 16, "the matrix is 4 x 2 x 2");
    }

    fn cfg(toml: &str) -> Config {
        Config::from_str(toml).unwrap()
    }

    #[test]
    fn no_window_class_yields_the_default_axes() {
        let c = cfg("");
        assert_eq!(resolve(&c, None), StyleAxes::default());
    }

    #[test]
    fn a_non_matching_class_yields_the_default_axes() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
        "#);
        assert_eq!(resolve(&c, Some("Alacritty")), StyleAxes::default());
    }

    #[test]
    fn a_matching_rule_overrides_only_the_axes_it_sets() {
        let c = cfg(r#"
            [style_default]
            styling = "casual"

            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
        "#);
        let got = resolve(&c, Some("thunderbird"));
        assert_eq!(got.context, Context::Email, "rule sets context");
        assert_eq!(got.styling, Styling::Casual, "unset axes inherit the default");
        assert_eq!(got.structure, Structure::Prose);
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)^slack$"
            styling = "casual"

            [[style_rules]]
            match_class = "(?i)slack"
            styling = "formal"
        "#);
        assert_eq!(resolve(&c, Some("Slack")).styling, Styling::Casual);
    }

    #[test]
    fn matching_is_a_search_not_a_full_match() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)mail"
            context = "email"
        "#);
        assert_eq!(resolve(&c, Some("org.gnome.Geary.Mail")).context, Context::Email);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core style`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `style.rs`**

```rust
use crate::config::{Config, StyleAxes};

/// Builds the control line S1-mini is steered by.
///
/// The format is fixed by the model card: three bracketed axes on one line,
/// immediately above the raw transcript. See spec 8.3.
pub fn control_line(axes: &StyleAxes) -> String {
    format!(
        "[Styling: {}] [Structure: {}] [Context: {}]",
        axes.styling, axes.structure, axes.context
    )
}

/// Resolves style axes for the window that had focus when recording started.
///
/// Rules are evaluated in order and the first whose `match_class` regex is
/// found anywhere in the class wins. Axes the winning rule leaves unset
/// inherit from `[style_default]`.
///
/// Regexes are validated at config load (see `Config::validate`), so a compile
/// failure here means the config was constructed without validation; such a
/// rule is skipped rather than panicking.
pub fn resolve(cfg: &Config, window_class: Option<&str>) -> StyleAxes {
    let mut axes = cfg.style_default;
    let Some(class) = window_class else {
        return axes;
    };

    for rule in &cfg.style_rules {
        let Ok(re) = regex::Regex::new(&rule.match_class) else {
            tracing::warn!(pattern = %rule.match_class, "skipping unparseable style rule");
            continue;
        };
        if re.is_match(class) {
            if let Some(v) = rule.styling { axes.styling = v; }
            if let Some(v) = rule.structure { axes.structure = v; }
            if let Some(v) = rule.context { axes.context = v; }
            return axes;
        }
    }
    axes
}
```

Add `pub mod style;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core style`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/owf-core/src/style.rs crates/owf-core/src/lib.rs
git commit -m "feat: control-line builder and per-window style resolution"
```

---

### Task 6: Guardrail

The highest-value pure logic in the codebase. This is what makes a 0.6B model safe to put between the user's voice and their keyboard.

**Files:**
- Create: `crates/owf-core/src/guardrail.rs`, `crates/owf-core/tests/guardrail_table.rs`
- Modify: `crates/owf-core/src/lib.rs`

**Interfaces:**
- Consumes: `owf_core::config::GuardrailConfig` (Task 4), `owf_core::lang::Lang` — **defined in Task 7**, so define the enum here and let Task 7 import it:
  ```rust
  pub enum Lang { English, Other }
  ```
  It lives in `lang.rs`. To keep Task 6 self-contained, create `lang.rs` now containing only the enum; Task 7 adds the detector to the same file.
- Produces:
  - `owf_core::guardrail::{Verdict, RejectReason}`
  - `owf_core::guardrail::evaluate(raw: &str, cleaned: &str, lang: Lang, cfg: &GuardrailConfig) -> Verdict`
  - `owf_core::guardrail::rule_based_fallback(raw: &str) -> String`
  - `owf_core::guardrail::tokenize(s: &str) -> Vec<String>`
  - `owf_core::guardrail::overlap(raw: &[String], cleaned: &[String]) -> f64`

- [ ] **Step 1: Create the `Lang` enum**

Create `crates/owf-core/src/lang.rs`:

```rust
/// Coarse language classification. The pipeline only needs to know whether
/// S1-mini is operating in-domain (English) or out of it — see spec 7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    English,
    Other,
}
```

Add `pub mod lang;` and `pub mod guardrail;` to `lib.rs`.

- [ ] **Step 2: Write the failing unit tests**

At the bottom of `crates/owf-core/src/guardrail.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GuardrailConfig;
    use crate::lang::Lang;

    fn cfg() -> GuardrailConfig {
        GuardrailConfig::default()
    }

    #[test]
    fn tokenize_lowercases_and_drops_punctuation() {
        assert_eq!(
            tokenize("Hello, World! It's 4:30."),
            vec!["hello", "world", "it", "s", "4", "30"]
        );
    }

    #[test]
    fn overlap_is_one_for_identical_token_bags() {
        let a = tokenize("the meeting is at four thirty");
        assert_eq!(overlap(&a, &a), 1.0);
    }

    #[test]
    fn overlap_is_zero_for_disjoint_bags() {
        let a = tokenize("alpha bravo charlie");
        let b = tokenize("delta echo foxtrot");
        assert_eq!(overlap(&a, &b), 0.0);
    }

    #[test]
    fn overlap_counts_with_multiplicity() {
        let raw = tokenize("yes yes yes yes");
        let cleaned = tokenize("yes yes");
        assert_eq!(overlap(&raw, &cleaned), 0.5);
    }

    #[test]
    fn overlap_of_an_empty_raw_bag_is_one() {
        assert_eq!(overlap(&[], &tokenize("anything")), 1.0);
    }

    #[test]
    fn empty_cleaned_output_is_rejected() {
        let v = evaluate("hello there friend", "   ", Lang::English, &cfg());
        assert!(matches!(v, Verdict::Reject(RejectReason::Empty)));
    }

    #[test]
    fn a_faithful_cleanup_is_accepted() {
        let raw = "um so the meeting is at uh four thirty on tuesday";
        let cleaned = "So the meeting is at 4:30 on Tuesday.";
        assert!(matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept));
    }

    #[test]
    fn a_cleanup_that_drops_most_of_the_content_is_rejected() {
        let raw = "the quarterly numbers came in higher than we forecast \
                   across every region except the nordics";
        let cleaned = "The numbers came in.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::WordRatio { .. })
        ));
    }

    #[test]
    fn a_cleanup_that_invents_content_is_rejected() {
        let raw = "send it tomorrow";
        // 3 raw words; 1.80 x 3 = 5.4, so 6+ words trips the ratio.
        let cleaned = "Please make sure that you send it tomorrow morning without fail.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::WordRatio { .. })
        ));
    }

    #[test]
    fn a_confidently_wrong_rewrite_is_rejected_on_overlap() {
        let raw = "alpha bravo charlie delta echo foxtrot golf hotel";
        // Same length, entirely different words.
        let cleaned = "One two three four five six seven eight.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Overlap { .. })
        ));
    }

    #[test]
    fn non_english_uses_the_stricter_overlap_threshold() {
        // Overlap here is 0.6: above the English floor (0.55), below Other (0.70).
        let raw = "eins zwei drei vier funf sechs sieben acht neun zehn";
        let cleaned = "Eins zwei drei vier funf sechs alpha bravo charlie delta.";
        assert!(
            matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept),
            "English threshold should accept this"
        );
        assert!(
            matches!(
                evaluate(raw, cleaned, Lang::Other, &cfg()),
                Verdict::Reject(RejectReason::Overlap { .. })
            ),
            "Other threshold should reject this"
        );
    }

    #[test]
    fn a_degenerate_loop_is_rejected() {
        let raw = "please send the report to the team by friday afternoon at the latest";
        let cleaned = "Please send the report to the team \
                       please send the report to the team \
                       please send the report to the team.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Loop { .. })
        ));
    }

    #[test]
    fn template_bleed_is_rejected() {
        for bleed in [
            "[Styling: casual] Hello there friend.",
            "[Structure: prose] Hello there friend.",
            "[Context: email] Hello there friend.",
            "<think>hmm</think> Hello there friend.",
            "<|im_start|>Hello there friend.",
        ] {
            assert!(
                matches!(
                    evaluate("hello there friend", bleed, Lang::English, &cfg()),
                    Verdict::Reject(RejectReason::TemplateBleed { .. })
                ),
                "should have rejected: {bleed}"
            );
        }
    }

    #[test]
    fn short_inputs_skip_the_ratio_and_overlap_checks() {
        // 2 raw words; a normalizer may legitimately expand "gonna" or fix a
        // homophone, and the ratios are meaningless at this length.
        let v = evaluate("k thx", "Okay, thanks!", Lang::English, &cfg());
        assert!(matches!(v, Verdict::Accept), "got {v:?}");
    }

    #[test]
    fn short_inputs_still_reject_empty_and_bleed() {
        assert!(matches!(
            evaluate("k thx", "", Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Empty)
        ));
        assert!(matches!(
            evaluate("k thx", "[Styling: casual] Okay", Lang::English, &cfg()),
            Verdict::Reject(RejectReason::TemplateBleed { .. })
        ));
    }

    #[test]
    fn number_normalisation_survives_the_overlap_check() {
        // S1-mini rewrites spoken numbers into digits, which legitimately
        // destroys token overlap. The 0.55 floor exists to tolerate this.
        let raw = "call me at five five five one two three four";
        let cleaned = "Call me at 555-1234.";
        assert!(
            matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept),
            "digit rewriting must not trip the guardrail"
        );
    }

    #[test]
    fn fallback_collapses_whitespace_capitalises_and_terminates() {
        assert_eq!(rule_based_fallback("  hello   there  "), "Hello there.");
        assert_eq!(rule_based_fallback("already done."), "Already done.");
        assert_eq!(rule_based_fallback("what about this?"), "What about this?");
        assert_eq!(rule_based_fallback("hey!"), "Hey!");
        assert_eq!(rule_based_fallback(""), "");
        assert_eq!(rule_based_fallback("   "), "");
        // Leading non-alphabetic characters must not block capitalisation.
        assert_eq!(rule_based_fallback("42 things happened"), "42 Things happened.");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core guardrail`
Expected: FAIL — module does not exist.

- [ ] **Step 4: Implement `guardrail.rs`**

```rust
use std::collections::HashMap;

use crate::config::GuardrailConfig;
use crate::lang::Lang;

const BLEED_MARKERS: [&str; 5] = [
    "[Styling:",
    "[Structure:",
    "[Context:",
    "<think>",
    "<|im_start|>",
];

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    Empty,
    WordRatio { ratio: f64 },
    Overlap { overlap: f64 },
    Loop { ngram: String },
    TemplateBleed { marker: &'static str },
}

impl RejectReason {
    /// Stable short name used in rejections.jsonl.
    pub fn code(&self) -> &'static str {
        match self {
            RejectReason::Empty => "empty",
            RejectReason::WordRatio { .. } => "word_ratio",
            RejectReason::Overlap { .. } => "overlap",
            RejectReason::Loop { .. } => "loop",
            RejectReason::TemplateBleed { .. } => "template_bleed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Accept,
    Reject(RejectReason),
}

/// Case-folded alphanumeric tokens. Punctuation is a separator, so a cleanup
/// that only adds punctuation produces an identical token bag.
pub fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Fraction of raw tokens that also appear in the cleaned text, counted with
/// multiplicity. 1.0 means every raw token survived.
pub fn overlap(raw: &[String], cleaned: &[String]) -> f64 {
    if raw.is_empty() {
        return 1.0;
    }
    let mut budget: HashMap<&str, usize> = HashMap::new();
    for t in cleaned {
        *budget.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut hits = 0usize;
    for t in raw {
        if let Some(n) = budget.get_mut(t.as_str()) {
            if *n > 0 {
                *n -= 1;
                hits += 1;
            }
        }
    }
    hits as f64 / raw.len() as f64
}

fn repeated_ngram(tokens: &[String], n: usize, max_repeats: usize) -> Option<String> {
    if n == 0 || tokens.len() < n {
        return None;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for w in tokens.windows(n) {
        let key = w.join(" ");
        let c = counts.entry(key.clone()).or_insert(0);
        *c += 1;
        if *c >= max_repeats {
            return Some(key);
        }
    }
    None
}

/// Decides whether S1-mini's output is safe to type.
///
/// Order matters: the always-on checks (empty, loop, template bleed) run
/// first, then the length-sensitive ones, which are skipped for very short
/// inputs where the ratios carry no signal. See spec 9.1.
pub fn evaluate(raw: &str, cleaned: &str, lang: Lang, cfg: &GuardrailConfig) -> Verdict {
    if cleaned.trim().is_empty() {
        return Verdict::Reject(RejectReason::Empty);
    }

    for marker in BLEED_MARKERS {
        if cleaned.contains(marker) {
            return Verdict::Reject(RejectReason::TemplateBleed { marker });
        }
    }

    let raw_tokens = tokenize(raw);
    let clean_tokens = tokenize(cleaned);

    if let Some(ngram) = repeated_ngram(&clean_tokens, cfg.ngram_size, cfg.ngram_max_repeats) {
        return Verdict::Reject(RejectReason::Loop { ngram });
    }

    if raw_tokens.len() < cfg.short_input_words {
        return Verdict::Accept;
    }

    let ratio = clean_tokens.len() as f64 / raw_tokens.len() as f64;
    if ratio < cfg.min_word_ratio || ratio > cfg.max_word_ratio {
        return Verdict::Reject(RejectReason::WordRatio { ratio });
    }

    let ov = overlap(&raw_tokens, &clean_tokens);
    let floor = match lang {
        Lang::English => cfg.min_overlap_english,
        Lang::Other => cfg.min_overlap_other,
    };
    if ov < floor {
        return Verdict::Reject(RejectReason::Overlap { overlap: ov });
    }

    Verdict::Accept
}

/// The minimal cleanup applied to raw ASR text when normalization is skipped
/// or rejected. Deliberately tiny: the user should be able to tell at a glance
/// that S1-mini did not run. See spec 9.2.
pub fn rule_based_fallback(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return String::new();
    }
    let mut chars: Vec<char> = collapsed.chars().collect();
    if let Some(i) = chars.iter().position(|c| c.is_alphabetic()) {
        let upper: Vec<char> = chars[i].to_uppercase().collect();
        chars.splice(i..=i, upper);
    }
    let mut s: String = chars.into_iter().collect();
    if !s.ends_with(['.', '!', '?', '\u{2026}']) {
        s.push('.');
    }
    s
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core guardrail`
Expected: PASS, 17 tests.

If `non_english_uses_the_stricter_overlap_threshold` fails, recount: the test needs an overlap strictly between 0.55 and 0.70. Print `overlap(&tokenize(raw), &tokenize(cleaned))` and adjust the fixture words until it sits in that band — do not adjust the thresholds.

- [ ] **Step 6: Commit**

```bash
git add crates/owf-core/src/guardrail.rs crates/owf-core/src/lang.rs \
        crates/owf-core/src/lib.rs
git commit -m "feat: guardrail rejecting unfaithful S1-mini output

Five checks per spec 9.1: empty, word-count ratio, token-bag overlap
(stricter for non-English), degenerate n-gram loops, template bleed.
Short inputs skip the ratio checks. Rejection falls back to raw ASR."
```

---

### Task 7: Language detection and token estimation

**Files:**
- Modify: `crates/owf-core/src/lang.rs`, `crates/owf-core/Cargo.toml`
- Create: `crates/owf-core/src/normalize.rs` (the pure half only; the HTTP client lands in Task 10)
- Modify: `crates/owf-core/src/lib.rs`

**Interfaces:**
- Consumes: `owf_core::lang::Lang` (Task 6).
- Produces:
  - `owf_core::lang::LanguageDetector` trait: `fn detect(&self, text: &str) -> Lang`
  - `owf_core::lang::WhatlangDetector` (unit struct, `Default`)
  - `owf_core::normalize::max_tokens_for(raw: &str) -> u32`

- [ ] **Step 1: Add the dependency**

`crates/owf-core/Cargo.toml`: `whatlang = "0.18"`

- [ ] **Step 2: Write the failing tests**

Append to `crates/owf-core/src/lang.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confident_english_is_english() {
        let d = WhatlangDetector::default();
        assert_eq!(
            d.detect("the meeting is at four thirty on tuesday and i will send the notes"),
            Lang::English
        );
    }

    #[test]
    fn confident_german_is_other() {
        let d = WhatlangDetector::default();
        assert_eq!(
            d.detect("das treffen ist um halb funf am dienstag und ich schicke die notizen"),
            Lang::Other
        );
    }

    #[test]
    fn very_short_text_defaults_to_english() {
        // Trigram detection is unreliable under ~12 chars, and English is the
        // configured primary language. See spec 7.4.
        let d = WhatlangDetector::default();
        assert_eq!(d.detect("ok thanks"), Lang::English);
        assert_eq!(d.detect(""), Lang::English);
    }

    #[test]
    fn detection_is_deterministic() {
        let d = WhatlangDetector::default();
        let s = "please forward the invoice to accounting before the end of the week";
        assert_eq!(d.detect(s), d.detect(s));
    }
}
```

And in `crates/owf-core/src/normalize.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_tokens_never_drops_below_the_floor() {
        assert_eq!(max_tokens_for(""), 32);
        assert_eq!(max_tokens_for("hi"), 34); // ceil(2/3.5)=1 -> ceil(1.3)=2 -> 34
    }

    #[test]
    fn max_tokens_scales_with_input_length() {
        // 350 chars -> est 100 tokens -> ceil(130) + 32 = 162
        let s = "a".repeat(350);
        assert_eq!(max_tokens_for(&s), 162);
    }

    #[test]
    fn max_tokens_is_capped() {
        let s = "a".repeat(100_000);
        assert_eq!(max_tokens_for(&s), 1024);
    }

    #[test]
    fn max_tokens_counts_characters_not_bytes() {
        // Multi-byte characters must not inflate the estimate.
        let s = "\u{e4}".repeat(35); // 35 chars, 70 bytes
        assert_eq!(max_tokens_for(&s), max_tokens_for(&"a".repeat(35)));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core lang normalize`
Expected: FAIL — `WhatlangDetector` and `max_tokens_for` are undefined.

- [ ] **Step 4: Implement**

Append to `crates/owf-core/src/lang.rs` (above the test module):

```rust
/// Coarse language classification for the guardrail.
///
/// Deliberately a trait: `whatlang` is chosen for its ~1 MB footprint, and
/// `lingua` restricted to the configured languages is the documented upgrade
/// if rejection logs show misclassification. See spec 7.4.
pub trait LanguageDetector: Send + Sync {
    fn detect(&self, text: &str) -> Lang;
}

/// Below this length, trigram detection is noise.
const MIN_CHARS_FOR_DETECTION: usize = 12;

/// Below this confidence, treat the guess as unusable.
const MIN_CONFIDENCE: f64 = 0.6;

#[derive(Debug, Default, Clone, Copy)]
pub struct WhatlangDetector;

impl LanguageDetector for WhatlangDetector {
    fn detect(&self, text: &str) -> Lang {
        if text.chars().count() < MIN_CHARS_FOR_DETECTION {
            return Lang::English;
        }
        match whatlang::detect(text) {
            Some(info)
                if info.lang() == whatlang::Lang::Eng && info.confidence() >= MIN_CONFIDENCE =>
            {
                Lang::English
            }
            Some(_) => Lang::Other,
            None => Lang::English,
        }
    }
}
```

Create `crates/owf-core/src/normalize.rs` with the pure half (above the test module):

```rust
/// Characters per token, empirically about right for English on a Qwen3
/// tokenizer. Used only to size `max_tokens`; a local estimate avoids a second
/// round trip to /tokenize on the critical path. See spec 8.2.
const CHARS_PER_TOKEN: f64 = 3.5;

/// S1-mini's model card sizes generation at roughly 1.3x input plus a margin.
pub fn max_tokens_for(raw: &str) -> u32 {
    let est = (raw.chars().count() as f64 / CHARS_PER_TOKEN).ceil();
    let want = (1.3 * est).ceil() as i64 + 32;
    want.clamp(32, 1024) as u32
}
```

Add `pub mod normalize;` to `lib.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core lang normalize`
Expected: PASS, 8 tests.

If `confident_german_is_other` fails, print `whatlang::detect(s)` — the confidence for short German may fall under 0.6, in which case lengthen the test sentence rather than lowering `MIN_CONFIDENCE`.

- [ ] **Step 6: Commit**

```bash
git add crates/owf-core/src/lang.rs crates/owf-core/src/normalize.rs \
        crates/owf-core/src/lib.rs crates/owf-core/Cargo.toml
git commit -m "feat: whatlang language detection and max_tokens estimation

Language only selects a guardrail threshold, so a coarse English/other
signal suffices; whatlang costs ~1 MB against lingua's tens of MB."
```

---

### Task 8: VAD trimming

**Files:**
- Create: `crates/owf-core/src/vad.rs`
- Modify: `crates/owf-core/src/lib.rs`

**Interfaces:**
- Consumes: `owf_core::paths::models_dir`, `owf_core::asr::SAMPLE_RATE` (Task 3).
- Produces:
  - `owf_core::vad::Trimmer` trait: `fn trim(&self, samples: &[f32], padding_ms: u32) -> Option<(usize, usize)>` returning an inclusive-exclusive sample range, or `None` when no speech was found.
  - `owf_core::vad::SileroTrimmer::new(models_dir: &Path) -> anyhow::Result<Self>`

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/owf-core/src/vad.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_expands_the_span_and_clamps_to_the_buffer() {
        // 16 kHz: 200 ms = 3200 samples.
        assert_eq!(pad_and_clamp(10_000, 20_000, 200, 32_000), (6_800, 23_200));
        // Clamps at the start.
        assert_eq!(pad_and_clamp(1_000, 20_000, 200, 32_000), (0, 23_200));
        // Clamps at the end.
        assert_eq!(pad_and_clamp(10_000, 31_000, 200, 32_000), (6_800, 32_000));
        // Zero padding is a no-op.
        assert_eq!(pad_and_clamp(10_000, 20_000, 0, 32_000), (10_000, 20_000));
    }

    #[test]
    fn a_span_covering_the_whole_buffer_stays_in_bounds() {
        assert_eq!(pad_and_clamp(0, 32_000, 500, 32_000), (0, 32_000));
    }

    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn silence_yields_no_speech_span() {
        let t = SileroTrimmer::new(&crate::paths::models_dir()).unwrap();
        let silence = vec![0.0f32; 16_000 * 2];
        assert_eq!(t.trim(&silence, 200), None);
    }

    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn speech_surrounded_by_silence_is_trimmed_inward() {
        let mut r = hound::WavReader::open("fixtures/hello_english.wav").unwrap();
        let speech: Vec<f32> = r
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();

        // One second of silence on each side.
        let mut padded = vec![0.0f32; 16_000];
        padded.extend_from_slice(&speech);
        padded.extend(std::iter::repeat(0.0f32).take(16_000));

        let t = SileroTrimmer::new(&crate::paths::models_dir()).unwrap();
        let (start, end) = t.trim(&padded, 200).expect("should find speech");

        assert!(start > 0, "should have trimmed leading silence, got {start}");
        assert!(
            end < padded.len(),
            "should have trimmed trailing silence, got {end} of {}",
            padded.len()
        );
        assert!(end - start < padded.len(), "span should be shorter than the input");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core vad`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `vad.rs`**

All `sherpa-onnx` signatures below were verified against the 1.13.6 docs:
`VoiceActivityDetector::create(&VadModelConfig, f32) -> Option<Self>`,
`accept_waveform(&[f32])`, `flush()`, `is_empty() -> bool`,
`front() -> Option<SpeechSegment>`, `pop()`, `reset()`;
`SpeechSegment::start() -> i32`, `n() -> i32`.

```rust
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Mutex;

use sherpa_onnx::{VadModelConfig, VoiceActivityDetector};

use crate::asr::SAMPLE_RATE;

/// Finds the span of a buffer that actually contains speech.
pub trait Trimmer: Send + Sync {
    /// Returns `[start, end)` sample indices, or `None` if no speech was found.
    fn trim(&self, samples: &[f32], padding_ms: u32) -> Option<(usize, usize)>;
}

/// Expands a speech span by `padding_ms` on each side, clamped to the buffer.
/// Split out as a pure function so the arithmetic is testable without models.
fn pad_and_clamp(start: usize, end: usize, padding_ms: u32, len: usize) -> (usize, usize) {
    let pad = (SAMPLE_RATE as usize * padding_ms as usize) / 1000;
    (start.saturating_sub(pad), (end + pad).min(len))
}

pub struct SileroTrimmer {
    // VoiceActivityDetector is Sync, but its queue is stateful across calls,
    // so serialise access to keep `trim` a pure function of its input.
    vad: Mutex<VoiceActivityDetector>,
}

impl SileroTrimmer {
    pub fn new(models_dir: &Path) -> Result<Self> {
        let model = models_dir.join("silero_vad.onnx");
        anyhow::ensure!(model.exists(), "missing {}", model.display());

        let mut config = VadModelConfig::default();
        config.silero_vad.model = Some(model.to_string_lossy().into_owned());
        config.sample_rate = SAMPLE_RATE;
        config.num_threads = 1;
        config.debug = false;

        // Buffer sized for the longest recording the daemon permits.
        let vad = VoiceActivityDetector::create(&config, 130.0)
            .context("VoiceActivityDetector::create returned None")?;

        Ok(Self { vad: Mutex::new(vad) })
    }
}

impl Trimmer for SileroTrimmer {
    fn trim(&self, samples: &[f32], padding_ms: u32) -> Option<(usize, usize)> {
        if samples.is_empty() {
            return None;
        }
        let vad = self.vad.lock().ok()?;
        vad.reset();
        vad.accept_waveform(samples);
        vad.flush();

        let mut first: Option<usize> = None;
        let mut last: Option<usize> = None;
        while !vad.is_empty() {
            if let Some(seg) = vad.front() {
                let start = seg.start().max(0) as usize;
                let end = start + seg.n().max(0) as usize;
                first.get_or_insert(start);
                last = Some(end.min(samples.len()));
            }
            vad.pop();
        }

        let (start, end) = (first?, last?);
        if end <= start {
            return None;
        }
        Some(pad_and_clamp(start, end, padding_ms, samples.len()))
    }
}
```

Add `pub mod vad;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core vad` (pure tests)
Expected: PASS, 2 tests.

Run: `cargo test -p owf-core vad -- --ignored`
Expected: PASS, 2 tests.

If `SpeechSegment::start()` turns out to be measured in something other than samples, the `speech_surrounded_by_silence_is_trimmed_inward` test will fail with an implausible index; print `seg.start()` and `seg.n()` against a known buffer length to confirm the unit before adjusting.

- [ ] **Step 5: Commit**

```bash
git add crates/owf-core/src/vad.rs crates/owf-core/src/lib.rs
git commit -m "feat: Silero VAD trimming of leading and trailing silence

Parakeet's cost is linear in audio length and push-to-talk reliably
produces silence at both ends."
```

---

### Task 9: Audio capture

**Files:**
- Create: `crates/owf-core/src/capture.rs`
- Modify: `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`

**Interfaces:**
- Consumes: `owf_core::asr::SAMPLE_RATE`, `owf_core::config::AudioConfig`.
- Produces:
  - `owf_core::capture::Recorder::new(cfg: &AudioConfig) -> anyhow::Result<Self>`
  - `Recorder::start(&self, on_level: impl Fn(f32) + Send + 'static) -> anyhow::Result<()>`
  - `Recorder::stop(&self) -> anyhow::Result<Vec<f32>>` — always 16 kHz mono
  - `Recorder::is_recording(&self) -> bool`

- [ ] **Step 1: Add dependencies**

`crates/owf-core/Cargo.toml`:

```toml
cpal = "0.18"
rubato = "5.0"
```

- [ ] **Step 2: Write the failing tests**

At the bottom of `crates/owf-core/src/capture.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_averages_interleaved_channels() {
        // Two channels: L = [1.0, 3.0], R = [0.0, 1.0]
        let interleaved = [1.0f32, 0.0, 3.0, 1.0];
        assert_eq!(downmix(&interleaved, 2), vec![0.5, 2.0]);
    }

    #[test]
    fn downmix_is_a_copy_for_mono() {
        let mono = [0.1f32, -0.2, 0.3];
        assert_eq!(downmix(&mono, 1), mono.to_vec());
    }

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms(&[0.0; 128]), 0.0);
    }

    #[test]
    fn rms_of_a_constant_signal_is_its_magnitude() {
        assert!((rms(&[0.5; 128]) - 0.5).abs() < 1e-6);
        assert!((rms(&[-0.5; 128]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rms_of_an_empty_slice_is_zero() {
        assert_eq!(rms(&[]), 0.0);
    }

    #[test]
    fn resampling_preserves_duration_within_a_frame() {
        // 1 second at 48 kHz must become ~1 second at 16 kHz.
        let input = vec![0.0f32; 48_000];
        let out = resample_to_16k(&input, 48_000).unwrap();
        let drift = (out.len() as i64 - 16_000).abs();
        assert!(drift < 1_000, "expected ~16000 samples, got {}", out.len());
    }

    #[test]
    fn resampling_is_a_passthrough_when_already_16k() {
        let input = vec![0.25f32; 1_000];
        let out = resample_to_16k(&input, 16_000).unwrap();
        assert_eq!(out, input);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core capture`
Expected: FAIL — module does not exist.

- [ ] **Step 4: Implement `capture.rs`**

```rust
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::asr::SAMPLE_RATE;
use crate::config::AudioConfig;

/// Averages interleaved channels down to mono.
fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Offline resample of a complete mono buffer to 16 kHz.
///
/// The device is asked for 16 kHz first (PipeWire almost always obliges), so
/// this is a fallback path for hardware that refuses.
fn resample_to_16k(input: &[f32], from_rate: u32) -> Result<Vec<f32>> {
    use rubato::{FftFixedIn, Resampler};

    if from_rate == SAMPLE_RATE as u32 {
        return Ok(input.to_vec());
    }

    const CHUNK: usize = 1024;
    let mut resampler = FftFixedIn::<f32>::new(
        from_rate as usize,
        SAMPLE_RATE as usize,
        CHUNK,
        1, // sub-chunks
        1, // channels
    )
    .context("building resampler")?;

    let mut out: Vec<f32> = Vec::with_capacity(
        input.len() * SAMPLE_RATE as usize / from_rate as usize + CHUNK,
    );
    let mut pos = 0;
    while pos < input.len() {
        let end = (pos + CHUNK).min(input.len());
        let mut chunk = input[pos..end].to_vec();
        chunk.resize(CHUNK, 0.0); // zero-pad the tail
        let produced = resampler
            .process(&[chunk], None)
            .map_err(|e| anyhow!("resample failed: {e}"))?;
        out.extend_from_slice(&produced[0]);
        pos = end;
    }

    // Trim the tail introduced by zero-padding the final chunk.
    let expected = input.len() * SAMPLE_RATE as usize / from_rate as usize;
    out.truncate(expected.min(out.len()));
    Ok(out)
}

struct Shared {
    buffer: Mutex<Vec<f32>>,
    channels: usize,
    rate: u32,
}

pub struct Recorder {
    device: cpal::Device,
    max_samples_native: usize,
    recording: Arc<AtomicBool>,
    shared: Arc<Shared>,
    stream: Mutex<Option<cpal::Stream>>,
}

// cpal::Stream is not Send on all backends; the daemon owns the Recorder on a
// single thread and only shares it behind a lock, so this is sound here.
unsafe impl Send for Recorder {}
unsafe impl Sync for Recorder {}

impl Recorder {
    pub fn new(cfg: &AudioConfig) -> Result<Self> {
        let host = cpal::default_host();
        let device = if cfg.device == "default" {
            host.default_input_device()
                .context("no default input device")?
        } else {
            host.input_devices()?
                .find(|d| d.name().map(|n| n == cfg.device).unwrap_or(false))
                .with_context(|| format!("input device not found: {}", cfg.device))?
        };

        // Prefer 16 kHz directly; PipeWire resamples transparently.
        let supported = device.default_input_config().context("default input config")?;
        let rate = if device
            .supported_input_configs()?
            .any(|r| {
                r.min_sample_rate().0 <= SAMPLE_RATE as u32
                    && r.max_sample_rate().0 >= SAMPLE_RATE as u32
            }) {
            SAMPLE_RATE as u32
        } else {
            supported.sample_rate().0
        };
        let channels = supported.channels() as usize;

        tracing::info!(rate, channels, device = ?device.name(), "input device selected");

        Ok(Self {
            device,
            max_samples_native: rate as usize * channels * cfg.max_seconds as usize,
            recording: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(Shared {
                buffer: Mutex::new(Vec::new()),
                channels,
                rate,
            }),
            stream: Mutex::new(None),
        })
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }

    pub fn start(&self, on_level: impl Fn(f32) + Send + 'static) -> Result<()> {
        if self.is_recording() {
            return Ok(()); // idempotent, per spec 6
        }
        self.shared.buffer.lock().unwrap().clear();
        self.shared
            .buffer
            .lock()
            .unwrap()
            .reserve(self.max_samples_native);

        let shared = Arc::clone(&self.shared);
        let cap = self.max_samples_native;
        let config = cpal::StreamConfig {
            channels: self.shared.channels as u16,
            sample_rate: cpal::SampleRate(self.shared.rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = self.device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                on_level(rms(data));
                let mut buf = shared.buffer.lock().unwrap();
                if buf.len() < cap {
                    let room = cap - buf.len();
                    buf.extend_from_slice(&data[..data.len().min(room)]);
                }
            },
            |err| tracing::error!(?err, "input stream error"),
            None,
        )?;
        stream.play()?;
        *self.stream.lock().unwrap() = Some(stream);
        self.recording.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stops capture and returns 16 kHz mono samples.
    pub fn stop(&self) -> Result<Vec<f32>> {
        if !self.is_recording() {
            return Ok(Vec::new());
        }
        self.recording.store(false, Ordering::SeqCst);
        drop(self.stream.lock().unwrap().take()); // dropping the stream stops it

        let raw = std::mem::take(&mut *self.shared.buffer.lock().unwrap());
        let mono = downmix(&raw, self.shared.channels);
        resample_to_16k(&mono, self.shared.rate)
    }
}
```

Add `pub mod capture;` to `lib.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core capture`
Expected: PASS, 7 tests.

- [ ] **Step 6: Verify against real hardware**

Add a temporary check that capture actually produces audio — this cannot be unit-tested:

```bash
cargo run --release -p owf-cli --bin owf-bench -- /dev/null 2>&1 | head -1 || true
```

Instead, verify with a throwaway snippet in `owf-bench` or trust Task 14's end-to-end test. The important check is that `Recorder::new` selects 16 kHz: run the daemon in Task 13 and confirm the `input device selected` log line reads `rate=16000`. If it reads 48000, the resampler path is live and `resampling_preserves_duration_within_a_frame` is the test that guards it.

- [ ] **Step 7: Commit**

```bash
git add crates/owf-core/src/capture.rs crates/owf-core/src/lib.rs \
        crates/owf-core/Cargo.toml
git commit -m "feat: microphone capture at 16 kHz mono with RMS level events

Requests 16 kHz from the device directly (PipeWire obliges); falls back
to an FFT resampler for hardware that refuses."
```

---

### Task 10: llama-server supervision and the S1-mini client

**Files:**
- Create: `crates/owf-core/src/llama.rs`, `crates/owf-core/tests/normalize_http.rs`
- Modify: `crates/owf-core/src/normalize.rs`, `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`

**Interfaces:**
- Consumes: `owf_core::config::NormalizeConfig`, `owf_core::style::control_line`, `owf_core::normalize::max_tokens_for`, `owf_core::paths::{models_dir, runtime_port}`.
- Produces:
  - `owf_core::llama::LlamaServer::spawn(cfg: &NormalizeConfig) -> anyhow::Result<Self>` (kills the child on drop)
  - `LlamaServer::port(&self) -> u16`, `LlamaServer::wait_healthy(&self, timeout: Duration) -> anyhow::Result<()>`
  - `owf_core::normalize::Normalizer` trait: `fn normalize(&self, control: &str, raw: &str) -> anyhow::Result<String>`
  - `owf_core::normalize::S1MiniClient::new(base_url: String, timeout_ms: u64) -> Self`
  - `owf_core::normalize::SYSTEM_PROMPT: &str`

- [ ] **Step 1: Add the dev-dependency**

`crates/owf-core/Cargo.toml`:

```toml
[dev-dependencies]
httpmock = "0.8"
hound = "3.5"
```

- [ ] **Step 2: Write the failing integration tests**

Create `crates/owf-core/tests/normalize_http.rs`:

```rust
use httpmock::prelude::*;
use owf_core::normalize::{Normalizer, S1MiniClient, SYSTEM_PROMPT};

fn ok_body(content: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [ { "message": { "role": "assistant", "content": content } } ]
    })
}

#[test]
fn sends_the_verbatim_system_prompt_and_required_flags() {
    let server = MockServer::start();
    let m = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .json_body_partial(
                serde_json::json!({
                    "temperature": 0,
                    "top_k": 1,
                    "stream": false,
                    "chat_template_kwargs": { "enable_thinking": false }
                })
                .to_string(),
            );
        then.status(200).json_body(ok_body("Hello there."));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    let out = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hello there")
        .unwrap();

    assert_eq!(out, "Hello there.");
    m.assert();
}

#[test]
fn the_user_turn_is_the_control_line_then_a_newline_then_the_transcript() {
    let server = MockServer::start();
    let control = "[Styling: formal] [Structure: lists] [Context: email]";
    let m = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_contains(SYSTEM_PROMPT)
            .body_contains(&format!("{control}\\nhello there"));
        then.status(200).json_body(ok_body("Hello there."));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    c.normalize(control, "hello there").unwrap();
    m.assert();
}

#[test]
fn a_500_is_an_error_not_a_panic() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(500).body("upstream exploded");
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_malformed_body_is_an_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).body("{ not json");
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_response_with_no_choices_is_an_error() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(serde_json::json!({ "choices": [] }));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    assert!(c.normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there").is_err());
}

#[test]
fn a_slow_server_times_out_within_the_configured_budget() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .delay(std::time::Duration::from_millis(1_500))
            .json_body(ok_body("too late"));
    });

    let c = S1MiniClient::new(server.base_url(), 300);
    let t0 = std::time::Instant::now();
    let err = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hi there")
        .unwrap_err();
    let elapsed = t0.elapsed();

    assert!(err.to_string().to_lowercase().contains("timeout"), "got: {err}");
    assert!(elapsed < std::time::Duration::from_millis(1_200), "took {elapsed:?}");
}

#[test]
fn output_is_trimmed() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).json_body(ok_body("  Hello there.\n\n"));
    });

    let c = S1MiniClient::new(server.base_url(), 5_000);
    let out = c
        .normalize("[Styling: casual] [Structure: prose] [Context: general]", "hello there")
        .unwrap();
    assert_eq!(out, "Hello there.");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core --test normalize_http`
Expected: FAIL — `S1MiniClient` is undefined.

- [ ] **Step 4: Implement the client**

Append to `crates/owf-core/src/normalize.rs` (above the test module):

```rust
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Reproduced verbatim from the S1-mini model card. Paraphrasing it changes
/// the model's behaviour; do not edit. See spec 8.2.
pub const SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

pub trait Normalizer: Send + Sync {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String>;
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    messages: Vec<Message<'a>>,
    temperature: f32,
    top_k: u32,
    stream: bool,
    max_tokens: u32,
    chat_template_kwargs: ThinkingFlag,
}

#[derive(Serialize)]
struct ThinkingFlag {
    enable_thinking: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

pub struct S1MiniClient {
    base_url: String,
    timeout: Duration,
}

impl S1MiniClient {
    pub fn new(base_url: String, timeout_ms: u64) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

impl Normalizer for S1MiniClient {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String> {
        let body = ChatRequest {
            messages: vec![
                Message { role: "system", content: SYSTEM_PROMPT.to_string() },
                Message { role: "user", content: format!("{control_line}\n{raw}") },
            ],
            temperature: 0.0,
            top_k: 1,
            stream: false,
            max_tokens: max_tokens_for(raw),
            // Passed per-request as well as on the server command line: either
            // alone has been a reported source of blank output.
            chat_template_kwargs: ThinkingFlag { enable_thinking: false },
        };

        let url = format!("{}/v1/chat/completions", self.base_url);
        let payload = serde_json::to_string(&body)?;

        // The timeout is enforced here rather than by the HTTP client so that a
        // hung connection is bounded by exactly the configured budget.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                let mut resp = ureq::post(&url)
                    .header("content-type", "application/json")
                    .send(&payload)
                    .map_err(|e| anyhow!("request failed: {e}"))?;
                if resp.status() != 200 {
                    bail!("llama-server returned status {}", resp.status());
                }
                Ok(resp.body_mut().read_to_string()?)
            })();
            let _ = tx.send(result);
        });

        let text = match rx.recv_timeout(self.timeout) {
            Ok(r) => r?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!("normalization timeout after {:?}", self.timeout)
            }
            Err(e) => bail!("normalization worker died: {e}"),
        };

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("malformed response body: {e}"))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("response contained no choices"))?
            .message
            .content;
        Ok(content.trim().to_string())
    }
}
```

Note: `ureq::post(...).send(&payload)` sends a string body; if the compiler prefers `send_json(&body)`, use that instead and drop the manual `serde_json::to_string`. Either satisfies the tests.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core --test normalize_http`
Expected: PASS, 7 tests.

- [ ] **Step 6: Implement the supervisor**

Create `crates/owf-core/src/llama.rs`:

```rust
use anyhow::{bail, Context, Result};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::NormalizeConfig;
use crate::paths;

/// A supervised `llama-server` child running S1-mini by Superwhisper.
///
/// The child is killed when this value is dropped, so a daemon crash cannot
/// leave a 600 MB model resident.
pub struct LlamaServer {
    child: Child,
    port: u16,
}

/// Tries the configured port, then the next nine, per spec 8.1.
fn pick_port(preferred: u16) -> Result<u16> {
    for port in preferred..preferred.saturating_add(10) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    bail!("no free port in {}..{}", preferred, preferred + 10)
}

impl LlamaServer {
    pub fn spawn(cfg: &NormalizeConfig) -> Result<Self> {
        let model = paths::models_dir().join("s1-mini-q4_k_m.gguf");
        anyhow::ensure!(model.exists(), "missing {}", model.display());

        let port = pick_port(cfg.port)?;

        let child = Command::new(&cfg.llama_server_path)
            .arg("-m").arg(&model)
            .arg("--host").arg("127.0.0.1")
            .arg("--port").arg(port.to_string())
            .arg("-c").arg(cfg.context_size.to_string())
            .arg("-t").arg(cfg.threads.to_string())
            .arg("--jinja")
            .arg("--chat-template-kwargs").arg(r#"{"enable_thinking":false}"#)
            .arg("--temp").arg("0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {}", cfg.llama_server_path))?;

        std::fs::write(paths::runtime_port(), port.to_string()).ok();
        tracing::info!(port, "llama-server spawned");
        Ok(Self { child, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Polls `/health` until the model is loaded and serving.
    pub fn wait_healthy(&self, timeout: Duration) -> Result<()> {
        let url = format!("{}/health", self.base_url());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ureq::get(&url).call().map(|r| r.status() == 200).unwrap_or(false) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        bail!("llama-server did not become healthy within {timeout:?}")
    }

    pub fn is_healthy(&self) -> bool {
        ureq::get(&format!("{}/health", self.base_url()))
            .call()
            .map(|r| r.status() == 200)
            .unwrap_or(false)
    }
}

impl Drop for LlamaServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(paths::runtime_port());
        tracing::info!("llama-server stopped");
    }
}
```

Add `pub mod llama;` to `lib.rs`.

- [ ] **Step 7: Verify the supervisor against the real binary**

Run this one-off check (it loads a 462 MB model, so it takes a few seconds):

```bash
cargo test -p owf-core --lib llama 2>/dev/null; \
cargo run --release -p owf-cli --bin owf-bench 2>/dev/null >/dev/null; \
llama-server -m ~/.local/share/openwhisprflow/models/s1-mini-q4_k_m.gguf \
  --host 127.0.0.1 --port 8730 -c 2048 -t 4 --jinja \
  --chat-template-kwargs '{"enable_thinking":false}' --temp 0 &
sleep 20
curl -s http://127.0.0.1:8730/health
curl -s http://127.0.0.1:8730/v1/chat/completions -H 'content-type: application/json' -d '{
  "messages":[
    {"role":"system","content":"You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text."},
    {"role":"user","content":"[Styling: semi-casual] [Structure: prose] [Context: general]\num so the meeting is at uh four thirty on tuesday"}
  ],
  "temperature":0,"top_k":1,"stream":false,"max_tokens":128,
  "chat_template_kwargs":{"enable_thinking":false}}' | python3 -m json.tool
kill %1
```

Expected: `/health` returns an OK JSON body, and the completion returns something close to `So the meeting is at 4:30 on Tuesday.` **If the content is an empty string, the thinking flag is not taking effect** — that is the failure mode the model card warns about. Confirm `--jinja` is present and that the installed llama.cpp supports `--chat-template-kwargs`; if it does not, upgrade `llama-cpp` before continuing.

Also record how long `/health` took to go green from a cold start — that is the daemon's warm-up budget in Task 13.

- [ ] **Step 8: Commit**

```bash
git add crates/owf-core/src/normalize.rs crates/owf-core/src/llama.rs \
        crates/owf-core/src/lib.rs crates/owf-core/Cargo.toml \
        crates/owf-core/tests/normalize_http.rs
git commit -m "feat: S1-mini normalization over a supervised llama-server

Verbatim system prompt, enable_thinking=false and temp 0 on both the
command line and every request, hard 6s timeout enforced by the caller,
and a Drop impl that never leaves the model resident."
```

---

### Task 11: Text injection

**Files:**
- Create: `crates/owf-core/src/inject.rs`
- Modify: `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`

**Interfaces:**
- Consumes: `owf_core::config::{InjectConfig, InjectBackend}`.
- Produces:
  - `owf_core::inject::TextInjector` trait: `fn inject(&self, text: &str) -> Result<(), InjectError>` and `fn name(&self) -> &'static str`
  - `owf_core::inject::{WtypeInjector, ClipboardInjector, MockInjector, InjectError}`
  - `owf_core::inject::build(cfg: &InjectConfig) -> Box<dyn TextInjector>`
  - `owf_core::inject::inject_with_fallback(primary: &dyn TextInjector, text: &str) -> anyhow::Result<&'static str>` returning the backend that succeeded

- [ ] **Step 1: Add the dependency**

`crates/owf-core/Cargo.toml`: `notify-rust = "4.18"`

- [ ] **Step 2: Write the failing tests**

At the bottom of `crates/owf-core/src/inject.rs`:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p owf-core inject`
Expected: FAIL — module does not exist.

- [ ] **Step 4: Implement `inject.rs`**

```rust
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
/// rather than misread as flags; `-d` inserts an inter-keystroke delay that
/// some Electron and XWayland surfaces need to avoid dropping characters.
fn wtype_argv(text: &str, delay_ms: u32) -> Vec<String> {
    vec![
        "-d".to_string(),
        delay_ms.to_string(),
        "--".to_string(),
        text.to_string(),
    ]
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
```

Add `pub mod inject;` to `lib.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p owf-core inject`
Expected: PASS, 6 tests.

- [ ] **Step 6: Verify wtype's `--` handling against the real binary**

This is the M1 acceptance test from spec 10.2 and it cannot be automated — it needs a focused window.

Open a text editor, click into it, and from another terminal run:

```bash
sleep 3; wtype -d 2 -- "-- hello -x"
```

Switch focus to the editor within the 3 seconds.
Expected: the editor contains exactly `-- hello -x`.

If `wtype` errors on `--`, set `backend = "clipboard"` as the default in `DEFAULT_CONFIG_TOML` and note it — per spec 10.2, do not invent an escaping scheme.

- [ ] **Step 7: Commit**

```bash
git add crates/owf-core/src/inject.rs crates/owf-core/src/lib.rs \
        crates/owf-core/Cargo.toml
git commit -m "feat: wtype text injection with a clipboard fallback

TextInjector trait keeps ydotool reachable for XWayland surfaces where
wtype is known to fail (spec 10.3)."
```

---

### Task 12: Pipeline orchestration

**Files:**
- Create: `crates/owf-core/src/pipeline.rs`, `crates/owf-core/tests/pipeline_e2e.rs`
- Modify: `crates/owf-core/src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 3–11.
- Produces:
  - `owf_core::pipeline::Pipeline::new(cfg, transcriber, trimmer, detector, normalizer, injector) -> Self` (all trait objects, so tests inject fakes)
  - `Pipeline::process(&self, samples: &[f32], window_class: Option<&str>) -> anyhow::Result<Option<Outcome>>` — `Ok(None)` means no speech was found or the transcript was empty, and nothing was injected
  - `Pipeline::config(&self) -> &Config`
  - `owf_core::pipeline::Outcome { pub text: String, pub raw: String, pub normalized: bool, pub reject_reason: Option<String>, pub backend: &'static str, pub timings: Timings }`
  - `owf_core::pipeline::Timings { pub vad_ms: u128, pub asr_ms: u128, pub normalize_ms: u128, pub inject_ms: u128 }`

- [ ] **Step 1: Write the failing integration tests**

Create `crates/owf-core/tests/pipeline_e2e.rs`:

```rust
use owf_core::asr::Transcriber;
use owf_core::config::Config;
use owf_core::inject::MockInjector;
use owf_core::lang::{Lang, LanguageDetector};
use owf_core::normalize::Normalizer;
use owf_core::pipeline::Pipeline;
use owf_core::vad::Trimmer;

struct FixedAsr(String);
impl Transcriber for FixedAsr {
    fn transcribe(&self, _: &[f32]) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

struct WholeBuffer;
impl Trimmer for WholeBuffer {
    fn trim(&self, s: &[f32], _: u32) -> Option<(usize, usize)> {
        if s.is_empty() { None } else { Some((0, s.len())) }
    }
}

struct NoSpeechTrimmer;
impl Trimmer for NoSpeechTrimmer {
    fn trim(&self, _: &[f32], _: u32) -> Option<(usize, usize)> {
        None
    }
}

struct AlwaysEnglish;
impl LanguageDetector for AlwaysEnglish {
    fn detect(&self, _: &str) -> Lang {
        Lang::English
    }
}

struct FixedNormalizer(String);
impl Normalizer for FixedNormalizer {
    fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

struct BrokenNormalizer;
impl Normalizer for BrokenNormalizer {
    fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
        anyhow::bail!("llama-server is down")
    }
}

/// Captures the control line the normalizer was handed.
struct SpyNormalizer(std::sync::Mutex<Option<String>>);
impl Normalizer for SpyNormalizer {
    fn normalize(&self, control: &str, raw: &str) -> anyhow::Result<String> {
        *self.0.lock().unwrap() = Some(control.to_string());
        Ok(raw.to_string())
    }
}

fn samples() -> Vec<f32> {
    vec![0.1; 16_000]
}

#[test]
fn a_good_cleanup_is_injected() {
    let injector = MockInjector::default();
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("um so the meeting is at uh four thirty on tuesday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("So the meeting is at 4:30 on Tuesday.".into())),
        Box::new(injector),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(out.normalized);
    assert_eq!(out.text, "So the meeting is at 4:30 on Tuesday. ");
    assert!(out.reject_reason.is_none());
}

#[test]
fn a_rejected_cleanup_falls_back_to_raw() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("the quarterly numbers came in higher than we forecast".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Numbers.".into())), // far too short
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason.as_deref(), Some("word_ratio"));
    assert!(out.text.starts_with("The quarterly numbers"), "got {:?}", out.text);
    assert!(out.text.trim_end().ends_with('.'));
}

#[test]
fn a_dead_normalizer_still_produces_text() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(BrokenNormalizer),
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.text, "Send the invoice on friday. ");
}

#[test]
fn disabling_normalization_skips_it_entirely() {
    let p = Pipeline::new(
        Config::from_str("[normalize]\nenabled = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(BrokenNormalizer), // would error if it were called
        Box::new(MockInjector::default()),
    );

    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert!(!out.normalized);
    assert_eq!(out.reject_reason, None, "skipping is not a rejection");
}

#[test]
fn no_speech_injects_nothing() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("should never be reached".into())),
        Box::new(NoSpeechTrimmer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(MockInjector::default()),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
}

#[test]
fn an_empty_transcript_injects_nothing() {
    let p = Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(FixedAsr("   ".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("nope".into())),
        Box::new(MockInjector::default()),
    );
    assert!(p.process(&samples(), None).unwrap().is_none());
}

#[test]
fn the_window_class_selects_the_control_line() {
    let spy = std::sync::Arc::new(SpyNormalizer(std::sync::Mutex::new(None)));

    struct Fwd(std::sync::Arc<SpyNormalizer>);
    impl Normalizer for Fwd {
        fn normalize(&self, c: &str, r: &str) -> anyhow::Result<String> {
            self.0.normalize(c, r)
        }
    }

    let p = Pipeline::new(
        Config::from_str(
            r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            styling = "semi-formal"
            context = "email"
            "#,
        )
        .unwrap(),
        Box::new(FixedAsr("please find the invoice attached below".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(Fwd(spy.clone())),
        Box::new(MockInjector::default()),
    );

    p.process(&samples(), Some("thunderbird")).unwrap();
    assert_eq!(
        spy.0.lock().unwrap().as_deref(),
        Some("[Styling: semi-formal] [Structure: prose] [Context: email]")
    );
}

#[test]
fn trailing_space_can_be_disabled() {
    let p = Pipeline::new(
        Config::from_str("[inject]\ntrailing_space = false\n").unwrap(),
        Box::new(FixedAsr("send the invoice on friday".into())),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(FixedNormalizer("Send the invoice on Friday.".into())),
        Box::new(MockInjector::default()),
    );
    let out = p.process(&samples(), None).unwrap().expect("some outcome");
    assert_eq!(out.text, "Send the invoice on Friday.");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core --test pipeline_e2e`
Expected: FAIL — `Pipeline` is undefined.

- [ ] **Step 3: Implement `pipeline.rs`**

```rust
use anyhow::Result;
use std::io::Write;
use std::time::Instant;

use crate::asr::Transcriber;
use crate::config::Config;
use crate::guardrail::{self, RejectReason, Verdict};
use crate::inject::{inject_with_fallback, TextInjector};
use crate::lang::{Lang, LanguageDetector};
use crate::normalize::Normalizer;
use crate::paths;
use crate::style;
use crate::vad::Trimmer;

#[derive(Debug, Default, Clone, Copy)]
pub struct Timings {
    pub vad_ms: u128,
    pub asr_ms: u128,
    pub normalize_ms: u128,
    pub inject_ms: u128,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// Exactly what was handed to the injector, trailing space included.
    pub text: String,
    pub raw: String,
    pub normalized: bool,
    /// The guardrail code, when a cleanup was produced and rejected.
    pub reject_reason: Option<String>,
    pub backend: &'static str,
    pub timings: Timings,
}

pub struct Pipeline {
    cfg: Config,
    asr: Box<dyn Transcriber>,
    trimmer: Box<dyn Trimmer>,
    detector: Box<dyn LanguageDetector>,
    normalizer: Box<dyn Normalizer>,
    injector: Box<dyn TextInjector>,
}

impl Pipeline {
    pub fn new(
        cfg: Config,
        asr: Box<dyn Transcriber>,
        trimmer: Box<dyn Trimmer>,
        detector: Box<dyn LanguageDetector>,
        normalizer: Box<dyn Normalizer>,
        injector: Box<dyn TextInjector>,
    ) -> Self {
        Self { cfg, asr, trimmer, detector, normalizer, injector }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Runs a complete utterance. `Ok(None)` means there was nothing to say and
    /// nothing was injected.
    pub fn process(
        &self,
        samples: &[f32],
        window_class: Option<&str>,
    ) -> Result<Option<Outcome>> {
        let mut timings = Timings::default();

        let t = Instant::now();
        let Some((start, end)) = self.trimmer.trim(samples, self.cfg.audio.vad_padding_ms) else {
            tracing::info!("no speech detected");
            return Ok(None);
        };
        timings.vad_ms = t.elapsed().as_millis();

        let t = Instant::now();
        let raw = self.asr.transcribe(&samples[start..end])?;
        timings.asr_ms = t.elapsed().as_millis();
        if raw.trim().is_empty() {
            tracing::info!("empty transcript");
            return Ok(None);
        }

        let lang = self.detector.detect(&raw);
        let axes = style::resolve(&self.cfg, window_class);
        let control = style::control_line(&axes);

        let mut normalized = false;
        let mut reject_reason: Option<String> = None;
        let mut text = guardrail::rule_based_fallback(&raw);

        if self.cfg.normalize.enabled {
            let t = Instant::now();
            match self.normalizer.normalize(&control, &raw) {
                Ok(cleaned) => {
                    match guardrail::evaluate(&raw, &cleaned, lang, &self.cfg.guardrail) {
                        Verdict::Accept => {
                            text = cleaned;
                            normalized = true;
                        }
                        Verdict::Reject(reason) => {
                            reject_reason = Some(reason.code().to_string());
                            self.log_rejection(&raw, &cleaned, &reason, lang, &control);
                        }
                    }
                }
                Err(e) => {
                    // Spec 15: a failed cleanup must never cost the transcript.
                    tracing::warn!(error = %e, "normalization failed; using raw transcript");
                }
            }
            timings.normalize_ms = t.elapsed().as_millis();
        }

        if self.cfg.inject.trailing_space {
            text.push(' ');
        }

        let t = Instant::now();
        let backend = inject_with_fallback(self.injector.as_ref(), &text)?;
        timings.inject_ms = t.elapsed().as_millis();

        tracing::info!(
            ?timings, normalized, ?reject_reason, backend,
            "utterance complete"
        );

        Ok(Some(Outcome { text, raw, normalized, reject_reason, backend, timings }))
    }

    /// Appends one line to rejections.jsonl. This file is the input to M3
    /// threshold tuning — see spec 9.3. Failures here are never fatal.
    fn log_rejection(
        &self,
        raw: &str,
        cleaned: &str,
        reason: &RejectReason,
        lang: Lang,
        control: &str,
    ) {
        let record = serde_json::json!({
            "reason": reason.code(),
            "detail": format!("{reason:?}"),
            "lang": match lang { Lang::English => "English", Lang::Other => "Other" },
            "raw": raw,
            "cleaned": cleaned,
            "control": control,
        });
        let path = paths::rejections_file();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{record}"));
    }
}
```

Add `pub mod pipeline;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core --test pipeline_e2e`
Expected: PASS, 8 tests.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test -p owf-core`
Expected: PASS. Then `cargo test -p owf-core -- --ignored` for the model-backed tests.

- [ ] **Step 6: Commit**

```bash
git add crates/owf-core/src/pipeline.rs crates/owf-core/src/lib.rs \
        crates/owf-core/tests/pipeline_e2e.rs
git commit -m "feat: pipeline orchestration with guardrailed fallback

Holds the spec 15 invariant: once audio is transcribed the user gets
text, whatever happens downstream. Rejections are logged for tuning."
```

---

### Task 13: Hyprland window class and keybind config

**Files:**
- Create: `crates/owf-core/src/hypr.rs`
- Modify: `crates/owf-core/src/lib.rs`

**Interfaces:**
- Produces:
  - `owf_core::hypr::active_window_class() -> Option<String>`
  - `owf_core::hypr::HYPR_CONFIG: &str`

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/owf-core/src/hypr.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core hypr`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `hypr.rs`**

```rust
use std::process::Command;

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
    let out = Command::new("hyprctl").args(["-j", "activewindow"]).output().ok()?;
    if !out.status.success() {
        tracing::debug!("hyprctl activewindow failed; using default style");
        return None;
    }
    parse_class(&String::from_utf8_lossy(&out.stdout))
}
```

Add `pub mod hypr;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core hypr`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/owf-core/src/hypr.rs crates/owf-core/src/lib.rs
git commit -m "feat: read the focused window class and emit Hyprland config

Window class selects the S1-mini control line; the generated config
carries the focus rules the M2 overlay will depend on."
```

---

### Task 14: Socket protocol, daemon, and control CLI

**Files:**
- Create: `crates/owf-core/src/proto.rs`, `crates/owf-cli/src/bin/owf-daemon.rs`
- Modify: `crates/owf-cli/src/bin/owf-ctl.rs`, `crates/owf-cli/Cargo.toml`, `crates/owf-core/src/lib.rs`

**Interfaces:**
- Consumes: everything above, including `owf_core::hypr::{active_window_class, HYPR_CONFIG}` (Task 13).
- Produces:
  - `owf_core::proto::{Request, Response, State}` — serde-tagged NDJSON types
  - `owf_core::proto::send(req: &Request) -> anyhow::Result<Response>` (client side)

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/owf-core/src/proto.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_to_the_documented_wire_form() {
        assert_eq!(
            serde_json::to_string(&Request::PttStart).unwrap(),
            r#"{"cmd":"ptt-start"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::PttStop).unwrap(),
            r#"{"cmd":"ptt-stop"}"#
        );
        assert_eq!(serde_json::to_string(&Request::Cancel).unwrap(), r#"{"cmd":"cancel"}"#);
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        assert_eq!(serde_json::to_string(&Request::Reload).unwrap(), r#"{"cmd":"reload"}"#);
    }

    #[test]
    fn requests_round_trip() {
        for r in [Request::PttStart, Request::PttStop, Request::Cancel, Request::Status, Request::Reload] {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn an_unknown_command_fails_to_parse() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"launch-missiles"}"#).is_err());
    }

    #[test]
    fn states_serialise_lowercase() {
        assert_eq!(serde_json::to_string(&State::Warming).unwrap(), r#""warming""#);
        assert_eq!(serde_json::to_string(&State::Recording).unwrap(), r#""recording""#);
        assert_eq!(serde_json::to_string(&State::Transcribing).unwrap(), r#""transcribing""#);
        assert_eq!(serde_json::to_string(&State::Normalizing).unwrap(), r#""normalizing""#);
        assert_eq!(serde_json::to_string(&State::Injecting).unwrap(), r#""injecting""#);
        assert_eq!(serde_json::to_string(&State::Idle).unwrap(), r#""idle""#);
    }

    #[test]
    fn an_error_response_carries_ok_false_and_a_reason() {
        let r = Response::err("busy");
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], serde_json::json!(false));
        assert_eq!(v["err"], serde_json::json!("busy"));
    }

    #[test]
    fn an_ok_response_carries_the_state() {
        let r = Response::ok(State::Recording);
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["state"], serde_json::json!("recording"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p owf-core proto`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement `proto.rs`**

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    PttStart,
    PttStop,
    Cancel,
    Status,
    Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Warming,
    Idle,
    Recording,
    Transcribing,
    Normalizing,
    Injecting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(state: State) -> Self {
        Self { ok: true, state: Some(state), err: None, warm: None, last_ms: None }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, state: None, err: Some(msg.into()), warm: None, last_ms: None }
    }
}

/// Client side: one connection, one request line, one response line.
pub fn send(req: &Request) -> Result<Response> {
    let sock = paths::runtime_socket();
    let stream = UnixStream::connect(&sock).with_context(|| {
        format!("cannot reach the daemon at {} — is owf-daemon running?", sock.display())
    })?;
    let mut w = stream.try_clone()?;
    writeln!(w, "{}", serde_json::to_string(req)?)?;
    w.flush()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(line.trim())?)
}
```

Add `pub mod proto;` to `lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p owf-core proto`
Expected: PASS, 6 tests.

- [ ] **Step 5: Implement the daemon**

Create `crates/owf-cli/src/bin/owf-daemon.rs`:

```rust
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use owf_core::asr::SherpaTranscriber;
use owf_core::capture::Recorder;
use owf_core::config::Config;
use owf_core::inject;
use owf_core::lang::WhatlangDetector;
use owf_core::llama::LlamaServer;
use owf_core::normalize::S1MiniClient;
use owf_core::paths;
use owf_core::pipeline::Pipeline;
use owf_core::proto::{Request, Response, State};
use owf_core::vad::SileroTrimmer;

const WARMING: u8 = 0;
const IDLE: u8 = 1;
const RECORDING: u8 = 2;
const BUSY: u8 = 3;

fn state_of(v: u8) -> State {
    match v {
        WARMING => State::Warming,
        RECORDING => State::Recording,
        BUSY => State::Transcribing,
        _ => State::Idle,
    }
}

struct Daemon {
    state: AtomicU8,
    recorder: Recorder,
    pipeline: Mutex<Option<Pipeline>>,
    window_class: Mutex<Option<String>>,
    max_seconds: u32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Single-instance guard: an exclusive lock on a runtime file.
    let lock_path = paths::runtime_lock();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    if !try_lock_exclusive(&lock) {
        eprintln!("openwhisprflow is already running (lock held on {})", lock_path.display());
        std::process::exit(1);
    }

    let cfg = Config::load().context("loading config")?;

    let sock_path = paths::runtime_socket();
    let _ = std::fs::remove_file(&sock_path); // stale socket; we hold the lock
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;

    let daemon = Arc::new(Daemon {
        state: AtomicU8::new(WARMING),
        recorder: Recorder::new(&cfg.audio)?,
        pipeline: Mutex::new(None),
        window_class: Mutex::new(None),
        max_seconds: cfg.audio.max_seconds,
    });

    // Warm up off the accept loop so `status` answers immediately.
    {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || {
            match warm_up(cfg) {
                Ok(p) => {
                    *daemon.pipeline.lock().unwrap() = Some(p);
                    daemon.state.store(IDLE, Ordering::SeqCst);
                    tracing::info!("ready");
                }
                Err(e) => tracing::error!(error = ?e, "warm-up failed; daemon stays in warming"),
            }
        });
    }

    tracing::info!(socket = %sock_path.display(), "listening");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle(Arc::clone(&daemon), s),
            Err(e) => tracing::warn!(error = ?e, "accept failed"),
        }
    }
    Ok(())
}

/// `flock` via libc through std: `std::fs::File` has no portable lock API on
/// this toolchain, so use the raw syscall.
fn try_lock_exclusive(f: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // LOCK_EX | LOCK_NB
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn warm_up(cfg: Config) -> Result<Pipeline> {
    let models = paths::models_dir();

    let server = LlamaServer::spawn(&cfg.normalize)?;
    server.wait_healthy(Duration::from_secs(120))?;
    let base_url = server.base_url();
    // The supervisor must outlive this function or Drop kills the child.
    std::mem::forget(server);

    let asr = SherpaTranscriber::new(&models, cfg.asr.num_threads)?;
    let trimmer = SileroTrimmer::new(&models)?;
    let normalizer = S1MiniClient::new(base_url, cfg.normalize.timeout_ms);
    let injector = inject::build(&cfg.inject);

    Ok(Pipeline::new(
        cfg,
        Box::new(asr),
        Box::new(trimmer),
        Box::new(WhatlangDetector),
        Box::new(normalizer),
        injector,
    ))
}

fn handle(daemon: Arc<Daemon>, stream: UnixStream) {
    let mut line = String::new();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => dispatch(&daemon, req),
        Err(e) => Response::err(format!("bad request: {e}")),
    };
    let mut w = stream;
    let _ = writeln!(w, "{}", serde_json::to_string(&resp).unwrap_or_default());
}

fn dispatch(daemon: &Arc<Daemon>, req: Request) -> Response {
    let current = daemon.state.load(Ordering::SeqCst);
    match req {
        Request::Status => {
            let mut r = Response::ok(state_of(current));
            r.warm = Some(current != WARMING);
            r
        }
        Request::PttStart => {
            match current {
                WARMING => Response::err("warming"),
                RECORDING => Response::ok(State::Recording), // idempotent
                BUSY => Response::err("busy"),
                _ => start_recording(daemon),
            }
        }
        Request::PttStop => {
            if current != RECORDING {
                return Response::ok(state_of(current)); // no-op
            }
            daemon.state.store(BUSY, Ordering::SeqCst);
            let d = Arc::clone(daemon);
            std::thread::spawn(move || run_utterance(d));
            Response::ok(State::Transcribing)
        }
        Request::Cancel => {
            let _ = daemon.recorder.stop();
            daemon.state.store(if current == WARMING { WARMING } else { IDLE }, Ordering::SeqCst);
            Response::ok(state_of(daemon.state.load(Ordering::SeqCst)))
        }
        Request::Reload => {
            if current != IDLE {
                return Response::err("reload requires idle");
            }
            match Config::load() {
                Ok(_) => Response::ok(State::Idle),
                Err(e) => Response::err(format!("config error: {e}")),
            }
        }
    }
}

fn start_recording(daemon: &Arc<Daemon>) -> Response {
    // Captured before recording so the overlay (M2) can never confuse it.
    *daemon.window_class.lock().unwrap() = owf_core::hypr::active_window_class();

    if let Err(e) = daemon.recorder.start(|_level| {}) {
        return Response::err(format!("cannot start capture: {e}"));
    }
    daemon.state.store(RECORDING, Ordering::SeqCst);

    // Safety valve: a stuck key must not leave the microphone hot (spec 6.1).
    let d = Arc::clone(daemon);
    let limit = Duration::from_secs(d.max_seconds as u64);
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        if d.state.load(Ordering::SeqCst) == RECORDING {
            tracing::warn!("recording hit the time limit; stopping");
            d.state.store(BUSY, Ordering::SeqCst);
            run_utterance(d);
        }
    });

    Response::ok(State::Recording)
}

fn run_utterance(daemon: Arc<Daemon>) {
    let samples = match daemon.recorder.stop() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "capture stop failed");
            daemon.state.store(IDLE, Ordering::SeqCst);
            return;
        }
    };
    let class = daemon.window_class.lock().unwrap().clone();

    let guard = daemon.pipeline.lock().unwrap();
    if let Some(p) = guard.as_ref() {
        match p.process(&samples, class.as_deref()) {
            Ok(Some(out)) => tracing::info!(chars = out.text.len(), "injected"),
            Ok(None) => tracing::info!("nothing to inject"),
            Err(e) => tracing::error!(error = ?e, "pipeline failed"),
        }
    }
    drop(guard);
    daemon.state.store(IDLE, Ordering::SeqCst);
}
```

Add to `crates/owf-cli/Cargo.toml`:

```toml
libc = "0.2"

[[bin]]
name = "owf-daemon"
path = "src/bin/owf-daemon.rs"
```

- [ ] **Step 6: Extend `owf-ctl` with the socket commands**

Replace the `match` in `crates/owf-cli/src/bin/owf-ctl.rs`:

```rust
use anyhow::Result;
use owf_core::proto::{self, Request};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let req = match refs.as_slice() {
        ["ptt-start"] => Request::PttStart,
        ["ptt-stop"] => Request::PttStop,
        ["cancel"] => Request::Cancel,
        ["status"] => Request::Status,
        ["reload"] => Request::Reload,
        ["setup"] => return setup(false),
        ["setup", "--update-lock"] => return setup(true),
        ["setup", "--print-hypr"] => {
            print!("{}", owf_core::hypr::HYPR_CONFIG);
            return Ok(());
        }
        _ => {
            eprintln!(
                "usage: owf-ctl <ptt-start|ptt-stop|cancel|status|reload>\n\
                 \x20      owf-ctl setup [--update-lock|--print-hypr]"
            );
            std::process::exit(2);
        }
    };

    let resp = proto::send(&req)?;
    println!("{}", serde_json::to_string(&resp)?);
    if !resp.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn setup(update_lock: bool) -> Result<()> {
    let mut last = 0u64;
    owf_core::models::download_all(update_lock, &mut |url, done, total| {
        if done - last > 8 << 20 || Some(done) == total {
            last = done;
            match total {
                Some(t) => eprintln!("  {:>5.1}%  {}", 100.0 * done as f64 / t as f64, url),
                None => eprintln!("  {} MB  {}", done >> 20, url),
            }
        }
    })?;
    println!("models ready in {}", owf_core::paths::models_dir().display());
    Ok(())
}
```

- [ ] **Step 7: Verify the daemon and CLI talk to each other**

`owf_core::hypr` comes from Task 13, so this builds as-is. Run:

```bash
cargo build --release -p owf-cli
./target/release/owf-daemon &
sleep 45   # model load; check the log for "ready"
./target/release/owf-ctl status
```

Expected: `{"ok":true,"state":"idle","warm":true}`. While warming it returns `{"ok":true,"state":"warming","warm":false}`.

Then:

```bash
./target/release/owf-ctl ptt-start   # {"ok":true,"state":"recording"}
./target/release/owf-ctl ptt-start   # idempotent, same response
sleep 3                               # speak
./target/release/owf-ctl ptt-stop    # {"ok":true,"state":"transcribing"}
./target/release/owf-ctl ptt-start   # {"ok":false,"err":"busy"}
```

Confirm the log line `input device selected` reads `rate=16000`.

- [ ] **Step 8: Commit**

```bash
git add crates/owf-core/src/proto.rs crates/owf-core/src/lib.rs \
        crates/owf-cli/src/bin crates/owf-cli/Cargo.toml
git commit -m "feat: unix socket protocol, daemon, and owf-ctl

Single-instance flock, warm-up off the accept loop, idempotent
ptt-start, busy rejection rather than queueing, and a 120s safety
valve so a stuck key cannot leave the microphone hot."
```

---

### Task 15: Install and end-to-end verification

- [ ] **Step 1: Add the prerequisite check to `owf-ctl setup`**

Spec 14.3 requires `setup` to report missing prerequisites rather than
failing obscurely later. Add to `crates/owf-cli/src/bin/owf-ctl.rs`:

```rust
/// Reports required external programs without installing anything.
/// Returns false when something essential is missing.
fn check_prerequisites() -> bool {
    // (binary, why it is needed, fatal)
    let checks = [
        ("llama-server", "S1-mini normalization", true),
        ("wtype", "typing into the focused window", true),
        ("wl-copy", "clipboard fallback when typing fails", true),
        ("hyprctl", "per-application style rules", false),
    ];

    let mut ok = true;
    for (bin, why, fatal) in checks {
        let found = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        match (found, fatal) {
            (true, _) => eprintln!("  ok       {bin}  ({why})"),
            (false, true) => {
                eprintln!("  MISSING  {bin}  ({why})");
                ok = false;
            }
            (false, false) => eprintln!("  absent   {bin}  ({why}) - optional"),
        }
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR").is_some();
    eprintln!(
        "  {}  XDG_RUNTIME_DIR  (socket location)",
        if runtime { "ok      " } else { "absent  " }
    );

    ok
}
```

Call it at the top of `setup`, before any download:

```rust
fn setup(update_lock: bool) -> Result<()> {
    eprintln!("prerequisites:");
    if !check_prerequisites() {
        anyhow::bail!("install the missing programs, then re-run: sudo pacman -S llama-cpp wtype wl-clipboard");
    }
    let mut last = 0u64;
    // ... existing download loop unchanged ...
```

Verify: `owf-ctl setup` prints an `ok` line for each of the four programs and
proceeds. Temporarily rename `wtype` on PATH to confirm it aborts with the
pacman hint rather than downloading 1.1 GB first.

- [ ] **Step 2: Install and wire up Hyprland**

```bash
cargo build --release -p owf-cli
mkdir -p ~/.local/bin
install -m755 target/release/owf-daemon target/release/owf-ctl ~/.local/bin/
owf-ctl setup --print-hypr >> ~/.config/hypr/hyprland.conf
hyprctl reload
```

Confirm `~/.local/bin` is on PATH (it is, on this machine). Then log out and back in, or start the daemon manually once: `owf-daemon &`.

- [ ] **Step 3: End-to-end manual verification**

This is the M1 exit criterion. Work through each row and record the result.

| # | Check | Expected |
|---|---|---|
| 1 | `owf-ctl status` after ~45 s | `{"ok":true,"state":"idle","warm":true}` |
| 2 | Focus Alacritty, hold SUPER+D, say "the meeting is at four thirty on tuesday", release | Cleaned, punctuated text appears in the terminal |
| 3 | Same in Firefox's address bar | Text appears |
| 4 | Same in an Electron app (VS Code, Slack) | Text appears |
| 5 | Same in an XWayland window (`xterm` or a Wine app) | Text appears, **or** the "copied to clipboard" notification fires — both are passes; a silent failure is not |
| 6 | Hold SUPER+D and release without speaking | Nothing is typed; log says "no speech detected" |
| 7 | Hold SUPER+D, then press SUPER+Escape while still holding | Nothing is typed |
| 8 | Dictate twice in a row without a pause | Two transcripts, separated by a space, no run-together |
| 9 | `kill` the llama-server process, then dictate | Raw ASR text is still typed; log warns about normalization |
| 10 | Dictate a German sentence | Text appears; check `rejections.jsonl` for whether the guardrail fired |
| 11 | `cat ~/.local/state/openwhisprflow/rejections.jsonl` | Valid JSON lines, one per rejection |
| 12 | `owf-daemon` a second time while one runs | Exits with "already running" |

For row 2, also time it with a stopwatch from key release to text appearing, and compare against the M0 estimate from Task 3.

- [ ] **Step 4: Replace the README**

**Files:** Modify `README.md` (currently the unmodified Tauri template).

The current `README.md` is the unmodified Tauri template. Replace it with real setup instructions covering: prerequisites (`rustup`, `llama-cpp`, `wtype`, `wl-clipboard`), `owf-ctl setup --update-lock`, installing the binaries, `owf-ctl setup --print-hypr`, the config file location, and a short "how it works" paragraph naming **S1-mini by Superwhisper** with that exact capitalization (required by its license) and Parakeet TDT 0.6b v3. Include the measured latency numbers from Task 3.

- [ ] **Step 5: Final verification and commit**

Run: `cargo test -p owf-core`
Expected: PASS, all tests.

Run: `cargo test -p owf-core -- --ignored`
Expected: PASS, all model-backed tests.

Run: `cargo clippy -p owf-core -p owf-cli -- -D warnings`
Expected: clean.

```bash
git add crates/owf-cli/src/bin/owf-ctl.rs README.md
git commit -m "feat: Hyprland keybind integration and setup docs

Completes M1: holding SUPER+D dictates into the focused window.
Records the manual verification matrix results in the README."
```

---

## What this plan does not cover

Deliberately deferred to the M2/M3 plan, written after these numbers exist:

- The Tauri overlay window, RMS waveform rendering, and state presentation (spec §12). `Recorder::start` already takes an `on_level` callback that the daemon currently ignores — that is the hook.
- `llama-server` crash detection and backoff restart (spec §5.1). Task 14 uses `std::mem::forget` on the supervisor to keep the child alive for the daemon's lifetime; M2 replaces that with a supervised handle that can restart it.
- Guardrail threshold tuning against real `rejections.jsonl` data (spec §9, §17.4).
- `YdotoolInjector` (spec §10.3).
- Config hot-reload actually rebuilding the pipeline — `Request::Reload` currently only validates the file.
