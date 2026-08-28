# One-Process Tray App Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn OpenWhisprFlow from three processes driven by `owf-ctl` and pasted Hyprland config into one binary you launch from the app launcher, which sits in the tray and dictates on a single `--toggle` shortcut.

**Architecture:** The Tauri app (`src-tauri`, package `openwhisprflow`) links `owf-core` and runs the pipeline in-process. `crates/owf-cli` is deleted; its daemon moves to `owf-core/src/server.rs` and its client becomes flags on the app binary, dispatched at the top of `main` before any GTK/WebKit initialisation. The tray is a native StatusNotifierItem (`ksni`), not Tauri's appindicator-backed tray, because only native SNI has a left-click `Activate`.

**Tech Stack:** Rust 2021, Tauri 2.11.5 (GTK3 on Linux), `ksni` for the tray, `gtk-layer-shell` for the overlay surface, React + Vite + `motion/react` for both windows, `sherpa-onnx`/`cpal`/`rubato` via `owf-core`.

**Spec:** `docs/superpowers/specs/2026-08-28-one-process-tray-app-design.md` (rev 3)

## Global Constraints

- **Invariant 1 is absolute.** Once ASR has produced text, the user gets text. No task may add a path that loses a transcribed utterance. `pipeline.rs`'s module doc and spec §15 govern.
- **Invariant 2 holds throughout.** The overlay window must never take keyboard focus. Its mechanism changes (layer-shell `keyboard-interactivity: none` instead of a compositor rule) but it is never unenforced, not even between tasks.
- **Every subprocess call goes through `procutil::run_with_timeout`.** No exceptions added.
- **Config keeps `#[serde(deny_unknown_fields)]`.** Any documented `config.toml` key must have a matching struct field in the same commit, or the daemon refuses to boot.
- **`cargo test --workspace && cargo clippy --workspace --all-targets` is the gate.** It must pass at the end of every task. There is no CI; this is the only gate that exists.
- **The workspace must build green after every task.** No task may leave `owf-cli` half-deleted or a module referenced but absent.
- **Tests are named as full sentences.** Documented-but-unfixed behaviour is pinned with a `known_limitation_` prefix.
- **Never apply Hyprland config on the user's behalf.** Emit it; the user pastes it.
- **Do not record from the microphone without explicit permission.** Every test in this plan is offline and silent.
- **German is the settings UI language.** New user-visible strings match the existing tone in `src/settings/schema.ts`.
- **`rejections.jsonl` is live data.** Any test touching the guardrail redirects writes with `with_rejections_path`.

---

## File Structure

**Created:**
- `crates/owf-core/src/server.rs` — the daemon, moved verbatim from `crates/owf-cli/src/daemon.rs`
- `src-tauri/src/cli.rs` — `route()`, the pure argv → action function, plus its tests
- `src-tauri/src/client.rs` — the socket client behind every *client* flag
- `src-tauri/src/bench.rs` — moved from `crates/owf-cli/src/bench.rs`
- `src-tauri/src/setup.rs` — moved from the local-utility half of `crates/owf-cli/src/ctl.rs`
- `src-tauri/src/tray.rs` — the `ksni` StatusNotifierItem
- `src-tauri/src/layer.rs` — `gtk-layer-shell` application to the overlay window
- `src-tauri/src/settings_cmds.rs` — `get_config` / `set_config` / `list_input_devices` as in-process Tauri commands
- `docs/superpowers/plans/2026-08-28-one-process-tray-app.md` — this file

**Modified:**
- `Cargo.toml` — workspace members lose `crates/owf-cli` and `settings-tauri`
- `src-tauri/Cargo.toml` — gains `owf-core`, `ksni`, `gtk-layer-shell`, `tauri` unchanged
- `src-tauri/src/lib.rs` — hosts the server; two windows; tray
- `src-tauri/src/main.rs` — argv dispatch before `openwhisprflow_lib::run()`
- `src-tauri/src/replay.rs` — uses `owf_core::proto::OverlayEvent`
- `src-tauri/tauri.conf.json` — adds the settings window
- `crates/owf-core/src/lib.rs` — declares `server`
- `crates/owf-core/src/proto.rs` — `Toggle`, `Quit`, `ShowSettings`, `SetPaused`
- `crates/owf-core/src/config_write.rs` — the "hold-to-talk cap" annotation
- `crates/owf-core/src/hypr.rs` — emits the new shortcut block
- `CLAUDE.md`, `README.md`, `HANDOVER.md`

**Deleted:**
- `crates/owf-cli/` (whole crate)
- `settings-tauri/` (whole crate)
- `src-tauri/src/wire.rs` — invariant 3's third copy
- `src-tauri/src/connection.rs` — the overlay's socket client

---

## Phase 0 — Probes

Spec §12 lists three claims this design rests on that have never been tested here. They come first because a failure changes the design, not the schedule.

### Task 1: Verify the three platform claims

**Files:**
- Create: `/tmp/owf-probe/` (throwaway, not committed)
- Modify: `docs/superpowers/specs/2026-08-28-one-process-tray-app-design.md` (§12 results)

**Interfaces:**
- Consumes: nothing
- Produces: a decision for Task 12 (tray: `Activate` or menu-only) and Task 14 (overlay: layer-shell or toplevel+rule)

- [ ] **Step 1: Probe SNI `Activate` with a minimal ksni item**

```bash
mkdir -p /tmp/owf-probe && cd /tmp/owf-probe
cargo init --name owf_probe 2>/dev/null
cargo add ksni@0.2
```

```rust
// /tmp/owf-probe/src/main.rs
use ksni::{Tray, TrayService};

struct Probe;

impl Tray for Probe {
    fn icon_name(&self) -> String { "audio-input-microphone".into() }
    fn title(&self) -> String { "owf probe".into() }
    // The whole point of the probe: does a left click reach this?
    fn activate(&mut self, _x: i32, _y: i32) {
        println!("ACTIVATE received");
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![ksni::menu::StandardItem {
            label: "Menu item".into(),
            activate: Box::new(|_| println!("MENU item activated")),
            ..Default::default()
        }.into()]
    }
}

fn main() {
    TrayService::new(Probe).spawn();
    std::thread::park();
}
```

- [ ] **Step 2: Run it and left-click the icon**

Run: `cd /tmp/owf-probe && cargo run`
Then left-click the new tray icon in the quickshell bar.
Expected: `ACTIVATE received` on stdout.
Record the actual result. If nothing prints, the spec's §12 fallback applies: Task 12 makes Einstellungen the first menu item instead.

