# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Press-to-start, press-to-stop dictation for Hyprland/Wayland. Press `SUPER+D`, speak,
press `SUPER+D` again; the audio is captured, VAD-trimmed, transcribed (one of several
`sherpa-onnx` models, chosen by `[asr] model`; Parakeet TDT 0.6b v3 by default),
rewritten by S1-mini (llama.cpp, in-process), checked by a guardrail, and
typed into the focused window with `wtype` (or, if `[inject] backend` selects it,
handed to a *user-supplied script* — a `script` backend since 2026-09-09, replacing the
`ydotool` one: yappr runs `[inject] script` with the finished transcript as `$1` and
passes nothing else, so the script owns the clipboard, the paste chord and any window
detection; a failure of any kind falls back to the clipboard, per invariant 1). Fully
local at dictation time. `SUPER+ALT+D`
cancels a recording in progress; nothing else can end one deliberately — see invariant 11.
`[inject] paste_chord` and `[inject] terminal_classes` still load, but nothing reads
them: choosing a chord needed a window class yappr cannot always get, which is what
retired the old backend — see the window-class gotcha at the bottom of this file.

yappr is one binary, `yappr`, and one process. Running it with no
arguments starts everything: a tray icon (no window), the Unix socket, the models, and
the pipeline. Left-clicking the tray (or `yappr --settings`) opens the settings
window; right-clicking gives Status / Einstellungen / Diktat pausieren / Beenden. There is
no `owf-ctl`, no separate daemon binary, and no systemd unit.

The app was called **OpenWhisprFlow** until 2026-08-29, and the rename reached the
binary, the crate (`owf-core` → `yappr-core`), the Tauri identifier, and the whole
XDG namespace (`~/.config/yappr`, which the config file has since left for
`~/.local/state/yappr` — see invariant 9; `~/.local/share/yappr/models`, `yappr.sock`,
`~/yappr` for debug capture). Two places deliberately still say `openwhisprflow`,
and both are correct: the design docs and SDD logs described below, which are dated
records of what was decided under the old name, and the migration text in `hypr.rs`
plus README's upgrade section, which name the *old* binary and window title because
their whole job is telling a user which dead lines to delete. Watch for that
distinction when editing either — a blanket rename through them makes them false.
`owf-ctl`, `owf-cli` and `owf-daemon` in comments are the same case: deleted crates
that genuinely had those names, not stale spellings.

`README.md` is the user-facing setup guide. `docs/HANDOVER.md` is the current state-of-play,
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

# Tests (470 passed, 0 failed, 11 #[ignore]d because they need downloaded models)
cargo test --workspace
# NOT optional. These are the only tests that catch a C++ ABI mismatch between
# sherpa-onnx and llama.cpp -- see the gotcha at the bottom of this file. A wrong
# `CXXFLAGS` aborts the process inside the VAD, and the default run notices nothing.
cargo test --workspace -- --ignored --test-threads=1   # needs models on disk (Settings' Setup pane, or --update-lock)
#   ^ `--test-threads=1` is not optional in practice. Run in parallel, several of
#     these load S1-mini (~480 MB) and an ASR model at once and
#     `llama::engine_tests::the_normalizer_trait_cleans_up_a_transcript_end_to_end`
#     intermittently fails at its `.expect` -- `NormalizeConfig::default()`'s
#     `timeout_ms` elapsing under the contention, not a real regression. It passes
#     alone and all 11 pass serially. Pre-existing; see docs/HANDOVER.md.
cargo test -p yappr-core guardrail::        # one module
cargo test -p yappr-core --test pipeline_e2e a_good_cleanup_is_injected
cargo test -p yappr-core --lib server::tests::state_of_and_snapshot_event_report_failed_as_a_real_error_state
cargo clippy --workspace --all-targets    # kept clean

