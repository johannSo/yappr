import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AnimatePresence, MotionConfig, motion } from "motion/react";
import { Icon } from "./settings/icons";
import { Commit, Device, Field, ResetButton, TableEditor } from "./settings/controls";
import {
  Json,
  RESTART_SECTIONS,
  SECTION_NOTES,
  SETUP_CATEGORY,
  Section,
  categorize,
  jsonEqual,
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

/// A model `setup_status()`/`run_setup()` reported missing — `provision.rs`'s
/// `MissingModel`, unchanged across the wire.
type MissingModel = { name: string; display: string };

/// `provision::setup_status`'s response shape, and also what `run_setup`
/// resolves to once provisioning finishes.
type SetupStatus = {
  ready: boolean;
  missing_prerequisites: string[];
  missing_models: MissingModel[];
};

/// One artifact's live download progress, keyed by `MissingModel.name` — kept
/// only for artifacts a `"setup-progress"` event has actually mentioned, so a
/// model nothing has reported on yet renders as "fehlt" rather than a bar
/// stuck at 0%.
type DownloadProgress = { display: string; done: number; total: number | null };

/// `provision::SetupProgress`, unchanged across the wire (`#[serde(tag =
/// "kind")]` is what makes the discriminated union below work).
type SetupProgressEvent =
  | { kind: "downloading"; name: string; display: string; done: number; total: number | null }
  | { kind: "finished" }
  | { kind: "failed"; message: string };

export default function Settings() {
  const [config, setConfig] = useState<Section | null>(null);
  const [defaults, setDefaults] = useState<Section | null>(null);
  const [configPath, setConfigPath] = useState<string>("");
  const [devices, setDevices] = useState<Device[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [scrolled, setScrolled] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("clean");
  const [loading, setLoading] = useState(true);

  // Task 15: first-run Setup. `setupStatus` is `null` until the first check
  // resolves, deliberately distinct from "ready" — the Setup pane must not
  // flash into existence and back out before the very first answer arrives.
  const [setupStatus, setSetupStatus] = useState<SetupStatus | null>(null);
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);
  const [downloads, setDownloads] = useState<Record<string, DownloadProgress>>({});

  // The save path reads the config through a ref: a debounced write fires long
  // after the render that scheduled it, and must send what the config looks
  // like *then*, not what it looked like when the key was pressed.
  const configRef = useRef<Section | null>(null);
  const savingRef = useRef(false);
  const queuedRef = useRef(false);
  const timerRef = useRef<number | null>(null);
  const savedTimerRef = useRef<number | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  const paneRef = useRef<HTMLDivElement | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const res = (await invoke("get_config")) as {
        config: Section;
        config_path?: string;
        defaults?: Section;
      };
      setConfig(res.config);
      configRef.current = res.config;
      setDefaults(res.defaults ?? null);
      setConfigPath(res.config_path ?? "");
      setSaveState("clean");
      setSaveError(null);
    } catch (e) {
      setLoadError(String(e));
    } finally {
      setLoading(false);
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

  /// `setup_status()` hashes whatever models are already on disk (up to
  /// ~1.1 GB), so it can take noticeably longer than `get_config` — a
  /// separate call for the same reason `list_input_devices` is one: a slow
  /// or failing check here must not hold up the rest of the window loading.
  /// A failure is swallowed into `ready: true` deliberately — this pane is
  /// the one piece of first-run guidance the app can offer; if it cannot
  /// even tell what's missing, staying out of the way of the rest of the
  /// settings window is better than blocking on it forever.
  const checkSetup = useCallback(async () => {
    try {
      const res = (await invoke("setup_status")) as SetupStatus;
      setSetupStatus(res);
    } catch {
      setSetupStatus({ ready: true, missing_prerequisites: [], missing_models: [] });
    }
  }, []);

  useEffect(() => {
    void checkSetup();
  }, [checkSetup]);

  // Listens for `run_setup`'s progress for the life of the window, not just
  // while the Setup pane is on screen — a user who switches to another pane
  // mid-download must not lose the running total, and `Finished`/`Failed`
  // still need to land on `installing`/`notice` wherever they arrive.
  useEffect(() => {
    const unlistenPromise = listen<SetupProgressEvent>("setup-progress", (event) => {
      const payload = event.payload;
      if (payload.kind === "downloading") {
        setDownloads((prev) => ({
          ...prev,
          [payload.name]: { display: payload.display, done: payload.done, total: payload.total },
        }));
      } else if (payload.kind === "finished") {
        setInstalling(false);
        setDownloads({});
        setNotice(
          "Installation abgeschlossen. Starte OpenWhisprFlow neu, damit die neuen Modelle geladen werden.",
        );
        void checkSetup();
      } else if (payload.kind === "failed") {
        setInstalling(false);
        setInstallError(payload.message);
      }
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, [checkSetup]);

  const startInstall = useCallback(() => {
    setInstalling(true);
    setInstallError(null);
    setDownloads({});
    // Resolution/rejection both also arrive as "setup-progress" events
    // (Finished/Failed), which is what actually drives `installing` and
    // `installError` back down — this `catch` exists only so a rejected
    // invoke doesn't surface as an unhandled promise rejection.
    invoke("run_setup").catch(() => {});
  }, []);

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

  const categories = useMemo(() => {
    const base = config ? categorize(Object.keys(config)) : [];
    // Prepended, not appended: a first-run user's very first pane should be
    // the one telling them what's missing, not the last thing they scroll
    // past to find it. Disappears on its own once `setupStatus.ready` flips
    // true — see `SETUP_CATEGORY`'s doc comment.
    return setupStatus && !setupStatus.ready ? [SETUP_CATEGORY, ...base] : base;
  }, [config, setupStatus]);

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

  return (
    <MotionConfig reducedMotion="user">
      <main className="shell">
        <nav className="sidebar">
          <div className="brand">OpenWhisprFlow</div>

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

          <div className="sidebar-foot">
            {configPath && (
              <p className="path" title={configPath}>
                {configPath}
              </p>
            )}
            <button
              type="button"
              className="ghost"
              onClick={() => void load()}
              title="Konfiguration neu vom Daemon laden"
            >
              <Icon name="reset" className="icon-sm" />
              {/* In a span so the icon-only sidebar can drop the word without
                  dropping the button -- a bare text node has no selector. */}
              <span>Neu laden</span>
            </button>
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
                  {/* Not config-backed (see `SETUP_CATEGORY`'s doc comment),
                      so it renders alongside `shown.map` below rather than
                      through it — `current.sections` is empty for this
                      category, so that map contributes nothing here on its
                      own. */}
                  {!searching && current.id === "setup" && setupStatus && (
                    <SetupPane
                      status={setupStatus}
                      downloads={downloads}
                      installing={installing}
                      installError={installError}
                      onInstall={startInstall}
                    />
                  )}
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

/// Renders one download's progress as a fraction of a known total, or (no
/// `Content-Length` header) as a raw MB count climbing with no visible
/// ceiling — the same fallback `openwhisprflow --update-lock`'s own terminal
/// output uses for the same reason (`setup.rs`'s `progress_line`).
function downloadStatusText(progress: DownloadProgress | undefined, installing: boolean): string {
  if (!progress) return installing ? "wartet…" : "fehlt";
  if (progress.total !== null) {
    const pct = Math.min(100, Math.round((progress.done / progress.total) * 100));
    return `${pct} %`;
  }
  return `${Math.round(progress.done / (1 << 20))} MB`;
}

/// The first-run Setup pane (spec §7, Task 15): the one pane in this window
/// that renders from `setup_status()`/`run_setup()` rather than from
/// `config.toml` — see `SETUP_CATEGORY`'s doc comment in `schema.ts` for why
/// it gets its own component instead of a `SectionCard`.
function SetupPane({
  status,
  downloads,
  installing,
  installError,
  onInstall,
}: {
  status: SetupStatus;
  downloads: Record<string, DownloadProgress>;
  installing: boolean;
  installError: string | null;
  onInstall: () => void;
}) {
  const { missing_prerequisites: missingPrerequisites, missing_models: missingModels } = status;

  return (
    <>
      <section className="group">
        <div className="group-head">
          <h2>Voraussetzungen</h2>
        </div>
        <p className="note">
          Diese Programme kommen nicht von OpenWhisprFlow selbst und müssen von Hand
          installiert werden.
        </p>
        <div className="card">
          {missingPrerequisites.length === 0 ? (
            <div className="setup-row ok">
              <Icon name="check" className="icon-sm" />
              <span>Alle benötigten Programme sind installiert.</span>
            </div>
          ) : (
            missingPrerequisites.map((pkg) => (
              <div className="setup-row missing" key={pkg}>
                <Icon name="warn" className="icon-sm" />
                <span>{pkg} fehlt.</span>
              </div>
            ))
          )}
        </div>
        {missingPrerequisites.length > 0 && (
          <p className="setup-command">
            Installieren mit: <code>sudo pacman -S {missingPrerequisites.join(" ")}</code>
          </p>
        )}
      </section>

      <section className="group">
        <div className="group-head">
          <h2>Modelle</h2>
        </div>
        <p className="note">
          Spracherkennung, Erkennung von Sprachpausen und Nachbearbeitung laufen lokal
          und brauchen dafür diese Modelle — insgesamt etwa 1,1 GB.
        </p>
        <div className="card">
          {missingModels.length === 0 ? (
            <div className="setup-row ok">
              <Icon name="check" className="icon-sm" />
              <span>Alle Modelle sind vorhanden.</span>
            </div>
          ) : (
            missingModels.map((m) => {
              const progress = downloads[m.name];
              const done = !!progress && progress.total !== null && progress.done >= progress.total;
              return (
                <div className={`setup-row${done ? " ok" : " missing"}`} key={m.name}>
                  <Icon name={done ? "check" : "warn"} className="icon-sm" />
                  <div className="setup-row__body">
                    <div className="setup-row__head">
                      <span>{m.display}</span>
                      <span className="setup-row__status">
                        {downloadStatusText(progress, installing)}
                      </span>
                    </div>
                    {progress && !done && (
                      <div className="progressbar">
                        <div
                          className="progressbar__fill"
                          style={{
                            width:
                              progress.total !== null
                                ? `${Math.min(100, (progress.done / progress.total) * 100)}%`
                                : "35%",
                          }}
                        />
                      </div>
                    )}
                  </div>
                </div>
              );
            })
          )}
        </div>
        {missingModels.length > 0 && (
          <div className="setup-actions">
            <button type="button" className="add" onClick={onInstall} disabled={installing}>
              {installing ? "Installation läuft…" : "Installation starten"}
            </button>
          </div>
        )}
      </section>

      {installError && (
        <div className="banner error">
          <Icon name="warn" className="icon-sm" />
          <span>{installError}</span>
        </div>
      )}
    </>
  );
}
