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

- **`wtype` does nothing here.** Mutter doesn't implement the virtual-keyboard protocol
  `wtype` types through. The wizard detects GNOME and sets `[inject] backend =
  "clipboard"` for you on a first run: the transcript lands in the clipboard and you
  press Ctrl+V. To have it inserted for you, write a paste script and use the
  [`script` backend](#pasting-with-your-own-script) — GNOME needs no other setup.
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
editable there — microphone, dictation vocabulary, styles, thresholds, colours — across
six panes: **Allgemein**, **Sprache**, **Stil**, **Darstellung**, **Erweitert**,
**Diagnose**. There's a search box; it matches German labels, help text, *and* the raw
`config.toml` key names.

There is no Save button. Toggles and dropdowns save immediately, text and number fields
700 ms after you stop typing. Saving rewrites `config.toml` from scratch; a save that
changes nothing leaves the file byte-identical, and a config that wouldn't load is
rejected before anything is written. Every row has a reset button that appears only when
the value isn't the default.

### Themes

**Darstellung → Farbschema** picks the palette, and it applies to both windows —
the settings window *and* the dictation overlay. It takes effect the moment you
choose it; nothing to restart.

| | |
|---|---|
| **System** | The palette yappr ships with, following your desktop's light/dark preference. The default. |
| **yappr Hell**, **yappr Dunkel** | The same two palettes, pinned, so they stay put when your desktop switches. |
| **Catppuccin Latte**, **Catppuccin Mocha** | [Catppuccin](https://catppuccin.com), light and dark. |
| **Tokyo Night Day**, **Tokyo Night Night** | [Tokyo Night](https://github.com/folke/tokyonight.nvim), light and dark. |

Two things worth knowing. The overlay is a capsule floating over whatever you're
dictating into, so on a light theme it becomes a *light* capsule — it carries a
harder edge and a heavier shadow to stay legible over a light wallpaper, but if
you dictate mostly over dark windows, a dark theme will read better.

And the borrowed palettes are **not quite** upstream's. Neither Latte nor Tokyo
Night Day clears WCAG AA as small text on its own background (Latte's green
measures 2.96:1, Tokyo Night Day's comment grey 2.54:1), and the settings window
is a form. So text colours are the theme's own hues darkened just far enough to
clear 4.5:1 — same hue, a little deeper. Surfaces and accent fills are upstream's
untouched. If you'd rather have the exact upstream colours than readable ones,
that's a knob yappr doesn't have.

**`[asr]` and `[normalize]` changes can need a restart**, and the window asks you when
they do: a prompt with a **Jetzt neu starten** button that shuts yappr down and brings it
straight back. Pick **Später** and the offer stays as a bar at the top of the pane until
you take it. You never have to restart it by hand.

They only need one if the models are actually loaded at that moment. With the default
`[models] preload_at_startup = false`, an idle yappr has nothing resident and picks up
the new model or normalizer on your next dictation, so nothing is asked. Everything else
— the microphone included — applies at your next dictation, or immediately with
`yappr --reload`.

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
| `[inject]` | `backend` (`wtype`, `script`, `clipboard`), `script` (the program the `script` backend runs — see [Pasting with your own script](#pasting-with-your-own-script)), `trailing_space`, `keystroke_delay_ms` (wtype only) |
| `[vocabulary]` | terms and replacements applied to the raw transcript before clean-up — put short acronyms in `replacements`, not `terms` |
| `[style_default]`, `[[style_rules]]` | the `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched on window class (regex) |
| `[debug]` | `enabled` (off), `dir` (default `~/yappr`), `save_audio` — see [Troubleshooting](#troubleshooting) |
| `[overlay]` | `position`, `width`, `height` — read, but inert: under Wayland a window can't place itself, so this changes nothing today |
| `[ui]` | `theme` — see [Themes](#themes) |

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

### Pasting with your own script

`wtype` is the default and needs no setup: it types through the compositor's own
virtual-keyboard protocol. But it does nothing on GNOME, and it's known to drop
keystrokes in some XWayland and Electron windows. For those, the `script` backend hands
the finished transcript to a program of your own and lets it do the inserting.

```toml
[inject]
backend = "script"
script = "~/bin/paste.sh"
```

The contract is one line long: **yappr runs your program with the finished transcript as
`"$1"`, and passes nothing else.** No second argument, no `YAPPR_*` environment
variables. Everything after that is your script's business — putting the text on the
clipboard, deciding between Ctrl+V and Ctrl+Shift+V, asking your desktop which window is
focused, restoring whatever was in the clipboard before. yappr deliberately does none of
it, which is also why a script written for another dictation tool usually works
unchanged.

The path is tilde-expanded (`~/bin/paste.sh` works) and the file must be executable
(`chmod +x`). Set it in Settings → Allgemein → Texteingabe → *Einfüge-Skript*, or in
`config.toml` as above.

The smallest useful script:

```bash
#!/bin/sh
# Minimal: copy and paste with Ctrl+Shift+V.
wl-copy -- "$1"
sleep 0.2
ydotool key 29:1 42:1 47:1 47:0 42:0 29:0
```

That one needs `ydotool` and a running `ydotoold` with write access to `/dev/uinput` —
but that is now your script's dependency to install and document, not yappr's. A script
using `wtype`, `xdotool`, `dotool`, `gdbus` or a GNOME extension is equally valid.

**If it fails, you still get your text.** A non-zero exit, a missing or non-executable
file, more than 15 seconds without returning, or an empty `[inject] script` all fall back
to copying the transcript to the clipboard, with a desktop notification saying so. That
is invariant 1: once speech has been transcribed, the text reaches you somehow.

Your script's stdout and stderr are logged even when it exits 0, so a `echo` is a
perfectly good way to see what it decided. `yappr --debug` prints the last dictation's
record, including the target window class yappr saw (it does not pass that to your
script — a script that wants it asks its own desktop).

#### Upgrading from the `ydotool` backend

`backend = "ydotool"` still loads: it is read as `script`, so nothing breaks on start and
no settings are lost. The next time anything saves your config the file is rewritten as
`backend = "script"`. What you need to add is the `script` key pointing at a program that
does what the old backend did — the minimal script above is that program, plus whichever
terminal check you want. Until you do, the backend has no script to run and every
dictation lands in the clipboard with a notification.

`[inject] paste_chord` and `[inject] terminal_classes` are still accepted so your
existing file loads, but nothing reads them any more and they are dropped from the file
on the next save. Your script picks the chord now.

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
| **Nothing gets typed, but the overlay says it worked** | `wtype` can't reach that window — you're on GNOME, or it's an XWayland/Electron window. Check the clipboard: the text is probably there. Fix it with the [`script` backend](#pasting-with-your-own-script). |
| **The `script` backend produced nothing and a notification says it fell back to the clipboard** | Your script failed. Run it by hand — `~/bin/paste.sh "hallo"` — and watch what it says; yappr logs its stdout and stderr either way. The usual causes are a path that isn't executable (`chmod +x`), a wrong `[inject] script` path, and a helper it calls (`ydotoold`, `wl-copy`) not being up. `cargo run -p yappr-core --example script_probe -- "hallo"` runs it exactly the way yappr does. |
| **Nothing is pasted *into a terminal*** | Your script sent plain Ctrl+V, which terminals ignore. That choice is your script's, not yappr's: have it check the focused window's class and send Ctrl+Shift+V for terminals, or send Ctrl+Shift+V unconditionally — browsers and Electron read it as "paste as plain text", which is what you want for dictation anyway. |
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
