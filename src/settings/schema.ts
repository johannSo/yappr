/// Presentation tables for the settings window.
///
/// Nothing here restates the config schema — the config arrives from the
/// daemon as plain JSON and leaves the same way. These tables only decide how
/// a key that already exists is *shown*: which pane it lives in, what it is
/// called in German, whether it deserves an editor better than a text box.
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
  { id: "allgemein", title: "Allgemein", icon: "sliders", sections: ["audio", "inject"] },
  { id: "sprache", title: "Sprache", icon: "waveform", sections: ["asr", "vocabulary"] },
  { id: "stil", title: "Stil", icon: "pen", sections: ["style_default", "style_rules"] },
  { id: "erweitert", title: "Erweitert", icon: "gear", sections: ["models", "normalize", "guardrail"] },
  { id: "darstellung", title: "Darstellung", icon: "palette", sections: ["ui"] },
  { id: "diagnose", title: "Diagnose", icon: "pulse", sections: ["debug", "overlay"] },
];

/// The pane that catches sections no `CATEGORIES` entry names. Rendered only
/// when it would have something in it, so it stays invisible until a future
/// config section makes it necessary — at which point that section is still
/// reachable rather than silently dropped.
export const CATCH_ALL: Category = {
  id: "weitere",
  title: "Weitere",
  icon: "dots",
  sections: [],
};

export const SECTION_TITLES: Record<string, string> = {
  audio: "Mikrofon & Aufnahme",
  vocabulary: "Vokabular",
  style_default: "Stil",
  style_rules: "Stilregeln pro Fenster",
  models: "Modelle & Speicher",
  normalize: "Nachbearbeitung",
  inject: "Texteingabe",
  asr: "Spracherkennung",
  guardrail: "Schutzschwellen",
  debug: "Diagnose",
  overlay: "Overlay",
  ui: "Farben & Thema",
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
    "Die Fensterklasse ist ein regulärer Ausdruck; die erste passende Regel gewinnt. Nicht gesetzte Achsen erben aus dem Stil darüber.",
  overlay:
    "Wird eingelesen, steuert aber noch nichts: unter Wayland kann ein Fenster seine eigene Position nicht setzen, die kommt aus einer Compositor-Regel.",
};

export const LABELS: Record<string, string> = {
  "ui.theme": "Farbschema",
  "audio.device": "Mikrofon",
  "audio.max_seconds": "Maximale Aufnahmedauer",
  "audio.vad_padding_ms": "Sprachpuffer",
  "asr.model": "Modell",
  "asr.language": "Sprache",
  "asr.num_threads": "Threads",
  "models.preload_at_startup": "Modelle beim Start laden",
  "models.idle_unload_seconds": "Modelle entladen nach",
  "normalize.enabled": "Nachbearbeitung aktiv",
  "normalize.timeout_ms": "Zeitlimit",
  "normalize.context_size": "Kontextgröße",
  "normalize.threads": "Threads",
  "guardrail.min_word_ratio": "Minimales Wortverhältnis",
  "guardrail.max_word_ratio": "Maximales Wortverhältnis",
  "guardrail.min_overlap_english": "Mindestüberlappung (Englisch)",
  "guardrail.min_overlap_other": "Mindestüberlappung (andere Sprachen)",
  "guardrail.short_input_words": "Grenze „kurze Eingabe“",
  "guardrail.ngram_size": "N-Gramm-Größe",
  "guardrail.ngram_max_repeats": "Maximale N-Gramm-Wiederholungen",
  "inject.backend": "Verfahren",
  "inject.script": "Einfüge-Skript",
  "inject.trailing_space": "Leerzeichen anhängen",
  "inject.keystroke_delay_ms": "Tastenverzögerung",
  "vocabulary.enabled": "Vokabular aktiv",
  "vocabulary.terms": "Begriffe",
  "vocabulary.replacements": "Feste Ersetzungen",
  "vocabulary.max_error_ratio": "Zulässige Abweichung",
  "vocabulary.min_term_chars": "Mindestlänge für unscharfe Treffer",
  "style_default.styling": "Tonfall",
  "style_default.structure": "Struktur",
  "style_default.context": "Kontext",
  "debug.enabled": "Diagnose aktiv",
  "debug.dir": "Verzeichnis",
  "debug.save_audio": "Audio mitschreiben",
  "overlay.position": "Position",
  "overlay.width": "Breite",
  "overlay.height": "Höhe",
  // Not a `config.toml` key — see `Settings.tsx`'s `AutostartCard` and
  // `settings_cmds.rs`'s module doc for why this is filesystem state
  // (`~/.config/autostart/yappr.desktop` existing or not) rather
  // than a section here. The path `"autostart.enabled"` exists only so this
  // row can borrow the same `LABELS`/`HELP` lookup every config row uses.
  "autostart.enabled": "Beim Anmelden starten",
};

