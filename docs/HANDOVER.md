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
   yappr --features custom-protocol` plus `install`. Then delete every binary
   that no longer exists:
   ```bash
   rm -f ~/.local/bin/owf-ctl ~/.local/bin/owf-daemon ~/.local/bin/owf-bench \
         ~/.local/bin/openwhisprflow ~/.local/bin/openwhisprflow-settings
   ```
   `~/.local/bin/openwhisprflow` is in that list now, and the rename is why. Before it,
   the one-process build kept the old overlay's name, and that shared name is the leftover
   that actually bit: **observed on this machine on 2026-08-29, two overlays on screen at
   once.** The pre-rewrite binary of that name parses no arguments, so a `SUPER+D` bound to
   the bare name resolved through `PATH` to the old build, which opened its own overlay,
   subscribed to the *new* daemon's socket, and rendered a second pill from the same events
   — same waveform, same timer, no error anywhere. Renaming the app to `yappr` retires that
   collision by construction: the new binary cannot shadow or be shadowed by the old one,
   because they no longer answer to the same name. What replaces the hazard is a quieter
   one — a `SUPER+D` still bound to `openwhisprflow` now runs a binary that either doesn't
   exist (nothing happens, silently) or is the stale pre-rewrite overlay (a ghost pill and
   no dictation). Deleting it, per the `rm` above, turns the second case into the first,
   and step 3 rebinds the shortcut. Afterwards: `which -a yappr` (expect one path),
   `which -a openwhisprflow` (expect none), and `yappr --status` (expect one line of JSON —
   a stale build prints nothing and opens a window). See README's "Upgrading from an older,
   multi-binary build".

3. **Edit your three Hyprland files** (this session did not do this for you — see the
   standing rule about never touching your compositor config):
   - `bindings.lua`: delete the three `o.bind` lines calling `owf-ctl ptt-start` /
     `ptt-stop` / `cancel`. Add the two lines `yappr --print-shortcuts` prints.
   - `autostart.lua`: delete both `o.launch_on_start` lines. There is no replacement line —
     autostart is now the Settings window's "Start at login" toggle (step 5).
   - `windows.lua`: delete the `o.window("openwhisprflow", {...})` block. On Hyprland you
     do not need to paste a replacement — the overlay now positions and unfocuses itself
     via `wlr-layer-shell`. (If you'd rather have the belt-and-braces fallback rule too,
     `--print-shortcuts` prints a title-matched one; it's harmless either way.)

   `yappr --print-shortcuts`'s output leads with exactly what to delete, in that
   order, before what to add — this is tested (`hypr.rs`'s
   `the_emitted_block_names_the_lines_to_delete_before_the_ones_to_add`).

4. **Reload and check:**
   ```bash
   hyprctl reload && hyprctl configerrors
   ```

5. **Launch the app** (from an app launcher, if you added the `.desktop` entry from
   `README.md`, or `yappr &`). You should see a tray icon and no window. Left-click
   it — Settings should open. Since the models are already downloaded from before, the
   Setup pane should not appear; if it does, something about the on-disk models changed and
   it will say what.

6. **Turn on "Start at login"** in Settings if you want the old autostart behaviour
   back — it now writes `~/.config/autostart/yappr.desktop` instead of a Hyprland
   line.

7. **Press `SUPER+D`.** This is press-to-start, press-to-stop now, not hold-to-talk: press
   once to start recording, speak, press again to stop and transcribe. `SUPER+ALT+D`
   cancels. Nothing ends a recording on its own except `audio.max_seconds` (120 s default)
   if you forget the second press — see CLAUDE.md invariant 11.

## What changed

| | |
|---|---|
| **Process topology** | Three processes (daemon, overlay, settings) become one binary, `yappr`, hosting the pipeline, socket, tray, overlay, and settings window in a single process. `crates/owf-cli` and `settings-tauri` are deleted. |
| **Dictation gesture** | Hold-to-talk (`ptt-start`/`ptt-stop` on press/release) becomes press-to-start/press-to-stop (`--toggle`, resolved against the server's current state). `--cancel` is unchanged as a separate shortcut. |
| **The tray** | New. A native StatusNotifierItem (`ksni`, not Tauri's own tray feature — appindicator-only hosts don't send click events). Left-click opens Settings; right-click gives Status / Settings / Setup / Pause dictation / Quit. |
| **Pausing** | New. `PAUSED` is a real state; `--toggle` is refused and no microphone opens while paused. Never interrupts an utterance already in flight. |
| **First run** | `owf-ctl setup` is gone. A Setup pane in the Settings window now provisions models and checks prerequisites (same checks, driven from the GUI). |
| **Autostart** | An XDG `.desktop` entry written by a Settings toggle, not a Hyprland `exec-once` line. No systemd unit is authored by this project. |
| **The overlay** | Self-anchors bottom-centre and refuses focus at the protocol level via `wlr-layer-shell`, on Hyprland and other wlroots compositors — no window rule needed there. Falls back to an unpositioned toplevel plus an emitted, **title**-matched (not class-matched) window rule on GNOME/Mutter. |
| **The settings window** | Still a second window, now of the *same* Tauri app rather than a second Tauri app — its GUI code (`src/settings/`, `Settings.tsx`) is unchanged; only the transport under it changed from a socket to direct Tauri commands. |
| **`OverlayEvent`** | Down to two hand-maintained copies (`yappr-core/src/proto.rs`, `src/Overlay.tsx`) from three — `src-tauri/src/wire.rs` is gone now that the overlay links `yappr-core` directly. |

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
  The code handles both answers (if it doesn't, Settings is the context menu's first,
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
from Settings → General → Text entry → Method. Spec 10.3 planned this and deferred it;
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

- `cargo test --workspace`: **415 passed, 0 failed, 4 ignored** (377 before,
  plus 33 for this feature and 5 more from the whole-branch review round).
  `cargo clippy --workspace --all-targets`: clean. Frontend `bun run build`:
  clean.
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
    - *Narrower than first recorded, and worse in one specific way.*
      `wait_healthy` calls `Child::try_wait` between its health polls, so a
      missing `llama-server` binary, a missing ggml compute backend (the
      Arch case in CLAUDE.md's environment gotchas), or a bad model path all
      fail in milliseconds rather than after 120 s. Spending the full budget
      needs a child that stays alive and never answers `/health` — a much
      rarer thing than "llama-server is broken". Against that: repeated
      cancel-then-press cycles queue loader threads on `load_lock`, so if a
      hang of that kind *does* happen, N presses serialise into N × 120 s
      rather than sharing one wait.
  - **`cargo test` disturbs a running daemon's port file.** `LlamaServer::drop`
    unconditionally removes `paths::runtime_port()`, and several tests
    construct stub children. Pre-existing, not introduced here; fixing it
    means changing `LlamaServer::drop`.
  - **One race test synchronises with a 100 ms sleep** rather than a
    deterministic handshake. It can only ever produce a false negative
    (missing a regression on a loaded machine), never a flaky failure, but a
    handshake would be strictly better.
  - **`[normalize] enabled = false` is only *partially* applied until a
    restart.** `restart_reason` already tells the user that `[normalize]`
    needs one, and `--reload` refuses the flip outright — but the file is
    written either way, so this is what a user who does not restart is
    running. The pipeline side then behaves correctly: the next lazy load
    builds `UnavailableNormalizer` and spawns no child. The housekeeping
    thread does not, because its `normalize_cfg` is a value captured at
    startup (`server.rs`'s `spawn_housekeeping` call in `start`) and never
    re-read. Its supervisor keeps deciding a `llama-server` ought to be
    running, finds `daemon.llama` empty, and respawns a ~955 MB child the
    pipeline will never call. The end state the user asked for is still
    correct — normalization really is off, the text is rule-cleaned and
    injected — what leaks is the memory this feature exists to reclaim.
    Restarting clears it, but so does the idle unload: the stray child is
    bounded by `idle_unload_seconds` unless that is set to `0`, since
    `unload_models` doesn't consult the stale `normalize_cfg` either — it
    just tears down whatever `daemon.llama` holds. Not fixed here: the fix
    is to give `spawn_housekeeping` a live view of `[normalize]` instead of
    a startup snapshot, which is a change to how that thread is configured,
    not a review-round patch.
  - **A stale RSS figure survives in one test comment.** The comment on
    `the_supervisor_is_skipped_entirely_while_the_models_are_unloaded`
    (`server.rs`) still says "a fresh 697 MB llama-server" — an earlier
    estimate, predating the measurement above. `~955 MB` is the measured
    figure and the one used everywhere else in this document and in
    CLAUDE.md's invariant 12. Left alone because it's comment-only
    staleness with no functional or test impact, same class as the
    `warm_up`-naming fix this task already made elsewhere in this file —
    flagging here so the next person editing that test doesn't propagate
    the wrong number.
  - **A model load still in flight when the process quits orphans its
    `llama-server` child.** `ensure_models_loaded_with` re-checks
    `SHUTTING_DOWN` after installing the pipeline, but every route into
    `shutdown` calls it and then immediately `std::process::exit(0)`, and
    nothing joins the detached thread `start_recording` spawns to run the
    load. So the re-check only ever fires for a load that finishes in the
    narrow gap between `kill_llama` and `exit(0)`; a load still inside
    `load_models` — which on every cold press means the ~3.5 s window
    between `llama-server`'s cold start (~750 ms in) and the ASR model
    finishing — is not reached at all before the process is gone. Press
    SUPER+D, cancel with SUPER+ALT+D, then tray → Quit, during that
    window, and a ~955 MB `llama-server` is left running; the next start is
    unaffected (`pick_port` just walks past it), so the only symptom is the
    memory this feature exists to reclaim never coming back. Not fixed
    here: a real fix needs either installing the `LlamaServer` into
    `daemon.llama` right after `spawn_and_wait_healthy` returns instead of
    after the ASR build finishes (the supervisor's `models_loaded` gate
    already keeps it inert until then), or setting `PR_SET_PDEATHSIG` on
    the child. See the comment on `ensure_models_loaded_with`'s `Ok` arm in
    `server.rs` for the full reasoning.
  - **`--reload` mirrors `[models]` into the daemon but not `[audio]`.**
    The same shape of gap as the `[normalize]` one above: a hand-edited
    `max_seconds` or `device` still needs a restart to take effect, because
    nothing re-populates `daemon.audio_cfg` from a reloaded config, while
    the Settings GUI's own save path applies such changes live. Pre-existing,
    not introduced by this branch; recorded here so it isn't rediscovered.

## Selectable ASR models (2026-09-08)

`[asr] model` chooses between three speech-recognition models; the dropdown is
in the settings window's Language pane and in the wizard's models step. Design:
`docs/superpowers/specs/2026-09-08-asr-model-selection-design.md`. Plan:
`docs/superpowers/plans/2026-09-08-asr-model-selection.md`.

Verified on this machine:

- **Both new models transcribe for real**, via
  `cargo test --workspace -- --ignored` against `fixtures/hallo_german.wav`.
  The full gate is green: 445 passed / 0 failed / 10 ignored, the 10 ignored
  all passing under `--ignored`, clippy clean.
- **`feat_dim` is 128 for both encoders**, read out of the ONNX metadata
  rather than inferred. The sherpa-onnx Rust crate defaults
  `OnlineRecognizerConfig::feat_config.feature_dim` to 80, and a wrong value
  here produces confident wrong text rather than an error.
- **The cache-aware streaming model needs silence fed on *both* sides.**
  Measured on the 2.75 s German fixture: no padding gives "Nur die Wurst",
  tail-only gives "Nur die Wurst hat zwei", lead+tail gives the whole
  sentence. The trailing half is what upstream's own example does; the
  *leading* half is in no upstream example and matters here because a
  cache-aware model spends its first chunk priming zeroed caches, and
  `SileroTrimmer` has already stripped the silence that would have absorbed
  it. Every real dictation hits this model with an abrupt start.
- **The language option does nothing observable on this clip.** `"de"`,
  `"auto"` and omitting the call entirely produced byte-identical output.
  `[asr] language` is still exposed (the model is `EncDecRNNTBPEModelWithPrompt`
  and multilingual), but do not assume it is load-bearing without measuring.

Not verified — still open:

- **No end-to-end dictation with a non-default model.** The `--ignored` tests
  drive `asr::build` against a fixture; nobody has yet pressed SUPER+D with
  `nemotron-3.5` selected and watched text land in a window.
- **The wizard and Language-pane dropdowns are built, not clicked.**
  `bun run build` passes for both entry points; the UI has not been exercised
  against a running daemon.
- ~~primeline-parakeet is not shipped.~~ **Shipped and verified end to end on
  2026-09-09.** Exported from primeline's `.nemo` with sherpa-onnx's own v3
  export script (`scripts/export-primeline-onnx.sh`), published CC-BY-4.0 at
  `Joni000000000/parakeet-primeline-sherpa-onnx-int8`, and installed *through
  `models::download_all`* rather than by hand: `required_artifacts` asked for
  exactly `[silero, s1-mini, parakeet-primeline-de]`, fetched only the
  486,631,039-byte tarball, passed `verify` against the committed pin, and
  transcribed the German fixture as "Alles hat ein Ende, nur die Wurst hat
  zwei." The published file was also re-downloaded anonymously (no token) to
  confirm it is publicly readable and hashes to `67adc0d8…`.

  One trap is recorded in the export script and worth repeating: **sherpa-onnx
  decides a model is a Token-and-Duration Transducer by substring-searching
  the `url` ONNX metadata field for "tdt"**. A TDT joiner emits
  `vocab_size + num_durations` outputs (8198 against 8193 tokens), so without
  that substring `OfflineRecognizer::create` rejects the model outright with
  `vocab_size: 8193 != output_size: 8198`. A provenance edit that removed it
  cost one full re-upload. The metadata check passed throughout; only
  transcribing real audio caught it.

## The restart prompt (2026-09-09)

A settings change that needs a restart now asks for one and performs it, instead
of printing a banner and leaving the user to run `yappr --quit` and start the app
again by hand. `Request::Restart` is `Request::Quit`'s arm plus one call:
`EventSink::relaunch`, between `shutdown` and `std::process::exit(0)`.
`TauriSink::relaunch` spawns the binary `pick_successor_binary` chose, with no
arguments. The settings window latches the requirement, shows a dialog once the
save settles, and keeps an actionable amber bar if the user picks **Later**.

Verified on this machine:

- **The full gate is green.** `cargo test --workspace`: 452 passed / 0 failed /
  11 ignored. `cargo clippy --workspace --all-targets`: clean. `bun run build`
  (`tsc` included): clean, both entry points.
- **The ordering that makes a restart a restart is pinned by a test.**
  `restart_starts_the_successor_only_after_shutdown_released_the_lock` asserts,
  through a spy sink that answers "was `yappr.lock` still there when you were
  called?", both that the relaunch happens after `shutdown` has unlinked it and
  that it does not happen at all while an utterance is still `TRANSCRIBING`.
- **Tauri's own `AppHandle::restart` cannot be used here, and would have failed
  silently in the worst way.** Read `tauri-2.11.5/src/process.rs:83-88`: it
  spawns the successor and *then* `exit(0)`s, so the successor starts while this
  process still holds the exclusive `flock` on `$XDG_RUNTIME_DIR/yappr.lock`
  (`server.rs`'s single-instance guard), loses it, prints "yappr is already
  running" and exits 1 — after which the parent exits too. A restart button that
  closes the app for good. This is why the relaunch is hand-rolled and why its
  position in the teardown is load-bearing rather than incidental.
- **`restart_reason` now consults `models_loaded`.** `[asr]`/`[normalize]` are
  read when the models are *built*, and `ensure_models_loaded` re-reads
  `config.toml` from disk at that moment — so with nothing resident the change
  lands on the next press and a restart buys nothing. The lazy-lifecycle design
  doc (2026-08-29 §1) already called the old unconditional answer "pessimistic
  — not wrong", which a passive pill could afford and a modal cannot: with
  `preload_at_startup` defaulting to `false`, an idle daemon is the ordinary
  case, so the dialog would have nagged nearly every user for nothing.
- **A real restart was performed on this desktop, twice.** Hyprland session, the
  debug binary, `{"cmd":"restart"}` over the socket. The log reads `shutting
  down` -> `restart: successor started pid=3111677
  path=.../target/debug/yappr` -> `listening` -> `ready`; afterwards exactly one
  yappr process exists, it is the new one, `yappr.lock` has a *different inode*
  than before (2416 -> 2418, which is the handoff working as designed rather
  than a reused file), and `{"cmd":"status"}` answers `{"ok":true,"state":"idle"}`.
- **The ksni tray item re-registers under the new PID.** After the restart,
  `org.kde.StatusNotifierWatcher`'s `RegisteredStatusNotifierItems` contains
  `org.kde.StatusNotifierItem-3111677-1` — the successor's PID. The
  predecessor's `unregister_tray` and the successor's registration landing
  close together does not wedge the item.
- **`restart_required` is true only when it should be.** Against a preloaded
  daemon (`warm: true`), `set-config` on `{"asr":{"num_threads":7}}` answers
  `restart_required: true` with the corrected reason; the same call with
  `{"vocabulary":{"terms":[...]}}` answers `restart_required: false`. Driven
  through a scratch `XDG_STATE_HOME` so the real `config.toml` was never
  touched (confirmed by hash afterwards).

**A bug found by that first live run, which no test would have caught.** The
first implementation used `tauri::process::current_binary`, and the restart
started */home/joni/AppImages/t3_code_nightly.appimage* — a completely unrelated
application — while yappr stayed down. `current_binary` returns `$APPIMAGE`
whenever that variable is merely *set*, and it is inherited like any other
environment variable, so a yappr launched from a terminal that is itself an
AppImage sees its ancestor's path. `pick_successor_binary` in `lib.rs` replaces
it: `$APPIMAGE` is trusted only when `current_exe` actually lives inside
`$APPDIR`, the mount of the AppImage whose payload is running. Three tests
cover it, including the inherited-variable case that shipped first. Do not
"simplify" this back to Tauri's helper.

Not verified — still open:

- **The dialog has been built, not seen.** `tsc` and `vite build` pass and the
  settings window opens and maps (749x814 under Hyprland), but `grim` cannot
  capture in this session at all — every invocation times out waiting for a
  screencopy frame, for whole outputs as well as regions — so nobody has looked
  at the scrim, the spring, the focus behaviour or the Escape handling. The
  logic behind it is verified from the daemon side; the pixels are not.
- **The overlay's layer-shell surface has not been re-anchored.** The restart
  test never mapped the overlay, because that needs a dictation and recording
  from the microphone was out of scope. `anchor_overlay` is an ordinary startup
  path, but it has not run twice in one session.
- **The AppImage path is reasoned and unit-tested, not run.** No AppImage has
  restarted itself from its own dialog. The unit tests pin the decision, not
  the behaviour of a real squashfs mount being torn down.

Pre-existing flake found while running the gate, unrelated to this change:

- **`llama::engine_tests::the_normalizer_trait_cleans_up_a_transcript_end_to_end`
  fails intermittently under `cargo test --workspace -- --ignored`.** It failed
  at `llama.rs:340` — the `.expect("normalizing through the trait")`, i.e.
  `normalize` returned `Err`, i.e. `NormalizeConfig::default()`'s `timeout_ms`
  elapsed. Passes alone, and all 11 ignored tests pass with
  `--test-threads=1`; the parallel run has several S1-mini and ASR loads
  contending at once. Not caused by this branch (none of this branch's tests
  even execute under `--ignored`), but worth knowing before the next person
  reads a red gate as a regression. The fix is a deadline that is not
  wall-clock, or serialising the model-loading tests.

---

## Added after this letter: the `script` backend replaces `ydotool` (2026-09-09)

**The `ydotool` injector documented above no longer exists.** `[inject] backend` now
accepts `"wtype"`, `"script"` and `"clipboard"`; `"ydotool"` survives only as a read-only
serde alias for `"script"`, so a pre-existing `config.toml` still loads (invariant 4 — an
unrecognised value would have `server::start` quarantine the user's only settings file)
and the next save rewrites it. The entries above are left as written: they are a dated
record of what was true then, and the "not verified on hardware" note in particular is
still the honest state of that backend — it was retired without ever having been proven
on this machine.

What replaced it: `[inject] script` names an executable, and `ScriptInjector` runs it with
the finished transcript as `argv[1]` and nothing else — no pre-copy to the clipboard, no
environment of its own, no paste chord. The chord is what killed the old backend: it
needed the focused window's class, every provider can answer `None`, and an unknown class
meant a plain Ctrl+V that terminals ignore — from a `ydotool` that exited 0, so nothing
failed, no fallback notification fired, and the user simply got no text.

`[inject] paste_chord` and `[inject] terminal_classes` are accepted-and-ignored keys now,
on the `[normalize] port` precedent: `#[serde(skip_serializing)]` and hidden by
`schema.ts`'s `OBSOLETE_FIELDS`. GNOME's recommended backend is `clipboard`, and
`prerequisites_for` no longer checks for `ydotool` on any desktop —
`wizard::backend_prereqs` returns an empty list everywhere and its wizard card is gone.

