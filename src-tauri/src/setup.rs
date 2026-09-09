//! `yappr --setup`, `--debug` and `--purge-logs`: everything that
//! inspects or provisions the local install without talking to the daemon.
//! `send()` and `open_settings()` used to live here too; `client::dispatch`
//! replaces both directly, so they were deleted rather than moved.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use anyhow::{Context, Result};
use yappr_core::desktop::{self, Desktop};

/// Prints a short summary of the most recently written debug record --
/// see `yappr_core::debug` and `[debug]` in config.toml. Reads straight off
/// disk (not through the daemon): the debug facility writes independently
/// of `owf-ctl`, so there is nothing to ask the daemon for.
pub fn debug_summary() -> Result<()> {
    let cfg = yappr_core::config::Config::load().context("loading config")?;
    if !cfg.debug.enabled {
        eprintln!("[debug].enabled is false in config.toml -- no records are being written");
        std::process::exit(1);
    }

    let dir = yappr_core::debug::expand_tilde(&cfg.debug.dir);
    let logs_dir = dir.join("logs");
    let json_path = yappr_core::debug::latest_record_path(&logs_dir)
        .with_context(|| format!("no debug records found under {}", logs_dir.display()))?;
    let contents = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let record: yappr_core::debug::DebugRecord = serde_json::from_str(&contents)
        .with_context(|| format!("parsing {}", json_path.display()))?;

    print_debug_summary(&record, &json_path, &dir);
    Ok(())
}

fn print_debug_summary(
    record: &yappr_core::debug::DebugRecord,
    json_path: &Path,
    dir: &Path,
) {
    println!("most recent utterance: {}", record.ts);

    if let Some(c) = &record.capture {
        println!(
            "  capture: {} native samples / {} expected (ratio {:.3}, {} stream error(s))",
            c.native_samples_captured, c.native_samples_expected, c.capture_ratio, c.stream_errors
        );
        println!(
            "           device={:?} native_rate={} channels={} mono_16k_samples={}",
            c.device, c.native_sample_rate, c.channels, c.mono_16k_samples
        );
    } else {
        println!("  capture: (no capture stats recorded for this utterance)");
    }

    println!(
        "  raw audio:     rms={:.4} peak={:.4}",
        record.audio.raw.rms, record.audio.raw.peak
    );
    match &record.audio.trimmed {
        Some(t) => println!("  trimmed audio: rms={:.4} peak={:.4}", t.rms, t.peak),
        None => println!("  trimmed audio: (none -- no speech found)"),
    }

    match (record.vad.start_secs, record.vad.end_secs) {
        (Some(s), Some(e)) => println!(
            "  vad span: {}..{} samples ({:.3}s .. {:.3}s)",
            record.vad.start_sample.unwrap_or(0),
            record.vad.end_sample.unwrap_or(0),
            s,
            e
        ),
        _ => println!("  vad span: no speech found"),
    }

    match &record.asr_raw {
        Some(raw) => println!("  raw transcript: {raw:?}"),
        None => println!("  raw transcript: (none)"),
    }

    match &record.vocab {
        Some(subs) if !subs.is_empty() => {
            println!("  vocabulary: {} correction(s)", subs.len());
            for sub in subs {
                let how = if sub.fuzzy { "fuzzy" } else { "exact" };
                println!("      {:?} -> {:?} ({how})", sub.from, sub.to);
            }
        }
        // Distinguished on purpose: "nothing matched" is the answer to a
        // different question than "the section is not configured", and a user
        // debugging a rule that will not fire needs to tell them apart.
        _ => println!("  vocabulary: no corrections applied"),
    }

    match &record.inject {
        Some(i) => {
            println!("  final text ({}): {:?}", i.backend, i.final_text);
            // The ydotool backend picks its paste chord from this, so an
            // unknown class is a diagnosis, not a missing detail -- say so
            // rather than printing nothing.
            match &i.window_class {
                Some(c) => println!("  target window class: {c:?}"),
                None => println!(
                    "  target window class: (unknown -- hyprctl could not answer; \
                     the ydotool backend pasted with plain Ctrl+V)"
                ),
            }
            if let Some(err) = &i.primary_error {
                println!(
                    "  primary injector {} failed: {err}",
                    i.primary_backend.as_deref().unwrap_or("?"),
                );
            }
        }
        None => println!("  final text: (nothing was injected)"),
    }

    if let Some(g) = &record.guardrail {
        println!(
            "  guardrail: {} reason={:?} overlap={:?} word_ratio={:?}",
            g.verdict, g.reason, g.overlap, g.word_ratio
        );
    }

    println!("  files:");
    println!("    json:        {}", json_path.display());
    let raw_wav = dir.join("audio").join(format!("{}-raw.wav", record.ts));
    let trimmed_wav = dir.join("audio").join(format!("{}-trimmed.wav", record.ts));
    println!(
        "    raw wav:     {}{}",
        raw_wav.display(),
        if raw_wav.exists() { "" } else { " (missing -- save_audio was off?)" }
    );
    println!(
        "    trimmed wav: {}{}",
        trimmed_wav.display(),
        if trimmed_wav.exists() { "" } else { " (missing)" }
    );
}

