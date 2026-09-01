# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Press-to-start, press-to-stop dictation for Hyprland/Wayland. Press `SUPER+D`, speak,
press `SUPER+D` again; the audio is captured, VAD-trimmed, transcribed (Parakeet TDT via
`sherpa-onnx`), rewritten by S1-mini (llama.cpp, in-process), checked by a guardrail, and
typed into the focused window with `wtype` (or `ydotool`, if `[inject] backend` selects
it). Fully local at dictation time. `SUPER+ALT+D`
cancels a recording in progress; nothing else can end one deliberately — see invariant 11.

yappr is one binary, `yappr`, and one process. Running it with no
arguments starts everything: a tray icon (no window), the Unix socket, the models, and
the pipeline. Left-clicking the tray (or `yappr --settings`) opens the settings
window; right-clicking gives Status / Einstellungen / Diktat pausieren / Beenden. There is
no `owf-ctl`, no separate daemon binary, and no systemd unit.

The app was called **OpenWhisprFlow** until 2026-08-29, and the rename reached the
binary, the crate (`owf-core` → `yappr-core`), the Tauri identifier, and the whole
XDG namespace (`~/.config/yappr`, `~/.local/share/yappr/models`, `yappr.sock`,
`~/yappr` for debug capture). Two places deliberately still say `openwhisprflow`,
and both are correct: the design docs and SDD logs described below, which are dated
records of what was decided under the old name, and the migration text in `hypr.rs`
plus README's upgrade section, which name the *old* binary and window title because
their whole job is telling a user which dead lines to delete. Watch for that
distinction when editing either — a blanket rename through them makes them false.
`owf-ctl`, `owf-cli` and `owf-daemon` in comments are the same case: deleted crates
that genuinely had those names, not stale spellings.

`README.md` is the user-facing setup guide. `HANDOVER.md` is the current state-of-play,
including what has and has not been verified on real hardware.

## Commands

```bash
# One binary, `yappr`. No arguments starts the app (tray, socket, models,
# pipeline); every other invocation is a client call against a running instance or a
# local utility, and both exit before touching Tauri/GTK/WebKit or a model
# (`src-tauri/src/cli.rs`'s route(), `client.rs`'s dispatch).
cargo build --release -p yappr --features custom-protocol
#   ^ `custom-protocol` is deliberately NOT a default feature. Without it a plain
#     cargo build embeds `devUrl` instead of `frontendDist` and the window is blank
#     unless a Vite dev server is running.
bun run tauri dev                         # dev: Vite on :1420 + the Tauri windows
bun run build                             # frontend only (tsc && vite build -> dist/)
#   ^ builds BOTH pages: index.html (overlay) and settings.html (settings window).

# Tests (399 passed, 0 failed, 8 #[ignore]d because they need downloaded models)
cargo test --workspace
# NOT optional. These are the only tests that catch a C++ ABI mismatch between
# sherpa-onnx and llama.cpp -- see the gotcha at the bottom of this file. A wrong
# `CXXFLAGS` aborts the process inside the VAD, and the default run notices nothing.
cargo test --workspace -- --ignored       # needs models already on disk (Settings' Setup pane, or --update-lock)
cargo test -p yappr-core guardrail::        # one module
cargo test -p yappr-core --test pipeline_e2e a_good_cleanup_is_injected
cargo test -p yappr-core --lib server::tests::state_of_and_snapshot_event_report_failed_as_a_real_error_state
cargo clippy --workspace --all-targets    # kept clean

# Exercising things without a microphone
cargo run -p yappr -- --replay src-tauri/fixtures/replay-full.ndjson
cargo run -p yappr-core --example list_devices
cargo run --release -p yappr -- --bench   # ASR latency table

# Runtime inspection / control (against a running instance)
yappr --status | --debug | --subscribe | --reload
yappr --settings | --wizard | --toggle | --cancel | --quit
yappr --print-shortcuts | --purge-logs | --update-lock
```

There is no CI. `cargo test --workspace && cargo test --workspace -- --ignored && cargo
clippy --workspace --all-targets` is the gate -- the `--ignored` half included, for the
ABI reason above. Tags `known-good-m1` / `known-good-m2` predate this one-process rewrite (16 tasks of
it, on this branch) — they are safety points for the M1/M2 milestone they were cut at, not
a "recent work" undo button; nothing on this branch has added an equivalent tag yet.

## Architecture

