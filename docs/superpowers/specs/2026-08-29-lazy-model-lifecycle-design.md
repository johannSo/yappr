# Lazy model lifecycle — design

Status: proposed, 2026-08-29. Extends `2026-08-28-one-process-tray-app-design.md`
(§1 process topology, §8 shutdown) and revises the model-loading half of
`2026-08-27-openwhisprflow-design.md` §5. Adds no new process, no new binary,
and no new wire-format variant.

## Purpose

The app holds every model resident for the life of the process. Measured on
the development machine while the app sat idle, having transcribed nothing:

```
openwhisprflow   777 MB RSS   sherpa Parakeet TDT + Silero VAD
llama-server     697 MB RSS   S1-mini
                ─────────
                ~1.4 GB       held continuously, from login onwards
```

Dictation is bursty. A user presses SUPER+D a handful of times an hour, and
between those presses the 1.4 GB buys nothing. This design makes the models
follow the dictation instead of the process:

1. Nothing model-shaped is loaded at startup (by default).
2. Pressing SUPER+D starts capture immediately and begins loading the models
   **in parallel with the user speaking**.
3. `[models] idle_unload_seconds` after the last dictation ends, both the ASR
   and VAD models and the `llama-server` child are released.

The trade is explicit and belongs to the user: the first dictation after an
unload pays a cold start. `preload_at_startup` exists for anyone who would
rather spend the RAM.

## 1. Configuration

A new `[models]` section, with the usual consequences of invariant 4 — the
struct in `config.rs` is not optional; documenting the section without adding
it is a hard startup failure.

```toml
[models]
# Modelle beim Start laden, statt beim ersten Tastendruck.
preload_at_startup = false
# Modelle nach dieser Ruhezeit wieder entladen. 0 = nie entladen.
idle_unload_seconds = 60
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `preload_at_startup` | `bool` | `false` | Load ASR/VAD and spawn `llama-server` during startup, exactly as the current `warm_up` does. |
| `idle_unload_seconds` | `u32` | `60` | Seconds of inactivity after which loaded models are released. `0` disables unloading entirely. |

`preload_at_startup = true` with `idle_unload_seconds = 0` reproduces today's
behaviour byte for byte, and is the configuration a user should be pointed at
if any of this misbehaves.

`config_write.rs`'s `DEFAULT_CONFIG_TOML` carries both annotations above. The
Settings window shows the section under **Erweitert** with the title
`Modelle` (`schema.ts`'s `SECTION_TITLES` and `CATEGORIES`). Per-row reset
buttons need no work: `GetConfig` already ships `Config::default()` as
`defaults`, which is exactly why that field is on the wire (invariant 9).

`schema.ts`'s `RESTART_SECTIONS` is deliberately **not** changed. It names
`asr` and `normalize` as sections the daemon cannot apply without a restart,
and that remains true whenever the models are currently resident. It becomes
pessimistic — not wrong — for a user whose models happen to be unloaded.

## 2. Splitting `warm_up`

`server.rs`'s `warm_up` currently does three things in one call: constructs
the `Recorder`, loads ASR and VAD, and spawns `llama-server`. Only the first
is genuinely a startup concern.

It splits into:

- **`warm_up_recorder(cfg, daemon)`** — the eager `Recorder::new` attempt and
  its `tracing::error!` fallback. Runs at startup unconditionally, on the
  warm-up thread, for the reason I7 already gives: `Recorder::new` can block
  for a long time in `cpal`, and a wedge on the Tauri `setup()` thread means
  no window, no tray, and no way to quit. Unchanged behaviour, unchanged
  laziness — `ensure_recorder` still retries on the next `ptt-start`.
- **`load_models(cfg, daemon) -> Result<(Pipeline, Option<LlamaServer>)>`** —
  everything else `warm_up` did: `spawn_and_wait_healthy`, `SherpaTranscriber`,
  `SileroTrimmer`, the `Normalizer` choice, `inject::build`, the
  `with_stage_events` wiring, `Pipeline::new`. Its error contract is
  unchanged from `warm_up`'s: a `llama-server` that will not come up degrades
  to `UnavailableNormalizer` (C1, spec 15) and does **not** fail the call;
  only ASR or VAD failing does.

`load_models` still takes its `Config` by value, but its lazy caller
(`ensure_models_loaded`, §3) reads that config **from disk at the moment of
the call** — `Config::load_from(&daemon.config_path)`, as `GetConfig` and
`Reload` already do — rather than from a snapshot taken at startup. The
preload path passes the startup config, exactly as `warm_up` does today. A consequence worth
naming: for a user running lazily, a changed `[asr]` or `[normalize]` setting
now takes effect on the next dictation rather than on the next app restart.
This is a side effect of the design, not a feature it promises — the
`RESTART_SECTIONS` note stays as written because it is still correct for the
preloaded case.

### Startup

```
preload_at_startup = true    state: WARMING ──load_models──▶ IDLE   (or FAILED)
preload_at_startup = false   state: WARMING ─────────────────▶ IDLE
                                                  models: None, llama: None
