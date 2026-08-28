# Settings GUI — design

Status: implemented, 2026-08-28. See *Deviations* at the end.

## Purpose

Every OpenWhisprFlow setting must be changeable from a graphical window, without
hand-editing `~/.config/openwhisprflow/config.toml`. The two settings that
prompted this are the input device and the dictation vocabulary, but the
requirement is deliberately total: *no* setting may be GUI-unreachable.

That totality is the constraint that shapes the design. A hand-written form
listing today's fields satisfies it today and quietly stops satisfying it the
first time someone adds a config key. The form is therefore generated from what
the daemon sends, with labels and help text layered on top, so a new key appears
in the GUI as a generic editor rather than not at all.

## Why a separate binary

The overlay must never take keyboard focus — a focused overlay means `wtype`
types the dictation into it instead of the target window (spec 5.3). That is
enforced in `hypr.rs` by window rules matched on `class:^(openwhisprflow)$`,
including `no_focus`, `pin` and a `move` that parks the window bottom-centre.

Every window of a Tauri app shares one class. A settings window created inside
the existing overlay app would therefore inherit `no_focus` and be unable to
accept a single keystroke — a settings form you cannot type into. The
alternative, narrowing the overlay rules to match on window *title*, works but
costs the user a re-paste of their compositor config.

So: a second Tauri app, `openwhisprflow-settings`, whose window class does not
match the existing rules at all. Nothing in the user's Hyprland config changes.

The class must be **verified empirically** (`hyprctl clients -j`) once the app
runs, not assumed from `productName` — see `hypr.rs`'s own doc comment on why
rules in this project are checked against the running machine rather than
against documentation.

Like the overlay, the settings app does **not** depend on `owf-core`: it would
inherit `sherpa-onnx`, `cpal` and `rubato` for a form. It is a socket client.

## Protocol additions

Three requests, in `owf-core/src/proto.rs`, mirrored per invariant 3 where
applicable:

| Request | Response |
|---|---|
| `GetConfig` | the whole `Config` as JSON, plus the config file's path |
| `SetConfig { config }` | applied / rejected, plus what needs a restart |
| `ListInputDevices` | one entry per input device: name plus a default flag |

`Config` already derives `Serialize`/`Deserialize`, so the wire form is
`serde_json::to_value(&config)` — the settings app never needs the Rust type,
and there is no fourth hand-maintained copy of the config schema.

`ListInputDevices` runs `cpal` enumeration **on a worker thread with a
timeout**. Invariant 6 exists because a hung subprocess previously wedged the
daemon's single-threaded accept loop forever; device enumeration on a sick ALSA
stack is the same hazard by a different route.

## Writing the config

`SetConfig` must not destroy the comments in a file the user is also invited to
edit by hand. Serializing `Config` back with `toml::to_string` would flatten
every comment and reorder every section, so the write path is:

1. Parse the existing file into a `toml_edit::DocumentMut`.
2. Set each incoming leaf value in place, creating tables as needed. Existing
   comments, ordering and whitespace survive.
3. Render the document to a string and parse it with `Config::from_str` —
   the same validation the daemon boots with. **A config that would not load is
   rejected here, before anything is written.**
4. Write atomically: temp file in the same directory, then rename. A crash
   mid-write must not leave a truncated config, because a truncated config means
   a daemon that will not start.

`toml_edit` is already in `Cargo.lock` as a transitive dependency of `toml`, so
making it a direct dependency adds no node to the graph.

Known limitation to document: comments written *inside* an array (`terms`,
`replacements`) are lost when the GUI rewrites that array, because the array is
replaced wholesale. Comments around it survive.

## What takes effect when

Not every setting is live-reloadable, and the GUI must not imply otherwise.
The daemon owns that classification — computing it in TypeScript would be a
second copy of a rule that already exists in `Pipeline::update_reloadable`.

- **Live** — read off `cfg` per utterance: `[guardrail]`, `[inject]`,
  `[style_default]`, `[style_rules]`, `[vocabulary]`, `[debug]`,
  `audio.vad_padding_ms`.
