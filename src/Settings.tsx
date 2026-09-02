import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AnimatePresence, MotionConfig, motion } from "motion/react";
import { Icon } from "./settings/icons";
import { Commit, Device, Field, ResetButton, Row, TableEditor, Toggle } from "./settings/controls";
import { Wizard, WizardState } from "./settings/wizard";
import {
  HELP,
  Json,
  RESTART_SECTIONS,
  SECTION_NOTES,
  Section,
  categorize,
  jsonEqual,
  labelFor,
  orderedFields,
  paneTitleOf,
  search,
  titleFor,
} from "./settings/schema";

/// The settings window's shell: the sidebar, one pane per category, and the
/// autosave machine. Everything about how an individual setting *looks* lives
/// in `settings/controls.tsx`; everything about what it is *called* lives in
/// `settings/schema.ts`. This file only knows that the config is JSON with
/// sections at the top level.

/// How long a half-typed number or path is allowed to settle before it is
/// written. Long enough that typing "6000" is one save rather than four,
/// short enough that letting go of the keyboard feels like it committed.
const DEBOUNCE_MS = 700;

/// How long the "Gespeichert" capsule stays up before it leaves.
const SAVED_MS = 1800;

/// The house spring, in the two shapes this window uses. Apple's damping
/// ratio and response, spelled `bounce` and `duration` — `bounce: 0` is
/// critically damped, and `duration` is a settle time rather than a fixed
/// playback length, so an interrupted spring re-targets from wherever it
/// happens to be rather than restarting.
const SETTLE = { type: "spring", bounce: 0, duration: 0.34 } as const;
/// The moving selection behind the sidebar's active row. Slightly slower and
/// allowed a touch of overshoot: it is the one element here that travels a
/// visible distance, and a critically damped slide over 200 px reads as
/// sluggish where the same spring over 18 px reads as crisp.
const GLIDE = { type: "spring", bounce: 0.18, duration: 0.42 } as const;

type SaveState = "clean" | "pending" | "saving" | "saved" | "error";