/// Carries the progress-reporting state from one report decision to the
/// next: how many bytes had been reported as of the last report, and for
/// which URL that count applies. Shared by every consumer of
/// `download_all`'s progress callback -- the CLI's `progress_line` below and
/// the Setup pane's structured `"setup-progress"` events in `provision.rs`
/// -- so the underflow fix `crosses_report_threshold` documents lives in
/// exactly one place instead of being re-derived (and possibly re-broken) by
/// each caller.
#[derive(Default, Clone)]
pub(crate) struct ProgressState {
    last: u64,
    last_url: String,
}

/// Decides whether *any* consumer of `download_all`'s progress callback
/// should report now, and returns the state to carry into the next call.
///
/// Reports roughly every 8 MB within a single artifact's download, plus
/// always on completion (`Some(done) == total`), so a 622 MB download does
/// not spam whatever is consuming these reports (a terminal, or a Tauri
/// event channel).
///
/// A new `url` resets the "since last report" byte count to 0 instead of
/// comparing the new download's `done` (which starts back at 0) against the
/// *previous* artifact's much larger final `done`. That comparison used to
/// live directly in `setup`'s closure as `done - last`, which underflowed a
/// `u64` and panicked partway through a real ~1.1 GB, three-artifact run the
/// first time `download_all` moved from the ~487 MB parakeet download to the
/// ~629 KB silero download.
pub(crate) fn crosses_report_threshold(
    url: &str,
    done: u64,
    total: Option<u64>,
    state: ProgressState,
) -> (bool, ProgressState) {
    let last = if url == state.last_url { state.last } else { 0 };
    let report = done - last > 8 << 20 || Some(done) == total;
    let new_last = if report { done } else { last };
    (
        report,
        ProgressState {
            last: new_last,
            last_url: url.to_string(),
        },
    )
}

/// Decides whether a progress line should be printed for this update, and
/// returns the state to carry into the next call. The CLI-specific half
/// (formatting) of `crosses_report_threshold`'s decision.
fn progress_line(
    url: &str,
    done: u64,
    total: Option<u64>,
    state: ProgressState,
) -> (Option<String>, ProgressState) {
    let (report, new_state) = crosses_report_threshold(url, done, total, state);
    let line = report.then(|| match total {
        Some(t) => format!("  {:>5.1}%  {}", 100.0 * done as f64 / t as f64, url),
        None => format!("  {} MB  {}", done >> 20, url),
    });
    (line, new_state)
}

/// Prints one `<label>  <name>  (<why>)` line, columns aligned so a run of
/// `ok`/`MISSING`/`absent (optional)` lines stays readable.
fn status_line(label: &str, name: &str, why: &str) {
    eprintln!("  {label:<18} {name}  ({why})");
}

/// One external program this install may need: the binary to look for, why
/// it is needed, the pacman package that provides it, and whether its
/// absence is fatal.
struct Prerequisite {
    bin: &'static str,
    why: &'static str,
    pkg: &'static str,
    fatal: bool,
}

