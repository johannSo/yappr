/// Presentation tables for the settings window.
///
/// Nothing here restates the config schema — the config arrives from the
/// daemon as plain JSON and leaves the same way. These tables only decide how
/// a key that already exists is *shown*: which pane it lives in, what it is
/// called, whether it deserves an editor better than a text box.
///
/// The consequence worth protecting: a config key added in Rust and not
/// mentioned here still appears in this window, rendered by its JSON type, in
/// its section's pane — or in the catch-all pane if `CATEGORIES` has never
/// heard of its section. A new setting can become unlabelled; it cannot
/// become unreachable.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Section = { [key: string]: Json };

export type Category = {
  id: string;
  title: string;
  icon: string;
  /** Config sections shown in this pane, in order. */
  sections: string[];
};

/// The five panes of the sidebar. Sections are grouped by what a user is
/// trying to change, not by which Rust struct they live in — `audio` and
/// `inject` are both "the mechanics of one dictation", however far apart they
/// sit in the pipeline.
export const CATEGORIES: Category[] = [
  { id: "general", title: "General", icon: "sliders", sections: ["audio", "inject"] },
  { id: "language", title: "Language", icon: "waveform", sections: ["asr", "vocabulary"] },
  { id: "style", title: "Style", icon: "pen", sections: ["style_default", "style_rules"] },
  { id: "advanced", title: "Advanced", icon: "gear", sections: ["models", "normalize", "guardrail"] },
  { id: "appearance", title: "Appearance", icon: "palette", sections: ["ui"] },
  { id: "diagnostics", title: "Diagnostics", icon: "pulse", sections: ["debug", "overlay"] },
];

/// The pane that catches sections no `CATEGORIES` entry names. Rendered only
/// when it would have something in it, so it stays invisible until a future
/// config section makes it necessary — at which point that section is still
/// reachable rather than silently dropped.
export const CATCH_ALL: Category = {
  id: "other",
  title: "Other",
  icon: "dots",
  sections: [],
};

export const SECTION_TITLES: Record<string, string> = {
  audio: "Microphone & recording",
  vocabulary: "Vocabulary",
  style_default: "Style",
  style_rules: "Per-window style rules",
  models: "Models & memory",
  normalize: "Post-processing",
  inject: "Text entry",
  asr: "Speech recognition",
  guardrail: "Guardrail thresholds",
  debug: "Diagnostics",
  overlay: "Overlay",
  ui: "Colours & theme",
};

/**
 * Sections the daemon may not be able to apply without a restart — see
 * `restart_reason`, which is where the rule actually lives (in Rust, so this
 * cannot drift into deciding anything). "May not" because since the lazy
 * model lifecycle the answer depends on whether the models are resident when
 * the save lands: unloaded, `ensure_models_loaded` reads the new config off
 * disk on the next press and no restart is needed. This Set only decides
 * whether to mark the section in the GUI, never whether a restart happens.
 */
export const RESTART_SECTIONS = new Set(["asr", "normalize"]);

export const SECTION_NOTES: Record<string, string> = {
  style_rules:
    "The window class is a regular expression; the first matching rule wins. Axes a rule leaves unset inherit from the style above.",
  overlay:
    "Read, but not in control of anything yet: under Wayland a window cannot set its own position — that comes from a compositor rule.",
};

