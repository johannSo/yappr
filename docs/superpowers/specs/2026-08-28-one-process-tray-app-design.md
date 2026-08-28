# One-process tray app — design

Status: proposed, 2026-08-28 (rev 3). Supersedes the process topology in
`2026-08-27-openwhisprflow-design.md` §5 and the "why a separate binary"
section of `2026-08-28-settings-gui-design.md`.

- rev 2 replaced rev 1's GlobalShortcuts-portal hotkey with desktop shortcuts
  invoking the binary, and deleted `owf-ctl`.
- rev 3 drops hold-to-talk entirely in favour of press-to-start /
  press-to-stop.

## Purpose

OpenWhisprFlow becomes a normal desktop application, driven by one binary:

1. Find it in the app launcher, click it. It starts with no window; a tray
   icon appears.
2. Left-click the tray icon → the settings window opens.
3. Right-click → **Beenden** → everything stops.
4. A desktop shortcut — Hyprland, GNOME, anything that can run a command —
   bound to `openwhisprflow --toggle`. Press it, speak, press it again. It
   transcribes and types.

There is no `owf-ctl`, no separate daemon, no systemd unit, and no second
binary of any kind. Everything the tool can do is an argument to
`openwhisprflow`.

## 1. Process topology

One process, one binary. The Tauri app links `owf-core` and runs the pipeline
in-process. It owns the models, the `llama-server` child and its existing
supervision, the tray icon, the overlay window, the settings window, and the
Unix socket.

`crates/owf-cli` is **deleted**. Its modules are redistributed:

| Today | Becomes |
|---|---|
| `owf-cli/src/daemon.rs` | `owf-core/src/server.rs` — moved with its tests, unchanged |
| `owf-cli/src/ctl.rs` | `src-tauri/src/client.rs` — the socket client behind the flags in §2 |
| `owf-cli/src/bench.rs` | `src-tauri/src/bench.rs` — behind `--bench` |
| `owf-cli/src/lib.rs`'s `route()` | `src-tauri/src/cli.rs` — same pure function, same test discipline |

`route()` survives the move deliberately. It is a pure `&[&str] -> Route`
function whose tests pin that every accepted invocation resolves to the action
it names, and that discipline matters *more* now: these strings are what a
user's shortcut config contains, and a silent change to one of them breaks
dictation with no error anywhere.

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
| `--toggle` | client | Start recording if idle; stop and transcribe if recording |
| `--cancel` | client | Discard the in-flight utterance |
| `--settings` | client | Show the settings window |
| `--quit` | client | Shut the whole app down, exactly as the tray's Beenden |
| `--status` | client | One JSON line: state, warmth, last timings |
| `--debug` | client | The last utterance's debug record |
| `--subscribe` | client | Stream `OverlayEvent` NDJSON until interrupted |
| `--reload` | client | Re-read `config.toml` |
| `--replay <path>` | local | Drive the overlay from an NDJSON fixture, no pipeline |
| `--bench` | local | ASR latency table |
| `--print-shortcuts` | local | Emit the shortcut block to paste |
| `--purge-logs` | local | Delete the debug log |
| `--update-lock` | local | Refresh `models.lock.toml` |

There is deliberately no `--start` or `--stop`. Two flags for the two edges of
a keypress is the hold-to-talk model, and keeping them "just in case" would
leave two ways to reach one behaviour and a second state path to test. The
`PttStart` / `PttStop` protocol requests stay internal to the server, which is
why the state machine needs no change: `--toggle` resolves to one or the other
on arrival.

`--cancel` remains separate because it is not a phase of the same gesture —
it is the escape hatch, and binding it to its own shortcut is the point.

### Toggle semantics

`--toggle` is resolved against the current state, not against a client-side
memory of what was pressed last:

| State on arrival | Effect |
|---|---|
| `IDLE` | Start recording |
| `RECORDING` | Stop, transcribe, inject |
| `WARMING` | Refuse with a stated reason; models are not loaded yet |
| `TRANSCRIBING` / `NORMALIZING` / `INJECTING` | Refuse; a press during processing must not open the microphone behind the utterance still being typed |
| `PAUSED` | Refuse; the tray item says so |
| `FAILED` | Refuse with the warm-up failure's reason |

Refusals are silent to the desktop but visible in the overlay, which is
already what the `Error` flash and the busy states are for.

### The safety valve is now load-bearing

Hold-to-talk had a physical guarantee that recording ends: you let go. Toggle
has none. A missed second press leaves the microphone open, and the only thing
that closes it is the existing watchdog in `daemon.rs` — an epoch-checked
timer that fires at `audio.max_seconds` and runs the utterance rather than
discarding it.