# Exercising things without a microphone
cargo run -p yappr -- --replay src-tauri/fixtures/replay-full.ndjson
cargo run -p yappr-core --example list_devices
cargo run -p yappr-core --example script_probe -- "hi"                    # injection only, no mic
cargo run -p yappr-core --example script_probe -- "hi" ~/bin/paste.sh    # override the path
#   ^ runs your real paste script, which presses real keys into the focused window and
#     will replace your clipboard; prints the program, its argv[1] and what it exited
#     with. Replaced `paste_probe` on 2026-09-09 -- yappr no longer chooses a chord, so
#     there is no chord left to probe.
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
  Colour lives in two files of its own. `palettes.css` holds every theme as a set
  of `--p-*` ramp slots and is the only place a colour is named; `Settings.css`
  and `Overlay.css` each map that ramp onto their own semantic tokens exactly
  once, so a seventh theme is ~25 hexes rather than a second stylesheet. `theme.ts`
  owns the `data-theme`/`data-appearance` attributes and is the one place `[ui]
  theme = "system"` stops being a possibility and becomes a palette — see
  invariant 15.

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

`[asr] model` picks which speech-recognition model runs, from the catalogue in
`models.rs` (`ASR_MODELS`). **Provisioning follows the selection rather than
requiring every artifact**: `required_artifacts` is the support pair (Silero,
S1-mini) plus the one selected model, and `verify` / `looks_present` /
`download_all` all take that list. A model left on disk by an earlier selection
is never verified, redownloaded or deleted, so switching back is instant and
offline. `all_artifacts()` is the whole catalogue and is for name/URL lookups
and the lock-file completeness test only — never for "what must be present".
`--update-lock` is the one caller that legitimately wants all of it, because an
unpinned entry is one `download_all` refuses to install.

Two flavours sit behind `asr::build`: `Offline` (sherpa's `OfflineRecognizer`,
as always) and `CacheAwareStreaming` (`OnlineRecognizer`, fed the whole
utterance at once — yappr has no streaming UI, and the nemotron export is
published no other way). The streaming flavour **must be fed silence on both
sides** or it silently drops the start and end of the utterance; see
`SILENCE_PADDING`'s comment in `asr.rs` for the measured evidence, and note
that the leading half appears in no upstream example. An `Artifact::name` is a
key in `models.lock.toml` and therefore must be a TOML *bare* key — no dots, or
the whole lock file stops parsing. See
`docs/superpowers/specs/2026-09-08-asr-model-selection-design.md`.

## Invariants worth knowing before editing

1. **Once ASR has produced text, the user gets text.** Normalization erroring, timing out,
   or being rejected by the guardrail all fall back to the raw transcript (or a rule-based
   cleanup of it). See `pipeline.rs`'s module doc and spec §15. Never add a path that can
   lose a transcribed utterance.
2. **The overlay must never take keyboard focus** — a focused overlay means the injector
   (`wtype`, and any paste script that presses keys — both follow keyboard focus) types the dictation into
   the overlay instead of the target window. Enforcement is per-compositor, and only the
   layer-shell mechanism is airtight:
   - The **overlay** always carries Tauri's `focus: false` / `focusable: false`
     (`tauri.conf.json`) — but know what that buys: it reaches GTK's `accept_focus`, an
     **X11 mechanism that is inert on Wayland** (xdg_shell gives a toplevel no way to
     refuse focus). It protects X11/XWayland only. The settings window is
     `focus: true` / `focusable: true` and must stay that way -- it hosts a
     form and the setup wizard, neither of which could take a keystroke
     otherwise. That asymmetry is why the fallback rule below matches title.
   - On a compositor implementing `wlr-layer-shell` (every wlroots compositor, including
     Hyprland; not Mutter), `src-tauri/src/layer.rs`'s `anchor_overlay` puts
     the overlay on a layer-shell surface with `KeyboardMode::None` — focus refused at the
     protocol level, no compositor window rule involved. This is the real protection on
     Wayland.
   - On Hyprland, `hypr.rs`'s emitted window rule is belt-and-braces — keyed on the
     overlay's **title** (`"yappr overlay"`), not its class. The settings window
     became a window of this same app, so a class-matched rule would reach it too and it
     could no longer take a keystroke — exactly the regression the previous two-Tauri-app
     split existed to avoid, now fixed by matching title instead.
   - Where layer-shell is unavailable (GNOME/Mutter, or `anchor_overlay` failing for any
     other reason), **none of the above holds** — verified on Fedora/GNOME: Mutter
     focuses the overlay when it maps, and the whole dictation was typed into the HUD.
     The enforcement there is at injection time instead:
     `TauriSink::hide_overlay_if_it_hijacks_injection` (`src-tauri/src/lib.rs`) checks
     `is_focused()` on the `Injecting` broadcast, hides a focused overlay natively, and
     blocks the pipeline thread `FOCUS_RETURN_DELAY` (300 ms) so the compositor returns
     focus to the target before the injector spawns. `Overlay.tsx`'s `injecting` case
     deliberately never calls `showOnce()` and resets its `visible` ref to match — do
     not "fix" either back.
