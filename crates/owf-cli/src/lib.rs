//! One command, three jobs.
//!
//! `owf-daemon`, `owf-ctl` and `owf-bench` used to be three binaries built
//! from three files under `src/bin/`. They are one binary now: [`ctl`] and
//! [`bench`] moved here unchanged. The daemon moved one hop further, into
//! `owf_core::server`, so a later Tauri app can host it in-process without
//! this crate's binary in between. What the merge had to preserve is the
//! push-to-talk keybinding, which the compositor runs on every key press
//! and release --
//! `o.bind("SUPER + D", ..., "owf-ctl ptt-start")` and its `release = true`
//! twin. Keeping `owf-ctl` as the command name is what makes those keep
//! working with no change to anyone's Hyprland config; the daemon and the
//! bench became subcommands of it.
//!
//! [`route`] is deliberately a pure function over `argv[1..]`, so the promise
//! above is a unit test rather than something that has to be tried on a live
//! desktop.

pub mod bench;
pub mod ctl;

use owf_core::proto::Request;

pub const USAGE: &str = "\
usage: owf-ctl <ptt-start|ptt-stop|cancel|status|reload|subscribe|debug>
       owf-ctl setup [--update-lock|--print-hypr|--purge-logs]
       owf-ctl daemon
       owf-ctl bench
       owf-ctl settings";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Plain,
    UpdateLock,
    PrintHypr,
    PurgeLogs,
}

/// What an invocation resolves to. One variant per command, so that routing
/// can be checked exhaustively without starting a process, opening a socket
/// or touching the filesystem.
/// Not `Copy`/`Eq` any more: `Request::SetConfig` carries a `serde_json::Value`.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Daemon,
    Bench,
    Send(Request),
    Subscribe,
    Debug,
    Setup(SetupMode),
    Settings,
    Usage,
}

/// Resolves `argv[1..]` to an action.
///
/// The slice patterns are exact-length on purpose: `["ptt-start"]` matches a
/// lone `ptt-start` and nothing else, so `owf-ctl ptt-start now` falls through
/// to usage instead of silently ignoring the typo.
pub fn route(args: &[&str]) -> Route {
    match args {
        ["ptt-start"] => Route::Send(Request::PttStart),
        ["ptt-stop"] => Route::Send(Request::PttStop),
        ["cancel"] => Route::Send(Request::Cancel),
        ["status"] => Route::Send(Request::Status),
        ["reload"] => Route::Send(Request::Reload),
        ["subscribe"] => Route::Subscribe,
        ["debug"] => Route::Debug,
        ["setup"] => Route::Setup(SetupMode::Plain),
        ["setup", "--update-lock"] => Route::Setup(SetupMode::UpdateLock),
        ["setup", "--print-hypr"] => Route::Setup(SetupMode::PrintHypr),
        ["setup", "--purge-logs"] => Route::Setup(SetupMode::PurgeLogs),
        ["daemon"] => Route::Daemon,
        ["settings"] => Route::Settings,
        ["bench"] => Route::Bench,
        _ => Route::Usage,
    }
}

pub fn dispatch(route: Route) -> anyhow::Result<()> {
    match route {
        Route::Daemon => owf_core::server::run(),
        Route::Bench => bench::run(),
        Route::Send(req) => ctl::send(req),
        Route::Subscribe => ctl::subscribe(),
        Route::Debug => ctl::debug_summary(),
        Route::Setup(SetupMode::Plain) => ctl::setup(false),
        Route::Setup(SetupMode::UpdateLock) => ctl::setup(true),
        Route::Setup(SetupMode::PrintHypr) => {
            print!("{}", owf_core::hypr::hypr_config());
            Ok(())
        }
        Route::Setup(SetupMode::PurgeLogs) => ctl::purge_logs(),
        Route::Settings => ctl::open_settings(),
        Route::Usage => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract that keeps a working desktop working. Every one of these
    /// invocations resolved to an action before the three binaries were
    /// merged; each must still resolve to the *same* action afterwards. The
    /// first three lines are what `~/.config/hypr/bindings.lua` runs on every
    /// dictation, so a regression here is a broken keyboard, not a failing
    /// build.
    #[test]
    fn every_invocation_the_old_owf_ctl_accepted_still_routes_to_the_same_action() {
        assert_eq!(route(&["ptt-start"]), Route::Send(Request::PttStart));
        assert_eq!(route(&["ptt-stop"]), Route::Send(Request::PttStop));
        assert_eq!(route(&["cancel"]), Route::Send(Request::Cancel));
        assert_eq!(route(&["status"]), Route::Send(Request::Status));
        assert_eq!(route(&["reload"]), Route::Send(Request::Reload));
        assert_eq!(route(&["subscribe"]), Route::Subscribe);
        assert_eq!(route(&["debug"]), Route::Debug);
        assert_eq!(route(&["setup"]), Route::Setup(SetupMode::Plain));
        assert_eq!(route(&["setup", "--update-lock"]), Route::Setup(SetupMode::UpdateLock));
        assert_eq!(route(&["setup", "--print-hypr"]), Route::Setup(SetupMode::PrintHypr));
        assert_eq!(route(&["setup", "--purge-logs"]), Route::Setup(SetupMode::PurgeLogs));
    }

    #[test]
    fn the_two_absorbed_binaries_are_reachable_as_subcommands() {
        assert_eq!(route(&["daemon"]), Route::Daemon);
        assert_eq!(route(&["bench"]), Route::Bench);
    }

    #[test]
    fn the_settings_window_is_reachable_as_a_subcommand() {
        assert_eq!(route(&["settings"]), Route::Settings);
        assert_eq!(route(&["settings", "--now"]), Route::Usage);
    }

    #[test]
    fn no_arguments_is_usage() {
        assert_eq!(route(&[]), Route::Usage);
    }

    #[test]
    fn an_unknown_command_is_usage() {
        assert_eq!(route(&["dictate"]), Route::Usage);
        assert_eq!(route(&["setup", "--wat"]), Route::Usage);
    }

    /// `owf-ctl ptt-start now` is a typo, not a request to start dictating.
    /// Silently ignoring the extra word would hide it.
    #[test]
    fn trailing_arguments_are_rejected_rather_than_ignored() {
        assert_eq!(route(&["ptt-start", "now"]), Route::Usage);
        assert_eq!(route(&["daemon", "--foreground"]), Route::Usage);
    }
}
