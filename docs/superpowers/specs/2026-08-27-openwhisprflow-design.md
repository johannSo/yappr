# OpenWhisprFlow — Design

**Date:** 2026-08-27
**Status:** Approved, ready for implementation planning

A push-to-talk dictation tool for Hyprland. Hold a key, speak, release; the
text appears in whatever window has focus. Transcription runs on Parakeet TDT
0.6b v3, cleanup runs on Superwhisper's s1-mini, and injection uses `wtype`.
Everything runs locally.

## 1. Scope

In scope for v1:

- Push-to-talk capture bound to a Hyprland key
- Local ASR via Parakeet TDT 0.6b v3 (int8)
- Local transcript normalization via s1-mini (Q4_K_M), guardrailed
- Per-application style control using s1-mini's three control axes
- Text injection via `wtype`, with a clipboard fallback
- A small always-on-top overlay showing pipeline state
- First-run model provisioning with checksum verification

Explicitly **not** in v1:

- Streaming or partial transcripts. The pipeline is batch: release-then-process.
- Toggle-mode recording. Push-to-talk only.
- Transcript history, editing, or a settings GUI. Config is a TOML file.
- ydotool/dotool injection. The `TextInjector` trait exists so this is a
  contained addition later (see §10.3).
- Any non-local network call except model downloads during `owf-ctl setup`.

## 2. Target environment

Verified on the development machine, 2026-08-27:

| Property | Value | Consequence |
|---|---|---|
| CPU | Intel i7-10510U, 4 cores / 8 threads, ULV | CPU-only inference; `-t 4`, not 8 |
| GPU | Intel UHD (Comet Lake), no CUDA/ROCm | No GPU offload path |
| RAM | 15 GiB total, ~6 GiB free | Model residency budget ~1.6 GiB |
| Session | Wayland, Hyprland | **Tauri's global-shortcut plugin does not work.** See §5.2 |
| Installed | `wtype`, `wl-copy`, `pw-record`, `ffmpeg`, `cmake`, `bun`, `uv` | — |
| Missing | `cargo`, `rustc`, `llama-cpp`, model weights | Prerequisites, see §14 |
| Available in `extra` | `rustup`, `llama-cpp`, `ydotool` | — |

The CPU is the binding constraint on this design. Every latency decision below
follows from it.

## 3. Engine selection

| Stage | Engine | Version | Rationale |
|---|---|---|---|
| Hotkey | Hyprland `bind`/`bindr` → `owf-ctl` → Unix socket | — | Wayland has no global keyboard grab; the compositor is the only reliable source of a global hotkey |
| Capture | `cpal` + `rubato` | latest | In-process, no subprocess; gives RMS for the overlay meter without parsing a pipe |
| VAD | Silero VAD, via the `sherpa-onnx` crate | 1.13.6 | ~2 MB, already linked; trims dead air off the ASR input |
| ASR | Parakeet TDT 0.6b v3 int8, via `sherpa-onnx` crate | 1.13.6 | Official k2-fsa Rust bindings; **downloads prebuilt native libs, no local cmake build**; model stays warm in-process |
| Language ID | `whatlang` | 0.18 | See §7.4 — chosen over `lingua` on memory grounds |
| Cleanup | s1-mini Q4_K_M, via `llama-server` | llama.cpp (Arch `extra/llama-cpp`) | The model card's own documented invocation; stays warm; OpenAI-compatible HTTP |
| Injection | `wtype` | 0.4 | Already installed, no root, no uinput rule |
| Shell/UI | Tauri 2 + React 19 + Vite | existing scaffold | Already scaffolded in this repo |

### 3.1 Architecture choice

Considered three placements for the two models:

- **A — ASR in-process (Rust), s1-mini as an `llama-server` sidecar.** Chosen.
  One extra process. Zero IPC on the latency-critical audio path. `llama-server`
  gives the exact documented s1-mini invocation for free.
- **B — both as sidecars** (Python ASR daemon + `llama-server`). Lower build
  risk, easier per-stage debugging, but ~250 MB more resident, slower cold
  start, three processes to supervise. **Retained as the documented fallback**
  if the `sherpa-onnx` crate does not work out (§17.1).
