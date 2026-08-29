# Handover — the one-process rewrite

**This replaces the previous `HANDOVER.md`**, which was a letter about a single night's
work on the old three-process architecture (the capture-rate bug, the M2 overlay). That
system no longer exists in this repo, so that letter stopped being true days ago. This one
describes what 17 tasks changed, what's verified, and — the part that matters most, since
there is exactly one user — exactly what to do on *this* machine to cut over.

**Nothing has touched your running system.** No task in this effort started the new app,
bound the runtime socket, removed a runtime file, registered a tray item, opened the
microphone, or edited anything under `~/.config/hypr/`. Everything below is either a test
result, a `--replay` run, or a description of files this session only *read*.

## The state of this exact machine, right now

Two old-architecture processes are still running, from before this rewrite:

- `owf-ctl daemon` (PID varies — `pgrep -fa 'owf-ctl daemon'`) — the old two-binary daemon.
- `openwhisprflow` (PID varies — `pgrep -fa '/openwhisprflow$'`) — the old, overlay-only M2
  build. Both were started by `~/.config/hypr/autostart.lua`'s two `o.launch_on_start` lines
  at your last login, and both are the *old* code — this session has not rebuilt or
  restarted them.

`~/.config/hypr/bindings.lua`, `autostart.lua`, and `windows.lua` on this machine still
have the exact old-architecture lines the design doc describes as the worst-case migration
target: `owf-ctl ptt-start`/`ptt-stop`/`cancel` bindings, two `launch_on_start` lines, and a
class-matched `o.window("openwhisprflow", {...})` rule. This isn't a hypothetical example —
it's what's actually on disk here, read (not edited) for this handover.

An AppImage was built and smoke-tested earlier in this effort:
`~/Downloads/openwhisprflow_0.1.0_amd64.AppImage` (114 MB). It answered `--status` correctly
against the *old* running daemon, which only proves the new binary's client fast path speaks
the same wire protocol — it has not been run as the app itself, and this session did not
re-touch it.

## Do this first — cutting over

1. **Stop the old processes.**
   ```bash
   pkill -f 'owf-ctl daemon'
   pkill -x openwhisprflow          # not -f '/openwhisprflow$'
   ```
   `-x` on the process name, not `-f` on the command line: a shortcut that runs the bare
   name through `PATH` gives a command line of `openwhisprflow --toggle`, which the
   `/openwhisprflow$` pattern does not match — the anchor wants an absolute path and no
   arguments. That miss is how an old overlay survives a cutover you believe you completed.
   The old daemon's `llama-server` child should exit with it (see CLAUDE.md invariant
   6-adjacent supervision code); check `pgrep llama-server` afterward and kill it directly
   if it didn't.