export const LABELS: Record<string, string> = {
  "ui.theme": "Colour scheme",
  "audio.device": "Microphone",
  "audio.max_seconds": "Maximum recording length",
  "audio.vad_padding_ms": "Speech padding",
  "asr.model": "Model",
  "asr.language": "Language",
  "asr.num_threads": "Threads",
  "models.preload_at_startup": "Load models at startup",
  "models.idle_unload_seconds": "Unload models after",
  "normalize.enabled": "Post-processing on",
  "normalize.timeout_ms": "Time limit",
  "normalize.context_size": "Context size",
  "normalize.threads": "Threads",
  "guardrail.min_word_ratio": "Minimum word ratio",
  "guardrail.max_word_ratio": "Maximum word ratio",
  "guardrail.min_overlap_english": "Minimum overlap (English)",
  "guardrail.min_overlap_other": "Minimum overlap (other languages)",
  "guardrail.short_input_words": "\u201cShort input\u201d cutoff",
  "guardrail.ngram_size": "N-gram size",
  "guardrail.ngram_max_repeats": "Maximum n-gram repeats",
  "inject.backend": "Method",
  "inject.script": "Paste script",
  "inject.paste_chord": "Paste shortcut",
  "inject.terminal_classes": "Terminal window classes",
  "inject.trailing_space": "Append a space",
  "inject.keystroke_delay_ms": "Keystroke delay",
  "vocabulary.enabled": "Vocabulary on",
  "vocabulary.terms": "Terms",
  "vocabulary.replacements": "Fixed replacements",
  "vocabulary.max_error_ratio": "Allowed deviation",
  "vocabulary.min_term_chars": "Minimum length for fuzzy matches",
  "style_default.styling": "Tone",
  "style_default.structure": "Structure",
  "style_default.context": "Context",
  "debug.enabled": "Diagnostics on",
  "debug.dir": "Directory",
  "debug.save_audio": "Save audio too",
  "overlay.position": "Position",
  "overlay.width": "Width",
  "overlay.height": "Height",
  // Not a `config.toml` key — see `Settings.tsx`'s `AutostartCard` and
  // `settings_cmds.rs`'s module doc for why this is filesystem state
  // (`~/.config/autostart/yappr.desktop` existing or not) rather
  // than a section here. The path `"autostart.enabled"` exists only so this
  // row can borrow the same `LABELS`/`HELP` lookup every config row uses.
  "autostart.enabled": "Start at login",
};

/// Rendered after the input rather than inside the label, so a row reads
/// "Time limit  [6000] ms" instead of putting the unit in a parenthesis the
/// eye has to jump back to.
export const UNITS: Record<string, string> = {
  "audio.max_seconds": "s",
  "audio.vad_padding_ms": "ms",
  "models.idle_unload_seconds": "s",
  "normalize.timeout_ms": "ms",
  "normalize.context_size": "tokens",
  "inject.keystroke_delay_ms": "ms",
  "guardrail.short_input_words": "words",
  "vocabulary.min_term_chars": "characters",
  "overlay.width": "px",
  "overlay.height": "px",
};