- **C — both in-process Rust** (`sherpa-onnx` + `llama-cpp-2`). Tightest
  memory, but requires compiling llama.cpp on a 2019 ULV laptop, tracks a
  fast-moving binding crate, and forces hand-rolling the chat template that
  s1-mini is documented to be fragile about.

The ASR stage sits behind a `Transcriber` trait specifically so A → B is a
single implementation swap.

## 4. Data flow

```
 SUPER+D press                          SUPER+D release
      |                                       |
      v                                       v
 owf-ctl ptt-start                      owf-ctl ptt-stop
      |                                       |
      +---------------> Unix socket <---------+
                             |
                             v
                    +-----------------+
                    |  State machine  |
                    +-----------------+
                             |
   [Recording] cpal 16kHz mono f32 --> ring buffer --> RMS --> overlay
                             |
   [Transcribing] Silero VAD trim --> Parakeet TDT 0.6b v3 --> raw text
                             |
                    whatlang --> lang tag (sets guardrail strictness)
                             |
   [Normalizing] control line + raw --> llama-server (s1-mini) --> cleaned
                             |
                        Guardrail: accept cleaned, or fall back to raw
                             |
   [Injecting] wtype -- <text>   (on failure: wl-copy + notify)
                             |
                          [Idle]
```

## 5. Process model

### 5.1 Processes

**`openwhisprflow`** — the Tauri application, one instance, autostarted by
Hyprland with `exec-once`. On startup it:

1. Acquires a lock on `$XDG_RUNTIME_DIR/openwhisprflow.lock`; a second instance
   exits immediately with a message.
2. Binds `$XDG_RUNTIME_DIR/openwhisprflow.sock` (removing a stale socket if the
   lock was free).
3. Creates the overlay window hidden.
4. Enters `Warming`: spawns `llama-server`, loads Silero and Parakeet on a
   background thread.
5. Enters `Idle` once both the ASR models are resident and `llama-server`
   answers `GET /health`.

**`owf-ctl`** — a second binary in the same crate (`[[bin]]` alongside the
Tauri bin). It connects to the socket, writes one NDJSON line, reads one NDJSON
line, prints it, exits. It must stay under 10 ms wall clock or the keybind
feels laggy — so it links none of the model crates.

`owf-ctl` has two command families. **Socket commands** (`ptt-start`,
`ptt-stop`, `cancel`, `status`, `reload`) require the app to be running and are
defined in §6. **Local subcommands** (`setup`, `setup --update-lock`,
`setup --print-hypr`, `setup --purge-logs`) run standalone without the app and
touch only the filesystem.

**`llama-server`** — a supervised child. Spawned in its own process group and
killed on app exit (including on `SIGTERM`/panic, via a drop guard). Restarted
with exponential backoff (1s, 2s, 4s, 8s, capped at 30s) if it dies. Health is
polled on `/health` every 10 s while `Idle`.

### 5.2 Why not Tauri's global shortcut plugin

`tauri-plugin-global-shortcut` relies on an X11 keyboard grab or a platform
API that Wayland deliberately does not expose. On Hyprland it registers without
error and never fires. The compositor keybind + IPC pattern is the working
approach, and it has the side benefit that the hotkey is configured where the
user already configures every other hotkey.

### 5.3 Hyprland configuration

`owf-ctl setup --print-hypr` emits this block for the user to include:

```
exec-once = openwhisprflow

bind  = SUPER, D,      exec, owf-ctl ptt-start
bindr = SUPER, D,      exec, owf-ctl ptt-stop
bind  = SUPER, ESCAPE, exec, owf-ctl cancel

windowrulev2 = float,          class:^(openwhisprflow)$
windowrulev2 = nofocus,        class:^(openwhisprflow)$
windowrulev2 = noinitialfocus, class:^(openwhisprflow)$
windowrulev2 = pin,            class:^(openwhisprflow)$
windowrulev2 = noborder,       class:^(openwhisprflow)$
```

The three focus rules are load-bearing, not cosmetic. If the overlay takes
keyboard focus, `wtype` delivers the dictation to the overlay instead of the
user's target window. An M2 acceptance test covers exactly this.

## 6. Control protocol

Newline-delimited JSON over the Unix socket, one request and one response per
connection.

