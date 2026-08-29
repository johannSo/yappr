# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Press-to-start, press-to-stop dictation for Hyprland/Wayland. Press `SUPER+D`, speak,
press `SUPER+D` again; the audio is captured, VAD-trimmed, transcribed (Parakeet TDT via
`sherpa-onnx`), rewritten by a local S1-mini `llama-server`, checked by a guardrail, and
typed into the focused window with `wtype` (or `ydotool`, if `[inject] backend` selects
it). Fully local at dictation time. `SUPER+ALT+D`
cancels a recording in progress; nothing else can end one deliberately — see invariant 11.

OpenWhisprFlow is one binary, `openwhisprflow`, and one process. Running it with no
arguments starts everything: a tray icon (no window), the Unix socket, the models, and
the pipeline. Left-clicking the tray (or `openwhisprflow --settings`) opens the settings
window; right-clicking gives Status / Einstellungen / Diktat pausieren / Beenden. There is
no `owf-ctl`, no separate daemon binary, and no systemd unit.

`README.md` is the user-facing setup guide. `HANDOVER.md` is the current state-of-play,
including what has and has not been verified on real hardware.

## Commands

```bash
# One binary, `openwhisprflow`. No arguments starts the app (tray, socket, models,
# pipeline); every other invocation is a client call against a running instance or a
# local utility, and both exit before touching Tauri/GTK/WebKit or a model
# (`src-tauri/src/cli.rs`'s route(), `client.rs`'s dispatch).
cargo build --release -p openwhisprflow --features custom-protocol
#   ^ `custom-protocol` is deliberately NOT a default feature. Without it a plain
#     cargo build embeds `devUrl` instead of `frontendDist` and the window is blank
#     unless a Vite dev server is running.
bun run tauri dev                         # dev: Vite on :1420 + the Tauri windows
bun run build                             # frontend only (tsc && vite build -> dist/)
#   ^ builds BOTH pages: index.html (overlay) and settings.html (settings window).

# Tests (415 passed, 0 failed, 4 #[ignore]d because they need downloaded models)
cargo test --workspace
cargo test --workspace -- --ignored       # needs models already on disk (Settings' Setup pane, or --update-lock)
cargo test -p owf-core guardrail::        # one module
cargo test -p owf-core --test pipeline_e2e a_good_cleanup_is_injected
cargo test -p owf-core --lib server::tests::state_of_and_snapshot_event_report_failed_as_a_real_error_state
cargo clippy --workspace --all-targets    # kept clean

# Exercising things without a microphone
cargo run -p openwhisprflow -- --replay src-tauri/fixtures/replay-full.ndjson
cargo run -p owf-core --example list_devices
cargo run --release -p openwhisprflow -- --bench   # ASR latency table

# Runtime inspection / control (against a running instance)
openwhisprflow --status | --debug | --subscribe | --reload
openwhisprflow --settings | --toggle | --cancel | --quit
openwhisprflow --print-shortcuts | --purge-logs | --update-lock
```

There is no CI. `cargo test --workspace && cargo clippy --workspace --all-targets` is the
gate. Tags `known-good-m1` / `known-good-m2` predate this one-process rewrite (16 tasks of
it, on this branch) — they are safety points for the M1/M2 milestone they were cut at, not
a "recent work" undo button; nothing on this branch has added an equivalent tag yet.

## Architecture

Two Cargo members, one process:

- **`crates/owf-core`** — the library. Every pipeline stage plus config, paths, wire
  format, model provisioning, debug capture — and, since the one-process rewrite,
  `server.rs`: the socket server and the `AtomicU8` state machine, moved here unchanged
  (with its tests) from the now-deleted `crates/owf-cli`. Links `sherpa-onnx`, `cpal`,
  `rubato`, so anything that depends on it inherits a heavy build — which now includes
  the GUI itself, deliberately (see "Why not keep the daemon separate" in the design doc).