That code already exists and is correct. What changes is its standing: its
comment calls it a guard against "a stuck key", a rare accident. It is now the
sole terminator of a forgotten recording, so it must be documented as such,
and its default of 120 s reconsidered by the user rather than inherited
silently. `config_write.rs`'s annotated fixture calls it a "hold-to-talk cap"
in a string two tests assert on; that wording is now wrong (§11).

### The client fast path

Argument dispatch happens at the top of `main`, **before** any Tauri, GTK or
WebKit initialisation and before any model is touched. A client invocation
opens the socket, writes one NDJSON request line, reads one response line,
prints it, and exits.

Measured baseline on this machine, 2026-08-28: today's `owf-ctl status` — a
46 MB binary already linking `owf-core` — spawns and round-trips in **96 ms**.

Under hold-to-talk this was a design-threatening number, because it sat on the
press edge and ate the beginning of every utterance. Under toggle it does not:
the user presses, *then* speaks, and the gap between those two acts is human
and far larger than the binary's start-up. Dropping hold-to-talk dissolves the
riskiest item in this design rather than merely simplifying it. The number is
still worth measuring (§12), but as a performance note, not a gate.

### When no instance is running

A client call with no app running exits non-zero with a one-line message on
stderr *and* raises a desktop notification. Today the equivalent failure is
silent: the shortcut fires, nothing happens, and there is nothing to look at.
A user whose only interface is a shortcut needs the failure to be visible
without a terminal.

## 3. Shortcuts

The app binds nothing itself. The user binds a shortcut in whatever their
desktop provides, and `--print-shortcuts` emits the block to paste — never
applied automatically, per this project's standing rule about the user's
compositor config.

Because the binding is press-only, **every desktop gets identical behaviour**.
Hyprland:

```
bind = SUPER, D,     exec, openwhisprflow --toggle
bind = SUPER ALT, D, exec, openwhisprflow --cancel
```

GNOME: two custom shortcuts in Settings → Keyboard, running the same two
commands. No `bindr`, no release edge, no per-desktop caveat, and nothing in
the settings window that has to explain which mode the user is in. This is the
single largest simplification in rev 3 — the previous revision needed a whole
subsection to explain why GNOME felt different, and that subsection is gone.

### The GlobalShortcuts portal: considered, not chosen

`xdg-desktop-portal-hyprland` on this machine exposes
`org.freedesktop.portal.GlobalShortcuts` v1 with `BindShortcuts` and
`Activated` — verified 2026-08-28. Toggle needs only the press edge, so the
portal is now a *better* fit than it was in rev 1, where push-to-talk also
required `Deactivated`.

It is still not the design, because it binds the feature to a portal
implementation that varies per desktop and may prompt or fail to persist,
where a command shortcut works anywhere a desktop can run a command. Recorded
so it is not rediscovered from scratch if the paste ever becomes the thing
worth eliminating.

## 4. The socket

The app binds and serves `$XDG_RUNTIME_DIR/openwhisprflow.sock` with the
existing `proto.rs` request set, `secure_socket`'s owner-only mode, and the
existing runtime lock for single-instance enforcement.

The socket is the load-bearing mechanism, not a developer convenience: every
flag marked *client* in §2 is a line on it. `Request` gains `Toggle`, `Quit`,
`ShowSettings` and `SetPaused`; `PttStart` / `PttStop` / `Cancel` and the
settings requests are unchanged.

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
  server's `AtomicU8`. While paused, `--toggle` is refused and no microphone
  is opened. Pausing never interrupts an utterance already in flight
  (invariant 1).
- **Icon** reflects state from the same `OverlayEvent` broadcast the overlay
  consumes: warming, idle, recording, transcribing, paused, failed.

The recording state matters more than it did. Under hold-to-talk the user's
own finger was the indicator that the microphone was open; under toggle the
only indicators are the overlay and this icon, so neither is decorative.

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
GNOME the overlay falls back to a plain toplevel, unpositioned (§14).

The settings window is an ordinary focusable toplevel in both cases.

## 7. First run

"Plain app starts, works" requires the app to provision its own models.

On first launch, if `models.lock.toml`'s artifacts are absent, the app opens
the settings window on a **Setup** pane showing per-model download progress,
sha256 verification, and the `llama-server` / `ggml` backend check that
`owf-ctl setup` performs today. The tray icon shows warming throughout, and
`--toggle` is refused with a stated reason until setup completes.

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

