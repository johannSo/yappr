//! The KI pane's OpenClaw card: install the plugin, register yappr as
//! OpenClaw's dictation provider, and report honestly on all of it.
//!
//! OpenClaw is a separate product with its own config file, its own plugin
//! registry and its own gateway process. Three rules follow from that, and
//! every awkward thing in this module is one of them:
//!
//! 1. **Nothing here edits `~/.openclaw/openclaw.json` directly.** Every
//!    read and every write goes through `openclaw config get`/`config set`,
//!    which is the interface OpenClaw documents, resolves
//!    `$OPENCLAW_CONFIG_PATH` for us, parses the JSON5 that file is allowed
//!    to be, validates against the live schema, and refuses when the user
//!    has set `OPENCLAW_CONFIG_READONLY=1`. Hand-parsing that file would be
//!    a second implementation of all five, and the failure mode is
//!    destroying a config yappr does not own. This is the same instinct
//!    `hypr.rs` follows for Hyprland, one step further: there we only print
//!    the lines, here we call the other program's own writer.
//! 2. **Every step is reported, including the ones that failed.** The
//!    install is five subprocess calls against another program; any of them
//!    can fail on its own (no CLI, a read-only config, a plugin id already
//!    installed from a different source). A card that collapsed that into
//!    one boolean would be unable to say which half happened, which is the
//!    only question worth asking when dictation into OpenClaw does not
//!    work.
//! 3. **stdout is read as carefully as stderr.** CLAUDE.md has paid for
//!    this twice already -- `hyprctl` reports failures on stdout, and a
//!    paste script's `ydotool` does too. The OpenClaw CLI documents a JSON
//!    failure envelope *on stdout* for `--json` calls, so a diagnostic that
//!    read only stderr would routinely say nothing at all.
//!
//! The gateway is deliberately **not** restarted. OpenClaw's own docs say a
//! newly linked plugin's `register(api)` runs after the gateway that serves
//! the channel restarts, so the last step is a note saying so rather than a
//! `gateway restart` this app performs on the user's behalf: that process is
//! serving live agent sessions, and ending them is not a side effect anyone
//! clicking "Installieren" in a dictation app's settings window has asked
//! for.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use yappr_core::paths;
use yappr_core::proto::Request;
use yappr_core::server::{dispatch, realtime_status, Daemon};

use crate::settings_cmds::Server;

/// The plugin id, spelled the same in `openclaw.plugin.json`, in the
/// provider's `id`, and in every config path below. OpenClaw matches on it
/// three separate ways (the manifest contract, the registry, the streaming
/// provider selection), so it is a constant rather than four literals.
const PLUGIN_ID: &str = "yappr";

/// Where the streaming transcription provider is configured today.
///
/// From the build brief, quoting OpenClaw's own `docs/nodes/talk.md`: "The
/// current Gateway relay uses the Voice Call streaming provider config until
/// a dedicated Talk transcription config surface ships." So this path is
/// known-temporary, which is why it appears exactly once on each side --
/// here, and in the plugin's `config.js` -- rather than being spread through
/// either.
const STREAMING_PATH: &str = "plugins.entries.voice-call.config.streaming";

/// The plugin, embedded. `include_str!` is what makes the repo copy the only
/// copy: there is no build step that could be skipped and no second source
/// to update, and a file renamed in `openclaw-plugin/` fails the build here
/// rather than shipping a directory OpenClaw cannot load.
const PLUGIN_FILES: &[(&str, &str)] = &[
    ("package.json", include_str!("../../openclaw-plugin/package.json")),
    ("openclaw.plugin.json", include_str!("../../openclaw-plugin/openclaw.plugin.json")),
    ("index.js", include_str!("../../openclaw-plugin/index.js")),
    ("capability-catalog.js", include_str!("../../openclaw-plugin/capability-catalog.js")),
    ("provider-factory.js", include_str!("../../openclaw-plugin/provider-factory.js")),
    ("config.js", include_str!("../../openclaw-plugin/config.js")),
    ("README.md", include_str!("../../openclaw-plugin/README.md")),
];

/// Reading the CLI's answer to a question. Generous because it is a Node
/// process starting cold: ~0.5 s is normal, and a machine under load is
/// slower without anything being wrong.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Writing. `plugins install` resolves and registers a package.
const WRITE_TIMEOUT: Duration = Duration::from_secs(90);

/// One line of the card's result list.
fn step(label: &str, ok: bool, detail: impl Into<String>) -> Value {
    json!({ "label": label, "ok": ok, "detail": detail.into() })
}

/// Where the `openclaw` executable is, or why it isn't.
///
/// `$PATH` first, then the places npm's global prefix actually puts it. The
/// fallbacks are not paranoia: yappr is started from a tray icon or a
/// `.desktop` entry and inherits no login shell, so `$PATH` here is
/// routinely the short system one while the user's own shell has had
/// `~/.npm-global/bin` on it for years. The same asymmetry is why
/// `ydotoold`'s socket path is a documented trap in CLAUDE.md.
fn find_cli() -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|p| p.join("openclaw")));
    }
    if let Some(home) = dirs::home_dir() {
        for rel in [
            ".npm-global/bin/openclaw",
            ".local/bin/openclaw",
            ".local/share/npm/bin/openclaw",
            ".bun/bin/openclaw",
            ".volta/bin/openclaw",
        ] {
            candidates.push(home.join(rel));
        }
    }
    candidates.push(PathBuf::from("/usr/local/bin/openclaw"));
    candidates.push(PathBuf::from("/usr/bin/openclaw"));

    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    Err("OpenClaw wurde nicht gefunden. Installiere es mit `npm i -g openclaw` und öffne diese Einstellungen erneut.".to_string())
}