3. **`OverlayEvent` exists in two hand-maintained copies**: `yappr-core/src/proto.rs` (source
   of truth) and the TS union in `src/Overlay.tsx`. `src-tauri/src/wire.rs`'s separate
   mirror is gone — the overlay now links `yappr-core` directly (same process as the server),
   so there is nothing left to avoid pulling `sherpa-onnx`/`cpal` into by duplicating the
   type. Changing or adding a variant still means touching both *and* adding a line to
   `src-tauri/fixtures/replay-full.ndjson` — two tests cross-check drift mechanically
   (`the_overlay_replay_fixture_parses_as_this_crates_overlay_event` in `proto.rs`,
   `checked_in_fixture_covers_every_event_kind` in `replay.rs`).
4. **Config uses `#[serde(deny_unknown_fields)]` everywhere, and an unrecognised key
   is quarantined rather than fatal.** Documenting a `config.toml` section without
   adding the matching struct still costs the user their settings — that is what
   `OverlayConfig` exists to prevent — but it no longer bricks boot.
   `config::load_or_quarantine` is used by `server::start` and by nothing else: it
   renames the unreadable file to `config.toml.broken-<unix seconds>`, writes a fresh
   default, and hands the reason back so `Status` and `GetConfig` can report it as
   `Response::config_notice` and the settings window can show it. Two bad starts in
   the same second get distinct names; that file is the user's only copy.
   Every *other* `Config::load_from` caller — `Reload`, `GetConfig`, `SetConfig`,
   `ensure_models_loaded`, `setup.rs`, `wizard.rs` — stays strict on purpose. They
   have a user who asked and a socket to answer on, and a `Reload` that silently reset
   someone's settings would be the worst possible reading of "never fail"
   (`reload_reports_a_broken_config_instead_of_quarantining_it`).
   Why this changed: `server::start` `?`d on the load and `setup()` calls it, so one
   typo meant no tray and no settings window — leaving a text editor as the only
   repair tool for the one file the app has stopped inviting anyone to edit. See
   `docs/superpowers/specs/2026-09-02-app-owned-config-design.md`.
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
   `hyprctl` previously wedged the server's single-threaded accept loop forever. The
   helper drains stdout/stderr on threads and takes what they have collected
   `PIPE_DRAIN_GRACE` after the child exits, instead of reading the pipes to EOF: a
   child that forks a daemon (`wl-copy` does, on every successful copy) leaves a
   grandchild holding the pipes open, and `read_to_end` on that blocked the pipeline
   thread at `INJECTING` until the clipboard was next replaced — the ydotool paste
   backend never reached `ydotool` at all. This is now the *script* backend's ordinary
   case, not a corner one: a paste script that restores the previous clipboard does it
   from a backgrounded `nohup … &`, so the grandchild is there on every successful
   dictation. Pinned twice:
   `a_child_that_exits_but_leaves_a_grandchild_holding_its_pipes_does_not_block` in
   `procutil.rs`, and
   `a_paste_script_that_backgrounds_its_clipboard_restore_does_not_block` in
   `inject.rs`.
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
9. **`config.toml` is the app's file, and the settings GUI is how it changes.**
   It lives at `~/.local/state/yappr/config.toml` (not `~/.config`) and is
   rendered whole from `Config` by `config::render` — a fixed two-line header plus
   `toml::to_string_pretty`. Nothing is invited to edit it by hand; a hand edit
   loads fine and is overwritten by the next save.
   The no-op guarantee survives, and is now free: `render` is a pure function of
   `Config`, so an unchanged config cannot produce two different files. That
   replaced ~400 lines of `toml_edit` merging, comment `decor` carrying and
   unchanged-leaf skipping which existed only to protect a hand-written file.
   **`config_write::save_config` still has to apply a *patch*, not just render.**
   `Request::SetConfig` passes its JSON straight through and `wizard_finish`'s
   `backend_patch` is a single leaf, so `incoming` is routinely partial: the base
   is loaded as a `Config` and the patch merged into it as JSON (objects recurse,
   arrays replace wholesale, `null` removes a key — which is how a `StyleRule`'s
   unset axes stay expressible in a format with no null). Render `incoming`
   directly and the first wizard-driven save blanks the whole config. Then
   validate-then-atomic-rename, unchanged: that property is about crash-safety,
   not hand-editing.
   `[normalize]`'s two accepted-and-ignored keys (`port`, `llama_server_path`) are
   `#[serde(skip_serializing)]`: read so a pre-existing file still loads, never
   written, so a canonical dump does not push them into every user's file.
   This is what lets the GUI autosave at
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