Two Cargo members, one process:

- **`crates/yappr-core`** — the library. Every pipeline stage plus config, paths, wire
  format, model provisioning, debug capture — and, since the one-process rewrite,
  `server.rs`: the socket server and the `AtomicU8` state machine, moved here unchanged
  (with its tests) from the now-deleted `crates/owf-cli`. Links `sherpa-onnx`, `cpal`,
  `rubato`, so anything that depends on it inherits a heavy build — which now includes
  the GUI itself, deliberately (see "Why not keep the daemon separate" in the design doc).
- **`src-tauri`** (package `yappr`, binary `yappr`) — the whole app:
  the overlay window, the settings window, the tray (a native StatusNotifierItem via
  `ksni` — see `tray.rs`'s module doc for why not Tauri's own `tray-icon` feature), the
  layer-shell placement logic (`layer.rs`), and the socket server itself (`yappr_core::server`,
  started in-process by `setup()` in `lib.rs`). It links `yappr-core` directly — the split
  that used to keep the overlay a lightweight, read-only socket client is gone along with
  `settings-tauri`, the second Tauri app the settings window used to be; both were casualties
  of collapsing to one process. `cli.rs`'s `route()` and `client.rs`'s `dispatch()` are what
  survives of the old `owf-cli`: a pure `&[&str] -> Route` function with a test per row (a
  compositor shortcut is not something to break silently), and the fast path that runs
  ahead of any Tauri/GTK/WebKit initialisation for every flag except no-args and `--replay`.
  `GetConfig`/`SetConfig`/`ListInputDevices` (`settings_cmds.rs`) are plain Tauri commands
  now, not socket requests — the settings window is in-process, so there's no transport
  left to put them on; they call `yappr_core::server::dispatch` directly with the same
  request/response JSON shapes the socket used to carry, so `src/settings/` and
  `Settings.tsx` needed no changes at all.
- **`src/`** — the React frontend for both of the app's windows, built as two Vite entry
  points from one `bun run build`. Motion (`motion/react`) is the one UI
  dependency: every transition in both windows is a spring, parameterised the
  way Apple parameterises one — `bounce` is the damping ratio (`0` = critically
  damped, no overshoot) and `duration` is the *response*, a settle time rather
  than a fixed playback length, so an interrupted spring re-targets from where
  it is instead of restarting. Each file declares its springs as named consts
  at the top (`SETTLE`, `LAND`, `SNAP`, `GLIDE`, `KNOB`) and uses nothing else;
  overshoot is reserved for motion that had momentum behind it.
  `App.tsx` is just `<Overlay />`. `Settings.tsx` is
  the settings window's shell — sidebar, panes, autosave — over `src/settings/`:
  `wizard.tsx` (the four-step first-run wizard, which takes over the whole
  window while it is active and replaced the old Setup pane; it owns the
  `setup_status`/`run_setup` calls that pane used to make),
  `schema.ts` (which pane a section lives in, what a field is called in German, which
  fields get an editor better than a text box), `controls.tsx` (the setting-row
  primitive and its toggle/select/number/list/table editors), `icons.tsx` (inline SVG,
  so the window needs no icon dependency). No pipeline logic in TS, and no copy of the
  config schema: `schema.ts` only decides how a key that already exists is *shown*, so
  a key added in Rust and named nowhere here still renders, by its JSON type, in its
  section's pane — or in the `Weitere` pane if no category claims its section. A new
  setting can become unlabelled; it cannot become unreachable. `schema.ts` also
  owns `search()`, which matches a query against a row's German label, its help
  text, *and* its raw `config.toml` key — a key added in Rust and named nowhere
  here is still findable by the name Rust gives it.

Wayland has no global keyboard grab, which is why argument dispatch in `main.rs` runs
`cli::route()` *before* any Tauri/GTK/WebKit initialisation and before any model is
touched: a shortcut you bind yourself runs `yappr --toggle` / `--cancel`, and
that invocation must behave like a thin client, not pay the cost of the whole app, even
though it *is* the same binary. A client call opens `$XDG_RUNTIME_DIR/yappr.sock`,
writes one NDJSON request line, and reads one response line back (`yappr-core/src/proto.rs`
— unchanged wire format from when a separate `owf-ctl` held this logic).
`Request::Subscribe` is the exception: it converts the connection into an open-ended
NDJSON `OverlayEvent` stream, starting with a snapshot of the current state.
`server.rs`'s `handle` special-cases it before `dispatch`.

