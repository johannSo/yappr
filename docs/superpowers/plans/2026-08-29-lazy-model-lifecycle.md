# Lazy Model Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Load the ASR/VAD models and `llama-server` when the user presses SUPER+D rather than at startup, and release them a configurable idle timeout after the last dictation, so the app stops holding ~1.4 GB resident while doing nothing.

**Architecture:** `Daemon.pipeline` and `Daemon.llama` are already `Mutex<Option<..>>`, so "unloaded" is `None` in both — no new ownership model. `warm_up` splits into `warm_up_recorder` (always at startup) and `load_models` (on demand). A `models_loaded: AtomicBool` is the lock-free projection of "`pipeline` is `Some`" for readers that must never block behind an in-flight utterance. The existing housekeeping thread gains the unload deadline; its `llama-server` supervisor gains one gate so it stops resurrecting a deliberately-unloaded child.

**Tech Stack:** Rust (`crates/owf-core`, `src-tauri`), `serde`/`toml`/`toml_edit`, React + TypeScript (`src/settings/`), `bun`.

**Spec:** `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`

## Global Constraints

- **Invariant 4:** config uses `#[serde(deny_unknown_fields)]` everywhere. A documented `config.toml` section without a matching Rust struct is a hard startup failure. `[models]` gets its struct in the same task that documents it.
- **Invariant 3 must not be triggered:** this feature adds **no** `OverlayEvent` or `State` variant. `crates/owf-core/src/proto.rs`, `src/Overlay.tsx` and `src-tauri/fixtures/replay-full.ndjson` are not modified by any task in this plan.
- **Invariant 9:** the settings GUI writes via `config_write`, never `toml::to_string`. A save that changes nothing leaves `config.toml` byte-identical.
- **Lock ordering (new, load-bearing):** `load_lock` is always taken **before** `pipeline`, never the reverse. `process_utterance` takes only `pipeline` and holds it for the whole `process` call.
- **Never lock `pipeline` to answer a question.** `dispatch`'s `Status` handler and the housekeeping thread both read `models_loaded` instead. Locking `pipeline` there would block for the length of a whole transcription.
- **Defaults:** `preload_at_startup = false`, `idle_unload_seconds = 60`. `idle_unload_seconds = 0` means never unload.
- **Gate:** `cargo test --workspace && cargo clippy --workspace --all-targets` must both be clean at the end of every task. Baseline before this plan: **377 passed, 0 failed, 4 ignored**.
- **German UI copy.** Every label and help string in `src/settings/schema.ts` is German, matching the surrounding entries.
- **Build note:** `gtk-layer-shell` must be installed or anything touching `src-tauri` fails in `gtk-layer-shell-sys`'s build script. No task here modifies `src-tauri`.

---

### Task 1: The `[models]` config section

Adds the two settings, their defaults, their commented form in the shipped
config file, and their presentation in the Settings window. Nothing reads
them yet — later tasks do. Reviewable on its own: a user can open Settings
and see two new rows under **Erweitert** that save correctly.

**Files:**
- Modify: `crates/owf-core/src/config.rs` (add `ModelsConfig`, add the `models` field to `Config`, extend `DEFAULT_CONFIG_TOML`)
- Modify: `crates/owf-core/src/config_write.rs` (tests only)
- Modify: `src/settings/schema.ts` (`CATEGORIES`, `SECTION_TITLES`, `LABELS`, `UNITS`, `HELP`)

**Interfaces:**
- Consumes: nothing.
- Produces: `owf_core::config::ModelsConfig { preload_at_startup: bool, idle_unload_seconds: u32 }`, reachable as `Config::models`. `ModelsConfig::default()` is `{ preload_at_startup: false, idle_unload_seconds: 60 }`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block at the bottom of `crates/owf-core/src/config.rs`:

```rust
#[test]
fn models_defaults_to_lazy_loading_with_a_sixty_second_idle_timeout() {
    let c = ModelsConfig::default();
    assert!(!c.preload_at_startup);
    assert_eq!(c.idle_unload_seconds, 60);
}

#[test]
fn a_config_written_before_the_models_section_existed_still_parses() {
    // Every user upgrading into this feature has one of these on disk.
    let c = Config::from_str("[audio]\ndevice = \"default\"\n").unwrap();
    assert_eq!(c.models, ModelsConfig::default());
}

#[test]
fn an_unknown_key_in_models_is_a_hard_error() {
    // Invariant 4: `deny_unknown_fields`, so a typo fails loudly at startup
    // rather than being silently ignored.
    let err = Config::from_str("[models]\nidle_unload_secs = 30\n").unwrap_err().to_string();
    assert!(err.contains("idle_unload_secs"), "unhelpful error: {err}");
}

#[test]
fn zero_seconds_is_accepted_and_means_never_unload() {
    let c = Config::from_str("[models]\nidle_unload_seconds = 0\n").unwrap();
    assert_eq!(c.models.idle_unload_seconds, 0);
}

#[test]
fn the_shipped_default_config_declares_the_models_section() {
    // DEFAULT_CONFIG_TOML is what a first run writes to disk; a section that
    // exists in Rust but not there is a setting no hand-editor discovers.
    let c = Config::from_str(DEFAULT_CONFIG_TOML).unwrap();
    assert_eq!(c.models, ModelsConfig::default());
    assert!(DEFAULT_CONFIG_TOML.contains("[models]"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p owf-core config::tests::models -- --nocapture; cargo test -p owf-core config::tests::a_config_written_before`

Expected: FAIL to compile — `cannot find type ModelsConfig in this scope`.

- [ ] **Step 3: Add `ModelsConfig`**

In `crates/owf-core/src/config.rs`, directly after the `AsrConfig` block (after its `impl Default`, around line 148), insert:

```rust
/// How long the models stay in memory, and whether they are there before
/// anyone asks. See `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.
///
/// The default is lazy: nothing model-shaped is loaded until the first
/// `ptt-start`, and both the ASR/VAD models and the `llama-server` child are
/// released `idle_unload_seconds` after the last dictation. Measured on the
/// development machine, that is ~1.4 GB not held while the app sits idle.
///
/// `preload_at_startup = true` with `idle_unload_seconds = 0` reproduces the
/// behaviour that predates this section: everything loaded during startup and
/// never released. That pair is the configuration to point a user at if any
/// of the lazy path misbehaves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    #[serde(default = "d_preload_at_startup")]
    pub preload_at_startup: bool,
    /// `0` disables unloading entirely; there is deliberately no lower bound
    /// above that, because a user who wants the models gone the moment a
    /// dictation ends is asking for something coherent.
    #[serde(default = "d_idle_unload_seconds")]
    pub idle_unload_seconds: u32,
}

fn d_preload_at_startup() -> bool {
    false
}
fn d_idle_unload_seconds() -> u32 {
    60
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            preload_at_startup: d_preload_at_startup(),
            idle_unload_seconds: d_idle_unload_seconds(),
        }
    }
}
```

- [ ] **Step 4: Add the field to `Config`**

In the `pub struct Config` block, insert between the `asr` and `normalize` fields:

```rust
    #[serde(default)]
    pub models: ModelsConfig,
```

- [ ] **Step 5: Add the section to `DEFAULT_CONFIG_TOML`**

In the `DEFAULT_CONFIG_TOML` raw string, insert between the `[asr]` and `[normalize]` blocks:

```
[models]
# Modelle erst beim ersten Tastendruck laden, statt beim Start. Aus heißt:
# das erste Diktat nach dem Start wartet einmalig auf die Modelle.
preload_at_startup = false
# Modelle nach dieser Ruhezeit wieder entladen und den Speicher freigeben.
# 0 = nie entladen.
idle_unload_seconds = 60

```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p owf-core config::`

Expected: PASS, including the pre-existing `DEFAULT_CONFIG_TOML` round-trip tests.

- [ ] **Step 7: Write the failing config-writer test**

Invariant 9's upgrade path: a `config.toml` written before this section existed
must *gain* it on the next GUI save. Add to `#[cfg(test)] mod tests` in
`crates/owf-core/src/config_write.rs`:

```rust
#[test]
fn a_config_without_a_models_section_gains_one_when_the_gui_saves() {
    // Every existing user's file looks like this. The GUI must be able to
    // write a setting whose whole section is missing from the document.
    let original = "[audio]\ndevice = \"default\"\n";
    let mut cfg = Config::from_str(original).unwrap();
    cfg.models.idle_unload_seconds = 30;
    let as_json = serde_json::to_value(&cfg).unwrap();

    let out = merge_json_into_toml(original, &as_json).unwrap();

    assert!(out.contains("[models]"), "section not created:\n{out}");
    assert!(out.contains("idle_unload_seconds = 30"), "value not written:\n{out}");
    // The rendered result must still be a config the daemon accepts.
    assert_eq!(Config::from_str(&out).unwrap().models.idle_unload_seconds, 30);
}

#[test]
fn changing_the_idle_timeout_produces_a_one_line_diff() {
    // Invariant 9: `config.toml` is a file the user is invited to edit by
    // hand, so a save that changes one setting must not rewrite the document.
    let original = crate::config::DEFAULT_CONFIG_TOML;
    let mut cfg = Config::from_str(original).unwrap();
    cfg.models.idle_unload_seconds = 300;
    let as_json = serde_json::to_value(&cfg).unwrap();

    let out = merge_json_into_toml(original, &as_json).unwrap();

    let changed: Vec<_> = original
        .lines()
        .zip(out.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(changed.len(), 1, "expected exactly one changed line, got {changed:?}");
    assert_eq!(changed[0].1.trim(), "idle_unload_seconds = 300");
}
```

- [ ] **Step 8: Run them**

Run: `cargo test -p owf-core config_write::`

Expected: PASS. `merge_json_into_toml` already creates missing tables; these
tests pin that it keeps doing so for this section. If
`a_config_without_a_models_section_gains_one_when_the_gui_saves` fails, that is
a real pre-existing bug in the writer and must be fixed in `config_write.rs`
(not worked around in the test) before continuing.

- [ ] **Step 9: Add the Settings window presentation**

In `src/settings/schema.ts`, make five edits.

`CATEGORIES` — add `"models"` as the first section of the `erweitert` pane:

```ts
  { id: "erweitert", title: "Erweitert", icon: "gear", sections: ["models", "normalize", "guardrail"] },
```

`SECTION_TITLES` — add:

```ts
  models: "Modelle & Speicher",
```

`LABELS` — add after the `asr.num_threads` entry:

```ts
  "models.preload_at_startup": "Modelle beim Start laden",
  "models.idle_unload_seconds": "Modelle entladen nach",
```

`UNITS` — add:

```ts
  "models.idle_unload_seconds": "s",
```

`HELP` — add after the `asr.num_threads` entry:

```ts
  "models.preload_at_startup":
    "Lädt Spracherkennung und Sprachmodell schon beim Programmstart. Aus heißt: sie werden erst beim ersten Tastendruck geladen — das spart im Leerlauf über ein Gigabyte, kostet aber beim ersten Diktat einmalig Wartezeit.",
  "models.idle_unload_seconds":
    "So lange nach dem letzten Diktat bleiben die Modelle im Speicher. Danach werden sie entladen und beim nächsten Tastendruck neu geladen. 0 heißt: nie entladen.",
```

- [ ] **Step 10: Build the frontend**

Run: `bun run build`

Expected: clean `tsc` and two Vite entry points built. Then run
`cargo test --workspace && cargo clippy --workspace --all-targets` — both clean,
test count 377 + 7 new = **384 passed**.

- [ ] **Step 11: Commit**