/// Rendered after the input rather than inside the label, so a row reads
/// "Zeitlimit  [6000] ms" instead of putting the unit in a parenthesis the
/// eye has to jump back to.
export const UNITS: Record<string, string> = {
  "audio.max_seconds": "s",
  "audio.vad_padding_ms": "ms",
  "models.idle_unload_seconds": "s",
  "normalize.timeout_ms": "ms",
  "normalize.context_size": "Token",
  "inject.keystroke_delay_ms": "ms",
  "guardrail.short_input_words": "Wörter",
  "vocabulary.min_term_chars": "Zeichen",
  "overlay.width": "px",
  "overlay.height": "px",
};

/// One or two sentences behind each row's ⓘ. Deliberately near-complete: the
/// info button only reads as part of the layout if nearly every row has one,
/// and a threshold like `min_overlap_other` is not a setting anyone can guess
/// the meaning of from its name.
export const HELP: Record<string, string> = {
  "audio.device":
    "„Systemstandard“ nimmt, was das System gerade als Standardeingang meldet. Ob ein Gerät wirklich funktioniert, zeigt sich beim nächsten Diktat — der Daemon meldet einen Fehler, wenn er es nicht öffnen kann.",
  "audio.max_seconds":
    "Danach bricht die Aufnahme von selbst ab, damit eine hängengebliebene Taste nicht endlos mitschneidet.",
  "audio.vad_padding_ms":
    "Wie viel Ton vor und nach der erkannten Sprache erhalten bleibt. Zu wenig schneidet Wortanfänge ab, zu viel nimmt Stille mit in die Erkennung.",
  "asr.model":
    "Welches Modell den gesprochenen Text erkennt. Parakeet TDT v3 ist mehrsprachig und die Voreinstellung. primeline Parakeet versteht nur Deutsch, erkennt es aber deutlich genauer als alle anderen hier — die beste Wahl, wenn du nur auf Deutsch diktierst. Parakeet Unified versteht nur Englisch, erkennt es aber genauer. Nemotron 3.5 ist mehrsprachig. Ein Wechsel lädt einmalig rund 500 MB herunter; bereits geladene Modelle bleiben liegen, ein Zurückwechseln geht also ohne erneuten Download.",
  "asr.language":
    "Nur für mehrsprachige Modelle. „auto“ lässt das Modell die Sprache selbst erkennen; ein Kürzel wie „de“ legt sie fest. Parakeet TDT v3 und Parakeet Unified ignorieren diese Einstellung.",
  "asr.num_threads":
    "Rechenkerne für die Spracherkennung. Mehr Threads verkürzen die Wartezeit, bis die Kerne ausgelastet sind.",
  "models.preload_at_startup":
    "Lädt Spracherkennung und Sprachmodell schon beim Programmstart. Aus heißt: sie werden erst beim ersten Tastendruck geladen — das spart im Leerlauf über ein Gigabyte, kostet aber beim ersten Diktat einmalig Wartezeit.",
  "models.idle_unload_seconds":
    "So lange nach dem letzten Diktat bleiben die Modelle im Speicher. Danach werden sie entladen und beim nächsten Tastendruck neu geladen. 0 heißt: nie entladen.",
  "normalize.enabled":
    "Lässt das lokale Sprachmodell den erkannten Text glätten. Aus heißt: der Text wird nur nach Regeln bereinigt und sofort eingefügt.",
  "normalize.timeout_ms":
    "Wie lange auf das Sprachmodell gewartet wird. Läuft die Zeit ab, wird der reine Erkennungstext eingefügt — verloren geht nichts.",
  "normalize.context_size":
    "Wie viel Text das Sprachmodell auf einmal sieht. Größer kostet Speicher, kleiner schneidet lange Diktate ab.",
  "normalize.threads": "Rechenkerne für die Nachbearbeitung.",
  "guardrail.min_word_ratio":
    "Untergrenze für die Wortzahl nach der Nachbearbeitung im Verhältnis zu davor. 0,55 verwirft eine Fassung, die fast die Hälfte weggekürzt hat.",
  "guardrail.max_word_ratio":
    "Obergrenze im selben Verhältnis. Fängt ein Modell ab, das anfängt zu dichten statt zu glätten.",
  "guardrail.min_overlap_english":
    "Anteil der ursprünglichen Wörter, die in der überarbeiteten Fassung wieder vorkommen müssen. Für Englisch niedriger, weil die Nachbearbeitung dort mehr umformt.",
  "guardrail.min_overlap_other":
    "Dieselbe Schwelle für alle anderen Sprachen, Deutsch eingeschlossen.",
  "guardrail.short_input_words":
    "Bis zu dieser Länge gilt eine Eingabe als kurz und die Verhältnisprüfungen greifen nicht — bei drei Wörtern sagen sie nichts aus.",
  "guardrail.ngram_size": "Länge der Wortfolge, auf die die Schleifenerkennung achtet.",
  "guardrail.ngram_max_repeats":
    "Wie oft dieselbe Wortfolge vorkommen darf, bevor die Fassung als Schleife verworfen wird.",
  "inject.backend":
    "wtype tippt den Text Zeichen für Zeichen ins Fenster und braucht keine Einrichtung — unter GNOME (Mutter) bewirkt es allerdings nichts. script übergibt den fertigen Text als erstes Argument an ein eigenes Programm, das dann selbst entscheidet, wie es ihn einfügt. clipboard legt ihn nur in die Zwischenablage, einfügen musst du selbst.",
  "inject.script":
    "Pfad zu dem Programm, das das script-Verfahren aufruft. Es bekommt den fertigen Text als erstes und einziges Argument ($1) und ist danach für alles zuständig: Zwischenablage, Tastenkombination, Fenstererkennung. Muss ausführbar sein; ~ wird aufgelöst. Beispiel: ~/bin/paste.sh. Schlägt es fehl oder ist hier nichts eingetragen, landet der Text wie beim clipboard-Verfahren in der Zwischenablage.",
  "inject.trailing_space":
    "Hängt ein Leerzeichen an, damit das nächste Diktat nicht am vorherigen klebt.",
  "inject.keystroke_delay_ms":
    "Pause zwischen zwei simulierten Tastenanschlägen. Höher setzen, wenn ein Fenster Zeichen verschluckt. Nur das wtype-Verfahren tippt Zeichen für Zeichen; die anderen ignorieren das.",
  "vocabulary.enabled":
    "Korrigiert Fachbegriffe und Namen schon im Erkennungstext, bevor die Nachbearbeitung ihn zu sehen bekommt.",
  "vocabulary.terms":
    "Diese Begriffe werden auch bei leichter Fehlerkennung getroffen. Kurze Abkürzungen gehören stattdessen in die Ersetzungstabelle.",
  "vocabulary.replacements":
    "Wird genau so ersetzt, ohne unscharfen Vergleich — der richtige Ort für kurze Abkürzungen.",
  "vocabulary.max_error_ratio":
    "0,25 erlaubt bei einem Begriff mit acht Zeichen zwei falsche Zeichen. 0 schaltet unscharfe Treffer ab, feste Ersetzungen bleiben.",
  "vocabulary.min_term_chars":
    "Kürzere Begriffe werden nur exakt getroffen. Das schützt Dreibuchstabenwörter davor, versehentlich ersetzt zu werden.",
  "style_default.styling": "Wie förmlich das Ergebnis klingen soll.",
  "style_default.structure": "Fließtext oder Aufzählung.",
  "style_default.context":
    "Wofür der Text gedacht ist. „email“ erlaubt Anrede und Grußformel.",
  "debug.enabled":
    "Schreibt zu jedem Diktat einen Datensatz mit Erkennungstext, überarbeiteter Fassung und Zeiten.",
  "debug.dir": "Verzeichnis für diese Datensätze.",
  "debug.save_audio":
    "Legt zusätzlich die aufgenommene Tonspur ab. Braucht deutlich mehr Platz.",
  "overlay.position":
    "Wird eingelesen, steuert aber nichts — unter Wayland kann ein Fenster seine eigene Position nicht setzen.",
  "ui.theme":
    "Gilt für beide Fenster, auch für das Diktat-Overlay. \u201eSystem\u201c folgt der Hell-/Dunkel-Einstellung des Schreibtischs; jedes andere Schema legt eine Variante fest und bleibt auch dann, wenn der Schreibtisch wechselt. Wirkt sofort, kein Neustart nötig.",
  "overlay.width": "Wird eingelesen, steuert aber nichts.",
  "overlay.height": "Wird eingelesen, steuert aber nichts.",
  "autostart.enabled":
    "Legt ~/.config/autostart/yappr.desktop an; systemd startet yappr davon bei der nächsten Anmeldung automatisch. Aus heißt: die Datei existiert nicht, und nichts startet von selbst.",
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
  "inject.backend": ["wtype", "script", "clipboard"],
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
    system: "System (hell/dunkel folgen)",
    "yappr-light": "yappr Hell",
    "yappr-dark": "yappr Dunkel",
    "catppuccin-latte": "Catppuccin Latte",
    "catppuccin-mocha": "Catppuccin Mocha",
    "tokyo-night-day": "Tokyo Night Day",
    "tokyo-night-night": "Tokyo Night Night",
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
  from: "Erkannt als",
  to: "Ersetzen durch",
  match_class: "Fensterklasse",
  styling: "Tonfall",
  structure: "Struktur",
  context: "Kontext",
};

