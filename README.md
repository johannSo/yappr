# OpenWhisprFlow

Push-to-talk dictation for Hyprland. Hold `SUPER+D`, speak, release: the
audio is transcribed, cleaned up, and typed into whatever window has focus.
Everything runs locally — no network calls at dictation time.

## How it works

`owf-daemon` is a long-lived background process that owns a Unix socket.
`owf-ctl` is the thin client a Hyprland keybind runs on key press/release.
One dictation goes through this pipeline:

1. **Capture** — the microphone is recorded at 16 kHz mono while the key is held.
2. **VAD** — Silero VAD trims leading and trailing silence from the recording.
3. **ASR** — [Parakeet TDT 0.6b v3](https://github.com/k2-fsa/sherpa-onnx) (int8, via `sherpa-onnx`) transcribes the trimmed audio.
4. **Language ID** — `whatlang` tags the transcript's language, which selects the guardrail's overlap threshold below.
5. **Normalization** — **S1-mini** by **Superwhisper**, served locally through a supervised `llama-server`, rewrites the raw transcript into cleaned, punctuated prose (or a list, depending on style).
6. **Guardrail** — a set of cheap, deterministic checks (word-count ratio, token overlap, n-gram loop detection, template-bleed detection) decides whether S1-mini's rewrite is trustworthy. If not, the pipeline falls back to the raw ASR text (or a light rule-based cleanup of it) instead of typing something S1-mini invented. Every rejection is appended to `rejections.jsonl` for later review.
7. **Injection** — `wtype` types the result into the focused window; if that fails, the text is copied to the clipboard with `wl-copy` instead, and a notification says so.

`llama-server` is started once, supervised, when the daemon starts, and stays
warm for the life of the daemon — normalization only pays a network
round-trip to `localhost`, not a model load, per dictation.

## Measured performance

Measured on an i7-10510U (4 threads), release build:

| Stage | Latency |
|---|---|
| ASR (11.5 s of audio) | 1,700 ms (RTF 0.147) |
| S1-mini normalization, 8-word transcript | 438 ms |
| S1-mini normalization, 26-word transcript | 585 ms |
| S1-mini normalization, 58-word transcript | 1,313 ms |
| `llama-server` cold start (once, at daemon start) | 751 ms |
| ASR model load (once, at daemon start) | 3,497 ms |

A typical ~10 s dictation, once the daemon is warm, is roughly **2 seconds
end to end** — key release to text appearing.

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
- `wtype` — types the cleaned transcript into the focused window
- `wl-clipboard` (provides `wl-copy`) — clipboard fallback when typing fails
- Hyprland (`hyprctl`) — optional; used to look up the focused window's class for per-application style rules. Everything else works without it.

`owf-ctl setup` checks all of the above before doing anything else — see
below.

## Install

```bash
cargo build --release -p owf-cli
mkdir -p ~/.local/bin
install -m755 target/release/owf-daemon target/release/owf-ctl ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH`.

## Set up models

```bash
owf-ctl setup
```

This first prints a prerequisite check — one `ok` / `MISSING` / `absent
(optional)` line per requirement — and aborts with an actionable message
(naming the exact `pacman` packages to install) if anything essential is
missing, **before** downloading anything. Once prerequisites are satisfied it
downloads and verifies (~1.1 GB total):

- Parakeet TDT 0.6b v3 (int8) — the ASR model
- Silero VAD
- S1-mini by Superwhisper (GGUF, q4_k_m)

into `models_dir()` (`$XDG_DATA_HOME/openwhisprflow/models`, typically
`~/.local/share/openwhisprflow/models`), pinned by sha256 in
`models.lock.toml`. Run `owf-ctl setup --update-lock` instead to re-resolve
and re-pin the lock file (e.g. after an upstream model update).

## Wire up Hyprland

```bash
owf-ctl setup --print-hypr >> ~/.config/hypr/hyprland.conf
hyprctl reload
```

This appends the keybinds (`SUPER+D` to hold-to-talk, `SUPER+Escape` to
cancel while held) and the window rules that keep the future overlay window
from stealing focus during dictation. Log out and back in (so
`exec-once = owf-daemon` starts the daemon), or start it manually once:

```bash
owf-daemon &
```

## Configuration

The config file lives at `$XDG_CONFIG_HOME/openwhisprflow/config.toml`
(typically `~/.config/openwhisprflow/config.toml`) and is created with
commented defaults on first run. The main knobs:

- **`[audio]`** — `device`, `max_seconds` (recording cutoff), `vad_padding_ms` (silence kept around trimmed speech).
- **`[asr]`** — `num_threads` for Parakeet.
- **`[normalize]`** — `enabled` (set `false` to skip S1-mini entirely and type rule-based-cleaned raw ASR text), `port`/`timeout_ms`/`llama_server_path`/`context_size`/`threads` for the supervised `llama-server`.
- **`[guardrail]`** — `min_word_ratio`/`max_word_ratio`, `min_overlap_english`/`min_overlap_other`, `short_input_words` (below this many raw words, the ratio/overlap checks are skipped — see Known limitations), `ngram_size`/`ngram_max_repeats` (loop detection).
- **`[inject]`** — `backend` (`wtype` or `clipboard`), `trailing_space`, `keystroke_delay_ms`.
- **`[style_default]`** and **`[[style_rules]]`** — the default `styling`/`structure`/`context` axes S1-mini is prompted with, and per-application overrides matched by focused window class (regex).

`owf-ctl reload` re-validates the file on disk against the running daemon;
it does not yet hot-swap a running pipeline (see Known limitations /
deferred work).

## Known limitations

Found during development, not yet fixed — worth knowing before relying on this:

- **Short utterances are effectively unguarded.** Below `guardrail.short_input_words` raw words (4 by default), the ratio and overlap checks are skipped entirely, so a short S1-mini rewrite that inverts the meaning of what was said could be typed verbatim. Longer dictations are checked normally.
- **Dense digit sequences get rejected back to raw ASR text.** Dictating a phone number as separate digits ("five five five one two three four") normalizes to a much shorter token count ("555-1234"), which trips `min_word_ratio` before the (faithful) rewrite is ever evaluated for overlap. The fallback is safe — you get the raw ASR text, not silence or garbage — but not the cleaned-up form you'd expect. This is pinned by a test named `known_limitation_dense_digit_sequences_trip_word_ratio` in `crates/owf-core/src/guardrail.rs`.

Both are slated for guardrail threshold tuning against real
`rejections.jsonl` data in a later milestone.

## Manual end-to-end verification — not yet run

The following has **not** been performed on real hardware and is not
claimed to work; it requires a human speaking into a microphone and
watching the result. Treat dictation as unverified until this is done:

- [ ] `owf-ctl status` reports `warm: true` ~45 s after daemon start
- [ ] Dictation into Alacritty, Firefox's address bar, an Electron app (VS Code/Slack), and an XWayland window (`xterm` or a Wine app)
- [ ] Holding `SUPER+D` and releasing without speaking types nothing and logs "no speech detected"
- [ ] `SUPER+Escape` while still holding `SUPER+D` cancels cleanly
- [ ] Two dictations back-to-back without a pause don't run together
- [ ] Killing `llama-server` mid-session still types raw ASR text (with a logged warning) instead of failing
- [ ] A German dictation, and whether the guardrail fires for it (check `rejections.jsonl`)
- [ ] `~/.local/state/openwhisprflow/rejections.jsonl` contains valid JSON, one object per line
- [ ] A second `owf-daemon` refuses to start with "already running"
- [ ] Stopwatch timing from key release to text appearing, for comparison against the estimate above

This machine additionally cannot run `llama-server` at all right now (see
Prerequisites — the `ggml-cpu` package is not installed), so normalization,
the guardrail, and injection have only been exercised through automated
tests, not a live dictation.
