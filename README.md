# OpenWhisprFlow

Press-to-start, press-to-stop dictation for Hyprland. Press `SUPER+D`, speak,
press `SUPER+D` again: the audio is transcribed, cleaned up, and typed into
whatever window has focus. `SUPER+ALT+D` cancels a recording in progress.
Everything runs locally — no network calls at dictation time.

## How it works

`openwhisprflow` is the only binary. Launching it with no arguments starts
everything: a tray icon (no window), the pipeline, and a Unix socket. A
shortcut you bind yourself (see below) runs the same binary with `--toggle` or
`--cancel` — that invocation is a thin client that talks to the running app
over the socket and exits immediately, so pressing the shortcut does not pay
the cost of starting a whole app.

One dictation goes through this pipeline:

1. **Capture** — the microphone records at 16 kHz mono from the first press until the second.
2. **VAD** — Silero VAD trims leading and trailing silence from the recording.
3. **ASR** — [Parakeet TDT 0.6b v3](https://github.com/k2-fsa/sherpa-onnx) (int8, via `sherpa-onnx`) transcribes the trimmed audio.
4. **Language ID** — `whatlang` tags the transcript's language, which selects the guardrail's overlap threshold below.
5. **Normalization** — **S1-mini** by **Superwhisper**, served locally through a supervised `llama-server`, rewrites the raw transcript into cleaned, punctuated prose (or a list, depending on style).
6. **Guardrail** — a set of cheap, deterministic checks (word-count ratio, token overlap, n-gram loop detection, template-bleed detection) decides whether S1-mini's rewrite is trustworthy. If not, the pipeline falls back to the raw ASR text (or a light rule-based cleanup of it) instead of typing something S1-mini invented. Every rejection is appended to `rejections.jsonl` for later review.
7. **Injection** — `wtype` types the result into the focused window; if that fails, the text is copied to the clipboard with `wl-copy` instead, and a notification says so. `[inject] backend = "ydotool"` swaps in `ydotool` for windows `wtype` cannot reach (see below).

`llama-server` is started once, supervised, when the app starts, and stays warm
for as long as the app runs — normalization only pays a network round-trip to
`localhost`, not a model load, per dictation.

There is no second press that ends a recording on its own the way releasing a
key used to. If you forget to press again, a watchdog ends the recording after
`audio.max_seconds` (120 s by default) and transcribes whatever it captured —
it does not discard it. See [Configuration](#configuration) if you want that
number lower.

## Measured performance

Measured on an i7-10510U (4 threads), release build:

| Stage | Latency |
|---|---|
| ASR (11.5 s of audio) | 1,700 ms (RTF 0.147) |
| S1-mini normalization, 8-word transcript | 438 ms |
| S1-mini normalization, 26-word transcript | 585 ms |
| S1-mini normalization, 58-word transcript | 1,313 ms |
| `llama-server` cold start (once, at app start) | 751 ms |
| ASR model load (once, at app start) | 3,497 ms |

A typical ~10 s dictation, once the app is warm, is roughly **2 seconds
end to end** — second press to text appearing.

## Prerequisites

- [`rustup`](https://rustup.rs/) (to build from source)
- `llama-cpp` — provides `llama-server`, used for S1-mini normalization
- **`ggml-cpu`** — Arch's `llama-cpp` package depends only on the base `ggml`
  package, which ships `libggml-base.so` but **no compute backend**. Without
  one, `llama-server` fails to load any model with ggml's own opaque *"no
  backends are loaded"* error. Install the CPU backend explicitly:
  ```bash
  sudo pacman -S ggml-cpu
  ```
  (Use `ggml-vulkan`, `ggml-cuda`, etc. instead/as well if you have the
  matching hardware and want it used — `owf-core` does not pass any backend
  selection flags, so `llama-server` picks the best one it finds.)
- **`gtk-layer-shell`** — needed to *build* the app at all, not just to run it:
  the overlay positions and unfocuses itself via `wlr-layer-shell` on Hyprland
  and other wlroots compositors, and the Rust binding to that library links
  against it at compile time. Without it, `cargo build` fails outright before
  producing a binary. Install it first:
  ```bash
  sudo pacman -S gtk-layer-shell
  ```
- `wtype` — types the cleaned transcript into the focused window
- `wl-clipboard` (provides `wl-copy`) — clipboard fallback when typing fails
- `ydotool` — optional; only if you switch `[inject] backend` to it (see below). Not needed otherwise.
- Hyprland (`hyprctl`) — optional; used to look up the focused window's class for per-application style rules, and for `wlr-layer-shell` overlay placement. Everything else works without it, including on GNOME, with a plainer (unpositioned) overlay.

## Install

```bash
cargo build --release -p openwhisprflow --features custom-protocol
mkdir -p ~/.local/bin
install -m755 target/release/openwhisprflow ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH`.

To find it in an app launcher (rofi, wofi, GNOME Shell, …), add a launcher
entry — this is the same `.desktop` shape and the same bare, `PATH`-relative
`Exec=` line the autostart toggle below writes for itself:

```bash
mkdir -p ~/.local/share/applications
cat > ~/.local/share/applications/openwhisprflow.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=OpenWhisprFlow
Comment=Startet das Diktat-Overlay im Hintergrund
Exec=openwhisprflow
Terminal=false
Categories=Utility;AudioVideo;
EOF
```

### Upgrading from an older, multi-binary build

If you built this project before the one-process rewrite, delete every binary
that no longer exists — a stale one on your `PATH` keeps working and keeps
running the *old* code, which is the worst possible failure mode:

```bash
rm -f ~/.local/bin/owf-ctl ~/.local/bin/owf-daemon ~/.local/bin/owf-bench \
      ~/.local/bin/openwhisprflow-settings
```

`~/.local/bin/openwhisprflow` is deliberately *not* on that list — it is the one
name the rewrite kept, so deleting it would delete the new build too. It is also
the most dangerous leftover of the three, because it fails silently rather than
loudly: the pre-rewrite binary of that name is the overlay-only Tauri app, which
parses no arguments at all. Run it as `openwhisprflow --toggle` and it never
touches the socket — it opens *its own* overlay window, subscribes to whatever
daemon is listening, and draws a second overlay from the same events. Both pills
show the same waveform and the same timer, because they are the same events; one
of them is a ghost. Overwrite it, and confirm afterwards that only one binary
answers to the name:

```bash
install -m755 target/release/openwhisprflow ~/.local/bin/   # overwrites in place
which -a openwhisprflow                                     # expect exactly one path
openwhisprflow --status                                     # expect one line of JSON
```

That last line is the cheap test: the current binary answers `--status` on stdout
and exits. A pre-rewrite binary of the same name prints nothing and opens a
window. If you run the AppImage rather than installing, point `~/.local/bin/openwhisprflow`
at it (`ln -s`) instead of leaving an older real file there — a shortcut, a
`.desktop` entry or a shell that resolves the bare name through `PATH` will
otherwise find the old build and you get the two-overlay symptom above.

Then see [Migrating an existing Hyprland config](#migrating-an-existing-hyprland-config)
below — this is a breaking change for your shortcuts and window rules too, not
just for which binaries exist.

## First run

Launch it — from the app launcher, or `openwhisprflow &`. It starts with no
window, just a tray icon. If the models aren't downloaded yet, left-click the
tray icon (or run `openwhisprflow --settings`) to open Settings; a **Setup**
pane appears automatically. It checks the same prerequisites listed above
(naming the exact `pacman` package if one is missing) and, once they're
satisfied, downloads and verifies (~1.1 GB total):

- Parakeet TDT 0.6b v3 (int8) — the ASR model
- Silero VAD
- S1-mini by Superwhisper (GGUF, q4_k_m)

into `models_dir()` (`$XDG_DATA_HOME/openwhisprflow/models`, typically
`~/.local/share/openwhisprflow/models`), pinned by sha256 in
`models.lock.toml`, with per-model progress shown in the pane. Until this
finishes, the tray shows "warming" and `--toggle` is refused with a stated
reason. Run `openwhisprflow --update-lock` (not part of normal first run) to
re-resolve and re-pin the lock file, e.g. after an upstream model update.

## Bind your shortcuts

```bash
openwhisprflow --print-shortcuts
```

This detects whether your Hyprland is configured in Lua (as Omarchy is) or
classic `.conf` and prints the matching block — never applied for you; paste
it yourself. Hyprland 0.56+ configured in Lua rejects the legacy keyword
parser entirely, so the two formats are not interchangeable.

**Lua config** (`~/.config/hypr/hyprland.lua` exists) — add to
`~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + D", "Dictation: toggle", "openwhisprflow --toggle")
o.bind("SUPER + ALT + D", "Dictation: cancel", "openwhisprflow --cancel")
```

**Classic config** — add to `~/.config/hypr/hyprland.conf`:

```
bind  = SUPER, D,     exec, openwhisprflow --toggle
bind  = SUPER ALT, D, exec, openwhisprflow --cancel
```

Check first that those keys are free — `omarchy menu keybindings --print` on
Omarchy — and unbind anything you are replacing. Then validate:

```bash
hyprctl reload && hyprctl configerrors
```

That's it: two lines, one file, no daemon to start and no autostart line to
add by hand (see [Autostart](#autostart)). Unlike the old hold-to-talk
bindings, there is no release-edge counterpart to pair either one with, so
every desktop gets identical behaviour — on GNOME, add two custom shortcuts
in Settings → Keyboard running the same two commands.

**On Hyprland you don't need a window rule either.** The overlay positions
and unfocuses itself automatically via `wlr-layer-shell`. `--print-shortcuts`
still emits one as a fallback, title-matched rather than class-matched, for
compositors without layer-shell (GNOME/Mutter); paste that block too if
you're on one of those, or if you'd rather have the belt-and-braces version.
It's inert everywhere else.

### Migrating an existing Hyprland config

If you set this up before this rewrite, your config still has the old lines:
`exec-once = owf-ctl daemon`, `exec-once = openwhisprflow`, a `bind`/`bindr`
pair calling `owf-ctl ptt-start`/`ptt-stop`, and a window rule matched on
`class:^(openwhisprflow)$`. All of that is now dead weight, and it fails in
the worst way available: `owf-ctl` no longer exists, so the shortcut silently
does nothing. Worse — the old class-matched window rule now also matches the
*settings* window, since it became a window of this same app, so until you
delete that rule you cannot type into the Settings form at all.

`openwhisprflow --print-shortcuts`'s output leads with exactly what to
delete, before the lines to add — paste the whole thing and follow it.

## Autostart

A **"Beim Anmelden starten"** toggle in Settings writes or removes
`~/.config/autostart/openwhisprflow.desktop`. There is no Hyprland
`exec-once` line to add by hand, and this project installs no systemd unit —
`xdg-autostart-generator` turns the `.desktop` entry into one automatically.
Off by default.

## Settings

Left-click the tray icon, or run `openwhisprflow --settings`. Everything in
`config.toml` is editable there, including the microphone and the dictation
vocabulary. Saving writes the file in place: comments and layout survive, a
save that changes nothing leaves the file byte-identical, and a config that
would not load is rejected before anything is written. Changes to `[asr]` and
`[normalize]` need restarting the app (`--quit`, then launch again); the
window says so. Everything else, the microphone included, takes effect at the
next dictation.

## Configuration

The config file lives at `$XDG_CONFIG_HOME/openwhisprflow/config.toml`
(typically `~/.config/openwhisprflow/config.toml`) and is created with
commented defaults on first run. The main knobs:

- **`[audio]`** — `device`, `max_seconds` (ends a forgotten recording — under press/press toggle nothing else does; see "How it works" above), `vad_padding_ms` (silence kept around trimmed speech).
- **`[asr]`** — `num_threads` for Parakeet.
- **`[normalize]`** — `enabled` (set `false` to skip S1-mini entirely and type rule-based-cleaned raw ASR text), `port`/`timeout_ms`/`llama_server_path`/`context_size`/`threads` for the supervised `llama-server`.
- **`[guardrail]`** — `min_word_ratio`/`max_word_ratio`, `min_overlap_english`/`min_overlap_other`, `short_input_words` (below this many raw words, the ratio/overlap checks are skipped — see Known limitations), `ngram_size`/`ngram_max_repeats` (loop detection).
- **`[inject]`** — `backend` (`wtype`, `ydotool` or `clipboard`), `trailing_space`, `keystroke_delay_ms`.
- **`[style_default]`** and **`[[style_rules]]`** — the default `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched by focused window class (regex).
- **`[debug]`** — `enabled` (off by default), `dir` (default `~/owf`), `save_audio`. When enabled, every utterance writes a WAV of the raw capture and the post-VAD-trim buffer under `<dir>/audio/`, plus a JSON diagnostic record (capture stats — device, native rate, samples captured vs. expected, stream error count — audio RMS/peak, VAD span, ASR/normalize/guardrail/inject results, and timings) under `<dir>/logs/`. The app's own log is also mirrored to `<dir>/logs/daemon.log` (append) while enabled. Run `openwhisprflow --debug` to print a summary of the most recent record and the paths to its files.

`openwhisprflow --reload` re-validates `config.toml` against the running app
and applies every reloadable section to the live pipeline immediately —
`[guardrail]`, `[inject]`, `[style_default]`/`[[style_rules]]`, the
vocabulary, and more. Only `[asr]` and `[normalize]` still need a restart (see
Settings above); `--reload` refuses outright rather than silently leaving the
old normalizer in place if you try to change those without one.

### Typing with `ydotool`

`wtype` is the default and needs no setup: it types through the compositor's
own virtual-keyboard protocol. It is also known to drop or ignore keystrokes in
some XWayland and Electron windows. `[inject] backend = "ydotool"` types
through the kernel's `/dev/uinput` instead, which those windows cannot tell
apart from a real keyboard — at the cost of some setup, which is why it is not
the default:

```bash
sudo pacman -S ydotool

# /dev/uinput is root-only by default. Grant the input group write access:
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/80-uinput.rules
sudo usermod -aG input "$USER"          # log out and back in for this to apply

systemctl --user enable --now ydotoold
```

`ydotool` talks to `ydotoold` over a socket. If your `ydotoold` does not use
the default path, export `YDOTOOL_SOCKET` in the session environment
OpenWhisprFlow itself starts in — a value set only in your shell's rc file will
not reach a tray app started by the session.

Then set the backend in Settings → Allgemein → Texteingabe → Verfahren, or in
`config.toml`:

```toml
[inject]
backend = "ydotool"
```

If any of that is missing, `ydotool` exits non-zero, and the clipboard fallback
carries the transcript exactly as it does for a failing `wtype` — you get a
"typing failed — copied to clipboard" notification rather than a lost dictation.
Note that `keystroke_delay_ms` means something slightly different here:
`ydotool` applies it per key event, so a character costs twice the configured
delay.

## Known limitations

Found during development, not yet fixed — worth knowing before relying on this:

- **Short utterances are effectively unguarded.** Below `guardrail.short_input_words` raw words (4 by default), the ratio and overlap checks are skipped entirely, so a short S1-mini rewrite that inverts the meaning of what was said could be typed verbatim. Longer dictations are checked normally.
- **Dense digit sequences get rejected back to raw ASR text.** Dictating a phone number as separate digits ("five five five one two three four") normalizes to a much shorter token count ("555-1234"), which trips `min_word_ratio` before the (faithful) rewrite is ever evaluated for overlap. The fallback is safe — you get the raw ASR text, not silence or garbage — but not the cleaned-up form you'd expect. This is pinned by a test named `known_limitation_dense_digit_sequences_trip_word_ratio` in `crates/owf-core/src/guardrail.rs`.
- **Nothing physically ends a recording under press/press toggle.** A missed second press keeps the microphone open until `audio.max_seconds` (see "How it works"); the only warning before then is on screen. This is the price of dropping hold-to-talk, not a bug.

Both of the first two are slated for guardrail threshold tuning against real
`rejections.jsonl` data in a later milestone.

## Manual end-to-end verification — not yet run

The following has **not** been performed on real hardware and is not
claimed to work; it requires a human speaking into a microphone and
watching the result. Treat dictation as unverified until this is done:

- [ ] `openwhisprflow --status` reports `warm: true` ~45 s after the app starts
- [ ] Dictation into Alacritty, Firefox's address bar, an Electron app (VS Code/Slack), and an XWayland window (`xterm` or a Wine app)
- [ ] Pressing `SUPER+D` and pressing it again immediately without speaking types nothing and logs "no speech detected"
- [ ] `SUPER+ALT+D` while a recording is in progress cancels cleanly
- [ ] Two dictations back-to-back without a pause don't run together
- [ ] Killing `llama-server` mid-session still types raw ASR text (with a logged warning) instead of failing
- [ ] A German dictation, and whether the guardrail fires for it (check `rejections.jsonl`)
- [ ] `~/.local/state/openwhisprflow/rejections.jsonl` contains valid JSON, one object per line
- [ ] A second `openwhisprflow` instance refuses to start with "already running"
- [ ] `openwhisprflow --toggle` with no instance running exits non-zero and raises a visible desktop notification, rather than doing nothing silently
- [ ] Forgetting the second press: the watchdog ends the recording at `audio.max_seconds` and types what it captured, rather than losing it
- [ ] Stopwatch timing from the second press to text appearing, for comparison against the estimate above

This machine's own build has never had a human speak into it either — see
`HANDOVER.md` — so treat every claim in this file about dictation quality,
timing under load, or per-application injection behaviour as design intent,
not a verified result.
