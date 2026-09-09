# Script Injection Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `ydotool` injection backend with a `script` backend that runs a user-supplied executable with the finished transcript as its single argument.

**Architecture:** `InjectBackend::Ydotool` becomes `InjectBackend::Script`, keeping `"ydotool"` as a read-only serde alias so pre-existing `config.toml` files still load (invariant 4). A new `[inject] script` key names the executable. `ScriptInjector` replaces `YdotoolInjector` and does nothing but run that executable with `argv[1] = text` — no clipboard staging, no environment of its own, no chord decision — so the script owns the whole injection. `paste_chord` and `terminal_classes` lose their only reader and become accepted-and-ignored keys on the `[normalize] port` precedent. GNOME stops recommending `ydotool` and recommends `clipboard`, with no prerequisites at all.

**Tech Stack:** Rust (`yappr-core`, `src-tauri`), TypeScript/React (`src/settings/`), `serde`/`toml`.

**Spec:** No separate design document — this was a bounded change agreed in chat on 2026-09-09. The design it implements is restated in full under "Design" below; that section is the spec this plan argues from.

## Design

The reference script this must work with, unmodified, is the user's `handy-paste.sh`. It reads **only `$1`**, and does everything else itself: saves the old clipboard with `wl-paste`, `wl-copy`s the new text, asks a GNOME extension for the focused window class over `gdbus`, picks Ctrl+V or Ctrl+Shift+V, presses it with `ydotool key`, then restores the old clipboard from a backgrounded `nohup … &` subshell. It also `unset`s `LD_LIBRARY_PATH`/`LD_PRELOAD` and defaults `YDOTOOL_SOCKET` on its own.

Three consequences follow, and they are the whole contract:

1. **`$1` is everything yappr passes.** No extra positional arguments, no `YAPPR_*` environment variables. A script that wants the window class asks for it the way `handy-paste.sh` does.
2. **yappr must not pre-copy to the clipboard.** The old `YdotoolInjector` ran `ClipboardInjector` first. `handy-paste.sh` saves the previous clipboard *before* it copies — a pre-copy would make it "restore" yappr's own transcript.
3. **The backgrounded restore holds the script's stdout/stderr pipes open after it exits.** This is exactly the case `procutil::run_with_timeout` was built for (invariant 6, `PIPE_DRAIN_GRACE`); it must be the only way this backend spawns anything, and a test must pin it.

## Global Constraints

- **Invariant 1 — once ASR has produced text, the user gets text.** Every script failure (missing path, non-executable, non-zero exit, timeout, empty `[inject] script`) must fall through `inject_with_recovery` to the clipboard fallback and then to `unsent.txt`. Never add a path that loses a transcript.
- **Invariant 4 — `#[serde(deny_unknown_fields)]` everywhere.** Deleting the `"ydotool"` *value* would make `server::start` quarantine the user's only settings file. Deleting the `paste_chord` / `terminal_classes` *keys* would do the same. Both stay readable; neither is written.
- **Invariant 6 — every subprocess call goes through `procutil::run_with_timeout`.** No bare `Command::output()`, no `Command::status()`.
- **Invariant 9 — `config.toml` is rendered whole from `Config` by `config::render`.** A field that must not appear in a written file is `#[serde(skip_serializing)]`; there is no template to edit.
- **`winclass.rs`, `gnome.rs` and `hypr.rs` are not touched.** They look like ydotool machinery, but `style::resolve` reads `winclass::active_window_class()` too, for per-window style rules. Only comments in them that *name* ydotool get reworded (Task 5).
- **The gate is** `cargo test --workspace && cargo clippy --workspace --all-targets`, run at the end of every task. `cargo test --workspace -- --ignored --test-threads=1` is not affected by this work (it needs downloaded models) and is run once, at the end of Task 5.
- **Exact spellings:** the config value is `script` (not `custom`, not `exec`); the config key is `[inject] script` (not `script_path`); `TextInjector::name()` returns `"script"`; the German GUI label is `Einfüge-Skript`.

---

### Task 1: `ScriptInjector`

Adds the injector and its tests. Nothing selects it yet — `InjectBackend::Ydotool` and `YdotoolInjector` are still in place after this task, so the workspace stays green and this task is reviewable on its own.

**Files:**
- Modify: `crates/yappr-core/src/inject.rs` (add `SCRIPT_TIMEOUT`, `InjectError::NotConfigured`, generalise `run_typer`, add `ScriptInjector`)
- Test: `crates/yappr-core/src/inject.rs` (`mod tests`, in-file — this crate tests injectors inline)

