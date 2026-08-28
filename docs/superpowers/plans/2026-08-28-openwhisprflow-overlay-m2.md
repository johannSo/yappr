# OpenWhisprFlow M2: Overlay and Supervision — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** The user can see what the dictation pipeline is doing while it does it, and the daemon survives a `llama-server` crash.

**Architecture:** The daemon gains a broadcast channel of `OverlayState` events. A Tauri app (`src-tauri`, currently an untouched scaffold excluded from the workspace) becomes a second binary that subscribes to those events over the existing Unix socket and renders a small always-on-top pill. The overlay holds no pipeline logic; it is a view.

**Spec:** `docs/superpowers/specs/2026-08-27-openwhisprflow-design.md` — §12 (overlay), §6.1 (states), §5.1 (supervision), §15 (error matrix).

**Baseline:** `main` at tag `known-good-m1`, 137 tests, clippy clean.

## A lesson from M1 that changes how this plan is written

M1's plan carried complete reference code for every task. **That code was wrong more than a dozen times** — non-compiling `sha2`/`cpal`/`rubato`/`httpmock` calls, an integer underflow that crashed a live download, VAD defaults that classified silence as speech, two test fixtures that could never pass, and window-rule syntax that does not exist on the target compositor.

So this plan specifies **intent, interfaces, and acceptance criteria precisely, and reference code sparingly.** Where an external API is involved, the task says *verify it against the installed crate/source* rather than pasting what I remember. Implementers are expected to check and to report what they found.

## Global Constraints

- **No async runtime in non-dev dependencies.** Tauri brings its own; it must stay confined to `src-tauri` and must not leak into `owf-core` or `owf-cli`.
- **The overlay must never take keyboard focus.** If it does, `wtype` types the dictation into the overlay instead of the user's target window. The verified Omarchy/Lua form is `o.window("openwhisprflow", { no_focus = true })`; **verify any additional rule against `/usr/share/hypr/stubs/hl.meta.lua` and `$OMARCHY_PATH/default/hypr/windows.lua` on the running machine — do not write window-rule syntax from memory.** Hyprland 0.56+ under Lua rejects the legacy keyword parser entirely.
- **The overlay is a view.** No pipeline logic in the frontend; it renders state it is told about.
- Audio is 16 kHz mono f32 by the time it leaves `capture.rs`.
- Once audio has been transcribed, the user gets text. Nothing in M2 may weaken that.
- S1-mini's license requires "S1-mini" by "Superwhisper" with that exact capitalization in user-facing text — including anything the overlay renders.
- Target is Linux + Wayland + Hyprland only.

## Verification reality

**No microphone testing is possible during this milestone's implementation** — the machine is locked and unattended, and recording audio in an unattended room is not acceptable regardless. Every task must be verifiable by unit/integration test, by driving the daemon socket directly, or by rendering the overlay against synthetic state. Tasks that genuinely need a human speaking are marked and deferred to a handover checklist.

---

### Task 1: Daemon state broadcast

**Files:** `crates/owf-core/src/proto.rs`, `crates/owf-cli/src/bin/owf-daemon.rs`

**Produces:** `proto::OverlayEvent` (serde, NDJSON-serializable) covering every state in spec §12: `Warming`, `Idle`, `Recording { level: f32, elapsed_ms: u64 }`, `Transcribing`, `Normalizing`, `Injecting`, `Done { preview: String }`, `Error { reason: String }`, `BusyRejected`. Plus a `Request::Subscribe` that turns a socket connection into a long-lived event stream.

Currently `state_of` collapses everything after `ptt-stop` into `Transcribing`, and `State::Normalizing` / `State::Injecting` are constructed only in a serialization test. Spec §6.1 requires them to be real. The pipeline already computes per-stage `Timings`; the daemon must emit a transition as each stage begins.