- `cargo test --workspace`: **473 passed, 0 failed, 11 ignored.** `cargo clippy
  --workspace --all-targets`: clean. `bun run build`: clean.
- `crates/yappr-core/examples/paste_probe.rs` is replaced by `script_probe.rs`: it runs
  the configured script for one line of text and prints the program, its `argv[1]` and
  the outcome. There is no chord left to probe.
- **Not verified on hardware, and this is the one to know.** The script path has been
  exercised end to end against shell fixtures only — argv shape, non-zero exit, missing
  file, tilde expansion, and (invariant 6) a script that backgrounds a grandchild holding
  its stdout open, which is what a clipboard-restoring paste script does on every run.
  Nobody has yet dictated into a real window through a real paste script. The reference
  script the design was built against is the user's `handy-paste.sh`, which needs GNOME's
  "Window Calls Extended" extension and a running `ydotoold`; neither is present on this
  Arch/Hyprland machine.

---

## Added after this letter: the `libei` injection backend (2026-09-12)

A fifth `[inject] backend`, `"libei"`, alongside `wtype` / `ydotool` / `script` /
`clipboard`. **Nothing above is retired or changed by it.** `wtype` is still the
default; `ydotool` is still the only backend proven end to end on real hardware
(2026-09-07, Fedora 44/GNOME 50, into Ptyxis and GNOME Text Editor), and this one does
not inherit that standing by being newer.

