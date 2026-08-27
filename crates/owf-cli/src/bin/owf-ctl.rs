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
    let mut last_url = String::new();
    owf_core::models::download_all(update_lock, &mut |url, done, total| {
        // `last` tracks progress within a single artifact's download; reset it
        // whenever the URL changes so a small artifact's `done` (which starts
        // back at 0) is never compared against the previous, much larger
        // artifact's final `done` value (that comparison underflowed a u64
        // and panicked when downloading multiple artifacts in one run).
        if url != last_url {
            last_url = url.to_string();
            last = 0;
        }
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