- **`src-tauri`** (package `openwhisprflow`, binary `openwhisprflow`) — the whole app:
  the overlay window, the settings window, the tray (a native StatusNotifierItem via
  `ksni` — see `tray.rs`'s module doc for why not Tauri's own `tray-icon` feature), the
  layer-shell placement logic (`layer.rs`), and the socket server itself (`owf_core::server`,
  started in-process by `setup()` in `lib.rs`). It links `owf-core` directly — the split
  that used to keep the overlay a lightweight, read-only socket client is gone along with
  `settings-tauri`, the second Tauri app the settings window used to be; both were casualties
  of collapsing to one process. `cli.rs`'s `route()` and `client.rs`'s `dispatch()` are what
  survives of the old `owf-cli`: a pure `&[&str] -> Route` function with a test per row (a
  compositor shortcut is not something to break silently), and the fast path that runs
  ahead of any Tauri/GTK/WebKit initialisation for every flag except no-args and `--replay`.
  `GetConfig`/`SetConfig`/`ListInputDevices` (`settings_cmds.rs`) are plain Tauri commands
  now, not socket requests — the settings window is in-process, so there's no transport
  left to put them on; they call `owf_core::server::dispatch` directly with the same
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
touched: a shortcut you bind yourself runs `openwhisprflow --toggle` / `--cancel`, and
that invocation must behave like a thin client, not pay the cost of the whole app, even
though it *is* the same binary. A client call opens `$XDG_RUNTIME_DIR/openwhisprflow.sock`,
writes one NDJSON request line, and reads one response line back (`owf-core/src/proto.rs`
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

`llama-server` is spawned by `load_models` — at startup only when `[models]
preload_at_startup` is on, otherwise on the first `ptt-start` — and supervised
while it lives (`spawn_housekeeping` / `supervise_llama_once`: 10 s health poll,
1→30 s backoff restart, zombie reaping). That supervision is gated on
`daemon.models_loaded`, so a child killed deliberately by `unload_models` stays
dead instead of being restarted (invariant 12). A dead `llama-server` degrades
to `UnavailableNormalizer` rather than failing the app.

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
   - Every window always carries Tauri's `focus: false` / `focusable: false`
     (`tauri.conf.json`), regardless of compositor.
   - On a compositor implementing `wlr-layer-shell` (every wlroots compositor, including
     Hyprland; not Mutter), `src-tauri/src/layer.rs`'s `anchor_overlay` additionally puts
     the overlay on a layer-shell surface with `KeyboardMode::None` — focus refused at the
     protocol level, no compositor window rule involved.
   - Where layer-shell is unavailable (GNOME/Mutter, or `anchor_overlay` failing for any
     other reason), `hypr.rs`'s emitted window rule is the fallback — now keyed on the
     overlay's **title** (`"openwhisprflow overlay"`), not its class. The settings window
     became a window of this same app, so a class-matched rule would reach it too and it
     could no longer take a keystroke — exactly the regression the previous two-Tauri-app
     split existed to avoid, now fixed by matching title instead.
3. **`OverlayEvent` exists in two hand-maintained copies**: `owf-core/src/proto.rs` (source
   of truth) and the TS union in `src/Overlay.tsx`. `src-tauri/src/wire.rs`'s separate
   mirror is gone — the overlay now links `owf-core` directly (same process as the server),
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
    an idle daemon routinely has `pipeline: None` and no `llama-server` child
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
      `normalize_available` exists.
    - **The `llama-server` supervisor must stay gated on `models_loaded`.**
      Without that gate it respawns a ~955 MB child seconds after every unload,
      and the whole feature silently does nothing.
    - **A lazy load failure is retryable, not fatal.** `run_utterance`
      broadcasts `Error` and returns to `IDLE`; only the `preload_at_startup`
      path still latches `FAILED` with a stored `fatal_error`. On a fresh
      install this is what lets the Setup pane's download be followed by a
      press rather than a restart.
    `preload_at_startup = true` with `idle_unload_seconds = 0` reproduces the
    pre-2026-08-29 behaviour exactly, and is what to point a user at if the
    lazy path misbehaves. See
    `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.

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
  logs in `.superpowers/sdd/*/progress.md`.
- Tests are named as full sentences. Documented-but-unfixed behaviour is pinned by a test
  prefixed `known_limitation_` (e.g. `known_limitation_dense_digit_sequences_trip_word_ratio`
  in `guardrail.rs`).
- `Pipeline` gains test seams as builder methods (`with_rejections_path`,
  `with_recovery_dir`, `with_fallback_injector`, `with_stage_events`), not new positional
  parameters on `new`. `rejections_file()` is a live guardrail-tuning dataset — tests must
  redirect writes to it or they poison real data.
- `owf-core`'s `test-util` feature exposes `LlamaServer::from_child` (test-only child-process
  construction) so a crate testing the server can stub a process that cannot run here (a
  real `llama-server` needs a model file and a working `ggml` backend). Since `server.rs`
  moved into `owf-core` itself, its own `#[cfg(test)]` builds already satisfy
  `cfg(any(test, feature = "test-util"))` without the feature flag — it now matters only if
  a crate *outside* `owf-core` ever needs the same seam, which none currently does.
- All filesystem locations come from `owf-core/src/paths.rs` (XDG): config
  `~/.config/openwhisprflow/config.toml`, models `~/.local/share/openwhisprflow/models`
  (pinned by sha256 in `crates/owf-core/models.lock.toml`), `rejections.jsonl` in the
  state dir, socket/lock/port in `$XDG_RUNTIME_DIR`, autostart entry at
  `~/.config/autostart/openwhisprflow.desktop`. Debug capture writes to `~/owf` by default.

## Environment gotchas

- **`llama-server` needs an explicit ggml compute backend.** Arch's `llama-cpp` pulls only
  base `ggml`, which has no backend, and fails with ggml's opaque "no backends are loaded".
  `ggml-cpu` (or `ggml-vulkan`/`ggml-cuda`) must be installed. The Settings window's Setup
  pane checks this on first run, the same check `owf-ctl setup` used to run.
- **`gtk-layer-shell` is a build-time link dependency, not a runtime check.** `src-tauri`'s
  `Cargo.toml` pulls in `gtk-layer-shell = { version = "0.8", features = ["v0_6"] }` for
  `layer.rs`. Without the system library, `cargo build` (or `cargo test`/`cargo clippy` —
  anything touching this crate) fails outright in `gtk-layer-shell-sys`'s build script,
  unable to find `gtk-layer-shell-0` via `pkg-config`. On Arch: `sudo pacman -S
  gtk-layer-shell`. Unlike the `ggml-cpu` gotcha above, there is no way to defer this to a
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
- **Do not trust `cpal`'s advertised sample-rate range.** It advertised 16 kHz on hardware
  that rejected the stream build; `capture.rs` now probes by building a throwaway stream and
  falls back to 48 kHz plus `rubato` resampling.
- **Never apply Hyprland config on the user's behalf** — emit it (`openwhisprflow
  --print-shortcuts`) and let them paste it. A bad window rule breaks their desktop.
  `hypr.rs` detects Lua vs classic `.conf`; Hyprland 0.56+ with a Lua config rejects the
  legacy keyword parser outright, so the formats are not interchangeable.
- **Real dictation is still unverified end to end** (no one has spoken into it; see
  `HANDOVER.md`). Do not record from the microphone without explicit permission — that
  constraint is why the audio half remains untested.
