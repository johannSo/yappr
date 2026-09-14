/// The OpenClaw card in the KI pane.
///
/// Like `Settings.tsx`'s `AutostartCard`, and for the same reason: what this
/// card reports is not a `config.toml` section. It is the state of *another
/// program* on this machine — whether its CLI exists, whether the yappr
/// plugin is linked into it, whether it has yappr selected as its dictation
/// provider — plus the one fact about yappr's own endpoint that the config
/// cannot answer (`realtime.running`: the key can say `enabled = true` while
/// the listener is not up, because the port is taken). None of that can be
/// mirrored into a config key without immediately being free to disagree with
/// the thing it mirrors, so it is asked for directly and rendered alongside
/// `shown.map` rather than through it.
///
/// Three commands, all in `openclaw_*`: `openclaw_status` is cheap and
/// read-only, `openclaw_install` does the work and `openclaw_remove` undoes
/// it. The two writing ones answer with a list of steps *and* a fresh status,
/// so nothing here has to guess what a partial failure left behind.
///
/// No motion. This window's springs are declared per file as named consts and
/// reused, never invented — and this card is the same shape as
/// `AsrModelDownload` and `AutostartCard`, neither of which animates. A state
/// readout that slides in is a state readout you read a moment later.
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { Icon } from "./icons";
import { titleFor } from "./schema";

/// `openclaw_status`'s response, unchanged across the wire.
export type OpenClawStatus = {
  /** The `openclaw` CLI itself. Nothing else here matters if `found` is false. */
  cli: { found: boolean; path: string | null; version: string | null; error: string | null };
  /** The yappr plugin: materialised on disk (`dir`), and linked into OpenClaw. */
  plugin: { installed: boolean; dir: string; linked: boolean };
  /** OpenClaw's own config: the provider written, actually chosen, and
   *  still in step with `[realtime]` — `stale` means OpenClaw is pointed at
   *  a port or a token this app no longer uses, which is invisible from
   *  either side on its own. */
  provider: { configured: boolean; selected: boolean; stale: boolean };
  /** yappr's side: the `[realtime]` key, whether the listener is really up,
   *  and, when it is not, what stopped it. */
  realtime: { enabled: boolean; port: number; running: boolean; error: string | null };
};

/// One line of what `openclaw_install`/`openclaw_remove` did. `ok: false` is
/// not necessarily a failed run — a step may be refused while the rest
/// succeed, which is the whole reason these are rendered rather than reduced
/// to a boolean.
type RunStep = { label: string; ok: boolean; detail: string };

type RunResult = { steps: RunStep[]; status: OpenClawStatus };

/// Everything in place: the plugin is there, OpenClaw has chosen yappr, and
/// yappr's endpoint is both switched on and actually listening. Four
/// conditions rather than one flag because each of them can be false on its
/// own, and each reads differently to the user.
function isComplete(s: OpenClawStatus): boolean {
  return (
    s.plugin.installed &&
    s.provider.selected &&
    !s.provider.stale &&
    s.realtime.enabled &&
    s.realtime.running
  );
}

/// The one line at the top of the card. Ordered from "nothing to work with"
/// down to "done", so a partial state is described by the first thing that is
/// actually missing rather than by the extreme it is nearest to.
function headline(s: OpenClawStatus): { tone: "ok" | "todo" | "warn"; text: string } {
  if (!s.cli.found) {
    return { tone: "warn", text: "OpenClaw wurde auf diesem Rechner nicht gefunden." };
  }
  if (!s.plugin.installed) {
    return { tone: "todo", text: "Das yappr-Plugin ist in OpenClaw noch nicht eingerichtet." };
  }
  if (!s.provider.selected) {
    return {
      tone: "todo",
      text: s.provider.configured
        ? "Das Plugin ist installiert und yappr ist in OpenClaws Konfiguration eingetragen, aber dort nicht als Diktat-Anbieter ausgewählt."
        : "Das Plugin ist installiert, aber yappr ist in OpenClaw nicht als Diktat-Anbieter eingetragen.",
    };
  }
  if (s.provider.stale) {
    // Nothing is broken on either side's own terms — OpenClaw has a
    // provider entry, yappr has a listener — and dictation still fails,
    // because the entry names the port or the token from before the last
    // change here. The only state in this card that looks finished and is
    // not.
    return {
      tone: "warn",
      text: `OpenClaw ist noch auf die früheren Zugangsdaten eingetragen (Port oder Zugangsschlüssel wurden hier geändert). „Erneut einrichten“ schreibt sie neu.`,
    };
  }
  if (!s.realtime.enabled) {
    return {
      tone: "todo",
      text: "OpenClaw ist auf yappr eingestellt, der lokale Zugang unten ist aber ausgeschaltet — es kommt nichts an.",
    };
  }
  if (!s.realtime.running) {
    // The state that looks fine in `config.toml` and works nowhere. Named
    // with both plausible causes and pointed at the section directly below,
    // which is the only place the port can be changed.
    return {
      tone: "warn",
      text: s.realtime.error
        ? `Der Zugang ist eingeschaltet, lauscht aber nicht: ${s.realtime.error}. Unter „${titleFor("realtime")}“ lässt sich eine andere Portnummer eintragen.`
        : `Der Zugang ist eingeschaltet, lauscht aber nicht auf Port ${s.realtime.port}. Unter „${titleFor("realtime")}“ lässt sich eine andere Nummer eintragen.`,
    };
  }
  return {
    tone: "ok",
    text: `Eingerichtet: OpenClaw diktiert über yappr, der Zugang lauscht auf Port ${s.realtime.port}.`,
  };
}