What it is: the same paste `ydotool` performs — `wl-copy`, settle, one Ctrl+V
(Ctrl+Shift+V for a terminal, by the same `inject::wants_shift`) — pressed through
`org.freedesktop.portal.RemoteDesktop.NotifyKeyboardKeysym` instead of a `ydotoold` the
user has to run. It costs one approval dialog, once, and then nothing: the portal's
restore token is stored `0600` at `~/.local/state/yappr/libei-restore-token` with
`PersistMode::ExplicitlyRevoked`. `crates/yappr-core/src/libei.rs` owns the session —
module-level rather than on the injector, because `SetConfig` rebuilds the injector on
every settings autosave and a session re-approving itself per keystroke would be worse
than none.

Phase 1 (`NotifyKeyboardKeysym`), not phase 2 (`ConnectToEIS`). The task prompt framed
these as fallback and future; the reach argument runs the other way. `NotifyKeyboardKeysym`
is long-standing on GNOME and Plasma 5.27+, `ConnectToEIS` needs GNOME 45+/Plasma 6+/
xdg-desktop-portal 1.18+, and the Notify path is ~100 lines against a protocol
implementation. It is also layout-independent in a way `ydotool key` is not: a keysym is
resolved by the compositor against the user's own keymap, where a raw evdev keycode
trusts a key position. `reis` is not a dependency and no EIS code was written.

