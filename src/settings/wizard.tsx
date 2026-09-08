import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AnimatePresence, motion } from "motion/react";
import { Icon } from "./icons";

/// The first-run setup wizard (spec §1). It takes over the settings window
/// rather than opening one of its own: that window already exists, already
/// has every command wired, and already comes up on a fresh install.
///
/// It lives here rather than in `Settings.tsx` because that file is a
/// 900-line shell already, and because the wizard shares almost nothing with
/// it — no sidebar, no autosave, no config rows. What it *does* share is the
/// provisioning backend: `setup_status`, `run_setup` and the
/// `setup-progress` event stream are the ones the deleted Setup pane used,
/// unchanged.

/// The house spring, in the two shapes this flow uses. `bounce: 0`
/// throughout: a step change is a discrete navigation with no momentum behind
/// it, and this codebase reserves overshoot for motion that had some.
const STEP = { type: "spring", bounce: 0, duration: 0.36 } as const;
const FADE = { type: "spring", bounce: 0, duration: 0.24 } as const;

export const STEPS = ["welcome", "models", "shortcuts", "done"] as const;
export type Step = (typeof STEPS)[number];

/// `yappr_core::desktop::ShortcutBinding`, unchanged across the wire.
export type WizardShortcutBinding = { name: string; command: string; keys: string };

/// `yappr_core::desktop::ShortcutInstructions`.
export type WizardShortcut = {
  kind: "hypr-lua" | "hypr-conf" | "gnome" | "generic";
  target: string | null;
  snippet: string;
  bindings: WizardShortcutBinding[];
};

/// `wizard::build_wizard_state`'s response shape.
export type WizardState = {
  should_open: boolean;
  start_step: Step;
  desktop: "hyprland" | "gnome" | "other" | "unknown";
  desktop_name: string;
  recommended_backend: string;
  current_backend: string;
  backend_prereqs: string[];
  shortcut: WizardShortcut;
};

/// `provision::MissingModel`, unchanged across the wire.
type MissingModel = { name: string; display: string };

/// `provision::setup_status`'s response shape.
type SetupStatus = {
  ready: boolean;
  missing_prerequisites: string[];
  missing_models: MissingModel[];
};

/// One artifact's live download progress, keyed by `MissingModel.name` — kept
/// only for artifacts a `"setup-progress"` event has actually mentioned, so a
/// model nothing has reported on yet renders as "fehlt" rather than a bar
/// stuck at 0 %.
type DownloadProgress = { display: string; done: number; total: number | null };

/// `provision::SetupProgress`, unchanged across the wire.
type SetupProgressEvent =
  | { kind: "downloading"; name: string; display: string; done: number; total: number | null }
  | { kind: "finished" }
  | { kind: "failed"; message: string };

function downloadStatusText(progress: DownloadProgress | undefined, installing: boolean): string {
  if (!progress) return installing ? "wartet…" : "fehlt";
  if (progress.total !== null) {
    const pct = Math.min(100, Math.round((progress.done / progress.total) * 100));
    return `${pct} %`;
  }
  return `${Math.round(progress.done / (1 << 20))} MB`;
}

/// Everything the Modelle step needs, owned by `Wizard` rather than by the
/// step itself: the `setup-progress` listener has to outlive the step, so a
/// user who walks on to the shortcut step mid-download does not lose the
/// running total — the same reason it used to be scoped to the whole window.
function useSetup() {
  const [status, setStatus] = useState<SetupStatus | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);
  const [downloads, setDownloads] = useState<Record<string, DownloadProgress>>({});

  /// Fails *closed*: `ready: false` with the error carried separately, never
  /// a silent `ready: true`. A fresh install where `setup_status` itself is
  /// broken is exactly when this step needs to be seen.
  const check = useCallback(async () => {
    try {
      const res = (await invoke("setup_status")) as SetupStatus;
      setStatus(res);
      setCheckError(null);
    } catch (e) {
      setStatus({ ready: false, missing_prerequisites: [], missing_models: [] });
      setCheckError(String(e));
    }
  }, []);

  useEffect(() => {
    void check();
  }, [check]);

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
        void check();
      } else if (payload.kind === "failed") {
        setInstalling(false);
        setInstallError(payload.message);
        // An artifact failing partway through does not undo the ones already
        // promoted before it (`download_all`), so what is still missing may
        // be a shorter list than it was.
        void check();
      }
    });
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, [check]);

  const install = useCallback(() => {
    setInstalling(true);
    setInstallError(null);
    setDownloads({});
    invoke("run_setup").catch((e) => {
      // A failure inside `download_all` also arrives as a "failed" event. The
      // reentrancy guard rejects *before* `download_all` runs, though, so no
      // event fires for that one at all — without this the button would stay
      // on "Installation läuft…" forever. The functional update keeps a more
      // specific error the event already reported.
      setInstalling(false);
      setInstallError((prev) => prev ?? String(e));
    });
  }, []);

  return { status, checkError, check, installing, installError, downloads, install };
}

