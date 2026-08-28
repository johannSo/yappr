//! `openwhisprflow --subscribe`: the one-shot CLI counterpart to the
//! overlay's own long-lived socket client (`connection.rs`). Split out of
//! `setup.rs` because `client::dispatch` calls it directly and `setup.rs`
//! is reserved for the setup/debug/purge-logs surface.

use anyhow::{Context, Result};
use owf_core::proto::Request;

/// Connects to the daemon, sends `Request::Subscribe`, and prints every
/// `OverlayEvent` NDJSON line it receives until the daemon closes the
/// connection -- the same wire path the overlay itself will use (spec 12).
/// A plain pass-through rather than parsing each line into an `OverlayEvent`
/// and re-serializing it: the daemon's own wire format *is* the thing being
/// sanity-checked here, so printing anything other than exactly what came
/// off the socket would hide a wire-format bug rather than surface it.
///
/// This only ever reads from the daemon; it never sends `ptt-start` or any
/// other command, so running it opens no microphone.
pub fn subscribe() -> Result<()> {
    use std::io::{BufRead, BufReader, Write as _};
    use std::os::unix::net::UnixStream;

    let sock = owf_core::paths::runtime_socket();
    let stream = UnixStream::connect(&sock).with_context(|| {
        format!("cannot reach the daemon at {} — is owf-daemon running?", sock.display())
    })?;
    let mut writer = stream.try_clone().context("cloning socket for writing")?;
    writeln!(writer, "{}", serde_json::to_string(&Request::Subscribe)?)?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).context("reading from the daemon")?;
        if n == 0 {
            break; // the daemon closed the connection.
        }
        print!("{line}");
        std::io::stdout().flush().ok();
    }
    Ok(())
}
