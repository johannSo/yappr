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
[Settings](#settings) · [OpenClaw](#dictating-in-openclaw) · [Troubleshooting](#troubleshooting) ·
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
  press Ctrl+V. To have it inserted for you, switch *Verfahren* to `libei` (nothing to
  install; approve one dialog), to `ydotool` (which needs a running `ydotoold`), or to
  `script` with a paste script of your own — see
  [Pasting where `wtype` can't type](#pasting-where-wtype-cant-type).
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
seven panes: **Allgemein**, **Sprache**, **Stil**, **KI**, **Erweitert**,
**Darstellung**, **Diagnose**. There's a search box; it matches German labels, help text, *and* the raw
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

### Dictating in OpenClaw

[OpenClaw](https://openclaw.ai) is a separate, locally installed AI agent. Its
dictation normally goes to a cloud speech service. The **KI** pane turns that
around: one button installs a yappr plugin into OpenClaw and points its dictation
at this machine's models instead — same Parakeet, same S1-mini clean-up, same
vocabulary and style rules as the text yappr types into your editor, and nothing
leaves the machine.

**Einrichten** does five things, and reports each one separately:

1. writes the plugin to `~/.local/share/yappr/openclaw-plugin/`
2. switches on yappr's local endpoint (`[realtime] enabled = true`)
3. runs `openclaw plugins install --link` against that directory
4. runs `openclaw plugins enable yappr`
5. writes yappr into OpenClaw's own config as its streaming transcription provider

Steps 3–5 call the `openclaw` CLI, which is what writes OpenClaw's config file —
yappr never edits it directly. If OpenClaw isn't installed, the button says so and
does nothing; install it separately (`npm i -g openclaw`) and press **Erneut
prüfen**.

**One manual step is left, deliberately:** OpenClaw only loads a newly linked
plugin when its gateway restarts (`openclaw gateway restart`). yappr doesn't do
that for you — that process is serving live agent sessions, and ending them is not
a side effect a dictation app's settings window should have.

**Entfernen** undoes the OpenClaw side (unselects the provider, removes its entry,
disables and unlinks the plugin) and deliberately leaves `[realtime]` alone: that
is yappr's own setting, and you may have switched the endpoint on for something
else.

#### What the endpoint is

`[realtime]` opens a WebSocket on **127.0.0.1 only** — there is no setting that
puts it on a network interface. A program connects, streams microphone audio, and
gets finished sentences back:

| | |
|---|---|
| **Address** | `ws://127.0.0.1:17869/v1/transcribe` (`[realtime] port`) |
| **Audio in** | PCM s16le or G.711 µ-law, mono, any sample rate from 8 kHz up — declared as `?sample_rate=&encoding=`. OpenClaw always sends µ-law at 8 kHz; that is its relay's fixed contract, not a setting |
| **Text out** | one JSON `{"type":"final","text":…}` per utterance, cut by the same Silero VAD yappr uses on its own recordings |
| **Interim results** | none. yappr transcribes whole utterances; there is nothing to show mid-sentence |
| **Access** | any local program, unless you set `[realtime] token` — then `Authorization: Bearer <token>` or `?token=` is required |

`silence_ms` (700 ms) is how long a pause ends a sentence, and
`max_utterance_seconds` (20 s) is where a stretch of unbroken speech gets cut
anyway — what gets cut is still transcribed, never dropped. `normalize` decides
whether those transcripts get the S1-mini rewrite or stop at the raw ASR plus
capitalisation and punctuation; it can only turn clean-up *off*, never on when
`[normalize] enabled = false`.

The endpoint costs nothing while nothing is connected, and it opens no microphone
of its own — the audio comes from whatever connected to it. Changes to `[realtime]`
take effect immediately; there is no restart to do.

If you change the port or the token *after* installing, OpenClaw is left pointing at
the old ones and the card says so (“veraltete Zugangsdaten”) — press **Erneut
einrichten** to write them across.

The plugin's own source, protocol notes and manual install instructions are in
[`openclaw-plugin/README.md`](openclaw-plugin/README.md).

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
| `[inject]` | `backend` (`wtype`, `libei`, `ydotool`, `script`, `clipboard`), `script` (the program the `script` backend runs), `paste_chord`/`terminal_classes`/`restore_clipboard` (`ydotool` and `libei` only) — see [Pasting where `wtype` can't type](#pasting-where-wtype-cant-type) — plus `trailing_space`, `keystroke_delay_ms` (wtype only) |
| `[vocabulary]` | terms and replacements applied to the raw transcript before clean-up — put short acronyms in `replacements`, not `terms` |
| `[style_default]`, `[[style_rules]]` | the `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched on window class (regex) |
| `[debug]` | `enabled` (off), `dir` (default `~/yappr`), `save_audio` — see [Troubleshooting](#troubleshooting) |
| `[overlay]` | `position`, `width`, `height` — read, but inert: under Wayland a window can't place itself, so this changes nothing today |
| `[ui]` | `theme` — see [Themes](#themes) |
| `[realtime]` | `enabled` (off), `port`, `token`, `silence_ms`, `max_utterance_seconds`, `normalize` — the local endpoint other programs dictate through; see [Dictating in OpenClaw](#dictating-in-openclaw) |

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

### Pasting where `wtype` can't type

`wtype` is the default and needs no setup: it types through the compositor's own
virtual-keyboard protocol. But it does nothing on GNOME, and it's known to drop
keystrokes in some XWayland and Electron windows. Three backends cover those, under
Settings → Allgemein → Texteingabe → *Verfahren*:

| `[inject] backend` | What it does | What it needs |
|---|---|---|
| `wtype` | Types the text into the focused window. | Nothing. Does nothing on GNOME. |
| `libei` | Copies the text and presses the paste chord through the desktop portal. | Nothing to install. One approval dialog, once. Needs a portal that implements `RemoteDesktop` (GNOME, KDE; not `xdg-desktop-portal-wlr`). |
| `ydotool` | Copies the text and presses the paste chord for you. | `ydotool`, plus a running `ydotoold` with write access to `/dev/uinput`. |
| `script` | Hands the text to a program of yours, which does the inserting. | A script you write. |
| `clipboard` | Copies the text; you paste it. | Nothing. |

`libei` and `ydotool` do the same thing by different routes and share the same three
settings, `paste_chord`, `terminal_classes` and `restore_clipboard`. `ydotool` is the
one verified end to end on real hardware, which is the only reason it is listed first in
this section; `libei` is the one that asks nothing of you beyond a click.

Both paste by putting the transcript on the clipboard, so both **put back what was in
the clipboard before** once the paste has landed (`[inject] restore_clipboard`, on by
default). What you had copied is the current clipboard entry again, and the dictation
sits one place below it in whatever clipboard history you run — so the next Ctrl+V you
press by hand is still *your* text, not the last thing you dictated. Turn it off if a
program reads the clipboard so late that it ends up pasting the old contents instead of
the dictation; nothing can detect that from yappr's side. An empty clipboard and a
failed paste both restore nothing on purpose — in the second case the transcript is the
clipboard fallback you are being notified about.

#### `libei`

```toml
[inject]
backend = "libei"
paste_chord = "auto"          # or "ctrl_v" / "ctrl_shift_v"
restore_clipboard = true      # put back what you had copied, after pasting
```

Same paste as `ydotool` — `wl-copy`, then one Ctrl+V (Ctrl+Shift+V for a terminal) —
sent through `org.freedesktop.portal.RemoteDesktop` instead of a daemon of your own.
Nothing to install: the portal is already running, there is no `/dev/uinput`, no group
to join and no socket path to get wrong. The first time yappr asks, your desktop shows
an approval dialog; say yes and the permission is stored as a portal *restore token*
under `~/.local/state/yappr/libei-restore-token`, and you are never asked again. Delete
that file to be asked afresh; revoke it in your desktop's privacy settings to take the
permission away.

Two things worth knowing. It presses a **keysym**, not a key position, so the
compositor resolves `v` against the keymap you actually have — the one respect in which
it is better than `ydotool` rather than merely cheaper. And **the portal refuses to
start a session while the screen is locked** (`Session creation inhibited`), which is
harmless: yappr retries, and a dictation that lands in that window falls back to the
clipboard with a notification.

`xdg-desktop-portal-wlr` — sway, river, Wayfire — implements no `RemoteDesktop` at all,
so this backend does nothing there. Those are wlroots compositors, where `wtype` works
natively and is already the default.

```bash
cargo run -p yappr-core --example libei_probe -- "hallo welt"
```

runs exactly what a dictation runs, without a microphone: it prints whether the session
came up, whether the stored token was honoured or a dialog appeared, the window class,
the chord it chose, and what the paste returned. It presses real keys into the focused
window and replaces your clipboard, so focus a scratch window first.

#### `ydotool`

```toml
[inject]
backend = "ydotool"
paste_chord = "auto"          # or "ctrl_v" / "ctrl_shift_v"
restore_clipboard = true      # put back what you had copied, after pasting
```

It copies the transcript with `wl-copy` and then presses **one Ctrl+V** by raw keycode
— Ctrl+Shift+V when the focused window's class is in `[inject] terminal_classes`, since
terminals reserve plain Ctrl+V for whatever runs inside them. Raw keycodes, not `ydotool
type`, because `type` maps characters through a hard-coded US-QWERTY table: on a German
layout it swaps z/y and drops umlauts and ß outright, while key *positions* are the same
on every layout and pasted text arrives as whatever UTF-8 the clipboard holds.

Set up `ydotoold` yourself — yappr never does it for you. The one thing worth knowing:
the daemon takes `--socket-path`, but the client looks at `$YDOTOOL_SOCKET` and
otherwise `$XDG_RUNTIME_DIR/.ydotool_socket`. A unit that starts it anywhere else (the
widely copy-pasted `%h/.ydotool_socket` is the usual culprit) breaks every paste, and
yappr cannot fix that from its side: it starts from the tray or a `.desktop` entry and
inherits no shell export.

**`paste_chord` is the setting to reach for when a terminal gets nothing.** `auto` needs
to know the focused window's class, and that can come back unknown — no `hyprctl` and no
accessibility answer, a Hyprland session whose environment never got
`HYPRLAND_INSTANCE_SIGNATURE`, nothing focused, an Electron or Qt app registered with no
accessibility bus. Unknown reads as "not a terminal", so `auto` sends plain Ctrl+V, the
terminal ignores it, `ydotool` exits 0 and nothing reports a failure: you just get no
text. Setting `ctrl_shift_v` fixes that outright, and browsers and Electron read it as
"paste as plain text" anyway, which is what you want for dictation. `yappr --debug`
prints the class yappr actually saw, as `window_class`.

#### Your own script

For a desktop neither of those answers for, the `script` backend hands the finished
transcript to a program of your own and lets it do the inserting.

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

That one is the `ydotool` backend, written out — use the backend unless you need
something it does not do. A script using `wtype`, `xdotool`, `dotool`, `gdbus` or a
GNOME extension is equally valid, and so is one that restores the previous clipboard
afterwards, which is the usual reason to write your own.

**If it fails, you still get your text.** A non-zero exit, a missing or non-executable
file, more than 15 seconds without returning, or an empty `[inject] script` all fall back
to copying the transcript to the clipboard, with a desktop notification saying so. That
is invariant 1: once speech has been transcribed, the text reaches you somehow.

Your script's stdout and stderr are logged even when it exits 0, so a `echo` is a
perfectly good way to see what it decided. `yappr --debug` prints the last dictation's
record, including the target window class yappr saw (it does not pass that to your
script — a script that wants it asks its own desktop).

#### If you used `ydotool` on 0.2.5 or 0.2.6

Those two releases retired the backend: `backend = "ydotool"` still loaded, but it was
read as `script`, and with no `script` set every dictation went to the clipboard with a
notification instead of being pasted. 0.2.7 restores it as a real backend, and
`paste_chord` / `terminal_classes` are real settings again.

If your config still says `ydotool`, it now means `ydotool` again and there is nothing
to do. If anything saved your config while you were on 0.2.5 or 0.2.6, the word was
rewritten to `script` — set *Verfahren* back to `ydotool` in Settings, or edit the file:
yappr will not guess which of the two you meant. Your `paste_chord` and
`terminal_classes` were dropped from the file by that same save; they fall back to their
defaults (`auto`, and the shipped terminal list), so re-enter them only if you had
changed them.

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
| **Nothing gets typed, but the overlay says it worked** | `wtype` can't reach that window — you're on GNOME, or it's an XWayland/Electron window. Check the clipboard: the text is probably there. Fix it with the [`libei`, `ydotool` or `script` backend](#pasting-where-wtype-cant-type). |
| **The `libei` backend pastes nothing** | Run `cargo run -p yappr-core --example libei_probe -- "hallo"`, which prints why. `Session creation inhibited` means the screen was locked when yappr asked — unlock and it retries. A portal that answers nothing at all means your desktop has no `RemoteDesktop` implementation (`xdg-desktop-portal-wlr`); use `wtype` or `script` there. If it pastes everywhere *except* terminals, it's the chord, not the portal: set `paste_chord = "ctrl_shift_v"`. |
| **The `ydotool` backend pastes nothing** | Usually `ydotoold`: not running, or listening on a socket the client doesn't look at (see above). yappr logs what `ydotool` said on *both* streams — it reports this one on stdout — and falls back to the clipboard with a notification. If it pastes everywhere *except* terminals, it's the chord, not the daemon: set `paste_chord = "ctrl_shift_v"`. |
| **The `script` backend produced nothing and a notification says it fell back to the clipboard** | Your script failed. Run it by hand — `~/bin/paste.sh "hallo"` — and watch what it says; yappr logs its stdout and stderr either way. The usual causes are a path that isn't executable (`chmod +x`), a wrong `[inject] script` path, and a helper it calls (`ydotoold`, `wl-copy`) not being up. `cargo run -p yappr-core --example script_probe -- "hallo"` runs it exactly the way yappr does. |
| **Nothing is pasted *into a terminal*** | Plain Ctrl+V was sent, which terminals ignore. Under `ydotool` or `libei`: set `paste_chord = "ctrl_shift_v"`, or add the window class `yappr --debug` reports to `terminal_classes`. Under `script`: that choice is your script's — have it check the focused window's class, or send Ctrl+Shift+V unconditionally, which browsers and Electron read as "paste as plain text" anyway. |
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
