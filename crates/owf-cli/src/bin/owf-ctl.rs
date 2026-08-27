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

/// Carries the progress-reporting state from one `progress_line` call to the
/// next: how many bytes had been reported as of the last line printed, and
/// for which URL that count applies.
#[derive(Default)]
struct ProgressState {
    last: u64,
    last_url: String,
}

/// Decides whether a progress line should be printed for this update, and
/// returns the state to carry into the next call.
///
/// Reports roughly every 8 MB within a single artifact's download, plus
/// always on completion (`Some(done) == total`), so a 622 MB download does
/// not spam the terminal.
///
/// A new `url` resets the "since last report" byte count to 0 instead of
/// comparing the new download's `done` (which starts back at 0) against the
/// *previous* artifact's much larger final `done`. That comparison used to
/// live directly in `setup`'s closure as `done - last`, which underflowed a
/// `u64` and panicked partway through a real ~1.1 GB, three-artifact run the
/// first time `download_all` moved from the ~487 MB parakeet download to the
/// ~629 KB silero download.
fn progress_line(
    url: &str,
    done: u64,
    total: Option<u64>,
    state: ProgressState,
) -> (Option<String>, ProgressState) {
    let last = if url == state.last_url { state.last } else { 0 };
    if done - last > 8 << 20 || Some(done) == total {
        let line = match total {
            Some(t) => format!("  {:>5.1}%  {}", 100.0 * done as f64 / t as f64, url),
            None => format!("  {} MB  {}", done >> 20, url),
        };
        (
            Some(line),
            ProgressState {
                last: done,
                last_url: url.to_string(),
            },
        )
    } else {
        (
            None,
            ProgressState {
                last,
                last_url: url.to_string(),
            },
        )
    }
}

fn setup(update_lock: bool) -> Result<()> {
    let mut state = ProgressState::default();
    owf_core::models::download_all(update_lock, &mut |url, done, total| {
        let (line, new_state) = progress_line(url, done, total, std::mem::take(&mut state));
        state = new_state;
        if let Some(line) = line {
            eprintln!("{line}");
        }
    })?;
    println!("models ready in {}", owf_core::paths::models_dir().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