/// One or two sentences behind each row's ⓘ. Deliberately near-complete: the
/// info button only reads as part of the layout if nearly every row has one,
/// and a threshold like `min_overlap_other` is not a setting anyone can guess
/// the meaning of from its name.
export const HELP: Record<string, string> = {
  "audio.device":
    "\u201cSystem default\u201d takes whatever the system currently reports as its default input. Whether a device actually works shows up on the next dictation \u2014 the daemon reports an error if it cannot open it.",
  "audio.max_seconds":
    "A recording stops by itself after this long, so a forgotten one cannot keep the microphone open indefinitely.",
  "audio.vad_padding_ms":
    "How much audio either side of the detected speech is kept. Too little clips the starts of words, too much feeds silence into recognition.",
  "asr.model":
    "Which model recognises the spoken text. Parakeet TDT v3 is multilingual and the default. primeline Parakeet understands German only, but recognises it far more accurately than anything else here \u2014 the best choice if you dictate in German only. Parakeet Unified understands English only, but recognises it more accurately. Nemotron 3.5 is multilingual. Switching downloads about 500 MB once; models already on disk are left alone, so switching back needs no new download.",
  "asr.language":
    "For multilingual models only. \u201cauto\u201d lets the model detect the language itself; a code such as \u201cen\u201d fixes it. Parakeet TDT v3 and Parakeet Unified ignore this setting.",
  "asr.num_threads":
    "CPU cores used for speech recognition. More threads shorten the wait, up to the point where the cores are saturated.",
  "models.preload_at_startup":
    "Loads the speech recognition and language models as soon as the app starts. Off means they are loaded on the first keypress instead \u2014 that saves over a gigabyte while idle, at the cost of a one-off wait on the first dictation.",
  "models.idle_unload_seconds":
    "How long after the last dictation the models stay in memory. After that they are unloaded and reloaded on the next keypress. 0 means never unload.",
  "normalize.enabled":
    "Lets the local language model tidy up the recognised text. Off means the text is only cleaned up by rule and inserted straight away.",
  "normalize.timeout_ms":
    "How long to wait for the language model. If the time runs out, the raw recognised text is inserted \u2014 nothing is lost.",
  "normalize.context_size":
    "How much text the language model sees at once. Larger costs memory, smaller truncates long dictations.",
  "normalize.threads": "CPU cores used for post-processing.",
  "guardrail.min_word_ratio":
    "Lower bound on the word count after post-processing relative to before. 0.55 rejects a version that has cut away nearly half.",
  "guardrail.max_word_ratio":
    "Upper bound on the same ratio. Catches a model that has started inventing rather than tidying.",
  "guardrail.min_overlap_english":
    "The share of the original words that must reappear in the reworked version. Lower for English, because post-processing rephrases more there.",
  "guardrail.min_overlap_other":
    "The same threshold for every other language.",
  "guardrail.short_input_words":
    "Up to this length an input counts as short and the ratio checks are skipped \u2014 at three words they say nothing.",
  "guardrail.ngram_size": "Length of the word sequence the loop detection watches for.",
  "guardrail.ngram_max_repeats":
    "How often the same word sequence may occur before the version is rejected as a loop.",
  "inject.backend":
    "wtype types the text into the window character by character and needs no setup \u2014 but it does nothing under GNOME (Mutter). ydotool puts the text on the clipboard and presses Ctrl+V once (Ctrl+Shift+V in terminals); that works where wtype cannot, but it requires a running ydotoold with write access to /dev/uinput. script hands the finished text to a program of your own as its first argument, which then decides for itself how to insert it. clipboard only puts it on the clipboard; pasting is up to you.",
  "inject.script":
    "Path to the program the script method runs. It is given the finished text as its first and only argument ($1) and owns everything after that: clipboard, key chord, window detection. Must be executable; ~ is expanded. Example: ~/bin/paste.sh. If it fails, or nothing is set here, the text lands on the clipboard just as with the clipboard method.",
  "inject.paste_chord":
    "Which key chord the ydotool method presses. auto decides by window class: Ctrl+Shift+V for anything listed as a terminal below, Ctrl+V otherwise. If yappr cannot name the focused window, that becomes a plain Ctrl+V \u2014 which terminals ignore, with no error reported. If you mostly dictate into terminals and nothing arrives, fix this to Ctrl+Shift+V.",
  "inject.terminal_classes":
    "Window classes that count as a terminal and therefore get Ctrl+Shift+V under auto. Case does not matter. Your window's class is in the debug record under window_class.",
  "inject.trailing_space":
    "Appends a space, so the next dictation does not run into the previous one.",
  "inject.keystroke_delay_ms":
    "Pause between two simulated keystrokes. Raise it if a window swallows characters. Only the wtype method types character by character; the others ignore this.",
  "vocabulary.enabled":
    "Corrects technical terms and names in the recognised text, before post-processing ever sees it.",
  "vocabulary.terms":
    "These terms are matched even when slightly misrecognised. Short abbreviations belong in the replacement table instead.",
  "vocabulary.replacements":
    "Replaced exactly as written, with no fuzzy comparison \u2014 the right place for short abbreviations.",
  "vocabulary.max_error_ratio":
    "0.25 allows two wrong characters in an eight-character term. 0 turns fuzzy matching off; fixed replacements stay.",
  "vocabulary.min_term_chars":
    "Shorter terms are matched exactly only. That keeps three-letter words from being replaced by accident.",
  "style_default.styling": "How formal the result should sound.",
  "style_default.structure": "Prose or a list.",
  "style_default.context":
    "What the text is for. \u201cemail\u201d allows a salutation and a sign-off.",
  "debug.enabled":
    "Writes a record for every dictation with the recognised text, the reworked version and timings.",
  "debug.dir": "Directory for those records.",
  "debug.save_audio":
    "Also stores the recorded audio. Needs considerably more space.",
  "overlay.position":
    "Read, but controls nothing \u2014 under Wayland a window cannot set its own position.",
  "ui.theme":
    "Applies to both windows, including the dictation overlay. \u201cSystem\u201d follows the desktop's light/dark setting; any other scheme pins a variant and stays there even when the desktop switches. Takes effect immediately, no restart needed.",
  "overlay.width": "Read, but controls nothing.",
  "overlay.height": "Read, but controls nothing.",
  "autostart.enabled":
    "Creates ~/.config/autostart/yappr.desktop; systemd starts yappr from it automatically at your next login. Off means the file does not exist, and nothing starts by itself.",
};


