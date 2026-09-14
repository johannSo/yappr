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

/// The selectable ASR models, in catalogue order. Kept next to `STEPS`
/// rather than in `schema.ts` because the wizard renders its own `<select>`
/// -- `schema.ts`'s ENUMS drives the settings form only. The spellings must
/// match `models::ASR_MODELS`; an unknown one is rejected by the config's
/// deny_unknown_fields on save rather than silently defaulted.
export const ASR_MODELS: { value: string; label: string }[] = [
  { value: "parakeet-tdt-v3", label: "Parakeet TDT v3 — multilingual (default)" },
  { value: "parakeet-primeline-de", label: "primeline Parakeet — German only, most accurate" },
  { value: "parakeet-unified-en", label: "Parakeet Unified — English only" },
  { value: "nemotron-3.5", label: "Nemotron 3.5 — multilingual" },
];

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
  /** Whether the wizard opens at all: a first run, or an explicit request. */
  should_open: boolean;
  /**
   * `provision::setup_status`'s answer, passed through whole rather than
   * reduced to a flag — `Settings.tsx`'s banner has to *name* the gap, and a
   * missing package is not a missing model. See `wizard::should_open`.
   */
  setup: SetupStatus;
  start_step: Step;
  desktop: "hyprland" | "gnome" | "other" | "unknown";
  desktop_name: string;
  recommended_backend: string;
  current_backend: string;
  backend_prereqs: string[];
  shortcut: WizardShortcut;
};

/// `provision::MissingModel`, unchanged across the wire.
export type MissingModel = { name: string; display: string };

/// `provision::setup_status`'s response shape.
export type SetupStatus = {
  ready: boolean;
  missing_prerequisites: string[];
  missing_models: MissingModel[];
};

/// What to tell the user when `setup.ready` is false, naming the actual gap.
///
/// The wording used to blame the selected ASR model for every cause, which
/// on a machine missing only a *package* read as "the model is missing" beside
/// a models step reporting "All models are present" — the contradiction
/// that got the startup gate narrowed (`wizard::should_open`). The
/// cause-unknown arm is `provision::status_or_assume_incomplete`'s error
/// case: both lists empty and `ready` still false. Saying so is the point;
/// guessing a cause is what this replaced.
export function setupGapSummary(setup: SetupStatus): string {
  const models = setup.missing_models.map((m) => m.display);
  const pkgs = setup.missing_prerequisites;
  if (models.length === 0 && pkgs.length === 0) {
    return "Setup is incomplete — the cause could not be determined.";
  }
  const parts: string[] = [];
  if (models.length > 0) {
    parts.push(
      models.length === 1
        ? `the model ${models[0]} is still missing`
        : `the models ${models.join(", ")} are still missing`,
    );
  }
  if (pkgs.length > 0) {
    parts.push(
      pkgs.length === 1
        ? `the program ${pkgs[0]} is still missing`
        : `the programs ${pkgs.join(", ")} are still missing`,
    );
  }
  return `Setup is incomplete — ${parts.join("; ")}. Until then every dictation fails.`;
}

/// One artifact's live download progress, keyed by `MissingModel.name` — kept
/// only for artifacts a `"setup-progress"` event has actually mentioned, so a
/// model nothing has reported on yet renders as "missing" rather than a bar
/// stuck at 0 %.
export type DownloadProgress = { display: string; done: number; total: number | null };

/// `provision::SetupProgress`, unchanged across the wire.
type SetupProgressEvent =
  | { kind: "downloading"; name: string; display: string; done: number; total: number | null }
  | { kind: "finished" }
  | { kind: "failed"; message: string };

function downloadStatusText(progress: DownloadProgress | undefined, installing: boolean): string {
  if (!progress) return installing ? "waiting…" : "missing";
  if (progress.total !== null) {
    const pct = Math.min(100, Math.round((progress.done / progress.total) * 100));
    return `${pct} %`;
  }
  return `${Math.round(progress.done / (1 << 20))} MB`;
}