/// Row order within a section.
///
/// Needed because the config arrives as a `serde_json::Value`, whose object is
/// a `BTreeMap` -- keys reach this window sorted by their *Rust* name, not in
/// the order the struct declares them. Alphabetical-by-Rust-name is close to
/// arbitrary once the labels are German: it puts `max_word_ratio` above
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
  // Retired with the ydotool backend on 2026-09-09. Rust still accepts both
  // keys so a pre-existing config.toml loads (invariant 4) and still sends
  // them over the wire (`skip_serializing` is TOML-only), but nothing reads
  // them: a script picks its own paste chord. They are not settings.
  "inject.paste_chord",
  "inject.terminal_classes",
]);

/// Only an ordering hint: a key missing from this table still renders, after
/// the listed ones. Nothing here can hide a setting -- see
/// `OBSOLETE_FIELDS` for the one thing that can, and why it is not one.
export const FIELD_ORDER: Record<string, string[]> = {
  audio: ["device", "max_seconds", "vad_padding_ms"],
  asr: ["model", "language", "num_threads"],
  inject: ["backend", "script", "trailing_space", "keystroke_delay_ms"],
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
/// The single filter applied here is `OBSOLETE_FIELDS`, and `search()` goes
/// through this function precisely so a key hidden from a pane is not still
/// reachable through the search box.
export function orderedFields(section: string, value: Section): [string, Json][] {
  const declared = FIELD_ORDER[section] ?? [];
  const live = (k: string) => !OBSOLETE_FIELDS.has(`${section}.${k}`);
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
/// and `ß` all removed. Typing "grosse" has to find "Größe" and typing
/// "vokabular" has to find "Vokabular" — a settings search that only matches
/// what you can already spell exactly is a search nobody uses twice.
export function fold(s: string): string {
  return s
    .toLowerCase()
    .replace(/ß/g, "ss")
    .normalize("NFD")
    .replace(/\p{Diacritic}/gu, "");
}

/// Everything one row can be found by: its German label, its raw config key
/// (so `min_overlap_english` finds it even though the window never shows that
/// string), its section's title and raw name, its unit, and its help text.
///
/// The raw key is in there deliberately. Nobody is invited to *edit*
/// `config.toml` any more, but people still read it — in a bug report, or in a
/// daemon error naming a key — and someone arriving from either is searching
/// for the name Rust uses, not the one German uses.
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