2. **Install the new build.** Either run the AppImage above, or build from source —
   `README.md`'s Install section, which now needs only `cargo build --release -p
   openwhisprflow --features custom-protocol` plus `install`. Then delete every binary
   that no longer exists:
   ```bash
   rm -f ~/.local/bin/owf-ctl ~/.local/bin/owf-daemon ~/.local/bin/owf-bench \
         ~/.local/bin/openwhisprflow-settings
   ```
   `~/.local/bin/openwhisprflow` is missing from that list on purpose — it is the one name
   the rewrite kept — but it is the leftover that actually bites, and it did: **observed on
   this machine on 2026-08-29, two overlays on screen at once.** The pre-rewrite binary of
   that name parses no arguments, so a `SUPER+D` bound to the bare name resolved through
   `PATH` to the old build, which opened its own overlay, subscribed to the *new* daemon's
   socket, and rendered a second pill from the same events — same waveform, same timer, no
   error anywhere. Overwrite it with `install`, or symlink it at the AppImage, and then run
   `which -a openwhisprflow` (expect one path) and `openwhisprflow --status` (expect one
   line of JSON — a stale build prints nothing and opens a window). See README's
   "Upgrading from an older, multi-binary build".

3. **Edit your three Hyprland files** (this session did not do this for you — see the
   standing rule about never touching your compositor config):
   - `bindings.lua`: delete the three `o.bind` lines calling `owf-ctl ptt-start` /
     `ptt-stop` / `cancel`. Add the two lines `openwhisprflow --print-shortcuts` prints.
   - `autostart.lua`: delete both `o.launch_on_start` lines. There is no replacement line —
     autostart is now the Settings window's "Beim Anmelden starten" toggle (step 5).
   - `windows.lua`: delete the `o.window("openwhisprflow", {...})` block. On Hyprland you
     do not need to paste a replacement — the overlay now positions and unfocuses itself
     via `wlr-layer-shell`. (If you'd rather have the belt-and-braces fallback rule too,
     `--print-shortcuts` prints a title-matched one; it's harmless either way.)

   `openwhisprflow --print-shortcuts`'s output leads with exactly what to delete, in that
   order, before what to add — this is tested (`hypr.rs`'s
   `the_emitted_block_names_the_lines_to_delete_before_the_ones_to_add`).

4. **Reload and check:**
   ```bash
   hyprctl reload && hyprctl configerrors
   ```

5. **Launch the app** (from an app launcher, if you added the `.desktop` entry from
   `README.md`, or `openwhisprflow &`). You should see a tray icon and no window. Left-click
   it — Settings should open. Since the models are already downloaded from before, the
   Setup pane should not appear; if it does, something about the on-disk models changed and
   it will say what.

6. **Turn on "Beim Anmelden starten"** in Settings if you want the old autostart behaviour
   back — it now writes `~/.config/autostart/openwhisprflow.desktop` instead of a Hyprland
   line.

7. **Press `SUPER+D`.** This is press-to-start, press-to-stop now, not hold-to-talk: press
   once to start recording, speak, press again to stop and transcribe. `SUPER+ALT+D`
   cancels. Nothing ends a recording on its own except `audio.max_seconds` (120 s default)
   if you forget the second press — see CLAUDE.md invariant 11.

## What changed

| | |
|---|---|
| **Process topology** | Three processes (daemon, overlay, settings) become one binary, `openwhisprflow`, hosting the pipeline, socket, tray, overlay, and settings window in a single process. `crates/owf-cli` and `settings-tauri` are deleted. |
| **Dictation gesture** | Hold-to-talk (`ptt-start`/`ptt-stop` on press/release) becomes press-to-start/press-to-stop (`--toggle`, resolved against the server's current state). `--cancel` is unchanged as a separate shortcut. |
| **The tray** | New. A native StatusNotifierItem (`ksni`, not Tauri's own tray feature — appindicator-only hosts don't send click events). Left-click opens Settings; right-click gives Status / Einstellungen / Diktat pausieren / Beenden. |
| **Pausing** | New. `PAUSED` is a real state; `--toggle` is refused and no microphone opens while paused. Never interrupts an utterance already in flight. |
| **First run** | `owf-ctl setup` is gone. A Setup pane in the Settings window now provisions models and checks prerequisites (same checks, driven from the GUI). |
| **Autostart** | An XDG `.desktop` entry written by a Settings toggle, not a Hyprland `exec-once` line. No systemd unit is authored by this project. |
| **The overlay** | Self-anchors bottom-centre and refuses focus at the protocol level via `wlr-layer-shell`, on Hyprland and other wlroots compositors — no window rule needed there. Falls back to an unpositioned toplevel plus an emitted, **title**-matched (not class-matched) window rule on GNOME/Mutter. |
| **The settings window** | Still a second window, now of the *same* Tauri app rather than a second Tauri app — its GUI code (`src/settings/`, `Settings.tsx`) is unchanged; only the transport under it changed from a socket to direct Tauri commands. |
| **`OverlayEvent`** | Down to two hand-maintained copies (`owf-core/src/proto.rs`, `src/Overlay.tsx`) from three — `src-tauri/src/wire.rs` is gone now that the overlay links `owf-core` directly. |

## What I verified myself

- `cargo test --workspace`: **371 passed, 0 failed, 4 ignored** (the pre-existing 370, plus
  one new test this task added — see below). `cargo clippy --workspace --all-targets`:
  clean.
- The exact migration lines this machine needs, by reading its actual
  `~/.config/hypr/{bindings,autostart,windows}.lua` side by side with
  `shortcut_config()`'s output — not from the spec's example or from memory.
- That `shortcut_config()`'s emitted text genuinely leads with what to delete before what
  to add, and that the classic `.conf` format still names the `bindr` line that has no
  replacement — added a test for this (`hypr.rs`'s
  `the_emitted_block_names_the_lines_to_delete_before_the_ones_to_add`) after confirming,
  by actually running it, that the brief's own suggested version (calling `shortcut_config()`
  rather than the two format constants directly) fails on this very machine — it has
  `hyprland.lua`, so `shortcut_config()` returns the Lua variant, and the Lua text never
  contains the string `"bindr"` (only the *classic* format's migration comment does, since
  `bindr` is that format's own name for the bind pair's release edge). Fixed by testing both
  constants directly instead of the machine-dependent dispatcher.
- That the two new environment gotchas below (`gtk-layer-shell` at build time, the AppImage
  `gdk-pixbuf` `.pc` issue) are accurately described, by reading the code and the earlier
  tasks' own reports of hitting them live on this machine. I did not re-trigger either
  myself — see below.

## What I could NOT verify (and why)

- **Real dictation, end to end.** Still never done by a human on this project. Unchanged by
  this rewrite, and I did not open a microphone to test it — forbidden by this project's
  standing rule, independent of this task.
- **Whether left-click on the tray actually sends `Activate` on this machine's tray host.**
  The code handles both answers (if it doesn't, Einstellungen is the context menu's first,
  actionable item), but nobody has clicked the tray icon of the *new*, one-process build —
  doing that myself would mean starting a second instance while the old one still owns the
  socket and tray slot, which is exactly what this task's constraints forbid.
- **Whether `wlr-layer-shell` actually anchors the overlay bottom-centre with no window rule,
  live, on this Hyprland session.** Same reason: starting the app was off-limits here.
- **Whether the AppImage at `~/Downloads/openwhisprflow_0.1.0_amd64.AppImage` still runs
  correctly today.** It was smoke-tested via `--status` (against the old daemon) when it was
  built; I did not re-run it for this task.
- **Whether the emitted shortcut block, actually pasted into this machine's real config,
  reloads clean under `hyprctl configerrors`.** I compared the emitted text against the
  machine's current config by reading both; pasting and reloading edits your live compositor
  config, which no task in this project may do without you doing it yourself.

## Judgement calls

- **Replaced this file rather than appending to it.** The previous version was a letter
  about the M1→M2 capture-rate bug on a system that no longer exists; CLAUDE.md calls this
  file "the current state-of-play," which stopped being true the moment the one-process
  rewrite started, well before this task. Nothing in it described anything still running.
- **Adapted the brief's example test** (see "What I verified myself" above) rather than
  implementing it verbatim, because I could show, on this exact machine, that the verbatim
  version fails for a reason unrelated to the thing it's supposed to check.

---

## Added after this letter: `ydotool` as a third injector

`[inject] backend` now accepts `"ydotool"` alongside `"wtype"` and `"clipboard"`, selectable
from Settings → Allgemein → Texteingabe → Verfahren. Spec 10.3 planned this and deferred it;
it is no longer deferred. `wtype` is still the default — it needs no setup, and `ydotool`
needs a running `ydotoold` plus write access to `/dev/uinput`, which README's "Typing with
`ydotool`" section documents rather than automates.

- `cargo test --workspace`: **377 passed, 0 failed, 4 ignored** (371 before, plus six new).
  `cargo clippy --workspace --all-targets`: clean. Frontend `bun run build`: clean.
- **Found and fixed a pre-existing config-writer bug on the way.** `toml_edit`'s
  `TableLike::insert` calls `Key::fmt()`, which resets the key's decor, so
  `set_preserving_decor` — which only carried the *value*'s decor across — deleted every
  comment written on its own line *above* a setting the first time the GUI saved that
  setting. In the shipped `DEFAULT_CONFIG_TOML` that included `# Cleanup runs on S1-mini by
  Superwhisper.`, directly above `[normalize] enabled`: toggling normalization once from the
  GUI silently removed an attribution the spec requires. Pinned by
  `changing_a_value_keeps_the_comment_lines_above_it`.
- **Not verified on hardware:** nobody has typed anything with `ydotool` from this app.
  `ydotool` is not installed on this machine, so the argv shape (`ydotool type --key-delay
  <ms> -- <text>`) was checked against ydotool's man page, not against the binary — the same
  way `wtype`'s `--` handling was originally specified, and spec 10.2's acceptance test
  (`inject("-- hello -x")` produces exactly `-- hello -x`) has an unrun `ydotool` twin. If it
  turns out `ydotool type` rejects `--`, the fix is `--file -` on stdin, which its man page
  also documents; do not invent an escaping scheme.

---

## Added after this letter: the models follow the dictation

`[models] preload_at_startup` (default `false`) and `idle_unload_seconds`
(default `60`) replace "every model resident for the life of the process".
Design: `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.
Invariant 12 in CLAUDE.md is the part to read before editing `server.rs`.