/// Everything the models step needs, owned by `Wizard` rather than by the
/// step itself: the `setup-progress` listener has to outlive the step, so a
/// user who walks on to the shortcut step mid-download does not lose the
/// running total — the same reason it used to be scoped to the whole window.
export function useSetup() {
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
      // on "Installing…" forever. The functional update keeps a more
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
      <span>{copied ? "Copied" : "Copy"}</span>
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
  const [asrModel, setAsrModel] = useState("parakeet-tdt-v3");
  const { check } = setup;

  // Read the current selection rather than assuming the default: this wizard
  // reopens whenever the selected model is missing (invariant 13's
  // `should_open`), which includes a machine that already chose one.
  useEffect(() => {
    void (async () => {
      try {
        const res = (await invoke("get_config")) as {
          config?: { asr?: { model?: string } };
        };
        const current = res.config?.asr?.model;
        if (current) setAsrModel(current);
      } catch {
        // Leave the default showing; the step still works.
      }
    })();
  }, []);

  // Writes the selection, then re-runs the status check so the missing-model
  // list below is about the model just picked. A single-leaf patch, which
  // `config_write::save_config` merges rather than renders -- the same path
  // `wizard_finish`'s backend patch takes (invariant 9).
  const chooseModel = useCallback(
    async (model: string) => {
      setAsrModel(model);
      try {
        await invoke("set_config", { config: { asr: { model } } });
      } catch (e) {
        console.error("set_config(asr.model) failed", e);
      }
      await check();
    },
    [check],
  );

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
              <h1>Welcome to yappr</h1>
              <p className="wizard-lead">
                Dictation into any window — entirely local, no cloud. Over the next
                three steps you download the models, set up your shortcut, and
                you're done.
              </p>
              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("models")}>
                  Let’s go
                </button>
              </div>
            </section>
          )}

          {step === "models" && (
            <section className="wizard-body">
              <h1>Download the models</h1>
              <p className="wizard-lead">
                Speech recognition, speech detection and post-processing all run
                entirely on this machine. For that, yappr needs about 1.1 GB of
                models, once.
              </p>

              <div className="card">
                <label className="setup-row" htmlFor="wizard-asr-model">
                  <span>Speech recognition model</span>
                  <select
                    id="wizard-asr-model"
                    value={asrModel}
                    disabled={setup.installing}
                    onChange={(e) => void chooseModel(e.target.value)}
                  >
                    {ASR_MODELS.map((m) => (
                      <option key={m.value} value={m.value}>
                        {m.label}
                      </option>
                    ))}
                  </select>
                </label>
                <p className="setup-command">
                  Only the selected model is downloaded. Switching later in the
                  settings fetches the new one then.
                </p>
              </div>

              {setup.checkError && (
                <div className="banner error">
                  <Icon name="warn" className="icon-sm" />
                  <span>Could not determine the setup status: {setup.checkError}</span>
                  <button type="button" className="ghost" onClick={() => void setup.check()}>
                    Try again
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
                          <span>{pkg} is missing.</span>
                        </div>
                      ))}
                      <p className="setup-command">
                        Install with:{" "}
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
                        <span>All models are present.</span>
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
                    the deliberate "Later" below, which a running download
                    replaces rather than hides. */}
                {!setup.installing && setup.status && !modelsDone && (
                  <button type="button" className="add" onClick={setup.install}>
                    Download now
                  </button>
                )}
                <button
                  type="button"
                  className={setup.installing || modelsDone ? "add" : "ghost"}
                  disabled={setup.installing}
                  onClick={() => setStep("shortcuts")}
                >
                  {setup.installing ? "Downloading…" : modelsDone ? "Continue" : "Later"}
                </button>
                {/* A returning user reaches this step from the settings
                    banner, for one errand: load the model. Making them walk
                    the remaining two steps to get back is what this whole
                    flow was just changed to stop doing. Not offered on a
                    first run -- there is no settings form to go back to yet,
                    and the shortcut step is the point of the exercise. */}
                {!firstRun && (
                  <button
                    type="button"
                    className="ghost"
                    disabled={setup.installing}
                    onClick={onOpenSettings}
                  >
                    Settings
                  </button>
                )}
              </div>
            </section>
          )}

          {step === "shortcuts" && (
            <section className="wizard-body">
              <h1>Set up the shortcut</h1>
              <p className="wizard-lead">
                Your desktop: <strong>{state.desktop_name}</strong>.{" "}
                {unsupported
                  ? "Hyprland and GNOME are the officially supported ones — the two commands below still work, you just have to bind them to a key yourself."
                  : "Wayland has no global keyboard grab, so yappr does not create the shortcut itself — you add it wherever your desktop expects it."}
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
                  {state.desktop === "gnome" ? "or from a terminal" : "Paste these lines"}
                </summary>
                <pre className="wizard-pre">{state.shortcut.snippet}</pre>
                <CopyButton text={state.shortcut.snippet} />
              </details>

              <p className="note">
                Text entry: <code>{state.recommended_backend}</code>
                {state.recommended_backend === "clipboard"
                  ? " — GNOME (Mutter) does not support the protocol wtype types through. yappr puts the text on the clipboard instead; you paste it yourself with Ctrl+V."
                  : " — needs no further setup."}
              </p>

              {state.recommended_backend === "clipboard" && (
                <div className="card">
                  <p className="setup-command">
                    Pasting automatically is still possible, under General → Text entry
                    → Method. <code>ydotool</code> presses the paste itself; that needs
                    the <code>ydotool</code> package installed and{" "}
                    <code>ydotoold</code> running, with write access to{" "}
                    <code>/dev/uinput</code>. <code>script</code> hands the finished
                    text to a program of your own instead (path under{" "}
                    <em>Paste script</em>), as its first argument (<code>$1</code>);
                    everything after that — clipboard, key chord, window detection — is
                    then up to the script.
                  </p>
                </div>
              )}

              <div className="wizard-actions">
                <button type="button" className="add" onClick={() => setStep("done")}>
                  Continue
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
              <h1>All set up</h1>
              <p className="wizard-lead">
                Press <kbd>Super</kbd>+<kbd>D</kbd>, speak, and press again. yappr
                keeps running in the system tray.
              </p>
              <div className="wizard-actions">
                <button
                  type="button"
                  className="add"
                  onClick={() => onFinish(firstRun ? state.recommended_backend : null)}
                >
                  Done
                </button>
                <button type="button" className="ghost" onClick={onOpenSettings}>
                  Open settings
                </button>
              </div>
            </section>
          )}
        </motion.div>
      </AnimatePresence>
    </div>
  );
}