Dependencies: `ashpd 0.13` (`default-features = false`, `async-io`, `remote_desktop`,
`screencast`) and `async-io 2`. Pure Rust, nothing linked, no `libei.so` at runtime, no
new system package in README's install lists, and the `ubuntu:22.04` distrobox AppImage
build is unaffected. `cargo tree -p yappr-core` reports exactly one `zbus` (5.19.0),
shared with `atspi` — checked, and worth re-checking on any ashpd bump. Two ashpd traps
are recorded in `CLAUDE.md`'s gotchas: its default feature is `tokio`, and its
`remote_desktop` feature does not compile without `screencast`.

### What is verified, and what is not

- `cargo test --workspace`: **507 passed, 0 failed, 12 ignored.** `cargo clippy
  --workspace --all-targets`: clean. `bun run build`: clean.
- `cargo test --workspace -- --ignored --test-threads=1`: **8 passed, 4 failed.** The
  four are `asr_fixture`'s, and they fail for want of downloaded models on this machine
  (`tokens.txt does not exist` for `parakeet-tdt-0.6b-v3-int8` and
  `nemotron-3.5-asr-streaming-0.6b-560ms-int8`; only `parakeet-primeline-de-int8` is on
  disk), not for anything this change touched. The half of that run the gate actually
  exists for did pass: `vad::tests::*` builds a `SileroTrimmer` and the four
  `llama::engine_tests` load S1-mini, both in one process, with no
  `free(): invalid pointer` — so adding ashpd did not disturb the `_GLIBCXX_USE_CXX11_ABI=0`
  arrangement `.cargo/config.toml` enforces. The twelfth ignored test is new and is the
  one live check this backend can have without a dialog:
  `libei::tests::the_portal_on_this_desktop_hands_out_keyboards` asks the real portal
  for its version and device types and stops short of `CreateSession`. It passes here
  (version 2, `Keyboard | Pointer | Touchscreen`), which is what proves the ashpd
  wiring actually talks to this desktop rather than merely compiling.