13. **A self-restart must spawn the successor *after* `shutdown`, and
    `AppHandle::restart` must never be used.** `Request::Restart` is
    `Request::Quit`'s arm plus one call -- `EventSink::relaunch`, placed
    between `wait_for_busy_to_clear_then_shutdown` and
    `std::process::exit(0)` -- and that position is the whole feature, not a
    detail. `shutdown` is what unlinks `$XDG_RUNTIME_DIR/yappr.lock`, and
    `server::start`'s single-instance guard takes an exclusive `flock` on it
    and `exit(1)`s with "yappr is already running" if it cannot. So a
    successor started any earlier loses the lock and dies, and then the
    parent exits too: the app closes for good. Tauri's own
    `AppHandle::restart` does exactly that -- read
    `tauri-2.11.5/src/process.rs:83-88`, it spawns and *then* `exit(0)`s --
    which is why this is hand-rolled, and why `lib.rs`'s `RunEvent::Exit`
    hook (written when nothing called `exit`/`restart`) is still not the
    thing that runs on this path. `TauriSink::relaunch` resolves the binary
    with `tauri::process::current_binary`, not `std::env::current_exe`:
    under an AppImage the latter names a path inside a mount that is torn
    down as this process exits. It passes **no** arguments, so a `--replay`
    session cannot resurrect itself as a second replay. Pinned by
    `restart_starts_the_successor_only_after_shutdown_released_the_lock`,
    which asserts the ordering through a spy sink rather than the mere fact
    of the call. Note also that `restart_reason` takes `models_loaded`: with
    nothing resident, an `[asr]`/`[normalize]` change needs no restart at all
    (`ensure_models_loaded` re-reads the file on the next press), and since
    `preload_at_startup` defaults to `false` that is the ordinary case --
    a dialog that ignored this would nag almost every user for nothing.