| Request | Response | Behaviour |
|---|---|---|
| `{"cmd":"ptt-start"}` | `{"ok":true,"state":"recording"}` | Starts capture. Idempotent while already `Recording`. |
| `{"cmd":"ptt-stop"}` | `{"ok":true,"state":"transcribing"}` | Stops capture, runs the pipeline. No-op unless `Recording`. |
| `{"cmd":"cancel"}` | `{"ok":true,"state":"idle"}` | Aborts from any active state, discards audio, injects nothing. |
| `{"cmd":"status"}` | `{"ok":true,"state":"...","warm":true,"last_ms":{...}}` | Diagnostics. |
| `{"cmd":"reload"}` | `{"ok":true}` or `{"ok":false,"err":"..."}` | Re-reads config (§13). Rejected unless `Idle`; on a config error the previous config stays active. |

`ptt-start` while `Transcribing`, `Normalizing`, or `Injecting` returns
`{"ok":false,"err":"busy"}` and the overlay flashes. v1 deliberately does not
queue a second utterance behind an in-flight one; overlapping dictations would
race on the injection target, and the failure mode (text landing in the wrong
window) is worse than a rejected keypress.

### 6.1 State machine

```
Warming --> Idle --> Recording --> Transcribing --> Normalizing --> Injecting --> Idle
              ^          |              |               |              |
              |          +--------------+---------------+--------------+
              |                    cancel / error
              +-----------------------------------------------------------
```

`Recording` auto-stops at 120 s and proceeds to `Transcribing` — a safety
valve against a stuck key leaving the microphone hot.

## 7. Pipeline stages

### 7.1 Capture

`cpal` opens the default input device. If the device supports 16 kHz mono f32
it is used directly; otherwise the native rate is captured and resampled with
`rubato` (sinc, `SincInterpolationType::Linear`) to 16 kHz mono. Multi-channel
input is downmixed by averaging.

Samples accumulate in a `Vec<f32>` behind a mutex. Every 50 ms the capture
thread computes RMS over the newest window and emits a Tauri event to the
overlay. Capacity is pre-reserved for 120 s (1.92 M samples, 7.7 MB) so no
reallocation happens mid-recording.

### 7.2 VAD trim

Silero VAD runs over the completed buffer. The retained span is from the first
speech onset to the last speech offset, padded 200 ms on each side and clamped
to the buffer. If no speech segment is found, the pipeline aborts: the overlay
flashes "no speech" and nothing is injected.

This exists because Parakeet's cost is linear in audio length, and push-to-talk
reliably produces 200–500 ms of silence at each end.

### 7.3 ASR

A warm `OfflineRecognizer` configured with `model_type = "nemo_transducer"`,
pointing at `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, and
`tokens.txt`. `num_threads = 4`. Greedy search.

Output is raw text — typically lowercase, unpunctuated, with disfluencies
intact. That is exactly the input distribution s1-mini was trained on.

Parakeet TDT 0.6b v3 covers 25 European languages: Bulgarian, Croatian, Czech,
Danish, Dutch, English, Estonian, Finnish, French, German, Greek, Hungarian,
Italian, Latvian, Lithuanian, Maltese, Polish, Portuguese, Romanian, Russian,
Slovak, Slovenian, Spanish, Swedish, Ukrainian. It does not emit a language
tag, which is why §7.4 exists.

### 7.4 Language identification

`whatlang` over the raw transcript, producing a language code and confidence.

**This is a deviation from the design presented in chat, which named `lingua`.**
Two things changed the calculus. First, the requirement weakened: language no
longer gates whether s1-mini runs (the decision was to run it on everything),
it only selects a guardrail threshold. Second, `lingua`'s detection models are
memory-expensive — even restricted to 25 languages in low-accuracy mode it
costs tens of megabytes, against roughly 1 MB for `whatlang`, on a machine with
6 GiB free. A coarse English / not-English signal is all this stage owes the
rest of the pipeline, and `whatlang` delivers that.

If guardrail tuning in M3 shows misclassification is actually causing bad
fallbacks, `lingua` restricted to the configured language set is the documented
upgrade; the stage is one function behind a `LanguageDetector` trait.

The result is: `English` if detected language is English with confidence ≥ 0.6,
otherwise `Other`. Transcripts under 12 characters are treated as `English`,
since trigram detection is unreliable at that length and English is the
configured primary language.

### 7.5 Normalization

See §8.

### 7.6 Guardrail

See §9.

### 7.7 Injection

See §10.

## 8. s1-mini contract

### 8.1 Server

```
llama-server \
  -m ~/.local/share/openwhisprflow/models/s1-mini-q4_k_m.gguf \
  --host 127.0.0.1 --port <port> \
  -c 2048 -t 4 \
  --jinja --chat-template-kwargs '{"enable_thinking":false}' \
  --temp 0
