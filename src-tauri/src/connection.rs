//! Connects to the daemon's Unix socket, sends `Subscribe`, and streams
//! `OverlayEvent`s to a callback for as long as the process runs --
//! reconnecting with exponential backoff whenever the daemon isn't up yet
//! or the connection drops.
//!
//! This is the overlay's read-only client for the socket documented in
//! `crates/owf-core/src/proto.rs`. It sends exactly one line ever
//! (`Subscribe`) and only reads afterwards, so running it opens no
//! microphone and starts no dictation -- the same guarantee `owf-ctl
//! subscribe` documents about itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::wire::{OverlayEvent, SUBSCRIBE_LINE};

/// First retry delay after a failed connection attempt.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
/// Backoff ceiling. The daemon takes roughly 8s to warm on session start,
/// so a handful of doublings from 250ms comfortably covers that window
/// (250, 500, 1000, 2000, 4000, 5000, 5000, ...) without the overlay
/// hammering a socket that isn't there yet.
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Delay before reconnecting after a *clean* disconnect (the daemon closed
/// the connection deliberately, e.g. it's restarting). Short, because this
/// isn't a failure to back off from -- the daemon may already be back.
const CLEAN_DISCONNECT_RETRY: Duration = Duration::from_millis(250);

/// Mirrors `owf_core::paths::runtime_socket()` without depending on
/// `owf-core` -- see `wire.rs`'s module doc for why this crate keeps its
/// own copy of the small pieces of the wire contract it needs.
fn runtime_socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("openwhisprflow.sock")
}

/// Runs forever on the calling thread. Intended to be spawned on its own
/// `std::thread` at startup so a slow or absent daemon never blocks the
/// Tauri event loop or the window's first paint.
pub fn run(on_event: impl Fn(OverlayEvent) + Send + 'static) -> ! {
    let sock = runtime_socket();
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match connect_and_stream(&sock, &on_event) {
            Ok(()) => {
                // Clean EOF: the daemon closed the connection on purpose.
                // Not a failure -- reset backoff and try again promptly.
                backoff = INITIAL_BACKOFF;
                std::thread::sleep(CLEAN_DISCONNECT_RETRY);
            }
            Err(_) => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Connects once, subscribes, and streams events until the daemon closes
/// the connection (`Ok(())`) or a socket error occurs (`Err`).
fn connect_and_stream(
    sock: &PathBuf,
    on_event: &impl Fn(OverlayEvent),
) -> std::io::Result<()> {
    let stream = UnixStream::connect(sock)?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{SUBSCRIBE_LINE}")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // the daemon closed the connection
        }
        match serde_json::from_str::<OverlayEvent>(line.trim()) {
            Ok(event) => on_event(event),
            Err(e) => {
                // A wire-format mismatch must not take the overlay down;
                // drop the line and keep listening for the next one.
                eprintln!("overlay: dropping malformed event line ({e}): {line:?}");
            }
        }
    }
}