- **Not verified on hardware. Nothing has been pasted through this backend.** The
  reason is specific and worth knowing rather than apologising for: **the RemoteDesktop
  portal refuses to create a session while the screen is locked**, answering
  `Session creation inhibited`, and this machine's session was locked
  (`LockedHint=yes`, `org.gnome.ScreenSaver.GetActive` → `true`) for the whole of the
  work. `CreateSession` was reached, with a throwaway ashpd binary, and got exactly
  that refusal; `Start`, the approval dialog, the restore-token round trip and the chord
  itself were never exercised.
- So the open question is the one the prompt asked to settle first, and it is still
  open: **whether Mutter 50 acts on `NotifyKeyboardKeysym` for a keyboard-only session
  with no linked ScreenCast.** What is measured is that the method exists —
  `busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop
  org.freedesktop.portal.RemoteDesktop` lists `.NotifyKeyboardKeysym` and `.ConnectToEIS`
  at `version 2`, `AvailableDeviceTypes 7`. If it turns out to be inert, phase 2 is the
  answer and `libei.rs`'s `Portal::press` is the only thing that has to change:
  `SessionReport::path` exists so the probe reports which way in was used rather than
  asserting one.
- **To verify it, on an unlocked session:** focus a scratch window and run
  `cargo run -p yappr-core --example libei_probe -- "hallo welt"`. It presses real keys
  and replaces the clipboard. It prints whether the session came up (with the portal's
  own words if not), whether the stored token was honoured or a dialog appeared — the
  dialog is inferred from how long `Start` took, and the probe says so — the window
  class, the chord chosen, and the injection result. Running it twice is the restore-token
  test: the second run should print `restore token sent = true` and a `Start` in
  milliseconds.

### Everything that changed

`config.rs` (the variant, the two doc comments that said "only the ydotool backend reads
this"), `inject.rs` (`LibeiInjector`, the `build()` arm, a new `InjectError::Portal` for
a backend with no exit status to report), `libei.rs` (new), `server.rs` (prewarm at
start and on `SetConfig`/`Reload` — never in `LibeiInjector::new`, which `cargo test`
reaches), `wizard.rs`'s backend match, `setup.rs`'s prerequisite doc (no check added:
there is no binary, and the portal is not a `$PATH` question), `schema.ts`
(`ENUMS`, the help text, and **`DEPENDENT_FIELDS`**, where `paste_chord` and
`terminal_classes` had to become `["ydotool", "libei"]` or the new backend's two settings
would be invisible under it), `wizard.tsx`'s GNOME copy, `README.md`, `CLAUDE.md`.

### First real use, same day (2026-09-12)

The section above says nothing had ever been pasted through this backend. That is no
longer true, and three things came out of the first session on hardware
(Fedora 44 / GNOME Shell 50 / mutter 50.0, an AppImage built from this branch).

- **Phase 1 works.** A plain Ctrl+V delivered through
  `RemoteDesktop.NotifyKeyboardKeysym` pastes. Mutter 50 acts on the method, which was
  the one open question the whole backend was gambling on, and it settles the choice of
  phase 1 over `ConnectToEIS`. The first `Start` took 4,0 s (the approval dialog), issued
  a restore token, and stored it.