```

`-c 2048` rather than the model's native 40960: the model card recommends
inputs of roughly 1000 tokens, and KV cache is memory this machine does not
have to spare. `-t 4` matches physical cores; llama.cpp is slower with
hyperthreads included.

Port selection: the configured port (default 8730) is tried first; if bind
fails, ports 8731–8739 are tried in order. The resolved port is written to
`$XDG_RUNTIME_DIR/openwhisprflow.port` and used for all requests.

The `llama-server` binary is located on `PATH`, overridable via
`llama_server_path` in config.

### 8.2 Request

`POST /v1/chat/completions`:

```json
{
  "messages": [
    { "role": "system",
      "content": "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text." },
    { "role": "user",
      "content": "[Styling: semi-casual] [Structure: prose] [Context: general]\n<raw transcript>" }
  ],
  "temperature": 0,
  "top_k": 1,
  "stream": false,
  "max_tokens": 256,
  "chat_template_kwargs": { "enable_thinking": false }
}
```

The system prompt is reproduced **verbatim** from the model card and must not
be paraphrased. `enable_thinking: false` is mandatory — the model card
documents that omitting it typically yields blank output — and it is passed
both on the server command line and per-request, because either alone has been
a reported source of integration bugs.

`max_tokens` is computed as
`clamp(ceil(1.3 * est_tokens) + 32, 32, 1024)` where
`est_tokens = ceil(chars / 3.5)`. The 1.3× factor is the model card's own
sizing guidance. The estimate is deliberately local arithmetic rather than a
call to `/tokenize`, to avoid a second round trip on the critical path.

Timeout: 6 seconds. Timeout, connection failure, non-200, or a malformed body
all route to the raw-text fallback (§9.2) — a failed cleanup must never cost
the user their transcript.

### 8.3 Control line

`[Styling: S] [Structure: T] [Context: C]` where:

- `S` ∈ `casual`, `semi-casual`, `semi-formal`, `formal`
- `T` ∈ `prose`, `lists`
- `C` ∈ `general`, `email`

Values are resolved per §11. The builder is a pure function and is unit-tested
against the full 4 × 2 × 2 matrix; unknown values are a config-load error, not
a runtime error.

## 9. Guardrail

s1-mini is a 0.6B model sitting between the user's voice and their keyboard.
It will occasionally drop a clause, loop, or emit template fragments. The
guardrail is what makes that acceptable rather than dangerous.

### 9.1 Rejection criteria

The cleaned output is rejected if **any** holds:

1. Empty or whitespace-only.
2. Word count `< 0.55 ×` or `> 1.80 ×` the raw word count.
3. Token-bag overlap with raw, case-folded and stripped to alphanumerics,
   below threshold: **0.55** when language is `English`, **0.70** when `Other`.
   The stricter non-English threshold reflects that s1-mini v1 is English-only
   and therefore out of domain there.
4. Any 6-gram appears 3 or more times — degenerate loop detection.
5. Output contains `[Styling:`, `[Structure:`, `[Context:`, `<think>`, or
   `<|im_start|>` — template or thinking bleed.

Criteria 2 and 3 are skipped when the raw transcript is under 4 words, where
the ratios are too noisy to mean anything; criteria 1, 4, and 5 always apply.

All thresholds live in `[guardrail]` in config so they can be tuned without a
rebuild. The values above are starting points, not measurements.

### 9.2 Fallback

On rejection, the raw ASR text is injected after a minimal rule-based pass:
collapse runs of whitespace, capitalize the first alphabetic character, and
append a period if the text ends without terminal punctuation. Nothing else —
the fallback must be predictable enough that the user can tell at a glance that
cleanup did not run.

### 9.3 Rejection logging

Every rejection appends one JSON line to
`~/.local/state/openwhisprflow/rejections.jsonl`:

```json
{"ts":"2026-08-27T18:04:11Z","reason":"overlap","lang":"English",
 "raw":"...","cleaned":"...","overlap":0.41,"control":"[Styling: semi-casual] ..."}
```

This file is the input to M3 threshold tuning. It is local-only and never
transmitted. `owf-ctl setup --purge-logs` clears it.

## 10. Text injection

### 10.1 Trait

```rust
trait TextInjector {
    fn inject(&self, text: &str) -> Result<(), InjectError>;
    fn name(&self) -> &'static str;
}
```

### 10.2 `WtypeInjector` (v1)

Invokes `wtype -d 2 -- <text>`. The `--` terminates option parsing so a
transcript beginning with `-` is typed rather than misread as flags; `-d 2`
inserts a 2 ms inter-keystroke delay, which some Electron and XWayland surfaces
need to avoid dropping characters.

If `config.trailing_space` is true (default), a single space is appended so
consecutive dictations do not run together.

**M1 acceptance test:** `inject("-- hello -x")` must produce exactly
`-- hello -x` in a focused text field. If `wtype` turns out not to honour `--`,
the documented remedy is to make the clipboard path (§10.4) the default
injector rather than to invent an escaping scheme.

### 10.3 `YdotoolInjector` (implemented — no longer deferred)

**Status update:** implemented as `inject::YdotoolInjector`, selectable as
`[inject] backend = "ydotool"`. It invokes `ydotool type --key-delay <ms> --
<text>`; the setup cost catalogued below is unchanged and is documented in
`README.md` ("Typing with `ydotool`") rather than automated, per the
never-configure-the-user's-system rule. `wtype` remains the default. The
original text follows.


The original request named `(y)dotool`, and `wtype` was chosen instead for v1
because it needs no root and is already installed. `wtype` is known to fail in
some XWayland surfaces, so this remains the planned second implementation. It
requires: `pacman -S ydotool`, a udev rule granting the `input` group access to
`/dev/uinput` (currently `crw------- root root`), a `ydotoold` user service,
and `YDOTOOL_SOCKET` in the app environment. Capturing it here so the setup
cost is known rather than rediscovered.

### 10.4 Clipboard fallback

If the injector returns an error, the text is written to the clipboard with
`wl-copy` and a desktop notification says "typing failed — copied to
clipboard". A transcript is never silently lost.

## 11. Style control

Window class is read at `ptt-start` — off the critical path — via
`hyprctl -j activewindow`, parsed for `.class`. If `hyprctl` is unavailable or
fails, the default style is used and a debug line is logged.

Rules are evaluated in order; the first whose `match_class` regex matches wins.
Unset axes on a matching rule inherit from `[style_default]`.

```toml
[style_default]
styling   = "semi-casual"
structure = "prose"
context   = "general"

[[style_rules]]
match_class = "(?i)thunderbird|^Mail$"
styling     = "semi-formal"
context     = "email"

[[style_rules]]
match_class = "(?i)^(Slack|discord|element)$"
styling     = "casual"

[[style_rules]]
match_class = "(?i)^(code|zed|Alacritty)$"
styling     = "semi-formal"
structure   = "prose"
```

## 12. Overlay

A 280 × 72 px window, bottom-centre, transparent background, no decorations,
never focused, `skipTaskbar`. Position is configurable.

| State | Presentation |
|---|---|
| `Warming` | Indeterminate spinner, "loading models" |
| `Idle` | Hidden |
| `Recording` | Live bars driven by the 50 ms RMS events, elapsed time in seconds |
| `Transcribing` | "transcribing" + indeterminate progress |
| `Normalizing` | "cleaning" + indeterminate progress |
| `Injecting` → done | 800 ms flash of the first ~60 chars that were pasted, then hide |
| `Error` | Red pill with a one-line reason, 2 s, then hide |
| Busy rejection | Amber flash, 400 ms |

Distinguishing `Transcribing` from `Normalizing` is a real requirement, not
decoration: the end-to-end round trip on this hardware is seconds long, and the
stage label is the only way the user can tell a slow pipeline from a hung one.

The overlay is a Tauri window driven by Tauri events; the frontend holds no
pipeline logic.

## 13. Configuration

`~/.config/openwhisprflow/config.toml`, created with defaults on first run.
Loaded at startup; `owf-ctl reload` re-reads it without restarting.

```toml
[audio]
device = "default"        # cpal device name, or "default"
max_seconds = 120
vad_padding_ms = 200

[asr]
num_threads = 4

[normalize]
enabled = true
port = 8730
timeout_ms = 6000
llama_server_path = "llama-server"   # or an absolute path
context_size = 2048
threads = 4

[guardrail]
min_word_ratio = 0.55
max_word_ratio = 1.80
min_overlap_english = 0.55
min_overlap_other = 0.70
short_input_words = 4
ngram_size = 6
ngram_max_repeats = 3

[inject]
backend = "wtype"         # "wtype" | "clipboard"
trailing_space = true
keystroke_delay_ms = 2

[overlay]
position = "bottom-center"
width = 280
height = 72

[style_default]
styling   = "semi-casual"
structure = "prose"
context   = "general"
```

Invalid enum values, unknown keys, and out-of-range numbers are load-time
errors reported in the overlay, and the previous good config stays active.

## 14. Filesystem layout and provisioning

```
~/.config/openwhisprflow/config.toml
~/.local/share/openwhisprflow/models/
    parakeet-tdt-0.6b-v3-int8/
        encoder.int8.onnx      (622 MB)
        decoder.int8.onnx      (12 MB)
        joiner.int8.onnx       (6.1 MB)
        tokens.txt             (92 KB)
    silero_vad.onnx            (~2 MB)
    s1-mini-q4_k_m.gguf        (462 MB)
    models.lock.toml
~/.local/state/openwhisprflow/
    openwhisprflow.log
    rejections.jsonl
$XDG_RUNTIME_DIR/openwhisprflow.{sock,lock,port}
```

Total on disk ≈ 1.1 GB. Expected resident ≈ 1.6 GB (Parakeet ~700 MB,
s1-mini + KV ~600 MB, Tauri/WebKit ~250 MB).

### 14.1 Sources

- Parakeet: `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2`
- Silero VAD: `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx`
- s1-mini: `https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf`

### 14.2 Integrity

`owf-ctl setup` downloads with progress reported to the overlay, then verifies
each file's SHA-256 against `models.lock.toml`. If the lock file is absent,
`owf-ctl setup --update-lock` writes the observed hashes; this is run once
during M0 and the resulting `models.lock.toml` is committed to the repository,
so every subsequent install verifies against pinned values. A mismatch aborts
setup and leaves existing models untouched.

### 14.3 Prerequisites

`owf-ctl setup` checks and reports, without installing anything itself:

- `cargo` / `rustc` — install `rustup` from `extra` (currently missing)
- `llama-server` — install `llama-cpp` from `extra` (currently missing)
- `wtype`, `wl-copy`, `hyprctl` — present
- A writable `$XDG_RUNTIME_DIR`

## 15. Error handling

| Failure | Handling | User sees |
|---|---|---|
| No input device / cpal open fails | Abort to `Idle` | Error pill, "no microphone" |
| VAD finds no speech | Abort to `Idle`, nothing injected | Flash, "no speech" |
| ASR returns empty string | Abort to `Idle`, nothing injected | Flash, "no speech" |
| ASR throws | Abort to `Idle`, log | Error pill, "transcription failed" |
| `llama-server` down / unhealthy | Skip normalization, inject raw + rule pass | Normal paste; log line |
| Normalization times out (>6 s) | Same as above | Normal paste; log line |
| Guardrail rejects | Inject raw + rule pass, append to `rejections.jsonl` | Normal paste |
| `wtype` fails | `wl-copy` + notification | "typing failed — copied to clipboard" |
| Second `ptt-start` while busy | Reject, no state change | Amber flash |
| Recording hits 120 s | Auto-stop, process normally | Normal pipeline |
| `llama-server` crashes | Backoff restart (1→30 s) | Overlay shows degraded badge while down |
| Stale socket at startup | Removed if lock is free; otherwise second instance exits | Message on stderr |

The invariant across this table: **once audio has been transcribed, the user
gets text.** Every downstream failure degrades to raw output rather than
discarding the transcript.

## 16. Testing

TDD applies. The pure logic is written test-first, because it is where the
correctness risk actually lives and it needs neither audio nor models.

**Unit (test-first):**

- Guardrail — table-driven over: empty, whitespace, too-short, too-long, low
  overlap English, low overlap non-English, looping 6-grams, template bleed,
  short-input exemption, and a set of realistic accept cases.
- Control-line builder — full 4 × 2 × 2 matrix, plus rejection of invalid enums.
- Style-rule resolution — first-match-wins, axis inheritance from default,
  no-match, invalid regex, `hyprctl` failure.
- `max_tokens` computation — clamping at both ends, the 1.3× factor.
- Rule-based fallback — whitespace collapse, capitalization, terminal punctuation.
- VAD trim boundaries — padding, clamping at buffer edges, no-speech.
- Config load — defaults, invalid enum, out-of-range, unknown key.

**Integration:**

- Fixture WAVs (English and one non-English) through the real recognizer,
  asserted with a WER tolerance rather than exact match.
- A stubbed `llama-server` (wiremock) returning: a good cleanup, a timeout, a
  500, malformed JSON, a looping output, and template bleed — exercising every
  branch of §15.
- `MockInjector` capturing injected text, used for full-pipeline assertions.
- Socket protocol — every command in every state, including the busy rejection
  and idempotent `ptt-start`.

**Manual (M2 checklist, cannot be honestly automated):**

- Injection into Alacritty, Firefox, an Electron app, and an XWayland app.
- Overlay does not steal focus — dictate into a text editor with the overlay
  visible and confirm the text lands in the editor.
- Hold-to-talk feel: keypress-to-recording-indicator latency.

**Measured at M0 and recorded in the plan:** real end-to-end latency on this
laptop for a 10 s utterance, broken down per stage.

## 17. Risks

### 17.1 The `sherpa-onnx` crate is new

Version 1.13.6 was published 2026-08-24, three days before this design. It is
the official k2-fsa binding and downloads prebuilt native libraries rather than
building from source, which removes the failure mode that mattered most — but
it is unproven here.

**Mitigation:** M0 is a spike that loads Parakeet v3 and transcribes one WAV,
before any other work. Timebox one hour. If it fails, ASR moves to a Python
sidecar (architecture B) behind the same `Transcriber` trait; nothing else in
this design changes.

### 17.2 Latency is the product risk

The estimate is ~1.5–3 s ASR plus ~2–4 s normalization for a 10 s utterance:
4–7 s from key release to paste. That is fine for a paragraph and poor for a
two-word search query. The estimate is derived from the hardware, not measured.

**Mitigation:** M0 measures it before any UI is built. If it lands materially
worse, the options — in preference order — are: skip normalization below a
configurable word count, drop to a smaller ASR model, or make normalization
opt-in per application. That decision is deferred until there is a number.

### 17.3 `wtype` and XWayland

Known to fail in some XWayland surfaces. Mitigated by the clipboard fallback
(§10.4) and the `TextInjector` trait, with ydotool documented in §10.3.

### 17.4 s1-mini on non-English input

The decision was to run s1-mini on all languages and let the guardrail catch
the damage. This is expected to reject frequently on non-English input, costing
2–4 s for no benefit. The tighter non-English overlap threshold (0.70) is the
control. If M3's rejection logs show a high non-English rejection rate, the
cheap remedy is to skip normalization when language is `Other`, which is a
one-line config-gated change.

## 18. Milestones

**M0 — Foundations and spike.** Install `rustup` and `llama-cpp`. Implement
`owf-ctl setup`: download, verify, write `models.lock.toml`. Spike the
`sherpa-onnx` crate on a fixture WAV. **Measure and report end-to-end latency
per stage on this hardware.** Exit criteria: a transcript printed to stdout
from a WAV, and a latency table.

**M1 — Headless pipeline.** The full chain behind `owf-ctl`, no UI: capture →
VAD → ASR → language ID → s1-mini → guardrail → `wtype`. `llama-server`
supervision. Socket protocol and state machine. All unit and integration tests
from §16. Exit criteria: holding a Hyprland keybind dictates into a focused
window.

**M2 — Overlay.** Tauri window, state events, RMS waveform, the Hyprland window
rules, `--print-hypr`. Exit criteria: the manual checklist in §16 passes,
especially the focus-stealing test.

**M3 — Style and tuning.** Per-application style rules, config reload,
guardrail threshold tuning against real `rejections.jsonl` data, README with
setup instructions. Exit criteria: thresholds justified by measurements rather
than the guesses in §9.
