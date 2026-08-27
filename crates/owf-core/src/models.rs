use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

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

pub static ARTIFACTS: [Artifact; 3] = [
    Artifact {
        name: "parakeet",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        display: "Parakeet TDT 0.6b v3 (int8)",
        rel_path: "parakeet-tdt-0.6b-v3-int8",
        archive: true,
    },
    Artifact {
        name: "silero",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
        display: "Silero VAD",
        rel_path: "silero_vad.onnx",
        archive: false,
    },
    Artifact {
        name: "s1-mini",
        url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf",
        // License-mandated capitalization: "S1-mini" by "Superwhisper", exactly.
        display: "S1-mini by Superwhisper",
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

    #[test]
    fn s1_mini_display_name_matches_the_license_text() {
        // The S1-mini license requires this exact capitalization in
        // user-facing text: "S1-mini" by "Superwhisper". `name` (the
        // committed lock-file key) stays lowercase and is never shown to a
        // user directly.
        let s1 = ARTIFACTS.iter().find(|a| a.name == "s1-mini").unwrap();
        assert_eq!(s1.display, "S1-mini by Superwhisper");
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
        let dir = std::env::temp_dir().join("owf-test-promote-file");
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
        let dir = std::env::temp_dir().join("owf-test-promote-dir");
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
        let dir = std::env::temp_dir().join("owf-test-discard");
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