The server's state machine is an `AtomicU8` with consts at the top of `server.rs`
(`WARMING`, `IDLE`, `RECORDING`, `TRANSCRIBING`, `NORMALIZING`, `INJECTING`, `FAILED`,
`PAUSED`); `proto::State` / `proto::OverlayEvent` are the wire projections of it.
`Request::Toggle` is resolved against whichever of these the server is in when it
arrives, never against a client-side memory of the last press — the client is a fresh
process every time. A single utterance runs through `pipeline::Pipeline::process_with_capture`.

S1-mini is loaded by `load_models` — at startup only when `[models]
preload_at_startup` is on, otherwise on the first `ptt-start` — into this same
process, via `llama-cpp-2` (`llama.rs`'s `LlamaEngine`). llama.cpp and a ggml
CPU backend are linked statically; `ldd target/release/yappr` resolves no
`libllama` or `libggml`. A model that fails to load degrades to
`UnavailableNormalizer` rather than failing the app, and the next dictation
retries.

This replaced a spawned, supervised `llama-server` child, and took a lot with
it: `LlamaServer`, port picking, `/health` polling, the 10 s poll and 1→30 s
backoff restart in `spawn_housekeeping`, `supervise_llama_once`,
`should_supervise_llama`, `kill_llama`, the `daemon.llama` mutex, the
`$XDG_RUNTIME_DIR/yappr.port` file, and `setup.rs`'s ggml-backend probe. The
engine's lifetime is now exactly the `Pipeline`'s — it *is* the pipeline's
normalizer — so `unload_models` setting `pipeline = None` is the whole of
releasing the model, and a load finishing during shutdown can no longer orphan
a child. `spawn_housekeeping` now only reaps subscribers and runs the
idle-unload deadline.

The trade, stated where it will be seen: a crash inside llama.cpp now takes the
app down, where a crashing child used to degrade to raw ASR text.
`sherpa-onnx` already had that property, on a hotter path.

Two `[normalize]` keys are **accepted and ignored**: `port` and
`llama_server_path`. There is no server to point them at, but the section is
`deny_unknown_fields` (invariant 4), so deleting them from `NormalizeConfig`
would turn every pre-existing `config.toml` into a hard startup failure. They
are gone from the annotated default and hidden from the settings GUI by
`schema.ts`'s `OBSOLETE_FIELDS` — the one thing in that file that can hide a
row, and only because these are not settings.

`[models]` owns the model lifetime: `preload_at_startup` (default `false`) and
`idle_unload_seconds` (default `60`, `0` = never). `ensure_models_loaded` reads
the config from disk at load time, so `[asr]`/`[normalize]` changes take effect
on the next dictation for a lazily-loaded daemon — `schema.ts`'s
`RESTART_SECTIONS` is unchanged because it is still correct whenever the models
happen to be resident.

## Invariants worth knowing before editing

1. **Once ASR has produced text, the user gets text.** Normalization erroring, timing out,
   or being rejected by the guardrail all fall back to the raw transcript (or a rule-based
   cleanup of it). See `pipeline.rs`'s module doc and spec §15. Never add a path that can
   lose a transcribed utterance.
2. **The overlay must never take keyboard focus** — a focused overlay means `wtype` types
   the dictation into the overlay instead of the target window. Enforced twice, on two
   mechanisms that both apply unconditionally, not chosen between at runtime:
   - The **overlay** always carries Tauri's `focus: false` / `focusable: false`
     (`tauri.conf.json`), regardless of compositor. The settings window is
     `focus: true` / `focusable: true` and must stay that way -- it hosts a
     form and the setup wizard, neither of which could take a keystroke
     otherwise. That asymmetry is why the fallback rule below matches title.
   - On a compositor implementing `wlr-layer-shell` (every wlroots compositor, including
     Hyprland; not Mutter), `src-tauri/src/layer.rs`'s `anchor_overlay` additionally puts
     the overlay on a layer-shell surface with `KeyboardMode::None` — focus refused at the
     protocol level, no compositor window rule involved.
   - Where layer-shell is unavailable (GNOME/Mutter, or `anchor_overlay` failing for any
     other reason), `hypr.rs`'s emitted window rule is the fallback — now keyed on the
     overlay's **title** (`"yappr overlay"`), not its class. The settings window
     became a window of this same app, so a class-matched rule would reach it too and it
     could no longer take a keystroke — exactly the regression the previous two-Tauri-app
     split existed to avoid, now fixed by matching title instead.
3. **`OverlayEvent` exists in two hand-maintained copies**: `yappr-core/src/proto.rs` (source
   of truth) and the TS union in `src/Overlay.tsx`. `src-tauri/src/wire.rs`'s separate
   mirror is gone — the overlay now links `yappr-core` directly (same process as the server),
   so there is nothing left to avoid pulling `sherpa-onnx`/`cpal` into by duplicating the
   type. Changing or adding a variant still means touching both *and* adding a line to
   `src-tauri/fixtures/replay-full.ndjson` — two tests cross-check drift mechanically
   (`the_overlay_replay_fixture_parses_as_this_crates_overlay_event` in `proto.rs`,
   `checked_in_fixture_covers_every_event_kind` in `replay.rs`).
4. **Config uses `#[serde(deny_unknown_fields)]` everywhere**, so an unrecognised key or
   section is a hard startup failure. Documenting a `config.toml` section without
   adding the matching struct bricks boot — that is what `OverlayConfig` exists to prevent.
5. **Client-side window positioning is a no-op under Hyprland's `xdg_shell` — superseded for
   the overlay on any compositor with `wlr-layer-shell`.** The overlay frontend still calls
   the `position_overlay` Tauri command (`src-tauri/src/lib.rs`) unconditionally on mount,
   and it is still a genuine no-op under Wayland either way (`set_position` does nothing on
   `xdg_shell`, and layer-shell surfaces aren't positioned that way either) — harmless dead
   weight kept only because it would do something on X11/XWayland/macOS/Windows. What
   actually places the overlay now depends on the compositor: where `wlr-layer-shell` is
   available, `layer.rs`'s `anchor_overlay` sets `Edge::Bottom` with a margin directly, at
   the protocol level, with no window rule involved — real positioning, independent of
   `position_overlay`. On GNOME/Mutter (no `wlr-layer-shell`) the overlay falls back to an
   ordinary, unpositioned toplevel, and placement there can only come from the emitted,
   title-matched compositor rule (invariant 2) — `position_overlay`'s no-op is the whole of
   this fallback path's own contribution, same as before this rewrite.
6. **Every subprocess call goes through `procutil::run_with_timeout`.** A hung `wtype` or
   `hyprctl` previously wedged the server's single-threaded accept loop forever.
7. **Debug records are written from `DebugRecordGuard`'s `Drop`**, not at each return point,
   so a `?` added anywhere in `process_with_capture` still produces a record.
8. **Capitalisation and terminal punctuation are applied at one choke point**
   (`finish::finish`, called once in `process_with_capture` just before injection),
   never per assignment to `text`. They used to live only in
   `guardrail::rule_based_fallback`, so the *fallback* paths produced finished text
   while an accepted S1-mini cleanup was injected exactly as the model returned it —
   which for German means every sentence start lowercased and the final stop dropped.
   The guardrail cannot catch that: `guardrail::tokenize` lowercases and strips
   punctuation before comparing, so case damage is invisible to it by construction.
   `finish` is idempotent; keep it that way and the choke point stays safe.
9. **The settings GUI writes `config.toml` through `config_write`, never
   `toml::to_string`.** It merges into the existing document with `toml_edit`,
   skips leaves whose value is unchanged, validates the rendered result with
   `Config::from_str` *before* touching the file, and replaces it by atomic
   rename. `config.toml` is a file the user is invited to edit by hand: a save
   that changes nothing must leave it byte-identical, and one that changes a
   setting must produce a one-line diff. This is what lets the GUI autosave at
   all: it has no Save button, and writes a toggle or dropdown immediately and a
   text or number field 700 ms after the last keystroke (`DEBOUNCE_MS`), with one
   write in flight at a time and later edits coalesced into a single follow-up.
   A rejected save keeps the value on screen — the daemon validates before it
   writes, so the file is untouched and discarding what the user typed on top of
   that would be the GUI's own doing.
   `GetConfig` also answers with `defaults` (`Config::default()` as JSON, shaped
   like `config`), which is what each row's reset button restores to and what
   makes it hide itself on a row already at its default. It travels on the wire
   for the same reason `config` does: a defaults table maintained in TypeScript
   would be a second copy of the schema, free to drift.
10. **The dictation vocabulary runs before normalization** (`vocab::apply`, on the ASR
    output), so S1-mini and the guardrail both see corrected text and the overlap check
    compares like with like. `dbg.asr_raw` keeps what the ASR actually said. Short
    acronyms belong in `[[vocabulary.replacements]]`, not `terms`: a three-character
    term is within edit distance 2 of most three-letter words, so fuzzy-matching it
    would rewrite unrelated ones.
11. **`audio.max_seconds` is no longer a rare-accident guard — it is the *sole* terminator
    of a forgotten recording.** Hold-to-talk had a physical guarantee that a recording
    ends: the user's own finger. Press/press toggle has none: once a recording starts,
    nothing but a second `--toggle` press (or `--cancel`) closes the microphone, except
    the epoch-checked watchdog `start_recording` spawns (`spawn_safety_valve` in
    `server.rs`), which fires after `audio.max_seconds` (120 s default) and, per
    invariant 1, *transcribes* what it captured rather than discarding it — it is not a
    failure path, and it must stay that way. Because a missed second press now leaves the
    microphone open until this fires, its default is a number the user should choose
    deliberately rather than inherit silently; `config_write.rs`'s annotated fixture and
    the Settings GUI describe it as ending "a forgotten recording" (Sicherheitsnetz), not
    a stuck key, for the same reason.
12. **The models are not resident by default, and `load_lock` is always taken
    before `pipeline`.** `[models] preload_at_startup` defaults to `false`, so
    an idle daemon routinely has `pipeline: None` and no model resident
    at all — a state that used to be reachable only during warm-up and is now
    ordinary. Three consequences that are easy to break:
    - **Never lock `pipeline` to ask whether the models are loaded.** Not
      merely slow — an outright self-deadlock, on the commonest failure path
      there is. `process_utterance` holds that mutex's guard for the whole
      `process` call and broadcasts `Error` *from inside the guard*: once for
      "no speech detected", once for a pipeline error. On `Error`, and only on
      `Error`, `TauriSink::refresh_tray_icon` dispatches `Request::Status`
      **synchronously** (`src-tauri/src/lib.rs`) to tell a fatal startup
      failure from a transient one. So a `Status` handler that
      locked `pipeline` would re-enter a `std::sync::Mutex` it already holds,
      on the same thread — which is UB-or-deadlock, not a wait — every time
      the user presses twice without speaking. `Status` and the housekeeping
      thread read `daemon.models_loaded` instead, the same reason
      `normalize_available` exists. (The `llama-server` supervisor that used
      to be the other reader of this flag is gone; the deadlock it would have
      caused is not.)
    - **A lazy load failure is retryable, not fatal.** `run_utterance`
      broadcasts `Error` and returns to `IDLE`; only the `preload_at_startup`
      path still latches `FAILED` with a stored `fatal_error`. On a fresh
      install this is what lets the Setup pane's download be followed by a
      press rather than a restart.
    `preload_at_startup = true` with `idle_unload_seconds = 0` reproduces the
    pre-2026-08-29 behaviour exactly, and is what to point a user at if the
    lazy path misbehaves. See
    `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.

13. **The first-run wizard never writes desktop config, on either desktop.**
    Hyprland is the standing rule (a bad window rule breaks the desktop).
    GNOME is the same rule for a different reason: `gsettings set ...
    custom-keybindings` *replaces* the list, so the obvious one-liner destroys
    every custom shortcut the user already had -- `desktop.rs`'s
    `GNOME_GSETTINGS_SNIPPET` reads the current value and appends, and the
    wizard shows it rather than running it. The wizard opens when
    `~/.local/state/yappr/wizard-done` is absent *or* `setup_status()` is not
    ready (`src-tauri/src/wizard.rs`'s `should_open`), so a model deleted
    after setup brings it back -- at the models step, not the welcome -- 
    instead of surfacing as a failed dictation days later. A state file, not
    a config key: a key would have to join a `deny_unknown_fields` struct and
    then appear in the settings GUI as a setting nobody should touch. See
    `docs/superpowers/specs/2026-08-29-first-run-wizard-design.md`.

## Conventions

- Code comments cite **`spec N`** / **`spec §N`**, meaning section numbers in one of two
  design docs under `docs/superpowers/specs/`: `2026-08-27-openwhisprflow-design.md` for
  the pipeline, guardrail, config and injection code that predates the one-process
  rewrite, and `2026-08-28-one-process-tray-app-design.md` (rev 3) for anything about the
  single binary, the tray, toggle semantics, layer-shell, or migration. Neither doc names
  itself inline in these citations, so which one a given `spec N` means is inferred from
  what the surrounding code does — worth watching for a section-number collision between
  the two docs, since nothing currently disambiguates one from the other in the comment
  itself. Implementation plans live in `docs/superpowers/plans/`, and per-task decision
  logs in `.superpowers/sdd/*/progress.md`. All of these predate the rename and still
  say `openwhisprflow` throughout, filenames included — they are history, and the
  stored `review-*.diff` files under `.superpowers/sdd/` have to keep matching the
  commits they record.
- Tests are named as full sentences. Documented-but-unfixed behaviour is pinned by a test
  prefixed `known_limitation_` (e.g. `known_limitation_dense_digit_sequences_trip_word_ratio`
  in `guardrail.rs`).
- `Pipeline` gains test seams as builder methods (`with_rejections_path`,
  `with_recovery_dir`, `with_fallback_injector`, `with_stage_events`), not new positional
  parameters on `new`. `rejections_file()` is a live guardrail-tuning dataset — tests must
  redirect writes to it or they poison real data.
- `yappr-core` no longer has a `test-util` feature. It existed to expose
  `LlamaServer::from_child`, a seam for stubbing the supervised child with `sleep 300`;
  with no child process left there is nothing to stub, so it was deleted rather than
  left declared and unused. The tests that relied on it now observe `shutdown` through
  the runtime files it removes.
- All filesystem locations come from `yappr-core/src/paths.rs` (XDG): config
  `~/.config/yappr/config.toml`, models `~/.local/share/yappr/models`
  (pinned by sha256 in `crates/yappr-core/models.lock.toml`), `rejections.jsonl` in the
  state dir, socket/lock/port in `$XDG_RUNTIME_DIR`, autostart entry at
  `~/.config/autostart/yappr.desktop`. Debug capture writes to `~/yappr` by default.
- **The app icon has one source: `public/yappr.png`.** Everything else is derived from
  it. `src-tauri/icons/*` is generated — `bunx tauri icon public/yappr.png`, then delete
  the `android/` and `ios/` trees it also writes, which this app has no target for.
  `tauri.conf.json`'s `bundle.icon` list is what Linux packaging turns into
  `hicolor/<w>x<h>/apps/yappr.png`, so a size missing from that list is a size the
  desktop has to scale for itself. The two HTML entries' favicons and the settings
  sidebar's brand mark reference `/yappr.png` directly, out of `public/` — replace that
  one file and the whole app follows. The tray is the deliberate exception: `tray.rs`
  names *freedesktop symbolic* icons because its icon is a state readout, not branding
  (invariant: recording must never look like idle), and a full-colour plate would neither
  theme with the panel nor survive 22 px.

## Environment gotchas

- **sherpa-onnx and llama.cpp must be built with the same C++ ABI, and `.cargo/config.toml`
  is what makes that true.** The most dangerous thing about linking both into one binary,
  and it fails at runtime rather than at link time. `sherpa-onnx` ships a prebuilt
  `libonnxruntime.a` compiled with the *old* libstdc++ ABI (`_GLIBCXX_USE_CXX11_ABI=0`:
  11,675 old-ABI `std::string` symbols in it, zero new-ABI); `llama-cpp-sys-2` builds
  llama.cpp here, and cmake defaults to the *new* ABI. Both instantiate `std::regex`'s
  `_Compiler` as a weak symbol, the linker keeps one, and ONNX Runtime's
  `DeviceDiscovery::DiscoverDevicesForPlatform` — which builds a `std::regex` while
  creating its `OrtEnv` — then runs against a `std::string` with the wrong layout. The
  process aborts with `free(): invalid pointer` inside `SileroTrimmer::new`: in the VAD,
  on every utterance.

  `.cargo/config.toml` forces `CXXFLAGS=-D_GLIBCXX_USE_CXX11_ABI=0`, with `force = true`
  so an inherited `CXXFLAGS` cannot silently win and produce a memory-corrupting binary.
  Do not remove it, and do not add another C++ dependency without checking its ABI:

  ```bash
  nm libwhatever.a | grep -c NSt7__cxx1112basic_string        # new ABI
  nm libwhatever.a | grep -cE 'Ss[0-9EC]|SbIcSt11char_traits' # old ABI
  ```

  Only `cargo test --workspace -- --ignored` catches a regression here. Two cheaper
  guards were tried and both are useless — read the note at the top of `vad.rs`'s test
  module before writing a third.
- **`gtk-layer-shell` is a build-time link dependency, not a runtime check.** `src-tauri`'s
  `Cargo.toml` pulls in `gtk-layer-shell = { version = "0.8", features = ["v0_6"] }` for
  `layer.rs`. Without the system library, `cargo build` (or `cargo test`/`cargo clippy` —
  anything touching this crate) fails outright in `gtk-layer-shell-sys`'s build script,
  unable to find `gtk-layer-shell-0` via `pkg-config`. On Arch: `sudo pacman -S
  gtk-layer-shell`. Unlike the ABI gotcha above, there is no way to defer this to a
  runtime check or a Setup-pane warning — the binary simply does not exist without it.
- **Building an AppImage on current Arch needs two environment fixes, and fails with only
  "failed to run linuxdeploy" if either is missing.** Both failures produce that same
  unhelpful message: the Tauri CLI swallows linuxdeploy's stderr, so the error text tells
  you nothing about which one you hit.
  1. **gdk-pixbuf's `.pc` advertises a path that no longer exists.** Tauri's
     `linuxdeploy-plugin-gtk` asks `pkg-config` for gdk-pixbuf's loader directory, and
     Arch's installed `.pc` still advertises `/usr/lib/gdk-pixbuf-2.0/2.10.0`, which
     modern gdk-pixbuf no longer ships because its loaders are compiled in now. The plugin
     tries to `cp` from that nonexistent path and dies. Shadow the `.pc` on
     `PKG_CONFIG_PATH`: copy `/usr/lib/pkgconfig/gdk-pixbuf-2.0.pc` to a scratch directory
     and rewrite `gdk_pixbuf_binarydir`, `gdk_pixbuf_moduledir` and
     `gdk_pixbuf_cache_file` to point inside a directory that exists. The directory must
     exist; the `loaders.cache` file it names does **not** need to (verified).
  2. **`NO_STRIP=1` is required.** Without it linuxdeploy's strip step fails and takes the
     whole bundle with it.

  Verified 2026-08-29, one command, no manual `linuxdeploy` invocation:

  ```bash
  PKG_CONFIG_PATH=<scratch-dir>:$PKG_CONFIG_PATH NO_STRIP=1 \
      bun run tauri build --bundles appimage
  ```

  This corrects two earlier claims here. The Tauri CLI **does** propagate
  `PKG_CONFIG_PATH` to the plugin subprocess, so driving `linuxdeploy` by hand — and the
  matching warning about keeping compile flags identical across two runs — is no longer
  necessary. `NO_STRIP=1` was not previously documented and is the likelier cause if a
  build fails today with the shadow `.pc` already in place.
- **A freshly built AppImage has a 32×32 root icon; run `scripts/fix-appimage-icon.sh`
  on it after every build.** The linuxdeploy build Tauri pins (1-alpha, 659c9db) has no
  icon size preference: it overwrites the root `yappr.png` Tauri placed (the 512×512)
  with a symlink to the first hicolor icon it lists — the 32×32 — and `.DirIcon` points
  there. That root icon is what desktop integrators (Gear Lever, appimaged) extract for
  the app menu, so without the fix every user gets a 32 px menu icon scaled up. The
  script repacks the AppImage in place with the 256×256 at the root (the AppImage spec's
  recommended `.DirIcon` size), reusing the original runtime; verified 2026-09-01. The
  release CI runs it after `tauri build` — a locally built AppImage needs it run by hand.
- **Do not trust `cpal`'s advertised sample-rate range.** It advertised 16 kHz on hardware
  that rejected the stream build; `capture.rs` now probes by building a throwaway stream and
  falls back to 48 kHz plus `rubato` resampling.
- **Never apply Hyprland config on the user's behalf** — emit it (`yappr
  --print-shortcuts`) and let them paste it. A bad window rule breaks their desktop.
  `hypr.rs` detects Lua vs classic `.conf`; Hyprland 0.56+ with a Lua config rejects the
  legacy keyword parser outright, so the formats are not interchangeable.
- **Real dictation is still unverified end to end** (no one has spoken into it; see
  `HANDOVER.md`). Do not record from the microphone without explicit permission — that
  constraint is why the audio half remains untested.
