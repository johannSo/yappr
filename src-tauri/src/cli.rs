//! Argv → action, as a pure function.
//!
//! These strings are what a user's Hyprland or GNOME shortcut config
//! contains. The compositor runs them; nothing else validates them. So the
//! mapping is a pure function over `argv[1..]` with a test per row, exactly
//! as it was when this lived in `owf-cli` -- the crate moved, the discipline
//! did not.

use yappr_core::proto::Request;

pub const USAGE: &str = "\
usage: yappr                  start the app (tray, no window)
       yappr --toggle         start or stop dictating
       yappr --cancel         discard the current utterance
       yappr --settings       show the settings window
       yappr --quit           shut everything down
       yappr --status|--debug|--subscribe|--reload
       yappr --bench|--print-shortcuts|--purge-logs|--update-lock
       yappr --replay <path>";

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Run,
    Send(Request),
    Subscribe,
    Debug,
    Bench,
    PrintShortcuts,
    PurgeLogs,
    UpdateLock,
    Replay(std::path::PathBuf),
    Usage,
}

pub fn route(args: &[&str]) -> Route {
    match args {
        [] => Route::Run,
        ["--toggle"] => Route::Send(Request::Toggle),
        ["--cancel"] => Route::Send(Request::Cancel),
        ["--settings"] => Route::Send(Request::ShowSettings),
        ["--quit"] => Route::Send(Request::Quit),
        ["--status"] => Route::Send(Request::Status),
        ["--reload"] => Route::Send(Request::Reload),
        ["--subscribe"] => Route::Subscribe,
        ["--debug"] => Route::Debug,
        ["--bench"] => Route::Bench,
        ["--print-shortcuts"] => Route::PrintShortcuts,
        ["--purge-logs"] => Route::PurgeLogs,
        ["--update-lock"] => Route::UpdateLock,
        ["--replay", path] => Route::Replay(path.into()),
        _ => Route::Usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These strings live in the user's shortcut config. A rename that
    /// compiles is still a broken keyboard, so every one of them is pinned.
    #[test]
    fn the_dictation_flags_route_to_their_requests() {
        assert_eq!(route(&["--toggle"]), Route::Send(Request::Toggle));
        assert_eq!(route(&["--cancel"]), Route::Send(Request::Cancel));
    }

    /// Rev 3 dropped hold-to-talk. Two flags for the two edges of a keypress
    /// *is* that model, so their absence is deliberate and pinned: a future
    /// edit that reintroduces them should have to delete this test and say why.
    #[test]
    fn the_hold_to_talk_flags_are_deliberately_absent() {
        assert_eq!(route(&["--start"]), Route::Usage);
        assert_eq!(route(&["--stop"]), Route::Usage);
    }

    #[test]
    fn the_lifecycle_and_inspection_flags_route() {
        assert_eq!(route(&["--settings"]), Route::Send(Request::ShowSettings));
        assert_eq!(route(&["--quit"]), Route::Send(Request::Quit));
        assert_eq!(route(&["--status"]), Route::Send(Request::Status));
        assert_eq!(route(&["--reload"]), Route::Send(Request::Reload));
        assert_eq!(route(&["--subscribe"]), Route::Subscribe);
        assert_eq!(route(&["--debug"]), Route::Debug);
    }

    #[test]
    fn the_local_utilities_route() {
        assert_eq!(route(&["--bench"]), Route::Bench);
        assert_eq!(route(&["--print-shortcuts"]), Route::PrintShortcuts);
        assert_eq!(route(&["--purge-logs"]), Route::PurgeLogs);
        assert_eq!(route(&["--update-lock"]), Route::UpdateLock);
        assert_eq!(
            route(&["--replay", "f.ndjson"]),
            Route::Replay("f.ndjson".into())
        );
    }

    /// No arguments starts the app. This is the launcher's invocation and the
    /// one that must never fall through to usage.
    #[test]
    fn no_arguments_starts_the_app() {
        assert_eq!(route(&[]), Route::Run);
    }

    #[test]
    fn an_unknown_flag_is_usage() {
        assert_eq!(route(&["--dictate"]), Route::Usage);
        assert_eq!(route(&["toggle"]), Route::Usage);
    }

    /// `yappr --toggle now` is a typo, not a request to dictate.
    #[test]
    fn trailing_arguments_are_rejected_rather_than_ignored() {
        assert_eq!(route(&["--toggle", "now"]), Route::Usage);
        assert_eq!(route(&["--replay"]), Route::Usage);
    }
}