export const ENUMS: Record<string, string[]> = {
  // Mirrors `Theme::ALL` in `crates/yappr-core/src/config.rs`, in the same
  // order: the shipped pair first, then the two families. One of the three
  // hand-maintained copies of this list -- see `src/theme.ts`'s header, and
  // the drift test that reads all three.
  "ui.theme": [
    "system",
    "yappr-light",
    "yappr-dark",
    "catppuccin-latte",
    "catppuccin-mocha",
    "tokyo-night-day",
    "tokyo-night-night",
  ],
  "asr.model": [
    "parakeet-tdt-v3",
    "parakeet-primeline-de",
    "parakeet-unified-en",
    "nemotron-3.5",
  ],
  "inject.backend": ["wtype", "ydotool", "script", "clipboard"],
  "inject.paste_chord": ["auto", "ctrl_v", "ctrl_shift_v"],
  "style_default.styling": ["casual", "semi-casual", "semi-formal", "formal"],
  "style_default.structure": ["prose", "lists"],
  "style_default.context": ["general", "email"],
  "style_rules.styling": ["casual", "semi-casual", "semi-formal", "formal"],
  "style_rules.structure": ["prose", "lists"],
  "style_rules.context": ["general", "email"],
};

/// How an enum value is *shown*, where the raw value is not the best thing to
/// read. Optional per key and per value: an enum with no entry here renders
/// its raw values, which is right for `wtype` or `parakeet-tdt-v3` -- those
/// are names, and renaming them in the GUI would hide the string the user has
/// to type into `config.toml` or a bug report.
///
/// Themes are the case where it is not right: `catppuccin-mocha` is a slug
/// for a thing with a proper name, and a dropdown of slugs reads like a
/// config file rather than a choice of how the app looks. Nothing is lost by
/// labelling them -- `search()` still matches the raw key and value.
export const ENUM_LABELS: Record<string, Record<string, string>> = {
  "ui.theme": {
    system: "System (follow light/dark)",
    "yappr-light": "yappr Light",
    "yappr-dark": "yappr Dark",
    "catppuccin-latte": "Catppuccin Latte",
    "catppuccin-mocha": "Catppuccin Mocha",
    "tokyo-night-day": "Tokyo Night Day",
    "tokyo-night-night": "Tokyo Night Night",
  },
  "inject.paste_chord": {
    auto: "Automatic (by window class)",
    ctrl_v: "Always Ctrl+V",
    ctrl_shift_v: "Always Ctrl+Shift+V",
  },
};

/// Columns for a table of objects. Needed because an *empty* array carries no
/// keys to infer them from — without this you could never add the first
/// replacement rule, which is exactly the state a new user is in.
export const TABLE_COLUMNS: Record<string, string[]> = {
  "vocabulary.replacements": ["from", "to"],
  style_rules: ["match_class", "styling", "structure", "context"],
};

export const COLUMN_LABELS: Record<string, string> = {
  from: "Heard as",
  to: "Replace with",
  match_class: "Window class",
  styling: "Tone",
  structure: "Structure",
  context: "Context",
};

/// Row order within a section.
///
/// Needed because the config arrives as a `serde_json::Value`, whose object is
/// a `BTreeMap` -- keys reach this window sorted by their *Rust* name, not in
/// the order the struct declares them. Alphabetical-by-Rust-name is close to
/// arbitrary once the labels are prose: it puts `max_word_ratio` above
/// `min_word_ratio` with two unrelated overlap thresholds in between, and
/// sinks a section's master `enabled` switch to third place behind
/// `context_size`. A section reads top to bottom as "what is this, then how
/// does it behave", so that order is stated here rather than inherited.
///
/// Keys that `Config` still accepts but no longer acts on.
///
/// The one and only exception to "nothing here can hide a setting", and it
/// is not really an exception: these are not settings. S1-mini moved
/// in-process, so there is no `llama-server` to give a path to and no port
/// for it to listen on -- but `[normalize]` is `deny_unknown_fields`, so the
/// Rust struct has to keep accepting both keys or every `config.toml`
/// written before that change would be quarantined and its settings reset
/// on the next start (invariant
/// 4). See `NormalizeConfig::port`'s own comment.
///
/// A key belongs here only when the Rust side has documented it as accepted
/// and ignored. A *real* setting must never be added to this set: the
/// property that a setting can become unlabelled but never unreachable is
/// what makes `schema.ts` safe to leave alone when Rust gains a field.
export const OBSOLETE_FIELDS = new Set([
  "normalize.port",
  "normalize.llama_server_path",
  // `inject.paste_chord` and `inject.terminal_classes` were briefly here,
  // between the ydotool backend's retirement on 2026-09-09 and its return on
  // 2026-09-11. They are real settings again, with a real reader, and they
  // are hidden by `DEPENDENT_FIELDS` under the backends that ignore them --
  // which is the table for a setting that is merely inert, not retired.
]);