/// What one CLI call produced: both streams, kept apart, plus the status.
struct CliRun {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl CliRun {
    /// The human sentence for a step's `detail`.
    ///
    /// stderr first, then stdout, mirroring `inject::diagnostic` and for the
    /// identical reason: a program that reports its failure the usual way
    /// still reads the way it always did, and one that reports it on stdout
    /// (which this CLI does, for `--json` calls) is no longer silent.
    fn diagnostic(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let err = self.stderr.trim();
        let out = self.stdout.trim();
        if !err.is_empty() {
            parts.push(err);
        }
        if !out.is_empty() {
            parts.push(out);
        }
        let joined = parts.join(" — ");
        // Terminal output can be long (a stack trace, a JSON dump). The card
        // shows this inline, so cap it where a sentence stops being one.
        if joined.chars().count() > 400 {
            joined.chars().take(400).collect::<String>() + " …"
        } else {
            joined
        }
    }
}

/// Runs the CLI, bounded. Invariant 6: nothing in this app calls a
/// subprocess without a timeout -- and this one is a Node program that talks
/// to a possibly-unreachable gateway, which is exactly the shape of hang
/// that rule exists for.
fn run_cli(cli: &Path, args: &[&str], timeout: Duration) -> Result<CliRun, String> {
    let mut cmd = Command::new(cli);
    cmd.args(args);
    // The CLI is interactive by default for anything that looks like a
    // trust decision. There is no terminal behind this call, so an
    // unanswered prompt would be a `timeout` with no explanation.
    cmd.env("CI", "1");
    cmd.env("NO_COLOR", "1");
    let out = yappr_core::procutil::run_with_timeout(cmd, timeout, None)
        .map_err(|e| format!("`openclaw {}` konnte nicht gestartet werden: {e}", args.join(" ")))?;
    Ok(CliRun {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The first JSON value in `text`, which is how every `--json` call here is
/// read.
///
/// Not `serde_json::from_str` on the whole output: the CLI prints warnings
/// and progress lines around its JSON often enough that strict parsing would
/// make the card report "not installed" for a healthy install. Scans for the
/// first `{` or `[` that parses to the end of the text.
fn first_json(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'{' || *b == b'[' {
            let mut de = serde_json::Deserializer::from_str(&text[i..]).into_iter::<Value>();
            if let Some(Ok(v)) = de.next() {
                return Some(v);
            }
        }
    }
    None
}

/// Whether any object anywhere in `v` has `"id": <id>`.
///
/// `plugins list --json` is documented as "inventory plus registry
/// diagnostics and package dependency install state", i.e. a shape that is
/// free to grow. Walking it beats indexing into a key path that a future
/// release renames -- the question being asked is only "does OpenClaw know
/// about this plugin", and a false answer to that would have the card offer
/// an install that is already done.
fn contains_plugin(v: &Value, id: &str) -> bool {
    match v {
        Value::Object(map) => {
            if map.get("id").and_then(Value::as_str) == Some(id) {
                return true;
            }
            map.values().any(|child| contains_plugin(child, id))
        }
        Value::Array(items) => items.iter().any(|child| contains_plugin(child, id)),
        _ => false,
    }
}

/// Writes the plugin into `dir`, creating it if needed.
///
/// Only rewrites a file whose content differs. `openclaw plugins install
/// --link` loads from this directory on every OpenClaw start, so rewriting
/// unchanged bytes would churn mtimes under a watcher for nothing -- and, on
/// the path that matters, an install re-run after an upgrade touches exactly
/// the files the upgrade changed.
fn materialize(dir: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("{} konnte nicht angelegt werden: {e}", dir.display()))?;
    let mut written = 0;
    for (name, body) in PLUGIN_FILES {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(*body) {
            continue;
        }
        std::fs::write(&path, body)
            .map_err(|e| format!("{} konnte nicht geschrieben werden: {e}", path.display()))?;
        written += 1;
    }
    Ok(written)
}

/// The version of the plugin about to be (or already) installed, read out of
/// the embedded `package.json` so the card and the manifest cannot disagree.
fn plugin_version() -> String {
    PLUGIN_FILES
        .iter()
        .find(|(name, _)| *name == "package.json")
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// The provider block written under `<STREAMING_PATH>.providers.yappr`, and
/// the selection that makes it the active one.
///
/// Built from the *live* `[realtime]` config rather than from constants, so
/// a user who moved the port or set a token gets a plugin that still
/// connects. That is the whole reason this is generated at install time
/// instead of being baked into `openclaw.plugin.json`.
fn streaming_patch(realtime: &yappr_core::config::RealtimeConfig) -> Value {
    json!({
        "enabled": true,
        "provider": PLUGIN_ID,
        "providers": { PLUGIN_ID: provider_entry(realtime) },
    })
}

/// The connection settings themselves, written to two places: inside
/// [`streaming_patch`] (where the host reads them) and to
/// `plugins.entries.yappr.config` (where OpenClaw's plugin page shows them).
///
/// Every key here is declared in `openclaw-plugin/openclaw.plugin.json`'s
/// `configSchema`, and that is a requirement rather than a coincidence: the
/// manifest schema is `additionalProperties: false`, so a key written here and
/// not declared there fails validation and takes the whole entry with it.
/// `model` is deliberately not written -- yappr's active ASR model is
/// `[asr] model` in this app, not something an OpenClaw session can switch, and
/// a form row that changes nothing is worse than no row.
fn provider_entry(realtime: &yappr_core::config::RealtimeConfig) -> Value {
    let mut entry = json!({
        "host": "127.0.0.1",
        "port": realtime.port,
        // 8 kHz mu-law is not a preference, it is what OpenClaw's browser
        // transcription relay emits -- `RELAY_INPUT_ENCODING = "g711_ulaw"`,
        // `RELAY_INPUT_SAMPLE_RATE_HZ = 8e3` in the host's `talk-*.mjs`, with
        // an `assertRelayInputAudioConfig` that refuses to start a session
        // against a provider declaring anything else. The first version of
        // this function wrote 16 kHz `linear16` on the reasoning that a
        // browser mic is not a telephone, and every dictation failed with
        // "Gateway transcription relay requires g711_ulaw/8000 audio"
        // (reported 2026-09-14). yappr expands and resamples it on the other
        // side; see `realtime::PcmDecoder`.
        "sampleRate": 8000,
        "encoding": "mulaw",
    });
    if !realtime.token.is_empty() {
        entry["token"] = json!(realtime.token);
    }
    entry
}

/// The whole card's state, in one object. Every field is read fresh; none of
/// it is cached, because all four facts live in other programs' files and
/// can change without this app being told.
fn status(daemon: Option<&Arc<Daemon>>) -> Value {
    let dir = paths::openclaw_plugin_dir();
    let (cli_found, cli_path, cli_version, cli_error) = match find_cli() {
        Ok(path) => {
            let version = run_cli(&path, &["--version"], READ_TIMEOUT)
                .ok()
                .filter(|r| r.ok)
                .map(|r| r.stdout.trim().to_string())
                .filter(|v| !v.is_empty());
            (true, Some(path.display().to_string()), version, None)
        }
        Err(e) => (false, None, None, Some(e)),
    };

    let mut installed = false;
    let mut linked = false;
    let mut configured = false;
    let mut selected = false;
    let mut stale = false;
    // The same values `install` would write, read from the same place it
    // reads them, so "in step" means exactly "a re-run would change
    // nothing".
    let live_realtime = realtime_config();
    if let Some(path) = cli_path.as_deref().map(PathBuf::from) {
        if let Ok(run) = run_cli(&path, &["plugins", "list", "--json"], READ_TIMEOUT) {
            if let Some(v) = first_json(&run.stdout) {
                installed = contains_plugin(&v, PLUGIN_ID);
                // The linked install records this exact directory, so its
                // presence anywhere in the inventory is what separates
                // "yappr's plugin" from "some other plugin that happens to
                // share the id".
                linked = run.stdout.contains(&dir.display().to_string());
            }
        }
        if let Ok(run) = run_cli(
            &path,
            &["config", "get", STREAMING_PATH, "--json"],
            READ_TIMEOUT,
        ) {
            // An unset path exits 1 with a failure envelope, which is a
            // perfectly good "not configured" and not worth reporting as an
            // error.
            if run.ok {
                if let Some(v) = first_json(&run.stdout) {
                    let entry = v.pointer(&format!("/providers/{PLUGIN_ID}"));
                    configured = entry.is_some_and(|p| !p.is_null());
                    selected = v.get("provider").and_then(Value::as_str) == Some(PLUGIN_ID)
                        && v.get("enabled").and_then(Value::as_bool) != Some(false);
                    // Drift, which is otherwise invisible: `install` copies
                    // the port and the token into OpenClaw's config, and
                    // nothing re-copies them when either changes here
                    // afterwards. The symptom is a provider that reports
                    // itself configured and selected while every dictation
                    // silently fails to connect, so the answer has to be a
                    // state the card can see rather than a sentence in a
                    // help text. The token is compared by *presence* only:
                    // `config get` reads a redacted snapshot, so its value
                    // is not necessarily the real one.
                    if let Some(entry) = entry {
                        stale = drifted(entry, &live_realtime);
                    }
                }
            }
        }

        // The second copy, which is the one the plugin actually prefers at
        // runtime (`readOwnEntryConfig` merges it over what the host resolves).
        // A stale entry here is therefore worse than a stale streaming record,
        // not merely untidy: it is the value that wins.
        if !stale {
            if let Ok(run) = run_cli(
                &path,
                &["config", "get", &format!("plugins.entries.{PLUGIN_ID}.config"), "--json"],
                READ_TIMEOUT,
            ) {
                if run.ok {
                    if let Some(v) = first_json(&run.stdout) {
                        stale = drifted(&v, &live_realtime);
                    }
                }
            }
        }
    }

    let rt = match daemon {
        Some(d) => realtime_status(d),
        None => yappr_core::server::RealtimeStatus {
            enabled: false,
            port: 0,
            running: false,
            error: None,
        },
    };

    json!({
        "cli": { "found": cli_found, "path": cli_path, "version": cli_version, "error": cli_error },
        "plugin": {
            "installed": installed,
            "dir": dir.display().to_string(),
            "linked": linked,
            "version": plugin_version(),
        },
        "provider": { "configured": configured, "selected": selected, "stale": stale },
        "realtime": {
            "enabled": rt.enabled,
            "port": rt.port,
            "running": rt.running,
            "error": rt.error,
        },
    })
}

/// Whether a stored provider record still matches this machine's
/// `[realtime]`.
///
/// Compares the port by value and the token by *presence* only: `config get`
/// answers from a redacted snapshot, so the token's characters are not
/// necessarily the real ones and comparing them would report drift on every
/// machine that has one.
fn drifted(entry: &Value, realtime: &yappr_core::config::RealtimeConfig) -> bool {
    let port_there = entry.get("port").and_then(Value::as_u64);
    let token_there = entry.get("token").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
    port_there != Some(realtime.port as u64) || token_there != !realtime.token.is_empty()
}

/// Reads the current `[realtime]` config off disk.
///
/// Off disk rather than out of the daemon because this is the same file the
/// settings window is editing, and `save_config` has already written
/// anything the user changed before this button could be clicked.
fn realtime_config() -> yappr_core::config::RealtimeConfig {
    yappr_core::config::Config::load_from(&paths::config_file())
        .map(|c| c.realtime)
        .unwrap_or_default()
}

#[tauri::command]
pub async fn openclaw_status(server: tauri::State<'_, Server>) -> Result<Value, String> {
    let daemon = server.0.clone();
    tauri::async_runtime::spawn_blocking(move || status(daemon.as_ref()))
        .await
        .map_err(|e| format!("interner Fehler: {e}"))
}

#[tauri::command]
pub async fn openclaw_install(server: tauri::State<'_, Server>) -> Result<Value, String> {
    let daemon = server.0.clone();
    tauri::async_runtime::spawn_blocking(move || install(daemon.as_ref()))
        .await
        .map_err(|e| format!("interner Fehler: {e}"))?
}

#[tauri::command]
pub async fn openclaw_remove(server: tauri::State<'_, Server>) -> Result<Value, String> {
    let daemon = server.0.clone();
    tauri::async_runtime::spawn_blocking(move || remove(daemon.as_ref()))
        .await
        .map_err(|e| format!("interner Fehler: {e}"))?
}

/// The button.
///
/// Ordered so that the one step nothing external can break -- turning
/// yappr's own endpoint on -- happens first, and so that the plugin exists
/// on disk before anything is asked to load it. A step that fails does not
/// stop the ones after it: linking can succeed while the config write is
/// refused by `OPENCLAW_CONFIG_READONLY`, and the user is better served by
/// a list showing exactly that than by a run that stopped at the first
/// problem and left the rest unattempted.
fn install(daemon: Option<&Arc<Daemon>>) -> Result<Value, String> {
    let cli = find_cli()?;
    let dir = paths::openclaw_plugin_dir();
    let mut steps: Vec<Value> = Vec::new();

    match materialize(&dir) {
        Ok(0) => steps.push(step(
            "Plugin-Dateien",
            true,
            format!("Bereits aktuell in {}", dir.display()),
        )),
        Ok(n) => steps.push(step(
            "Plugin-Dateien",
            true,
            format!("{n} Datei(en) nach {} geschrieben", dir.display()),
        )),
        // Nothing after this can work without the files, and the cause is
        // always local (no permission, no disk), so this one really is
        // fatal.
        Err(e) => return Err(e),
    }

    // yappr's own endpoint, through the same `SetConfig` the settings form
    // uses -- which validates, writes atomically, and re-syncs the listener
    // (invariant 9, and `server::sync_realtime`). A patch of one leaf: every
    // other `[realtime]` key keeps whatever the user chose.
    let realtime_step = match daemon {
        Some(d) => {
            let req = Request::SetConfig { config: json!({ "realtime": { "enabled": true } }) };
            let resp = dispatch(d, req);
            if resp.ok {
                let rt = realtime_status(d);
                if rt.running {
                    step("Lokaler Endpunkt", true, format!("Läuft auf 127.0.0.1:{}", rt.port))
                } else {
                    step(
                        "Lokaler Endpunkt",
                        false,
                        rt.error.unwrap_or_else(|| {
                            format!("Eingeschaltet, aber Port {} antwortet nicht", rt.port)
                        }),
                    )
                }
            } else {
                step(
                    "Lokaler Endpunkt",
                    false,
                    resp.err.unwrap_or_else(|| "unbekannter Fehler".into()),
                )
            }
        }
        None => step("Lokaler Endpunkt", false, "Kein Daemon in diesem Prozess"),
    };
    steps.push(realtime_step);

    let dir_arg = dir.display().to_string();
    let run = run_cli(
        &cli,
        &["plugins", "install", "--link", &dir_arg, "--force", "--accept-capabilities"],
        WRITE_TIMEOUT,
    )?;
    steps.push(step(
        "In OpenClaw einbinden",
        run.ok,
        if run.ok { format!("Verknüpft mit {dir_arg}") } else { run.diagnostic() },
    ));

    let run = run_cli(
        &cli,
        &["plugins", "enable", PLUGIN_ID, "--accept-capabilities"],
        WRITE_TIMEOUT,
    )?;
    steps.push(step(
        "Plugin aktivieren",
        run.ok,
        if run.ok { "Aktiviert".to_string() } else { run.diagnostic() },
    ));

    let realtime = realtime_config();
    let patch = streaming_patch(&realtime).to_string();
    let run = run_cli(
        &cli,
        &["config", "set", STREAMING_PATH, &patch, "--strict-json", "--merge"],
        WRITE_TIMEOUT,
    )?;
    steps.push(step(
        "Als Diktat-Anbieter eintragen",
        run.ok,
        if run.ok {
            format!("{STREAMING_PATH}.provider = \"{PLUGIN_ID}\"")
        } else {
            run.diagnostic()
        },
    ));

    // The same values again, into the plugin's own entry -- which is what
    // OpenClaw's plugin page renders as a settings form. Writing it is the
    // difference between that form arriving filled in with this machine's
    // port and arriving empty next to a working install, and the plugin
    // reads it back (`readOwnEntryConfig`), so a field edited there is a
    // field that takes effect. Both copies are written in the same run, so
    // they can only diverge if someone edits one by hand -- which `status`
    // reports as `provider.stale`.
    let entry_patch = provider_entry(&realtime).to_string();
    let run = run_cli(
        &cli,
        &[
            "config",
            "set",
            &format!("plugins.entries.{PLUGIN_ID}.config"),
            &entry_patch,
            "--strict-json",
            "--merge",
        ],
        WRITE_TIMEOUT,
    )?;
    steps.push(step(
        "Plugin-Einstellungen ausfüllen",
        run.ok,
        if run.ok {
            format!("127.0.0.1:{} in OpenClaws Plugin-Seite eingetragen", realtime.port)
        } else {
            run.diagnostic()
        },
    ));

    steps.push(step(
        "Hinweis",
        true,
        "OpenClaw lädt neu eingebundene Plugins erst beim nächsten Start des Gateways: `openclaw gateway restart`.",
    ));

    Ok(json!({ "steps": steps, "status": status(daemon) }))
}

/// Undoes [`install`] on OpenClaw's side only.
///
/// `[realtime]` is left exactly as it is. It is yappr's own setting, it may
/// have been turned on for something else entirely, and a "remove the
/// OpenClaw plugin" button that quietly closed a port the user opened would
/// be doing something they did not ask for. The card says so.
fn remove(daemon: Option<&Arc<Daemon>>) -> Result<Value, String> {
    let cli = find_cli()?;
    let mut steps: Vec<Value> = Vec::new();

    // Only the keys this app wrote. `unset` on the whole `streaming` block
    // would take a Deepgram or OpenAI provider the user configured
    // themselves with it.
    let before = status(daemon);
    if before.pointer("/provider/selected").and_then(Value::as_bool) == Some(true) {
        let run = run_cli(
            &cli,
            &["config", "unset", &format!("{STREAMING_PATH}.provider")],
            WRITE_TIMEOUT,
        )?;
        steps.push(step(
            "Auswahl zurücknehmen",
            run.ok,
            if run.ok {
                "OpenClaw wählt wieder automatisch".to_string()
            } else {
                run.diagnostic()
            },
        ));
    }
    let run = run_cli(
        &cli,
        &["config", "unset", &format!("{STREAMING_PATH}.providers.{PLUGIN_ID}")],
        WRITE_TIMEOUT,
    )?;
    steps.push(step(
        "Anbieter-Eintrag entfernen",
        run.ok,
        if run.ok { "Entfernt".to_string() } else { run.diagnostic() },
    ));

    // `plugins uninstall` alone, deliberately: it removes the install
    // record, the load path *and* the plugin's settings. Calling `plugins
    // disable` first -- the obvious symmetry with the install's `enable` --
    // writes `plugins.entries.yappr.enabled = false` into OpenClaw's config
    // and uninstall then leaves it there, so the tidy-up leaves a permanent
    // entry for a plugin that no longer exists. Measured, on this machine,
    // 2026-09-14.
    let run = run_cli(&cli, &["plugins", "uninstall", PLUGIN_ID, "--force"], WRITE_TIMEOUT)?;
    steps.push(step(
        "Plugin entfernen",
        run.ok,
        if run.ok { "Aus OpenClaw entfernt".to_string() } else { run.diagnostic() },
    ));

    // `plugins uninstall` leaves `plugins.entries.yappr.enabled = false`
    // behind -- measured on this machine, 2026-09-14, both with and without
    // a preceding `plugins disable`. Harmless, but it is a settings entry
    // for a plugin that is gone, and it is the difference between "removed"
    // and "removed, as if it had never been there". Only swept when it holds
    // nothing but that flag: a `config` block under it would be the user's.
    if let Some(run) = clean_own_entry(&cli) {
        steps.push(step(
            "Eintrag entfernen",
            run.ok,
            if run.ok { "Entfernt".to_string() } else { run.diagnostic() },
        ));
    }

    // What the install wrote around the provider entry, but only if nothing
    // else is left in it. `streaming.enabled` is a Voice-Call-wide switch,
    // not yappr's, so it is removed only when the block it lives in holds
    // nothing but what this app put there -- anyone who configured a second
    // provider keeps theirs untouched.
    if let Some(run) = clean_empty_streaming(&cli) {
        steps.push(step(
            "Aufräumen",
            run.ok,
            if run.ok {
                "Leeren Abschnitt entfernt".to_string()
            } else {
                run.diagnostic()
            },
        ));
    }

    steps.push(step(
        "Lokaler Endpunkt",
        true,
        "Bleibt unverändert — das ist yapprs eigene Einstellung und steht unten.",
    ));

    Ok(json!({ "steps": steps, "status": status(daemon) }))
}

/// Removes `plugins.entries.yappr` when `plugins uninstall` has left it as a
/// bare `enabled: false`.
fn clean_own_entry(cli: &Path) -> Option<CliRun> {
    let path = format!("plugins.entries.{PLUGIN_ID}");
    let run = run_cli(cli, &["config", "get", &path, "--json"], READ_TIMEOUT).ok()?;
    if !run.ok {
        return None; // already gone
    }
    let v = first_json(&run.stdout)?;
    if !own_entry_is_vestigial(&v) {
        return None;
    }
    run_cli(cli, &["config", "unset", &path], WRITE_TIMEOUT).ok()
}

/// Whether our plugin entry holds nothing but the enablement flag
/// `plugins enable`/`plugins uninstall` write.
fn own_entry_is_vestigial(v: &Value) -> bool {
    let Some(map) = v.as_object() else { return false };
    map.iter().all(|(k, value)| match k.as_str() {
        "enabled" => value.as_bool() == Some(false),
        "config" => value.as_object().is_some_and(|c| c.is_empty()),
        _ => false,
    })
}

/// Removes `<STREAMING_PATH>` when the install's own leftovers are all that
/// is in it.
///
/// Returns `None` when there is nothing to do, so the step only appears in
/// the list when something actually happened. The shape it will remove is
/// `{}`, `{"enabled":…}`, `{"providers":{}}` or the two together -- anything
/// else means the user (or another plugin's setup) put something there, and
/// this app has no business deleting it.
fn clean_empty_streaming(cli: &Path) -> Option<CliRun> {
    let run = run_cli(cli, &["config", "get", STREAMING_PATH, "--json"], READ_TIMEOUT).ok()?;
    if !run.ok {
        return None; // already unset
    }
    let v = first_json(&run.stdout)?;
    if !streaming_is_vestigial(&v) {
        return None;
    }
    let removed = run_cli(cli, &["config", "unset", STREAMING_PATH], WRITE_TIMEOUT).ok()?;
    if !removed.ok {
        return Some(removed);
    }
    // And the now-empty plugin entry the path went through. `voice-call` is
    // not installed on the machine this was developed on -- OpenClaw reads
    // this config straight out of the tree either way -- so without this,
    // "remove" leaves behind a settings entry for a plugin that does not
    // exist, as evidence of a feature that was undone.
    let entry = format!("plugins.entries.{}", STREAMING_PATH.split('.').nth(2).unwrap_or("voice-call"));
    if let Ok(after) = run_cli(cli, &["config", "get", &entry, "--json"], READ_TIMEOUT) {
        let empty = after
            .ok
            .then(|| first_json(&after.stdout))
            .flatten()
            .is_some_and(|v| entry_is_empty(&v));
        if empty {
            let _ = run_cli(cli, &["config", "unset", &entry], WRITE_TIMEOUT);
        }
    }
    Some(removed)
}

/// Whether a plugin entry holds nothing at all -- `{}` or a `config` that is
/// itself `{}`. Anything else (an `enabled` flag, another section) is the
/// user's and stays.
fn entry_is_empty(v: &Value) -> bool {
    let Some(map) = v.as_object() else { return false };
    map.iter().all(|(k, value)| {
        k == "config" && value.as_object().is_some_and(|c| c.is_empty())
    })
}

/// Whether `streaming` holds nothing but what [`install`] wrote.
fn streaming_is_vestigial(v: &Value) -> bool {
    let Some(map) = v.as_object() else { return false };
    map.iter().all(|(k, value)| match k.as_str() {
        "enabled" => true,
        "providers" => value.as_object().is_some_and(|p| p.is_empty()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded plugin is a plugin: the manifest and the package agree
    /// with each other and with `PLUGIN_ID`, and the entry point named by
    /// `package.json` is a file that was actually embedded.
    ///
    /// This is the `include_str!` equivalent of the overlay-event fixture
    /// tests -- nothing else notices a renamed file, because the rename
    /// keeps compiling as long as the `include_str!` path is updated with
    /// it, and the result is a directory OpenClaw refuses at install time
    /// with a message nobody sees until they click the button.
    #[test]
    fn the_embedded_plugin_is_internally_consistent() {
        let pkg: Value = serde_json::from_str(
            PLUGIN_FILES.iter().find(|(n, _)| *n == "package.json").unwrap().1,
        )
        .expect("package.json must be valid JSON");
        let manifest: Value = serde_json::from_str(
            PLUGIN_FILES.iter().find(|(n, _)| *n == "openclaw.plugin.json").unwrap().1,
        )
        .expect("openclaw.plugin.json must be valid JSON");

        assert_eq!(manifest["id"], PLUGIN_ID);
        assert_eq!(
            manifest["contracts"]["realtimeTranscriptionProviders"][0], PLUGIN_ID,
            "the manifest must declare the provider contract, or an installed plugin's \
             registration is rejected outright"
        );
        assert_eq!(
            manifest["activation"]["onStartup"], false,
            "the capability catalog exists so the provider is discoverable without \
             activating the runtime; onStartup would defeat it"
        );

        for entry in ["extensions", "capabilityCatalogEntry"] {
            let named: Vec<String> = match (&pkg["openclaw"][entry], &manifest[entry]) {
                (Value::Array(a), _) => a.iter().filter_map(|v| v.as_str()).map(str::to_string).collect(),
                (_, Value::String(s)) => vec![s.clone()],
                _ => vec![],
            };
            for rel in named {
                let file = rel.trim_start_matches("./");
                assert!(
                    PLUGIN_FILES.iter().any(|(n, _)| *n == file),
                    "{entry} names {file}, which is not one of the embedded files"
                );
            }
        }
        assert!(!plugin_version().is_empty(), "package.json must carry a version");
    }

    /// The plugin must stay self-contained: it is copied to a directory with
    /// no `node_modules` above it, so a bare import specifier -- including
    /// `openclaw/plugin-sdk/*`, which is private-local anyway -- cannot
    /// resolve and the plugin fails to load with a stack trace in OpenClaw's
    /// log rather than anywhere the user will look.
    #[test]
    fn the_embedded_plugin_imports_nothing() {
        for (name, body) in PLUGIN_FILES {
            if !name.ends_with(".js") {
                continue;
            }
            for line in body.lines() {
                let line = line.trim_start();
                assert!(
                    !(line.starts_with("import ") && line.contains(" from ")
                        && !line.contains("./")),
                    "{name} imports a bare specifier, which cannot resolve from the \
                     materialized plugin directory: {line}"
                );
            }
        }
    }

    /// Every key the install writes into `plugins.entries.yappr.config` has
    /// to be declared in the plugin manifest, because that schema is
    /// `additionalProperties: false`: an undeclared key fails validation and
    /// OpenClaw refuses the whole entry. Nothing else would notice -- the
    /// install step reports the CLI's rejection, but only after a user has
    /// pressed the button and watched it fail.
    #[test]
    fn every_written_setting_is_one_the_plugin_manifest_declares() {
        let manifest: Value = serde_json::from_str(
            PLUGIN_FILES.iter().find(|(n, _)| *n == "openclaw.plugin.json").unwrap().1,
        )
        .unwrap();
        let declared = manifest["configSchema"]["properties"]
            .as_object()
            .expect("the manifest must declare a properties map");
        assert_eq!(
            manifest["configSchema"]["additionalProperties"], false,
            "this test is only load-bearing while the schema is strict"
        );

        let cfg = yappr_core::config::RealtimeConfig {
            token: "t".into(),
            ..Default::default()
        };
        for key in provider_entry(&cfg).as_object().unwrap().keys() {
            assert!(
                declared.contains_key(key),
                "the install writes {key}, which openclaw.plugin.json does not declare"
            );
        }
    }

    /// The audio format is the relay's, not ours, and it is the one thing in
    /// this record a reader would most reasonably "fix".
    ///
    /// OpenClaw's browser transcription relay emits `g711_ulaw` at 8 kHz and
    /// calls `assertRelayInputAudioConfig` before starting a session, which
    /// throws for any provider config that declares otherwise. Writing the
    /// nicer-sounding 16 kHz `linear16` here does not get 16 kHz audio -- it
    /// gets every dictation refused, which is what happened on 2026-09-14.
    #[test]
    fn the_written_audio_format_is_the_one_the_relay_emits() {
        let entry = provider_entry(&yappr_core::config::RealtimeConfig::default());
        assert_eq!(entry["encoding"], "mulaw");
        assert_eq!(entry["sampleRate"], 8000);
    }

    /// The two copies are the same record, or the settings form on OpenClaw's
    /// plugin page and the config the host actually resolves would disagree
    /// from the moment they are written.
    #[test]
    fn both_copies_of_the_provider_settings_are_written_from_one_source() {
        let cfg = yappr_core::config::RealtimeConfig {
            port: 4242,
            ..Default::default()
        };
        assert_eq!(streaming_patch(&cfg)["providers"][PLUGIN_ID], provider_entry(&cfg));
    }

    /// Drift is what the card reports as "veraltete Zugangsdaten". The port
    /// is compared by value; the token only by presence, because `config get`
    /// answers from a redacted snapshot.
    #[test]
    fn drift_is_detected_on_the_port_and_on_the_presence_of_a_token() {
        let cfg = yappr_core::config::RealtimeConfig {
            port: 17869,
            ..Default::default()
        };
        assert!(!drifted(&json!({ "port": 17869 }), &cfg));
        assert!(drifted(&json!({ "port": 17870 }), &cfg), "a moved port is drift");
        assert!(drifted(&json!({}), &cfg), "a record with no port at all is drift");
        assert!(
            drifted(&json!({ "port": 17869, "token": "leftover" }), &cfg),
            "a token OpenClaw still sends and yappr no longer expects is drift"
        );

        let with_token = yappr_core::config::RealtimeConfig {
            token: "s3cret".into(),
            ..cfg.clone()
        };
        assert!(
            drifted(&json!({ "port": 17869 }), &with_token),
            "a token yappr now requires and OpenClaw does not have is drift"
        );
        assert!(
            !drifted(&json!({ "port": 17869, "token": "***" }), &with_token),
            "a redacted token must not read as drift -- the value is not comparable"
        );
    }

    #[test]
    fn the_streaming_patch_names_this_app_and_its_port() {
        let cfg = yappr_core::config::RealtimeConfig {
            port: 12345,
            ..Default::default()
        };
        let v = streaming_patch(&cfg);
        assert_eq!(v["provider"], PLUGIN_ID);
        assert_eq!(v["enabled"], true);
        assert_eq!(v["providers"][PLUGIN_ID]["port"], 12345);
        assert_eq!(v["providers"][PLUGIN_ID]["host"], "127.0.0.1");
        assert!(
            v["providers"][PLUGIN_ID].get("token").is_none(),
            "an empty token must not be written: the plugin would then send an \
             Authorization header the endpoint does not expect"
        );
    }

    #[test]
    fn a_configured_token_reaches_the_plugin_config() {
        let cfg = yappr_core::config::RealtimeConfig {
            token: "hunter2".into(),
            ..Default::default()
        };
        let v = streaming_patch(&cfg);
        assert_eq!(v["providers"][PLUGIN_ID]["token"], "hunter2");
    }

    #[test]
    fn the_plugin_is_found_in_a_registry_inventory_whatever_shape_it_has() {
        let listing = json!({
            "plugins": [
                { "id": "deepgram", "enabled": true },
                { "id": "yappr", "origin": { "kind": "linked" } },
            ],
            "diagnostics": {},
        });
        assert!(contains_plugin(&listing, "yappr"));
        assert!(!contains_plugin(&listing, "elevenlabs"));
        // The same answer from a nested shape, which is the point of
        // walking rather than indexing.
        let nested = json!({ "registry": { "entries": { "a": { "id": "yappr" } } } });
        assert!(contains_plugin(&nested, "yappr"));
    }

    /// The tidy-up must not become a way to lose someone else's provider.
    #[test]
    fn only_this_apps_own_leftovers_are_swept_up() {
        assert!(streaming_is_vestigial(&json!({})));
        assert!(streaming_is_vestigial(&json!({ "enabled": true })));
        assert!(streaming_is_vestigial(&json!({ "enabled": true, "providers": {} })));
        // A second provider, or any key this app never wrote, stops it dead.
        assert!(!streaming_is_vestigial(&json!({
            "enabled": true,
            "providers": { "deepgram": { "apiKey": "x" } },
        })));
        assert!(!streaming_is_vestigial(&json!({ "provider": "deepgram" })));
        assert!(!streaming_is_vestigial(&json!({ "enabled": true, "endpointingMs": 800 })));
    }

    /// And the same restraint for yappr's own entry: a disabled plugin
    /// someone left configured is not litter.
    #[test]
    fn only_a_bare_disabled_flag_counts_as_a_leftover_entry() {
        assert!(own_entry_is_vestigial(&json!({})));
        assert!(own_entry_is_vestigial(&json!({ "enabled": false })));
        assert!(own_entry_is_vestigial(&json!({ "enabled": false, "config": {} })));
        // Still enabled -- the uninstall did not happen, and deleting this
        // would be deleting a working install's settings.
        assert!(!own_entry_is_vestigial(&json!({ "enabled": true })));
        assert!(!own_entry_is_vestigial(&json!({
            "enabled": false,
            "config": { "port": 18000 },
        })));
    }

    #[test]
    fn an_entry_is_only_empty_when_it_really_is() {
        assert!(entry_is_empty(&json!({})));
        assert!(entry_is_empty(&json!({ "config": {} })));
        assert!(!entry_is_empty(&json!({ "enabled": true })));
        assert!(!entry_is_empty(&json!({ "config": { "tts": { "auto": true } } })));
    }

    #[test]
    fn json_is_read_out_of_output_that_has_noise_around_it() {
        let noisy = "warning: plugins.allow is empty\n{\"providers\":{\"yappr\":{}}}\n";
        let v = first_json(noisy).expect("should find the object");
        assert!(v.pointer("/providers/yappr").is_some());
        assert!(first_json("no json here at all").is_none());
    }

    /// stdout is not a fallback for an empty stderr -- it is read every
    /// time, because this CLI puts its `--json` failure envelope there.
    #[test]
    fn a_diagnostic_carries_both_streams() {
        let r = CliRun { ok: false, stdout: "{\"error\":\"unknown path\"}".into(), stderr: "".into() };
        assert!(r.diagnostic().contains("unknown path"));
        let r = CliRun { ok: false, stdout: "extra".into(), stderr: "the real cause".into() };
        let d = r.diagnostic();
        assert!(d.starts_with("the real cause"), "stderr comes first: {d}");
        assert!(d.contains("extra"));
    }
}
