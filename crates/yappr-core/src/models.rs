use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::AsrModel;
use crate::paths;

pub struct Artifact {
    /// Stable key. This is the lock-file map key already committed in
    /// `models.lock.toml` — never show it to the user and never change it,
    /// changing it would invalidate the existing pin.
    pub name: &'static str,
    pub url: &'static str,
    /// Human-readable name for user-facing text (errors, progress, logs).
    /// Some artifacts carry license-mandated capitalization (S1-mini by
    /// Superwhisper requires this exact form), so this must not be derived
    /// from `name`.
    pub display: &'static str,
    /// Path relative to models_dir() once provisioned.
    pub rel_path: &'static str,
    /// True when the download is a .tar.bz2 that must be extracted.
    pub archive: bool,
}

/// How a model is decoded. Not a property of the file layout -- every entry
/// is encoder/decoder/joiner plus tokens.txt -- but of which sherpa-onnx
/// recognizer can read it.
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
    /// The dropdown label. Names the language, because that is what
    /// actually decides the choice.
    pub display: &'static str,
}

pub static ASR_MODELS: [AsrModelSpec; 4] = [
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
        display: "Parakeet TDT 0.6b v3 — multilingual",
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
        display: "Parakeet Unified 0.6b — English only",
    },
    AsrModelSpec {
        key: AsrModel::Nemotron35,
        artifact: Artifact {
            // No dot: a TOML bare key cannot contain one, and this string is
            // a key in models.lock.toml. "nemotron-3.5-560ms" parsed as the
            // dotted key `nemotron-3` -> `5-560ms` and made the whole lock
            // file unreadable. The user-facing config spelling keeps its dot
            // ("nemotron-3.5") because that is a TOML *value*, not a key.
            name: "nemotron-3-5-560ms",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2",
            display: "Nemotron 3.5 ASR 0.6b (560 ms, int8)",
            rel_path: "nemotron-3.5-asr-streaming-0.6b-560ms-int8",
            archive: true,
        },
        flavor: AsrFlavor::CacheAwareStreaming,
        display: "Nemotron 3.5 ASR 0.6b — multilingual",
    },
    AsrModelSpec {
        key: AsrModel::ParakeetPrimelineDe,
        artifact: Artifact {
            name: "parakeet-primeline-de",
            // The only catalogue entry not hosted by k2-fsa. Upstream
            // publishes primeline-parakeet as a .nemo checkpoint, which
            // sherpa-onnx cannot load, so this is our own export --
            // reproducible via scripts/export-primeline-onnx.sh, and
            // CC-BY-4.0 like the model it derives from.
            url: "https://huggingface.co/Joni000000000/parakeet-primeline-sherpa-onnx-int8/resolve/main/parakeet-primeline-de-int8.tar.bz2",
            display: "primeline Parakeet 0.6b (int8)",
            rel_path: "parakeet-primeline-de-int8",
            archive: true,
        },
        flavor: AsrFlavor::Offline,
        display: "primeline Parakeet 0.6b — German only, most accurate for German",
    },
];

/// Everything needed no matter which ASR model is selected.
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

/// Every artifact this build knows about, selected or not.
///
/// Used for name/URL lookups -- a download-progress event names a URL and
/// the Setup pane has to turn that back into a display name -- and for the
/// lock-file completeness check. Never as "what must be present": that is
/// [`required_artifacts`], and confusing the two is what would make an
/// unselected model's absence look like a broken install.
pub fn all_artifacts() -> Vec<&'static Artifact> {
    SUPPORT_ARTIFACTS
        .iter()
        .chain(ASR_MODELS.iter().map(|m| &m.artifact))
        .collect()
}

/// The catalogue entry for `model`.
///
/// Panics only if a variant were added to `AsrModel` without an entry here,
/// which `every_asr_model_enum_variant_has_a_catalogue_entry` keeps out of a
/// release.
pub fn spec_for(model: AsrModel) -> &'static AsrModelSpec {
    ASR_MODELS
        .iter()
        .find(|m| m.key == model)
        .unwrap_or_else(|| panic!("no catalogue entry for {model:?}"))
}

