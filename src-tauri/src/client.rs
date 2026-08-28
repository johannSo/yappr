//! Every flag that is not "start the app" runs here, in a process that never
//! initialises Tauri, GTK or WebKit and never loads a model.
//!
//! Spec §2: the client path is one socket connection, one request line, one
//! response line. Under hold-to-talk its latency ate the start of every
//! utterance; under toggle the user presses and *then* speaks, so it is a
//! performance note rather than a gate — but the structure that keeps it
//! cheap is still worth a test (see `only_run_starts_the_app_...`).

use anyhow::Result;

use crate::cli::Route;

/// Whether this route is fully handled without starting the app.
/// Pure, so the structural guarantee is testable without spawning anything.
pub fn handled_without_starting_the_app(route: &Route) -> bool {
    !matches!(route, Route::Run | Route::Replay(_))
}

/// Runs a non-`Run` route to completion. Returns `Ok(false)` when the caller
/// should go on to start the app.
pub fn dispatch(route: Route) -> Result<bool> {
    if !handled_without_starting_the_app(&route) {
        return Ok(false);
    }
    match route {
        Route::Send(req) => {
            let resp = owf_core::proto::send(&req)?;
            println!("{}", serde_json::to_string(&resp)?);
            if !resp.ok {
                std::process::exit(1);
            }
        }
        Route::Subscribe => crate::client_stream::subscribe()?,
        Route::Debug => crate::setup::debug_summary()?,
        Route::Bench => crate::bench::run()?,
        Route::PrintShortcuts => print!("{}", owf_core::hypr::shortcut_config()),
        Route::PurgeLogs => crate::setup::purge_logs()?,
        Route::UpdateLock => crate::setup::setup(true)?,
        Route::Usage => {
            eprintln!("{}", crate::cli::USAGE);
            std::process::exit(2);
        }
        Route::Run | Route::Replay(_) => unreachable!("guarded above"),
    }
    Ok(true)
}

/// Spec §2: a user whose only interface is a shortcut needs a failure to be
/// visible without a terminal. The shortcut fires, the app is not running,
/// and stderr goes nowhere anyone will read.
pub fn notify_failure(e: &anyhow::Error) {
    let body = format!("{e:#}");
    owf_core::procutil::notify_send("OpenWhisprFlow", &body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Route;

    /// The structural guarantee behind the flag surface: only `Route::Run`
    /// starts a window. Everything else is handled and exits.
    ///
    /// This matters because argument dispatch sits *above* `tauri::Builder`
    /// in `main`. An edit that moves it below would make every shortcut press
    /// pay GTK and WebKit initialisation, which is exactly the cost spec §2
    /// measures and §12 item 4 watches. A structural test fails loudly where
    /// a latency regression would only feel vaguely sluggish.
    #[test]
    fn only_run_starts_the_app_every_other_route_exits() {
        assert!(!handled_without_starting_the_app(&Route::Run));
        for r in [
            Route::Send(owf_core::proto::Request::Toggle),
            Route::Send(owf_core::proto::Request::Quit),
            Route::Subscribe,
            Route::Debug,
            Route::Bench,
            Route::PrintShortcuts,
            Route::PurgeLogs,
            Route::UpdateLock,
            Route::Usage,
        ] {
            assert!(
                handled_without_starting_the_app(&r),
                "{r:?} must not reach tauri::Builder"
            );
        }
    }

    /// `--replay` is the one non-Run route that *does* open a window: it
    /// drives the overlay from a fixture. Pinned separately so it is an
    /// explicit exception rather than an oversight.
    #[test]
    fn replay_opens_a_window_because_that_is_what_it_is_for() {
        assert!(!handled_without_starting_the_app(&Route::Replay("f.ndjson".into())));
    }
}
