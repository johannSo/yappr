<div align="center">

<img src="public/yappr.png" alt="yappr logo" width="128" height="128">

# yappr

**Local dictation.** Press a button, speak, press again —
what you said is typed into whatever window has focus, punctuated and tidied up.
No cloud, no account, no network calls while you dictate.

</div>

---

| | |
|---|---|
| **Settings** | left-click the tray icon |
| **Everything else** | right-click the tray icon |

**Jump to:** [Install](#install) · [First run](#first-run) · [Using it](#using-it) ·
[Settings](#settings) · [Troubleshooting](#troubleshooting) ·
[How it works](#how-it-works) · [Limitations](#known-limitations)

## Install

### 1. Install the system packages

**Arch:**

```bash
sudo pacman -S wtype wl-clipboard gtk-layer-shell
```

**Fedora:**
```bash
sudo dnf install wtype wl-clipboard gtk-layer-shell
```

`llama-cpp` and `ggml-cpu` used to be on this list. They are not needed any more: the
clean-up model runs inside `yappr` itself rather than behind a separate `llama-server`
process, so there is no second program to install, no compute backend to pick, and no
way to hit ggml's *"no backends are loaded"*. Nothing about dictation changes — same
model, same output.

### 2. Build and install `yappr` manually

You need [`rustup`](https://rustup.rs/) and [`bun`](https://bun.sh/).

```bash
bun install                 # frontend dependencies
bun run build               # builds both windows into dist/ — required before the next line
cargo build --release -p yappr --features custom-protocol

mkdir -p ~/.local/bin
install -m755 target/release/yappr ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH`.

Two things that bite if skipped:

- **`bun run build` first.** The web assets are compiled *into* the binary, so
  `cargo build` fails outright with `` the `frontendDist` configuration is set to
  "../dist" but this path doesn't exist `` if you haven't built them.
- **`--features custom-protocol` is not a default.** Without it the binary embeds a
  `localhost:1420` dev URL instead of the built assets, and every window comes up
  blank unless a Vite dev server happens to be running.

Check it worked — both of these run locally, without the app started:

```bash
which -a yappr            # exactly one path, in ~/.local/bin
yappr --print-shortcuts   # prints the config block for your compositor
```

### 3. Add it to your app launcher

```bash
mkdir -p ~/.local/share/applications
cat > ~/.local/share/applications/yappr.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=yappr
Comment=Startet das Diktat-Overlay im Hintergrund
Exec=yappr
Terminal=false
Categories=Utility;AudioVideo;
EOF
```

(Same shape and the same bare, `PATH`-relative `Exec=` line the autostart toggle in
Settings writes for itself.)

## Using it

### The gesture

Press `SUPER+D` to start recording. Speak. Press `SUPER+D` again to stop — the text
appears in the focused window a moment later. `SUPER+ALT+D` throws the recording away.

There is **no key to release**: a recording keeps going until you press again. If you
forget, a watchdog ends it after `audio.max_seconds` (120 s by default) and *transcribes what it captured* rather than discarding it. If 120 seconds of open microphone isn't a
trade you want, lower it in Settings → Allgemein → Mikrofon & Aufnahme.

### What the overlay shows

A small pill at the bottom of the screen, visible only while something is happening:

| It shows | Meaning |
|---|---|
| `loading models` | first dictation since startup (or since an idle unload) — recording has already started, this runs in parallel |
| a red dot, a level meter, a timer | recording |
| `transcribing` → `cleaning` → `typing` | the three stages after you press again |
| a checkmark and a preview | done — that text just got typed |
| a warning and a reason | something failed; the reason is on screen |

The overlay never takes keyboard focus — if it did, `wtype` would type your dictation
into the overlay instead of your editor.

### The tray

Left-click opens Settings. Right-click gives you:

- **Status** — what the app is doing right now
- **Einstellungen** — the settings window
- **Einrichtung…** — reopen the setup wizard
- **Diktat pausieren** — refuse `SUPER+D` entirely until you un-pause (never interrupts
  a dictation already in flight)
- **Beenden** — quit

### If typing fails

The text is copied to your clipboard instead and you get a notification saying so. A
transcript is never silently lost: once speech has been recognised, you get text, even
if the clean-up model is down, times out, or produces something the guardrail rejects
(then you get the raw transcript instead).

### Start it at login

Settings → Allgemein → **"Beim Anmelden starten"**. It writes
`~/.config/autostart/yappr.desktop`.
Off by default.

## Desktop notes

### Hyprland

`yappr --print-shortcuts` prints the block for your config — it detects Lua (as Omarchy
uses) versus classic `.conf` and prints the matching one. They are not interchangeable:
Hyprland 0.56+ with a Lua config rejects the legacy keyword parser outright.

**Lua** — add to `~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + D", "Dictation: toggle", "yappr --toggle")
o.bind("SUPER + ALT + D", "Dictation: cancel", "yappr --cancel")
```

**Classic** — add to `~/.config/hypr/hyprland.conf`:

```ini
bind  = SUPER, D,     exec, yappr --toggle
bind  = SUPER ALT, D, exec, yappr --cancel
```

Check the keys are free first (`omarchy menu keybindings --print` on Omarchy), then:

```bash
hyprctl reload && hyprctl configerrors
```

**You don't need a window rule.** The overlay positions and un-focuses itself through
`wlr-layer-shell`. `--print-shortcuts` still emits a title-matched fallback rule for
compositors without layer-shell; it's inert on Hyprland, so paste it or don't.

### GNOME

Two differences:

- **Use the `ydotool` backend.** Mutter doesn't implement the virtual-keyboard protocol
  `wtype` types through, so `wtype` silently does nothing. The wizard detects GNOME and
  sets `[inject] backend = "ydotool"` for you on a first run — see
  [Pasting with ydotool](#pasting-with-ydotool) for the one-time setup.
- **Add the shortcuts in Settings → Keyboard → Custom Shortcuts**, running
  `yappr --toggle` and `yappr --cancel`. The wizard also offers a `gsettings` script
  that *appends* to your existing custom shortcuts — never use a plain
  `gsettings set ... custom-keybindings`, which replaces the whole list and destroys
  every custom shortcut you already had.

Mutter has no `wlr-layer-shell`, so the overlay falls back to an ordinary borderless
window and lands wherever Mutter puts it. It still refuses keyboard focus (that part is
enforced by the window itself, not by the compositor), so it can't swallow your
dictation — it just isn't pinned to the bottom of the screen the way it is on wlroots
compositors.

### Other Wayland desktops

Nothing is desktop-specific except shortcut registration. Bind `yappr --toggle` and
`yappr --cancel` however your desktop does that, and try `wtype` first.

## Settings

Left-click the tray icon, or `yappr --settings`. Everything in `config.toml` is
editable there — microphone, dictation vocabulary, styles, thresholds — across five
panes: **Allgemein**, **Sprache**, **Stil**, **Erweitert**, **Diagnose**. There's a
search box; it matches German labels, help text, *and* the raw `config.toml` key names.

There is no Save button. Toggles and dropdowns save immediately, text and number fields
700 ms after you stop typing. Saving rewrites `config.toml` from scratch; a save that
changes nothing leaves the file byte-identical, and a config that wouldn't load is
rejected before anything is written. Every row has a reset button that appears only when
the value isn't the default.

**`[asr]` and `[normalize]` changes need a restart** (`yappr --quit`, then launch
again); the window says so on those rows. Everything else — the microphone included —
applies at your next dictation, or immediately with `yappr --reload`.

## config.toml

Lives at `~/.local/state/yappr/config.toml`, written on first run.

**This file belongs to the app.** The settings window is how you change things; the file
is generated from scratch on every save, so anything you type into it is overwritten the
next time you touch a setting — comments included. It is documented here because it is
useful to read (in a bug report, say), not because it is meant to be edited.

If you used yappr before 2026-09-02 your settings are moved across automatically on the
first start, and the old `~/.config/yappr/config.toml` is left behind as
`config.toml.migrated`.

| Section | What's in it |
|---|---|
| `[audio]` | `device`, `max_seconds` (the only thing that ends a forgotten recording), `vad_padding_ms` |
| `[models]` | `preload_at_startup`, `idle_unload_seconds` — see [Memory use](#memory-use) |
| `[asr]` | `num_threads` for Parakeet |
| `[normalize]` | `enabled` (`false` skips S1-mini and types rule-cleaned raw text), plus `timeout_ms`, `context_size`, `threads` |
| `[guardrail]` | `min_word_ratio`/`max_word_ratio`, `min_overlap_english`/`min_overlap_other`, `short_input_words`, `ngram_size`/`ngram_max_repeats` |
| `[inject]` | `backend` (`wtype`, `ydotool`, `clipboard`), `trailing_space`, `keystroke_delay_ms` (wtype only), `terminal_classes` (windows that paste with Ctrl+Shift+V), `paste_chord` (`auto`, `ctrl_v`, `ctrl_shift_v`; ydotool only) |
| `[vocabulary]` | terms and replacements applied to the raw transcript before clean-up — put short acronyms in `replacements`, not `terms` |
| `[style_default]`, `[[style_rules]]` | the `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched on window class (regex) |
| `[debug]` | `enabled` (off), `dir` (default `~/yappr`), `save_audio` — see [Troubleshooting](#troubleshooting) |
| `[overlay]` | `position`, `width`, `height` — read, but inert: under Wayland a window can't place itself, so this changes nothing today |

> **A config that won't load is moved aside, not ignored.** Every section is
> `deny_unknown_fields`, so an unrecognised key is still caught rather than silently
> dropped — but instead of stopping the app it renames the file to
> `config.toml.broken-<timestamp>`, starts on defaults, and tells you so in the settings
> window, naming the file it moved. The app has to keep starting: the settings window
> lives in the same process, so a config that stopped it would take the only tool for
> fixing it down too.

`yappr --reload` re-validates the file against the running app and applies every
reloadable section live. It refuses outright — rather than half-applying — if you
changed `[asr]` or `[normalize]`, and it reports a file it cannot read rather than
resetting it, which is the one place the app will not quietly replace your config.
Settings saved from the window already apply live; `--reload` is for a config that
changed some other way, such as one restored from a backup.

### Pasting with `ydotool`

`wtype` is the default and needs no setup: it types through the compositor's own
virtual-keyboard protocol. But it does nothing on GNOME, and it's known to drop
keystrokes in some XWayland and Electron windows. The `ydotool` backend goes through the
kernel's `/dev/uinput` instead, which no window can tell apart from a real keyboard — at
the cost of some setup, which is why it isn't the default.

It doesn't type the transcript, it pastes it: the text goes to the clipboard with
`wl-copy`, then ydotool presses a single Ctrl+V by raw keycode — or Ctrl+Shift+V when
the target window's class is in `[inject] terminal_classes`, because terminals reserve
plain Ctrl+V for the program running inside them. Key *positions* are
layout-independent, so this works on any keyboard layout, umlauts and ß included —
`ydotool type` would mangle them through its US-only keymap. The transcript stays in the
clipboard afterwards, so if the paste keystroke fails you can paste it yourself.

```bash
sudo pacman -S ydotool    # For Arch based distros
sudo dnf install ydotool  # For Fedora based distros
```

`ydotoold` needs write access to `/dev/uinput`, which is `root`-only out of the box.
Hand it to the `input` group once:

```bash
sudo tee /etc/udev/rules.d/60-uinput.rules <<'EOF'
KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"
EOF
sudo udevadm control --reload-rules && sudo udevadm trigger /dev/uinput
sudo usermod -aG input "$USER"   # log out and back in for the group to take effect
```

Then run the daemon as a **user** service. Some distros ship one — try
`systemctl --user enable --now ydotool.service` first (the unit is named for the
package, not for ydotoold). If `systemctl --user cat ydotool.service` finds nothing,
as on Fedora, write your own:

```ini
# ~/.config/systemd/user/ydotoold.service
[Unit]
Description=ydotoold - ydotool user daemon

[Service]
Type=simple
ExecStart=/usr/bin/ydotoold --socket-path=%t/.ydotool_socket --socket-perm=0600
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now ydotoold.service
ydotool type ''   # exits 0 once the socket is up; prints nothing, types nothing
```

Do **not** enable a system-wide `ydotoold` pointed at `/run/user/$UID/.ydotool_socket`.
A system unit starts at boot, before logind has created `/run/user/$UID`, so it dies
with `failed to bind socket: No such file or directory` and never retries once
systemd's restart limit is hit.

Then set it in Settings → Allgemein → Texteingabe → Verfahren, or:

```toml
[inject]
backend = "ydotool"
```

#### If nothing is pasted into a terminal

Choosing Ctrl+Shift+V over Ctrl+V needs the class of the window you dictated into, and
that answer comes from `hyprctl` alone. Three situations leave yappr without it:

- **On GNOME there is no `hyprctl`** — but since 2026-09-07 yappr asks the
  accessibility bus instead, which needs no extension and no setting, so `auto` works
  there too. It only comes up empty for an app that registers with no accessibility
  bus at all, which some Electron and Qt apps do not.
- **On Hyprland, `hyprctl` needs `HYPRLAND_INSTANCE_SIGNATURE`** in yappr's own
  environment. Started from a systemd user unit or a `.desktop` autostart on a session
  that never exported it, `hyprctl` fails and yappr is left without a class.
- **Nothing was focused** when you started talking.

Either way the class is unknown, yappr falls back to plain Ctrl+V, and every terminal
ignores it: the dictation lands in the clipboard and nothing appears. Nothing fails, so
there is no error — only a warning on yappr's stdout, which is `journalctl --user` if
your desktop session started it, or set `[debug] enabled = true` and read
`~/yappr/logs/daemon.log`. `yappr --debug` also prints the target window class of the
last dictation, which tells you directly whether this is what you hit. Force the chord:

```toml
[inject]
paste_chord = "ctrl_shift_v"   # "auto" (default) | "ctrl_v" | "ctrl_shift_v"
```

`auto` keeps the per-window behaviour and is right whenever `hyprctl` can answer.
Forcing `ctrl_shift_v` makes terminals work everywhere, at the cost of sending
Ctrl+Shift+V to ordinary windows too — in browsers and Electron apps that is
"paste as plain text", which is what you want for dictation anyway, but a few apps
bind it to something else (LibreOffice opens *Paste Special*).

## Memory use

By default the models load on your first key press and unload again a minute after your
last dictation. An idle yappr therefore holds a fraction of what speech recognition and
the language model need together — about 1.4 GB that would otherwise stay resident on
the development machine.

The price: the first dictation after a pause waits once for the models to load.
Recording starts instantly and loading happens while you speak, so you only notice it on
a very short dictation, where the text may arrive a couple of seconds late.

Two settings under **Erweitert → Modelle & Speicher**:

| Setting | Default | Meaning |
|---|---|---|
| Modelle beim Start laden (`preload_at_startup`) | off | Load everything at app start. No wait on the first dictation, but the memory is held from launch. |
| Modelle entladen nach (`idle_unload_seconds`) | 60 s | Idle time after which the models are released. `0` means never. |

Want them always warm? Turn the first **on** *and* set the second to `0`. Both are
needed — `preload_at_startup` alone still unloads after the idle timeout.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| **Nothing happens when I press `SUPER+D`** | The app isn't running (check for the tray icon), or the shortcut isn't bound. Run `yappr --toggle` in a terminal: with no app running it exits non-zero and raises a notification. |
| **Nothing gets typed, but the overlay says it worked** | `wtype` can't reach that window — you're on GNOME, or it's an XWayland/Electron window. Switch to [`ydotool`](#pasting-with-ydotool). Check the clipboard: the text is probably there. |
| **`ydotool` says `failed to connect socket ... Please check if ydotoold is running`** | `ydotoold` isn't up. `systemctl --user status ydotoold` — if it's a *system* unit bound to `/run/user/$UID/…`, that can never work; see [Pasting with `ydotool`](#pasting-with-ydotool). yappr falls back to the clipboard here, so the transcript is still there to paste by hand. |
| **Nothing is pasted *into a terminal* on the `ydotool` backend** | yappr couldn't read the target window's class, so it sent plain Ctrl+V, which terminals ignore. Check that your terminal's name is in `[inject] terminal_classes` — GNOME's Ptyxis reports itself as `ptyxis`. Failing that, set `[inject] paste_chord = "ctrl_shift_v"` — see [If nothing is pasted into a terminal](#if-nothing-is-pasted-into-a-terminal). |
| **`cargo build` fails in `gtk-layer-shell-sys`** | `sudo pacman -S gtk-layer-shell`. |
| **Blank windows after building** | Built without `--features custom-protocol`, or without `bun run build` first. |
| **The first dictation of the day is slow** | Expected — the models load lazily. See [Memory use](#memory-use). |
| **The setup wizard keeps reappearing** | A model file is missing or its hash doesn't match. The models step says which one. |
| **My clean-up rewrote something wrongly** | Every guardrail rejection is logged to `~/.local/state/yappr/rejections.jsonl`. Set `[normalize] enabled = false` to bypass S1-mini entirely. |
| **Two overlays on screen** | An old pre-rename binary is still installed and bound. See [Upgrading](#upgrading-from-an-older-build). |

Digging deeper: set `[debug] enabled = true`, dictate once, then run `yappr --debug`.
It prints a summary of the last utterance — capture stats, audio levels, the VAD span,
raw transcript, vocabulary substitutions, guardrail verdict, timings — and the paths to
the WAV files and JSON record it wrote under `~/yappr`. Those are yours to delete when
you're done; `--purge-logs` does *not* touch them — it deletes only
`rejections.jsonl`, the guardrail dataset, which holds raw/cleaned transcript pairs and
never leaves your machine either.

Other useful commands:

```bash
yappr --status            # one line of JSON: state, whether models are resident
yappr --subscribe         # live event stream, one JSON object per line
yappr --reload            # re-read config.toml into the running app
yappr --print-shortcuts   # the config block for your compositor
yappr --bench             # ASR latency table
yappr --purge-logs        # delete rejections.jsonl
yappr --update-lock       # re-resolve and re-pin models.lock.toml (after an upstream
                          # model update — not part of a normal first run)
yappr --quit              # shut down
```

`yappr` with no arguments starts the app; every other invocation is either a message to
a running instance or a local utility, and both exit before any window or model is
touched.

## How it works

One dictation runs through this pipeline:

1. **Capture** — 16 kHz mono from the first press to the second.
2. **VAD** — Silero trims leading and trailing silence.
3. **ASR** — [Parakeet TDT 0.6b v3](https://github.com/k2-fsa/sherpa-onnx) (int8, via `sherpa-onnx`) transcribes it.
4. **Vocabulary** — your `[vocabulary]` corrections are applied to the raw transcript.
5. **Language ID** — `whatlang` tags the language, which picks the guardrail threshold.
6. **Normalization** — S1-mini, loaded into this same process, rewrites the transcript into punctuated prose (or a list, depending on the style axes).
7. **Guardrail → Injection** — cheap deterministic checks (word-count ratio, token overlap, n-gram loop detection, template-bleed detection) decide whether to trust the rewrite. If not, you get the raw transcript or a rule-based clean-up of it instead of something the model invented; the rejection is appended to `rejections.jsonl`. Then `wtype` types it into the focused window.

If S1-mini fails to load, dictation degrades to raw ASR text rather than failing — and
the next dictation simply tries again. Nothing is lost either way: once speech has been
recognised, you get text.

### Measured performance

On an i7-10510U (4 threads), release build:

| Stage | Latency |
|---|---|
| ASR (11.5 s of audio) | 1,700 ms (RTF 0.147) |
| S1-mini clean-up, 8-word transcript | 438 ms |
| S1-mini clean-up, 26-word transcript | 585 ms |
| S1-mini clean-up, 58-word transcript | 1,313 ms |
| S1-mini load (once, at first press) | 438 ms |
| ASR model load (once, at first press) | 3,497 ms |

A typical ~10 s dictation, warm, is roughly **2 seconds** from the second press to text
appearing.