- [ ] **Step 3: Probe gtk-layer-shell against a Tauri window**

Add to `src-tauri/Cargo.toml` under `[target.'cfg(target_os = "linux")'.dependencies]`:

```toml
gtk = "0.18"
gtk-layer-shell = "0.8"
```

Temporarily, inside `src-tauri/src/lib.rs`'s existing `.setup(...)` closure, before anything else touches the window:

```rust
#[cfg(target_os = "linux")]
{
    use gtk_layer_shell::{Edge, Layer, LayerShell};
    let gtk_win = window.gtk_window()?;
    gtk_win.init_layer_shell();
    gtk_win.set_layer(Layer::Overlay);
    gtk_win.set_keyboard_mode(gtk_layer_shell::KeyboardMode::None);
    gtk_win.set_anchor(Edge::Bottom, true);
    gtk_win.set_margin(Edge::Bottom, 40);
    eprintln!("layer-shell initialised");
}
```

- [ ] **Step 4: Run the overlay and confirm placement**

Run: `cargo run -p openwhisprflow --features custom-protocol -- --replay src-tauri/fixtures/replay-full.ndjson`
Expected: the pill appears bottom-centre with no Hyprland window rule present, and `hyprctl clients -j` shows it. If `init_layer_shell` panics or the window does not map, record that: Task 14 takes the toplevel + title-matched-rule fallback.

- [ ] **Step 5: Record class and titles**

Run: `hyprctl clients -j | jq -r '.[] | select(.class|test("openwhisprflow")) | {class, title}'`
Record the exact strings. Task 14's fallback needs them.

- [ ] **Step 6: Revert the probe edits and record findings in the spec**

```bash
cd /home/joni/Work/JS_TS/OpenWhisprFlow
git checkout src-tauri/Cargo.toml src-tauri/src/lib.rs
rm -rf /tmp/owf-probe
```

Append a short "Probe results, <date>" subsection to spec §12 stating, for each of the three claims, what actually happened. Then:

```bash
git add docs/superpowers/specs/2026-08-28-one-process-tray-app-design.md
git commit -m "docs: record the three platform probe results in the spec"
```

---

## Phase 1 — Move the server into owf-core

### Task 2: Move the daemon to `owf-core/src/server.rs`

This is a relocation. A relocation that changes assertions is not a relocation: the test bodies must be byte-identical afterwards.

**Files:**
- Create: `crates/owf-core/src/server.rs` (moved)
- Modify: `crates/owf-core/src/lib.rs`, `crates/owf-core/Cargo.toml`, `crates/owf-cli/src/lib.rs`, `crates/owf-cli/src/daemon.rs` (deleted)

**Interfaces:**
- Consumes: nothing
- Produces: `owf_core::server::run() -> anyhow::Result<()>`, and the crate-visible items Task 6 needs: `owf_core::server::Daemon`, `server::serve(listener: UnixListener, daemon: Arc<Daemon>)`

- [ ] **Step 1: Move the file and declare the module**

```bash
git mv crates/owf-cli/src/daemon.rs crates/owf-core/src/server.rs
```

In `crates/owf-core/src/lib.rs`, add alongside the existing module declarations:

```rust
pub mod server;
```

- [ ] **Step 2: Fix the imports the move breaks**

In `server.rs`, every `use owf_core::x` becomes `use crate::x`. There are no other cross-crate references — `daemon.rs` only ever used `owf_core` and `std`.

- [ ] **Step 3: Point `owf-cli` at the moved module**

In `crates/owf-cli/src/lib.rs`, delete `pub mod daemon;` and change the dispatch arm:

```rust
Route::Daemon => owf_core::server::run(),
```

- [ ] **Step 4: Move the daemon's dev-dependencies**

`daemon.rs`'s tests use `tempfile`, and its llama tests use the `test-util` feature's `LlamaServer::from_child`. `test-util` is already declared in `crates/owf-core/Cargo.toml` — that is the crate the feature belongs to — so only `tempfile` moves. Delete it from `crates/owf-cli/Cargo.toml`'s `[dev-dependencies]` and add to `crates/owf-core/Cargo.toml`:

```toml
[dev-dependencies]
tempfile = "3"
```

The tests that need the stubbed child are already `#[cfg(feature = "test-util")]`-gated and now see it from inside their own crate, so run them with `cargo test -p owf-core --features test-util`.

- [ ] **Step 5: Run the full suite**

Run: `cargo test --workspace`
Expected: PASS, with the same test count as before the move (306). A lower count means tests were lost in the move, which is a failure of this task, not a detail.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: move the daemon into owf-core as server.rs

A relocation, not a rewrite: every test body is unchanged and the
count is identical. owf-cli's daemon subcommand now calls
owf_core::server::run()."
```

---

## Phase 2 — The app becomes the server

### Task 3: `src-tauri` links `owf-core`; delete `wire.rs`

**Files:**
- Modify: `src-tauri/Cargo.toml`, `src-tauri/src/lib.rs`, `src-tauri/src/replay.rs`
- Delete: `src-tauri/src/wire.rs`

**Interfaces:**
- Consumes: `owf_core::server` from Task 2
- Produces: `owf_core::proto::OverlayEvent` as the app's only event type

- [ ] **Step 1: Add the dependency**

In `src-tauri/Cargo.toml`:

```toml
[dependencies]
owf-core = { path = "../crates/owf-core" }
```

- [ ] **Step 2: Delete the duplicate and repoint its consumers**

```bash
git rm src-tauri/src/wire.rs
```

In `src-tauri/src/lib.rs` and `src-tauri/src/replay.rs`, replace `use crate::wire::{OverlayEvent, SUBSCRIBE_LINE}` with `use owf_core::proto::OverlayEvent;` and delete the `mod wire;` declaration.

- [ ] **Step 3: Update the drift test that no longer has two types to compare**

`the_overlay_replay_fixture_parses_as_this_crates_overlay_event` in `proto.rs` becomes trivially true but is kept — it still proves the checked-in fixture parses. `checked_in_fixture_covers_every_event_kind` in `replay.rs` is unchanged and still guards the remaining Rust/TypeScript pair. Add to `replay.rs`:

```rust
/// Invariant 3 used to say `OverlayEvent` exists in three hand-maintained
/// copies. `wire.rs` is gone, so it exists in two: this crate now shares
/// `owf-core`'s type outright, and only the TypeScript union in
/// `src/Overlay.tsx` is still maintained by hand.
#[test]
fn this_crate_shares_owf_cores_overlay_event_rather_than_copying_it() {
    let e: owf_core::proto::OverlayEvent = owf_core::proto::OverlayEvent::Idle;
    assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"event":"idle"}"#);
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p openwhisprflow`
Expected: PASS, including `checked_in_fixture_covers_every_event_kind`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor: drop wire.rs; the app shares owf-core's OverlayEvent

Invariant 3 goes from three hand-maintained copies to two."
```