export default function Settings() {
  const [config, setConfig] = useState<Section | null>(null);
  const [defaults, setDefaults] = useState<Section | null>(null);
  const [version, setVersion] = useState<string>("");
  const [devices, setDevices] = useState<Device[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [scrolled, setScrolled] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("clean");
  const [loading, setLoading] = useState(true);

  // The wizard takes over this whole window when it is active. `wizardState`
  // is `null` until `wizard_state()` answers, deliberately distinct from "not
  // needed" — nothing may flash on screen before the first answer arrives.
  const [wizardState, setWizardState] = useState<WizardState | null>(null);
  const [wizardActive, setWizardActive] = useState(false);

  // The save path reads the config through a ref: a debounced write fires long
  // after the render that scheduled it, and must send what the config looks
  // like *then*, not what it looked like when the key was pressed.
  const configRef = useRef<Section | null>(null);
  const savingRef = useRef(false);
  const queuedRef = useRef(false);
  const timerRef = useRef<number | null>(null);
  const savedTimerRef = useRef<number | null>(null);
  // Mirrors `saveState` for the reveal listener, which runs outside React's
  // render cycle and must not re-subscribe every time the capsule changes.
  const saveStateRef = useRef<SaveState>("clean");
  const searchRef = useRef<HTMLInputElement | null>(null);
  const paneRef = useRef<HTMLDivElement | null>(null);

  // `quiet` is the reveal path (see the `show-settings` listener below): the
  // same read, without the full-window "Lade…" state. A window that blanked
  // itself every time it was reopened would be a worse thing to look at than
  // a stale value, which rather defeats the point.
  const load = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    setLoadError(null);
    try {
      const res = (await invoke("get_config")) as {
        config: Section;
        defaults?: Section;
      };
      setConfig(res.config);
      configRef.current = res.config;
      setDefaults(res.defaults ?? null);
      setSaveState("clean");
      setSaveError(null);
    } catch (e) {
      setLoadError(String(e));
    } finally {
      if (!quiet) setLoading(false);
    }
    // Devices are a separate call on purpose: enumeration can be slow or fail
    // on a sick audio stack, and that must not stop the rest of the settings
    // from loading.
    try {
      const res = (await invoke("list_input_devices")) as { devices: Device[] };
      setDevices(res.devices ?? []);
    } catch {
      setDevices([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The running binary cannot change version while its own window is open, so
  // this is read once on mount rather than on every `load()`. A failure leaves
  // it empty and the sidebar foot simply stays blank: a settings window that
  // refused to open because it could not name itself would be a bad trade.
  useEffect(() => {
    invoke("app_version")
      .then((v) => setVersion(String(v)))
      .catch((e) => console.error("app_version failed", e));
  }, []);

  useEffect(() => {
    saveStateRef.current = saveState;
  }, [saveState]);

  // This window is hidden on close, never destroyed, so both it and its config
  // snapshot outlive every close -- and `flush` posts that whole snapshot. A
  // `config.toml` that changed underneath (edited by hand, or patched by
  // `wizard_finish`) would be silently written back to what this window
  // remembered at launch. `show_settings_window` emits on every reveal for
  // exactly this; see its comment for what that costs on GNOME.
  //
  // Refused outright when this window holds something newer than the file: a
  // debounced edit still waiting out `DEBOUNCE_MS`, a save in flight or
  // queued behind one, or a rejected save whose value invariant 9 deliberately
  // keeps on screen. Overwriting any of those would be the GUI discarding what
  // the user typed -- the one thing that autosave contract forbids.
  useEffect(() => {
    const unlistenPromise = listen("show-settings", () => {
      if (timerRef.current !== null || savingRef.current || queuedRef.current) return;
      if (saveStateRef.current === "error") return;
      void load(true);
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, [load]);

  // The window decides its own mode: `should_open` is computed from the same
  // marker-plus-readiness rule `lib.rs`'s startup thread uses, so there is no
  // ordering hazard between that thread and this webview's first paint.
  const loadWizardState = useCallback(async (activate: boolean) => {
    try {
      const res = (await invoke("wizard_state")) as WizardState;
      setWizardState(res);
      if (activate || res.should_open) setWizardActive(true);
    } catch (e) {
      // A wizard that cannot describe itself must not replace the settings
      // form with a blank screen — the window still works, and a user who
      // reached it from the tray gets what they asked for.
      console.error("wizard_state failed", e);
    }
  }, []);

  useEffect(() => {
    void loadWizardState(false);
  }, [loadWizardState]);

  // `yappr --wizard` and the tray's Einrichtung item. The state is re-read
  // rather than reused: the desktop or the backend may have changed since
  // this window mounted.
  useEffect(() => {
    const unlistenPromise = listen("show-wizard", () => {
      void loadWizardState(true);
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, [loadWizardState]);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
      if (savedTimerRef.current !== null) window.clearTimeout(savedTimerRef.current);
    };
  }, []);

  // Ctrl/Cmd+F reaches the search field and Escape leaves it, because a
  // window whose only way into search is a mouse trip to the top-left corner
  // is one where search does not get used.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === "f") {
        e.preventDefault();
        searchRef.current?.focus();
        searchRef.current?.select();
      } else if (e.key === "Escape" && query) {
        setQuery("");
        searchRef.current?.blur();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [query]);

  /// Writes whatever `configRef` holds right now. One save is in flight at a
  /// time; edits that arrive during one are not dropped but coalesced into a
  /// single follow-up run, so holding a key down cannot queue a write per
  /// keystroke against a daemon that answers in its own time.
  const flush = useCallback(async () => {
    const snapshot = configRef.current;
    if (!snapshot) return;
    if (savingRef.current) {
      queuedRef.current = true;
      return;
    }
    savingRef.current = true;
    setSaveState("saving");
    try {
      const res = (await invoke("set_config", { config: snapshot })) as {
        restart_required?: boolean;
        restart_reason?: string;
      };
      setSaveError(null);
      setNotice(res.restart_reason ?? null);
      setSaveState("saved");
      if (savedTimerRef.current !== null) window.clearTimeout(savedTimerRef.current);
      savedTimerRef.current = window.setTimeout(() => setSaveState("clean"), SAVED_MS);
    } catch (e) {
      // The local value is kept deliberately. The daemon validates before it
      // writes, so a rejection means the file on disk is untouched -- losing
      // what the user typed on top of that would be the GUI's own doing.
      setSaveError(String(e));
      setSaveState("error");
    } finally {
      savingRef.current = false;
      if (queuedRef.current) {
        queuedRef.current = false;
        void flush();
      }
    }
  }, []);

  const schedule = useCallback(
    (commit: Commit) => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
      if (commit === "now") {
        void flush();
        return;
      }
      setSaveState("pending");
      timerRef.current = window.setTimeout(() => {
        timerRef.current = null;
        void flush();
      }, DEBOUNCE_MS);
    },
    [flush],
  );

  const update = useCallback(
    (section: string, key: string, value: Json, commit: Commit) => {
      const prev = configRef.current;
      if (!prev) return;
      const next = { ...prev, [section]: { ...(prev[section] as Section), [key]: value } };
      configRef.current = next;
      setConfig(next);
      schedule(commit);
    },
    [schedule],
  );

  const updateSection = useCallback(
    (section: string, value: Json, commit: Commit) => {
      const prev = configRef.current;
      if (!prev) return;
      const next = { ...prev, [section]: value };
      configRef.current = next;
      setConfig(next);
      schedule(commit);
    },
    [schedule],
  );

  const categories = useMemo(
    () => (config ? categorize(Object.keys(config)) : []),
    [config],
  );

  // The first pane, until the user picks one. Resolved rather than stored so a
  // config whose sections changed under us cannot leave the sidebar pointing
  // at a pane that no longer exists.
  const current =
    categories.find((c) => c.id === active) ?? categories[0] ?? null;

  // Search is a lens over this window, not a different window: `null` means
  // no query, and everything below falls back to the category view.
  const hits = useMemo(() => (config ? search(config, query) : null), [config, query]);

  if (loading) {
    return (
      <main className="shell shell--bare">
        <p className="status">Lade…</p>
      </main>
    );
  }

  if (!config || !current) {
    return (
      <main className="shell shell--bare">
        <div className="pane">
          <div className="banner error">
            <Icon name="warn" className="icon-sm" />
            <span>{loadError ?? "Keine Konfiguration geladen."}</span>
          </div>
          <button type="button" className="add" onClick={() => void load()}>
            Erneut versuchen
          </button>
        </div>
      </main>
    );
  }

  const searching = hits !== null;
  const hitCount = hits?.reduce((n, h) => n + (h.keys?.length ?? 1), 0) ?? 0;
  const shown = searching ? hits : current.sections.map((s) => ({ section: s, keys: null }));

  if (wizardActive && wizardState) {
    return (
      <MotionConfig reducedMotion="user">
        <main className="shell shell--wizard">
          <Wizard
            state={wizardState}
            onFinish={(setBackend) => {
              // Not swallowed silently, but it must not trap the user in the
              // wizard either: the marker is a convenience, and a wizard that
              // will not close is worse than one that reappears next launch.
              invoke("wizard_finish", { setBackend })
                // `wizard_finish` puts `inject.backend` through `set_config`
                // before it hides the window, so this window's snapshot is a
                // key out of date the moment it returns. The reveal listener
                // would catch that on the next open; re-reading here means it
                // is never wrong in between.
                .then(() => load(true))
                .catch((e) => {
                  console.error("wizard_finish failed", e);
                });
              setWizardActive(false);
            }}
            onOpenSettings={() => setWizardActive(false)}
          />
        </main>
      </MotionConfig>
    );
  }

  return (
    <MotionConfig reducedMotion="user">
      <main className="shell">
        <nav className="sidebar">
          <div className="brand">
            {/* The app icon itself — the same `public/yappr.png` the bundle
                installs, so the window, the launcher and this rail all show
                one mark. Decorative: the window's own title bar already says
                which application this is. */}
            <img className="brand__mark" src="/yappr.png" alt="" aria-hidden="true" />
            <span className="brand__text">yappr</span>
          </div>

          <div className="searchbox">
            <Icon name="search" className="icon-sm searchbox__glyph" />
            <input
              ref={searchRef}
              type="search"
              className="searchbox__input"
              placeholder="Suchen"
              aria-label="Einstellungen durchsuchen"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <AnimatePresence>
              {query && (
                <motion.button
                  type="button"
                  className="searchbox__clear"
                  aria-label="Suche leeren"
                  onClick={() => {
                    setQuery("");
                    searchRef.current?.focus();
                  }}
                  initial={{ opacity: 0, scale: 0.6 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.6 }}
                  transition={SETTLE}
                >
                  <Icon name="close" className="icon-xs" />
                </motion.button>
              )}
            </AnimatePresence>
          </div>

          <div className="nav">
            {categories.map((c) => {
              const isActive = c.id === current.id && !searching;
              return (
                <button
                  type="button"
                  key={c.id}
                  className={`nav-item${isActive ? " active" : ""}`}
                  aria-current={isActive ? "page" : undefined}
                  onClick={() => {
                    setActive(c.id);
                    setQuery("");
                    paneRef.current?.scrollTo({ top: 0 });
                  }}
                >
                  {/* One selection that travels between rows rather than one
                      per row that blinks on and off. `layoutId` is what makes
                      it the same object moving: it tells you where the
                      selection went, which two independent fades cannot. */}
                  {isActive && (
                    <motion.span
                      layoutId="nav-selection"
                      className="nav-selection"
                      transition={GLIDE}
                    />
                  )}
                  <Icon name={c.icon} className="icon" />
                  <span className="nav-item__text">{c.title}</span>
                </button>
              );
            })}
          </div>

          {/* What the foot of this rail is for: the one fact about the
              running program that is not one of its settings. Nothing to
              click -- the config path and its reload button used to live
              here, and a line of text that answers "which build is this"
              is worth more at the bottom of a settings window than a
              control for a file the window already writes by itself. */}
          <div className="sidebar-foot">
            {version && <p className="version">Version: {version}</p>}
          </div>
        </nav>

        <div className="pane-wrap">
          <div
            className="pane"
            ref={paneRef}
            onScroll={(e) => {
              const next = e.currentTarget.scrollTop > 2;
              setScrolled((prev) => (prev === next ? prev : next));
            }}
          >
            {/* Sticky *inside* the scroller, so content genuinely passes
                beneath it and the blur has something to blur. A soft edge
                where the two meet, rather than a hairline rule that would sit
                there just as hard when there is nothing underneath it. */}
            <header className={`topbar${scrolled ? " is-scrolled" : ""}`}>
              <h1>{searching ? "Suchergebnisse" : current.title}</h1>
              {searching && (
                <span className="topbar__count">
                  {hitCount === 1 ? "1 Einstellung" : `${hitCount} Einstellungen`}
                </span>
              )}
            </header>

            <div className="pane-body">
              <AnimatePresence initial={false}>
                {saveError && (
                  <motion.div
                    key="save-error"
                    className="banner error"
                    layout
                    initial={{ opacity: 0, y: -8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={SETTLE}
                  >
                    <Icon name="warn" className="icon-sm" />
                    <span>{saveError}</span>
                    <button type="button" className="ghost" onClick={() => void flush()}>
                      Erneut speichern
                    </button>
                  </motion.div>
                )}
                {notice && (
                  <motion.div
                    key="notice"
                    className="banner notice"
                    layout
                    initial={{ opacity: 0, y: -8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={SETTLE}
                  >
                    <Icon name="warn" className="icon-sm" />
                    <span>{notice}</span>
                    <button type="button" className="ghost" onClick={() => setNotice(null)}>
                      Verstanden
                    </button>
                  </motion.div>
                )}
              </AnimatePresence>

              {searching && hitCount === 0 ? (
                <div className="nothing">
                  <Icon name="search" className="nothing__glyph" />
                  <p className="nothing__title">Nichts gefunden</p>
                  <p className="nothing__body">
                    Keine Einstellung passt zu „{query.trim()}“. Gesucht wird in Namen,
                    Erklärungen und den Schlüsseln aus <code>config.toml</code>.
                  </p>
                  <button type="button" className="add" onClick={() => setQuery("")}>
                    Suche leeren
                  </button>
                </div>
              ) : (
                // Keyed on what is being shown, so switching panes re-mounts
                // the body and it rises into place. No exit animation on the
                // way out: the response to a click has to be the new pane,
                // immediately, not the old one politely leaving first.
                <motion.div
                  key={searching ? `q:${query}` : current.id}
                  initial={{ opacity: 0, y: 8 }}
                  animate={{ opacity: 1, y: 0 }}
                  transition={SETTLE}
                >
                  {shown.map(({ section, keys }) => (
                    <SectionCard
                      key={section}
                      name={section}
                      value={config[section]}
                      defaults={defaults?.[section]}
                      devices={devices}
                      only={keys}
                      pane={searching ? paneTitleOf(section) : null}
                      onField={(key, value, commit) => update(section, key, value, commit)}
                      onSection={(value, commit) => updateSection(section, value, commit)}
                    />
                  ))}
                  {/* Same reasoning as the Setup pane above: filesystem
                      state, not a `config.toml` section (see
                      `AutostartCard`'s own doc comment), so it renders
                      alongside `shown.map` rather than through it. Appended
                      after the config-backed sections rather than before —
                      Allgemein's other rows (Mikrofon, Texteingabe) are
                      about how one dictation behaves, this is about the app
                      itself. */}
                  {!searching && current.id === "allgemein" && <AutostartCard />}
                </motion.div>
              )}
            </div>
          </div>

          <SaveCapsule state={saveState} />
        </div>
      </main>
    </MotionConfig>
  );
}

/// The autosave's only permanent voice.
///
/// It says nothing at rest, which is the point: a window with no Save button
/// has nothing to confirm most of the time, and a badge reading "Gesichert"
/// forever is a light that is always on. Failure is deliberately *not* here —
/// it stays as the banner at the top of the pane, because a failure needs to
/// persist and to carry a retry, and a capsule that fades out after two
/// seconds can do neither.
function SaveCapsule({ state }: { state: SaveState }) {
  const visible = state === "saving" || state === "saved";
  return (
    <div className="savebar" aria-live="polite">
      <AnimatePresence>
        {visible && (
          <motion.div
            key="save"
            className={`save-capsule ${state}`}
            initial={{ opacity: 0, y: 14, scale: 0.92, filter: "blur(8px)" }}
            animate={{ opacity: 1, y: 0, scale: 1, filter: "blur(0px)" }}
            exit={{ opacity: 0, y: 14, scale: 0.92, filter: "blur(8px)" }}
            transition={SETTLE}
          >
            {state === "saved" ? (
              <svg className="icon-sm" viewBox="0 0 24 24" aria-hidden="true">
                <motion.path
                  d="M5 12.5 10 17.5 19.5 7"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2.4"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  initial={{ pathLength: 0 }}
                  animate={{ pathLength: 1 }}
                  transition={{ duration: 0.26, ease: [0.22, 1, 0.36, 1] }}
                />
              </svg>
            ) : (
              <span className="save-capsule__spin" aria-hidden="true" />
            )}
            <span>{state === "saved" ? "Gespeichert" : "Speichert…"}</span>
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

/// "Beim Anmelden starten" (spec §9, task 16). Deliberately **not** rendered
/// through `SectionCard`/`config[section]` the way every other row in this
/// window is: the thing being toggled is whether
/// `~/.config/autostart/yappr.desktop` exists, which is filesystem
/// state that can change behind this app's back (the user clearing that
/// directory by hand, another autostart manager). Mirroring that into a
/// `config.toml` key would be a second copy of the same fact, free to
/// disagree with the file the moment either changes without the other — see
/// `settings_cmds.rs`'s module doc for the full reasoning.
///
/// So this card asks the filesystem directly (`autostart_status`) rather
/// than reading anything out of `config`, and every toggle press
/// (`set_autostart`) writes or removes the real file immediately — there is
/// no debounce here the way `NumberInput` needs one, because a toggle, like
/// every other boolean row in this window, is a finished decision the
/// moment it changes.
function AutostartCard() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    invoke("autostart_status")
      .then((res) => setEnabled((res as { enabled: boolean }).enabled))
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const onChange = useCallback(
    (next: boolean) => {
      // Guards the same race `disabled={enabled === null}` dims on screen:
      // without it, a click that lands before the first `autostart_status`
      // reply resolves would toggle from an unknown baseline and could
      // "revert" to `null` on failure instead of to whatever is real.
      if (enabled === null) return;
      // Optimistic, then reverted on failure — exactly `flush`'s rule for
      // every other setting in this window (CLAUDE.md invariant 9): the
      // backend validates/writes before this is trusted, so a rejection
      // must not leave the toggle showing something the disk disagrees with.
      const previous = enabled;
      setEnabled(next);
      setError(null);
      invoke("set_autostart", { enabled: next }).catch((e) => {
        setEnabled(previous);
        setError(String(e));
      });
    },
    [enabled],
  );

  return (
    <section className="group">
      <div className="group-head">
        <h2>Autostart</h2>
      </div>
      <div className="card">
        <Row
          label={labelFor("autostart.enabled", "enabled")}
          help={HELP["autostart.enabled"]}
          disabled={enabled === null}
          control={<Toggle value={enabled ?? false} onChange={(v) => onChange(v as boolean)} />}
        />
      </div>
      {error && (
        <div className="banner error">
          <Icon name="warn" className="icon-sm" />
          <span>{error}</span>
        </div>
      )}
    </section>
  );
}

function SectionCard({
  name,
  value,
  defaults,
  devices,
  only,
  pane,
  onField,
  onSection,
}: {
  name: string;
  value: Json;
  defaults: Json | undefined;
  devices: Device[];
  /** Search: the keys that matched, or `null` for the whole section. */
  only: string[] | null;
  /** Search: which pane this section normally lives in. */
  pane: string | null;
  onField: (key: string, value: Json, commit: Commit) => void;
  onSection: (value: Json, commit: Commit) => void;
}) {
  const note = SECTION_NOTES[name];
  // Only a section that is itself a list carries its own reset; a section of
  // keys resets one row at a time, which is the finer and less surprising
  // grain of the two.
  const sectionReset =
    Array.isArray(value) && defaults !== undefined && !jsonEqual(value, defaults);

  return (
    <section className="group">
      <div className="group-head">
        <h2>{titleFor(name)}</h2>
        {/* Where this section lives, shown only in search results — a flat
            list of hits is useless if you cannot get back to where one came
            from next time. */}
        {pane && <span className="group-pane">{pane}</span>}
        {RESTART_SECTIONS.has(name) && (
          <span
            className="tag"
            title="Diese Einstellungen greifen erst nach einem Neustart des Daemons."
          >
            Neustart nötig
          </span>
        )}
        {sectionReset && <ResetButton onClick={() => onSection(defaults as Json, "now")} />}
      </div>
      {note && <p className="note">{note}</p>}

      <div className="card">
        {Array.isArray(value) ? (
          <TableEditor
            path={name}
            rows={value as Section[]}
            onChange={(rows) => onSection(rows as Json, "now")}
          />
        ) : value && typeof value === "object" ? (
          orderedFields(name, value as Section)
            .filter(([key]) => only === null || only.includes(key))
            .map(([key, v]) => {
              const fallback = (defaults as Section | undefined)?.[key];
              return (
                <Field
                  key={key}
                  section={name}
                  fieldKey={key}
                  value={v}
                  devices={devices}
                  canReset={fallback !== undefined && !jsonEqual(v, fallback)}
                  onChange={(next, commit) => onField(key, next, commit)}
                  onReset={() => onField(key, fallback as Json, "now")}
                />
              );
            })
        ) : (
          <p className="empty">Unerwarteter Abschnitt.</p>
        )}
      </div>
    </section>
  );
}
