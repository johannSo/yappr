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
     autostart is now the Settings window's "Beim Anmelden starten" toggle (step 5).
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

6. **Turn on "Beim Anmelden starten"** in Settings if you want the old autostart behaviour
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
