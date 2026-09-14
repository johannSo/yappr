# Task prompt: add a `libei` injection backend to yappr

Copy everything below the line into a fresh Claude Code session in this repo.

---

Add a fifth injection backend to yappr, `InjectBackend::Libei`, that presses the paste
chord through the **XDG Desktop Portal RemoteDesktop interface** instead of shelling out
to `ydotool`. Read `CLAUDE.md` first — every invariant it lists applies here, especially
1 (a transcribed utterance is never lost), 2 (the overlay must never take focus), 4
(`deny_unknown_fields`) and 9 (`config.toml` is the app's file).

## Why

`ydotool` is the only backend that works on GNOME/Mutter, and it costs the user a
`ydotoold` daemon with write access to `/dev/uinput`, listening on a socket path the
client guesses (`$YDOTOOL_SOCKET`, else `$XDG_RUNTIME_DIR/.ydotool_socket`). yappr
starts from the tray or a `.desktop` entry and inherits no shell export, so a daemon
started anywhere else breaks every paste and reports it on *stdout* with exit 2 — the
whole story is in `inject::diagnostic`'s doc comment and the "A failed backend may say
why on stdout" gotcha in `CLAUDE.md`. The portal needs no daemon, no `/dev/uinput`, no
socket path and no group membership, and it returns real errors over D-Bus instead of a
string on the wrong stream.

## Target environment (measured on this machine, 2026-09-12)

- Fedora 44, GNOME Shell 50, `XDG_SESSION_TYPE=wayland`.
- `/usr/share/xdg-desktop-portal/portals/gnome.portal` exports
  `org.freedesktop.impl.portal.RemoteDesktop`.
- `libei-1.5.0` is installed (`/lib64/libei.so.1`), so the EIS path is available too.
- Crates: `ashpd` 0.13 (portal wrapper over zbus), `reis` 0.7 (pure-Rust libei/libeis
  protocol, no C dependency) if the EIS path is needed.

## Portability — what this backend costs on other distros

Nothing is linked. `reis` and `ashpd` are pure Rust, so no `libei.so` is needed at
runtime, there is no system package to add to README's install lists, and the
`ubuntu:22.04` distrobox AppImage build is unaffected. Contrast `gtk-layer-shell`,
which is a build-time link dependency and fails `cargo build` outright when missing.

What varies is the **portal backend**, i.e. the compositor, not the distro:

| Desktop | `NotifyKeyboardKeysym` (phase 1) | `ConnectToEIS` (phase 2) |
|---|---|---|
| GNOME (xdg-desktop-portal-gnome) | yes, long-standing | GNOME 45+ / xdp 1.18+ |
| KDE (xdg-desktop-portal-kde) | yes, Plasma 5.27+ | Plasma 6+ |
| Hyprland (xdg-desktop-portal-hyprland) | implements RemoteDesktop — verify | uncertain — verify |
| sway / river / Wayfire (xdg-desktop-portal-wlr) | **no RemoteDesktop impl at all** | no |

sway is the only gap and it costs nothing: `wtype` works natively on wlroots, which is
why it is the default backend. By distro that reduces to a version question — Arch
(rolling) and Fedora 40+ get both paths; Ubuntu 24.04+ and Debian 13 get both; Ubuntu
22.04 (GNOME 42, xdp 1.14) and Debian 12 (xdp 1.16) get phase 1 only.

**This inverts the usual reading of the two phases.** Phase 1 is the *more* portable
path, not the fallback: `NotifyKeyboardKeysym` reaches back years on GNOME and KDE
alike, while EIS needs a 2023-or-later stack. The case for phase 2 is longevity —
upstream keeps steering callers toward `ConnectToEIS` — not reach. Whichever ships
first, the backend must degrade to the clipboard fallback (invariant 1) on a desktop
whose portal answers neither, and must say which one it found in the probe example's
output.

Measured on the development machine, 2026-09-12: xdg-desktop-portal 1.21.1,
xdg-desktop-portal-gnome 50.0, mutter 50.0. `busctl --user introspect
org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop
org.freedesktop.portal.RemoteDesktop` lists **both** `.NotifyKeyboardKeysym` and
`.ConnectToEIS`, `version 2`, `AvailableDeviceTypes 7`. So the phase-1 gate above is
half-answered already — the method exists; what is unmeasured is whether Mutter *acts*
on it for a keyboard-only session with no linked ScreenCast.

## Design decisions already made — do not relitigate these

1. **Clipboard + chord, not synthesized text.** `LibeiInjector` does exactly what
   `YdotoolInjector` does: `ClipboardInjector.inject(...)`, sleep `PASTE_SETTLE`, then
   press one Ctrl+V (Ctrl+Shift+V per `wants_shift`). Reuse `wants_shift`,
   `paste_chord` and `terminal_classes` unchanged. Typing the transcript key by key
   reintroduces the layout problem `InjectBackend::Ydotool`'s doc comment describes.
2. **Phase 1 is `RemoteDesktop.NotifyKeyboardKeysym`, not EIS.** ashpd wraps it; it is
   ~100 lines and no libei dependency at all, and because the compositor resolves a
   *keysym* to a keycode in the current keymap it is layout-independent in a way
   `ydotool key`'s raw keycodes are not. Send `XK_Control_L` (0xffe3) press,
   `XK_Shift_L` (0xffe1) press when shifted, `XK_v` (0x76) press+release, then release
   in reverse order.
   **Verify first** whether Mutter 50 still honours `NotifyKeyboardKeysym` — upstream
   xdg-desktop-portal has been steering callers toward `ConnectToEIS`, and if the
   Notify methods are inert here the whole phase is dead. Test it with a throwaway
   `busctl`/ashpd snippet *before* writing the backend.
3. **Phase 2, only if phase 1 is inert:** `ConnectToEIS` → `reis::ei::Context` →
   bind `ei_keyboard` on the seat → `key(keycode, press)` + `frame(serial, time_usec)`.
   libei keycodes are raw evdev codes (`KEY_LEFTCTRL` 29, `KEY_LEFTSHIFT` 42, `KEY_V`
   47), i.e. XKB keycodes minus 8. The device's keymap arrives as an xkb fd on
   `ei_keyboard.keymap`; looking `v` up in it rather than hardcoding 47 is a
   refinement, not a requirement — note it in a comment either way.
4. **The session is created once and reused, and it is created *outside* a dictation.**
   A portal approval dialog per utterance is unusable, and the dialog takes keyboard
   focus, which collides with invariant 2 head-on if it opens while the overlay is up
   and the pipeline is at `INJECTING`. Establish the session eagerly — at server start
   when this backend is selected, or at `ptt-start` at the latest — and hold it in a
   `Mutex<Option<...>>` on the injector, reconnecting if it has died. Persist the
   portal's `restore_token` (`PersistMode::ExplicitlyRevoked`) to a file in
   `paths::state_dir()` so approval survives a restart and the user is asked once,
   ever. That file is the reason this backend is usable at all; if the token round-trip
   does not work, say so plainly rather than shipping a dialog-per-dictation.
5. **Async is confined to one thread, the way `gnome.rs` already does it.**
   `yappr-core` is synchronous and `server.rs` must stay that way. zbus is already in
   the tree via `atspi` (async-io flavour, deliberately *not* tokio) with `futures-lite`
   for `block_on`. Add `ashpd` so it resolves the *same* zbus major — check with
   `cargo tree -d -p yappr-core | grep -i zbus` and make it fail loudly in review if a
   second copy appears. Do not add tokio.
6. **Never block forever.** There is no subprocess here, so `procutil::run_with_timeout`
   does not apply, but its rule does: every portal round-trip gets a timeout, and a
   timeout maps onto `InjectError::Timeout { backend: "libei", .. }` so the clipboard
   fallback carries the transcript (invariant 1).

## Every place that has to change

The compiler will find some of these; the TypeScript ones it will not.

- `crates/yappr-core/src/config.rs` — the `InjectBackend::Libei` variant, with a doc
  comment in the register the neighbouring variants use (what it costs, what it buys,
  when to choose it). Note in the commit message that an old yappr reading a new
  `config.toml` fails on the unknown *value*; that is accepted, not a bug to work
  around.
- `crates/yappr-core/src/inject.rs` — `LibeiInjector`, the `build()` arm, and its
  `name()` (`"libei"`, which is what `debug.rs` and the notification text record).
- `src-tauri/src/wizard.rs:~205` — the `match c.inject.backend` that maps the variant to
  a wire string. Non-exhaustive, so it will not compile until you touch it.
- `src/settings/schema.ts` — `ENUMS["inject.backend"]`, `ENUM_LABELS`, the German help
  text at ~line 189 (which names all the backends and is the row a search for "chord"
  lands on), and **`DEPENDENT_FIELDS`**: `inject.paste_chord` and
  `inject.terminal_classes` are currently gated on `is: ["ydotool"]` and must become
  `["ydotool", "libei"]`, or the new backend's two settings are invisible. Read that
  table's doc comment before editing it — the bar for hiding a real setting is stated
  there and this change clears it.
- `src/settings/wizard.tsx:~539` — the backend chooser's German copy.
- `src-tauri/src/setup.rs` — there is no binary to check for, which is the point. If you
  add a readiness check at all it is "does the RemoteDesktop portal answer", and it must
  be **optional** for the same reason `ydotool` is (see
  `every_desktop_checks_for_ydotool_without_requiring_it` and the doc comment above the
  check list): a backend nobody is obliged to choose must never make the install report
  itself unready.
- `crates/yappr-core/src/config_write.rs` — the annotated-default fixture and its test.
- Docs: `CLAUDE.md` (the architecture blurb at the top, the window-class gotcha's table
  of consequences, the injection notes), `README.md`'s setup section, `docs/HANDOVER.md`
  under what is and is not verified on real hardware.

## Deliverables

- The backend, behind `[inject] backend = "libei"`. `wtype` stays the default.
- `cargo run -p yappr-core --example libei_probe -- "hallo welt"` — a no-microphone
  exercise modelled on `examples/script_probe.rs`: builds the injector, injects the
  argument, prints which path it took (Notify vs EIS), whether the restore token was
  reused or an approval dialog appeared, and what it failed with. `script_probe`'s own
  header explains why this kind of example exists; match its warning that it presses
  real keys into the focused window and replaces the clipboard.
- Tests named as full sentences, in `inject.rs`'s test module. The chord decision is
  already covered by `wants_shift`'s tests — extend them to the new backend rather than
  duplicating. Anything you cannot test without a live portal gets a
  `#[ignore]`d test or, if it is a documented limitation, a `known_limitation_` one.
- `cargo test --workspace && cargo test --workspace -- --ignored --test-threads=1 &&
  cargo clippy --workspace --all-targets` all clean. The `--ignored` half is not
  optional — it is the only thing that catches the C++ ABI breakage described at the
  bottom of `CLAUDE.md`, and adding a dependency is exactly when to run it.

## What not to do

- Do not remove or deprecate the `ydotool` backend. It was retired on 2026-09-09 and
  restored on 2026-09-11 because it is the one configuration verified end to end on
  real hardware, and removing it cost the user it worked for their working setup.
  `CLAUDE.md` tells that story at length; this backend earns the default by being
  verified, not by being newer.
- Do not make the chord "smarter". An unknown window class is unknown; the answer is
  `paste_chord`'s forced arm. The same silent-no-text hole applies here as under
  `ydotool` — `LibeiInjector` must emit the same warning on the `None` + `Auto`
  combination, for the same reason.
- Do not record from the microphone. Verify with `--replay`, the probe example and the
  unit tests.
- Do not touch the user's desktop configuration (invariant 14).

## Report back

State plainly which phase you landed (Notify or EIS), whether the restore token
actually suppressed the second dialog, and what is verified on hardware versus what is
only compiled and unit-tested.