/// Which programs are worth checking on `d`, and which of them are fatal.
///
/// Desktop-dependent for exactly one reason: **`wtype` cannot work on
/// GNOME.** It types through the virtual-keyboard protocol Mutter does not
/// implement, so there it is not missing, it is inapplicable -- which is why
/// `wizard::recommended_backend` sends GNOME to `ydotool` in the first
/// place. Reporting it anyway cost a GNOME user a `sudo pacman -S wtype`
/// that would still type nothing, and -- because a *fatal* gap makes
/// `setup_status` report the whole install as not ready -- a wizard that
/// reopened on every launch, forever. That gap is a standing "Einrichtung
/// unvollständig" banner rather than a reopening wizard since 2026-09-09
/// (invariant 13); a permanent banner nobody can act on is no better.
///
/// `ydotool` stays **optional** even on GNOME, where it is the only backend
/// that works. Everywhere else that is because `wtype` is the default
/// injector and needs no setup, so a machine without `ydotool` is fully
/// working and listing it would put a package almost nobody needs into the
/// "install these" list. On GNOME it is because making it fatal would
/// recreate exactly the never-ready loop above: it also needs `ydotoold`
/// running, which no `pacman -S` line can report. The wizard states that
/// requirement separately instead, unit and all -- `wizard::backend_prereqs`.
fn prerequisites_for(d: &Desktop) -> Vec<Prerequisite> {
    let gnome = matches!(d, Desktop::Gnome);
    let mut checks = Vec::new();
    if !gnome {
        checks.push(Prerequisite {
            bin: "wtype",
            why: "typing into the focused window",
            pkg: "wtype",
            fatal: true,
        });
    }
    checks.push(Prerequisite {
        bin: "wl-copy",
        why: "clipboard fallback when typing fails",
        pkg: "wl-clipboard",
        fatal: true,
    });
    checks.push(Prerequisite {
        bin: "ydotool",
        why: if gnome {
            "pasting via /dev/uinput -- the only backend Mutter supports"
        } else {
            "pasting via /dev/uinput when inject.backend = \"ydotool\""
        },
        pkg: "ydotool",
        fatal: false,
    });
    checks.push(Prerequisite {
        bin: "hyprctl",
        why: "per-application style rules",
        pkg: "hyprland",
        fatal: false,
    });
    checks
}

/// Reports required external programs and libraries without installing
/// anything. Returns the pacman packages that would fix every *fatal* gap
/// found; empty means every essential prerequisite is present (optional
/// ones, like `hyprctl`, may still be absent).
///
/// Spec 14.3 requires `setup` to report missing prerequisites rather than
/// failing obscurely later -- e.g. `wtype` missing at dictation time.
///
/// This list used to be longer. `llama-server` was on it, and so was a
/// bespoke ggml-compute-backend probe (`ggml_backend_present`, which parsed
/// `ldconfig` output to find `libggml-base.so` and then looked for a sibling
/// backend library) that existed purely to catch ggml's opaque "no backends
/// are loaded" before a user hit it mid-dictation. S1-mini is linked into
/// this binary now, so neither the program nor the backend can be missing:
/// if the app started, they are present.
///
/// It also used to be desktop-*independent*, and that was a bug on GNOME --
/// see [`prerequisites_for`].
///
/// `pub(crate)`: the Setup pane's `setup_status` command (`provision.rs`)
/// reports this alongside model presence, which is a separate question
/// (`yappr_core::models::verify`) -- neither subsumes the other, so both are
/// checked and reported independently rather than duplicating either check.
pub(crate) fn check_prerequisites() -> Vec<&'static str> {
    check_prerequisites_for(&desktop::detect())
}

/// [`check_prerequisites`] against a stated desktop rather than this
/// session's -- the seam the tests drive, since a test cannot pick the
/// desktop it runs under.
fn check_prerequisites_for(d: &Desktop) -> Vec<&'static str> {
    let mut missing_pkgs = Vec::new();
    for Prerequisite { bin, why, pkg, fatal } in prerequisites_for(d) {
        let found = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        match (found, fatal) {
            (true, _) => status_line("ok", bin, why),
            (false, true) => {
                status_line("MISSING", bin, why);
                missing_pkgs.push(pkg);
            }
            (false, false) => status_line("absent (optional)", bin, why),
        }
    }

    // Absent, not fatal: `paths::xdg_runtime()` falls back to
    // `std::env::temp_dir()` when unset, so the daemon and `owf-ctl` still
    // agree on a socket location -- just a less conventional one.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").is_some();
    status_line(
        if runtime { "ok" } else { "absent (optional)" },
        "XDG_RUNTIME_DIR",
        "socket location",
    );

    missing_pkgs
}