### Task 4: `route()` moves into the app with the new flag surface

**Files:**
- Create: `src-tauri/src/cli.rs`
- Modify: `src-tauri/src/lib.rs` (`mod cli;`)

**Interfaces:**
- Consumes: `owf_core::proto::Request`
- Produces: `cli::Route`, `cli::route(&[&str]) -> Route`, `cli::USAGE`

- [ ] **Step 1: Write the failing tests**

```rust
// src-tauri/src/cli.rs
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

    /// `openwhisprflow --toggle now` is a typo, not a request to dictate.
    #[test]
    fn trailing_arguments_are_rejected_rather_than_ignored() {
        assert_eq!(route(&["--toggle", "now"]), Route::Usage);
        assert_eq!(route(&["--replay"]), Route::Usage);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p openwhisprflow cli::`
Expected: FAIL — `cannot find function route` / `Route not found`.

- [ ] **Step 3: Write the implementation**

```rust
//! Argv → action, as a pure function.
//!
//! These strings are what a user's Hyprland or GNOME shortcut config
//! contains. The compositor runs them; nothing else validates them. So the
//! mapping is a pure function over `argv[1..]` with a test per row, exactly
//! as it was when this lived in `owf-cli` — the crate moved, the discipline
//! did not.

use owf_core::proto::Request;

pub const USAGE: &str = "\
usage: openwhisprflow                     start the app (tray, no window)
       openwhisprflow --toggle            start or stop dictating
       openwhisprflow --cancel            discard the current utterance
       openwhisprflow --settings          show the settings window
       openwhisprflow --quit              shut everything down
       openwhisprflow --status|--debug|--subscribe|--reload
       openwhisprflow --bench|--print-shortcuts|--purge-logs|--update-lock
       openwhisprflow --replay <path>";

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
```

Add `Toggle`, `Quit` and `ShowSettings` to `owf_core::proto::Request` (Task 7 implements their handlers; the variants are needed to compile here):

```rust
    /// Press-to-start / press-to-stop. Resolved against the server's current
    /// state, never against a client-side memory of the last press — the
    /// client is a fresh process every time and has no memory to consult.
    Toggle,
    /// Shut the whole app down. The tray's Beenden sends the same request.
    Quit,
    /// Show and focus the settings window.
    ShowSettings,
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p openwhisprflow cli::`
Expected: PASS, all eight tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: route() moves to the app with the --toggle flag surface

Pins the absence of --start/--stop as deliberately as it pins the
presence of --toggle."
```

### Task 5: The client fast path

**Files:**
- Create: `src-tauri/src/client.rs`
- Create: `src-tauri/src/bench.rs`, `src-tauri/src/setup.rs`, `src-tauri/src/client_stream.rs` (moved from `owf-cli`)
- Modify: `src-tauri/src/main.rs`

**Interfaces:**
- Consumes: `cli::route`, `owf_core::proto::send`
- Produces: `client::dispatch(Route) -> anyhow::Result<bool>` — `Ok(true)` means "handled, exit now"; `Ok(false)` means "start the app"

- [ ] **Step 1: Move the modules this task's dispatch calls**

`client::dispatch` below calls `bench::run`, `setup::debug_summary`, `setup::purge_logs`, `setup::setup` and `client_stream::subscribe`. They must exist before it compiles, so they move now rather than in Task 11:

```bash
git mv crates/owf-cli/src/bench.rs src-tauri/src/bench.rs
git mv crates/owf-cli/src/ctl.rs src-tauri/src/setup.rs
```

Split `setup.rs`: move `subscribe()` into a new `src-tauri/src/client_stream.rs`, and delete `send()` and `open_settings()` — `client::dispatch` replaces both. `debug_summary`, `setup`, `purge_logs` and their helpers (`check_prerequisites`, `ggml_backend_present`, `parse_ldconfig_dirs`, `purge_logs_at`) stay in `setup.rs` **with their tests**, which include `purge_logs_at_removes_an_existing_file` and `purge_logs_at_treats_an_already_absent_file_as_success`.

`crates/owf-cli/src/lib.rs` still references the two moved modules. Delete `pub mod bench;` and `pub mod ctl;` and the arms of its `dispatch` that call them; the crate keeps only `route()` and the `Daemon` arm until Task 11 deletes it entirely.

Declare all four in `src-tauri/src/lib.rs`:

```rust
mod bench;
mod cli;
mod client;
mod client_stream;
mod setup;
```

- [ ] **Step 2: Write the failing test**

```rust
// src-tauri/src/client.rs
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
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p openwhisprflow client::`
Expected: FAIL — `cannot find function handled_without_starting_the_app`.

- [ ] **Step 4: Write the implementation**

```rust
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
        // Renamed to `shortcut_config()` in Task 17, when its content changes.
        Route::PrintShortcuts => print!("{}", owf_core::hypr::hypr_config()),
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
```

In `src-tauri/src/main.rs`, above everything:

```rust
fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    let route = openwhisprflow_lib::cli::route(&args);
    match openwhisprflow_lib::client::dispatch(route) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            eprintln!("openwhisprflow: {e:#}");
            openwhisprflow_lib::client::notify_failure(&e);
            std::process::exit(1);
        }
    }
    openwhisprflow_lib::run();
}
```

- [ ] **Step 5: Add the visible-failure path spec §2 requires**

```rust
/// Spec §2: a user whose only interface is a shortcut needs a failure to be
/// visible without a terminal. The shortcut fires, the app is not running,
/// and stderr goes nowhere anyone will read.
pub fn notify_failure(e: &anyhow::Error) {
    let body = format!("{e:#}");
    let _ = owf_core::procutil::run_with_timeout(
        std::process::Command::new("notify-send")
            .arg("OpenWhisprFlow")
            .arg(&body),
        std::time::Duration::from_secs(3),
    );
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p openwhisprflow client::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: dispatch client flags above tauri::Builder

A structural test pins that only --replay and a bare invocation ever
reach the window layer, so a later edit cannot quietly make every
shortcut press pay GTK startup."
```

### Task 6: The app hosts the server

**Files:**
- Modify: `src-tauri/src/lib.rs`, `crates/owf-core/src/server.rs`
- Delete: `src-tauri/src/connection.rs`

**Interfaces:**
- Consumes: `owf_core::server::{Daemon, serve, warm_up}`
- Produces: an app whose `setup()` owns the socket, and a `Daemon` that emits to the frontend

- [ ] **Step 1: Split `server::run()` so the app can host it**

`run()` currently acquires the lock, binds the socket, warms up and loops. Extract the loop so the app supplies its own broadcast sink:

```rust
/// A sink for events the server produces. The standalone daemon had exactly
/// one consumer (socket subscribers); the app has two, because the overlay is
/// now in-process and gets its events through Tauri rather than a socket.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: &OverlayEvent);
}