```bash
git add crates/owf-core/src/config.rs crates/owf-core/src/config_write.rs src/settings/schema.ts
git commit -m "feat(config): add the [models] section

preload_at_startup (default false) and idle_unload_seconds (default 60,
0 = never). Nothing reads them yet. Shown in Settings under Erweitert as
'Modelle & Speicher'.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: Split `warm_up`, and honour `preload_at_startup`

Pure refactor plus one branch. After this task the app starts with no models
loaded when `preload_at_startup = false` — and cannot yet load them, so
dictation is broken until Task 4. That is deliberate: this is the smallest
change a reviewer can judge on its own.

**Files:**
- Modify: `crates/owf-core/src/server.rs` (`warm_up` → `warm_up_recorder` + `load_models`; the startup thread in `start`)

**Interfaces:**
- Consumes: `owf_core::config::ModelsConfig` from Task 1.
- Produces:
  - `fn warm_up_recorder(cfg: &Config, daemon: &Arc<Daemon>)` — infallible, logs and continues.
  - `fn load_models(cfg: Config, daemon: &Arc<Daemon>) -> Result<(Pipeline, Option<LlamaServer>)>` — same error contract `warm_up` had: `Err` only when ASR/VAD fail; a dead `llama-server` degrades to `UnavailableNormalizer`.

- [ ] **Step 1: Extract `warm_up_recorder`**

In `crates/owf-core/src/server.rs`, replace the opening block of `warm_up`
(the `match Recorder::new(&cfg.audio)` statement at the top of its body) by
moving it into a new function placed immediately above `warm_up`:

```rust
/// I7's eager first attempt at `daemon.recorder`, split out of `warm_up` so
/// it still runs at startup even when `[models] preload_at_startup` is off
/// and no model is loaded at all.
///
/// Infallible by design: `ensure_recorder` retries `Recorder::new` lazily on
/// the next `ptt-start` regardless, so nothing is lost by this attempt
/// failing but the timing of one log line. It runs at startup, off the Tauri
/// `setup()` thread, because `Recorder::new` can block for a long time in
/// `cpal` (see CLAUDE.md's cpal gotcha) and a wedge there means no window, no
/// tray, and no way to quit.
fn warm_up_recorder(cfg: &Config, daemon: &Arc<Daemon>) {
    match Recorder::new(&cfg.audio) {
        Ok(r) => *lock_ignoring_poison(&daemon.recorder) = Some(r),
        Err(e) => {
            tracing::error!(
                error = ?e,
                "no capture device at startup; will retry on the next ptt-start"
            );
        }
    }
}
```

- [ ] **Step 2: Rename the remainder to `load_models`**

Rename `fn warm_up(cfg: Config, daemon: &Arc<Daemon>) -> Result<(Pipeline, Option<LlamaServer>)>`
to `fn load_models(...)` with the identical signature, and delete the
`Recorder::new` block now living in `warm_up_recorder`. Update its doc comment:
keep the C1 and R9 paragraphs verbatim (they still describe this function's
error contract exactly), delete the I7 paragraph (it now belongs to
`warm_up_recorder`), and add at the top:

```rust
/// Builds the pipeline and, when normalization is enabled, the supervised
/// `llama-server` behind it.
///
/// Called at startup only when `[models] preload_at_startup` is on; otherwise
/// called on demand by `ensure_models_loaded`, off the press that needs it.
/// It therefore must not assume it is running exactly once, or at startup.
```

- [ ] **Step 3: Branch the startup thread**

In `start`, replace `std::thread::spawn(move || match warm_up(cfg, &daemon) {`
and its two match arms with:

```rust
        std::thread::spawn(move || {
            warm_up_recorder(&cfg, &daemon);

            // The lazy default: startup is finished the moment the recorder
            // attempt is. Nothing model-shaped is loaded, `models_loaded`
            // stays false, and the first `ptt-start` is what brings the
            // models up (`ensure_models_loaded`). `WARMING` still means
            // "startup has not finished" -- it is simply brief here.
            if !cfg.models.preload_at_startup {
                daemon.state.store(IDLE, Ordering::SeqCst);
                daemon.broadcast(OverlayEvent::Idle);
                tracing::info!("ready (models load on demand)");
                return;
            }

            match load_models(cfg, &daemon) {
                Ok((pipeline, server)) => {
                    let available = server.is_some();
                    *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
                    *lock_ignoring_poison(&daemon.llama) = server;
                    // Populated immediately (accuracy for `status` from the
                    // moment warm-up resolves), independent of when
                    // `spawn_housekeeping`'s own loop next wakes up and
                    // re-confirms the same thing to decide whether a
                    // `NormalizeDegraded`/`NormalizeRecovered` broadcast is due.
                    daemon.normalize_available.store(available, Ordering::SeqCst);
                    daemon.state.store(IDLE, Ordering::SeqCst);
                    daemon.broadcast(OverlayEvent::Idle);
                    tracing::info!("ready");
                }
                Err(e) => {
                    // The only way `load_models` returns `Err`: ASR/VAD failed
                    // to load. A `llama-server` failure never reaches here (see
                    // `load_models`'s doc comment) -- it's absorbed into
                    // `UnavailableNormalizer` and the daemon still comes up
                    // `Idle`. This is a permanent, fatal failure (Task 3, Work
                    // Item 3): `FAILED` plus the stored reason is what lets a
                    // subscriber connecting *after* this moment still learn the
                    // daemon is broken, instead of `snapshot_event(WARMING)`'s
                    // permanent spinner.
                    //
                    // Reachable only via `preload_at_startup`. The lazy path
                    // treats the same failure as retryable instead -- see
                    // `run_utterance`, and spec 2026-08-29 §4.
                    let reason = format!("warm-up failed: {e}");
                    tracing::error!(error = ?e, "warm-up failed; daemon marked failed");
                    *lock_ignoring_poison(&daemon.fatal_error) = Some(reason.clone());
                    daemon.state.store(FAILED, Ordering::SeqCst);
                    daemon.broadcast(OverlayEvent::Error { reason });
                }
            }
        });
```

Note the `daemon.models_loaded` store is **not** here — Task 3 introduces that
field and adds the store in this same `Ok` arm.

- [ ] **Step 4: Build and run the suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: **384 passed**, clippy clean. No test count change — this task is a
refactor plus a branch that no test exercises yet.

- [ ] **Step 5: Commit**

```bash
git add crates/owf-core/src/server.rs
git commit -m "refactor(server): split warm_up into warm_up_recorder and load_models

The recorder attempt (I7) is a startup concern and always runs; the model
half becomes callable on demand. preload_at_startup = false now skips it
and goes straight to IDLE with no models loaded.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: `models_loaded` and `ensure_models_loaded`

Adds the lazy loader, its lock discipline, and the two "unloaded is a normal
state now" consequences (`Status`'s `warm`, `Reload`'s `None` arm). Still
nothing calls the loader — Task 4 wires it to the press.

**Files:**
- Modify: `crates/owf-core/src/server.rs` (`Daemon` fields, both construction sites, `ensure_models_loaded`, `Status`, `Reload`, `SetConfig`)

**Interfaces:**
- Consumes: `load_models` from Task 2.
- Produces:
  - `Daemon.models_loaded: AtomicBool`, `Daemon.load_lock: Mutex<()>`, `Daemon.last_activity: Mutex<Instant>`, `Daemon.models_cfg: Mutex<ModelsConfig>`
  - `fn ensure_models_loaded(daemon: &Arc<Daemon>) -> Result<(), String>`
  - `fn ensure_models_loaded_with(daemon: &Arc<Daemon>, load: &mut dyn FnMut() -> Result<(Pipeline, Option<LlamaServer>)>) -> Result<(), String>` — the injectable seam, in the shape `supervise_llama_once`'s `respawn` already establishes.

- [ ] **Step 1: Write the failing tests**

Add to `#[cfg(test)] mod tests` in `crates/owf-core/src/server.rs`:

```rust
/// A `Pipeline` built from the stub stages this module already defines, for
/// tests that only need *a* pipeline to be installed. None of them ever call
/// `process` on it -- a real one needs an ASR model on disk, which is why
/// this file's four model-dependent tests are `#[ignore]`d.
fn stub_pipeline() -> Pipeline {
    Pipeline::new(
        Config::from_str("").unwrap(),
        Box::new(PanickingTranscriber),
        Box::new(WholeBuffer),
        Box::new(AlwaysEnglish),
        Box::new(NeverNormalizer),
        Box::new(MockInjector::default()),
    )
}

/// The seam that makes the loader testable at all. Counts its own calls, so
/// idempotence is observable.
fn counting_loader(
    calls: Arc<AtomicU64>,
) -> impl FnMut() -> anyhow::Result<(Pipeline, Option<LlamaServer>)> {
    move || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok((stub_pipeline(), None))
    }
}

#[test]
fn loading_the_models_twice_only_loads_them_once() {
    let daemon = fake_daemon(IDLE);
    let calls = Arc::new(AtomicU64::new(0));

    let mut load = counting_loader(Arc::clone(&calls));
    ensure_models_loaded_with(&daemon, &mut load).unwrap();
    ensure_models_loaded_with(&daemon, &mut load).unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the second call rebuilt the pipeline");
    assert!(daemon.models_loaded.load(Ordering::SeqCst));
    assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
}

#[test]
fn a_failed_load_leaves_the_daemon_unloaded_and_retryable() {
    // Spec 2026-08-29 §4: lazily, a load failure is not fatal. It must leave
    // no half-installed state behind, so the next press can simply try again.
    let daemon = fake_daemon(IDLE);
    let mut load = || anyhow::bail!("model paths: no such file");

    let err = ensure_models_loaded_with(&daemon, &mut load).unwrap_err();

    assert!(err.contains("model paths"), "the real reason must survive: {err}");
    assert!(!daemon.models_loaded.load(Ordering::SeqCst));
    assert!(lock_ignoring_poison(&daemon.pipeline).is_none());
    assert_eq!(daemon.state.load(Ordering::SeqCst), IDLE, "must not latch FAILED");
}

#[test]
fn status_reports_model_residency_rather_than_startup_completion() {
    let daemon = fake_daemon(IDLE);

    let before = dispatch(&daemon, Request::Status);
    assert_eq!(before.warm, Some(false), "idle with no models is not warm");

    let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
    ensure_models_loaded_with(&daemon, &mut load).unwrap();

    let after = dispatch(&daemon, Request::Status);
    assert_eq!(after.warm, Some(true));
}

#[test]
fn status_answers_promptly_while_an_utterance_holds_the_pipeline_lock() {
    // The defect this field exists to prevent: `process_utterance` holds
    // `pipeline`'s mutex for the whole `process` call, so a `Status` handler
    // that asked `pipeline.is_some()` would hang for the length of a
    // transcription. `models_loaded` is read instead.
    let daemon = fake_daemon(TRANSCRIBING);
    let held = lock_ignoring_poison(&daemon.pipeline);

    let start = Instant::now();
    let r = dispatch(&daemon, Request::Status);

    assert!(r.ok);
    assert!(start.elapsed() < Duration::from_millis(100), "Status blocked on the pipeline lock");
    drop(held);
}

#[test]
fn an_unloaded_pipeline_is_not_a_reason_to_refuse_a_reload() {
    // Before lazy loading this arm was documented as unreachable. It is now
    // the ordinary state of an idle daemon between dictations, and the next
    // `load_models` reads the file anyway.
    // `scratch_config` is this module's existing helper; `tempfile` is not a
    // dependency of this crate.
    let path = scratch_config("reload-unloaded");
    let daemon = fake_daemon_at(IDLE, false, path);
    assert!(lock_ignoring_poison(&daemon.pipeline).is_none());

    let r = dispatch(&daemon, Request::Reload);

    assert!(r.ok, "reload refused with no pipeline: {:?}", r.err);
}
```

`stub_pipeline`, `counting_loader` and `stub_llama` are defined in the code
block above and go in the same `#[cfg(test)] mod tests`. `PanickingTranscriber`,
`WholeBuffer`, `AlwaysEnglish`, `NeverNormalizer` and `MockInjector` all already
exist in that module — do not define second copies.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p owf-core --lib server::tests::loading_the_models_twice server::tests::a_failed_load server::tests::status_reports server::tests::status_answers_promptly server::tests::an_unloaded_pipeline`

Expected: FAIL to compile — `cannot find function ensure_models_loaded_with`, `no field models_loaded on type Daemon`.

- [ ] **Step 3: Add the four `Daemon` fields**

In `struct Daemon`, after the `pipeline` field, add:

```rust
    /// Held across a model load so two concurrent presses share one load
    /// rather than racing two, and so an unload can never interleave with
    /// one.
    ///
    /// **Lock order: this is always taken before `pipeline`, never after.**
    /// `process_utterance` takes only `pipeline`, and holds it for the whole
    /// `process` call; `ensure_models_loaded` and `unload_models` take this
    /// first and hold `pipeline` only for the assignment.
    load_lock: Mutex<()>,
    /// Lock-free projection of "`pipeline` is `Some`", for readers that must
    /// never block on it.
    ///
    /// Not redundant bookkeeping, for the same reason `normalize_available`
    /// is not: `process_utterance` holds `pipeline`'s mutex for the entire
    /// `process` call (deliberately -- see its own comment on the
    /// sherpa-onnx FFI), so any thread that answers a question by locking
    /// `pipeline` blocks for the length of a whole transcription. Two such
    /// readers exist -- `dispatch`'s `Status` handler, which would make
    /// `--status` hang mid-utterance, and the housekeeping thread, which
    /// would stop reaping subscribers for the same window. Both read this.
    models_loaded: AtomicBool,
    /// When the daemon was last doing something dictation-shaped: bumped by
    /// `start_recording` and by every utterance that ends (`IdleOnExit`).
    /// The idle-unload deadline is measured from this.
    last_activity: Mutex<Instant>,
    /// `[models]`, mirrored here so a Settings change takes effect without a
    /// restart -- the same reason, and the same shape, as `audio_cfg`.
    models_cfg: Mutex<ModelsConfig>,
```

Add `use crate::config::ModelsConfig;` to the imports if `config::` items are
imported by name there.

- [ ] **Step 4: Add them to both construction sites**

In `start`'s `Arc::new(Daemon { .. })`:

```rust
        load_lock: Mutex::new(()),
        models_loaded: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
        models_cfg: Mutex::new(cfg.models.clone()),
```

In the test helper `fake_daemon_at`:

```rust
            load_lock: Mutex::new(()),
            models_loaded: AtomicBool::new(false),
            last_activity: Mutex::new(Instant::now()),
            models_cfg: Mutex::new(ModelsConfig::default()),
```

- [ ] **Step 5: Set `models_loaded` on the preload path**

In the startup thread's `Ok` arm from Task 2, add immediately after the
`normalize_available` store:

```rust
                    daemon.models_loaded.store(true, Ordering::SeqCst);
```

- [ ] **Step 6: Write `ensure_models_loaded`**

Place both functions immediately after `load_models`:

```rust
/// Brings the models up if they are not already, and does nothing if they
/// are. Idempotent, safe to call from any thread, and safe to call
/// concurrently: `load_lock` makes two presses share one load rather than
/// racing two.
///
/// Reads its `Config` from disk at the moment of the call rather than from a
/// snapshot taken at startup, as `GetConfig` and `Reload` already do. A
/// consequence worth naming: for a user running lazily, a changed `[asr]` or
/// `[normalize]` setting takes effect on the next dictation rather than the
/// next restart. That is a side effect, not a promise -- `schema.ts`'s
/// `RESTART_SECTIONS` is unchanged because it is still correct whenever the
/// models happen to be resident.
fn ensure_models_loaded(daemon: &Arc<Daemon>) -> Result<(), String> {
    let cfg = Config::load_from(&daemon.config_path).map_err(|e| format!("config error: {e}"))?;
    let mut load = || load_models(cfg.clone(), daemon);
    ensure_models_loaded_with(daemon, &mut load)
}

/// [`ensure_models_loaded`] with the loader injected -- the seam a test uses
/// to exercise the locking, the idempotence and the failure path without an
/// ASR model on disk, in the same shape `supervise_llama_once`'s `respawn`
/// parameter already establishes.
///
/// On failure it installs nothing at all: `pipeline` stays `None`,
/// `models_loaded` stays `false`, and `state` is left alone. That is what
/// makes a lazy load failure retryable on the next press (spec 2026-08-29 §4)
/// rather than the permanent `FAILED` a startup failure still produces.
fn ensure_models_loaded_with(
    daemon: &Arc<Daemon>,
    load: &mut dyn FnMut() -> Result<(Pipeline, Option<LlamaServer>)>,
) -> Result<(), String> {
    let _guard = lock_ignoring_poison(&daemon.load_lock);
    if daemon.models_loaded.load(Ordering::SeqCst) {
        return Ok(());
    }
    match load() {
        Ok((pipeline, server)) => {
            let available = server.is_some();
            *lock_ignoring_poison(&daemon.pipeline) = Some(pipeline);
            *lock_ignoring_poison(&daemon.llama) = server;
            daemon.normalize_available.store(available, Ordering::SeqCst);
            // Last, and only on the success path: this is what the
            // housekeeping thread's supervisor gate and `Status` both read,
            // so it must never be true before the pipeline is installed.
            daemon.models_loaded.store(true, Ordering::SeqCst);
            tracing::info!("models loaded");
            Ok(())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}
```

- [ ] **Step 7: Change `Status`'s `warm`**

In `dispatch`'s `Request::Status` arm, replace:

```rust
            r.warm = Some(current != WARMING);
```

with:

```rust
            // "The models are resident right now", not "startup finished".
            // The atomic, never the `pipeline` lock -- see the field's own
            // doc comment for the hang that would otherwise be.
            r.warm = Some(daemon.models_loaded.load(Ordering::SeqCst));
```

- [ ] **Step 8: Fix `Reload`'s `None` arm**

In `dispatch`'s `Request::Reload` arm, replace the `None` arm:

```rust
                        // Unreachable in normal operation: IDLE is only ever
                        // reached once warm-up has populated `pipeline` (see
                        // `process_utterance`'s identical note).
                        None => Response::err("reload requires the pipeline to be warmed up"),
```

with:

```rust
                        // No longer unreachable, and no longer an error: with
                        // `[models] preload_at_startup` off, an idle daemon
                        // between dictations routinely has no pipeline. The
                        // config parsed above is all the validation there is
                        // to do -- `ensure_models_loaded` reads the file
                        // again when the next press brings the models up.
                        None => Response::ok(State::Idle),
```

- [ ] **Step 9: Keep `models_cfg` live on save**

In `dispatch`'s `Request::SetConfig` arm, directly above the existing
`if new_cfg.audio != old.audio {` block:

```rust
            // Mirrored for the same reason `audio_cfg` is: the housekeeping
            // thread reads the idle timeout on every tick, and a user who
            // changes it in Settings must not have to restart for it.
            *lock_ignoring_poison(&daemon.models_cfg) = new_cfg.models.clone();
```

`restart_reason` is deliberately not extended — `[models]` applies live, so it
must not tell the user a restart is needed.

- [ ] **Step 10: Run the tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: **389 passed** (384 + 5 new), clippy clean.

- [ ] **Step 11: Commit**

```bash
git add crates/owf-core/src/server.rs
git commit -m "feat(server): add ensure_models_loaded and models_loaded

models_loaded is the lock-free projection of pipeline.is_some(), for
Status and the housekeeping thread -- locking pipeline there would block
for the length of a whole transcription. Reload no longer refuses an
unloaded pipeline; status' warm now means model residency.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Load on the press

Wires the loader to `start_recording` (in the background, after the mic is
live) and to `run_utterance` (blocking, before the pipeline runs). After this
task the lazy path works end to end: dictation functions with
`preload_at_startup = false`.

**Files:**
- Modify: `crates/owf-core/src/server.rs` (`start_recording`, `run_utterance`, `IdleOnExit`)

**Interfaces:**
- Consumes: `ensure_models_loaded` from Task 3.
- Produces: `IdleOnExit::new(state: &AtomicU8, broadcaster: Broadcaster<'_>, last_activity: &Mutex<Instant>)` — one added parameter.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_recording_refreshes_the_idle_deadline_before_it_changes_state() {
    // Ordering matters: `unload_models` re-checks the deadline under
    // `load_lock`, so a press that bumps `last_activity` before storing
    // RECORDING can never be unloaded out from under.
    let daemon = fake_daemon(IDLE);
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

    touch_activity(&daemon);

    assert!(
        lock_ignoring_poison(&daemon.last_activity).elapsed() < Duration::from_secs(1),
        "the deadline was not refreshed"
    );
}

#[test]
fn an_utterance_ending_refreshes_the_idle_deadline_even_on_a_panic() {
    // `IdleOnExit` already recovers `state` when the utterance thread
    // unwinds; the deadline must ride along, or a panicking utterance leaves
    // a stale deadline and the models are unloaded early.
    let daemon = fake_daemon(TRANSCRIBING);
    let stale = Instant::now() - Duration::from_secs(3600);
    *lock_ignoring_poison(&daemon.last_activity) = stale;

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = IdleOnExit::new(&daemon.state, daemon.broadcaster(), &daemon.last_activity);
        panic!("pipeline blew up");
    }));

    assert!(result.is_err());
    assert_eq!(daemon.state.load(Ordering::SeqCst), IDLE);
    assert!(
        lock_ignoring_poison(&daemon.last_activity).elapsed() < Duration::from_secs(1),
        "the deadline was not refreshed on unwind"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p owf-core --lib server::tests::a_recording_refreshes server::tests::an_utterance_ending_refreshes`

Expected: FAIL to compile — `cannot find function touch_activity`, and
`IdleOnExit::new` takes 2 arguments.

- [ ] **Step 3: Add `touch_activity`**

Place it above `start_recording`:

```rust
/// Refreshes the idle-unload deadline. Called at the start of every
/// recording and at the end of every utterance, which between them cover
/// every way the daemon does dictation-shaped work.
fn touch_activity(daemon: &Daemon) {
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now();
}
```

- [ ] **Step 4: Teach `IdleOnExit` about the deadline**

Replace the struct, its `new`, and its `Drop`:

```rust
struct IdleOnExit<'a> {
    state: &'a AtomicU8,
    broadcaster: Broadcaster<'a>,
    /// Refreshed on every exit path, unwinding included: a panicking
    /// utterance that left a stale deadline behind would have its models
    /// unloaded early, on a clock that started before the utterance did.
    last_activity: &'a Mutex<Instant>,
    terminal_sent: Cell<bool>,
}

impl<'a> IdleOnExit<'a> {
    fn new(
        state: &'a AtomicU8,
        broadcaster: Broadcaster<'a>,
        last_activity: &'a Mutex<Instant>,
    ) -> Self {
        Self { state, broadcaster, last_activity, terminal_sent: Cell::new(false) }
    }
}

impl Drop for IdleOnExit<'_> {
    fn drop(&mut self) {
        *lock_ignoring_poison(self.last_activity) = Instant::now();
        self.state.store(IDLE, Ordering::SeqCst);
        if !self.terminal_sent.get() {
            self.broadcaster.broadcast(OverlayEvent::Idle);
        }
    }
}
```

Update every `IdleOnExit::new` call site — `run_utterance` and any existing
tests — to pass `&daemon.last_activity` as the third argument.

- [ ] **Step 5: Run the two tests**

Run: `cargo test -p owf-core --lib server::tests::a_recording_refreshes server::tests::an_utterance_ending_refreshes`

Expected: PASS.

- [ ] **Step 6: Bump the deadline and start the load in `start_recording`**

In `start_recording`, add as the **first statement of the function**, above
the existing `daemon.broadcast(OverlayEvent::Opening);`:

```rust
    // Before anything else, and specifically before the `RECORDING` store
    // below: `unload_models` re-checks this deadline while holding
    // `load_lock`, so a press that refreshes it first can never have its
    // models pulled out from under it by an unload that was already in
    // flight. Same discipline as `recording_epoch` and the safety valve.
    touch_activity(daemon);
```

Then, immediately after the existing `spawn_safety_valve(daemon, epoch);` call:

```rust
    // The models come up on their own thread, deliberately *after* capture is
    // already live. Nothing in the capture path needs a model, so the
    // microphone opens on exactly the timeline it always did and the load
    // overlaps with the user speaking -- for any utterance longer than the
    // load, it costs nothing at all. `run_utterance` calls the same function
    // and blocks there if this has not finished by the second press.
    //
    // A failure here is logged and dropped: `run_utterance` retries and is
    // the one that reports it to the user, so a failed load never produces
    // two error broadcasts for one press.
    {
        let d = Arc::clone(daemon);
        std::thread::spawn(move || {
            if let Err(e) = ensure_models_loaded(&d) {
                tracing::warn!(error = %e, "background model load failed; ptt-stop will retry");
            }
        });
    }
```

- [ ] **Step 7: Block on the load in `run_utterance`**

In `run_utterance`, insert between the `let class = ...` line and the
`process_utterance(` call:

```rust
    // Blocks only if the load started by `start_recording` has not finished.
    // No new overlay state: the daemon really is `TRANSCRIBING` (`claim_busy`
    // ran before this thread was spawned), so the overlay shows its existing
    // Transcribing view for however long this takes.
    if let Err(reason) = ensure_models_loaded(&daemon) {
        // Spec 2026-08-29 §4: retryable, not fatal. `FAILED` and
        // `fatal_error` are left alone, so the next press tries again -- on a
        // fresh install that means downloading the models in the Setup pane
        // and pressing again, with no restart. `reason` carries
        // `SherpaTranscriber`/`SileroTrimmer`'s own "model paths" error,
        // which is the string that tells the user which pane to open.
        tracing::error!(error = %reason, "models could not be loaded for this utterance");
        daemon.broadcast(OverlayEvent::Error { reason });
        idle_on_exit.terminal_sent.set(true);
        return;
    }

```

- [ ] **Step 8: Run the full suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: **391 passed** (389 + 2 new), clippy clean.

- [ ] **Step 9: Verify by hand that the app still dictates nothing away**

Run: `cargo run -p openwhisprflow --features custom-protocol -- --replay src-tauri/fixtures/replay-full.ndjson`

Expected: the overlay renders every event in the fixture, as before. `--replay`
does not touch `Daemon`, so this is a regression check on the overlay, not on
this task's logic.

- [ ] **Step 10: Commit**

```bash
git add crates/owf-core/src/server.rs
git commit -m "feat(server): load the models on the press, not at startup

start_recording opens the mic first and then loads on a background
thread, so the load overlaps with the user speaking. run_utterance
blocks on the same call. A lazy load failure is retryable rather than
latching FAILED.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: `should_unload` and `unload_models`

The release half, as a pure decision function plus the action it gates.
Nothing calls them yet — Task 6 wires them to the housekeeping tick.

**Files:**
- Modify: `crates/owf-core/src/server.rs`

**Interfaces:**
- Consumes: `Daemon.models_loaded`, `Daemon.last_activity`, `Daemon.models_cfg`, `Daemon.load_lock` from Task 3; `kill_llama` (existing).
- Produces:
  - `fn should_unload(last_activity: Instant, now: Instant, idle_seconds: u32, state: u8, loaded: bool) -> bool`
  - `fn unload_models(daemon: &Daemon) -> bool` — `true` if it actually unloaded.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn should_unload_waits_for_the_full_idle_timeout() {
    let now = Instant::now();
    let long_ago = now - Duration::from_secs(60);
    let recent = now - Duration::from_secs(59);

    assert!(should_unload(long_ago, now, 60, IDLE, true), "the boundary is inclusive");
    assert!(!should_unload(recent, now, 60, IDLE, true), "unloaded a second early");
}

#[test]
fn should_unload_never_fires_with_zero_seconds_configured() {
    // 0 is the documented "never unload" setting, not "unload immediately".
    let now = Instant::now();
    assert!(!should_unload(now - Duration::from_secs(86_400), now, 0, IDLE, true));
}

#[test]
fn should_unload_never_fires_when_nothing_is_loaded() {
    let now = Instant::now();
    assert!(!should_unload(now - Duration::from_secs(3600), now, 60, IDLE, false));
}

#[test]
fn should_unload_accepts_idle_and_paused_but_no_busy_state() {
    let now = Instant::now();
    let stale = now - Duration::from_secs(3600);

    // Paused counts as idle deliberately: it is reached only by a CAS from
    // IDLE, so there is never an utterance to protect, and a user who paused
    // dictation is the clearest signal the models are not about to be needed.
    for s in [IDLE, PAUSED] {
        assert!(should_unload(stale, now, 60, s, true), "state {s} should unload");
    }
    for s in [WARMING, RECORDING, TRANSCRIBING, NORMALIZING, INJECTING, FAILED] {
        assert!(!should_unload(stale, now, 60, s, true), "state {s} must not unload");
    }
}

#[test]
fn unloading_drops_the_pipeline_and_reaps_llama() {
    let daemon = fake_daemon(IDLE);
    let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
    ensure_models_loaded_with(&daemon, &mut load).unwrap();
    *lock_ignoring_poison(&daemon.llama) = Some(stub_llama());
    daemon.normalize_available.store(true, Ordering::SeqCst);
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

    assert!(unload_models(&daemon));

    assert!(!daemon.models_loaded.load(Ordering::SeqCst));
    assert!(lock_ignoring_poison(&daemon.pipeline).is_none());
    assert!(lock_ignoring_poison(&daemon.llama).is_none());
    assert!(!daemon.normalize_available.load(Ordering::SeqCst));
}

#[test]
fn unloading_declines_when_a_recording_started_after_the_decision() {
    // The race `unload_models`' re-check under `load_lock` exists to close:
    // the housekeeping thread decided to unload, then a press moved the
    // deadline before it got the lock.
    let daemon = fake_daemon(IDLE);
    let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
    ensure_models_loaded_with(&daemon, &mut load).unwrap();
    touch_activity(&daemon); // the press lands

    assert!(!unload_models(&daemon), "unloaded despite a fresh deadline");
    assert!(daemon.models_loaded.load(Ordering::SeqCst));
    assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
}

#[test]
fn unloading_declines_mid_utterance() {
    let daemon = fake_daemon(TRANSCRIBING);
    let mut load = counting_loader(Arc::new(AtomicU64::new(0)));
    ensure_models_loaded_with(&daemon, &mut load).unwrap();
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

    assert!(!unload_models(&daemon), "unloaded a pipeline an utterance is using");
    assert!(lock_ignoring_poison(&daemon.pipeline).is_some());
}
```

Add this helper alongside them, in the same `#[cfg(test)] mod tests`. It is
defined here rather than in Task 3 because Task 3 has no use for it, and an
unused `#[cfg(test)]` function fails that task's `clippy --all-targets` gate:

```rust
/// A stand-in for the supervised `llama-server` child. A real one cannot run
/// on this machine (no ggml compute backend), so every test that needs one
/// uses `sleep 300` through the test-only `LlamaServer::from_child`, exactly
/// as `kill_llama_terminates_a_stub_child_and_is_idempotent` already does.
fn stub_llama() -> LlamaServer {
    let child = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("spawning a stub child (`sleep 300`) for this test");
    LlamaServer::from_child(child, 0)
}
```

`counting_loader` and `stub_pipeline` come from Task 3, `touch_activity` from
Task 4.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p owf-core --lib server::tests::should_unload server::tests::unloading_`

Expected: FAIL to compile — `cannot find function should_unload`.

- [ ] **Step 3: Write `should_unload`**

Place it directly above `spawn_housekeeping`:

```rust
/// Whether the idle-unload deadline has passed and it is safe to act on it.
///
/// Pure, and therefore directly unit-testable, for the same reason
/// `should_emit_level` is: the interesting part is the boundary conditions,
/// not the clock.
///
/// `PAUSED` counts as idle deliberately. It is reached only by a CAS from
/// `IDLE` (Task 13), so there is never an utterance to protect while paused,
/// and a user who has explicitly paused dictation is the clearest possible
/// signal that the models are not about to be needed.
fn should_unload(
    last_activity: Instant,
    now: Instant,
    idle_seconds: u32,
    state: u8,
    loaded: bool,
) -> bool {
    if !loaded || idle_seconds == 0 {
        return false;
    }
    if state != IDLE && state != PAUSED {
        return false;
    }
    now.duration_since(last_activity) >= Duration::from_secs(idle_seconds as u64)
}
```

- [ ] **Step 4: Write `unload_models`**

Directly below `should_unload`:

```rust
/// Releases the models, if the deadline still says to. Returns whether it
/// actually did.
///
/// Takes `&Daemon` rather than the seven bare pieces it touches, which
/// `fake_daemon` makes perfectly testable -- `claim_busy` already takes one
/// the same way. The pieces-not-`&Daemon` convention elsewhere in this file
/// exists for functions a test cannot otherwise reach without live audio
/// hardware; this is not one of them.
///
/// The re-check under `load_lock` is what makes this race-free.
/// `start_recording` refreshes `last_activity` *before* it stores
/// `RECORDING`, so a recording that begins between the housekeeping thread's
/// decision and its acquisition of the lock has already moved the deadline,
/// and this declines. The worst remaining outcome is an unload immediately
/// followed by the load that same press requested: wasteful for one
/// dictation, never incorrect.
///
/// Nothing is broadcast. The daemon's `State` does not change -- it was
/// `IDLE` before and is `IDLE` after -- and the tray's icon is driven
/// entirely by broadcasts (`tray.rs`, "Icon and daemon state"), so an event
/// here would mean inventing a tray icon for it too.
fn unload_models(daemon: &Daemon) -> bool {
    let _guard = lock_ignoring_poison(&daemon.load_lock);

    let idle_seconds = lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds;
    if !should_unload(
        *lock_ignoring_poison(&daemon.last_activity),
        Instant::now(),
        idle_seconds,
        daemon.state.load(Ordering::SeqCst),
        daemon.models_loaded.load(Ordering::SeqCst),
    ) {
        return false;
    }

    // Cleared first: it is what the supervisor's gate reads, and it must
    // never still say "loaded" while the pipeline is being torn out.
    daemon.models_loaded.store(false, Ordering::SeqCst);
    *lock_ignoring_poison(&daemon.pipeline) = None;
    // The same kill-then-reap path `shutdown` uses; a `None` llama is fine.
    kill_llama(&daemon.llama);
    // Not `NormalizeDegraded`: normalization is not degraded, it is not
    // currently loaded, and the overlay's degraded badge means the former.
    daemon.normalize_available.store(false, Ordering::SeqCst);
    tracing::info!(idle_seconds, "models unloaded after idle timeout");
    true
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: **398 passed** (391 + 7 new), clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/owf-core/src/server.rs
git commit -m "feat(server): add should_unload and unload_models

Pure decision function plus the action it gates. The re-check under
load_lock closes the race where a press lands between the decision and
the unload. Nothing calls them yet.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Wire the unload into housekeeping, and stop the supervisor resurrecting llama

The last functional task. After it the feature is complete: models load on the
press and are released on the timer, and `llama-server` stays dead once
deliberately killed.

**Files:**
- Modify: `crates/owf-core/src/server.rs` (`spawn_housekeeping`, plus a new `time_until_unload`)

**Interfaces:**
- Consumes: `unload_models`, `should_unload` from Task 5; `Daemon.models_loaded` from Task 3.
- Produces: `fn time_until_unload(daemon: &Daemon) -> Option<Duration>`, `fn should_supervise_llama(state: u8, models_loaded: bool) -> bool`, `const MIN_UNLOAD_TICK: Duration`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn there_is_no_unload_deadline_when_nothing_is_loaded() {
    let daemon = fake_daemon(IDLE);
    assert_eq!(time_until_unload(&daemon), None);
}

#[test]
fn there_is_no_unload_deadline_when_unloading_is_disabled() {
    let daemon = fake_daemon(IDLE);
    daemon.models_loaded.store(true, Ordering::SeqCst);
    lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds = 0;
    assert_eq!(time_until_unload(&daemon), None);
}

#[test]
fn the_unload_deadline_shrinks_as_the_idle_time_passes() {
    let daemon = fake_daemon(IDLE);
    daemon.models_loaded.store(true, Ordering::SeqCst);
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(50);

    let remaining = time_until_unload(&daemon).expect("a deadline");

    // 60 s configured, 50 s elapsed -- about ten left, and never more.
    assert!(remaining <= Duration::from_secs(10), "got {remaining:?}");
    assert!(remaining >= Duration::from_secs(9), "got {remaining:?}");
}

#[test]
fn a_passed_deadline_never_shrinks_the_tick_below_the_floor() {
    // Without the floor, a deadline that has passed while the daemon is busy
    // makes the housekeeping loop spin at zero timeout until the utterance
    // finishes.
    let daemon = fake_daemon(TRANSCRIBING);
    daemon.models_loaded.store(true, Ordering::SeqCst);
    *lock_ignoring_poison(&daemon.last_activity) = Instant::now() - Duration::from_secs(3600);

    assert_eq!(time_until_unload(&daemon), Some(MIN_UNLOAD_TICK));
}

#[test]
fn the_supervisor_is_skipped_entirely_while_the_models_are_unloaded() {
    // Without this gate the supervisor spawns a fresh 697 MB llama-server
    // seconds after every unload, forever, and the whole feature is undone.
    assert!(!should_supervise_llama(IDLE, false), "unloaded: must not supervise");
    assert!(!should_supervise_llama(PAUSED, false));
    // A load still in flight owns its own `spawn_and_wait_healthy` child and
    // has not set `models_loaded` yet, so it is covered by the same arm.
    assert!(!should_supervise_llama(RECORDING, false));
}

#[test]
fn the_supervisor_still_stays_inert_during_warm_up() {
    // The pre-existing half of this gate, kept: `warm_up` has not settled
    // `daemon.llama` yet, so acting now could spawn a second llama-server
    // racing its own.
    assert!(!should_supervise_llama(WARMING, false));
    assert!(!should_supervise_llama(WARMING, true));
}

#[test]
fn the_supervisor_runs_once_the_models_are_loaded() {
    for s in [IDLE, PAUSED, RECORDING, TRANSCRIBING, NORMALIZING, INJECTING, FAILED] {
        assert!(should_supervise_llama(s, true), "state {s} should supervise");
    }
}

#[test]
fn the_housekeeping_tick_answers_while_an_utterance_holds_the_pipeline_lock() {
    // The second reader `models_loaded` exists for: this runs on the thread
    // that also reaps dead subscribers, so blocking it behind a
    // transcription would stall that unrelated work for the whole utterance.
    let daemon = fake_daemon(TRANSCRIBING);
    daemon.models_loaded.store(true, Ordering::SeqCst);
    let held = lock_ignoring_poison(&daemon.pipeline);

    let start = Instant::now();
    let _ = time_until_unload(&daemon);
    let _ = should_supervise_llama(
        daemon.state.load(Ordering::SeqCst),
        daemon.models_loaded.load(Ordering::SeqCst),
    );

    assert!(start.elapsed() < Duration::from_millis(100), "the tick blocked on the pipeline lock");
    drop(held);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p owf-core --lib server::tests::there_is_no_unload server::tests::the_unload_deadline server::tests::a_passed_deadline server::tests::the_supervisor server::tests::the_housekeeping_tick`

Expected: FAIL to compile — `cannot find function time_until_unload`, `cannot find function should_supervise_llama`, `cannot find value MIN_UNLOAD_TICK`.

- [ ] **Step 3: Add `MIN_UNLOAD_TICK` and `time_until_unload`**

Directly above `should_unload`:

```rust
/// The floor on an unload-derived housekeeping tick.
///
/// When the deadline has passed but the daemon is busy, `unload_models`
/// declines and the remaining time stays zero -- without this floor the
/// housekeeping loop would spin at a zero timeout until the utterance
/// finished. One second is invisible against a timeout measured in minutes.
const MIN_UNLOAD_TICK: Duration = Duration::from_secs(1);

/// How long until the idle-unload deadline, or `None` when there is no
/// deadline at all: nothing is loaded, or unloading is disabled.
///
/// This is what makes `idle_unload_seconds = 60` mean 60 seconds rather than
/// "somewhere in 60-70" at the mercy of `HEALTH_POLL_INTERVAL`: the
/// housekeeping loop takes the smaller of its own wait and this.
///
/// Reads `models_loaded`, never the `pipeline` lock -- this runs on the same
/// thread that reaps dead subscribers, and blocking it behind a
/// transcription would stall that unrelated work for the whole utterance.
fn time_until_unload(daemon: &Daemon) -> Option<Duration> {
    let idle_seconds = lock_ignoring_poison(&daemon.models_cfg).idle_unload_seconds;
    if idle_seconds == 0 || !daemon.models_loaded.load(Ordering::SeqCst) {
        return None;
    }
    let elapsed = Instant::now().duration_since(*lock_ignoring_poison(&daemon.last_activity));
    let remaining = Duration::from_secs(idle_seconds as u64).saturating_sub(elapsed);
    Some(remaining.max(MIN_UNLOAD_TICK))
}
```

- [ ] **Step 4: Shrink the housekeeping wait**

In `spawn_housekeeping`'s loop, replace:

```rust
            let wait = match &normalize_cfg {
                None => SUBSCRIBER_REAP_INTERVAL,
                Some(_) if last_known_available => HEALTH_POLL_INTERVAL,
                Some(_) => backoff,
            };
```

with:

```rust
            let wait = match &normalize_cfg {
                None => SUBSCRIBER_REAP_INTERVAL,
                Some(_) if last_known_available => HEALTH_POLL_INTERVAL,
                Some(_) => backoff,
            };
            // Wake for whichever comes first: this loop's own business, or
            // the idle-unload deadline. Without this, a 60 s timeout would
            // fire whenever `HEALTH_POLL_INTERVAL` next happened to come
            // round.
            let wait = time_until_unload(&daemon).map_or(wait, |d| wait.min(d));
```

- [ ] **Step 5: Attempt the unload on every tick**

Immediately after the existing `reap_dead_subscribers(&daemon.subscribers);`
call and **before** the `let Some(cfg) = &normalize_cfg else { continue };`
line:

```rust
            // Cheap when there is nothing to do: `unload_models` re-checks
            // the deadline itself and returns false. Placed before the llama
            // supervision below so an unload and the gate that must then skip
            // supervision happen in the same tick, not one tick apart.
            unload_models(&daemon);
```

- [ ] **Step 6: Gate the supervisor**

Add the predicate next to `should_unload`, so the gate is a named, tested
thing rather than an untestable `if` buried in a loop:

```rust
/// Whether this housekeeping tick should touch `llama-server` at all.
///
/// Pure and unit-testable, for the same reason `should_unload` is. It folds
/// in the pre-existing `WARMING` check rather than sitting beside it: both
/// arms answer the same question -- "is there a `llama-server` of ours to
/// supervise right now" -- and splitting them across a named function and an
/// inline `if` would leave half the gate untested.
fn should_supervise_llama(state: u8, models_loaded: bool) -> bool {
    if state == WARMING {
        // `warm_up` hasn't settled `daemon.llama` yet -- acting now could
        // spawn a second llama-server racing warm_up's own.
        return false;
    }
    // Models are unloaded, or a load is still in flight. Either way there is
    // no child of ours to supervise, and respawning one here would undo every
    // unload seconds after it happened. `models_loaded` is set only *after* a
    // load installs the pipeline, so an in-flight `load_models` -- which owns
    // its own `spawn_and_wait_healthy` child -- is never raced, exactly as
    // `warm_up`'s initial spawn never was.
    models_loaded
}
```

Then in `spawn_housekeeping`'s loop, replace the existing `WARMING` check:

```rust
            if daemon.state.load(Ordering::SeqCst) == WARMING {
                // warm_up hasn't settled `daemon.llama` yet -- acting now
                // could spawn a second llama-server racing warm_up's own.
                continue;
            }
```

with:

```rust
            if !should_supervise_llama(
                daemon.state.load(Ordering::SeqCst),
                daemon.models_loaded.load(Ordering::SeqCst),
            ) {
                continue;
            }
```

`supervise_llama_once` itself is **not** modified: its existing unit tests and
its injected `respawn` seam stay exactly as they are.

- [ ] **Step 7: Run the full suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

Expected: **406 passed** (398 + 8 new), clippy clean.

- [ ] **Step 8: Verify on the real binary**

```bash
cargo build --release -p openwhisprflow --features custom-protocol
```

Then, with no other instance running, start it and watch memory:

```bash
./target/release/openwhisprflow &
sleep 5 && ./target/release/openwhisprflow --status
ps -o rss,comm -p $(pgrep -x openwhisprflow) ; pgrep -x llama-server || echo "no llama-server: correct"
```

Expected: `--status` reports `"warm": false`; RSS is a fraction of the ~777 MB
baseline; **no `llama-server` process exists**.

Do **not** exercise the load by recording from the microphone — CLAUDE.md
forbids it without explicit permission. Drive it with a socket call instead:

```bash
./target/release/openwhisprflow --toggle && sleep 2 && ./target/release/openwhisprflow --cancel
sleep 3 && ./target/release/openwhisprflow --status   # expect "warm": true, llama-server up
sleep 70 && ./target/release/openwhisprflow --status  # expect "warm": false again
pgrep -x llama-server || echo "unloaded and stayed unloaded: correct"
./target/release/openwhisprflow --quit
```

`--toggle` opens the microphone but `--cancel` discards the buffer without
transcribing it, so nothing is recorded or written anywhere. If the machine has
no microphone the toggle fails — that is fine, the background load still runs.
Record the observed numbers; they go into HANDOVER.md in Task 7.

- [ ] **Step 9: Commit**

```bash
git add crates/owf-core/src/server.rs
git commit -m "feat(server): unload idle models, and stop resurrecting llama

The housekeeping loop now wakes for the unload deadline as well as its
own business, so idle_unload_seconds means what it says. One gate on
models_loaded stops the supervisor spawning a fresh llama-server seconds
after every unload; supervise_llama_once itself is unchanged.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Documentation

The feature is only finished when the next person reading `CLAUDE.md` learns
the lock order before they break it.

**Files:**
- Modify: `CLAUDE.md` (Architecture section; new invariant 12)
- Modify: `README.md` (a memory-use section)
- Modify: `HANDOVER.md` (what was and was not verified)

**Interfaces:**
- Consumes: everything above.
- Produces: nothing code-facing.

- [ ] **Step 1: Add invariant 12 to CLAUDE.md**

Append to the "Invariants worth knowing before editing" list:

```markdown
12. **The models are not resident by default, and `load_lock` is always taken
    before `pipeline`.** `[models] preload_at_startup` defaults to `false`, so
    an idle daemon routinely has `pipeline: None` and no `llama-server` child
    at all — a state that used to be reachable only during warm-up and is now
    ordinary. Three consequences that are easy to break:
    - **Never lock `pipeline` to ask whether the models are loaded.**
      `process_utterance` holds that mutex for the whole `process` call, so a
      reader that locks it blocks for the length of a transcription. `Status`
      and the housekeeping thread read `daemon.models_loaded` instead — the
      same reason `normalize_available` exists.
    - **The `llama-server` supervisor must stay gated on `models_loaded`.**
      Without that gate it respawns a 697 MB child seconds after every unload,
      and the whole feature silently does nothing.
    - **A lazy load failure is retryable, not fatal.** `run_utterance`
      broadcasts `Error` and returns to `IDLE`; only the `preload_at_startup`
      path still latches `FAILED` with a stored `fatal_error`. On a fresh
      install this is what lets the Setup pane's download be followed by a
      press rather than a restart.
    `preload_at_startup = true` with `idle_unload_seconds = 0` reproduces the
    pre-2026-08-29 behaviour exactly, and is what to point a user at if the
    lazy path misbehaves. See
    `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.
```

- [ ] **Step 2: Update CLAUDE.md's Architecture section**

Replace the sentence:

```
`llama-server` is spawned once when the app starts and supervised (`spawn_housekeeping` /
`supervise_llama_once`: 10 s health poll, 1→30 s backoff restart, zombie reaping). A dead
`llama-server` degrades to `UnavailableNormalizer` rather than failing the app.
```

with:

```
`llama-server` is spawned by `load_models` — at startup only when `[models]
preload_at_startup` is on, otherwise on the first `ptt-start` — and supervised
while it lives (`spawn_housekeeping` / `supervise_llama_once`: 10 s health poll,
1→30 s backoff restart, zombie reaping). That supervision is gated on
`daemon.models_loaded`, so a child killed deliberately by `unload_models` stays
dead instead of being restarted (invariant 12). A dead `llama-server` degrades
to `UnavailableNormalizer` rather than failing the app.
```

- [ ] **Step 3: Add the config summary line to CLAUDE.md**

In the same Architecture section, after the `warm_up` discussion, add:

```
`[models]` owns the model lifetime: `preload_at_startup` (default `false`) and
`idle_unload_seconds` (default `60`, `0` = never). `ensure_models_loaded` reads
the config from disk at load time, so `[asr]`/`[normalize]` changes take effect
on the next dictation for a lazily-loaded daemon — `schema.ts`'s
`RESTART_SECTIONS` is unchanged because it is still correct whenever the models
happen to be resident.
```

- [ ] **Step 4: Add a README section**

Insert after the configuration section:

```markdown
## Speicherverbrauch

Standardmäßig werden die Modelle erst beim ersten Tastendruck geladen und eine
Minute nach dem letzten Diktat wieder entladen. Im Leerlauf belegt das Programm
damit einen Bruchteil dessen, was Spracherkennung und Sprachmodell zusammen
brauchen — auf dem Entwicklungsrechner rund 1,4 GB, die sonst dauerhaft belegt
blieben.

Der Preis: das erste Diktat nach dem Start (und nach jeder Ruhephase) wartet
einmalig darauf, dass die Modelle geladen sind. Die Aufnahme selbst beginnt
sofort — das Laden läuft parallel zum Sprechen —, aber bei einem sehr kurzen
Diktat kann der Einfügevorgang ein paar Sekunden später kommen.

Zwei Einstellungen unter **Erweitert → Modelle & Speicher** steuern das:

| Einstellung | Standard | Bedeutung |
|---|---|---|
| Modelle beim Start laden | aus | Lädt alles schon beim Programmstart. Erstes Diktat ohne Wartezeit, dafür ist der Speicher ab dem Anmelden belegt. |
| Modelle entladen nach | 60 s | Ruhezeit, nach der die Modelle freigegeben werden. `0` heißt: nie entladen. |

Wer das alte Verhalten will — alles beim Start laden, nie entladen — setzt die
erste Einstellung auf an und die zweite auf `0`.
```

- [ ] **Step 5: Append to HANDOVER.md**

```markdown
---

## Added after this letter: the models follow the dictation

`[models] preload_at_startup` (default `false`) and `idle_unload_seconds`
(default `60`) replace "every model resident for the life of the process".
Design: `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.
Invariant 12 in CLAUDE.md is the part to read before editing `server.rs`.

- `cargo test --workspace`: **406 passed, 0 failed, 4 ignored** (377 before,
  plus 29 new). `cargo clippy --workspace --all-targets`: clean. Frontend
  `bun run build`: clean.
- **Verified on this machine, without a microphone:** the app starts with
  `"warm": false` and no `llama-server` process; a `--toggle` followed by
  `--cancel` brings both up; `--status` reports `"warm": false` again after the
  timeout, and `llama-server` stays dead rather than being respawned by the
  supervisor. [Replace this sentence with the RSS numbers actually observed in
  Task 6 Step 8.]
- **Not verified:** the cold-start latency of a real dictation. Measuring it
  means speaking into the microphone, which CLAUDE.md forbids without explicit
  permission, so nobody knows yet how much of the load a normal utterance
  actually hides. The number that matters is how long `run_utterance` blocks in
  `ensure_models_loaded` after a short utterance; `--status`'s `last_ms` does
  not include it.
- **Judgement call:** a lazy load failure returns to `IDLE` instead of latching
  `FAILED`. The startup path still latches, because a failure discovered at
  startup is a different thing from one discovered on the user's third
  dictation. This makes a fresh install recover from the Setup pane without a
  restart, which the old behaviour did not.
```

- [ ] **Step 6: Final verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && bun run build`

Expected: 406 passed, clippy clean, frontend clean.

- [ ] **Step 7: Commit**

```bash
git add CLAUDE.md README.md HANDOVER.md
git commit -m "docs: record the lazy model lifecycle

Invariant 12 (lock order, the supervisor gate, retryable load failures),
the README's Speicherverbrauch section, and what HANDOVER can and cannot
claim was verified without a microphone.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```