pub fn setup(update_lock: bool) -> Result<()> {
    eprintln!("prerequisites:");
    let missing = check_prerequisites();
    if !missing.is_empty() {
        anyhow::bail!(
            "install the missing programs, then re-run: sudo pacman -S {}",
            missing.join(" ")
        );
    }

    // `--update-lock` is the developer action that maintains the pins, so it
    // is the one caller that legitimately wants the *whole* catalogue: a
    // model with no pin is one `download_all` refuses to install, so leaving
    // an entry unpinned would make it undownloadable for every user who
    // selects it. Ordinary provisioning takes only what the selection
    // requires (spec asr-model §4, §7).
    let artifacts = if update_lock {
        yappr_core::models::all_artifacts()
    } else {
        let model = match yappr_core::config::Config::load() {
            Ok(c) => c.asr.model,
            Err(_) => yappr_core::config::AsrConfig::default().model,
        };
        yappr_core::models::required_artifacts(model)
    };

    let mut state = ProgressState::default();
    yappr_core::models::download_all(&artifacts, update_lock, &mut |url, done, total| {
        let (line, new_state) = progress_line(url, done, total, std::mem::take(&mut state));
        state = new_state;
        if let Some(line) = line {
            eprintln!("{line}");
        }
    })?;
    println!("models ready in {}", yappr_core::paths::models_dir().display());
    Ok(())
}

/// What [`purge_logs_at`] actually did, so [`purge_logs`] can report it
/// precisely and tests can assert on the outcome directly instead of
/// scraping printed text.
enum PurgeOutcome {
    Removed,
    AlreadyAbsent,
}

