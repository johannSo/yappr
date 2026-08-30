<div align="center">

# yappr

**Local dictation for Wayland.** Press `SUPER+D`, speak, press `SUPER+D` again —
what you said is typed into whatever window has focus, punctuated and tidied up.
No cloud, no account, no network calls while you dictate.

</div>

---

| | |
|---|---|
| **Start / stop dictating** | `SUPER+D` |
| **Cancel the current recording** | `SUPER+ALT+D` |
| **Settings** | left-click the tray icon |
| **Everything else** | right-click the tray icon |

**Jump to:** [Install](#install) · [First run](#first-run) · [Using it](#using-it) ·
[Settings](#settings) · [Troubleshooting](#troubleshooting) ·
[How it works](#how-it-works) · [Limitations](#known-limitations)

## What it is

One binary, `yappr`, and one process. Start it and you get a tray icon and nothing
else — no window in your way. Your dictation shortcut runs the *same* binary with
`--toggle`, which is a thin client: it hands the running app a one-line message over
a Unix socket and exits, so pressing the key costs nothing.

Everything happens on your machine: speech recognition (Parakeet), silence trimming
(Silero VAD), and the clean-up pass that adds punctuation and casing (S1-mini, run
locally through `llama-server`). About 1.1 GB of models are downloaded once, on first
run, and — by default — loaded only while you are actually dictating.

Tested on Hyprland; works on GNOME and other Wayland desktops with one extra step
(see [Desktop notes](#desktop-notes)).

## Install

### 1. Install the system packages

**Arch / Omarchy:**

```bash
sudo pacman -S llama-cpp ggml-cpu wtype wl-clipboard gtk-layer-shell
```

| Package | Why |
|---|---|
| `llama-cpp` | provides `llama-server`, which runs the clean-up model |
| **`ggml-cpu`** | **easy to miss.** Arch's `llama-cpp` pulls in base `ggml`, which has *no compute backend*. Without this, `llama-server` fails with the opaque *"no backends are loaded"*. Use `ggml-vulkan` / `ggml-cuda` instead (or as well) if you have the hardware — yappr passes no backend flags, so `llama-server` picks the best one it finds. |
| `wtype` | types the finished text into the focused window |
| `wl-clipboard` | provides `wl-copy`, the fallback when typing fails |
| **`gtk-layer-shell`** | **build-time dependency.** The overlay places and un-focuses itself via `wlr-layer-shell`, and the Rust binding links against this library at compile time. Without it `cargo build` fails before it produces a binary — there is no runtime fallback for its absence. |

Optional:

| Package | Why |
|---|---|
| `ydotool` | a second typing backend, for windows `wtype` cannot reach — **required on GNOME**, see [Desktop notes](#desktop-notes) |
| `hyprland` (`hyprctl`) | lets per-application style rules see the focused window's class |
| `libnotify` (`notify-send`) | desktop notifications for "typing failed — copied to clipboard" and similar |

yappr re-checks this list itself during setup and names the exact missing package,
so you can also just skip ahead and let the wizard tell you.

### 2. Build and install `yappr`

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

## First run

Launch yappr — from your app launcher, or `yappr &`. A **setup wizard** opens
automatically and walks you through four steps:

1. **Willkommen** — what you're about to do.
2. **Modelle laden** — checks the packages above (naming the missing `pacman` package
   if there is one) and downloads ~1.1 GB of models: Parakeet TDT 0.6b v3 (speech
   recognition), Silero VAD, and S1-mini by Superwhisper (clean-up). Each file is
   verified against a pinned sha256. The download doesn't block you — hit **Weiter**
   and read on while it runs.
3. **Kurzbefehl einrichten** — detects your desktop and shows the exact lines to paste
   for it, with a copy button. Wayland has no global keyboard grab, so yappr cannot
   register the shortcut itself, and it deliberately never edits your desktop config
   for you. This step also tells you which typing backend your desktop needs.
4. **Fertig** — press `SUPER+D` and start talking.

Models land in `~/.local/share/yappr/models`. Finishing the wizard writes a marker at
`~/.local/state/yappr/wizard-done`.

**The wizard comes back if the install stops being usable** — if a model file goes
missing, you get the wizard at the models step rather than a failed dictation days
later. You can also reopen it any time: tray → **Einrichtung…**, or `yappr --wizard`.

## Using it

### The gesture

Press `SUPER+D` to start recording. Speak. Press `SUPER+D` again to stop — the text
appears in the focused window a moment later. `SUPER+ALT+D` throws the recording away.

There is **no key to release**: a recording keeps going until you press again. If you
forget, a watchdog ends it after `audio.max_seconds` (120 s by default) and *transcribes
what it captured* rather than discarding it. If 120 seconds of open microphone isn't a
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
`~/.config/autostart/yappr.desktop`. No Hyprland `exec-once` line to add, no systemd
unit to install — `xdg-autostart-generator` handles that. Off by default.

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
  [Typing with ydotool](#typing-with-ydotool) for the one-time setup.
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
700 ms after you stop typing. Saving rewrites `config.toml` in place: your comments and
formatting survive, a save that changes nothing leaves the file byte-identical, and a
config that wouldn't load is rejected before anything is written. Every row has a reset
button that appears only when the value isn't the default.

**`[asr]` and `[normalize]` changes need a restart** (`yappr --quit`, then launch
again); the window says so on those rows. Everything else — the microphone included —
applies at your next dictation, or immediately with `yappr --reload`.

## config.toml

Lives at `~/.config/yappr/config.toml`, created with commented defaults on first run.
You can edit it by hand; the GUI is careful not to trample it.

| Section | What's in it |
|---|---|
| `[audio]` | `device`, `max_seconds` (the only thing that ends a forgotten recording), `vad_padding_ms` |
| `[models]` | `preload_at_startup`, `idle_unload_seconds` — see [Memory use](#memory-use) |
| `[asr]` | `num_threads` for Parakeet |
| `[normalize]` | `enabled` (`false` skips S1-mini and types rule-cleaned raw text), plus `port`, `timeout_ms`, `llama_server_path`, `context_size`, `threads` |
| `[guardrail]` | `min_word_ratio`/`max_word_ratio`, `min_overlap_english`/`min_overlap_other`, `short_input_words`, `ngram_size`/`ngram_max_repeats` |
| `[inject]` | `backend` (`wtype`, `ydotool`, `clipboard`), `trailing_space`, `keystroke_delay_ms` |
| `[vocabulary]` | terms and replacements applied to the raw transcript before clean-up — put short acronyms in `replacements`, not `terms` |
| `[style_default]`, `[[style_rules]]` | the `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched on window class (regex) |
| `[debug]` | `enabled` (off), `dir` (default `~/yappr`), `save_audio` — see [Troubleshooting](#troubleshooting) |
| `[overlay]` | `position`, `width`, `height` — read, but inert: under Wayland a window can't place itself, so this changes nothing today |

> **Unknown keys are a hard error.** Every section is `deny_unknown_fields`: a typo'd
> key stops the app from starting rather than being silently ignored.

`yappr --reload` re-validates the file against the running app and applies every
reloadable section live. It refuses outright — rather than half-applying — if you
changed `[asr]` or `[normalize]`.

### Typing with `ydotool`

`wtype` is the default and needs no setup: it types through the compositor's own
virtual-keyboard protocol. But it does nothing on GNOME, and it's known to drop
keystrokes in some XWayland and Electron windows. `ydotool` types through the kernel's
`/dev/uinput` instead, which no window can tell apart from a real keyboard — at the cost
of some setup, which is why it isn't the default.

```bash
sudo pacman -S ydotool
systemctl --user enable --now ydotool.service   # the unit is named for the package,
                                                # not for ydotoold
```

On Arch that's usually all of it — the package ships a udev rule giving the `input`
group write access to `/dev/uinput`, and Arch already puts desktop users in `input`.
Confirm before reaching for `sudo`:

```bash
ls -l /dev/uinput     # want: crw-rw---- 1 root input
id -nG | grep input   # want: your user is in the input group
```

Only if the group is missing do you need `sudo usermod -aG input "$USER"` and a
re-login; only if the device shows `crw------- root root` do you need the udev rule by
hand. On a distro that ships neither, do both.

Then set it in Settings → Allgemein → Texteingabe → Verfahren, or:

```toml
[inject]
backend = "ydotool"
```

Notes: client and daemon both default to `$XDG_RUNTIME_DIR/.ydotool_socket`, so a
systemd user unit and yappr agree without configuration — you only need
`YDOTOOL_SOCKET` if you run `ydotoold --socket-path`, and then it must be exported in
the *session* environment, not just your shell rc. And `keystroke_delay_ms` means
something slightly different here: `ydotool` applies it per key *event*, so each
character costs twice the configured delay. If `ydotool` fails, the clipboard fallback
catches the transcript exactly as it does for a failing `wtype`.

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
| **Nothing gets typed, but the overlay says it worked** | `wtype` can't reach that window — you're on GNOME, or it's an XWayland/Electron window. Switch to [`ydotool`](#typing-with-ydotool). Check the clipboard: the text is probably there. |
| **`llama-server` fails with "no backends are loaded"** | Missing ggml compute backend: `sudo pacman -S ggml-cpu`. |
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
6. **Normalization** — S1-mini, served locally by a supervised `llama-server`, rewrites the transcript into punctuated prose (or a list, depending on the style axes).
7. **Guardrail → Injection** — cheap deterministic checks (word-count ratio, token overlap, n-gram loop detection, template-bleed detection) decide whether to trust the rewrite. If not, you get the raw transcript or a rule-based clean-up of it instead of something the model invented; the rejection is appended to `rejections.jsonl`. Then `wtype` types it into the focused window.

If `llama-server` dies mid-session, it's restarted automatically (10 s health poll,
backoff from 1 s to 30 s) and dictation degrades to raw ASR text in the meantime rather
than failing.

### Measured performance

On an i7-10510U (4 threads), release build:

| Stage | Latency |
|---|---|
| ASR (11.5 s of audio) | 1,700 ms (RTF 0.147) |
| S1-mini clean-up, 8-word transcript | 438 ms |
| S1-mini clean-up, 26-word transcript | 585 ms |
| S1-mini clean-up, 58-word transcript | 1,313 ms |
| `llama-server` cold start (once, at first press) | 751 ms |
| ASR model load (once, at first press) | 3,497 ms |

A typical ~10 s dictation, warm, is roughly **2 seconds** from the second press to text
appearing.

## Known limitations

Found during development, not yet fixed — worth knowing before relying on this:

- **Short utterances are effectively unguarded.** Below `guardrail.short_input_words`
  raw words (4 by default), the ratio and overlap checks are skipped, so a short rewrite
  that inverts what you said could be typed verbatim. Longer dictations are checked
  normally.
- **Dense digit sequences fall back to raw ASR text.** Dictating a phone number as
  separate digits normalizes to far fewer tokens ("555-1234"), which trips
  `min_word_ratio` before the (faithful) rewrite is ever checked for overlap. Safe, but
  not the cleaned-up form you'd expect.
- **Nothing physically ends a recording.** A missed second press keeps the microphone
  open until `audio.max_seconds`; the only warning before then is on screen. That's the
  price of press/press instead of hold-to-talk.

Both of the first two await guardrail threshold tuning against real `rejections.jsonl`
data.

### Not yet verified end to end

**No one has spoken into this build yet.** The pipeline is covered by tests and replay
runs, but real dictation — microphone to typed text — has never been performed on real
hardware. Treat claims about dictation quality, timing under load, and per-application
injection behaviour as design intent, not results. `HANDOVER.md` records what else could
not be verified and why.

<details>
<summary>The checklist a first human user should work through</summary>

- [ ] Dictation into Alacritty, Firefox's address bar, an Electron app (VS Code/Slack), and an XWayland window (`xterm` or a Wine app)
- [ ] `yappr --status` reports `warm: true` right after a first `--toggle`, and `warm: false` again once `idle_unload_seconds` has passed — `warm` means "models are resident right now", not "startup finished", so under the default lazy config it stays `false` until the first press
- [ ] Pressing `SUPER+D` twice without speaking types nothing and logs "no speech detected"
- [ ] `SUPER+ALT+D` mid-recording cancels cleanly
- [ ] Two dictations back-to-back without a pause don't run together
- [ ] Killing `llama-server` mid-session still types raw ASR text (with a logged warning) instead of failing
- [ ] A German dictation, and whether the guardrail fires for it (check `rejections.jsonl`)
- [ ] `~/.local/state/yappr/rejections.jsonl` contains valid JSON, one object per line
- [ ] A second `yappr` instance refuses to start with "already running"
- [ ] `yappr --toggle` with no instance running exits non-zero and raises a visible notification, rather than doing nothing silently
- [ ] Forgetting the second press: the watchdog ends the recording at `audio.max_seconds` and types what it captured
- [ ] Stopwatch timing from the second press to text appearing, against the table above

</details>

## Upgrading from an older build

<details>
<summary>This project was called <b>openwhisprflow</b> until 2026-08-29, and before that
shipped several binaries. Expand if you ever installed one of those.</summary>

Delete every old binary — a stale one on your `PATH` keeps working, and keeps running
the *old* code:

```bash
rm -f ~/.local/bin/owf-ctl ~/.local/bin/owf-daemon ~/.local/bin/owf-bench \
      ~/.local/bin/openwhisprflow ~/.local/bin/openwhisprflow-settings
```

`~/.local/bin/openwhisprflow` is the dangerous one. The pre-rename one-process build
kept the old overlay's name, so it and the *previous* overlay-only app were the same
filename and either could shadow the other. That old app parses no arguments at all:
run it as `openwhisprflow --toggle` and it never touches the socket — it opens *its own*
overlay, subscribes to whatever is listening, and draws a second pill from the same
events. Same waveform, same timer, no error anywhere. (Observed on real hardware.)

`yappr` can't collide with it, so this is now plain cleanup:

```bash
install -m755 target/release/yappr ~/.local/bin/
which -a yappr                # expect exactly one path
which -a openwhisprflow       # expect nothing
```

If you run the AppImage instead of installing, symlink `~/.local/bin/yappr` at it.

**Then fix your desktop config — this is a breaking change for shortcuts too.** Old
setups have `exec-once = owf-ctl daemon`, `exec-once = openwhisprflow`, a `bind`/`bindr`
pair calling `owf-ctl ptt-start`/`ptt-stop`, and a window rule on
`class:^(openwhisprflow)$`. Post-rewrite, pre-rename setups have binds calling
`openwhisprflow --toggle`/`--cancel` and a rule on `title:^(openwhisprflow overlay)$`.
All of it now fails silently. The class-matched rule is worse than dead weight: the
settings window is a window of this same app now, so that rule also matches it, and you
cannot type into the settings form until you delete it.

`yappr --print-shortcuts` leads with exactly which lines to delete, before the ones to
add. Paste the whole thing and follow it. Autostart is now the Settings toggle, not an
`exec-once` line.

</details>