- **A held session is a permanent screen-sharing indicator, and that is not acceptable.**
  Reported immediately: GNOME's orange pill sat in the top bar for as long as yappr ran.
  Nothing in the design had considered it -- decision 4 ("the session is created once and
  reused") optimised entirely for not raising a dialog mid-dictation and never asked what
  an open session *looks like*. Fixed by closing the session `SESSION_LINGER` (2 s) after
  the last paste and re-opening it for the next, which the restore token makes silent.
  `KEEP_OPEN` is the guard on that assumption: the first time a `Start` that offered a
  token still takes long enough to have shown a dialog, the session stops being closed
  for the rest of the process and the log says why. One surprise dialog at worst, ever,
  and never one per dictation.
- **Resolved the same evening, and it was not the chord: the portal's own approval
  dialog poisons the window class.** Reported as Ctrl+Shift+V not working in a terminal.
  Three measurements cleared the chord, in order: an explicit `XK_Shift_L` *is* honoured
  by mutter 50 (all three shift encodings typed a capital `A`); Ctrl+Shift+V with keysym
  `v` *does* paste into ghostty through the portal; and the whole real path --
  `libei_probe`, so `ClipboardInjector` + `PASTE_SETTLE` + `winclass` + `press_chord` --
  pasted into a throwaway ghostty first try, with `active_window_class() =
  Some("ghostty")` and the chord resolved to Ctrl+Shift+V.

  The debug records named the real cause. The two dictations before the reported one
  read `window_class: "xdg-desktop-portal-gnome"` -- the RemoteDesktop approval dialog,
  still reporting its accessible window as `Active` twenty and thirty seconds after it
  was dismissed, and `gnome::active_app` returns the first `Active` window it walks
  onto. A portal is not in `terminal_classes`, so `wants_shift` chose plain Ctrl+V and
  the terminal ignored it. `gnome.rs` now filters `xdg-desktop-portal*` by prefix in
  `is_never_a_dictation_target`, beside `yappr` and `gnome-shell`.

  Two things worth keeping from this. The bug was *caused* by the feature that hit it --
  the dialog is one yappr raised -- and it landed on the one code path this repo has
  already been burned by twice. And it was found from the debug records, not from
  reading the code: three rounds of reasoning about mutter's
  `apply_level_modifiers_in_impl` produced nothing, and one `ls -t ~/yappr/logs` produced
  the answer.

- **Also measured: the restore token opens a session silently.** `Start` took 6,2 ms with
  a stored token against 4,0 s for the first, dialog-bearing one. That is the number the
  open-per-paste session lifetime depends on.

- **Superseded: Ctrl+Shift+V does not paste into a terminal.** Plain Ctrl+V works; the terminal
  chord does not, in ghostty, where the `ydotool` backend's identical chord did work on
  2026-09-07. This is not the unknown-window-class hole -- the debug record reads
  `window_class: "ghostty"`, so `wants_shift` returned true and the shifted chord is what
  was sent, and `inject` reported success with no fallback. Nor does mutter's source
  explain it: `apply_level_modifiers_in_impl` returns early for level 0, so an explicit
  `XK_Shift_L` is not dropped on the way to a level-0 `v`.

  `crates/yappr-core/examples/zz_keysym_scratch.rs` was a **temporary** diagnostic for
  exactly this, deleted once it had answered -- it lives in commit `e0c6f5d` if a
  recurrence ever needs it back. It spawned a throwaway ghostty, typed one character per
  candidate encoding into it, and printed which survived: explicit `Shift_L` + `a`, the
  shifted keysym `A` alone (mutter presses shift itself for the level), and both
  together. It needs an unlocked screen -- the portal refuses `CreateSession`
  otherwise -- which is why the question was still open at the time of writing.

  Do not guess a fix from the two plausible encodings. The one that is *not* verified
  will be wrong on some other desktop's portal, and a silently wrong chord is the exact
  failure mode this backend inherited from `ydotool` and which `CLAUDE.md` says twice not
  to paper over.

### The indicator, round two (2026-09-12, same evening)

Closing the session after each paste shipped, and the indicator stayed up anyway. The
log said exactly why:

```
12:58:07 session established token_offered=true token_issued=true start_ms=12
12:58:49 session established token_offered=true token_issued=true start_ms=3391
12:58:49 WARN the stored portal permission did not open a session silently ...
```

The prewarm was silent (12 ms) and closed. The first paste re-opened 42 s later and took
3,4 s -- a real dialog, confirmed by `Failed to associate portal window with parent
window` in xdg-desktop-portal-gnome's journal at 14:58:45 local. `KEEP_OPEN` then did
what it was written to do and stopped closing the session, for the life of the process.

**The guard was the bug.** Latching on a single dialog meant one unexplained event
reinstated the permanent indicator that the whole change existed to remove. It is a
count now (`DIALOG_STRIKES_BEFORE_HOLDING = 2`, cleared by any silent re-open), which
bounds the damage at two dialogs ever while still protecting a desktop whose permission
genuinely does not restore.

What the dialog itself was is **still unknown, and worth saying plainly rather than
dressing up**. Four deliberate close/re-open cycles at 5 s, 45 s and 90 s gaps were all
silent, ~5,5 ms, and returned the identical token; a stale token, by contrast, produces a
21,2 s dialog and a *different* token. So the token in the file had gone stale between
12:58:07 and 12:58:49 for a reason nothing reproduces -- most likely churn from the
several probe processes that share that one file, each rotating it. `SessionReport` now
carries `token_reused` and the establishment log line prints it, so the next occurrence
is one line of diagnosis instead of an afternoon of it.

## The clipboard is handed back after a paste (2026-09-14)

Both backends that paste rather than type -- `ydotool` and `libei` -- work by `wl-copy`ing
the transcript and pressing one chord, which left the transcript sitting in the clipboard
afterwards: whatever the user had copied before dictating was gone, and the next Ctrl+V
they pressed by hand repeated the dictation. They now read the clipboard first and write
it back once the paste has landed (`[inject] restore_clipboard`, default on), so the
user's own entry is current again and the dictation is the *second* entry in whatever
clipboard history they run.

`inject::paste_through_clipboard` is the shared body of both backends, and the ordering is
the whole feature: snapshot before staging (staging is what destroys it), restore after the
chord plus `CLIPBOARD_RESTORE_SETTLE` (300 ms -- the target still has to ask for the
selection and read it). Three cases restore nothing, all deliberately: the setting off, an
empty clipboard (`wl-copy --clear` would leave a user without a history manager nothing to
paste), and a failed press -- there the transcript staying put *is* the clipboard fallback
the user is about to be notified about, invariant 1.

### What is verified, and what is not

Verified: the ordering, the three skip cases and the type selection, by unit test
(`inject.rs`, through a `Clipboard` trait so `cargo test` never touches the developer's own
clipboard). `cargo test --workspace` and `cargo clippy --workspace --all-targets` are clean;
the `--ignored` half passes except the four ASR fixtures whose models are not downloaded on
this machine.

**Not verified: a real paste on real hardware.** Nobody has dictated through this yet. The
one way it can go wrong is invisible from inside yappr -- an application that asks for the
selection *after* the restore has happened pastes the old clipboard instead of the
dictation, and nothing reports which bytes a client read. 300 ms is `handy-paste.sh`'s
number and it has been good enough for that script's users; `restore_clipboard = false` is
the escape hatch, and it is why the behaviour is a setting rather than a heuristic.
`cargo run -p yappr-core --example libei_probe -- "hallo welt"` exercises the whole path
without a microphone and now prints the setting's value.

Also unverified: the non-text path. A copied image is snapshotted under its own MIME type
and handed back with `wl-copy --type`, which is the right shape, but only text has actually
been through it.

## Added after this letter: OpenClaw dictates through yappr (2026-09-14)

Two new things, and they only make sense together.

**A local streaming-transcription endpoint** (`crates/yappr-core/src/realtime.rs`,
`[realtime]`, off by default): a WebSocket bound to `127.0.0.1` that another
program streams PCM into and gets finished utterances back from. Silero cuts the
stream into utterances — the streaming half of the same model the dictation VAD
uses, added as `vad::SileroSegmenter` next to `SileroTrimmer` — and each one goes
through `Pipeline::dictate_text`, which is `process_with_capture` minus the
injector. Both now share `refine`, the extracted vocabulary → language → S1-mini
→ guardrail → `finish` sequence, and that sharing is the only reason the text
another program receives is the text yappr would have typed. `tungstenite 0.30`
is the one new dependency (sync, `handshake` only, no TLS flavour, no runtime).

**A button that installs an OpenClaw plugin** (`openclaw-plugin/`,
`src-tauri/src/openclaw.rs`, the new **AI** pane): OpenClaw's dictation is a
pluggable *realtime transcription provider*, so the plugin is one, and the
button materialises it from `include_str!` data into
`~/.local/share/yappr/openclaw-plugin/`, links it with `openclaw plugins install
--link`, enables it, and writes yappr into OpenClaw's own config as its
streaming provider. Every step is reported separately, failures included: it is
five subprocess calls against another program's CLI and any of them can fail
alone.

### Three decisions worth knowing

**Finals only, no partials.** yappr transcribes whole utterances; there is no
interim state to report, so `onPartial` is never called and the plugin says so
in a comment rather than fabricating deltas from finals.

**Nothing parses `~/.openclaw/openclaw.json`.** Every read and write goes
through `openclaw config get`/`config set`, which resolves
`$OPENCLAW_CONFIG_PATH`, parses the JSON5 that file may be, validates against
the live schema, and refuses under `OPENCLAW_CONFIG_READONLY=1`. This is
`hypr.rs`'s rule one step further along: there we print the lines and let the
user paste them, here we call the other program's own writer.

**The gateway is not restarted.** OpenClaw only loads a newly linked plugin when
its gateway restarts, and that process is serving live agent sessions. The
install's last step is a note naming `openclaw gateway restart`, not a command
this app runs.

### What is verified

- `cargo test --workspace`: **538 passed, 0 failed, 13 ignored.** `cargo clippy
  --workspace --all-targets`: clean. `bun run build`: clean.
- **The endpoint, end to end, against real models.**
  `realtime::tests::a_wav_file_streamed_through_the_socket_comes_back_as_text`
  (`#[ignore]`d) streams `fixtures/hallo_german.wav` through a real socket as
  s16le in 1023-byte frames — a size chosen to sit on neither a sample nor a
  VAD-window boundary — and gets words back. It passed on this machine on
  2026-09-14 with its config pointed at `parakeet-primeline-de`, the only ASR
  model on disk here; the committed version uses `AsrConfig::default()` like
  every other model-backed test, which on this machine fails for want of that
  model exactly as `asr_fixture`'s four already do.
- **The plugin, inside OpenClaw 2026.9.4.** Installed with the real argv the
  button uses, then `openclaw plugins inspect yappr --runtime --json`:
  `status: "loaded"`, `shape: "plain-capability"`, `capabilities:
  [{kind: "realtime-transcription", ids: ["yappr"]}]`, `diagnostics: []`. So
  the hand-written `definePluginEntry` equivalent, the manifest contract and
  the capability-catalog entry are all accepted by the host — the plugin
  imports nothing, because it is loaded from a directory with no
  `node_modules` above it.
- **The config write, and the way back.** `openclaw config set
  plugins.entries.voice-call.config.streaming …` round-tripped exactly. Note
  the warning it prints: `plugin not installed: voice-call`. It is cosmetic
  here — `getVoiceCallProviderConfig` in the host's `talk-*.mjs` reads that
  path straight out of the config tree — but it is why the streaming config
  is the *documented temporary* home for this, per OpenClaw's own
  `docs/nodes/talk.md`.
- **Remove leaves nothing behind**, which took two rounds to get right. The
  first version ran `plugins disable` before `plugins uninstall` and left
  `plugins.entries.yappr.enabled = false` in OpenClaw's config; dropping the
  disable step did not fix it, because `uninstall` writes that flag itself.
  There are now two guarded sweeps — `own_entry_is_vestigial` and
  `streaming_is_vestigial`/`entry_is_empty` — each of which refuses to delete
  anything holding a key this app did not write. Verified by installing and
  removing against the real CLI and diffing `openclaw.json` against a copy
  taken beforehand: identical.

### What is NOT verified

- **No dictation has been done in OpenClaw through this.** Doing so needs the
  gateway restarted and a microphone spoken into, neither of which this session
  did. What is proven is every layer under it: the plugin loads and registers,
  the config is written and read back, and the endpoint transcribes a real wav
  file through a real socket with real models.
- **The `mulaw` path has no model behind it.** `mulaw_to_f32` is unit-tested
  against the G.711 extremes (including the trap that the sign bit is read
  from the *complemented* byte, which the first version of that test got
  backwards), and 8 kHz is proven to arrive resampled, but no telephony audio
  has been transcribed.
- **The `[realtime] token` path has never refused a real client.** It is
  covered by unit tests on `negotiate` and by one session test that is
  genuinely refused the WebSocket upgrade with 401, but no OpenClaw install
  has been configured with one.

### Follow-up, same day: the plugin's settings page (2026-09-14)

Reported within the hour, against the install the button had just made: "in our
OpenClaw plugin settings menu is almost every option twice", with a screenshot
of "Asr model" above "Asr Model" and "Auth token" above "Auth Token".

**Cause:** `openclaw.plugin.json` declared every *alias* `config.js` accepts as
its own schema property. The Control UI renders one row per declared property
and humanises the key, so each alias pair became two rows with the same label in
different cases. The duplication was the smaller half of the problem, though —
the form wrote `plugins.entries.yappr.config`, a path the host never hands to a
transcription provider (`rawConfig` comes from
`plugins.entries.voice-call.config.streaming.providers.<id>` and nowhere else),
so every row in it was also inert.

**Fix, in three parts:**

1. The manifest declares six rows — `host`, `port`, `token`, `sampleRate`,
   `encoding`, `url` — canonical spellings only. The aliases stay accepted in
   `config.js`; `model` (advisory) and `language` (accepted and ignored) are no
   longer declared at all, because a row that changes nothing is worse than no
   row.
2. `config.js` gained `readOwnEntryConfig`: `resolveConfig` receives the whole
   `cfg`, so the plugin now merges `plugins.entries.yappr.config` **over** the
   `rawConfig` the host resolved. The form wins, deliberately — a field edited
   in front of you that does nothing is the worse failure — and the two copies
   are written together, from one `provider_entry`, by the install.
3. `openclaw_install` writes that second copy, so the page arrives **pre-filled
   with this machine's** host, port, sample rate and encoding rather than empty
   beside a working install. `status` now checks both copies for drift, and the
   entry copy matters more: it is the one that wins at runtime.

**Also reported:** "under Talk → Realtime voice yappr isn't even an option". It
is not, and should not be. That picker is `registerRealtimeVoiceProvider` —
bidirectional voice, where the assistant talks back — which is a different
contract from `registerRealtimeTranscriptionProvider`. yappr transcribes; it does
not speak. The dictation surface is the composer microphone, whose "Transcription
setup" link leads to the streaming provider config.

### Verified live this time

Against the real gateway on this machine, after `gateway restart`:

- `plugins inspect yappr --runtime --json`: `status: "loaded"`, `capabilities:
  [{kind: "realtime-transcription", ids: ["yappr"]}]`, `diagnostics: []`, and
  `configJsonSchema.properties` is exactly the six keys — so the rendered form is
  those six and no duplicates.
- `config get plugins.entries.yappr.config --json` answers
  `{host: "127.0.0.1", port: 17869, sampleRate: 16000, encoding: "linear16"}`:
  the page is pre-filled.
- `gateway call talk.catalog` → `transcription: { ready: true, activeProvider:
  "yappr" }`, with yappr `configured: true` and the other two providers
  `configured: false`. **This is the proof the dictation path is wired**, and it
  is what should have been checked the first time instead of reasoning from the
  config file.

Still not verified: nobody has dictated into OpenClaw and read the result.

### Second follow-up: the relay's audio format (2026-09-14)

First actual dictation attempt in OpenClaw:

> Error: Gateway transcription relay requires g711_ulaw/8000 audio

**Cause, and it was ours.** The install wrote `sampleRate: 16000, encoding:
"linear16"`, on the reasoning recorded in the plugin's own README: the consumer
is a browser microphone rather than a telephone, and yappr's pipeline is built
around 16 kHz. Both halves of that are true and the conclusion was still wrong.
The host's `talk-*.mjs` holds

```js
const RELAY_INPUT_ENCODING = "g711_ulaw";
const RELAY_INPUT_SAMPLE_RATE_HZ = 8e3;
```

and `assertRelayInputAudioConfig` throws that exact sentence for any provider
config declaring otherwise — for the **browser dictation mic**, not just for
Twilio. So bundled Deepgram's 8 kHz mulaw default was never a telephony choice;
it is what this seam speaks. There is no transcode path to negotiate around.

Fixed on both sides — `provider_entry` in `openclaw.rs` and the plugin's
`YAPPR_DEFAULT_*` constants — and pinned by
`the_written_audio_format_is_the_one_the_relay_emits`, because 16 kHz linear16
is the obvious-looking "improvement" and it breaks every dictation.

**What the narrowband costs, measured rather than asserted.** The first version
of this fix's comments claimed mulaw would "transcribe measurably worse". That
was an assumption, so it was checked: `fixtures/hallo_german.wav` through the
real socket with real models, in both formats, returned the *identical*
transcript — "Alles hat ein Ende, nur die Wurst hat zwei." The claim was removed
rather than softened. `realtime.rs` now carries both as `#[ignore]`d tests, and
the mu-law one is the path production takes: a broken companding or resampler
fails there while the 16 kHz test still passes. A `f32_to_mulaw` helper lives in
the test module (yappr itself only ever decodes) and is round-trip checked
against the shipped decoder, so an encoder bug cannot masquerade as a decoder
bug.

**What this still does not prove.** Nobody has yet spoken into OpenClaw's mic and
read the result. The format that failed is fixed and the whole path is verified
with a wav file; the last step is a human and a microphone.