/// What provisioning may treat as required: the support pair plus the one
/// selected ASR model.
///
/// Models left on disk by an earlier selection are deliberately absent --
/// not verified, not redownloaded, and never deleted -- so switching back is
/// instant and offline. See spec asr-model §4.
pub fn required_artifacts(model: AsrModel) -> Vec<&'static Artifact> {
    SUPPORT_ARTIFACTS
        .iter()
        .chain(std::iter::once(&spec_for(model).artifact))
        .collect()
}
/// The lock file committed to the repository at `crates/yappr-core/models.lock.toml`,
/// generated once during M0 via `owf-ctl setup --update-lock` (spec 14.2).
///
/// C2: nothing in the tree ever read this file -- `LockFile::load()` only
/// ever looked at the *runtime* lock under `paths::models_dir()`, which does
/// not exist on a fresh install. The result was that a brand-new machine
/// downloaded the full ~1.1 GB of models, hashed them, found nothing pinned,
/// and aborted with "re-run with --update-lock" -- discarding the very
/// integrity check spec 14.2 promises ("every subsequent install verifies
/// against pinned values"). Compiling the committed file in with
/// `include_str!` is what makes that promise true starting from the very
/// first run, without requiring a network fetch of the lock file itself.
const COMPILED_IN_LOCK: &str = include_str!("../models.lock.toml");

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
}

impl LockFile {
    pub fn path() -> PathBuf {
        paths::models_dir().join("models.lock.toml")
    }

    /// Parses the lock file compiled into this binary. The only way this can
    /// fail is if `crates/yappr-core/models.lock.toml` itself were malformed,
    /// which would fail every build, not just this call.
    fn compiled_in() -> Result<Self> {
        Ok(toml::from_str(COMPILED_IN_LOCK)?)
    }

    /// Loads the effective lock file: the runtime copy under
    /// `paths::models_dir()`, if any, with the compiled-in pin (see
    /// `COMPILED_IN_LOCK`) filling in any artifact it doesn't cover -- absent
    /// entirely (the common case on a fresh install) or just missing that
    /// one entry. A runtime entry always wins over the compiled-in one, which
    /// is what keeps `--update-lock` able to actually change a pin rather
    /// than being permanently shadowed by the committed file.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    /// The testable core of `load`: `path` is a parameter so a test can
    /// point it at a location that deliberately doesn't exist, proving the
    /// compiled-in fallback is used, without touching the real
    /// `models_dir()` -- which, on this machine, already has 1.1 GB of
    /// downloaded models and a runtime lock file that must not be disturbed.
    fn load_from(path: &Path) -> Result<Self> {
        let compiled = Self::compiled_in()?;
        if !path.exists() {
            return Ok(compiled);
        }
        let s = std::fs::read_to_string(path)?;
        let mut runtime: Self = toml::from_str(&s)?;
        for (name, hash) in compiled.hashes {
            runtime.hashes.entry(name).or_insert(hash);
        }
        Ok(runtime)
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
    // sha2 0.11 / digest 0.11 return a `hybrid-array` `Array<u8, _>` from
    // `finalize()`, which does not implement `LowerHex` the way the old
    // `generic-array` output did, so hex-encode the bytes by hand.
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        use std::fmt::Write as _;
        write!(hex, "{b:02x}").unwrap();
    }
    Ok(hex)
}

