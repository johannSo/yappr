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
