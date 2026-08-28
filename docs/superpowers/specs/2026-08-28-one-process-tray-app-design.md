# One-process tray app — design

Status: proposed, 2026-08-28 (rev 2). Supersedes the process topology in
`2026-08-27-openwhisprflow-design.md` §5 and the "why a separate binary"
section of `2026-08-28-settings-gui-design.md`.

Rev 2 replaces the GlobalShortcuts-portal hotkey of rev 1 with ordinary
desktop shortcuts invoking the binary, and deletes `owf-ctl` entirely.

## Purpose

OpenWhisprFlow becomes a normal desktop application, driven by one binary:

1. Find it in the app launcher, click it. It starts with no window; a tray
   icon appears.
2. Left-click the tray icon → the settings window opens.
3. Right-click → **Beenden** → everything stops.
4. A desktop shortcut — Hyprland, GNOME, anything that can run a command —
   bound to `openwhisprflow --start` / `--stop` / `--cancel` does the
   dictation.

There is no `owf-ctl`, no separate daemon, no systemd unit, and no second
binary of any kind. Everything the tool can do is an argument to
`openwhisprflow`.

## 1. Process topology

One process, one binary. The Tauri app links `owf-core` and runs the pipeline
in-process. It owns the models, the `llama-server` child and its existing
supervision, the tray icon, the overlay window, the settings window, and the
Unix socket.

`crates/owf-cli` is **deleted**. Its three modules are redistributed:

| Today | Becomes |
|---|---|
| `owf-cli/src/daemon.rs` | `owf-core/src/server.rs` — moved with its tests, unchanged |
| `owf-cli/src/ctl.rs` | `src-tauri/src/client.rs` — the socket client behind the flags in §2 |
| `owf-cli/src/bench.rs` | `src-tauri/src/bench.rs` — behind `--bench` |
| `owf-cli/src/lib.rs`'s `route()` | `src-tauri/src/cli.rs` — same pure function, same test discipline |

`route()` survives the move deliberately. It is a pure `&[&str] -> Route`
function whose tests pin that every accepted invocation resolves to the action
it names, and that discipline matters *more* now: these strings are what a
user's compositor config contains, and a silent change to one of them breaks
dictation with no error anywhere. The tests move with it and gain the new
flags.

### Why not keep the daemon separate

"The app launches everything, and it all shuts off when I click Close" is one
requirement, not two. A second process is a second thing that can be alive
when the app is dead. Collapsing to one process makes the tray's Beenden item
total by construction.

## 2. The command-line surface

`openwhisprflow` with no arguments starts the app. Every other invocation is
either a **client** call against a running instance or a **local** utility;
neither starts a window, and both exit immediately.