- `cargo test --workspace`: **410 passed, 0 failed, 4 ignored** (377 before,
  plus 33 new). `cargo clippy --workspace --all-targets`: clean. Frontend
  `bun run build`: clean.
- **Verified on this machine, in an isolated instance** (an `XDG_RUNTIME_DIR`
  and config override; the real config, `rejections.jsonl`, and the running
  production daemon were all confirmed untouched by mtime afterwards):
  - `preload_at_startup = true` with `idle_unload_seconds = 20`: models
    loaded, `llama-server` up, then at +20 s `--status` reported `"warm":
    false` and `llama-server` was gone.
  - It stayed gone for a further 67 s — six or seven housekeeping ticks —
    proving the supervisor gate holds. The daemon's own log shows no further
    "llama-server spawned" line.
  - A separate run with `idle_unload_seconds = 25` unloaded at +25-26 s,
    which confirms the deadline shrink is real rather than a coincidence of
    the 10 s health-poll interval.
- **Not verified: the cold-start latency of a real dictation.** The
  press-triggered load specifically was exercised only by tests, never on the
  real binary, because `--toggle` opens the microphone and CLAUDE.md forbids
  recording without explicit permission — which was not given for this work.
  So the cold-start latency of a real first dictation is unmeasured: the
  number that matters is how long `run_utterance` blocks in
  `ensure_models_loaded` after a short utterance, and `--status`'s `last_ms`
  does not include it. This is on top of the pre-existing fact that real
  dictation has never been verified end to end at all.
