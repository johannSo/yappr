# One-process tray app — design

Status: proposed, 2026-08-28. Supersedes the process topology in
`2026-08-27-openwhisprflow-design.md` §5 and the "why a separate binary"
section of `2026-08-28-settings-gui-design.md`.

## Purpose

OpenWhisprFlow becomes a normal desktop application. The whole user-facing
story is:

1. Find it in the app launcher, click it.
2. It starts with no window. A tray icon appears.
3. Left-click the tray icon → the settings window opens.
4. Right-click → **Beenden** → everything stops.
5. Hold `SUPER+D`, speak, release. It types.

No terminal, no `owf-ctl`, no hand-pasted compositor config, no systemd unit,
no separate daemon to start. Every one of those is a step the user currently
has to perform and will no longer perform.

This is a deliberate reversal of three decisions recorded in CLAUDE.md and in
the settings-GUI spec. Those decisions were correct for a keyboard-driven,
compositor-integrated tool assembled from parts. They are the wrong shape for
an application someone installs. The reversal is priced explicitly in §9.

## 1. Process topology

One process, one binary: `openwhisprflow`.

The Tauri app links `owf-core` and runs the pipeline in-process. It owns:

- the ASR/VAD models and the `Pipeline`
- the `llama-server` child and its existing supervision
- the tray icon
- the overlay window and the settings window
- the global shortcut session

`crates/owf-cli` is removed from the user's path entirely. The `owf-ctl`
binary survives as a **developer tool only** (`bench`, `debug`,
`setup --update-lock`), documented as such in README. Nothing a user does
requires it, and nothing in the app depends on it existing.

`crates/owf-core` keeps every pipeline stage exactly as it is. The daemon
module moves from `owf-cli/src/daemon.rs` to `owf-core/src/server.rs` with
its tests, so the headless test suite is unaffected by the move.

### Why not keep the daemon separate

Because "it all shuts off when I click Close" and "the app launches the daemon
and everything else" are one requirement, not two. A second process is a
second thing that can be alive when the app is dead, or dead when the app is
alive. Collapsing to one process makes the tray's Beenden item total by
construction rather than by careful teardown across a socket.

### The socket, and how the settings window talks now

The app still binds and serves `$XDG_RUNTIME_DIR/openwhisprflow.sock` with the
existing `proto.rs` request set. It is no longer on any user-facing path — it
is what keeps `owf-ctl debug`, `status` and `subscribe` working as developer
tools, and it costs one thread the process already has.

The settings window, being in-process, does **not** use it. `GetConfig`,
`SetConfig` and `ListInputDevices` become Tauri commands carrying byte-identical
JSON payloads, so `src/settings/` and `Settings.tsx` need no changes at all: the
same `config` / `defaults` object, the same rejection shape, the same autosave
and debounce behaviour described in CLAUDE.md invariant 9. Only the transport
under `invoke` differs.

## 2. The global shortcut, without touching Hyprland config

Wayland has no global keyboard grab. That is why the current design routes
`SUPER+D` through a compositor keybind that shells out to `owf-ctl ptt-start`.

The freedesktop **GlobalShortcuts portal** removes that requirement. Verified
on this machine, 2026-08-28:

```
$ busctl --user introspect org.freedesktop.portal.Desktop \
      /org/freedesktop/portal/desktop org.freedesktop.portal.GlobalShortcuts
.BindShortcuts      method   oa(sa{sv})sa{sv} o
.ConfigureShortcuts method   osa{sv}          -
.CreateSession      method   a{sv}            o
.ListShortcuts      method   oa{sv}           o
.version            property u                1
.Activated          signal   osta{sv}
.Deactivated        signal   osta{sv}
```

`Activated` and `Deactivated` are the press and release edges. That is
push-to-talk, delivered by the compositor's own portal implementation
(`xdg-desktop-portal-hyprland`, installed here), with no window rule and no
keybind line.

Flow at startup: `CreateSession` → `BindShortcuts` with one shortcut id
(`push-to-talk`, preferred trigger `SUPER+d`) → subscribe to `Activated` /
`Deactivated` on the session handle. `Activated` starts recording;
`Deactivated` stops it and runs the pipeline. The settings window's shortcut
row calls `ConfigureShortcuts`, which hands rebinding to the portal's own UI
rather than reimplementing a key-capture widget.

**Unverified, must be established before implementation:** whether the
Hyprland portal prompts on first `BindShortcuts`, whether the binding persists
across sessions without re-prompting, and whether `Deactivated` fires reliably
on key release under load. §8 makes this the first task.

**Fallback.** If the portal proves unusable, the app accepts `--ptt-start` /
`--ptt-stop` on its own binary and `hypr.rs` emits keybinds pointing at
`openwhisprflow --ptt-start`. This is a fallback, not the design: it costs the
user a config paste, which is the thing this spec exists to eliminate. It is
still not a separate CLI tool.

## 3. The tray

**Native StatusNotifierItem, not libappindicator.**

Evidence from this machine, 2026-08-28 — the two registered tray items are:

| Bus name | Object path | Owner |
|---|---|---|
| `:1.41` | `/org/ayatana/NotificationItem/tray_icon_tray_app_…` | `handy` |
| `:1.47` | `/StatusNotifierItem` | `claude-desktop` |

Handy is on the libayatana-appindicator path. That binding is menu-only: it
exposes no activation event, which is also why Tauri's own `tray-icon`
feature documents click events as unsupported on Linux. Claude Desktop is on
native SNI, which has an `Activate` method — left click.

The requirement "click the tray icon and it opens the settings" therefore
cannot be met by copying Handy, and cannot be met with Tauri's built-in tray.
The app implements StatusNotifierItem directly (via `ksni`), registering with
the `org.kde.StatusNotifierWatcher` that quickshell provides here.

