# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Push-to-talk dictation for Hyprland/Wayland. Hold `SUPER+D`, speak, release; the audio is
captured, VAD-trimmed, transcribed (Parakeet TDT via `sherpa-onnx`), rewritten by a local
S1-mini `llama-server`, checked by a guardrail, and typed into the focused window with
`wtype`. Fully local at dictation time.

`README.md` is the user-facing setup guide. `HANDOVER.md` is the current state-of-play,
including what has and has not been verified on real hardware.

## Commands

```bash
# Daemon + CLI (one binary: `owf-ctl daemon`, `owf-ctl ptt-start`, `owf-ctl bench`, ...)
cargo build --release -p owf-cli          # builds owf-ctl

# Settings window (Tauri app, binary name `openwhisprflow-settings`)
cargo build --release -p openwhisprflow-settings --features custom-protocol
#   ^ same `custom-protocol` trap as the overlay below.

# Overlay (Tauri app, binary name `openwhisprflow`)
bun run tauri dev                         # dev: Vite on :1420 + the Tauri window
cargo build --release -p openwhisprflow --features custom-protocol
#   ^ `custom-protocol` is deliberately NOT a default feature. Without it a plain
#     cargo build embeds `devUrl` instead of `frontendDist` and the window is blank
#     unless a Vite dev server is running.
bun run build                             # frontend only (tsc && vite build -> dist/)
#   ^ builds BOTH pages: index.html (overlay) and settings.html (settings window).

# Tests (306, all offline; 4 are #[ignore]d because they need downloaded models)
cargo test --workspace
cargo test --workspace -- --ignored       # needs `owf-ctl setup` to have run first
cargo test -p owf-core guardrail::        # one module
cargo test -p owf-core --test pipeline_e2e a_good_cleanup_is_injected
cargo test -p owf-cli --lib daemon::tests::state_of_and_snapshot_event_report_failed_as_a_real_error_state
cargo clippy --workspace --all-targets    # kept clean

# Exercising things without a microphone
cargo run -p openwhisprflow -- --replay src-tauri/fixtures/replay-full.ndjson
cargo run -p owf-core --example list_devices
cargo run --release -p owf-cli --bin owf-ctl -- bench   # ASR latency table

# Runtime inspection (against a running daemon)
owf-ctl status | owf-ctl debug | owf-ctl subscribe | owf-ctl reload
owf-ctl setup [--update-lock | --print-hypr | --purge-logs]
owf-ctl daemon | owf-ctl bench | owf-ctl settings
```

There is no CI. `cargo test --workspace && cargo clippy --workspace --all-targets` is the
gate. Tags `known-good-m1` / `known-good-m2` are safety points to reset to.

## Architecture

Four Cargo members, three processes:

- **`crates/owf-core`** — the library. Every pipeline stage plus config, paths, wire
  format, model provisioning, debug capture. Links `sherpa-onnx`, `cpal`, `rubato`, so
  anything that depends on it inherits a heavy build.
- **`crates/owf-cli`** — one binary, `owf-ctl`, with the daemon (long-lived; owns
  the models and the socket), the client a Hyprland keybind runs on press/release,
  and the benchmark as subcommands. The three used to be separate binaries; they
  are now `src/{daemon,ctl,bench}.rs` behind `lib.rs`'s pure `route()`, whose
  tests pin that every invocation the old `owf-ctl` accepted still resolves to
  the same action — a compositor keybinding is not something to break silently.
- **`src-tauri`** (package `openwhisprflow`) — the overlay window. A *read-only* socket
  client. It intentionally does **not** depend on `owf-core`; see below.
- **`settings-tauri`** (package `openwhisprflow-settings`) — the settings window.
  A second Tauri *app*, not a second window of the overlay app: every window of a
  Tauri app shares one class, and the overlay's class carries a Hyprland
  `no_focus` rule, so a settings form inside it could not accept a keystroke.
  Verified with `hyprctl clients`: this app's class is `openwhisprflow-settings`,
  which the overlay rules do not match. Also a read/write socket client with no
  `owf-core` dependency; the config crosses as JSON.
- **`src/`** — the React frontend for *both* Tauri apps, built as two Vite entry
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

Wayland has no global keyboard grab, which is why the split exists: the compositor runs
`owf-ctl ptt-start` / `ptt-stop`, which writes one NDJSON request line to
`$XDG_RUNTIME_DIR/openwhisprflow.sock` and reads one response line back
(`owf-core/src/proto.rs`). `Request::Subscribe` is the exception: it converts the
connection into an open-ended NDJSON `OverlayEvent` stream, starting with a snapshot of
the current state. `daemon.rs`'s `handle` special-cases it before `dispatch`.

The daemon's state machine is an `AtomicU8` with consts at the top of `daemon.rs`
(`WARMING`, `IDLE`, `RECORDING`, `TRANSCRIBING`, `NORMALIZING`, `INJECTING`, `FAILED`);
`proto::State` / `proto::OverlayEvent` are the wire projections of it. A single utterance
runs through `pipeline::Pipeline::process_with_capture`.