- [ ] Write failing tests: every `OverlayEvent` round-trips through serde; a subscriber receives events in pipeline order for one synthetic utterance; a subscriber that disconnects mid-stream does not affect the pipeline or wedge the accept loop.
- [ ] Implement. **The accept loop is single-threaded by deliberate design** — that is what makes the plain `store` in `PttStart` safe. A long-lived subscriber connection must NOT block it; give subscribers their own thread or a non-blocking fan-out, and say in your report which you chose and why it preserves the loop's single-threaded invariant.
- [ ] Wire `Recorder::start`'s `on_level` callback (currently `|_level| {}`) to emit `Recording` events at roughly 50 ms cadence per spec §7.1. Do not emit per-audio-callback; that would be ~100/s at 48 kHz.
- [ ] Verify: `owf-ctl subscribe` (or equivalent) prints a live event stream while you drive `ptt-start`/`cancel` over the socket. Paste the transcript.
- [ ] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` green. Commit.

---

### Task 2: `last_ms` and the diagnostics that were built but never wired

**Files:** `crates/owf-core/src/proto.rs`, `crates/owf-cli/src/bin/owf-daemon.rs`

`Response.last_ms` is declared and never populated. `Outcome.timings` is computed and read by nothing in production. `Outcome`'s `raw`, `normalized`, `reject_reason`, `backend` are likewise producer-only. A final review flagged all of this as dead code with a live producer.

- [ ] Failing test: after one synthetic utterance, `status` reports per-stage `last_ms` matching the `Timings` the pipeline produced.
- [ ] Implement; keep `Response`'s existing shape backward-compatible (fields are `skip_serializing_if = "Option::is_none"`).
- [ ] Verify against the running daemon. Commit.

---

### Task 3: `llama-server` supervision (spec §15, §5.1)

**Files:** `crates/owf-core/src/llama.rs`, `crates/owf-cli/src/bin/owf-daemon.rs`

`LlamaServer::is_healthy` has **zero callers**. Spec §15 requires backoff restart 1→30 s on crash; §5.1 requires a `/health` poll every 10 s while `Idle`. Neither exists: if `llama-server` dies mid-session, every later utterance silently falls back to raw ASR forever with no restart and no signal.

- [ ] Failing test: a supervised handle whose child exits is restarted; backoff grows 1→2→4→8→16→30 and caps; a healthy child is not restarted. Use a stub child process (the existing `LlamaServer::from_child` behind the `test-util` feature is the seam) rather than a real `llama-server`.
- [ ] Implement supervision on its own thread. It must not hold the pipeline mutex, must not fight the `SIGTERM` shutdown path (which already kills and reaps), and must be idempotent against a shutdown racing a restart.
- [ ] The overlay must be able to show a degraded badge, so emit an `OverlayEvent` when normalization becomes unavailable and when it recovers.
- [ ] Verify: kill the real `llama-server` out from under the running daemon; it comes back, and `status` reflects the gap. Paste the log.
- [ ] Commit.

---

### Task 4: Re-admit `src-tauri` to the workspace

**Files:** `Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`

Ruling R1 excluded `src-tauri` for M1 because nothing used it and it forced full Tauri dependency resolution on every cargo invocation. M2 needs it.

- [ ] Move `src-tauri` from `exclude` to `members`. Confirm `cargo test -p owf-core` still works and report how much slower a cold `cargo` invocation becomes — if it is materially worse, say so; keeping the overlay as a separate non-member crate is an acceptable alternative.
- [ ] Tauri must not pull an async runtime into `owf-core`/`owf-cli`. Verify with `cargo tree -e normal -p owf-core` and paste the result.
- [ ] `cargo build --release` for all members succeeds. Commit.

---

### Task 5: The overlay window

**Files:** `src-tauri/src/*`, `src/*` (the React frontend), `src-tauri/tauri.conf.json`

Spec §12: 280 × 72 px, bottom-centre, transparent, undecorated, `skipTaskbar`, **never focused**, position configurable.

- [ ] Configure the Tauri window per spec. **Verify the current Tauri 2 API for transparency, always-on-top, decorations and focus behaviour against the installed crate version — do not write it from memory.**
- [ ] The overlay connects to the daemon socket, sends `Subscribe`, and renders each `OverlayEvent` per spec §12's table. Reconnect with backoff if the daemon is not up yet.
- [ ] Frontend holds no pipeline logic. States render as: warming spinner; hidden when idle; live bars + elapsed seconds while recording; stage labels for transcribing/cleaning; an 800 ms preview flash of the first ~60 characters on done; a red pill for 2 s on error; a 400 ms amber flash on busy rejection.
- [ ] **Verifiable without a microphone:** add a way to drive the overlay from synthetic events (a `--replay <file>` mode reading an NDJSON event log, or a debug socket command). Use it to screenshot every state and confirm the rendering. This is the acceptance evidence.
- [ ] Commit.

---

### Task 6: Focus rules, and proving the overlay cannot steal focus

**Files:** `crates/owf-core/src/hypr.rs`, `README.md`

This is the one that breaks dictation if it is wrong: a focused overlay means `wtype` types into the overlay.

- [ ] Determine the correct Lua window rules **by reading `/usr/share/hypr/stubs/hl.meta.lua` and `$OMARCHY_PATH/default/hypr/windows.lua` on this machine.** `no_focus = true` is verified; anything else must be verified the same way. Report exactly what you checked.
- [ ] Emit them from `hypr_config()` for the Lua path, and the `.conf` equivalent for classic installs. Tests must fail if unverified syntax reappears (the existing `neither_config_ships_unverified_window_rules` test will need updating — update it to assert the *verified* forms rather than deleting it).
- [ ] **Do not modify the user's Hyprland config.** Emit the snippet and document it; applying it is the user's call in the morning.
- [ ] Commit.

---

### Task 7: `--purge-logs`, and the reload gap

**Files:** `crates/owf-cli/src/bin/owf-ctl.rs`, `crates/owf-core/src/pipeline.rs`, `crates/owf-cli/src/bin/owf-daemon.rs`

Two small known gaps:
- Spec §5.1 names `owf-ctl setup --purge-logs` as the only way to clear the rejection dataset; it is not implemented.
- **Ruling R15:** `update_reloadable` swaps `cfg` wholesale, so toggling `normalize.enabled` from `false` to `true` and reloading flips the flag without rebuilding the normalizer. Every later utterance then calls the unavailable stub, errors, and silently falls back to raw text forever while the user believes normalization is back on.

- [ ] Failing tests for both.
- [ ] Implement `--purge-logs`. Implement the reload fix — either reject a reload that changes `normalize.enabled` with a clear message, or genuinely rebuild the normalizer. Prefer rebuilding if Task 3's supervision makes it cheap; say which you chose.
- [ ] Commit.

---

### Task 8: Handover checklist

**Files:** `README.md`

- [ ] Document the overlay: what each state looks like, the window rules to add, how to disable it.
- [ ] Write a **"verify in the morning"** section listing every check that needs a human and a microphone, with the exact commands: dictate a full sentence and confirm the captured-vs-expected sample ratio in `~/owf/logs/`; confirm the overlay appears and does not steal focus; confirm text lands in Alacritty, Firefox, an Electron app and an XWayland window.
- [ ] State plainly what was **not** verified overnight and why.
- [ ] Commit.

---

## What this plan does not cover

- Guardrail threshold tuning against real rejection data (M3 — needs real dictation logs).
- `YdotoolInjector` (spec §10.3).
- The `owf-ctl` crate split (ruling R12) — reconsider once Task 4 shows the real cost of Tauri in the workspace.
- The short-utterance guardrail hole and same-length clause substitution — both determined intrinsic to a bag-of-words guardrail; they need a different technique, not tuning.