| Flag | Kind | Effect |
|---|---|---|
| *(none)* | — | Start the app: tray, no window |
| `--start` | client | Begin recording (the shortcut's press edge) |
| `--stop` | client | End recording and run the pipeline (the release edge) |
| `--cancel` | client | Discard the in-flight utterance |
| `--toggle` | client | Start if idle, stop if recording — see §3 |
| `--settings` | client | Show the settings window |
| `--quit` | client | Shut the whole app down, exactly as the tray's Beenden |
| `--status` | client | One JSON line: state, warmth, last timings |
| `--debug` | client | The last utterance's debug record |
| `--subscribe` | client | Stream `OverlayEvent` NDJSON until interrupted |
| `--reload` | client | Re-read `config.toml` |
| `--replay <path>` | local | Drive the overlay from an NDJSON fixture, no pipeline |
| `--bench` | local | ASR latency table |
| `--print-shortcuts` | local | Emit the Hyprland block (and the GNOME recipe) |
| `--purge-logs` | local | Delete the debug log |
| `--update-lock` | local | Refresh `models.lock.toml` |

`--stop` ends *recording*, not the app; `--quit` ends the app. The two are
named apart on purpose because a shortcut config containing both is exactly
where that confusion would be expensive.

### The client fast path

Argument dispatch happens at the top of `main`, **before** any Tauri, GTK or
WebKit initialisation and before any model is touched. A client invocation
opens the socket, writes one NDJSON request line, reads one response line,
prints it, and exits.

This path's latency is a first-class requirement, because `--start` sits on
the press edge: every millisecond it costs is a millisecond of the user's
first syllable that is not recorded. Measured baseline on this machine,
2026-08-28: today's `owf-ctl status` — a 46 MB binary already linking
`owf-core` — completes spawn-to-exit in **96 ms**. The new binary is larger,
so this is a regression risk, not a neutral move. §12 makes it a measured
gate with a threshold, not an assumption.

### When no instance is running

A client call with no app running exits non-zero with a one-line message on
stderr *and* raises a desktop notification. Today the equivalent failure is
silent: the keybind fires, nothing happens, and there is nothing to look at.
A user whose only interface is a hotkey needs the failure to be visible
without a terminal.

## 3. Shortcuts

The app binds nothing itself. The user binds a shortcut in whatever their
desktop provides, and `--print-shortcuts` emits the block to paste — never
applied automatically, per this project's standing rule about the user's
compositor config.

**Hyprland** gets true push-to-talk, because it can bind the release edge:

```
bind  = SUPER, D,     exec, openwhisprflow --start
bindr = SUPER, D,     exec, openwhisprflow --stop
bind  = SUPER ALT, D, exec, openwhisprflow --cancel
```

**GNOME cannot do this.** Its custom shortcuts run a command on key *press*
only; there is no release binding. Hold-to-talk is therefore not expressible
there, which is what `--toggle` exists for: one shortcut, press to start,
press again to stop. The emitted GNOME recipe binds `--toggle`, and the
settings window states plainly which mode is active rather than letting the
user discover that holding the key does nothing useful.

This is a real behavioural difference between desktops, not a gap to paper
over. Push-to-talk and toggle feel different, and the one you get is decided
by your compositor.

### The GlobalShortcuts portal: considered, not chosen

`xdg-desktop-portal-hyprland` on this machine exposes
`org.freedesktop.portal.GlobalShortcuts` v1 with `BindShortcuts`,
`Activated` **and** `Deactivated` — verified 2026-08-28. That is a complete
push-to-talk source requiring no compositor config at all, and on Hyprland it
would remove the paste entirely.

It is not the design because it binds the feature to a portal implementation
that varies per desktop and may prompt or fail to persist, where a command
shortcut works anywhere a desktop can run a command. The finding is recorded
here so it is not rediscovered from scratch if the paste ever becomes the
thing worth eliminating.

## 4. The socket

The app binds and serves `$XDG_RUNTIME_DIR/openwhisprflow.sock` with the
existing `proto.rs` request set, `secure_socket`'s owner-only mode, and the
existing runtime lock for single-instance enforcement.

The socket is now the load-bearing mechanism, not a developer convenience:
every flag marked *client* in §2 is a line on it. `Request` gains `Quit`,
`ShowSettings`, `Toggle` and `SetPaused`; `PttStart`/`PttStop`/`Cancel` and
the settings requests are unchanged.

The settings window, being in-process, does **not** use the socket.
`GetConfig`, `SetConfig` and `ListInputDevices` become Tauri commands carrying
byte-identical JSON payloads, so `src/settings/` and `Settings.tsx` need no
changes at all — same `config` / `defaults` object, same rejection shape, same
autosave and debounce behaviour (CLAUDE.md invariant 9). Only the transport
under `invoke` differs.

## 5. The tray

**Native StatusNotifierItem, not libappindicator.** Evidence from this
machine, 2026-08-28 — the two registered tray items are:

| Bus name | Object path | Owner |
|---|---|---|
| `:1.41` | `/org/ayatana/NotificationItem/tray_icon_tray_app_…` | `handy` |
| `:1.47` | `/StatusNotifierItem` | `claude-desktop` |

Handy is on the libayatana-appindicator path, which exposes no activation
event — which is also why Tauri's own `tray-icon` feature documents click
events as unsupported on Linux. Claude Desktop is on native SNI, which has
`Activate`. "Left-click opens settings" therefore cannot be met by copying
Handy or by using Tauri's built-in tray; the app implements SNI directly (via
`ksni`), registering with the `org.kde.StatusNotifierWatcher` quickshell
provides here.