This is a breaking change for the current setup, and it breaks in the worst
way available: a shortcut that silently does nothing.

Existing configs contain `exec-once = owf-ctl daemon`, `exec-once =
openwhisprflow`, `bind`/`bindr` pairs calling `owf-ctl ptt-start` and
`ptt-stop`, and window rules matched on `class:^(openwhisprflow)$`. After this
change `owf-ctl` does not exist and the `bindr` line has no replacement — it
is deleted, not rewritten.

`--print-shortcuts` therefore emits a complete replacement block whose output
**leads with the lines to delete**. README and `HANDOVER.md` are updated in the
same change. There is exactly one user today, which is the only reason a hard
cutover is acceptable.

## 11. Testing

The move must not cost a single test. `owf-core` keeps every pipeline,
guardrail, `finish`, `vocab` and `config_write` test exactly as it is, and the
server tests move with `server.rs` rather than being rewritten — a relocation
that changes assertions is not a relocation.

Gaining tests:

- **`cli.rs::route()`** — one case per row of §2's table, plus the pins that
  already exist. These strings live in users' shortcut configs; a rename that
  compiles is still a break, and only a test catches it.
- **Toggle resolution** — one case per row of the §2 state table, in
  particular that a press during `TRANSCRIBING` / `NORMALIZING` / `INJECTING`
  refuses rather than opening the microphone behind an utterance still being
  typed. This is the new state path and the one that can lose an utterance if
  it is wrong.
- **The safety valve under toggle** — that a recording with no second press is
  ended and *transcribed* by the watchdog, not discarded. The behaviour exists
  today; no test currently pins it as the sole terminator it now is.
- **Shutdown (§8)** — that `--quit` reaps the `llama-server` child, removes the
  socket and releases the lock, using `owf-core`'s existing `test-util` seam
  (`LlamaServer::from_child`), which exists precisely because a real one
  cannot run here.
- **The client fast path** — that a client invocation returns without
  initialising Tauri or touching a model, asserted structurally so a later
  edit that moves dispatch below `tauri::Builder` fails the suite.

Changing: `config_write.rs`'s annotated fixture describes `max_seconds` as a
"hold-to-talk cap" in a string asserted by two tests. Both the comment and the
assertions change with the gesture.

`cargo test --workspace && cargo clippy --workspace --all-targets` stays the
gate. There is still no CI.

## 12. What must be verified before implementing

Each item is a claim this design rests on that has not been tested on real
hardware, with a named fallback. Consistent with this project's practice, none
is taken from documentation.

| # | Claim | Fallback if false |
|---|---|---|
| 1 | quickshell sends SNI `Activate` on left click | Left click opens the menu; Einstellungen is its first item |
| 2 | `gtk-layer-shell` applies to Tauri's GTK window before WebKitGTK realizes it | Toplevel plus an emitted window rule, matched on title (both windows now share one class) |
| 3 | The app's window class and titles under `hyprctl clients -j` | — (informational, needed for the #2 fallback) |
| 4 | `openwhisprflow --toggle` round-trips in the same order as today's 96 ms | Performance note only; toggle does not sit on the speech edge (§2) |

Rev 2 carried a fifth item — whether GNOME can bind a key release — and a
latency *gate*. Both are gone: the first is moot under a press-only binding,
and the second is downgraded to item 4.

## 13. What this costs

Stated plainly, because these reverse decisions made deliberately.

- **Crash isolation is gone.** A WebKitGTK crash now takes the ASR models with
  it, costing a full reload (~8 s) rather than nothing.
- **The GUI inherits a heavy build graph.** Every frontend change builds
  against `sherpa-onnx`, `cpal`, `rubato`.
- **Nothing physically ends a recording.** Toggle trades the release edge's
  guarantee for a timer. The watchdog is sound, but a user who misses the
  second press keeps the microphone open until `audio.max_seconds`, and the
  only warning is on screen. This is the price of the gesture, not a defect
  in it.
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
  schema, beyond documenting `audio.max_seconds`' new standing.
- Any change to the settings GUI's form generation, autosave, or
  `config_write`, beyond the annotated comment in §11.
- Voice-activated stop. The gesture is press / press; ending an utterance on
  detected silence is a different feature with different failure modes, and
  the VAD in this pipeline trims audio rather than driving state.
- **Full GNOME support.** Under toggle, GNOME gets identical dictation
  behaviour and a working tray. What it does not get is a positioned overlay
  (§6). Nothing here is tested on GNOME; the design stops precluding it.
