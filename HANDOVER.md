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
   pkill -f '/openwhisprflow$'
   ```
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
