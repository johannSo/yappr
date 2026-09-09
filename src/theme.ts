/// Which palette both windows draw themselves in, and the one place `system`
/// stops being a possibility and becomes a palette.
///
/// The names here are `Theme`'s in `crates/yappr-core/src/config.rs`, spelled
/// identically on purpose: one string is the `config.toml` value, the
/// `data-theme` attribute, and the selector in `palettes.css`. Three
/// hand-maintained copies of that list exist -- the Rust enum, this table,
/// and `settings/schema.ts`'s `ENUMS["ui.theme"]` -- so they are checked
/// mechanically rather than trusted: `themes_are_declared_everywhere_they_
/// have_to_be` in `src-tauri` reads this file and `palettes.css` and fails on
/// drift, the same trick the overlay-event fixture uses.
///
/// The value is *which appearance a palette is*, which CSS cannot be asked:
/// there is no selector for "the theme whose `color-scheme` is dark". Rules
/// that depend on light-vs-dark rather than on which palette is chosen are
/// keyed on `[data-appearance]`, and this is where that attribute comes from.
export const APPEARANCE = {
  "yappr-light": "light",
  "yappr-dark": "dark",
  "catppuccin-latte": "light",
  "catppuccin-mocha": "dark",
  "tokyo-night-day": "light",
  "tokyo-night-night": "dark",
} as const;

export type ThemeName = keyof typeof APPEARANCE;

/// What `[ui] theme` can hold: a palette, or the instruction to follow the
/// desktop. `system` never reaches the DOM -- see `resolve`.
export type ConfiguredTheme = ThemeName | "system";

const DARK = "(prefers-color-scheme: dark)";

function prefersDark(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia(DARK).matches;
}

/// `system` and anything unrecognised become the pair the app has always
/// shipped. The second half of that matters more than it looks: every colour
/// in both windows now comes from a `[data-theme="..."]` block, so a name no
/// palette matches is not a wrong-looking window, it is an *unpainted* one.
/// Rust validates the value too (an unknown theme is a load error, not a
/// silent default), so this is the belt to that braces -- reached by a
/// frontend running against a daemon that knows a theme it does not.
function resolve(theme: ConfiguredTheme): ThemeName {
  if (theme in APPEARANCE) return theme as ThemeName;
  return prefersDark() ? "yappr-dark" : "yappr-light";
}

/// The last value the daemon told us about, kept so a `prefers-color-scheme`
/// change knows whether it is still entitled to act.
let configured: ConfiguredTheme = "system";

export function applyTheme(theme: ConfiguredTheme): void {
  configured = theme;
  const name = resolve(theme);
  const root = document.documentElement;
  root.dataset.theme = name;
  root.dataset.appearance = APPEARANCE[name];
}

/// Applied at import time -- before React mounts, and before anything has
/// painted -- so `data-theme` is never absent while there is something on
/// screen. The configured value arrives afterwards, over IPC (`get_config`
/// for the settings window, the `theme` command for the overlay) and on the
/// `theme-changed` event; until it does, and if those calls fail outright,
/// both windows look exactly as they did before themes existed.
applyTheme("system");

/// `system` has to keep following the desktop after the fact, and only while
/// it is what was chosen: someone who pinned Mocha did so precisely to stop
/// the sun coming up from changing their app.
if (typeof window.matchMedia === "function") {
  window.matchMedia(DARK).addEventListener("change", () => {
    if (configured === "system") applyTheme("system");
  });
}