14. **The first-run wizard never writes desktop config, on either desktop.**
    Hyprland is the standing rule (a bad window rule breaks the desktop).
    GNOME is the same rule for a different reason: `gsettings set ...
    custom-keybindings` *replaces* the list, so the obvious one-liner destroys
    every custom shortcut the user already had -- `desktop.rs`'s
    `GNOME_GSETTINGS_SNIPPET` reads the current value and appends, and the
    wizard shows it rather than running it.

    **The wizard marker is the only input to whether the wizard opens** --
    at startup and as a take-over of the settings form, one rule
    (`wizard.rs`'s `should_open`, the `should_open` field on the wire).
    Finished once, never again by itself, however broken the install is.
    It briefly had a second, sufficient reason (`setup_status()` not ready),
    and both stages of removing it are worth knowing, because the obvious
    "surface a deleted model immediately" instinct is what put it there:
    - **2026-09-09, first pass:** the two questions were split, because one
      unready model had the wizard *replace* the settings window on every
      launch, with the only way out three clicks away at the last step's
      "Einstellungen öffnen" -- the settings window became unreachable over
      an errand that takes one click. (That is also why the models step
      carries an "Einstellungen" button when the marker exists.)
    - **Same day, second pass:** `!ready` stopped opening the window at all.
      It is false for a missing prerequisite *binary* and for a hash
      mismatch too -- neither of which the wizard has a button for, and
      `ydotoold` not *running* is not even checkable -- so it reopened
      forever over gaps it could not close, while its models step said
      "Alle Modelle sind vorhanden". Reported as exactly that
      contradiction.
    The warning moved rather than went away: `build_wizard_state` passes
    `provision::setup_status`'s whole answer through as `setup`, and
    `Settings.tsx` renders it as a banner **naming the missing packages and
    models** (`setupGapSummary`), with the wizard one click behind it,
    opened at the models step. `provision::status_or_assume_incomplete` is
    that path's error case -- `ready: false` with both lists empty, which
    the banner reports as an unknown cause rather than blaming a model it
    never checked. Nothing else would tell the form that provisioning is
    incomplete, and nothing pops a window for it: seeing it costs opening
    the settings window, which is what the tray is for.

    The marker is a state file, not a config key: a key would have to join a
    `deny_unknown_fields` struct and then appear in the settings GUI as a
    setting nobody should touch. See
    `docs/superpowers/specs/2026-08-29-first-run-wizard-design.md`.

15. **A theme is chosen, so nothing may key on `prefers-color-scheme` any more —
    and a theme selector outranks a media query.** `[ui] theme` (default
    `system`) is one of seven values; six are palettes in `src/palettes.css`,
    and `system` is resolved to `yappr-light`/`yappr-dark` by `theme.ts`
    *before it reaches the DOM*, so CSS never sees it. Three things here break
    silently:
    - **Specificity.** `:root[data-theme="x"]` is (0,1,1); `:root` inside
      `@media (prefers-contrast: more)` is (0,1,0) and **loses to it**. Written
      the obvious way, every pinned theme would switch the high-contrast palette
      off for anyone not on `system`. Both windows' contrast blocks are therefore
      keyed `:root[data-appearance="light"|"dark"]` — a tie, broken by document
      order, which is why they must stay *after* the `palettes.css` import.
      `palettes.css`'s own appearance blocks come before its theme blocks for the
      same reason: that ordering is what lets `yappr-dark` keep the exact grain
      and shadow it shipped with while Mocha inherits the generic dark ones.
    - **Appearance is no longer the desktop's opinion.** Four rules used to key
      on `prefers-color-scheme` (the dark token block, the grain's
      `multiply`→`screen` flip, the brand plate's drop-shadow, the
      high-contrast-dark palette) and every one was wrong the moment Latte could
      be pinned on a dark desktop. Anything that varies by appearance rather than
      by palette is a slot in `palettes.css` (`--p-grain-blend`,
      `--p-mark-shadow`) or is keyed on `[data-appearance]`. Do not add a fifth.
    - **A missing slot renders nothing, not a wrong colour.** `var(--p-x)` with
      nothing behind it is an invalid value, so the property drops out and the
      surface gets no background or no text colour at all — invisible to `tsc`
      and to `vite build`. The theme list itself lives in four places (`Theme::ALL`,
      `palettes.css`, `theme.ts`'s `APPEARANCE`, `schema.ts`'s `ENUMS`/`ENUM_LABELS`).
      Both hazards are checked mechanically by
      `themes_are_declared_everywhere_they_have_to_be` and
      `every_palette_declares_every_slot_the_windows_read` in `src-tauri`, which
      read the frontend files off disk — the same trick as the overlay-event
      fixture tests. Verified to fail on drift, not just to pass.

    Two further things that are decisions, not accidents. The **overlay is themed
    too**, so a light theme means a *light* capsule — which reverses the
    "deliberately dark in both appearances" rationale in `Overlay.css`'s header,
    and is why light palettes carry a harder `--p-hud-edge` and heavier
    `--p-hud-shadow` to separate from a light wallpaper. And the **borrowed light
    palettes are not upstream's**: neither Catppuccin Latte nor Tokyo Night Day
    clears AA as small text on its own base, so every ink role is stepped toward
    black/white in 2% increments until it clears 4.5:1 (hue untouched), the same
    trade the shipped light palette already makes with the brand cyan. Surfaces
    and accent fills are upstream's untouched. `yappr-light`/`yappr-dark` and the
    dark capsule are byte-identical to what shipped before themes existed — that
    was checked against `git HEAD`, and it is the property that makes this a safe
    upgrade for someone who never asked for a theme.

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
  `~/.local/state/yappr/config.toml` (moved out of `~/.config` on 2026-09-02, with a
  one-time `config::migrate_from_legacy`; `paths::legacy_config_file()` is the old
  location and is read once per start and never written), models
  `~/.local/share/yappr/models`
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
- **The app ships its own type, and `public/fonts/*.woff2` is generated — regenerate it
  with `scripts/build-fonts.sh`, never by hand.** Three roles (`--font-display`,
  `--font-ui`, `--font-mono`), declared once in `src/fonts.css`, which both windows
  `@import`; they used to live in `Settings.css` while the overlay carried an unrelated
  `system-ui` stack, which is how the two windows came to disagree. The stacks still name
  system families behind the bundled ones, but only as a per-glyph net for a character
  outside the subset — not as the plan, which is what they were until the fonts were
  bundled. Two things there are not guessable. **The subset is not "all of Latin"**: the
  script's header records what each range costs in bytes and why Latin Extended-B, the
  combining block, Greek, Cyrillic and Vietnamese are all deliberately out, so widening it
  is a decision with a measured price rather than a free `+=`. And **iA Writer Duo is
  deliberately converted but never subset**, because it declares the Reserved Font Names
  "iA Writer" and "Plex": OFL-FAQ 2.6 makes glyph removal a modification, which forfeits
  an RFN, while 2.7/2.8 let a container change keep it. Adding `--unicodes` to that one
  call obliges you to rewrite the font's internal name records to a name of our own
  (OFL-FAQ 3.1) — i.e. to fork someone else's typeface. Adwaita Sans and Adwaita Mono
  declare no RFN and are subset freely. `--name-IDs='*'` on every call keeps the
  copyright and licence records inside each file; `public/fonts/OFL-*.txt` carries the
  full texts.

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
- **An AppImage built directly on this (Arch) machine runs on rolling-release hosts
  only — a portable one is built in the `yappr-build` distrobox.** glibc is backwards-
  but never forwards-compatible, so a binary linked against Arch's glibc demands symbol
  versions Fedora/Debian/Ubuntu don't have yet; the release CI builds in `ubuntu:22.04`
  (glibc 2.35, the oldest base that still ships webkit2gtk-4.1 — Tauri's own
  recommendation) for exactly this reason. The local equivalent is a distrobox on the
  same image (docker backend), sharing `$HOME`, so the host's rustup toolchain and
  mise-installed bun are used as-is:

  ```bash
  distrobox create --yes --name yappr-build --image ubuntu:22.04
  distrobox enter yappr-build -- sudo apt-get install -y --no-install-recommends \
      build-essential pkg-config cmake clang libclang-dev curl wget ca-certificates \
      git file unzip xz-utils xdg-utils libwebkit2gtk-4.1-dev libgtk-3-dev \
      libgtk-layer-shell-dev librsvg2-dev libssl-dev libxdo-dev \
      libayatana-appindicator3-dev libasound2-dev     # same list as .gitlab-ci.yml
  distrobox enter yappr-build -- sh -c 'cd ~/Work/JS_TS/OpenWhisprFlow \
      && export PATH="$HOME/.cargo/bin:$HOME/.local/share/mise/installs/bun/latest/bin:$PATH" \
      && CARGO_TARGET_DIR=$PWD/target-portable NO_STRIP=1 APPIMAGE_EXTRACT_AND_RUN=1 \
         bun run tauri build --bundles appimage'
  # then, on the HOST, both fix scripts against
  # target-portable/release/bundle/appimage/*.AppImage
  ```

  Three things in there are load-bearing. `CARGO_TARGET_DIR=target-portable`: cargo does
  not fingerprint the C/C++ toolchain, so sharing `target/` with host builds silently
  reuses Arch-compiled llama.cpp/sherpa-onnx objects and produces a "portable" binary
  that still needs Arch's glibc. `APPIMAGE_EXTRACT_AND_RUN=1`: no FUSE inside the
  container, same as CI. `NO_STRIP=1`: as documented below. The gdk-pixbuf `.pc` shadow
  hack is NOT needed in the box — that breakage is Arch-specific; Ubuntu 22.04's `.pc`
  still tells the truth. Verified 2026-09-01: worst symbol requirement across every
  bundled ELF is `GLIBC_2.35` (check: extract the AppImage, `objdump -T` everything,
  grep `GLIBC_[0-9.]+`, `sort -uV`).
- **A freshly built AppImage has a 32×32 root icon; run `scripts/fix-appimage-icon.sh`
  on it after every build.** The linuxdeploy build Tauri pins (1-alpha, 659c9db) has no
  icon size preference: it overwrites the root `yappr.png` Tauri placed (the 512×512)
  with a symlink to the first hicolor icon it lists — the 32×32 — and `.DirIcon` points
  there. That root icon is what desktop integrators (Gear Lever, appimaged) extract for
  the app menu, so without the fix every user gets a 32 px menu icon scaled up. The
  script repacks the AppImage in place with the 256×256 at the root (the AppImage spec's
  recommended `.DirIcon` size), reusing the original runtime; verified 2026-09-01. The
  release CI runs it after `tauri build` — a locally built AppImage needs it run by hand.
- **Every way a window-class provider can fail collapses to the same `None`, and the
  per-application style rules then fall back to their defaults.**
  `winclass::active_window_class()` is the *only* source of the target window's class,
  and since 2026-09-09 `style::resolve` is its only consumer — a `None` means no
  `[[style_rules]]` entry matches and S1-mini is prompted with `[style_default]`
  instead. That is a quiet wrong-tone, not a lost dictation.

  It used to be much worse, and the table below is the measurement that retired the
  ydotool backend: `inject::wants_shift` read `None` as "not a terminal", so yappr
  pressed plain Ctrl+V, which every terminal ignores — and `ydotool` exited 0, so
  `inject_with_recovery` recorded success, no clipboard-fallback notification fired,
  and the user saw a dictation that produced no text. yappr no longer chooses a chord
  at all; a paste script asks its own desktop. Measured against Hyprland 0.56.2 on
  2026-09-07, the three ways to get that `None` are **not** interchangeable and only
  one of them is a clean exit:

  | Situation | stdout | exit |
  |---|---|---|
  | `HYPRLAND_INSTANCE_SIGNATURE` unset (systemd unit, `.desktop` autostart) | `HYPRLAND_INSTANCE_SIGNATURE not set! (is hyprland running?)` | 1 |
  | signature set but stale — socket unreachable | `Couldn't connect to …/.socket.sock. (4)` | 4 |
  | Hyprland fine, nothing focused | `{}` | 0 |
  | not Hyprland at all (GNOME/Mutter) | — | spawn fails |

  Note `hyprctl` reports its own failures on **stdout**, not stderr, which is why
  `hypr::window_class` reads stdout *before* the status check and attaches it to
  the failure warning — the text naming the cause is only in hand there. The
  spawn-failure arm is deliberately `debug!`, not `warn!`: `start_recording` calls
  this on every utterance with no compositor guard, so on GNOME a warning would fire
  once per dictation forever, for every user, including the majority on `wtype` for
  whom the class changes nothing.

  GNOME used to be the case that mattered most — `wtype` does nothing there and
  `hyprctl` can never exist on it, so the chord's `auto` could never work. Since
  2026-09-08 the class is answerable there: `gnome.rs` answers from the accessibility
  bus, which needs no extension and no gsetting, and `winclass::active_window_class`
  falls back to it when `hyprctl` cannot be run. That provider now serves the style
  rules rather than a chord, but it is the same code and the same failure modes.

  Read that module's header before touching it. Three things there are not guessable.
  **AT-SPI delivers no `window:activate` events on this desktop** -- registration
  succeeds and nothing ever arrives, measured with this crate and with an independent
  `pyatspi` listener, with `toolkit-accessibility` both off and on -- so it *polls*
  the tree (a sweep costs ~7 ms; `POLL_INTERVAL` is 1 s, raised from a measured 500 ms
  that drew 2.3-2.6% of a core). An event-driven version
  was written first and fails viciously: its one startup sweep succeeds, so the class
  freezes on whatever was focused then, dictation keeps working in that window and
  silently stops working in every other one. **And it cannot query at `ptt-start`
  either**, because by then the overlay holds focus itself (invariant 2), so the
  answer has to be already in hand.

  **The application's own accessible name is not always one.** GTK answers `Unnamed`
  for any application that never called `g_set_application_name`, and ghostty is one:
  on GNOME it arrived as the class `Unnamed`, so no style rule written for it could
  fire, every other silent GTK application shared that same class, and the debug
  record named none of them. It surfaced on the retired ydotool backend, where the
  same `Unnamed` picked the paste chord and dictation into ghostty produced no text
  at all. So `Unnamed` is treated as no name at all (it can
  never be *recorded* either, whatever reports it), and the focused application is
  identified from the pid the accessibility bus keeps for its peer
  (`GetConnectionUnixProcessID`) — resolved through `/proc/<pid>/cmdline`'s `argv[0]`,
  not `comm`, which the kernel truncates to 15 bytes and would turn
  `gnome-text-editor` into `gnome-text-edit`. That lands on `ghostty`, the name every
  other provider gives it. The lookup runs only for the application
  already found focused, so a sweep costs no extra round trip in the ordinary case.
  Measured on GNOME Shell 50.0 / Fedora 44, 2026-09-10.

  `InjectDebug` still records `window_class` (written as `null` rather than omitted,
  so "unknown" is distinguishable from "old record"), which is how you tell a style
  rule that did not fire from one that fired wrong. An Electron or Qt app that
  registers with no accessibility bus is the setup that still answers `None` on GNOME.
  The chord measurements this note used to end on — `/dev/input/event*` reads of the
  `ydotoold virtual device`, verified end to end in kitty, ghostty and foot — belong
  to the retired backend and are kept only in git history.
- **A failed backend may say why on *stdout*, so `run_backend` records both streams.**
  `inject::diagnostic` joins them, stderr first, so a backend that reports failures
  the usual way reads exactly as it always did. The case that forced it: a paste
  script's `ydotool` prints "failed to connect socket ...: No such file or directory /
  Please check if ydotoold is running." on stdout and exits 2, stderr empty — the
  debug record then said `script exited with status exit status: 2: ` and named
  neither a cause nor anything to fix. `hyprctl` has the same habit (see the stdout
  note above), so this is the second time the same trap has been paid for.
  The underlying misconfiguration is worth recognising too, because a script inherits
  it silently: `ydotoold` takes `--socket-path`, the *client* looks at
  `$YDOTOOL_SOCKET` and else `$XDG_RUNTIME_DIR/.ydotool_socket`, and a unit that
  starts the daemon anywhere else (`%h/.ydotool_socket` is a widely copy-pasted
  example) breaks every paste. Nothing in the app can fix that from its side: yappr
  starts from the tray or a `.desktop` entry and inherits no shell export, so the
  daemon has to listen where the client looks. Measured on Fedora 44, 2026-09-10.
- **Do not trust `cpal`'s advertised sample-rate range.** It advertised 16 kHz on hardware
  that rejected the stream build; `capture.rs` now probes by building a throwaway stream and
  falls back to 48 kHz plus `rubato` resampling.
- **Never apply Hyprland config on the user's behalf** — emit it (`yappr
  --print-shortcuts`) and let them paste it. A bad window rule breaks their desktop.
  `hypr.rs` detects Lua vs classic `.conf`; Hyprland 0.56+ with a Lua config rejects the
  legacy keyword parser outright, so the formats are not interchangeable.
- **Real dictation was first verified end to end on 2026-09-07**, on Fedora 44/GNOME 50
  with the `ydotool` backend: spoken German, transcribed, normalised and pasted into
  Ptyxis and GNOME Text Editor. `docs/HANDOVER.md` predates that and still says
  otherwise. Still do not record from the microphone without explicit permission.