- **Activate** (left click) → show/focus the settings window.
- **ContextMenu** (right click) → Status (non-interactive), Einstellungen,
  Diktat pausieren, Beenden.
- **Diktat pausieren** is a checkable item backed by one new state in the
  daemon's `AtomicU8`. While paused the shortcut stays bound but `Activated`
  is ignored, so no microphone is opened; the icon shows the paused state and
  the item unchecks to resume. Pausing never interrupts an utterance already
  in flight (invariant 1).
- **Icon** reflects daemon state from the same `OverlayEvent` broadcast the
  overlay consumes: warming, idle, recording, transcribing, failed.

**Unverified:** that quickshell sends `Activate` on left click to a native
item. Task 2 in §8.

## 4. The overlay window, without a compositor rule

CLAUDE.md invariant 5 records that client-side positioning is a no-op under
Hyprland, so bottom-centre placement can only come from a window rule. That is
true for an `xdg_shell` toplevel. It is not true for a **layer-shell** surface.

`wlr-layer-shell` lets a client anchor itself to a screen edge with a margin
and declare `keyboard-interactivity: none`. That is simultaneously the answer
to invariant 5 (positioning) and invariant 2 (focus) — both without a single
line in the user's Hyprland config. Tauri v2 uses GTK3 on Linux, so
`gtk-layer-shell` can be applied to the overlay window's underlying
`GtkWindow` before realization.

The settings window stays an ordinary focusable toplevel.

**Unverified:** that `gtk-layer-shell` applies cleanly to the window Tauri
creates, before WebKitGTK realizes it. Task 3 in §8. If it does not work, the
overlay falls back to a toplevel plus an emitted window rule — the one paste
this design would otherwise not need.

## 5. First run

"Plain app starts, works" requires the app to provision its own models. Today
that is `owf-ctl setup`.

On first launch, if `models.lock.toml`'s artifacts are absent, the app opens
the settings window on a **Setup** pane showing per-model download progress,
the sha256 verification result, and the `llama-server` / `ggml` backend check
that `owf-ctl setup` performs today. The tray icon shows the warming state
throughout. Dictation is refused with a clear reason until setup completes.

The provisioning logic itself is unchanged `owf-core` code; only its caller
and its progress reporting are new.

## 6. Shutdown

**Beenden** must leave nothing behind. In order:

1. Stop accepting new utterances; if one is in flight, let `finish` and
   injection complete (invariant 1 — a transcribed utterance is never lost).
2. Close the GlobalShortcuts session.
3. Kill the `llama-server` child and reap it. This is the one that leaks
   today if the process is killed rather than dropped.
4. Unregister the tray item.
5. Remove the socket and release the runtime lock.
6. Exit.

Closing the settings window hides it; it does not exit the app. This is
standard tray-app behaviour and is what "shuts off when I click Close on the
tray icon" implies about every other close affordance.

## 7. Autostart

A settings toggle, **Beim Anmelden starten**, which writes or removes
`~/.config/autostart/openwhisprflow.desktop`. systemd's
`xdg-autostart-generator` turns that into a user unit automatically — the same
mechanism Handy uses here (`app-Handy@autostart.service`).

Off by default. The app is launched from the launcher until the user asks
otherwise. No unit file is authored by this project.

## 8. What must be verified before implementing

Every item below is a claim this design rests on that has not been tested on
real hardware. Each is a cheap probe, and each has a named fallback.

| # | Claim | Fallback if false |
|---|---|---|
| 1 | Portal `Activated`/`Deactivated` give reliable press/release for `SUPER+D`, persisting across sessions | Emitted Hyprland keybinds calling `openwhisprflow --ptt-start` (§2) |
| 2 | quickshell sends SNI `Activate` on left click | Left click opens the menu; Einstellungen is its first item |
| 3 | `gtk-layer-shell` applies to Tauri's GTK window | Toplevel + emitted window rule (§4) |
| 4 | The app's window class/title under `hyprctl clients -j` | — (informational, needed only for the §4 fallback) |

Consistent with this project's practice, none of these is taken from
documentation: each is confirmed against the running machine first.

## 9. What this costs

Stated plainly, because these are reversals of decisions made deliberately.

- **Crash isolation is gone.** A WebKitGTK crash now takes the ASR models with
  it, costing a full reload (~8 s) rather than nothing. Accepted: it buys the
  single-process lifetime the requirement is built on.
- **The GUI inherits a heavy build graph.** Every frontend change now builds
  against `sherpa-onnx`, `cpal`, `rubato`. Accepted.
- **Invariant 3 collapses, favourably.** `src-tauri/src/wire.rs` is deleted;
  `OverlayEvent` goes from three hand-maintained copies to two (Rust and the
  TS union). `checked_in_fixture_covers_every_event_kind` still guards the
  remaining pair.
- **Invariant 2 changes mechanism, not force.** Focus is refused by
  layer-shell `keyboard-interactivity: none` plus Tauri's `focusable: false`,
  rather than by a compositor rule. Still enforced twice.
- **Invariant 5 is superseded** for the overlay: layer-shell surfaces do
  position themselves.
- **The settings-GUI spec's "why a separate binary" is void.** Its premise was
  that both windows share one class and the class carries `no_focus`. With
  layer-shell there is no class-matched rule to inherit.

CLAUDE.md must be updated in the same change, not afterwards.

## 10. Out of scope

- Changing any pipeline stage, the guardrail, `finish`, or the config schema.
- Changing the settings GUI's form generation, autosave, or `config_write`.
- Porting to compositors other than Hyprland. The portal and layer-shell paths
  are standard and should work more widely, but nothing here is verified
  elsewhere and no claim is made.
