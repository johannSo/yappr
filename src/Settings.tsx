import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyTheme, type ConfiguredTheme } from "./theme";
import { AnimatePresence, MotionConfig, motion } from "motion/react";
import { Icon } from "./settings/icons";
import { Commit, Device, Field, ResetButton, Row, TableEditor, Toggle } from "./settings/controls";
import { setupGapSummary, Wizard, WizardState } from "./settings/wizard";
import { AsrModelDownload } from "./settings/model-download";
import { OpenClawCard } from "./settings/openclaw";
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

/// `[ui] theme` out of a config snapshot. The config arrives as untyped JSON
/// (this window deliberately keeps no copy of the schema), and an install
/// whose `config.toml` predates the section has no `ui` at all -- both of
/// which land on `"system"`, which `applyTheme` resolves to the shipped pair.
function themeIn(config: Section): ConfiguredTheme {
  const ui = config.ui;
  if (ui && typeof ui === "object" && !Array.isArray(ui)) {
    const theme = (ui as Record<string, unknown>).theme;
    if (typeof theme === "string") return theme as ConfiguredTheme;
  }
  return "system";
}

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
  // The restart latch. Deliberately sticky, and deliberately *not* a
  // a re-read of `restart_reason`: that arrives on one save and the very next
  // unrelated autosave used to clear the state holding it
  // (`setNotice(res.restart_reason ?? null)`), which for a passive banner was
  // a cosmetic loss and for a dialog that must be answered would be the whole
  // feature going missing because the user flipped some other toggle
  // afterwards. Once a restart is
  // owed it stays owed until the process actually restarts, which is the only
  // thing that can pay it off — hence no setter that clears this.
  const [restartPending, setRestartPending] = useState(false);
  // "Später": the dialog has been answered once, so it stops re-opening and
  // the amber banner carries the offer from then on. Not a dismissal of the
  // requirement itself — see `restartPending`.
  const [restartDeferred, setRestartDeferred] = useState(false);
  // True from the moment `restart_app` is invoked. Normally this never goes
  // back to false: the command returns as soon as the daemon has *accepted*
  // the restart, and the webview is torn down underneath us a moment later.
  // It exists to keep a second click from firing a second restart in that
  // window, and to say so on the button.
  const [restarting, setRestarting] = useState(false);
  const [restartError, setRestartError] = useState<string | null>(null);
  // Bumped by every *successful* save. `AsrModelDownload` re-checks on this
  // rather than on the dropdown's local value, which would race the save that
  // is still in flight -- `setup_status` answers from config.toml on disk.
  const [savedRevision, setSavedRevision] = useState(0);
  // Kept apart from the restart state deliberately. That one comes from a
  // save (`restart_required`), and the two used to share one string: a save
  // wiped the startup notice before the user had read it, and the next
  // reveal's `load(true)` brought the startup notice back over the restart
  // one. They are different messages with different lifetimes and both
  // matter — the split is now also a difference in kind, since the restart
  // side is a latched boolean and this one is the daemon's own text.
  const [configNotice, setConfigNotice] = useState<string | null>(null);
  // "Verstanden" has to stick. The daemon reports the same notice on every
  // `get_config`, and this window re-reads on every reveal, so without this the
  // banner would come back each time the window is reopened.
  const dismissedNoticeRef = useRef<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("clean");
  const [loading, setLoading] = useState(true);

  // The wizard takes over this whole window when it is active. `wizardState`
  // is `null` until `wizard_state()` answers, deliberately distinct from "not
  // needed" — nothing may flash on screen before the first answer arrives.
  const [wizardState, setWizardState] = useState<WizardState | null>(null);
  const [wizardActive, setWizardActive] = useState(false);
  // A marker that could not be written is the one remaining reason the wizard
  // legitimately comes back next launch (`wizard::remember_setup_seen`), so it
  // is said out loud in the banner slot rather than left in a console the user
  // has no way to open. It used to be a bare `console.error`, on the very path
  // whose whole symptom is "the setup screen keeps coming back for no reason".
  const [wizardError, setWizardError] = useState<string | null>(null);

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
        config_notice?: string;
      };
      setConfig(res.config);
      configRef.current = res.config;
      // The palette, from the same read as everything else. The
      // `theme-changed` listener below covers a change made *here*; this
      // covers the first load and a reveal, where the file may have moved on
      // without this window having been open to hear about it.
      applyTheme(themeIn(res.config));
      setDefaults(res.defaults ?? null);
      // Startup found a config.toml it could not read, moved it aside and came
      // up on defaults (spec §3). This window is the only place that can say
      // so: the app is running and dictation works, so nothing else about it
      // looks wrong -- and the settings on screen are not the settings the
      // user had. The banner names the file the old one went to, because that
      // is the only way back to it.
      setConfigNotice(
        res.config_notice && res.config_notice !== dismissedNoticeRef.current
          ? res.config_notice
          : null,
      );
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
  // Every accepted save broadcasts the configured palette, including this
  // window's own -- so the theme dropdown repaints through the same round
  // trip as the overlay rather than through an optimistic local path that
  // could disagree with what actually got written.
  useEffect(() => {
    const unlistenPromise = listen<ConfiguredTheme>("theme-changed", ({ payload }) =>
      applyTheme(payload),
    );
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

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

  // The window decides its own mode: `should_open` is the same marker check
  // `lib.rs`'s startup thread makes, so there is no ordering hazard between
  // that thread and this webview's first paint.
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

  // Re-reads the facts *without* the activate branch. Called on the way out
  // of the wizard: the banner below has to disappear once the model is there,
  // and `loadWizardState` cannot be used for that -- on a first run the
  // marker is not written until "Fertig", so it would put the user straight
  // back into the wizard they just left.
  const refreshWizardState = useCallback(async () => {
    try {
      setWizardState((await invoke("wizard_state")) as WizardState);
    } catch (e) {
      console.error("wizard_state failed", e);
    }
  }, []);

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
      setSavedRevision((n) => n + 1);
      // Only ever set, never cleared — see `restartPending`. A save that
      // needs no restart says nothing about one already owed by an earlier
      // save, so it must not answer for it.
      if (res.restart_required) {
        setRestartPending(true);
      }
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
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
        // Cleared means cleared. Only the timeout callback used to null this,
        // and cancelling it skipped that -- so after the ordinary sequence
        // "type in a field, then flip a toggle within DEBOUNCE_MS" the ref
        // stayed a stale non-null id for the life of the window, and the
        // `show-settings` guard below reads it as "an edit is still pending"
        // and refuses every reveal-time resync from then on.
        timerRef.current = null;
      }
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

  /// Hands the restart to the daemon and does not expect to be around
  /// afterwards. `Request::Restart` latches `quitting`, waits for any
  /// utterance in flight to finish (invariant 1), tears down, starts the
  /// successor and exits — so this promise resolving means "accepted", not
  /// "done", and the webview is gone a moment later. Nothing may be
  /// scheduled off the success path for that reason.
  ///
  /// The `catch` is not dead code even so: in `--replay` mode there is no
  /// daemon to restart and `restart_app` answers with a stated German error
  /// rather than exiting anything.
  const restartNow = useCallback(async () => {
    setRestarting(true);
    setRestartError(null);
    try {
      await invoke("restart_app");
    } catch (e) {
      setRestarting(false);
      setRestartError(String(e));
    }
  }, []);

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

  // A restart must never be started on top of an edit that has not been
  // written yet. `"pending"` is the 700 ms debounce still counting and
  // `"saving"` is a write in flight; restarting through either would throw
  // away what the user just typed, and the daemon would come back running
  // the previous value — the exact opposite of what pressing a button
  // labelled "restart so the change takes effect" is asking for.
  const saveSettled = saveState !== "pending" && saveState !== "saving";
  // The dialog waits for that same settling rather than firing straight off
  // the save response. Autosave writes a toggle immediately but a text field
  // 700 ms after the last keystroke, so a dialog on the response would land
  // in the middle of typing and steal the caret. Waiting means it appears
  // once, when the user pauses.
  const restartDialogOpen = restartPending && !restartDeferred && saveSettled;
  // The standing reminder, in the amber banner slot: whenever a restart is
  // owed and the dialog is not the thing saying so.
  const restartBanner = restartPending && !restartDialogOpen;

  if (wizardActive && wizardState) {
    return (
      <MotionConfig reducedMotion="user">
        <main className="shell shell--wizard">
          <Wizard
            state={wizardState}
            onFinish={(setBackend) => {
              // Not swallowed silently, but it must not trap the user in the
              // wizard either: a wizard that will not close is worse than one
              // that reappears next launch. `wizard_finish` writes the marker
              // before anything that can fail, so what a rejection here means
              // is "the marker itself could not be stored, or the backend
              // patch was refused" — and the banner says which.
              invoke("wizard_finish", { setBackend })
                // `wizard_finish` puts `inject.backend` through `set_config`
                // before it hides the window, so this window's snapshot is a
                // key out of date the moment it returns. The reveal listener
                // would catch that on the next open; re-reading here means it
                // is never wrong in between.
                .then(() => {
                  setWizardError(null);
                  return load(true);
                })
                .catch((e) => {
                  console.error("wizard_finish failed", e);
                  setWizardError(String(e));
                });
              void refreshWizardState();
              setWizardActive(false);
            }}
            // Leaving for the settings form is an exit from the wizard, and
            // every exit has to persist the marker — that is invariant 14's
            // "finished once, never again by itself". This one wrote nothing
            // until 2026-09-10, so a user who took the models step's
            // "Einstellungen" button, or the last step's "Einstellungen
            // öffnen", got the whole wizard back on every launch of a
            // perfectly set-up install.
            onOpenSettings={() => {
              invoke("wizard_dismiss")
                .then(() => setWizardError(null))
                .catch((e) => {
                  console.error("wizard_dismiss failed", e);
                  setWizardError(String(e));
                });
              void refreshWizardState();
              setWizardActive(false);
            }}
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
                beneath it rather than the whole column sliding under a fixed
                strip. It is opaque, and picks up a shadow only once the pane
                has scrolled -- a rule would sit there just as hard when there
                is nothing underneath it. */}
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
                {configNotice && (
                  <motion.div
                    key="config-notice"
                    className="banner error"
                    layout
                    initial={{ opacity: 0, y: -8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={SETTLE}
                  >
                    <Icon name="warn" className="icon-sm" />
                    <span>{configNotice}</span>
                    <button
                      type="button"
                      className="ghost"
                      onClick={() => {
                        dismissedNoticeRef.current = configNotice;
                        setConfigNotice(null);
                      }}
                    >
                      Verstanden
                    </button>
                  </motion.div>
                )}
                {wizardError && (
                  <motion.div
                    key="wizard-error"
                    className="banner error"
                    layout
                    initial={{ opacity: 0, y: -8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={SETTLE}
                  >
                    <Icon name="warn" className="icon-sm" />
                    <span>{wizardError}</span>
                    <button
                      type="button"
                      className="ghost"
                      onClick={() => {
                        invoke("wizard_dismiss")
                          .then(() => setWizardError(null))
                          .catch((e) => setWizardError(String(e)));
                      }}
                    >
                      Erneut versuchen
                    </button>
                  </motion.div>
                )}
                {/* What replaced the wizard reopening itself on every launch
                    of an install that is not ready (invariant 13, revised
                    2026-09-09). The warning is the same warning; it no longer
                    stands between the user and the other forty settings, and
                    it names its own cause -- `setup` carries both lists, so a
                    missing package cannot read as a missing model. */}
                {wizardState && !wizardState.setup.ready && (
                  <motion.div
                    key="setup-incomplete"
                    className="banner notice"
                    layout
                    initial={{ opacity: 0, y: -8 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0, y: -8 }}
                    transition={SETTLE}
                  >
                    <Icon name="warn" className="icon-sm" />
                    <span>{setupGapSummary(wizardState.setup)}</span>
                    <button type="button" className="add" onClick={() => setWizardActive(true)}>
                      Einrichtung öffnen
                    </button>
                  </motion.div>
                )}
                {restartBanner && (
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
                    {/* No "Verstanden" here any more. Acknowledging used to
                        be the only thing this banner could offer, because
                        nothing in the app could restart it; now that the
                        button next to it works, a dismissal would only hide
                        a requirement that is still owed and leave no way
                        back to it. The restart is the acknowledgement. */}
                    <span>Damit die Änderung greift, muss yappr neu starten.</span>
                    <button
                      type="button"
                      className="add"
                      disabled={!saveSettled || restarting}
                      onClick={() => void restartNow()}
                    >
                      {restarting ? "Startet neu…" : "Jetzt neu starten"}
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
                  {/* Above the `[realtime]` section, not after it: installing
                      the plugin is what a user opens this pane to do, and the
                      section below it is that feature's plumbing. Not rendered
                      while searching, for the same reason `AutostartCard` is
                      not — a search result list is rows that matched a query,
                      and this card matched nothing. */}
                  {!searching && current.id === "ki" && (
                    <OpenClawCard
                      revision={savedRevision}
                      // `openclaw_install`/`openclaw_remove` write
                      // `[realtime]` themselves, so this window's snapshot is
                      // a key out of date the moment either returns — and
                      // `flush` posts that whole snapshot, so the next
                      // unrelated toggle would write the old value back. Same
                      // guards as the reveal listener above: nothing is
                      // re-read on top of an edit this window holds and the
                      // file does not.
                      onConfigChanged={() => {
                        if (timerRef.current !== null || savingRef.current || queuedRef.current)
                          return;
                        if (saveStateRef.current === "error") return;
                        void load(true);
                      }}
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
                      extra={
                        section === "asr" ? (
                          <AsrModelDownload
                            model={String(
                              (config as Record<string, any>)?.asr?.model ?? "",
                            )}
                            revision={savedRevision}
                          />
                        ) : undefined
                      }
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

        <RestartDialog
          open={restartDialogOpen}
          restarting={restarting}
          error={restartError}
          onRestart={() => void restartNow()}
          onDefer={() => setRestartDeferred(true)}
        />
      </main>
    </MotionConfig>
  );
}

/// The restart prompt.
///
/// Why this is a dialog and not just the banner it replaces: until now the
/// app could only *tell* the user a restart was needed and leave them to run
/// `yappr --quit` and start it again by hand. Something the app can do for
/// you is worth interrupting for; something it cannot is not. The banner
/// remains as the standing reminder once this has been answered with
/// "Später", which is why deferring here does not clear the requirement.
///
/// The scrim is the only element in this window that covers the pane, and it
/// is not a blur: this window's own CSS header states that elevation is
/// declared in one direction and nothing here is translucent, so the scrim is
/// a flat dim and the card is the same material as the info popover, one
/// elevation above the sheet.
///
/// Motion follows the file's rule rather than inventing a spring: `SETTLE`,
/// `bounce: 0`. Overshoot is reserved for motion with momentum behind it —
/// the sidebar's travelling selection, a knob between two stops — and a
/// dialog arriving in place has none. It enters as a material, scale plus
/// blur, the same vocabulary `InfoTip` and `SaveCapsule` already use.
function RestartDialog({
  open,
  restarting,
  error,
  onRestart,
  onDefer,
}: {
  open: boolean;
  restarting: boolean;
  error: string | null;
  onRestart: () => void;
  onDefer: () => void;
}) {
  const card = useRef<HTMLDivElement>(null);

  /// Keeps Tab inside the card while the dialog is up.
  ///
  /// This exists because of the `aria-modal="true"` below. The scrim covers
  /// the pane *and* the rail, so a pointer cannot reach what is behind it,
  /// but Tab still could — and a dialog that announces itself as modal to a
  /// screen reader while its focus quietly walks off into the form behind it
  /// is worse than one that never made the claim. Two focusable elements is
  /// the whole cycle, but it is read out of the DOM rather than hardcoded so
  /// adding a third control to the card cannot silently break it.
  const keepTabInside = (e: React.KeyboardEvent) => {
    if (e.key !== "Tab" || !card.current) return;
    const stops = card.current.querySelectorAll<HTMLElement>(
      "button:not(:disabled)",
    );
    if (stops.length === 0) return;
    const first = stops[0];
    const last = stops[stops.length - 1];
    const on = e.shiftKey ? first : last;
    if (document.activeElement === on) {
      e.preventDefault();
      (e.shiftKey ? last : first).focus();
    }
  };

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          key="restart-scrim"
          className="scrim"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={SETTLE}
          onKeyDown={(e) => {
            // Escape defers, like every other cancel affordance in a dialog.
            // `stopPropagation` is load-bearing and not defensive tidiness:
            // this window installs a `keydown` listener on `window` that
            // claims Escape to clear the search query, and without this an
            // Escape aimed at the dialog would also wipe a search the user
            // had running behind it.
            if (e.key === "Escape") {
              e.stopPropagation();
              if (!restarting) onDefer();
              return;
            }
            keepTabInside(e);
          }}
        >
          <motion.div
            ref={card}
            className="dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="restart-title"
            initial={{ opacity: 0, scale: 0.94, y: 8, filter: "blur(8px)" }}
            animate={{ opacity: 1, scale: 1, y: 0, filter: "blur(0px)" }}
            exit={{ opacity: 0, scale: 0.94, y: 8, filter: "blur(8px)" }}
            transition={SETTLE}
          >
            <h2 className="dialog__title" id="restart-title">
              Neustart nötig
            </h2>
            <p className="dialog__body">
              Die Änderung ist gespeichert, greift aber erst, wenn yappr neu
              startet. Das dauert einen Moment; dein Kurzbefehl und deine
              Einstellungen bleiben dabei erhalten.
            </p>
            {error && (
              <p className="dialog__error">
                <Icon name="warn" className="icon-sm" />
                <span>{error}</span>
              </p>
            )}
            <div className="dialog__actions">
              <button type="button" className="ghost" onClick={onDefer} disabled={restarting}>
                Später
              </button>
              {/* Focused on open: this is the action the dialog exists to
                  offer, and it also puts the keyboard inside the dialog so
                  the Escape handler above is the one that sees the key. */}
              <button
                type="button"
                className="add"
                autoFocus
                onClick={onRestart}
                disabled={restarting}
              >
                {restarting ? "Startet neu…" : "Jetzt neu starten"}
              </button>
            </div>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
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
  extra,
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
  /** Rendered after the card. Used by `asr` for the model download row. */
  extra?: React.ReactNode;
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
            title="Diese Einstellungen werden gelesen, wenn die Modelle geladen werden. Sind sie gerade im Speicher, fragt yappr nach dem Speichern nach einem Neustart — sonst greift die Änderung beim nächsten Diktat."
          >
            Neustart möglich
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
      {extra}
    </section>
  );
}