/// Rows that only exist for one value of another row in the same section.
///
/// The second thing that can hide a row, and unlike `OBSOLETE_FIELDS` it
/// hides a *real* setting -- so the bar is narrow: the row must be inert for
/// every other value of the row it depends on, and that row must be visible
/// right above it, so the way to bring it back is on screen. `inject.script`
/// is the case that forced it: `inject::build` reads it only for
/// `InjectBackend::Script`, so under `wtype` it is a path field that changes
/// nothing, sitting directly under the dropdown that would make it matter.
/// `paste_chord` and `terminal_classes` are the same shape -- only
/// `YdotoolInjector` reads them -- and they are the reason the bar is worth
/// restating: they spent two days in `OBSOLETE_FIELDS` instead, which is
/// where a setting goes to be forgotten rather than merely hidden.
///
/// This does not make a setting unreachable, in either of the two ways that
/// would matter. Choosing the backend brings its rows back; and a search for
/// "script" or "chord" still lands on `inject.backend`, whose help text names
/// all four backends, which is the row you have to change anyway.
export const DEPENDENT_FIELDS: Record<string, { on: string; is: Json[] }> = {
  "inject.script": { on: "backend", is: ["script"] },
  "inject.paste_chord": { on: "backend", is: ["ydotool"] },
  "inject.terminal_classes": { on: "backend", is: ["ydotool"] },
};

/// Whether a row's dependency (if it has one) is currently satisfied.
///
/// A section that does not carry the key being depended on leaves the row
/// visible: this is fed whatever JSON the daemon sent, and a missing
/// `backend` must not quietly take `script` with it -- unlabelled is
/// allowed, unreachable is not.
function applies(section: string, key: string, value: Section): boolean {
  const dep = DEPENDENT_FIELDS[`${section}.${key}`];
  if (!dep) return true;
  const on = value[dep.on];
  if (on === undefined) return true;
  return dep.is.some((want) => jsonEqual(want, on));
}

/// Only an ordering hint: a key missing from this table still renders, after
/// the listed ones. Nothing here can hide a setting -- see
/// `OBSOLETE_FIELDS` and `DEPENDENT_FIELDS` for the two things that can, and
/// why neither is one.
export const FIELD_ORDER: Record<string, string[]> = {
  audio: ["device", "max_seconds", "vad_padding_ms"],
  asr: ["model", "language", "num_threads"],
  inject: [
    "backend",
    "script",
    "paste_chord",
    "terminal_classes",
    "trailing_space",
    "keystroke_delay_ms",
  ],
  normalize: ["enabled", "timeout_ms", "context_size", "threads"],
  guardrail: [
    "min_word_ratio",
    "max_word_ratio",
    "min_overlap_english",
    "min_overlap_other",
    "short_input_words",
    "ngram_size",
    "ngram_max_repeats",
  ],
  vocabulary: ["enabled", "terms", "replacements", "max_error_ratio", "min_term_chars"],
  style_default: ["styling", "structure", "context"],
  debug: ["enabled", "dir", "save_audio"],
  overlay: ["position", "width", "height"],
  ui: ["theme"],
};

/// A section's fields as rows, in `FIELD_ORDER` where it has an opinion and in
/// whatever order the daemon sent for everything else -- so a key added in Rust
/// and named nowhere here lands at the bottom of its section rather than
/// nowhere at all.
///
/// The only filters applied here are `OBSOLETE_FIELDS` and
/// `DEPENDENT_FIELDS`, and `search()` goes through this function precisely so
/// a key hidden from a pane is not still reachable through the search box.
export function orderedFields(section: string, value: Section): [string, Json][] {
  const declared = FIELD_ORDER[section] ?? [];
  const live = (k: string) =>
    !OBSOLETE_FIELDS.has(`${section}.${k}`) && applies(section, k, value);
  const listed = declared.filter((k) => k in value && live(k));
  const rest = Object.keys(value).filter((k) => !declared.includes(k) && live(k));
  return [...listed, ...rest].map((k) => [k, value[k]]);
}

export function labelFor(path: string, key: string): string {
  return LABELS[path] ?? key;
}

export function titleFor(section: string): string {
  return SECTION_TITLES[section] ?? section;
}