```

The `WARMING` state is not removed and its meaning does not change: it still
means "startup has not finished". In the lazy case it is simply brief.
`FAILED` is still reachable at startup, but only when `preload_at_startup` is
on — see §4 for what replaces it lazily.

## 3. Loading on demand

Two new pieces of `Daemon` state:

```rust
/// Held across a model load so two concurrent presses share one load rather
/// than racing two. ALWAYS taken before `pipeline`, never after.
load_lock: Mutex<()>,
/// Bumped by `start_recording` and by every utterance that ends. The idle
/// unload deadline is measured from this.
last_activity: Mutex<Instant>,
/// Lock-free projection of "`pipeline` is `Some`", for readers that must
/// never block on it. Set true after a load installs the pipeline, false
/// under `load_lock` as an unload clears it.
models_loaded: AtomicBool,
```

`models_loaded` is not redundant bookkeeping, and the reason is the same one
`normalize_available`'s own doc comment gives: `process_utterance` holds
`pipeline`'s mutex for the entire `process` call — deliberately, for the
sherpa-onnx FFI reason documented there — so any thread that answers a
question by locking `pipeline` blocks for the length of a whole
transcription. Two such readers exist here, and both would be broken by it:
`dispatch`'s `Status` handler, which would make `--status` hang mid-utterance,
and the housekeeping thread, which would stop reaping subscribers for the
same window. Both read this atomic instead.

**Lock ordering is an invariant of this design: `load_lock` before `pipeline`,
never the reverse.** `process_utterance` takes only `pipeline`, and holds it
for the whole `process` call for the sherpa-onnx FFI reason its comment
already documents. `ensure_models_loaded` and `unload_models` both take
`load_lock` first and hold `pipeline` only for the assignment.

```rust
fn ensure_models_loaded(daemon: &Arc<Daemon>) -> Result<(), String>
```

Takes `load_lock`; returns `Ok(())` immediately if `pipeline` is already
`Some`; otherwise calls `load_models`, installs the `Pipeline` and the
`LlamaServer`, sets `normalize_available`, and returns. Idempotent and safe
to call from any thread.

It is called from exactly two places:

1. **`start_recording`**, on a spawned thread, *after* capture has gone live.
   This ordering is the entire point of the design. Capture needs no model,
   so the microphone opens on the same timeline it does today — the existing
   `Opening` event still marks the gap before real samples arrive — and the
   models load while the user is speaking. For any utterance longer than the
   load, the cost is zero.
2. **`run_utterance`**, before `process_utterance`, where it blocks until the
   load begun in (1) has finished.

Per the decision recorded in brainstorming, a load still in flight at
`ptt-stop` produces **no new overlay state**. The daemon is genuinely in
`TRANSCRIBING` — `claim_busy` has already run — and the overlay shows its
existing `Transcribing` view for however long the load takes. `proto.rs`,
`src/Overlay.tsx` and `src-tauri/fixtures/replay-full.ndjson` are untouched,
so invariant 3's three-way maintenance burden is not incurred.

## 4. A failed load is retryable, not fatal

Today the only way `warm_up` returns `Err` is ASR or VAD failing to load, and
the daemon answers by storing `FAILED` permanently, recording `fatal_error`,
and refusing every subsequent `ptt-start` until the process is restarted.
That is the right answer for a failure discovered at startup. It is the wrong
answer for one discovered on the user's third dictation of the day.

Lazily, a `load_models` failure inside `run_utterance`:

- broadcasts `OverlayEvent::Error { reason }` carrying the real stored reason
  — for a fresh install that is `SherpaTranscriber`/`SileroTrimmer`'s own
  "model paths" error, the string that tells a user to open the Setup pane;
- returns the daemon to `IDLE` via the existing `IdleOnExit` guard;
- leaves `fatal_error` unset and `FAILED` unreached.

The next press retries the load. On a fresh install this is strictly better
than the current behaviour: download the models in the Setup pane, press
SUPER+D, and it works — no restart.

`FAILED` remains reachable exactly as before via `preload_at_startup = true`,
and `Request::PttStart`'s `FAILED` arm is unchanged.

Invariant 1 is not weakened. It governs text once ASR has produced it; a load
that fails has produced none, and the captured audio was never transcribable
by any path.

## 5. Unloading

No new thread. `spawn_housekeeping` already runs a timed loop; it gains the
unload deadline as a third thing to wake for.

```rust
/// Pure, therefore directly unit-testable — the same shape and the same
/// reasoning as `should_emit_level`.
fn should_unload(
    last_activity: Instant,
    now: Instant,
    idle_seconds: u32,
    state: u8,
    loaded: bool,
) -> bool
```

`true` only when `loaded`, `idle_seconds != 0`, `state` is `IDLE` or
`PAUSED`, and `now - last_activity >= idle_seconds`.

`PAUSED` counts as idle deliberately. It is reached only by a CAS from `IDLE`
(Task 13), so there is never an utterance to protect while paused, and a user
who has explicitly paused dictation is the clearest possible signal that the
models are not about to be needed.

The loop's wait becomes:

```rust
let wait = min(existing_wait, time_until_unload_deadline);
```

so `idle_unload_seconds = 60` unloads at 60 s, not somewhere in 60–70 s at
the mercy of `HEALTH_POLL_INTERVAL`. When nothing is loaded, or
`idle_unload_seconds` is `0`, there is no deadline and the existing wait is
used unchanged.

`unload_models` takes the bare pieces it needs rather than `&Daemon` — the
convention `kill_llama`, `process_utterance` and `supervise_llama_once` all
follow, and for the same reason: it stays unit-testable without audio
hardware. It takes `load_lock`, **re-checks `should_unload` while holding
it**, then drops the `Pipeline` (`*pipeline.lock() = None`), calls the
existing `kill_llama` — already the kill-then-reap path `shutdown` uses — and
stores `models_loaded = false` and `normalize_available = false`.

The re-check under the lock is what makes this race-free. `start_recording`
bumps `last_activity` *before* it stores `RECORDING`, so a recording that
begins between the housekeeping thread's decision and its acquisition of
`load_lock` moves the deadline, and the re-check declines. The worst
remaining outcome is an unload immediately followed by the load that the same
press requested: wasteful for one dictation, never incorrect. This mirrors
the epoch discipline `spawn_safety_valve` already uses, with `last_activity`
playing the part `recording_epoch` plays there.

Nothing is broadcast on unload. The daemon's `State` does not change — it was
`IDLE` before and is `IDLE` after — and the tray's icon is driven entirely by
broadcasts (`tray.rs`, "Icon and daemon state"), so inventing an event here
would mean inventing an icon for it too.

The `Recorder` is not touched. It is the microphone, it is already
constructed lazily by `ensure_recorder`, and it is not what the 1.4 GB is
made of.

## 6. The supervisor stops resurrecting `llama-server`

This is the change without which none of the rest works.
`supervise_llama_once` restarts a missing or unhealthy `llama-server` on
every housekeeping tick. Left alone, it would spawn a fresh 697 MB child
seconds after every unload, forever.

`spawn_housekeeping`'s llama half gains one gate, alongside its existing
`WARMING` check:

```rust
if !daemon.models_loaded.load(Ordering::SeqCst) {
    // Models are unloaded (or a load is still in flight, which owns its own
    // llama-server). Nothing to supervise; do not resurrect.
    continue;
}
```

Model residency is the correct predicate rather than a new `models_wanted`
flag, because `llama-server` is wanted precisely when the pipeline that uses
it exists. It also covers the in-flight-load window: `models_loaded` is set
only *after* a load installs the pipeline, so while `load_models` is running
this gate still skips, and that load's own `spawn_and_wait_healthy` owns the
child — the supervisor must not race it, exactly as it must not race
`warm_up`'s initial spawn today.

It reads the atomic rather than locking `pipeline` for the reason §3 gives:
this is the same thread that reaps dead subscribers, and blocking it behind a
transcription would stall that unrelated work for the length of the
utterance.

`supervise_llama_once` itself is **unchanged**, keeping its existing unit
tests and its injected `respawn` seam intact.

`last_known_available` is not reset when the gate skips a tick. After a
reload, a healthy `llama-server` against a `true` baseline broadcasts
nothing, which is right; against a `false` baseline it broadcasts
`NormalizeRecovered`, which is also right. Storing `normalize_available =
false` at unload time without broadcasting `NormalizeDegraded` is deliberate:
normalization is not degraded, it is not currently loaded, and the overlay's
degraded badge means the former.

## 7. Consequences elsewhere

**`Request::Reload`.** Its `None` arm returns
`"reload requires the pipeline to be warmed up"`, described in its own
comment as unreachable. Unloaded is now a routine state, so that arm becomes:
parse the config, and on success return `Response::ok(State::Idle)` without
touching a pipeline that does not exist. The next `load_models` reads the
file anyway. `Request::SetConfig` inherits this through the same path.

**`status`'s `warm` field.** `r.warm = Some(current != WARMING)` becomes
`Some(daemon.models_loaded.load(Ordering::SeqCst))` — "models are resident
right now" rather than "startup finished". The atomic, never the `pipeline`
lock, for the reason §3 gives. Nothing in the overlay, the tray or the settings window
reads `warm`; it appears only in `openwhisprflow --status` output. This is a
documented CLI-surface change, and `--status` gains it as the way to see
whether the models are currently up.

**`shutdown`.** Already calls `kill_llama` unconditionally and already
tolerates `None`. No change.

**`--bench` and `--replay`.** Construct their own models directly and never
go through `Daemon`. No change.

## 8. Testing

Pure and seam-testable, no models on disk required:

- `[models]` parses, defaults to `false`/`60`, and rejects an unknown key
  (`deny_unknown_fields`).
- `config_write` renders both keys with their comments, and changing
  `idle_unload_seconds` produces a one-line diff (invariant 9).
- `should_unload`: the `0` case, the `loaded == false` case, `IDLE` and
  `PAUSED` true, every busy state false, and the boundary at exactly
  `idle_unload_seconds`.
- The housekeeping loop's llama half does not respawn while `models_loaded`
  is `false` — driven with a scratch `Mutex<Option<LlamaServer>>` and a stub
  `respawn` that records whether it was called, as `supervise_llama_once`'s
  existing tests already do.
- Neither `Status` nor the housekeeping tick blocks while an utterance holds
  `pipeline`'s mutex: a test holds that lock on one thread and asserts both
  still answer promptly.
- `unload_models` declines when `last_activity` moved after the decision, and
  when the state is busy.
- `Reload` succeeds with `pipeline == None`.
- `warm` reports model residency rather than startup completion.

`load_models` itself needs a real ASR model and so cannot run here, for the
same reason the existing four tests are `#[ignore]`d. It therefore gets an
injectable seam in the shape `supervise_llama_once`'s `respawn` already
establishes, so `ensure_models_loaded`'s locking, idempotence, and
error-to-`IDLE` behaviour are all testable against a stub loader.

The gate remains `cargo test --workspace && cargo clippy --workspace
--all-targets`.

## 9. Documentation

- **CLAUDE.md** — a new invariant 12 for the model lifetime and the lock
  order; the Architecture section's description of `warm_up`; the `[models]`
  section in the config summary.
- **README.md** — a short note on memory use and the two knobs.
- **HANDOVER.md** — what was verified and what was not. As with everything
  touching the audio path, the cold-start timing of a real dictation cannot
  be measured here without recording from the microphone, which requires
  explicit permission.

## 10. Out of scope

- Separate timeouts for ASR and `llama-server`. One number, decided in
  brainstorming.
- Unloading anything mid-utterance, or under memory pressure. The deadline is
  the only trigger.
- Swapping ASR models or `llama-server` endpoints live. `load_models` reading
  fresh config makes some of this fall out lazily, but nothing here promises
  it, and `RESTART_SECTIONS` is unchanged accordingly.
- Predictive or speculative preloading (loading on focus, on window change,
  on a first keystroke). `preload_at_startup` is the only anticipatory knob.
- Releasing the `Recorder`.