/// The step rail. Not clickable: it reports where you are, it is not a way to
/// jump ahead of a download.
function Dots({ index }: { index: number }) {
  return (
    <div className="wizard-dots" aria-hidden="true">
      {STEPS.map((s, i) => (
        <span
          key={s}
          className={`wizard-dot${i === index ? " is-current" : ""}${i < index ? " is-done" : ""}`}
        />
      ))}
    </div>
  );
}

/// Copies `text` and says so for a moment. `navigator.clipboard` is available
/// in the WebKitGTK webview; the fallback is the text itself, which is
/// already on screen and selectable, so a failure costs the user a keystroke
/// rather than the step.
function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      className="ghost"
      onClick={() => {
        void navigator.clipboard
          .writeText(text)
          .then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1600);
          })
          .catch(() => setCopied(false));
      }}
    >
      <Icon name={copied ? "check" : "copy"} className="icon-sm" />
      <span>{copied ? "Kopiert" : "Kopieren"}</span>
    </button>
  );
}

export function Wizard({
  state,
  onFinish,
  onOpenSettings,
}: {
  state: WizardState;
  /** Passes the backend to write, or `null` to leave `config.toml` alone. */
  onFinish: (setBackend: string | null) => void;
  onOpenSettings: () => void;
}) {
  const [step, setStep] = useState<Step>(state.start_step);
  const index = STEPS.indexOf(step);
  const setup = useSetup();

  // Only a genuine first run writes the injection backend — see
  // `wizard_finish`'s doc comment. `start_step` is the frontend's evidence
  // for that, captured at mount so a marker written by this very click
  // cannot change the answer.
  const firstRun = state.start_step === "welcome";
  const modelsDone = setup.status?.missing_models.length === 0;
  const unsupported = state.desktop === "other" || state.desktop === "unknown";

  return (
    <div className="wizard">
      <Dots index={index} />
      <AnimatePresence mode="wait">
        <motion.div
          key={step}
          className="wizard-step"
          initial={{ opacity: 0, x: 24 }}
          animate={{ opacity: 1, x: 0 }}
          exit={{ opacity: 0, x: -24 }}
          transition={STEP}
        >
          {step === "welcome" && (
            <section className="wizard-body">
              <img className="wizard-mark" src="/yappr.png" alt="" aria-hidden="true" />
              <h1>Willkommen bei yappr</h1>
              <p className="wizard-lead">
                Diktieren in jedes Fenster — vollständig lokal, ohne Cloud. In den
                nächsten drei Schritten lädst du die Sprachmodelle, richtest deinen
                Kurzbefehl ein, und bist fertig.
              </p>
              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("models")}>
                  Los geht’s
                </button>
              </div>
            </section>
          )}

          {step === "models" && (
            <section className="wizard-body">
              <h1>Modelle laden</h1>
              <p className="wizard-lead">
                Spracherkennung, Sprachpausen-Erkennung und Nachbearbeitung laufen
                vollständig auf diesem Rechner. Dafür braucht yappr einmalig etwa
                1,1 GB an Modellen.
              </p>

              {setup.checkError && (
                <div className="banner error">
                  <Icon name="warn" className="icon-sm" />
                  <span>Setup-Status konnte nicht ermittelt werden: {setup.checkError}</span>
                  <button type="button" className="ghost" onClick={() => void setup.check()}>
                    Erneut versuchen
                  </button>
                </div>
              )}

              {setup.status && !setup.checkError && (
                <>
                  {setup.status.missing_prerequisites.length > 0 && (
                    <div className="card">
                      {setup.status.missing_prerequisites.map((pkg) => (
                        <div className="setup-row missing" key={pkg}>
                          <Icon name="warn" className="icon-sm" />
                          <span>{pkg} fehlt.</span>
                        </div>
                      ))}
                      <p className="setup-command">
                        Installieren mit:{" "}
                        <code>
                          sudo pacman -S {setup.status.missing_prerequisites.join(" ")}
                        </code>
                      </p>
                    </div>
                  )}

                  <div className="card">
                    {setup.status.missing_models.length === 0 ? (
                      <div className="setup-row ok">
                        <Icon name="check" className="icon-sm" />
                        <span>Alle Modelle sind vorhanden.</span>
                      </div>
                    ) : (
                      setup.status.missing_models.map((m) => {
                        const progress = setup.downloads[m.name];
                        const done =
                          !!progress && progress.total !== null && progress.done >= progress.total;
                        return (
                          <div className={`setup-row${done ? " ok" : " missing"}`} key={m.name}>
                            <Icon name={done ? "check" : "warn"} className="icon-sm" />
                            <div className="setup-row__body">
                              <div className="setup-row__head">
                                <span>{m.display}</span>
                                <span className="setup-row__status">
                                  {downloadStatusText(progress, setup.installing)}
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
                </>
              )}

              {setup.installError && (
                <div className="banner error">
                  <Icon name="warn" className="icon-sm" />
                  <span>{setup.installError}</span>
                </div>
              )}

              <div className="wizard-actions">
                {/* A running download *does* block the step. It used to not:
                    Weiter stayed live and the transfer continued while the
                    user read the next two steps — which reads as "this is
                    finished" at 3 %, and lands them on the done step with no
                    working dictation. Skipping is still allowed, but only as
                    the deliberate "Später" below, which a running download
                    replaces rather than hides. */}
                {!setup.installing && setup.status && !modelsDone && (
                  <button type="button" className="add" onClick={setup.install}>
                    Jetzt laden
                  </button>
                )}
                <button
                  type="button"
                  className={setup.installing || modelsDone ? "add" : "ghost"}
                  disabled={setup.installing}
                  onClick={() => setStep("shortcuts")}
                >
                  {setup.installing ? "Lädt…" : modelsDone ? "Weiter" : "Später"}
                </button>
              </div>
            </section>
          )}

          {step === "shortcuts" && (
            <section className="wizard-body">
              <h1>Kurzbefehl einrichten</h1>
              <p className="wizard-lead">
                Deine Arbeitsumgebung: <strong>{state.desktop_name}</strong>.{" "}
                {unsupported
                  ? "Offiziell unterstützt sind Hyprland und GNOME — die beiden Befehle unten funktionieren trotzdem, du musst sie nur selbst auf eine Taste legen."
                  : "Wayland kennt keinen globalen Tastatur-Grab, deshalb legt yappr den Kurzbefehl nicht selbst an — du fügst ihn dort ein, wo dein Desktop ihn erwartet."}
              </p>

              {state.shortcut.target && (
                <p className="wizard-target">
                  <Icon name="arrow" className="icon-sm" />
                  <code>{state.shortcut.target}</code>
                </p>
              )}

              {state.shortcut.bindings.length > 0 && (
                <div className="card">
                  {state.shortcut.bindings.map((b) => (
                    <div className="setup-row" key={b.command}>
                      <div className="setup-row__body">
                        <div className="setup-row__head">
                          <span>{b.name}</span>
                          <span className="setup-row__status">
                            <kbd>{b.keys}</kbd>
                          </span>
                        </div>
                        <code className="wizard-command">{b.command}</code>
                      </div>
                    </div>
                  ))}
                </div>
              )}

              <details className="wizard-snippet" open={state.shortcut.bindings.length === 0}>
                <summary>
                  {state.desktop === "gnome" ? "oder per Terminal" : "Diese Zeilen einfügen"}
                </summary>
                <pre className="wizard-pre">{state.shortcut.snippet}</pre>
                <CopyButton text={state.shortcut.snippet} />
              </details>

              <p className="note">
                Texteingabe: <code>{state.recommended_backend}</code>
                {state.recommended_backend === "ydotool"
                  ? " — GNOME (Mutter) unterstützt das Protokoll nicht, über das wtype tippt."
                  : " — braucht keine weitere Einrichtung."}
              </p>

              {state.backend_prereqs.length > 0 && (
                <div className="card">
                  <p className="setup-command">
                    Dafür noch nötig: <code>sudo pacman -S ydotool</code> und{" "}
                    <code>systemctl --user enable --now ydotoold</code>. Fügt yappr danach
                    nichts ein, fehlt meist der Zugriff auf <code>/dev/uinput</code>.
                  </p>
                  <p className="setup-command">
                    Kommt <em>nur in Terminals</em> nichts an, ist es etwas anderes: yappr
                    kann unter GNOME die Fensterklasse nicht ermitteln und drückt deshalb
                    Strg+V statt Strg+Umschalt+V. Dann unter Allgemein → Texteingabe die
                    Einfüge-Tastenkombination fest auf <code>ctrl_shift_v</code> stellen.
                  </p>
                </div>
              )}

              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("done")}>
                  Weiter
                </button>
              </div>
            </section>
          )}

          {step === "done" && (
            <section className="wizard-body">
              <motion.div
                className="wizard-tick"
                initial={{ opacity: 0, scale: 0.8 }}
                animate={{ opacity: 1, scale: 1 }}
                transition={FADE}
              >
                <Icon name="check" className="icon" />
              </motion.div>
              <h1>Alles eingerichtet</h1>
              <p className="wizard-lead">
                Drücke <kbd>Super</kbd>+<kbd>D</kbd>, sprich, und drücke noch einmal.
                yappr läuft weiter im Systemtray.
              </p>
              <div className="wizard-actions">
                <button
                  type="button"
                  className="add"
                  onClick={() => onFinish(firstRun ? state.recommended_backend : null)}
                >
                  Fertig
                </button>
                <button type="button" className="ghost" onClick={onOpenSettings}>
                  Einstellungen öffnen
                </button>
              </div>
            </section>
          )}
        </motion.div>
      </AnimatePresence>
    </div>
  );
}