**Interfaces:**
- Consumes: `procutil::run_with_timeout(Command, Duration, Option<&[u8]>) -> io::Result<Output>`; `debug::expand_tilde(&str) -> PathBuf`; `config::InjectConfig`.
- Produces: `pub struct ScriptInjector` with `pub fn new(cfg: &InjectConfig) -> Self`, implementing `TextInjector` with `name() == "script"`. `InjectError::NotConfigured`. `fn run_backend(backend: &'static str, program: &std::ffi::OsStr, argv: Vec<String>, timeout: Duration) -> Result<(), InjectError>` — the renamed, generalised `run_typer`. Task 2 calls `ScriptInjector::new` from `inject::build`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/yappr-core/src/inject.rs`. Put these helpers at the top of the block, right after the existing `use` lines:

```rust
    /// Writes an executable shell script into a fresh temp directory and
    /// returns `(dir, script_path)`. The caller removes `dir`.
    ///
    /// No `tempfile` dependency: this crate has none, and the tests that
    /// need scratch space elsewhere build their paths the same way.
    fn script_fixture(tag: &str, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!("yappr-script-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paste.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, path)
    }

    /// An `InjectConfig` whose script backend points at `path`.
    fn script_cfg(path: &std::path::Path) -> InjectConfig {
        InjectConfig { script: path.display().to_string(), ..InjectConfig::default() }
    }
```

Then the tests:

```rust
    #[test]
    fn a_script_receives_the_transcript_as_its_only_argument() {
        // The whole contract with a user's paste script: `$1` is the text,
        // and there is no `$2`. `handy-paste.sh` -- the script this backend
        // was built against -- reads nothing else, so anything extra here
        // would be a promise no script has to keep.
        let (dir, path) = script_fixture(
            "argv",
            r#"printf '%s' "$#" > "$(dirname "$0")/count"
printf '%s' "$1" > "$(dirname "$0")/arg1""#,
        );

        ScriptInjector::new(&script_cfg(&path)).inject("hallo welt", Some("kitty")).unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("count")).unwrap(), "1");
        assert_eq!(std::fs::read_to_string(dir.join("arg1")).unwrap(), "hallo welt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_transcript_that_begins_with_a_dash_reaches_the_script_intact() {
        // Same hazard `wtype_argv`'s `--` guards against, answered
        // differently: there is no shell and no option parsing between here
        // and the script, so the text is passed as one argv element and
        // arrives whole. Pinned because "just run it through sh -c" is the
        // obvious wrong turn, and it would break on this input.
        let (dir, path) =
            script_fixture("dash", r#"printf '%s' "$1" > "$(dirname "$0")/arg1""#);

        ScriptInjector::new(&script_cfg(&path)).inject("-n --version", None).unwrap();

        assert_eq!(std::fs::read_to_string(dir.join("arg1")).unwrap(), "-n --version");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unconfigured_script_path_fails_without_spawning_anything() {
        // `[inject] script` defaults to empty, so this is the state of every
        // user who selects the backend before writing a script. It must be a
        // clean, named error -- which `inject_with_recovery` turns into the
        // clipboard fallback (invariant 1) -- not a spawn of "".
        let err = ScriptInjector::new(&InjectConfig::default()).inject("hallo", None).unwrap_err();
        assert!(
            matches!(err, InjectError::NotConfigured { .. }),
            "expected NotConfigured, got {err:?}"
        );
        assert!(err.to_string().contains("[inject] script"), "the error must name the setting: {err}");
    }

    #[test]
    fn a_script_that_exits_nonzero_is_reported_as_a_failure() {
        // The user's own error path: ydotoold not running, no /dev/uinput,
        // wl-copy missing. The backend must report it so the clipboard
        // fallback runs and the notification fires.
        let (dir, path) = script_fixture("fails", r#"echo "no uinput" >&2
exit 3"#);

        let err = ScriptInjector::new(&script_cfg(&path)).inject("hallo", None).unwrap_err();

        assert!(matches!(err, InjectError::Failed { .. }), "expected Failed, got {err:?}");
        assert!(err.to_string().contains("no uinput"), "stderr must survive into the error: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_script_file_is_a_spawn_error_not_a_panic() {
        let cfg = InjectConfig {
            script: "/nonexistent/yappr/paste.sh".to_string(),
            ..InjectConfig::default()
        };
        let err = ScriptInjector::new(&cfg).inject("hallo", None).unwrap_err();
        assert!(matches!(err, InjectError::Spawn { .. }), "expected Spawn, got {err:?}");
    }

    #[test]
    fn a_paste_script_that_backgrounds_its_clipboard_restore_does_not_block() {
        // Invariant 6, in the exact shape this backend meets it.
        // `handy-paste.sh` ends with `nohup bash -c '... | wl-copy' &`, so a
        // grandchild outlives the script holding its stdout and stderr open.
        // Reading those pipes to EOF would wedge the pipeline thread at
        // INJECTING until that grandchild died -- which is the bug
        // `PIPE_DRAIN_GRACE` exists for. 5 s is far under the grandchild's
        // 30 s, so a regression here fails loudly rather than slowly.
        let (dir, path) = script_fixture("grandchild", "sleep 30 &\nexit 0");

        let started = std::time::Instant::now();
        ScriptInjector::new(&script_cfg(&path)).inject("hallo", None).unwrap();

        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}; the grandchild's pipes blocked the drain",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_path_beginning_with_a_tilde_is_expanded() {
        // `~/bin/paste.sh` is what a user types into the settings field, and
        // `Command` does no expansion -- only a shell does, and there is no
        // shell here. Without this the field silently never works.
        let cfg = InjectConfig { script: "~/bin/paste.sh".to_string(), ..InjectConfig::default() };
        let err = ScriptInjector::new(&cfg).inject("hallo", None).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains('~'), "the tilde must be gone by the time we spawn: {msg}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core inject::tests:: 2>&1 | tail -30`
Expected: compile failure — `cannot find struct ScriptInjector`, `no variant NotConfigured`, `no field script on InjectConfig`.

The `script` field is Task 2's, but these tests need it now. Add just the field and its default to `crates/yappr-core/src/config.rs` as part of this task — the *semantic* config changes (the variant rename, the obsolete keys) stay in Task 2:

In `InjectConfig`, after `keystroke_delay_ms`:

```rust
    /// Path to the program `InjectBackend::Script` runs, tilde-expanded at
    /// injection time. Empty -- the default -- means no script is
    /// configured; the backend then fails immediately with
    /// [`InjectError::NotConfigured`] and the clipboard fallback carries the
    /// transcript (invariant 1).
    #[serde(default = "d_script")]
    pub script: String,
```

Beside `d_keydelay`:

```rust
fn d_script() -> String {
    String::new()
}
```

And in `impl Default for InjectConfig`, after `keystroke_delay_ms: d_keydelay(),`:

```rust
            script: d_script(),
```

Re-run: `cargo test -p yappr-core inject::tests:: 2>&1 | tail -30`
Expected: compile failure now only about `ScriptInjector` and `NotConfigured`.

- [ ] **Step 3: Add the error variant and generalise the runner**

In `crates/yappr-core/src/inject.rs`, add to `enum InjectError`, after the `Mock` variant:

```rust
    #[error("no paste script configured -- set [inject] script to an executable path")]
    NotConfigured,
```

Rename `run_typer` to `run_backend` and give it an explicit program, so a backend whose program is a user-chosen path can reuse it. Replace the whole `fn run_typer` signature and its first two lines:

```rust
/// Runs one argv-driven injection backend under I3's timeout and maps the
/// outcome onto [`InjectError`].
///
/// `backend` is the error label and `program` the thing actually spawned.
/// For `wtype` these are the same string; for the script backend `program`
/// is the user's path and `backend` stays the stable `"script"` that
/// `InjectOutcome`, the debug record and the settings GUI all name.
fn run_backend(
    backend: &'static str,
    program: &std::ffi::OsStr,
    argv: Vec<String>,
    timeout: Duration,
) -> Result<(), InjectError> {
    tracing::debug!(backend, ?program, ?argv, ?timeout, "spawning injection backend");
    let mut cmd = Command::new(program);
```

The rest of the function body is unchanged. Update `WtypeInjector::inject`:

```rust
    fn inject(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
        run_backend(
            "wtype",
            std::ffi::OsStr::new("wtype"),
            wtype_argv(text, self.delay_ms),
            WTYPE_TIMEOUT,
        )
    }
```

And `YdotoolInjector::inject`'s last line, which Task 2 deletes but which must compile now:

```rust
        run_backend("ydotool", std::ffi::OsStr::new("ydotool"), paste_key_argv(shift), PASTE_KEY_TIMEOUT)
```

- [ ] **Step 4: Add `ScriptInjector`**

In `crates/yappr-core/src/inject.rs`, directly after the `ClipboardInjector` impl block:

```rust
/// How long a user's paste script may take before I3's timeout claims it.
/// Generous on purpose: `handy-paste.sh`, the script this backend was built
/// against, waits on `wl-paste` to save the old clipboard, sleeps 200 ms for
/// the compositor to hand over the new offer, presses six key events at
/// 50 ms, and sleeps another 300 ms -- ~1,5 s all told. The bound exists so
/// a *hung* script cannot wedge the pipeline thread, not to police a slow
/// one.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);

/// Spec 10.3's second injector, since 2026-09-09 a **user-supplied script**
/// rather than `ydotool`.
///
/// It runs `[inject] script` with the finished transcript as its single
/// argument and does nothing else -- no `wl-copy` first, no environment of
/// its own, no paste chord. The script owns the entire injection: how the
/// text reaches the clipboard, which chord it presses, how it names the
/// focused window, whether it restores what was in the clipboard before.
///
/// Each of those omissions is deliberate:
///
///   - **No pre-copy.** The `ydotool` backend this replaced staged the text
///     with [`ClipboardInjector`] before pressing a chord. A script that
///     saves and restores the previous clipboard (as the reference one does)
///     would then "restore" yappr's own transcript over the user's.
///   - **No chord.** Choosing Ctrl+V vs Ctrl+Shift+V needed the focused
///     window's class, and every provider for it can answer `None` (see
///     [`crate::winclass`]) -- which produced a plain Ctrl+V that terminals
///     ignore, from a `ydotool` that exited 0, so nothing failed and nothing
///     was logged. A script asks its own desktop, its own way.
///   - **No `$2`, no `YAPPR_*` environment.** `$1` is the whole contract, so
///     a script written for another dictation tool works unchanged.
///
/// Failure of any kind -- unconfigured, missing, non-executable, non-zero
/// exit, timeout -- is an `Err`, which `inject_with_recovery` turns into the
/// clipboard fallback and its notification. Invariant 1 holds.
pub struct ScriptInjector {
    path: std::path::PathBuf,
}

impl ScriptInjector {
    pub fn new(cfg: &InjectConfig) -> Self {
        // `Command` performs no expansion -- only a shell does, and there is
        // no shell on this path -- so `~/bin/paste.sh`, which is exactly what
        // a user types into the settings field, has to be expanded here.
        Self { path: crate::debug::expand_tilde(&cfg.script) }
    }
}

impl TextInjector for ScriptInjector {
    fn name(&self) -> &'static str {
        "script"
    }

    fn inject(&self, text: &str, _target_class: Option<&str>) -> Result<(), InjectError> {
        if self.path.as_os_str().is_empty() {
            return Err(InjectError::NotConfigured);
        }
        run_backend("script", self.path.as_os_str(), vec![text.to_string()], SCRIPT_TIMEOUT)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p yappr-core inject:: 2>&1 | tail -30`
Expected: PASS, all `inject::tests::` including the seven new ones.

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS — nothing has been removed yet.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/yappr-core/src/inject.rs crates/yappr-core/src/config.rs
git commit -m "feat(inject): a script injector that runs a user program with the transcript as \$1"
```

---

### Task 2: Swap the backend

Makes `script` the selectable backend and deletes `ydotool` from the Rust core. After this task nothing in `yappr-core` presses a key or knows what a terminal is.

**Files:**
- Modify: `crates/yappr-core/src/config.rs` (variant rename + alias, obsolete keys, tests)
- Modify: `crates/yappr-core/src/inject.rs` (delete `YdotoolInjector`, `wants_shift`, `is_terminal_class`, `paste_key_argv`, `PASTE_KEY_TIMEOUT`, `PASTE_SETTLE` and their tests; rewire `build`)
- Modify: `crates/yappr-core/src/config_write.rs:177-180` (test names the old value)
- Modify: `crates/yappr-core/src/server.rs:3094` (test names the old value)
- Modify: `src-tauri/src/wizard.rs:146` (match arm — one line, so the crate compiles; its *semantics* are Task 3)

**Interfaces:**
- Consumes: `ScriptInjector::new(&InjectConfig)` from Task 1.
- Produces: `InjectBackend::Script`, serialized `"script"`, deserialized from `"script"` or `"ydotool"`. `InjectConfig::paste_chord` and `InjectConfig::terminal_classes` still exist and still deserialize, but are `#[serde(skip_serializing)]` and read by nothing. Task 4 hides both in the GUI.

- [ ] **Step 1: Write the failing tests**

In `crates/yappr-core/src/config.rs`'s `mod tests`, **replace** `inject_defaults_cover_common_terminals` (its terminal list is about to go) with:

```rust
    #[test]
    fn a_config_that_still_says_ydotool_loads_as_the_script_backend() {
        // The upgrade path, and the reason the alias exists at all.
        // `InjectConfig` is `deny_unknown_fields`, so an unrecognised value
        // fails the load -- and `server::start` answers a failed load by
        // renaming the user's only settings file to `config.toml.broken-*`
        // and writing a fresh default (invariant 4). Dropping the spelling
        // would cost every ydotool user every setting they have.
        let c = Config::from_str("[inject]\nbackend = \"ydotool\"\n").unwrap();
        assert_eq!(c.inject.backend, InjectBackend::Script);
    }

    #[test]
    fn the_script_backend_is_never_written_back_as_ydotool() {
        // The alias is read-only: the first save canonicalises the file, so
        // the retired word does not live on in configs forever.
        let c = Config::from_str("[inject]\nbackend = \"ydotool\"\n").unwrap();
        let rendered = render(&c);
        assert!(rendered.contains("backend = \"script\""), "got: {rendered}");
        assert!(!rendered.contains("ydotool"), "the retired spelling must not be written: {rendered}");
    }

    #[test]
    fn the_retired_inject_keys_still_load_but_are_never_written() {
        // Same treatment as `[normalize] port` / `llama_server_path`, for the
        // same reason: `paste_chord` and `terminal_classes` had exactly one
        // reader (the ydotool backend's chord decision) and now have none,
        // but `deny_unknown_fields` makes deleting them a hard load failure
        // for every user who has them -- which is all of them, since
        // `config::render` wrote them.
        let c = Config::from_str(
            "[inject]\npaste_chord = \"ctrl_shift_v\"\nterminal_classes = [\"kitty\"]\n",
        )
        .expect("a pre-existing file naming the retired keys must still load");
        let rendered = render(&c);
        assert!(!rendered.contains("paste_chord"), "got: {rendered}");
        assert!(!rendered.contains("terminal_classes"), "got: {rendered}");
    }

    #[test]
    fn a_rendered_config_round_trips_through_the_loader() {
        // `render` drops the two retired keys, so the file it writes must
        // still be one `Config::load_from` accepts -- otherwise the first
        // autosave would produce a file the next start quarantines.
        let c = Config::from_str("[inject]\nbackend = \"ydotool\"\n").unwrap();
        let reloaded = Config::from_str(&render(&c)).expect("a rendered config must reload");
        assert_eq!(reloaded.inject.backend, InjectBackend::Script);
    }

    #[test]
    fn the_default_script_path_is_empty() {
        assert_eq!(Config::from_str("").unwrap().inject.script, "");
    }
```

In the same file, update `every_inject_backend_is_spelled_the_way_config_toml_spells_it` — replace its comment and table:

```rust
    #[test]
    fn every_inject_backend_is_spelled_the_way_config_toml_spells_it() {
        // `ENUMS["inject.backend"]` and `HELP["inject.backend"]` in
        // `src/settings/schema.ts` both hand-repeat these strings; this pins
        // what they have to agree with. `deny_unknown_fields` makes a
        // misspelling a hard startup failure, not a silent fallback.
        // `"ydotool"` is deliberately absent: it is an alias, covered by
        // `a_config_that_still_says_ydotool_loads_as_the_script_backend`,
        // and it must never reach the GUI's dropdown as a choice.
        for (spelling, expected) in [
            ("wtype", InjectBackend::Wtype),
            ("script", InjectBackend::Script),
            ("clipboard", InjectBackend::Clipboard),
        ] {
            let c = Config::from_str(&format!("[inject]\nbackend = \"{spelling}\"\n")).unwrap();
            assert_eq!(c.inject.backend, expected, "for {spelling}");
        }
    }
```

In `crates/yappr-core/src/inject.rs`'s `mod tests`, update `build_selects_the_configured_backend`:

```rust
    #[test]
    fn build_selects_the_configured_backend() {
        let mut cfg = InjectConfig::default();
        assert_eq!(build(&cfg).name(), "wtype");
        cfg.backend = InjectBackend::Script;
        assert_eq!(build(&cfg).name(), "script");
        cfg.backend = InjectBackend::Clipboard;
        assert_eq!(build(&cfg).name(), "clipboard");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr-core config::tests:: 2>&1 | tail -20`
Expected: compile failure — `no variant or associated item named Script found for enum InjectBackend`.

- [ ] **Step 3: Rename the variant and retire the two keys**

In `crates/yappr-core/src/config.rs`, replace the `Ydotool` variant and its doc comment (currently lines 372-385) with:

```rust
    /// Spec 10.3's second injector: a **user-supplied script**, run with the
    /// finished transcript as its single argument. See
    /// [`crate::inject::ScriptInjector`] for the contract and why it is that
    /// narrow.
    ///
    /// This was `ydotool` until 2026-09-09, and pasted by staging the text
    /// with `wl-copy` and pressing one Ctrl+V (Ctrl+Shift+V for terminals)
    /// by raw keycode. The chord decision is what killed it: it needed the
    /// focused window's class, no provider can always give one (see
    /// [`crate::winclass`]), and an unknown class meant a plain Ctrl+V that
    /// every terminal ignores -- from a `ydotool` that exited 0, so nothing
    /// failed, no fallback notification fired and the user simply got no
    /// text. A script asks its own desktop its own way, and yappr stops
    /// pretending it can answer for every compositor.
    ///
    /// `#[serde(alias = "ydotool")]` is load-bearing, not a courtesy.
    /// `InjectConfig` is `deny_unknown_fields` (invariant 4), so an
    /// unrecognised *value* fails the load -- and `server::start` answers a
    /// failed load by quarantining the file: every ydotool user would lose
    /// every setting they had, over a word this app wrote into their config
    /// itself. The alias is read and never written (the enum serializes as
    /// `"script"`), so the first save canonicalises the file and the
    /// retired spelling dies out on its own.
    #[serde(alias = "ydotool")]
    Script,
```

Retire `PasteChord`'s doc comment — replace it (currently lines 389-414) with:

```rust
/// **Accepted and ignored since 2026-09-09.** Which chord the retired
/// `ydotool` backend pressed to paste.
///
/// The script backend that replaced it presses its own chord, so nothing
/// reads this. It survives as a type only because
/// [`InjectConfig::paste_chord`] must keep deserializing -- see that field.
```

Change the two `InjectConfig` fields (currently lines 436-445) to:

```rust
    /// **Accepted and ignored since 2026-09-09.** Window classes whose paste
    /// chord was Ctrl+Shift+V rather than Ctrl+V, back when this app chose
    /// the chord. The script backend chooses its own.
    ///
    /// Kept for the same reason as `[normalize] port`: this section is
    /// `deny_unknown_fields` (invariant 4) and `config::render` wrote this
    /// key into every user's file, so removing the field would turn every
    /// pre-existing `config.toml` into a quarantined one. `skip_serializing`
    /// keeps it out of newly written files; `schema.ts`'s `OBSOLETE_FIELDS`
    /// keeps it out of the GUI. It is not a setting.
    #[serde(default = "d_terminal_classes", skip_serializing)]
    pub terminal_classes: Vec<String>,
    /// **Accepted and ignored since 2026-09-09.** See
    /// [`InjectConfig::terminal_classes`] -- same retirement, same reason.
    #[serde(default = "d_paste_chord", skip_serializing)]
    pub paste_chord: PasteChord,
```

Replace the whole `fn d_terminal_classes()` body (the ~25-entry list, currently lines 455-491) with:

```rust
/// Empty since 2026-09-09: [`InjectConfig::terminal_classes`] has no reader
/// left, so a shipped list of terminals would be a default nothing consults.
/// The function survives only as the `#[serde(default)]` for a key old files
/// still carry.
fn d_terminal_classes() -> Vec<String> {
    Vec::new()
}
```

- [ ] **Step 4: Delete the ydotool injector**

In `crates/yappr-core/src/inject.rs`, delete outright:

- `const PASTE_KEY_TIMEOUT` and `const PASTE_SETTLE` (and their doc comments)
- `fn paste_key_argv`
- `fn is_terminal_class`
- `fn wants_shift`
- `pub struct YdotoolInjector`, its `impl YdotoolInjector` and its `impl TextInjector`
- these tests: `the_paste_chord_for_a_normal_window_is_ctrl_v_in_nested_order`, `the_paste_chord_for_a_terminal_adds_shift_in_nested_order`, `an_unknown_window_class_falls_back_to_plain_ctrl_v_under_auto`, `a_forced_ctrl_shift_v_pastes_into_a_terminal_whose_class_is_unknown`, `a_forced_ctrl_v_never_adds_shift_even_for_a_known_terminal`, `auto_still_reads_the_class_when_one_is_available`, `terminal_detection_matches_the_configured_classes_case_insensitively`

Trim the now-stale `use`: `use crate::config::{InjectBackend, InjectConfig, PasteChord};` becomes

```rust
use crate::config::{InjectBackend, InjectConfig};
```

Rewire `build`:

```rust
pub fn build(cfg: &InjectConfig) -> Box<dyn TextInjector> {
    match cfg.backend {
        InjectBackend::Wtype => Box::new(WtypeInjector::new(cfg.keystroke_delay_ms)),
        InjectBackend::Script => Box::new(ScriptInjector::new(cfg)),
        InjectBackend::Clipboard => Box::new(ClipboardInjector),
    }
}
```

Update `TextInjector::inject`'s doc comment — the sentence "Only the ydotool backend reads it" is now false for every backend:

```rust
    /// Injects `text` into the window that was focused when the recording
    /// started. `target_class` is that window's class as captured at
    /// `ptt-start` (`daemon.window_class`, the same value the style rules
    /// match on), or `None` when no provider could name it. No shipped
    /// injector reads it any more -- the ydotool backend that did was
    /// retired on 2026-09-09 -- but it describes the *target*, `MockInjector`
    /// records it, and the debug record writes it, so it still travels with
    /// every injection.
```

And in `run_backend`, the stdout/stderr note now names the script backend:

```rust
    if !stdout.is_empty() || !stderr.is_empty() {
        // A user's paste script is the likely source here, and its output is
        // the only account of what it did -- surfaced at info rather than
        // debug for that reason, even on the success path.
        tracing::info!(backend, stdout = %stdout, stderr = %stderr,
            "injection backend succeeded but printed output");
    }
```

Finally, the `MockInjector::failing_named` doc comment references "the ydotool environment report". Reword its last sentence:

```rust
    /// A failing mock that reports `name` -- for tests that need a failure
    /// attributed to a specific backend rather than the generic `"mock"`.
```

- [ ] **Step 5: Repair the three tests that name the old value**

`crates/yappr-core/src/config_write.rs:177-180`:

```rust
        save_config(&path, &json!({"inject": {"backend": "script"}})).unwrap();

        let after = Config::load_from(&path).unwrap();
        assert_eq!(after.inject.backend, crate::config::InjectBackend::Script);
```

`crates/yappr-core/src/server.rs:3094`:

```rust
            "[inject]\nbackend = \"script\"\n",
```

`src-tauri/src/wizard.rs:146` — the match arm, so `src-tauri` compiles. Task 3 revisits this function's *behaviour*:

```rust
                yappr_core::config::InjectBackend::Script => "script",
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS. If `src-tauri` fails to build on `gtk-layer-shell-0`, install it (`sudo pacman -S gtk-layer-shell`) — see CLAUDE.md's gotchas.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -20`
Expected: no warnings. A `PasteChord` that is now constructed only by its `#[serde(default)]` may draw `dead_code` — it is `pub` and reachable through `InjectConfig`, so it should not; if it does, the fix is a comment explaining the retirement, not an `#[allow]`.

- [ ] **Step 7: Commit**

```bash
git add crates/yappr-core/src/config.rs crates/yappr-core/src/inject.rs \
        crates/yappr-core/src/config_write.rs crates/yappr-core/src/server.rs \
        src-tauri/src/wizard.rs
git commit -m "feat(inject)!: replace the ydotool backend with a user script

\"ydotool\" stays a read-only serde alias for \"script\" so a pre-existing
config.toml still loads; paste_chord and terminal_classes join
[normalize] port as accepted-and-ignored keys."
```

---

### Task 3: GNOME stops needing a daemon

`ydotool` and `ydotoold` leave the prerequisite check and the wizard's recommendation. GNOME's answer is `clipboard`: it works, needs nothing installed, and the user pastes.

**Files:**
- Modify: `src-tauri/src/setup.rs:227-277` (`prerequisites_for`) and its test at `:465-478`
- Modify: `src-tauri/src/wizard.rs:65-92` (`recommended_backend`, `backend_prereqs`) and its tests at `:275-310`

**Interfaces:**
- Consumes: `InjectBackend::Script` from Task 2.
- Produces: `recommended_backend(&Desktop::Gnome) == "clipboard"`, `backend_prereqs(&Desktop::Gnome) == Vec::<&str>::new()`. Task 4's `wizard.tsx` renders both.

- [ ] **Step 1: Write the failing tests**

In `src-tauri/src/wizard.rs`'s `mod tests`, **replace** `gnome_is_the_only_desktop_that_recommends_ydotool` with:

```rust
    /// GNOME recommends the one backend that needs nothing installed.
    ///
    /// Mutter implements neither the virtual-keyboard protocol `wtype` types
    /// through nor anything else yappr can drive itself, and the `ydotool`
    /// recommendation that used to fill that gap went with the backend on
    /// 2026-09-09. `clipboard` is honest: the transcript lands in the
    /// clipboard and the user presses Ctrl+V. Anyone who wants that
    /// automated writes a script and points `[inject] script` at it -- which
    /// no prerequisite check can verify, because the script does not exist
    /// until they write it.
    #[test]
    fn gnome_recommends_the_backend_that_needs_no_setup() {
        assert_eq!(recommended_backend(&Desktop::Gnome), "clipboard");
        assert!(backend_prereqs(&Desktop::Gnome).is_empty());
    }

    #[test]
    fn no_desktop_asks_the_user_to_install_anything_for_its_backend() {
        // `backend_prereqs` exists to name what a `pacman -S` line cannot
        // finish -- it was `ydotoold` having to be *running*. With that
        // backend gone nothing qualifies, and the wizard's card must not
        // reappear for some other desktop by accident.
        for d in [
                Desktop::Gnome,
                Desktop::Hyprland,
                Desktop::Other("sway".to_string()),
                Desktop::Unknown,
            ] {
            assert!(backend_prereqs(&d).is_empty(), "{d:?} still lists prerequisites");
        }
    }
```

Update the wizard-state test at `:298-307` — replace its `"ydotool"` expectations:

```rust
        let broken = build_wizard_state(true, no_model(), &Desktop::Gnome, "clipboard");
```

and delete the `assert_eq!(broken["backend_prereqs"][0], "ydotool");` line, replacing it with:

```rust
        assert!(
            broken["backend_prereqs"].as_array().unwrap().is_empty(),
            "no desktop has backend prerequisites since ydotool was retired"
        );
```

Update `backend_patch`'s test at `:344`:

```rust
        assert_eq!(backend_patch("script"), serde_json::json!({ "inject": { "backend": "script" } }));
```

In `src-tauri/src/setup.rs`'s `mod tests`, **replace** the test at `:470-478` (the one asserting `c.bin == "ydotool" && !c.fatal`) with:

```rust
    /// Nothing yappr can install makes injection work on GNOME, so nothing
    /// is listed for it.
    ///
    /// `ydotool` was here as an optional entry until 2026-09-09 -- optional
    /// because making it fatal would have made `setup_status` report every
    /// GNOME install as not ready forever (`ydotoold` also has to be
    /// *running*, which no package check can see). With the backend retired
    /// the entry has no meaning at all: a user's own paste script is not a
    /// package, and its absence is not a gap yappr can name.
    #[test]
    fn no_desktop_checks_for_ydotool_any_more() {
        for d in [
                Desktop::Gnome,
                Desktop::Hyprland,
                Desktop::Other("sway".to_string()),
                Desktop::Unknown,
            ] {
            let checks = prerequisites_for(&d);
            assert!(
                !checks.iter().any(|c| c.bin == "ydotool"),
                "{d:?} still checks for ydotool"
            );
        }
    }

    /// `wl-copy` is fatal on every desktop, and on GNOME it is now the whole
    /// of injection rather than a fallback.
    #[test]
    fn wl_copy_is_required_everywhere() {
        for d in [
                Desktop::Gnome,
                Desktop::Hyprland,
                Desktop::Other("sway".to_string()),
                Desktop::Unknown,
            ] {
            let checks = prerequisites_for(&d);
            assert!(
                checks.iter().any(|c| c.bin == "wl-copy" && c.fatal),
                "{d:?} does not require wl-copy"
            );
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p yappr wizard::tests:: setup::tests:: 2>&1 | tail -30`
Expected: FAIL — `assertion \`left == right\` failed: left: "ydotool", right: "clipboard"`, and `Gnome still checks for ydotool`.

- [ ] **Step 3: Implement**

`src-tauri/src/wizard.rs` — replace `recommended_backend` and `backend_prereqs` (lines 65-92) with:

```rust
/// The injection backend that works on `d` out of the box.
///
/// GNOME is the exception and Mutter is the reason: it does not implement
/// the virtual-keyboard protocol `wtype` types through, so `wtype` silently
/// does nothing there. Everywhere else `wtype` is right *and* free -- no
/// daemon, no `/dev/uinput`.
///
/// GNOME's answer was `ydotool` until 2026-09-09, at the cost of a package,
/// a systemd unit and write access to `/dev/uinput`. With that backend
/// retired the honest answer is `clipboard`: the transcript lands in the
/// clipboard and the user pastes it. Automating that is `[inject] script`
/// and a script of their own -- which is a paragraph in the wizard, not a
/// recommendation, because a default cannot point at a file that does not
/// exist yet.
pub(crate) fn recommended_backend(d: &Desktop) -> &'static str {
    match d {
        Desktop::Gnome => "clipboard",
        _ => "wtype",
    }
}

/// What else has to be true for [`recommended_backend`] to actually work.
///
/// Empty on every desktop since 2026-09-09, and kept rather than deleted
/// because the *shape* of the question is still right. It existed for one
/// thing no package check could express -- `ydotoold` having to be running,
/// on top of `pacman -S ydotool` -- and both recommendations left now need
/// nothing beyond the fatal prerequisites `setup.rs` already reports. The
/// wizard renders its card only when this is non-empty, so an empty list
/// means the card is simply absent.
pub(crate) fn backend_prereqs(_d: &Desktop) -> Vec<&'static str> {
    Vec::new()
}
```

`src-tauri/src/setup.rs` — replace the `ydotool` push in `prerequisites_for` (lines 261-268) with nothing: delete the whole `checks.push(Prerequisite { bin: "ydotool", … });` block. Then replace the function's doc comment (lines 222-243) with:

```rust
/// Which programs are worth checking on `d`, and which of them are fatal.
///
/// Desktop-dependent for exactly one reason: **`wtype` cannot work on
/// GNOME.** It types through the virtual-keyboard protocol Mutter does not
/// implement, so there it is not missing, it is inapplicable. Reporting it
/// anyway cost a GNOME user a `sudo pacman -S wtype` that would still type
/// nothing, and -- because a *fatal* gap makes `setup_status` report the
/// whole install as not ready -- a wizard that reopened on every launch,
/// forever. That gap is a standing "Einrichtung unvollständig" banner rather
/// than a reopening wizard since 2026-09-09 (invariant 14); a permanent
/// banner nobody can act on is no better.
///
/// `ydotool` was on this list, optional, until 2026-09-09. It is not
/// replaced by a check for the script backend's program: `[inject] script`
/// names a file the user writes themselves, and reporting its absence as a
/// missing prerequisite would put a permanent gap on every install that has
/// not opted into a backend almost nobody uses -- exactly the never-ready
/// loop above, wearing different clothes. `ScriptInjector` reports a missing
/// script when it is actually asked to run one, and the clipboard fallback
/// carries the transcript meanwhile (invariant 1).
///
/// `wl-copy` is fatal everywhere, and on GNOME it now carries the whole of
/// injection rather than only the fallback.
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p yappr wizard:: setup:: 2>&1 | tail -20`
Expected: PASS.

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -20`
Expected: no warnings. `backend_prereqs` now ignores its parameter — the `_d` prefix is deliberate; keep the parameter so the wizard's call site and the shape of the question survive.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/setup.rs src-tauri/src/wizard.rs
git commit -m "feat(setup): GNOME recommends clipboard and needs nothing installed"
```

---

### Task 4: The settings window and the wizard

**Files:**
- Modify: `src/settings/schema.ts` (`LABELS:103-107`, `HELP:187-196`, `ENUMS:246-247`, `OBSOLETE_FIELDS:321-324`, `FIELD_ORDER:332`)
- Modify: `src/settings/wizard.tsx:527-547` (the backend note and the prereq card)

**Interfaces:**
- Consumes: `recommended_backend`/`backend_prereqs` from Task 3, `InjectBackend::Script` from Task 2. The `config` object the settings window receives now carries `inject.script` (a string) and still carries `inject.paste_chord`/`inject.terminal_classes` on the wire — `GetConfig` sends `Config` as JSON, and `skip_serializing` only affects TOML rendering, not `serde_json`. Hiding them is `OBSOLETE_FIELDS`' job, which is exactly why it exists.
- Produces: nothing another task consumes.

- [ ] **Step 1: Update `schema.ts`**

`LABELS` — replace lines 103-107 with:

```ts
  "inject.backend": "Verfahren",
  "inject.script": "Einfüge-Skript",
  "inject.trailing_space": "Leerzeichen anhängen",
  "inject.keystroke_delay_ms": "Tastenverzögerung",
```

(`inject.terminal_classes` and `inject.paste_chord` lose their labels — a hidden row needs none.)

`HELP` — replace the `inject.backend` entry and delete the `inject.paste_chord` and `inject.terminal_classes` entries:

```ts
  "inject.backend":
    "wtype tippt den Text Zeichen für Zeichen ins Fenster und braucht keine Einrichtung — unter GNOME (Mutter) bewirkt es allerdings nichts. script übergibt den fertigen Text als erstes Argument an ein eigenes Programm, das dann selbst entscheidet, wie es ihn einfügt. clipboard legt ihn nur in die Zwischenablage, einfügen musst du selbst.",
  "inject.script":
    "Pfad zu dem Programm, das das script-Verfahren aufruft. Es bekommt den fertigen Text als erstes und einziges Argument ($1) und ist danach für alles zuständig: Zwischenablage, Tastenkombination, Fenstererkennung. Muss ausführbar sein; ~ wird aufgelöst. Beispiel: ~/bin/paste.sh. Schlägt es fehl oder ist hier nichts eingetragen, landet der Text wie beim clipboard-Verfahren in der Zwischenablage.",
```

`ENUMS` — replace lines 246-247 with:

```ts
  "inject.backend": ["wtype", "script", "clipboard"],
```

(The `inject.paste_chord` entry goes; the key is hidden, and an enum for a hidden row is a third copy of a retired list.)

`OBSOLETE_FIELDS` — replace lines 321-324:

```ts
export const OBSOLETE_FIELDS = new Set([
  "normalize.port",
  "normalize.llama_server_path",
  // Retired with the ydotool backend on 2026-09-09. Rust still accepts both
  // keys so a pre-existing config.toml loads (invariant 4) and still sends
  // them over the wire (`skip_serializing` is TOML-only), but nothing reads
  // them: a script picks its own paste chord. They are not settings.
  "inject.paste_chord",
  "inject.terminal_classes",
]);
```

`FIELD_ORDER` — line 332:

```ts
  inject: ["backend", "script", "trailing_space", "keystroke_delay_ms"],
```

- [ ] **Step 2: Update the wizard's GNOME step**

In `src/settings/wizard.tsx`, replace the backend note and the prereq card (lines 527-547) with:

```tsx
              <p className="note">
                Texteingabe: <code>{state.recommended_backend}</code>
                {state.recommended_backend === "clipboard"
                  ? " — GNOME (Mutter) unterstützt das Protokoll nicht, über das wtype tippt. yappr legt den Text deshalb in die Zwischenablage; einfügen musst du selbst mit Strg+V."
                  : " — braucht keine weitere Einrichtung."}
              </p>

              {state.recommended_backend === "clipboard" && (
                <div className="card">
                  <p className="setup-command">
                    Automatisch einfügen geht trotzdem, aber nur mit einem eigenen Skript:
                    unter Allgemein → Texteingabe das Verfahren auf <code>script</code> stellen
                    und bei <em>Einfüge-Skript</em> den Pfad eintragen. yappr ruft es mit dem
                    fertigen Text als erstem Argument auf (<code>$1</code>); alles Weitere —
                    Zwischenablage, Tastenkombination, Fenstererkennung — macht das Skript
                    selbst.
                  </p>
                </div>
              )}
```

The `state.backend_prereqs.length > 0` block is deleted with it. `backend_prereqs` stays on the wire (Task 3 keeps the field), so `WizardState`'s TS type needs no change — but if the field is now unused in this file, leave the type declaration alone and do not remove it: `build_wizard_state` still sends it.

- [ ] **Step 3: Build the frontend to verify it compiles**

Run: `bun run build 2>&1 | tail -20`
Expected: `tsc` clean, `vite build` writes both `dist/index.html` and `dist/settings.html`.

If `tsc` reports `'backend_prereqs' is declared but its value is never read`, that is a lint on the destructure, not the type — leave the interface field and remove only an unused local binding.

- [ ] **Step 4: Verify the drift test still passes**

`config::tests::every_inject_backend_is_spelled_the_way_config_toml_spells_it` pins the three strings `ENUMS["inject.backend"]` repeats. Confirm they agree:

Run: `cargo test -p yappr-core every_inject_backend_is_spelled 2>&1 | tail -10`
Expected: PASS.

Run: `grep -n '"inject.backend"' src/settings/schema.ts`
Expected: the `ENUMS` line lists exactly `wtype`, `script`, `clipboard` — no `ydotool`.

- [ ] **Step 5: Commit**

```bash
git add src/settings/schema.ts src/settings/wizard.tsx
git commit -m "feat(settings): show the script backend, hide the retired ydotool keys"
```

---

### Task 5: The probe, the comments and the docs

The last places that still say `ydotool`, plus the example that pressed keys.

**Files:**
- Delete: `crates/yappr-core/examples/paste_probe.rs`
- Create: `crates/yappr-core/examples/script_probe.rs`
- Modify: `crates/yappr-core/src/debug.rs:192-207` (the `window_class` doc) and `:370` (a test's backend string)
- Modify: `crates/yappr-core/src/gnome.rs:32,119,184` and `crates/yappr-core/src/hypr.rs:289,302` (comments naming ydotool)
- Modify: `src-tauri/src/lib.rs:129,171,750` (comments naming ydotool)
- Modify: `README.md`, `CLAUDE.md`, `docs/HANDOVER.md`

**Interfaces:**
- Consumes: everything from Tasks 1-4.
- Produces: nothing.

- [ ] **Step 1: Replace the example**

```bash
git rm crates/yappr-core/examples/paste_probe.rs
```

Create `crates/yappr-core/examples/script_probe.rs`:

```rust
//! Runs the configured paste script for one line of text, printing what it
//! was given and what it did -- without a microphone, an ASR model, or a
//! running daemon.
//!
//! This replaced `paste_probe` on 2026-09-09, and answers a narrower
//! question. The old probe existed because yappr chose the paste chord
//! itself and got it wrong silently; it printed the window class and the
//! chord. The script chooses now, so what is worth showing is the contract:
//! which program ran, with which argument, and what it exited with. The
//! window class is printed too, but only as a courtesy -- nothing on this
//! path reads it.
//!
//! **It runs your real paste script, which presses real keys into whatever
//! window is focused** and will very likely replace your clipboard. Focus a
//! scratch window first.
//!
//! ```bash
//! cargo run -p yappr-core --example script_probe -- "hallo"
//! cargo run -p yappr-core --example script_probe -- "hallo" ~/bin/paste.sh  # override the path
//! ```
use yappr_core::config::{Config, InjectBackend};
use yappr_core::{inject, winclass};

/// Poll until the focus tracker has an answer, or give up.
fn wait_for_class() -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(class) = winclass::active_window_class() {
            return Some(class);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn main() {
    let mut cfg = Config::load().unwrap_or_default();
    cfg.inject.backend = InjectBackend::Script;
    if let Some(path) = std::env::args().nth(2) {
        cfg.inject.script = path;
    }
    if cfg.inject.script.is_empty() {
        eprintln!(
            "no script configured: set [inject] script in config.toml, \
             or pass a path as the second argument"
        );
        std::process::exit(2);
    }
    // The GNOME provider answers from a thread that tracks focus, so a
    // short-lived process has to let it connect and sweep first. The daemon
    // has been tracking since startup and never waits like this.
    winclass::start_tracking();
    let class = wait_for_class();
    let text = std::env::args().nth(1).unwrap_or_else(|| "script_probe".to_string());
    println!("active_window_class() = {class:?}   (informational -- the script is not told)");
    println!("script                = {}", cfg.inject.script);
    println!("argv[1]               = {text:?}");
    match inject::build(&cfg.inject).inject(&text, class.as_deref()) {
        Ok(()) => println!("inject()              = Ok"),
        Err(e) => println!("inject()              = Err: {e}"),
    }
}
```

- [ ] **Step 2: Reword the comments that name ydotool**

`crates/yappr-core/src/debug.rs` — the `window_class` doc (lines 192-207). Its "why this field exists" story is historical now, and saying so is the point:

```rust
    /// The target window's class as captured at recording start, or `null`
    /// when no provider could name it. `#[serde(default)]` so records
    /// written before this field existed still deserialize.
    ///
    /// Added because its absence made a real bug undiagnosable: the retired
    /// ydotool backend picked Ctrl+V vs Ctrl+Shift+V from this value, and
    /// with it missing there was no way to tell, after the fact, whether a
    /// dictation that produced no text in a terminal had been sent the wrong
    /// chord. No injector reads it since 2026-09-09 -- a paste script asks
    /// its own desktop -- but the per-window style rules still resolve
    /// against it, so it stays a real diagnostic.
    ///
    /// Deliberately *not* `skip_serializing_if`, unlike the two fields
    /// below: an omitted key would make "the class was unknown" byte-
    /// identical to a record written before the field existed, which is
    /// precisely the distinction it was added to draw. A written `null` is
    /// the diagnosis.
```

Line 370, in `an_unknown_window_class_is_written_as_null_not_omitted`, and its comment's first sentence:

```rust
        // The whole diagnostic value of this field is telling "no provider
        // could name the focused window" apart from "this record predates
        // the field". Skipping the key when it is `None` would make those
        // two byte-identical, which is exactly the question the field was
        // added to answer.
        let rec = InjectDebug {
            backend: "script".to_string(),
```

`crates/yappr-core/src/gnome.rs` — three sites:
- line 32, `ydotool exits 0 whichever chord it pressed.` → `a paste script exits 0 whichever chord it pressed.`
- line 119, `the ydotool paste chord and style rules will fall back to their defaults` → `per-application style rules will fall back to their defaults`
- line 184, `` `[inject] terminal_classes` is matched against `` → `` a paste script's own terminal list is matched against ``

`crates/yappr-core/src/hypr.rs` — lines 289 and 302, both `style rules and the ydotool paste chord will fall back to their defaults` → `per-application style rules will fall back to their defaults`.

`src-tauri/src/lib.rs` — three sites:
- line 129, `difference between a working ydotool and a wtype that types` → `difference between a working injector and a wtype that types`
- lines 171 and 750, `injector (ydotool via uinput, and wtype alike)` / `injector (ydotool via uinput, wtype alike)` → `injector (wtype, and any paste script that presses keys)`

Verify none are left:

Run: `grep -rn "ydotool" crates/yappr-core/src crates/yappr-core/examples src-tauri/src src/`
Expected: exactly two hits, both in `crates/yappr-core/src/config.rs` — the `#[serde(alias = "ydotool")]` and the doc comment explaining it — plus any lines in `config.rs`'s retirement comments that name the retired backend on purpose. Nothing in `inject.rs`, `gnome.rs`, `hypr.rs`, `lib.rs`, `debug.rs` or `src/`.

- [ ] **Step 3: Update the docs**

`README.md` — find every `ydotool` mention with `grep -n ydotool README.md` and rewrite that section around the script backend. It must state, in the user's voice:

- `[inject] backend = "script"` plus `[inject] script = "/path/to/paste.sh"`.
- The script is called with the finished transcript as `"$1"` and nothing else. No `$2`, no environment variables.
- It is responsible for the clipboard, the paste chord and any window detection.
- A non-zero exit, a timeout (15 s) or an empty `[inject] script` falls back to copying to the clipboard, with a desktop notification.
- The path is tilde-expanded and must be executable (`chmod +x`).
- An existing `backend = "ydotool"` keeps working: it is read as `script`, and the next settings save rewrites it. Anyone upgrading points `script` at a wrapper that does what the ydotool backend used to.

Add a minimal working example so the smallest useful script is on the page:

````markdown
```bash
#!/bin/sh
# Minimal: copy and paste with Ctrl+Shift+V.
wl-copy -- "$1"
sleep 0.2
ydotool key 29:1 42:1 47:1 47:0 42:0 29:0
```
````

`CLAUDE.md` — three places:
1. The "What this is" paragraph, which describes the ydotool paste backend and `paste_chord` in detail. Rewrite it around the script backend and drop the `paste_chord` sentence.
2. The window-class gotcha under "Environment gotchas", whose entire premise is that a `None` class silently changes the ydotool paste chord. That failure mode is gone — yappr no longer chooses a chord. Keep the table (it is measured, and `style::resolve` still consumes the same `None`), but replace the framing: the consequence is now that per-application style rules fall back to their defaults, and the `hyprctl`-reports-on-stdout and AT-SPI-polls-because-no-events notes stay exactly as they are.
3. The `--replay`/example command block: `paste_probe` → `script_probe`, with the new usage line.

`docs/HANDOVER.md` — `grep -n ydotool docs/HANDOVER.md`. It records what was verified on real hardware on 2026-09-07 with the ydotool backend. **Do not rewrite history**: add a dated note that the backend was replaced on 2026-09-09 and that the script path has not been verified on hardware yet, leaving the original entry intact.

- [ ] **Step 4: Run the full gate**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: PASS.

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -20`
Expected: no warnings.

Run: `bun run build 2>&1 | tail -10`
Expected: clean.

Run: `cargo test --workspace -- --ignored --test-threads=1 2>&1 | tail -20`
Expected: 11 passed. `--test-threads=1` is not optional (CLAUDE.md). This is the run that would catch a C++ ABI regression; nothing here should touch it, and a failure means something unrelated broke.

Run the probe by hand, in a scratch window, only if a paste script is configured:

Run: `cargo run -p yappr-core --example script_probe -- "hallo welt"`
Expected: prints the script path, `argv[1] = "hallo welt"`, and `inject() = Ok`, with the text pasted into the focused window.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "docs: the script backend replaces ydotool, and script_probe replaces paste_probe"
```

---

## Self-Review

**Spec coverage.** Each design point maps to a task: `$1`-only contract → Task 1 Steps 1/4; no pre-copy → Task 1 Step 4's doc and the absence of a `ClipboardInjector` call; grandchild pipes → Task 1 Step 1's `a_paste_script_that_backgrounds_its_clipboard_restore_does_not_block`; the `"ydotool"` alias → Task 2 Steps 1/3; retired keys → Task 2 Steps 1/3 and Task 4 Step 1; GNOME → Task 3; GUI → Task 4; comments, example and docs → Task 5. `winclass`/`gnome`/`hypr` untouched except for wording → Task 5 Step 2, and the Global Constraints say why.

**Placeholder scan.** No "TBD"/"TODO"/"similar to Task N". Every code step carries the code. The one step that describes rather than shows is Task 5 Step 3's README/CLAUDE.md rewrite, which is prose in a document whose current text has to be read first — it is specified as a list of claims the text must make, plus the one code block that has to appear verbatim.

**Type consistency.** `ScriptInjector::new(&InjectConfig) -> Self` (Task 1) is what `build` calls in Task 2. `InjectError::NotConfigured` is a unit variant, matched as `InjectError::NotConfigured { .. }` in Task 1's test — which is valid Rust for a unit variant and stays valid if a field is ever added. `run_backend(&'static str, &OsStr, Vec<String>, Duration)` is defined in Task 1 Step 3 and called in Task 1 Step 4 and Task 2 Step 4 with that signature. `d_script() -> String` is added in Task 1 Step 2 and referenced by `#[serde(default = "d_script")]` in the same step. `recommended_backend(&Desktop) -> &'static str` and `backend_prereqs(&Desktop) -> Vec<&'static str>` keep their signatures through Task 3, so `build_wizard_state`'s call sites need no change.

**Variants checked, not assumed.** `yappr_core::desktop::Desktop` (`crates/yappr-core/src/desktop.rs:16`) has four variants — `Hyprland`, `Gnome`, `Other(String)`, `Unknown` — and derives `Debug, Clone, PartialEq, Eq`. Task 3's exhaustive tests iterate all four and construct `Other` with a payload; `{d:?}` in their assertion messages is available from the derived `Debug`. Note that `recommended_backend`'s and `prerequisites_for`'s `_ =>` arms already cover `Unknown` and `Other`, so only the `Gnome` arm changes.