/// Everything `run()` did except the accept loop, so a host that owns its own
/// event loop (the Tauri app) can call this and then serve on a thread.
pub fn start(sink: Arc<dyn EventSink>) -> Result<(Arc<Daemon>, UnixListener)> { /* body moved from run() */ }

pub fn serve(daemon: Arc<Daemon>, listener: UnixListener) { /* the accept loop moved from run() */ }

pub fn run() -> Result<()> {
    struct NoExtraSink;
    impl EventSink for NoExtraSink {
        fn emit(&self, _: &OverlayEvent) {}
    }
    let (daemon, listener) = start(Arc::new(NoExtraSink))?;
    serve(daemon, listener);
    Ok(())
}
```

`Daemon` gains a field:

```rust
    /// The in-process consumer of every broadcast. The standalone daemon had
    /// none; the app's overlay is no longer a socket client, so it is one.
    sink: Arc<dyn EventSink>,
```

`Daemon::broadcast` gains one line: after `broadcast_to(&self.subscribers, event.clone())`, call `self.sink.emit(&event)`.

Adding a field breaks every `Daemon { .. }` literal. There are exactly two today — the real one in `start()` (formerly `run()`, around line 431 of the old `daemon.rs`) and `fake_daemon_at` in the test module (around line 2225). Update both, and give the test module a sink-aware constructor beside the existing ones:

```rust
    /// `fake_daemon_at` and `fake_daemon_with_normalize` keep their
    /// signatures and pass a sink that drops events, so no existing test
    /// changes. Only the test asserting on the second consumer needs this.
    fn fake_daemon_with_sink(initial_state: u8, sink: Arc<dyn EventSink>) -> Arc<Daemon> {
        // identical to fake_daemon_at, with `sink` instead of the drop sink
    }