/// A readout row. Same shape, and the same classes, as the wizard's setup
/// rows — ok is green, a gap is amber rather than red, because a step not yet
/// taken is not a failure. `wizard-command` is borrowed rather than renamed:
/// it sets a size and an opacity and nothing else, so on a `<code>` it is the
/// wizard's dimmed path and on a `<span>` it is the same dimmed prose.
function Fact({
  ok,
  label,
  status,
  detail,
  mono = false,
}: {
  ok: boolean;
  label: string;
  status: string;
  /** A path, a directory, or a backend's own sentence about what went wrong. */
  detail?: string | null;
  /** Set for something that is copied or typed, not read. */
  mono?: boolean;
}) {
  return (
    <div className={`setup-row${ok ? " ok" : " missing"}`}>
      <Icon name={ok ? "check" : "warn"} className="icon-sm" />
      <div className="setup-row__body">
        <div className="setup-row__head">
          <span>{label}</span>
          <span className="setup-row__status">{status}</span>
        </div>
        {detail &&
          (mono ? (
            <code className="wizard-command">{detail}</code>
          ) : (
            <span className="wizard-command">{detail}</span>
          ))}
      </div>
    </div>
  );
}

export function OpenClawCard({
  revision,
  onConfigChanged,
}: {
  /** Bumped by every successful save, so flipping `realtime.enabled` in the
   *  section below re-checks whether the listener actually came up. */
  revision: number;
  /** Called after a run that may have written `config.toml` — see the call
   *  site in `Settings.tsx` for why this window has to hear about it. */
  onConfigChanged: () => void;
}) {
  // `null` is "not asked yet", deliberately distinct from every real answer:
  // this card's whole job is making claims about another program, and the
  // worst of them ("OpenClaw wurde nicht gefunden") is also the one a
  // zero-value default would flash on every mount.
  const [status, setStatus] = useState<OpenClawStatus | null>(null);
  const [steps, setSteps] = useState<RunStep[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"install" | "remove" | null>(null);
  // The single-flight guard proper. `busy` dims the buttons, but two clicks
  // inside one render both read the same `busy === null`; this does not.
  const runningRef = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const res = (await invoke("openclaw_status")) as OpenClawStatus;
      setStatus(res);
      setError(null);
    } catch (e) {
      // Nothing is claimed on a failed check. The card says the check itself
      // broke and offers a retry; inventing a `found: false` here would blame
      // OpenClaw for yappr's own error.
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh, revision]);

  const run = useCallback(
    async (which: "install" | "remove") => {
      if (runningRef.current) return;
      runningRef.current = true;
      setBusy(which);
      setError(null);
      setSteps(null);
      try {
        const res = (await invoke(
          which === "install" ? "openclaw_install" : "openclaw_remove",
        )) as RunResult;
        setSteps(res.steps);
        setStatus(res.status);
      } catch (e) {
        // The German string the command rejected with, shown as it arrived.
        // The status is re-read afterwards either way: a hard failure can
        // still have got halfway, and the rows below have to say where it
        // stopped rather than what they showed before the press.
        setError(String(e));
        await refresh();
      } finally {
        runningRef.current = false;
        setBusy(null);
        // Both commands touch yappr's own `[realtime] enabled`, so this
        // window's config snapshot is stale the moment either returns.
        onConfigChanged();
      }
    },
    [onConfigChanged, refresh],
  );

  const head = status ? headline(status) : null;
  const complete = status ? isComplete(status) : false;
  const canInstall = !!status && status.cli.found && busy === null;

  return (
    <section className="group">
      <div className="group-head">
        <h2>OpenClaw</h2>
      </div>
      <p className="note">
        OpenClaw ist ein eigenes, lokal installiertes Programm. Ist das Plugin
        eingerichtet, diktierst du dort über yappr — dieselbe Spracherkennung und
        dieselbe Nachbearbeitung wie hier, ohne Cloud.
      </p>

      <div className="card">
        {/* Nothing is asserted until the first `openclaw_status` has answered.
            A card that renders its zero values first would claim OpenClaw is
            missing for as long as the check takes, on every machine. */}
        {!status ? (
          <div className="setup-row">
            <span>Wird geprüft…</span>
          </div>
        ) : !status.cli.found ? (
          <Fact ok={false} label="OpenClaw" status="nicht gefunden" detail={status.cli.error} />
        ) : (
          <>
            <Fact
              ok
              label="OpenClaw"
              status={status.cli.version ?? "gefunden"}
              detail={status.cli.path}
              mono
            />
            <Fact
              ok={status.plugin.installed}
              label="yappr-Plugin"
              status={
                status.plugin.installed
                  ? status.plugin.linked
                    ? "eingerichtet"
                    : "vorhanden, nicht verknüpft"
                  : "nicht eingerichtet"
              }
              detail={status.plugin.installed ? status.plugin.dir : null}
              mono
            />
            <Fact
              ok={status.provider.selected && !status.provider.stale}
              label="Diktat-Anbieter in OpenClaw"
              status={
                status.provider.selected
                  ? status.provider.stale
                    ? "yappr, aber veraltete Zugangsdaten"
                    : "yappr"
                  : status.provider.configured
                    ? "eingetragen, nicht ausgewählt"
                    : "nicht eingetragen"
              }
            />
            <Fact
              ok={status.realtime.enabled && status.realtime.running}
              label="Lokaler Zugang"
              status={
                !status.realtime.enabled
                  ? "aus"
                  : status.realtime.running
                    ? `lauscht auf Port ${status.realtime.port}`
                    : `an, aber nicht erreichbar (Port ${status.realtime.port})`
              }
              detail={status.realtime.enabled ? status.realtime.error : null}
            />
          </>
        )}
      </div>

      {/* The one-line verdict. A state that is merely unfinished says so in
          the same quiet voice as the explanation under it; a state that looks
          set up and is not (no CLI, or a listener that did not come up) gets
          the amber banner, because that is the one a user would otherwise
          walk away from believing it worked. */}
      {head &&
        (head.tone === "warn" ? (
          <div className="banner notice">
            <Icon name="warn" className="icon-sm" />
            <span>{head.text}</span>
          </div>
        ) : (
          <p className="setup-command">{head.text}</p>
        ))}

      {status && !status.cli.found && (
        <p className="setup-command">
          OpenClaw wird getrennt von yappr installiert, zum Beispiel mit{" "}
          <code>npm i -g openclaw</code>. Danach hier erneut prüfen.
        </p>
      )}

      {status && status.cli.found && !complete && (
        <p className="setup-command">
          „Einrichten“ legt das yappr-Plugin an, verknüpft es mit OpenClaw, trägt
          yappr dort als Diktat-Anbieter ein und schaltet den lokalen Zugang unten
          ein. Dabei wird die Konfigurationsdatei von OpenClaw geschrieben.
        </p>
      )}

      {busy !== null && (
        <p className="setup-command">
          Das dauert ein paar Sekunden — OpenClaw wird dabei mehrfach aufgerufen.
        </p>
      )}

      {/* The only place a partial run is visible. Kept whole, failed steps
          included: "installiert, aber die Konfiguration wurde abgelehnt" is
          exactly the outcome a collapsed success message would hide. */}
      {steps && (
        <>
        <p className="note">Letzter Durchlauf</p>
        <div className="card">
          {steps.map((s, i) => (
            <Fact
              key={`${i}-${s.label}`}
              ok={s.ok}
              label={s.label}
              status={s.ok ? "erledigt" : "nicht erledigt"}
              detail={s.detail || null}
            />
          ))}
        </div>
        </>
      )}

      {error && (
        <div className="banner error">
          <Icon name="warn" className="icon-sm" />
          <span>{error}</span>
          <button type="button" className="ghost" onClick={() => void refresh()}>
            Erneut prüfen
          </button>
        </div>
      )}

      <div className="card-actions">
        {/* Offered whenever the plugin is on disk, not only when everything is
            in place: a half-finished install is exactly the state someone
            wants to undo, and hiding the way out of it until it is complete
            would be backwards. */}
        {status?.cli.found && status.plugin.installed && (
          <button
            type="button"
            className="ghost"
            disabled={busy !== null}
            onClick={() => void run("remove")}
          >
            {busy === "remove" ? "Entfernt…" : "Entfernen"}
          </button>
        )}
        <button
          type="button"
          className="add"
          disabled={!canInstall}
          onClick={() => void run("install")}
        >
          {/* Keyed on the plugin being there, not on everything being in
              place: a re-run is exactly what a half-finished install and a
              stale provider entry both need, and a button that still said
              "Einrichten" would read as "start over" rather than "fix
              this". The headline above names it by this label. */}
          {busy === "install"
            ? "Richtet ein…"
            : status?.plugin.installed
              ? "Erneut einrichten"
              : "Einrichten"}
        </button>
      </div>
    </section>
  );
}