- **Memory, measured on the same isolated instance:**

  | State | Main process | `llama-server` |
  |---|---|---|
  | Never loaded (nothing ever pressed) | ~216 MB | — |
  | Warm | ~1.24 GB | ~955 MB |
  | After the idle unload | ~826 MB | gone entirely |

  ~1.37 GB is returned to the OS by an unload. The ~955 MB is unambiguous — a
  real process exit. The main process itself only drops ~411 MB, though, and
  settles ~610 MB above its never-loaded baseline rather than back down to
  it. `smaps_rollup` showed that residual over 97% anonymous, and a second
  load/unload cycle peaked *lower* (980-1000 MB) than the first (1062-1065
  MB) — so it reads as glibc holding freed memory rather than returning it to
  the kernel, reused by the next load, not a leak and not a live model
  reference. **Left unsettled, not resolved:** across those same two cycles,
  the idle floor *after* unload crept up ~150-200 MB even though the peak did
  not grow. Two cycles is not enough to call that either way; worth watching
  if this gets measured again.
- **Judgement call:** a lazy load failure returns to `IDLE` instead of
  latching `FAILED`. The startup path still latches, because a failure
  discovered at startup is a different thing from one discovered on the
  user's third dictation. This makes a fresh install recover from the Setup
  pane without a restart, which the old behaviour did not.
- **Known issues, deliberately not fixed:**
  - **A hung `llama-server` can hold the first dictation for a long time.**
    `load_models` waits on `spawn_and_wait_healthy` with the 120 s
    `STARTUP_HEALTH_TIMEOUT` *before* building ASR/VAD, and a failed
    background load is not shared with the blocking caller, so the worst case
    is roughly double that. Under the old design this budget was spent at
    startup; lazily it lands on a dictation. Left alone: it only bites when
    `llama-server` is already broken, a case `UnavailableNormalizer` already
    treats as a degraded mode, and shortening the budget on the lazy path is
    a design decision for the spec rather than a review fix.
  - **`cargo test` disturbs a running daemon's port file.** `LlamaServer::drop`
    unconditionally removes `paths::runtime_port()`, and several tests
    construct stub children. Pre-existing, not introduced here; fixing it
    means changing `LlamaServer::drop`.
  - **One race test synchronises with a 100 ms sleep** rather than a
    deterministic handshake. It can only ever produce a false negative
    (missing a regression on a loaded machine), never a flaky failure, but a
    handshake would be strictly better.