```

- [ ] **Step 2: Write the failing test for the second sink**

```rust
// in crates/owf-core/src/server.rs tests
/// The overlay stopped being a socket client, so `broadcast` has two
/// consumers now. A future edit that returns early for one of them (a
/// no-subscribers fast path, say) would silently blind the overlay while
/// every socket test still passed.
#[test]
fn broadcast_reaches_the_in_process_sink_even_with_no_socket_subscribers() {
    #[derive(Default)]
    struct Recorder(Mutex<Vec<OverlayEvent>>);
    impl EventSink for Recorder {
        fn emit(&self, e: &OverlayEvent) {
            self.0.lock().unwrap().push(e.clone());
        }
    }
    let sink = Arc::new(Recorder::default());
    let daemon = fake_daemon_with_sink(IDLE, sink.clone());
    assert!(lock_ignoring_poison(&daemon.subscribers).is_empty());

    daemon.broadcast(OverlayEvent::Transcribing);

    assert_eq!(sink.0.lock().unwrap().as_slice(), &[OverlayEvent::Transcribing]);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p owf-core server::tests::broadcast_reaches_the_in_process_sink`
Expected: FAIL — `EventSink` not found.

- [ ] **Step 4: Implement, then wire the app's sink**

In `src-tauri/src/lib.rs`'s `setup()`:

```rust
struct TauriSink(tauri::AppHandle);

impl owf_core::server::EventSink for TauriSink {
    fn emit(&self, event: &owf_core::proto::OverlayEvent) {
        if let Err(e) = self.0.emit("overlay-event", event) {
            eprintln!("overlay: failed to emit event to the frontend: {e}");
        }
    }
}
```

```rust
// inside .setup(), replacing the connection::run thread
let (daemon, listener) = owf_core::server::start(Arc::new(TauriSink(app.handle().clone())))?;
app.manage(daemon.clone());
// Never on the Tauri event-loop thread — the accept loop blocks, and a
// blocked event loop is a frozen window and an unclickable tray.
std::thread::spawn(move || owf_core::server::serve(daemon, listener));
```

```bash
git rm src-tauri/src/connection.rs
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Verify the overlay still renders every state without a microphone**

Run: `cargo run -p openwhisprflow --features custom-protocol -- --replay src-tauri/fixtures/replay-full.ndjson`
Expected: the pill cycles through every state exactly as before. This is the acceptance evidence the project already uses; it must not regress.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: the app hosts the server; the overlay stops being a socket client

broadcast gains a second consumer, and a test pins that an empty
subscriber list does not blind it."
```

### Task 7: `Request::Toggle`

**Files:**
- Modify: `crates/owf-core/src/server.rs`, `crates/owf-core/src/proto.rs`

**Interfaces:**
- Consumes: `Request::Toggle` from Task 4
- Produces: the state table spec §2 defines

- [ ] **Step 1: Write the failing tests, one per row of the spec's table**

```rust
/// Spec §2's toggle table. `--toggle` is resolved against the server's state,
/// not against a client-side memory of the last press — the client is a fresh
/// process each time and has none.
#[test]
fn toggle_starts_recording_when_idle() {
    let d = fake_daemon_with_normalize(IDLE, false);
    let r = dispatch(&d, Request::Toggle);
    assert!(r.ok);
    assert_eq!(d.state.load(Ordering::SeqCst), RECORDING);
}

#[test]
fn toggle_stops_and_transcribes_when_recording() {
    let d = fake_daemon_with_normalize(RECORDING, false);
    let r = dispatch(&d, Request::Toggle);
    assert!(r.ok);
    assert_eq!(r.state, Some(State::Transcribing));
}

/// The row that can lose an utterance if it is wrong. A press while the
/// previous utterance is still being typed must not open the microphone
/// behind it — invariant 1 protects the text already produced, and nothing
/// protects it from a second recording started on top.
#[test]
fn toggle_refuses_while_an_utterance_is_still_being_processed() {
    for busy in [TRANSCRIBING, NORMALIZING, INJECTING] {
        let d = fake_daemon_with_normalize(busy, false);
        let r = dispatch(&d, Request::Toggle);
        assert!(!r.ok, "toggle must be refused in state {busy}");
        assert_eq!(d.state.load(Ordering::SeqCst), busy, "and must not change it");
    }
}

#[test]
fn toggle_refuses_while_warming_or_failed_with_a_stated_reason() {
    let d = fake_daemon_with_normalize(WARMING, false);
    assert_eq!(dispatch(&d, Request::Toggle).err.as_deref(), Some("warming"));

    let d = fake_daemon_with_normalize(FAILED, false);
    assert!(dispatch(&d, Request::Toggle).err.is_some());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p owf-core server::tests::toggle_`
Expected: FAIL — non-exhaustive match on `Request`.

- [ ] **Step 3: Implement**

In `dispatch`, one arm that delegates rather than duplicating:

```rust
// Spec §2: resolved against the current state. Deliberately expressed by
// delegating to the two existing arms rather than reimplementing them --
// PttStart and PttStop keep every guarantee they already have (the busy
// flash, the idempotent restart, `claim_busy`'s CAS against the safety
// valve), and toggle inherits all of it by construction.
Request::Toggle => match current {
    RECORDING => dispatch(daemon, Request::PttStop),
    _ => dispatch(daemon, Request::PttStart),
},
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p owf-core server::tests::toggle_`
Expected: PASS, all four.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: Request::Toggle, resolved against server state

Delegates to the PttStart/PttStop arms rather than reimplementing them,
so the busy flash and claim_busy's CAS against the safety valve are
inherited rather than re-derived."
```

### Task 8: Pin the safety valve as the sole terminator

**Files:**
- Modify: `crates/owf-core/src/server.rs` (test only), `crates/owf-core/src/config_write.rs`

**Interfaces:**
- Consumes: the existing watchdog at the former `daemon.rs:1475`
- Produces: nothing new — this task pins behaviour that already exists

- [ ] **Step 1: Write the failing test**

```rust
/// Hold-to-talk had a physical guarantee that recording ends: you let go.
/// Toggle has none. This watchdog is now the only thing that closes a
/// microphone whose second press never came, and it must *transcribe* what it
/// captured rather than discard it (invariant 1).
#[test]
fn a_recording_with_no_second_press_is_ended_and_transcribed_by_the_safety_valve() {
    let d = fake_daemon_with_normalize(IDLE, false);
    // The valve reads `audio_cfg`, so shorten it rather than sleeping 120 s.
    lock_ignoring_poison(&d.audio_cfg).max_seconds = 1;
    dispatch(&d, Request::Toggle);
    assert_eq!(d.state.load(Ordering::SeqCst), RECORDING);

    std::thread::sleep(Duration::from_millis(1_500));

    assert_ne!(
        d.state.load(Ordering::SeqCst),
        RECORDING,
        "the microphone is still open after max_seconds"
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p owf-core server::tests::a_recording_with_no_second_press`
Expected: PASS if the watchdog is intact — this test documents rather than drives. If it FAILS, the watchdog was broken by Tasks 2–7 and that is the bug to fix before continuing.

- [ ] **Step 3: Fix the now-wrong annotation**

In `crates/owf-core/src/config_write.rs`, the annotated fixture and the two tests asserting on it:

```rust
max_seconds = 120   # Sicherheitsnetz: beendet eine vergessene Aufnahme
```

Update both `assert!(out.contains(...))` strings to match.

- [ ] **Step 4: Run the config_write tests**

Run: `cargo test -p owf-core config_write::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test: pin the safety valve as toggle's sole terminator

Its comment called it a guard against a stuck key. Under press/press it
is the only thing that closes a forgotten microphone, so the config
annotation says so and a test holds it."
```

### Task 9: The settings window moves in-process

**Files:**
- Create: `src-tauri/src/settings_cmds.rs`
- Modify: `src-tauri/tauri.conf.json`, `src-tauri/src/lib.rs`
- Delete: `settings-tauri/`

**Interfaces:**
- Consumes: `owf_core::server::Daemon` from Task 6
- Produces: Tauri commands `get_config`, `set_config`, `list_input_devices` — **these three names are the frontend's contract** (`src/Settings.tsx:76,96,146`) and must not change

- [ ] **Step 1: Add the second window**

In `src-tauri/tauri.conf.json`'s `app.windows`, alongside `overlay`:

```json
{
  "label": "settings",
  "title": "OpenWhisprFlow – Einstellungen",
  "url": "settings.html",
  "width": 980,
  "height": 720,
  "minWidth": 720,
  "minHeight": 520,
  "resizable": true,
  "decorations": true,
  "visible": false,
  "focus": true,
  "focusable": true
}
```

- [ ] **Step 2: Write the commands with the same names and payloads**

```rust
//! The settings window's three commands.
//!
//! They were socket calls in `settings-tauri`; they are direct calls now. The
//! command names and the JSON they return are byte-identical on purpose:
//! `src/settings/` and `Settings.tsx` are unchanged by this move, and CLAUDE.md
//! invariant 9's autosave contract — validate before writing, atomic rename,
//! a rejected save keeps the value on screen — is `config_write`'s, not the
//! transport's.

use std::sync::Arc;
use owf_core::proto::Request;
use owf_core::server::{dispatch, Daemon};

#[tauri::command]
pub fn get_config(daemon: tauri::State<'_, Arc<Daemon>>) -> Result<serde_json::Value, String> {
    to_json(dispatch(&daemon, Request::GetConfig))
}

#[tauri::command]
pub fn set_config(
    daemon: tauri::State<'_, Arc<Daemon>>,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    to_json(dispatch(&daemon, Request::SetConfig { config }))
}

#[tauri::command]
pub fn list_input_devices(
    daemon: tauri::State<'_, Arc<Daemon>>,
) -> Result<serde_json::Value, String> {
    to_json(dispatch(&daemon, Request::ListInputDevices))
}

/// A `{"ok": false}` becomes an `Err`, exactly as the socket client did, so
/// the frontend's existing rejection handling keeps working unchanged.
fn to_json(resp: owf_core::proto::Response) -> Result<serde_json::Value, String> {
    let v = serde_json::to_value(&resp).map_err(|e| e.to_string())?;
    if resp.ok {
        Ok(v)
    } else {
        Err(resp.err.unwrap_or_else(|| "Unbekannter Fehler".into()))
    }
}
```

`server::dispatch` must become `pub` for this.

- [ ] **Step 3: Register them and delete the old crate**

```rust
.invoke_handler(tauri::generate_handler![
    position_overlay,
    settings_cmds::get_config,
    settings_cmds::set_config,
    settings_cmds::list_input_devices
])
```

```bash
git rm -r settings-tauri
```

Remove `"settings-tauri"` from `Cargo.toml`'s workspace members.

- [ ] **Step 4: Verify the frontend is genuinely untouched**

Run: `git status --short src/`
Expected: no modifications under `src/`. If `Settings.tsx` needed editing, the command names or payloads drifted — fix the Rust side instead, per this task's Interfaces block.

- [ ] **Step 5: Build and open the window**

Run: `bun run build && cargo build -p openwhisprflow --features custom-protocol`
Then: `cargo run -p openwhisprflow --features custom-protocol` and in another terminal `openwhisprflow --settings` once Task 10 lands; until then verify by temporarily setting the settings window's `visible: true`.
Expected: the form renders, a toggle autosaves, `config.toml` gains a one-line diff.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: the settings window becomes a window of the app

Same three command names, same JSON, so src/settings/ and Settings.tsx
are untouched. settings-tauri is deleted."
```

### Task 10: `--quit`, `--settings`, and total shutdown

**Files:**
- Modify: `crates/owf-core/src/server.rs`, `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: the existing `server::shutdown(&Daemon)`
- Produces: `Request::Quit` and `Request::ShowSettings` handlers

- [ ] **Step 1: Write the failing test**

```rust
/// Spec §8: Beenden must leave nothing behind. The llama-server child is the
/// one that leaks today when the process is killed rather than dropped.
#[test]
fn quit_reaps_the_llama_child_and_removes_the_runtime_files() {
    let dir = tempfile::tempdir().unwrap();
    let d = fake_daemon_at(IDLE, false, dir.path().join("config.toml"));
    let child = std::process::Command::new("sleep").arg("300").spawn().unwrap();
    let pid = child.id();
    *lock_ignoring_poison(&d.llama) = Some(LlamaServer::from_child(child, 0));

    shutdown(&d);

    assert!(!process_is_alive(pid), "llama-server survived shutdown");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p owf-core --features test-util server::tests::quit_reaps_the_llama_child`
Expected: FAIL initially if `process_is_alive` does not exist; add it as a test helper reading `/proc/<pid>`.

- [ ] **Step 3: Implement the two handlers**

```rust
Request::Quit => {
    // Spec §8 step 1: an utterance already in flight finishes. `shutdown`
    // stops housekeeping and kills llama, neither of which the in-flight
    // pipeline needs -- it has its own handles -- so this does not violate
    // invariant 1.
    let d = Arc::clone(daemon);
    std::thread::spawn(move || {
        shutdown(&d);
        d.sink.emit(&OverlayEvent::Idle);
        std::process::exit(0);
    });
    Response::ok(state_of(current))
}
Request::ShowSettings => {
    daemon.sink.show_settings();
    Response::ok(state_of(current))
}
```

`EventSink` gains `fn show_settings(&self) {}` with an empty default, so the standalone `run()` sink is unaffected. `TauriSink` implements it:

```rust
fn show_settings(&self) {
    if let Some(w) = self.0.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}
```

- [ ] **Step 4: Make closing the settings window hide it**

Spec §8: closing settings hides it; it does not exit the app.

```rust
if let Some(settings) = app.get_webview_window("settings") {
    let w = settings.clone();
    settings.on_window_event(move |e| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = e {
            api.prevent_close();
            let _ = w.hide();
        }
    });
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --features owf-core/test-util`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: --quit tears everything down; --settings shows the window

Closing the settings window hides it rather than exiting, which is what
'it all shuts off when I click Beenden' implies about every other close
affordance."
```

### Task 11: Delete `crates/owf-cli`

**Files:**
- Delete: `crates/owf-cli/`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: everything from Tasks 2–10; Task 5 already moved `bench.rs`, `setup.rs` and `client_stream.rs`
- Produces: a workspace of two members

- [ ] **Step 1: Confirm nothing is left that anyone needs**

Run: `ls crates/owf-cli/src/`
Expected: only `lib.rs`, whose `route()` was superseded by `src-tauri/src/cli.rs` in Task 4. `daemon.rs` left in Task 2; `bench.rs` and `ctl.rs` in Task 5. If any other file is present it was missed by an earlier task — move it before deleting.

- [ ] **Step 2: Delete the crate and update the workspace**

```bash
git rm -r crates/owf-cli
```

In `Cargo.toml`:

```toml
members = ["crates/owf-core", "src-tauri"]
```

- [ ] **Step 3: Run the full suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, no warnings. Compare the test count against Task 2's: the only tests that may have disappeared are `owf-cli`'s `route()` tests, which Task 4 replaced with a larger set, and `ctl::send`'s.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: delete crates/owf-cli

Two workspace members left. Everything the tool can do is now an
argument to openwhisprflow."
```

---

## Phase 3 — Presence

### Task 12: The tray

**Files:**
- Create: `src-tauri/src/tray.rs`
- Modify: `src-tauri/Cargo.toml`, `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's probe result; `owf_core::proto::{OverlayEvent, State}`
- Produces: `tray::spawn(handle: tauri::AppHandle) -> tray::Handle` with `Handle::set_state(State)`

- [ ] **Step 1: Add the dependency**

```toml
ksni = "0.2"
```

- [ ] **Step 2: Write the failing test for icon selection**

```rust
/// The icon is the only indicator that the microphone is open, now that the
/// user's own finger is not. Every state must map to a distinguishable icon,
/// and recording must never look like idle.
#[test]
fn every_state_maps_to_an_icon_and_recording_is_never_idles() {
    use owf_core::proto::State::*;
    let all = [Warming, Idle, Recording, Transcribing, Normalizing, Injecting, Error];
    let names: Vec<_> = all.iter().map(|s| icon_name(*s)).collect();
    assert!(names.iter().all(|n| !n.is_empty()));
    assert_ne!(icon_name(Recording), icon_name(Idle));
    assert_ne!(icon_name(Recording), icon_name(Transcribing));
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p openwhisprflow tray::`
Expected: FAIL — `cannot find function icon_name`.

- [ ] **Step 4: Implement**

```rust
//! The tray icon, as a native StatusNotifierItem.
//!
//! Not Tauri's `tray-icon` feature: that is backed by libayatana-appindicator
//! on Linux, which exposes no activation event — Tauri documents click events
//! as unsupported there. Verified on this machine: Handy's tray item sits at
//! `/org/ayatana/NotificationItem/...` (menu-only), Claude Desktop's at
//! `/StatusNotifierItem` (native SNI, with `Activate`). "Left-click opens the
//! settings" needs the second kind.

pub fn icon_name(state: owf_core::proto::State) -> &'static str {
    use owf_core::proto::State::*;
    match state {
        Warming => "content-loading-symbolic",
        Idle => "audio-input-microphone-symbolic",
        Recording => "media-record-symbolic",
        Transcribing | Normalizing | Injecting => "content-loading-symbolic",
        Error => "dialog-error-symbolic",
    }
}
```

The `ksni::Tray` impl routes `activate` to `Request::ShowSettings` and builds the menu: Status (disabled), Einstellungen, Diktat pausieren (checkable, Task 13), Beenden → `Request::Quit`.

If Task 1 found no `Activate`, make Einstellungen the first menu item and note it in `tray.rs`'s module doc.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p openwhisprflow tray::`
Expected: PASS.

- [ ] **Step 6: Verify by hand**

Run: `cargo run -p openwhisprflow --features custom-protocol`
Expected: an icon appears in the quickshell bar; left click opens settings; right click shows four items; Beenden exits and `pgrep openwhisprflow` returns nothing.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: native SNI tray with left-click to settings"
```

### Task 13: Pause

**Files:**
- Modify: `crates/owf-core/src/server.rs`, `src-tauri/src/tray.rs`

**Interfaces:**
- Consumes: `Request::SetPaused { paused: bool }`
- Produces: `PAUSED: u8 = 7` in the state machine

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn toggle_is_refused_while_paused_and_opens_no_microphone() {
    let d = fake_daemon_with_normalize(IDLE, false);
    dispatch(&d, Request::SetPaused { paused: true });
    assert_eq!(d.state.load(Ordering::SeqCst), PAUSED);

    let r = dispatch(&d, Request::Toggle);
    assert!(!r.ok);
    assert_eq!(d.state.load(Ordering::SeqCst), PAUSED);
}

/// Invariant 1: pausing is not a way to lose an utterance already in flight.
#[test]
fn pausing_never_interrupts_an_utterance_already_in_flight() {
    let d = fake_daemon_with_normalize(TRANSCRIBING, false);
    dispatch(&d, Request::SetPaused { paused: true });
    assert_eq!(
        d.state.load(Ordering::SeqCst),
        TRANSCRIBING,
        "pause must defer, never seize a busy state"
    );
}

#[test]
fn unpausing_returns_to_idle() {
    let d = fake_daemon_with_normalize(PAUSED, false);
    dispatch(&d, Request::SetPaused { paused: false });
    assert_eq!(d.state.load(Ordering::SeqCst), IDLE);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p owf-core server::tests::pausing server::tests::toggle_is_refused_while_paused`
Expected: FAIL — no `PAUSED`.

- [ ] **Step 3: Implement**

Add to `owf_core::proto::Request`:

```rust
    /// Tray-driven. Pausing leaves the shortcut bound and the app running; it
    /// only makes `Toggle` refuse, so no microphone is opened.
    SetPaused { paused: bool },
```

Then in `server.rs`: add `const PAUSED: u8 = 7;`, extend `state_of` and `snapshot_event`, and add the handler — a CAS from `IDLE` only, so a busy state is never seized:

```rust
Request::SetPaused { paused } => {
    let (from, to) = if paused { (IDLE, PAUSED) } else { (PAUSED, IDLE) };
    // Deliberately not a store: a `SetPaused` arriving mid-utterance must
    // defer to it. Invariant 1 protects the text already produced; nothing
    // else would protect it from a pause that seized TRANSCRIBING.
    let _ = daemon.state.compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst);
    Response::ok(state_of(daemon.state.load(Ordering::SeqCst)))
}
```

and add `PAUSED` to the refusal arms of `Toggle` and `PttStart`.

Add `State::Paused` to `proto.rs`, a matching `OverlayEvent::Paused`, one line to `src-tauri/fixtures/replay-full.ndjson`, and the TS union member in `src/Overlay.tsx` — invariant 3's remaining two copies, plus the fixture the drift test reads.

- [ ] **Step 4: Run the tests**

Run: `cargo test --workspace`
Expected: PASS, including `checked_in_fixture_covers_every_event_kind`, which will fail if the fixture line was forgotten.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: pause, as a state that defers rather than seizes"
```

### Task 14: The overlay as a layer-shell surface

**Files:**
- Create: `src-tauri/src/layer.rs`
- Modify: `src-tauri/src/lib.rs`, `crates/owf-core/src/hypr.rs`

**Interfaces:**
- Consumes: Task 1's probe result
- Produces: `layer::anchor_overlay(&tauri::WebviewWindow) -> anyhow::Result<()>`

- [ ] **Step 1: Implement, guarded by the probe's finding**

If Task 1 Step 4 succeeded:

```rust
//! Places the overlay without a compositor rule.
//!
//! CLAUDE.md invariant 5 says client-side positioning is a no-op under
//! Hyprland — true for an `xdg_shell` toplevel, and not true for a
//! layer-shell surface, which anchors itself. `keyboard-interactivity: None`
//! is the same surface's answer to invariant 2, so the overlay refuses focus
//! at the protocol level rather than by a rule the user has to paste.
//!
//! wlr-layer-shell is a wlroots protocol; Mutter does not implement it. On
//! GNOME this returns `Err` and the caller leaves the window a toplevel.
pub fn anchor_overlay(window: &tauri::WebviewWindow) -> anyhow::Result<()> {
    use gtk_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
    let gtk_win = window.gtk_window()?;
    gtk_win.init_layer_shell();
    gtk_win.set_layer(Layer::Overlay);
    // Invariant 2, at the protocol level: this surface can never be given
    // keyboard focus, so `wtype` cannot type the dictation into it.
    gtk_win.set_keyboard_mode(KeyboardMode::None);
    gtk_win.set_anchor(Edge::Bottom, true);
    gtk_win.set_margin(Edge::Bottom, crate::bottom_margin_logical() as i32);
    Ok(())
}
```

Call it in `setup()` before the window is shown, and fall back on error:

```rust
if let Err(e) = layer::anchor_overlay(&window) {
    eprintln!("overlay: layer-shell unavailable ({e}); falling back to a toplevel");
}
```

If it failed, `anchor_overlay` returns `Err` unconditionally with the probe's error, and `hypr.rs` emits the title-matched window rules instead. Either way `position_overlay`'s existing no-op stays as documentation of why it is a no-op.

- [ ] **Step 2: Delete the window rules that layer-shell replaces**

In `hypr.rs`, `shortcut_config()` (renamed from `hypr_config()`) emits only the two `bind` lines when layer-shell works, plus the "delete these" header from spec §10.

- [ ] **Step 3: Verify with no window rules present**

Run: `cargo run -p openwhisprflow --features custom-protocol -- --replay src-tauri/fixtures/replay-full.ndjson`
Expected: bottom-centre placement with no rules in `~/.config/hypr/windows.lua`.

- [ ] **Step 4: Verify the overlay still refuses focus**

Run: `hyprctl clients -j | jq -r '.[] | select(.title|test("openwhisprflow")) | {title, focusHistoryID}'`
Expected: the overlay never becomes the focused window. Invariant 2 is the one that types your dictation into the wrong window if it breaks.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: anchor the overlay with layer-shell instead of a window rule"
```

### Task 15: First run

**Files:**
- Modify: `src/settings/schema.ts`, `src/Settings.tsx`, `src-tauri/src/settings_cmds.rs`

**Interfaces:**
- Consumes: `owf_core`'s existing provisioning
- Produces: Tauri command `setup_status()` and event `setup-progress`

- [ ] **Step 1: Expose provisioning state**

```rust
/// What the Setup pane renders. Every check here already exists in
/// `setup.rs` — `check_prerequisites` encodes the Arch `ggml-cpu` trap that
/// makes `llama-server` fail with ggml's opaque "no backends are loaded", and
/// duplicating that knowledge in a second place is how it goes stale.
#[tauri::command]
pub fn setup_status() -> Result<serde_json::Value, String> {
    let missing = crate::setup::check_prerequisites();
    Ok(serde_json::json!({
        "missing_prerequisites": missing,
        "ready": missing.is_empty(),
    }))
}
```

`check_prerequisites()` is currently private to `setup.rs`; make it `pub(crate)`. It already reports missing models, `llama-server` and the ggml backend, which is the whole of what this pane shows — do not add a second model check beside it.

- [ ] **Step 2: Add the Setup pane**

A new first pane in `schema.ts`, shown automatically when `setup_status()` reports anything missing, with per-model progress and the prerequisite list.

- [ ] **Step 3: Verify on a pristine config**

Run: `XDG_DATA_HOME=$(mktemp -d) cargo run -p openwhisprflow --features custom-protocol`
Expected: the settings window opens on Setup; `--toggle` is refused with a stated reason.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat: first-run setup pane replaces owf-ctl setup"
```

### Task 16: Autostart toggle

**Files:**
- Modify: `src-tauri/src/settings_cmds.rs`, `src/settings/schema.ts`

- [ ] **Step 1: Write the failing test**

```rust
/// Writing the file is what enables autostart; removing it is what disables
/// it. Both must be idempotent — a user who toggles twice must not end up
/// with two entries or a stale one.
#[test]
fn the_autostart_desktop_file_is_written_and_removed_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("openwhisprflow.desktop");
    set_autostart_at(&p, true).unwrap();
    set_autostart_at(&p, true).unwrap();
    assert!(p.exists());
    assert!(std::fs::read_to_string(&p).unwrap().contains("Exec=openwhisprflow"));
    set_autostart_at(&p, false).unwrap();
    set_autostart_at(&p, false).unwrap();
    assert!(!p.exists());
}
```

- [ ] **Step 2: Run, implement, run**

Run: `cargo test -p openwhisprflow autostart`
Expected: FAIL, then PASS after implementing `set_autostart_at`.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: Beim Anmelden starten writes an XDG autostart entry"
```

### Task 17: Migration and docs

**Files:**
- Modify: `crates/owf-core/src/hypr.rs`, `README.md`, `HANDOVER.md`, `CLAUDE.md`

- [ ] **Step 1: Write the failing test for the emitted block**

```rust
/// Spec §10: this breaks in the worst way available — a shortcut that
/// silently does nothing. So the emitted block leads with what to delete.
#[test]
fn the_emitted_block_names_the_lines_to_delete_before_the_ones_to_add() {
    let out = shortcut_config();
    let delete_at = out.find("owf-ctl").expect("must name the old command");
    let add_at = out.find("--toggle").expect("must name the new one");
    assert!(delete_at < add_at);
    assert!(out.contains("bindr"), "the bindr line has no replacement and must be named");
}
```

- [ ] **Step 2: Run, implement, run**

Run: `cargo test -p owf-core hypr::`
Expected: FAIL, then PASS.

- [ ] **Step 3: Update CLAUDE.md**

Rewrite the architecture section for two crates; rewrite invariant 2 (mechanism), invariant 3 (two copies), invariant 5 (superseded on wlroots); replace every `owf-ctl` command in the Commands block with its flag; add `audio.max_seconds`' new standing as a numbered invariant.

- [ ] **Step 4: Update README.md and HANDOVER.md**

The setup guide loses `owf-ctl setup` and the exec-once lines; it gains "install, launch, paste two `bind` lines".

- [ ] **Step 5: Final gate**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "docs: migrate README, HANDOVER and CLAUDE.md to the one-binary shape"
```

---

## Notes for the executor

- **Do not record from the microphone.** Every verification step above is either a unit test, a `--replay` run, or a `hyprctl` query. The audio half of this pipeline remains unverified by a human and that is deliberate; see `HANDOVER.md`.
- **Task 1 can change Tasks 12 and 14.** Do not start Phase 3 before its results are written into the spec.
- **If a task's test count drops, stop.** Tasks 2 and 11 move large amounts of code; a lower count means tests were lost in a move, which is a failure of that task rather than a detail to fix later.