/// Deletes exactly `path` and reports which of those two things happened,
/// treating an already-absent file as success rather than an error --
/// running `--purge-logs` twice in a row (or on a fresh install that has
/// never rejected anything) must not fail.
///
/// Split out from `purge_logs` so this destructive operation is testable
/// against a scratch path instead of the real
/// `~/.local/state/yappr/rejections.jsonl`.
fn purge_logs_at(path: &Path) -> Result<PurgeOutcome> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(PurgeOutcome::Removed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PurgeOutcome::AlreadyAbsent),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Spec 5.1/9.3: `owf-ctl setup --purge-logs` is the only documented way to
/// clear the guardrail-rejection dataset (`rejections.jsonl`) -- a
/// local-only file of raw/cleaned transcript pairs (spec 9.3) that is never
/// transmitted anywhere, but is still real user dictation content sitting on
/// disk, so clearing it needs an explicit, deliberate command rather than
/// happening as a side effect of `setup` or `reload`.
///
/// Deletes exactly that one file -- nothing else under
/// `~/.local/state/yappr/` (the daemon's own `yappr.log`,
/// in particular, is untouched) -- and, being destructive, always says
/// exactly what it did: the path it removed, or that there was nothing to
/// remove.
pub fn purge_logs() -> Result<()> {
    let path = yappr_core::paths::rejections_file();
    match purge_logs_at(&path)? {
        PurgeOutcome::Removed => println!("removed {} -- rejection dataset cleared", path.display()),
        PurgeOutcome::AlreadyAbsent => {
            println!("{} does not exist -- nothing to remove", path.display())
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, collision-free scratch directory for a single test. Not a
    /// dependency: `tempfile` isn't in `[dev-dependencies]` here either (see
    /// the identical helper in `yappr-core`'s `inject.rs` tests).
    fn scratch_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-ctl-test-{tag}-{}-{n}", std::process::id()))
    }

    /// The GNOME half of `prerequisites_for`. `wtype` types through a
    /// protocol Mutter does not implement, so asking a GNOME user to install
    /// it asks for a package that will still type nothing -- and, as a
    /// *fatal* gap, it kept `setup_status` reporting the install as not
    /// ready: a reopening wizard then, a permanent banner now.
    #[test]
    fn gnome_is_never_asked_to_install_wtype() {
        assert!(
            !prerequisites_for(&Desktop::Gnome).iter().any(|c| c.bin == "wtype"),
            "wtype must not even be reported as absent on GNOME"
        );
        // Asserted on the returned list, not just the plan: this is the
        // value `setup_status` hands the wizard, and it must not name wtype
        // whatever this machine happens to have on PATH.
        assert!(!check_prerequisites_for(&Desktop::Gnome).contains(&"wtype"));
    }

    /// Everywhere else `wtype` is the default injector, and its absence is
    /// still the thing that makes a dictation silently produce nothing.
    #[test]
    fn every_other_desktop_still_requires_wtype() {
        for d in [Desktop::Hyprland, Desktop::Other("KDE".into()), Desktop::Unknown] {
            assert!(
                prerequisites_for(&d).iter().any(|c| c.bin == "wtype" && c.fatal),
                "for {d:?}"
            );
        }
    }

    /// Dropping `wtype` on GNOME must not drop anything beside it -- and
    /// `ydotool`, the only backend that works there, stays optional on
    /// purpose (see `prerequisites_for`'s doc comment: it also needs
    /// `ydotoold`, so a fatal entry would restore the never-ready loop).
    #[test]
    fn gnome_still_checks_everything_else_with_the_same_severities() {
        let checks = prerequisites_for(&Desktop::Gnome);
        assert!(checks.iter().any(|c| c.bin == "wl-copy" && c.fatal));
        assert!(checks.iter().any(|c| c.bin == "ydotool" && !c.fatal));
        assert!(checks.iter().any(|c| c.bin == "hyprctl" && !c.fatal));
    }

    #[test]
    fn resets_progress_when_the_url_changes_without_panicking() {
        // Finish a large artifact (parakeet-sized: ~487 MB).
        let big_url = "https://example.com/big.tar.bz2";
        let (line, state) = progress_line(big_url, 487_170_055, Some(487_170_055), ProgressState::default());
        assert!(line.is_some(), "reaching the total must always report");
        assert_eq!(state.last, 487_170_055);
        assert_eq!(state.last_url, big_url);

        // Start a much smaller artifact (silero-sized: ~629 KB). Naively
        // comparing this `done` against the previous artifact's `last`
        // would underflow a u64 and panic; this is the exact transition
        // that crashed the real download.
        let small_url = "https://example.com/small.onnx";
        let (line, state) = progress_line(small_url, 16_384, Some(643_854), state);
        assert!(
            line.is_none(),
            "16 KiB into a fresh artifact should not cross the 8 MB threshold"
        );
        assert_eq!(state.last, 0, "byte count must reset for the new URL");
        assert_eq!(state.last_url, small_url);

        // The small artifact completing must still report.
        let (line, state) = progress_line(small_url, 643_854, Some(643_854), state);
        assert!(line.is_some());
        assert_eq!(state.last, 643_854);
    }

    #[test]
    fn reports_roughly_every_eight_megabytes() {
        let url = "https://example.com/x";
        let (line, state) = progress_line(url, 1 << 20, Some(100 << 20), ProgressState::default());
        assert!(line.is_none(), "1 MiB in should not yet report");

        let (line, state) = progress_line(url, 9 << 20, Some(100 << 20), state);
        assert!(line.is_some(), "9 MiB in is > 8 MiB since the last report");

        let (line, _state) = progress_line(url, 10 << 20, Some(100 << 20), state);
        assert!(line.is_none(), "1 MiB since the last report should not yet report again");
    }

    #[test]
    fn falls_back_to_a_raw_mb_count_without_a_content_length() {
        let url = "https://example.com/x";
        let (line, _state) = progress_line(url, 9 << 20, None, ProgressState::default());
        let line = line.expect("crossing the threshold must report even without a total");
        assert!(line.contains("MB"));
        assert!(!line.contains('%'));
    }

    #[test]
    fn purge_logs_at_removes_an_existing_file() {
        let dir = scratch_dir("purge-existing");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rejections.jsonl");
        std::fs::write(&path, b"{\"ts\":\"...\"}\n").unwrap();

        let outcome = purge_logs_at(&path).unwrap();

        assert!(matches!(outcome, PurgeOutcome::Removed));
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn purge_logs_at_treats_an_already_absent_file_as_success() {
        // Running `--purge-logs` twice in a row, or on a fresh install that
        // has never rejected anything, must not be an error.
        let dir = scratch_dir("purge-absent");
        let path = dir.join("rejections.jsonl"); // dir doesn't even exist yet

        let outcome = purge_logs_at(&path).unwrap();

        assert!(matches!(outcome, PurgeOutcome::AlreadyAbsent));
    }

    #[test]
    fn purge_logs_at_touches_only_the_path_it_is_given() {
        // Spec 5.1/9.3 names exactly one file (`rejections.jsonl`) as what
        // `--purge-logs` clears -- proves it doesn't reach for anything else
        // that might live alongside it, like the daemon's own log file.
        let dir = scratch_dir("purge-scoped");
        std::fs::create_dir_all(&dir).unwrap();
        let rejections = dir.join("rejections.jsonl");
        let daemon_log = dir.join("yappr.log");
        std::fs::write(&rejections, b"{}\n").unwrap();
        std::fs::write(&daemon_log, b"log line\n").unwrap();

        purge_logs_at(&rejections).unwrap();

        assert!(!rejections.exists());
        assert!(daemon_log.exists(), "purge_logs_at must not touch any other file");
        std::fs::remove_dir_all(&dir).ok();
    }
}