`llama-server` is spawned once at daemon start and supervised (`spawn_housekeeping` /
`supervise_llama_once`: 10 s health poll, 1→30 s backoff restart, zombie reaping). A dead
`llama-server` degrades to `UnavailableNormalizer` rather than failing the daemon.

## Invariants worth knowing before editing

1. **Once ASR has produced text, the user gets text.** Normalization erroring, timing out,
   or being rejected by the guardrail all fall back to the raw transcript (or a rule-based
   cleanup of it). See `pipeline.rs`'s module doc and spec §15. Never add a path that can
   lose a transcribed utterance.
2. **The overlay must never take keyboard focus** — a focused overlay means `wtype` types
   the dictation into the overlay instead of the target window. Enforced twice, on purpose:
   `tauri.conf.json` (`focus`/`focusable: false`) and the compositor rules in `hypr.rs`.
3. **`OverlayEvent` exists in three hand-maintained copies**: `owf-core/src/proto.rs`
   (source of truth), `src-tauri/src/wire.rs` (mirror — duplicated so the overlay doesn't
   pull `sherpa-onnx`/`cpal` into its build graph), and the TS union in `src/Overlay.tsx`.
   Changing or adding a variant means touching all three *and* adding a line to
   `src-tauri/fixtures/replay-full.ndjson` — two tests cross-check drift mechanically
   (`the_overlay_replay_fixture_parses_as_this_crates_overlay_event` in `proto.rs`,
   `checked_in_fixture_covers_every_event_kind` in `replay.rs`).
4. **Config uses `#[serde(deny_unknown_fields)]` everywhere**, so an unrecognised key or
   section is a hard daemon-startup failure. Documenting a `config.toml` section without
   adding the matching struct bricks boot — that is what `OverlayConfig` exists to prevent.
5. **Client-side window positioning is a no-op under Hyprland.** `xdg_shell` has no
   client-settable toplevel position; `position_overlay` in `src-tauri/src/lib.rs` returns
   `Ok(())` and nothing moves. Placement can only come from a compositor window rule.
6. **Every subprocess call goes through `procutil::run_with_timeout`.** A hung `wtype` or
   `hyprctl` previously wedged the daemon's single-threaded accept loop forever.
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

## Conventions

- Code comments cite **`spec N`** / **`spec §N`**, meaning section numbers in
  `docs/superpowers/specs/2026-08-27-openwhisprflow-design.md`. Keep referencing it that
  way; implementation plans live in `docs/superpowers/plans/`, and per-task decision logs
  in `.superpowers/sdd/*/progress.md`.
- Tests are named as full sentences. Documented-but-unfixed behaviour is pinned by a test
  prefixed `known_limitation_` (e.g. `known_limitation_dense_digit_sequences_trip_word_ratio`
  in `guardrail.rs`).
- `Pipeline` gains test seams as builder methods (`with_rejections_path`,
  `with_recovery_dir`, `with_fallback_injector`, `with_stage_events`), not new positional
  parameters on `new`. `rejections_file()` is a live guardrail-tuning dataset — tests must
  redirect writes to it or they poison real data.
- `owf-core`'s `test-util` feature exposes `LlamaServer::from_child` so `owf-cli`'s tests
  can stub a process that cannot run here. Test builds only.
- All filesystem locations come from `owf-core/src/paths.rs` (XDG): config
  `~/.config/openwhisprflow/config.toml`, models `~/.local/share/openwhisprflow/models`
  (pinned by sha256 in `crates/owf-core/models.lock.toml`), `rejections.jsonl` in the
  state dir, socket/lock/port in `$XDG_RUNTIME_DIR`. Debug capture writes to `~/owf` by
  default.

## Environment gotchas

- **`llama-server` needs an explicit ggml compute backend.** Arch's `llama-cpp` pulls only
  base `ggml`, which has no backend, and fails with ggml's opaque "no backends are loaded".
  `ggml-cpu` (or `ggml-vulkan`/`ggml-cuda`) must be installed. `owf-ctl setup` checks this.
- **Do not trust `cpal`'s advertised sample-rate range.** It advertised 16 kHz on hardware
  that rejected the stream build; `capture.rs` now probes by building a throwaway stream and
  falls back to 48 kHz plus `rubato` resampling.
- **Never apply Hyprland config on the user's behalf** — emit it (`owf-ctl setup
  --print-hypr`) and let them paste it. A bad window rule breaks their desktop. `hypr.rs`
  detects Lua vs classic `.conf`; Hyprland 0.56+ with a Lua config rejects the legacy
  keyword parser outright, so the formats are not interchangeable.
- **Real dictation is still unverified end to end** (no one has spoken into it; see
  `HANDOVER.md`). Do not record from the microphone without explicit permission — that
  constraint is why the audio half remains untested.