- **Live via a rebuilt recorder** — `audio.device`, `audio.max_seconds`. These
  are baked into the `Recorder` at construction, but the daemon caches that
  recorder in a slot it can clear (`ensure_recorder`). Clearing the slot while
  IDLE means the next dictation builds a recorder on the new device. The
  microphone is the single most-changed setting here; "restart the daemon"
  would be a poor answer for it.
- **Restart required** — `[asr]` (model load) and `[normalize]` (llama-server).
  `update_reloadable` already refuses `normalize.enabled` outright rather than
  reporting a success it cannot deliver.

`SetConfig`'s response therefore carries `restart_required` and a human-readable
reason, computed by diffing old against new. The GUI shows a banner; it does not
decide.

## The settings app

- `settings-tauri/` — new workspace member, package `openwhisprflow-settings`.
  Own `tauri.conf.json`: normal decorated window, focusable, resizable.
- Frontend: a second Vite entry point (`settings.html` → `src/settings.tsx`),
  so one `bun run build` produces both pages and there is one toolchain.
- Form: rendered from the JSON the daemon sent. A label/help map keyed by
  config path supplies German field labels; a path with no entry renders with a
  generic editor keyed off the JSON value's type. Adding a config key can
  therefore never make it GUI-unreachable, only unlabelled.
- Bespoke editors where a generic one would be miserable: a dropdown for
  `audio.device` populated from `ListInputDevices`, a list editor for
  `vocabulary.terms`, a two-column table for `vocabulary.replacements`,
  dropdowns for the style enums.

## Launching it

`owf-ctl settings` spawns `openwhisprflow-settings`, found on `PATH` the same
way the daemon and the overlay already are. The window is an ordinary
application window, so a `.desktop` entry or a compositor keybinding works too;
neither is required.

## Testing

- `route(&["settings"])` joins the compatibility table in `owf-cli/src/lib.rs`.
- Config write: round-trip tests over a fixture with comments — comments
  survive, a rejected config leaves the file byte-identical, an array is
  replaced wholesale, the temp file is cleaned up.
- `restart_required` is computed from a diff: one test per classification row
  above, so the table in this document is executable rather than prose.
- Protocol: the new requests join `requests_serialise_to_the_documented_wire_form`
  and the round-trip test in `proto.rs`.
- Device enumeration honours its timeout.

## Non-goals

- Editing `models.lock.toml` or triggering model downloads from the GUI.
  `owf-ctl setup` owns that and prints a prerequisite report the GUI has no way
  to reproduce faithfully.
- Live-editing `[overlay]`. Those fields are parsed but not wired to anything
  (see `OverlayConfig`'s doc comment); exposing them would promise placement
  control that `xdg_shell` does not give a client.
- A settings tray icon.

## Deviations found during implementation

Two things in the design above turned out to be wrong when measured. Both are
recorded here rather than quietly fixed, because both were reasonable-sounding
ideas that the machine disproved.

**The `openable` probe was removed.** The design called for probing each device
by building a throwaway stream, on the reasoning that `cpal`'s advertised
capabilities are untrustworthy. Measured from inside the daemon, the probe
returned `false` for *every* device — including the one dictation was working
on — because the daemon holds its own recorder open and a second stream on the
same device from the same process fails. The same probe run from a standalone
process returned `true` for all of them. A flag that is wrong precisely for the
device the user is currently using would have greyed out their working
microphone, so there is no flag: a device that cannot be opened fails at the
next dictation, with the error the daemon already surfaces.

**The config writer only writes what changed.** The design said the GUI sends
the whole config and the writer merges it in. It does — but writing every leaf
materialised every defaulted key on the first save: a hand-written ten-line
`config.toml` came back as forty lines, and a top-level `style_rules = []` was
hoisted *above* the file's own header comment, because TOML requires bare keys
to precede the first table. The writer now compares each incoming leaf against
the file's current effective value and skips the ones that match. Saving
without changing anything leaves the file byte-identical; changing one setting
produces a one-line diff.

Two further facts, confirmed rather than corrected:

- The settings window's class really is `openwhisprflow-settings`
  (`hyprctl clients`), which does not match the overlay's
  `class:^(openwhisprflow)$` rules. No compositor config changes.
- The device enumeration needs shaping, not just listing: the host default
  reports itself as `Default Audio Device`, a name absent from
  `input_devices()`, and four distinct devices came back sharing one name.