- **Activate** (left click) → show and focus the settings window.
- **ContextMenu** (right click) → Status (non-interactive), Einstellungen,
  Diktat pausieren, Beenden.
- **Diktat pausieren** is a checkable item backed by one new state in the
  server's `AtomicU8`. While paused, `--start` returns a clean refusal and no
  microphone is opened; the icon shows the paused state. Pausing never
  interrupts an utterance already in flight (invariant 1).
- **Icon** reflects state from the same `OverlayEvent` broadcast the overlay
  consumes: warming, idle, recording, transcribing, paused, failed.

## 6. The overlay window

CLAUDE.md invariant 5 records that client-side positioning is a no-op under
Hyprland, so bottom-centre placement can only come from a window rule. That is
true for an `xdg_shell` toplevel and not true for a **layer-shell** surface.

`wlr-layer-shell` lets a client anchor itself to a screen edge with a margin
and declare `keyboard-interactivity: none` — simultaneously the answer to
invariant 5 (positioning) and invariant 2 (focus), with no window rule.
Tauri v2 uses GTK3 on Linux, so `gtk-layer-shell` can be applied to the
overlay's `GtkWindow` before realization.

`wlr-layer-shell` is a wlroots protocol; Mutter does not implement it. On
GNOME the overlay falls back to a plain toplevel, unpositioned. §14 is honest
about what that means.

The settings window is an ordinary focusable toplevel in both cases.

## 7. First run

"Plain app starts, works" requires the app to provision its own models.

On first launch, if `models.lock.toml`'s artifacts are absent, the app opens
the settings window on a **Setup** pane showing per-model download progress,
sha256 verification, and the `llama-server` / `ggml` backend check that
`owf-ctl setup` performs today. The tray icon shows warming throughout, and
dictation is refused with a stated reason until setup completes.

The provisioning logic is unchanged `owf-core` code; only its caller and its
progress reporting are new.

## 8. Shutdown

`--quit` and the tray's Beenden are the same code path, and it must leave
nothing behind:

1. Stop accepting new utterances; let an in-flight one finish through `finish`
   and injection (invariant 1 — a transcribed utterance is never lost).
2. Kill the `llama-server` child and reap it. This is the one that leaks today
   when the process is killed rather than dropped.
3. Unregister the tray item.
4. Remove the socket and release the runtime lock.
5. Exit.

Closing the settings window hides it; it does not exit the app.

## 9. Autostart

A settings toggle, **Beim Anmelden starten**, writing or removing
`~/.config/autostart/openwhisprflow.desktop`. systemd's
`xdg-autostart-generator` turns that into a user unit automatically — the same
mechanism Handy uses here (`app-Handy@autostart.service`). Off by default. No
unit file is authored by this project.

## 10. Migration

This is a breaking change for anyone with the current setup, and it breaks in
the worst way available: a keybind that silently does nothing.

Existing configs contain `exec-once = owf-ctl daemon`, `exec-once =
openwhisprflow`, `exec, owf-ctl ptt-start` and window rules matched on
`class:^(openwhisprflow)$`. After this change `owf-ctl` does not exist.

`--print-shortcuts` therefore emits a complete replacement block, and its
output leads with the lines to **delete**, not only the ones to add. README
and `HANDOVER.md` are updated in the same change. There is exactly one user
today, which is the only reason a hard cutover is acceptable at all.

## 11. Testing

The move must not cost a single test. `owf-core` keeps every pipeline,
guardrail, `finish`, `vocab` and `config_write` test exactly as it is, and the
server tests move with `server.rs` rather than being rewritten — a relocation
that changes assertions is not a relocation.

