/// The download affordance for a model chosen in Settings rather than in the
/// wizard.
///
/// A dropdown autosaves the moment it changes (invariant 9), so the Sprache
/// pane can be left naming a model that is not on disk. Only the *selected*
/// model is ever downloaded (spec asr-model §4), which makes that an ordinary
/// state rather than a broken install -- but the user has to be told here,
/// rather than finding out when the next dictation fails.
///
/// Reuses the wizard's `useSetup` wholesale: same `setup_status`, same
/// `run_setup`, same `setup-progress` listener, same single-flight guard. A
/// second implementation of the download UI is exactly how the two would
/// drift.
import { useEffect } from "react";

import { Icon } from "./icons";
import { useSetup } from "./wizard";

export function AsrModelDownload({ model, revision }: { model: string; revision: number }) {
  const setup = useSetup();
  const { check } = setup;

  // `revision` is bumped by a *successful* save, not by the local dropdown
  // value. `setup_status` answers from `config.toml` on disk, so re-checking
  // on `model` alone would race the save still in flight and report the
  // previous selection.
  useEffect(() => {
    void check();
  }, [check, model, revision]);

  const missing = setup.status?.missing_models ?? [];
  // A failed check is the Setup pane's business to report, not this row's:
  // staying silent here is better than claiming a model is missing because
  // the check itself broke.
  if (setup.checkError || missing.length === 0) return null;

  return (
    <div className="banner notice">
      <Icon name="warn" className="icon-sm" />
      <div className="setup-row__body">
        <span>{missing.map((m) => m.display).join(", ")} — noch nicht heruntergeladen.</span>
        {missing.map((m) => {
          const progress = setup.downloads[m.name];
          if (!progress) return null;
          const pct =
            progress.total === null ? null : Math.round((progress.done / progress.total) * 100);
          return (
            <span key={m.name} className="setup-row__head">
              {m.display}: {pct === null ? "lädt…" : `${pct} %`}
            </span>
          );
        })}
        {setup.installError && <span className="error">{setup.installError}</span>}
      </div>
      <button
        type="button"
        className="add"
        disabled={setup.installing}
        onClick={() => setup.install()}
      >
        {setup.installing ? "Lädt…" : "Jetzt laden"}
      </button>
    </div>
  );
}