/// The sidebar, resolved against a config that actually arrived: categories
/// keep only the sections present, empty ones drop out, and anything the
/// tables have never heard of lands in `CATCH_ALL` rather than nowhere.
export function categorize(present: string[]): Category[] {
  const claimed = new Set(CATEGORIES.flatMap((c) => c.sections));
  const panes = CATEGORIES.map((c) => ({
    ...c,
    sections: c.sections.filter((s) => present.includes(s)),
  })).filter((c) => c.sections.length > 0);

  const orphans = present.filter((s) => !claimed.has(s));
  return orphans.length > 0 ? [...panes, { ...CATCH_ALL, sections: orphans }] : panes;
}

/// Structural equality over the JSON the daemon speaks. Used to decide whether
/// a row still holds its default, which is what makes the reset button appear.
export function jsonEqual(a: Json | undefined, b: Json | undefined): boolean {
  if (a === b) return true;
  if (a === null || b === null || a === undefined || b === undefined) return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    return a.every((v, i) => jsonEqual(v, b[i]));
  }
  if (typeof a === "object" && typeof b === "object") {
    const ka = Object.keys(a);
    const kb = Object.keys(b);
    if (ka.length !== kb.length) return false;
    return ka.every((k) => k in b && jsonEqual(a[k], (b as Section)[k]));
  }
  return false;
}

/// Folds a string down to what a search should compare on: case, diacritics
/// and `ß` all removed. The labels are English, but the *values* are not
/// necessarily — a device name, a vocabulary term or a window class can carry
/// anything — so typing "grosse" still has to find "Größe". A search that only
/// matches what you can already spell exactly is a search nobody uses twice.
export function fold(s: string): string {
  return s
    .toLowerCase()
    .replace(/ß/g, "ss")
    .normalize("NFD")
    .replace(/\p{Diacritic}/gu, "");
}

/// Everything one row can be found by: its label, its raw config key (so
/// `min_overlap_english` finds it even though the window never shows that
/// string), its section's title and raw name, its unit, and its help text.
///
/// The raw key is in there deliberately. Nobody is invited to *edit*
/// `config.toml` any more, but people still read it — in a bug report, or in a
/// daemon error naming a key — and someone arriving from either is searching
/// for the name Rust uses, not the one this window prints.
function rowHaystack(section: string, key: string): string {
  const path = `${section}.${key}`;
  return fold(
    [
      LABELS[path] ?? "",
      key,
      path,
      SECTION_TITLES[section] ?? "",
      section,
      UNITS[path] ?? "",
      HELP[path] ?? "",
    ].join(" "),
  );
}

export function rowMatches(section: string, key: string, needle: string): boolean {
  return rowHaystack(section, key).includes(needle);
}

/// A section that is itself a list (`style_rules`) has no keys to match, and a
/// section whose own name matches should bring all of its rows with it.
export function sectionMatches(section: string, needle: string): boolean {
  return fold(
    [SECTION_TITLES[section] ?? "", section, SECTION_NOTES[section] ?? ""].join(" "),
  ).includes(needle);
}

/// The sections and rows a query leaves standing, in the order the panes
/// would have shown them. Returns `null` for an empty query, which is the
/// caller's signal to render the normal category view instead — search is a
/// lens over this window, not a different window.
export function search(
  config: Section,
  query: string,
): { section: string; keys: string[] | null }[] | null {
  const needle = fold(query.trim());
  if (!needle) return null;

  const ordered = categorize(Object.keys(config)).flatMap((c) => c.sections);
  const hits: { section: string; keys: string[] | null }[] = [];

  for (const section of ordered) {
    const value = config[section];
    const wholeSection = sectionMatches(section, needle);

    // An array section is one indivisible editor; it either matches or it
    // doesn't. `keys: null` means "show this section whole".
    if (Array.isArray(value)) {
      if (wholeSection) hits.push({ section, keys: null });
      continue;
    }
    if (!value || typeof value !== "object") continue;

    const keys = orderedFields(section, value as Section)
      .map(([k]) => k)
      .filter((k) => wholeSection || rowMatches(section, k, needle));
    if (keys.length > 0) hits.push({ section, keys });
  }

  return hits;
}

/// Which pane a section lives in, for the caption under a search hit. Search
/// results are flat, and a result with no home is a result you cannot go back
/// to — this is what lets each hit say where it normally lives.
export function paneTitleOf(section: string): string {
  return CATEGORIES.find((c) => c.sections.includes(section))?.title ?? CATCH_ALL.title;
}