/// For an archive artifact the hash is taken over the extracted encoder file,
/// which is the piece that actually matters and the only one large enough for
/// corruption to be plausible. Always points at the *live*, already-installed
/// location — used by `verify()` and by the "is it already good" fast path in
/// `download_all()`. It is never used for the staged (not-yet-verified) copy
/// of a fresh download; see `fetch_and_stage_file`/`fetch_and_stage_archive`.
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
pub fn verify(lock: &LockFile, required: &[&'static Artifact]) -> Result<Vec<String>> {
    let mut bad = Vec::new();
    for a in required.iter().copied() {
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

/// The pure core of [`looks_present`]: true when every path in `targets`
/// exists and is non-empty. Split out so it's testable against scratch
/// paths instead of `hash_target`'s real, `models_dir()`-rooted ones.
fn all_targets_look_present(targets: &[PathBuf]) -> bool {
    targets
        .iter()
        .all(|t| std::fs::metadata(t).map(|m| m.len() > 0).unwrap_or(false))
}

/// Fast, non-authoritative stand-in for `verify`: true only when every
/// artifact's on-disk target exists and is non-empty. Never reads a byte of
/// any file's contents, so unlike `verify` it cannot catch a corrupted
/// install that happens to keep the right size -- callers that need that
/// guarantee (an explicit re-check from the Setup pane, and the moment a
/// download finishes and its hash is already known) still call `verify`
/// directly.
///
/// Exists because `verify`'s sha256 pass over up to ~1.1 GB of models is too
/// expensive to run unconditionally every time something needs to ask "is
/// anything still missing" -- which, once first-run setup is done, is a
/// question asked on every single subsequent app launch (see
/// `src-tauri/src/provision.rs`, the one caller). The common case -- a
/// machine that finished setup once and never touched the models directory
/// again -- only ever needs this cheap answer.
pub fn looks_present(required: &[&'static Artifact]) -> bool {
    let targets: Vec<PathBuf> = required.iter().map(|a| hash_target(a)).collect();
    all_targets_look_present(&targets)
}

/// Builds a sibling path by appending `suffix` to `p`'s *whole* file name.
///
/// `Path::with_extension` is the wrong tool for this: its "extension" is
/// whatever comes after the *last* dot in the file name, and artifact
/// directory names like `parakeet-tdt-0.6b-v3-int8` contain a dot inside the
/// version number. `.with_extension("staging")` on that path silently
/// produces `parakeet-tdt-0.staging`, discarding `6b-v3-int8` — the bug was
/// harmless only by luck (nothing ever read the truncated name back). This
/// helper preserves the full original name unconditionally.
fn sibling_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().expect("path has no file name").to_os_string();
    name.push(suffix);
    p.with_file_name(name)
}

/// Streams `url` to `tmp` (which must be the caller's, not the live
/// install's, path), reporting progress as it goes. Deliberately does not
/// touch anything else: the caller decides whether and when to promote `tmp`
/// into a live location, only after verifying its contents. That way a
/// corrupt or truncated download can never disturb an existing good
/// installation.
fn download_to(url: &str, tmp: &Path, progress: &mut dyn FnMut(&str, u64, Option<u64>)) -> Result<()> {
    std::fs::create_dir_all(tmp.parent().unwrap())?;
    let resp = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let mut reader = resp.into_body().into_reader();
    let mut out = std::fs::File::create(tmp)?;
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
    Ok(())
}

/// Extracts `archive` (a .tar.bz2) into a staging directory next to `dest`
/// and flattens the single top-level directory the upstream tarball wraps
/// everything in. Returns the flattened staging directory's path.
///
/// Never touches `dest` itself: the caller hashes the staged copy and only
/// promotes it into `dest` after that hash is verified.
fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<PathBuf> {
    let f = std::fs::File::open(archive)?;
    let dec = bzip2::read::BzDecoder::new(f);
    let mut tar = tar::Archive::new(dec);

    let unpack_dir = sibling_with_suffix(dest, ".unpack");
    if unpack_dir.exists() {
        std::fs::remove_dir_all(&unpack_dir)?;
    }
    std::fs::create_dir_all(&unpack_dir)?;
    tar.unpack(&unpack_dir)?;

    let top = std::fs::read_dir(&unpack_dir)?
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .map(|e| e.path())
        .context("archive had no top-level directory")?;

    let staged = sibling_with_suffix(dest, ".staged");
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::rename(&top, &staged)?;
    std::fs::remove_dir_all(&unpack_dir).ok();
    Ok(staged)
}

/// Downloads a plain-file artifact to a staging path next to `dest` and
/// hashes it there. Returns the staged path and its hash; does not touch
/// `dest`.
fn fetch_and_stage_file(
    a: &Artifact,
    dest: &Path,
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<(PathBuf, String)> {
    let part = sibling_with_suffix(dest, ".part");
    download_to(a.url, &part, progress)?;
    let got = sha256_file(&part)?;
    Ok((part, got))
}

/// Downloads and extracts an archive artifact into a staging directory next
/// to `dest`, hashing the staged encoder. Returns the staged directory's
/// path and the hash; does not touch `dest`.
fn fetch_and_stage_archive(
    a: &Artifact,
    dest: &Path,
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<(PathBuf, String)> {
    let archive_tmp = paths::models_dir().join(format!("{}.tar.bz2", a.name));
    download_to(a.url, &archive_tmp, progress)?;
    let staged = extract_tar_bz2(&archive_tmp, dest)?;
    std::fs::remove_file(&archive_tmp).ok();
    let got = sha256_file(&staged.join("encoder.int8.onnx"))?;
    Ok((staged, got))
}

/// Discards a staged download that failed verification (or that we declined
/// to accept because it isn't pinned and `--update-lock` wasn't given).
/// Best-effort: the existing live installation is what matters, not this.
fn discard_staged(staged: &Path) {
    if staged.is_dir() {
        let _ = std::fs::remove_dir_all(staged);
    } else {
        let _ = std::fs::remove_file(staged);
    }
}

/// Promotes a verified staged file into place, replacing `dest` if present.
/// A same-filesystem file-to-file rename is atomic on its own.
fn promote_file(staged: &Path, dest: &Path) -> Result<()> {
    std::fs::rename(staged, dest).with_context(|| format!("installing {}", dest.display()))
}

/// Promotes a verified staged directory into place. `rename` cannot replace
/// a non-empty directory directly, so the previous install (if any) is
/// backed up first and removed only after the new one is safely in place;
/// this narrows, though cannot fully eliminate, the window in which `dest`
/// is briefly absent.
fn promote_dir(staged: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        let backup = sibling_with_suffix(dest, ".prev");
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        std::fs::rename(dest, &backup)
            .with_context(|| format!("backing up {}", dest.display()))?;
        std::fs::rename(staged, dest)
            .with_context(|| format!("installing {}", dest.display()))?;
        std::fs::remove_dir_all(&backup).ok();
    } else {
        std::fs::rename(staged, dest)
            .with_context(|| format!("installing {}", dest.display()))?;
    }
    Ok(())
}

fn promote_staged(staged: &Path, dest: &Path) -> Result<()> {
    if staged.is_dir() {
        promote_dir(staged, dest)
    } else {
        promote_file(staged, dest)
    }
}

pub fn download_all(
    required: &[&'static Artifact],
    update_lock: bool,
    progress: &mut dyn FnMut(&str, u64, Option<u64>),
) -> Result<()> {
    let mut lock = LockFile::load()?;
    for a in required.iter().copied() {
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
                    a.display
                );
            }
        }

        let dest = paths::models_dir().join(a.rel_path);
        // Fetch into a staging location and hash it there, *before* it ever
        // touches `dest`. This is what makes "a mismatch aborts setup and
        // leaves existing models untouched" true: a bad or interrupted
        // download never gets a chance to replace a good existing install.
        let (staged, got) = if a.archive {
            fetch_and_stage_archive(a, &dest, progress)?
        } else {
            fetch_and_stage_file(a, &dest, progress)?
        };

        match lock.hashes.get(a.name) {
            Some(expected) if expected != &got => {
                discard_staged(&staged);
                bail!(
                    "checksum mismatch for {}: expected {expected}, got {got}",
                    a.display
                );
            }
            Some(_) => {}
            None if !update_lock => {
                discard_staged(&staged);
                bail!(
                    "{} has no pinned hash; re-run with --update-lock",
                    a.display
                );
            }
            None => {}
        }

        promote_staged(&staged, &dest)?;
        if update_lock {
            lock.hashes.insert(a.name.to_string(), got);
        }
    }
    if update_lock {
        lock.save()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::config::AsrModel;

    const EVERY_MODEL: [AsrModel; 4] = [
        AsrModel::ParakeetTdtV3,
        AsrModel::ParakeetUnifiedEn,
        AsrModel::Nemotron35,
        AsrModel::ParakeetPrimelineDe,
    ];

    #[test]
    fn required_artifacts_is_the_support_pair_plus_exactly_one_asr_model() {
        for model in EVERY_MODEL {
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
        // Renaming this key invalidates models.lock.toml on every machine
        // that already downloaded the model. See Artifact::name's doc comment.
        assert_eq!(spec_for(AsrModel::ParakeetTdtV3).artifact.name, "parakeet");
    }

    #[test]
    fn every_artifact_name_is_usable_as_a_toml_bare_key() {
        // `Artifact::name` is a key in models.lock.toml. TOML bare keys allow
        // only letters, digits, `_` and `-`; a dot makes it a *dotted* key,
        // which silently nests it into a sub-table and makes the whole lock
        // file fail to parse -- taking down every read of it, not just the
        // one entry. Caught exactly this way once already.
        for a in all_artifacts() {
            assert!(
                !a.name.is_empty()
                    && a.name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{:?} is not a TOML bare key, so it cannot be a lock-file key",
                a.name
            );
        }
    }

    #[test]
    fn every_asr_model_enum_variant_has_a_catalogue_entry() {
        // `spec_for` panics on a variant with no entry, which is the failure
        // this asserts against: adding a variant without an artifact must not
        // compile its way into a panic on someone's first dictation.
        for model in EVERY_MODEL {
            let _ = spec_for(model);
        }
    }

    #[test]
    fn sha256_of_known_content_matches() {
        let dir = std::env::temp_dir().join("yappr-test-sha");
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

    /// A fresh, collision-free scratch directory for a single test -- same
    /// pattern as `src-tauri/src/setup.rs`'s identical helper, needed here
    /// too since `all_targets_look_present` must never be exercised against
    /// the real `models_dir()` (already populated with ~1.1 GB of real
    /// models on a machine that has run setup).
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-models-test-{tag}-{}-{n}", std::process::id()))
    }

    #[test]
    fn all_targets_look_present_is_false_for_a_missing_path() {
        let dir = scratch_dir("missing");
        let never_created = dir.join("nope.onnx");
        assert!(!all_targets_look_present(&[never_created]));
    }

    #[test]
    fn all_targets_look_present_is_false_for_an_empty_file() {
        let dir = scratch_dir("empty-file");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("truncated.onnx");
        std::fs::write(&p, b"").unwrap();
        assert!(
            !all_targets_look_present(&[p]),
            "a zero-byte file must not look present -- a truncated download \
             left one behind before, and the cheap check exists to catch \
             exactly that without a full hash"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn all_targets_look_present_is_true_only_when_every_target_is_present() {
        let dir = scratch_dir("mixed");
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present.onnx");
        std::fs::write(&present, b"not empty").unwrap();
        let missing = dir.join("missing.onnx");

        assert!(all_targets_look_present(std::slice::from_ref(&present)));
        assert!(!all_targets_look_present(&[present, missing]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_artifact_has_a_distinct_name_and_https_url() {
        let mut names = std::collections::HashSet::new();
        for a in all_artifacts() {
            assert!(names.insert(a.name), "duplicate artifact name: {}", a.name);
            assert!(a.url.starts_with("https://"), "{} is not https", a.name);
        }
        // Two support artifacts plus one entry per selectable ASR model.
        assert_eq!(all_artifacts().len(), SUPPORT_ARTIFACTS.len() + ASR_MODELS.len());
    }

    #[test]
    fn compiled_in_lock_covers_every_artifact() {
        let compiled = LockFile::compiled_in().unwrap();
        for a in all_artifacts() {
            assert!(
                compiled.hashes.contains_key(a.name),
                "crates/yappr-core/models.lock.toml has no pinned hash for {}",
                a.name
            );
        }
    }

    /// C2: proves a fresh machine -- no runtime `models.lock.toml` at all --
    /// still gets a real pin for every artifact, sourced from the file
    /// compiled into the binary. Before this fix, `LockFile::load()` in this
    /// exact situation returned an empty map, which is what let a fresh
    /// install burn the entire ~1.1 GB download only to be told to re-run
    /// with `--update-lock`.
    #[test]
    fn load_falls_back_to_the_compiled_in_pin_when_no_runtime_lock_exists() {
        let dir = std::env::temp_dir()
            .join(format!("yappr-test-no-runtime-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let never_created = dir.join("models.lock.toml");

        let lock = LockFile::load_from(&never_created).unwrap();

        for a in all_artifacts() {
            assert!(
                lock.hashes.contains_key(a.name),
                "compiled-in pin should cover {} when no runtime lock exists",
                a.name
            );
        }
        // Matches the hash actually committed in crates/yappr-core/models.lock.toml.
        assert_eq!(
            lock.hashes.get("silero").map(String::as_str),
            Some("9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6")
        );
    }

    #[test]
    fn load_prefers_a_runtime_entry_but_fills_gaps_from_the_compiled_in_pin() {
        let dir =
            std::env::temp_dir().join(format!("yappr-test-partial-runtime-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.lock.toml");
        std::fs::write(&path, "[hashes]\nsilero = \"deadbeef\"\n").unwrap();

        let lock = LockFile::load_from(&path).unwrap();

        assert_eq!(
            lock.hashes.get("silero").map(String::as_str),
            Some("deadbeef"),
            "an explicit runtime entry must win over the compiled-in pin"
        );
        assert!(
            lock.hashes.contains_key("s1-mini") && lock.hashes.contains_key("parakeet"),
            "artifacts absent from the runtime file must still fall back to the compiled-in pin"
        );

        std::fs::remove_dir_all(&dir).ok();
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
        let required = required_artifacts(AsrModel::ParakeetTdtV3);
        let missing = verify(&lock, &required).unwrap();
        // With an empty lock, every *required* artifact counts as unverified
        // -- and only the required ones, which is the whole point of the
        // selection-driven list.
        assert_eq!(missing.len(), required.len());
    }

    #[test]
    fn s1_mini_display_name_matches_the_license_text() {
        // The S1-mini license requires this exact capitalization in
        // user-facing text: "S1-mini" by "Superwhisper" — which a finetune's
        // display name must still carry verbatim. `name` (the committed
        // lock-file key) stays lowercase and is never shown to a user
        // directly.
        let s1 = all_artifacts().into_iter().find(|a| a.name == "s1-mini").unwrap();
        assert_eq!(s1.display, "S1-mini by Superwhisper (de-v3 Finetune)");
        assert!(s1.display.contains("S1-mini by Superwhisper"));
        assert_eq!(s1.name, "s1-mini", "lock-file key must not change");
    }

    #[test]
    fn sibling_with_suffix_preserves_dots_in_the_original_name() {
        // Path::with_extension would truncate at the dot inside the version
        // number ("...int8" -> "...0"), silently discarding the rest of the
        // artifact's directory name (verified empirically during review).
        let p = Path::new("/models/parakeet-tdt-0.6b-v3-int8");
        assert_eq!(
            sibling_with_suffix(p, ".staging"),
            Path::new("/models/parakeet-tdt-0.6b-v3-int8.staging")
        );
        assert_eq!(
            sibling_with_suffix(p, ".staged"),
            Path::new("/models/parakeet-tdt-0.6b-v3-int8.staged")
        );
    }

    #[test]
    fn promote_file_replaces_an_existing_destination() {
        let dir = std::env::temp_dir().join("yappr-test-promote-file");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("model.bin");
        let staged = dir.join("model.bin.part");
        std::fs::write(&dest, b"old content").unwrap();
        std::fs::write(&staged, b"new content").unwrap();

        promote_file(&staged, &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
        assert!(!staged.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn promote_dir_replaces_an_existing_nonempty_destination() {
        let dir = std::env::temp_dir().join("yappr-test-promote-dir");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("parakeet-tdt-0.6b-v3-int8");
        let staged = dir.join("parakeet-tdt-0.6b-v3-int8.staged");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.onnx"), b"old").unwrap();
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("encoder.int8.onnx"), b"new").unwrap();

        promote_dir(&staged, &dest).unwrap();

        assert!(dest.join("encoder.int8.onnx").exists());
        assert!(!dest.join("old.onnx").exists(), "old content must be fully replaced");
        assert!(!staged.exists());
        assert!(
            !sibling_with_suffix(&dest, ".prev").exists(),
            "backup must be cleaned up after a successful swap"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discard_staged_removes_files_and_directories() {
        let dir = std::env::temp_dir().join("yappr-test-discard");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let file = dir.join("x.part");
        std::fs::write(&file, b"x").unwrap();
        discard_staged(&file);
        assert!(!file.exists());

        let subdir = dir.join("x.staged");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("y"), b"y").unwrap();
        discard_staged(&subdir);
        assert!(!subdir.exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