Three areas gain tests:

- **`cli.rs::route()`** — one case per row of §2's table, plus the pins that
  already exist. These strings live in users' compositor configs; a rename
  that compiles is still a break, and only a test catches it.
- **Shutdown (§8)** — that `--quit` reaps the `llama-server` child, removes the
  socket and releases the lock. The child is stubbed through `owf-core`'s
  existing `test-util` seam (`LlamaServer::from_child`), which exists precisely
  because a real one cannot run here.
- **The client fast path** — that a client invocation returns without
  initialising Tauri or touching a model. Asserted structurally, so a future
  edit that moves argument dispatch below `tauri::Builder` fails the suite
  instead of quietly costing every keypress the §12 gate.

`--replay` remains the acceptance evidence for every overlay state without a
microphone, and `checked_in_fixture_covers_every_event_kind` still guards the
Rust/TypeScript pair.

`cargo test --workspace && cargo clippy --workspace --all-targets` stays the
gate. There is still no CI.

## 12. What must be verified before implementing

Each item is a claim this design rests on that has not been tested on real
hardware, with a named fallback. Consistent with this project's practice, none
is taken from documentation.

| # | Claim | Fallback if false |
|---|---|---|
| 1 | `openwhisprflow --start` completes spawn-to-socket-write in **≤ 120 ms** (baseline: `owf-ctl status`, 96 ms) | Investigate lazy-loading the ASR libraries off the client path; if that fails, reconsider a second small client binary — which would cost the one-binary requirement, so it is a last resort |
| 2 | quickshell sends SNI `Activate` on left click | Left click opens the menu; Einstellungen is its first item |
| 3 | `gtk-layer-shell` applies to Tauri's GTK window before WebKitGTK realizes it | Toplevel plus an emitted window rule, matched on title (both windows now share one class) |
| 4 | The app's window class and titles under `hyprctl clients -j` | — (informational, needed for the #3 fallback) |
| 5 | GNOME custom shortcuts are press-only, so `--toggle` is required there | If a release binding exists, GNOME gets true push-to-talk too |

Item 1 is a gate, not a note: if the client path is slow, users lose the start
of every utterance, and that is a worse outcome than any structural benefit
this design delivers.

## 13. What this costs

Stated plainly, because these reverse decisions made deliberately.

- **Crash isolation is gone.** A WebKitGTK crash now takes the ASR models with
  it, costing a full reload (~8 s) rather than nothing.
- **The GUI inherits a heavy build graph.** Every frontend change builds
  against `sherpa-onnx`, `cpal`, `rubato`.
- **Every keypress pays the binary's start-up cost**, and the binary is now
  larger than the one that pays it today. Gated by §12 item 1.
- **Invariant 3 collapses, favourably.** `src-tauri/src/wire.rs` is deleted;
  `OverlayEvent` goes from three hand-maintained copies to two (Rust and the
  TS union). `checked_in_fixture_covers_every_event_kind` still guards the
  remaining pair.
- **Invariant 2 changes mechanism, not force.** Focus is refused by
  layer-shell `keyboard-interactivity: none` plus Tauri's `focusable: false`,
  rather than by a compositor rule — on wlroots. On GNOME only the Tauri half
  applies.
- **Invariant 5 is superseded** for the overlay on wlroots compositors.
- **The settings-GUI spec's "why a separate binary" is void.** Its premise was
  that both windows share one class and that class carries `no_focus`. With
  layer-shell there is no class-matched rule to inherit.

CLAUDE.md is updated in the same change, not afterwards.

## 14. Out of scope

- Any change to a pipeline stage, the guardrail, `finish`, or the config
  schema.
- Any change to the settings GUI's form generation, autosave, or
  `config_write`.
- **Full GNOME support.** GNOME gets the shortcut mechanism (`--toggle`) and
  a working tray. It does not get push-to-talk, and its overlay is
  unpositioned until someone verifies an alternative. Nothing here is tested
  on GNOME; the design merely stops actively precluding it.
