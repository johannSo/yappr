import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, type Window as TauriWindow } from "@tauri-apps/api/window";
import "./Overlay.css";

// Wire shape emitted by the Rust backend (`src-tauri/src/wire.rs`), which
// itself mirrors `owf_core::proto::OverlayEvent` (crates/owf-core/src/proto.rs).
// This is the *only* place the frontend knows about the pipeline: it never
// decides state transitions, timing budgets, or retries -- it renders
// whatever it's told (spec 12).
type OverlayEvent =
  | { event: "warming" }
  | { event: "idle" }
  | { event: "recording"; level: number; elapsed_ms: number }
  | { event: "transcribing" }
  | { event: "normalizing" }
  | { event: "injecting" }
  | { event: "done"; preview: string }
  | { event: "error"; reason: string }
  | { event: "busy_rejected" }
  // Spec 15 (M2 Task 3): normalization availability, independent of the
  // pipeline's own state -- rendered as a small persistent badge alongside
  // whatever the state-driven pill is already showing, never in place of it.
  | { event: "normalize_degraded"; reason: string }
  | { event: "normalize_recovered" };

// How many of the most recent `recording` levels to keep for the bar
// meter. At the spec's ~50ms cadence this is roughly 1.4s of history --
// enough to look alive without the bars feeling like a full waveform.
const RECORDING_HISTORY = 28;

// Presentation durations from spec 12's table. These are UI-only timers
// (how long a flash stays on screen), not pipeline behaviour, so they live
// here rather than in Rust.
const DONE_FLASH_MS = 800;
const ERROR_FLASH_MS = 2000;
const BUSY_FLASH_MS = 400;
const PREVIEW_MAX_CHARS = 60;

type ViewState =
  | { kind: "hidden" }
  | { kind: "warming" }
  | { kind: "recording"; levels: number[]; elapsedMs: number }
  | { kind: "transcribing" }
  | { kind: "normalizing" }
  | { kind: "injecting" }
  | { kind: "done"; preview: string }
  | { kind: "error"; reason: string }
  | { kind: "busy" };

// Wayland gives a window no monitor to query until it has been mapped at
// least once (there is no X11-style "primary monitor" concept), so the
// bottom-centre position (spec 12) can only be computed *after* `show()` --
// see the Rust-side doc comment on `position_overlay`. Re-invoking it on
// every show is cheap and self-corrects if the window migrates outputs.
function showAndPosition(win: TauriWindow) {
  win
    .show()
    .then(() => invoke("position_overlay"))
    .catch(() => {});
}

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max).trimEnd()}…`;
}

/// RMS levels from the capture thread run roughly 0..0.3 for normal speech.
/// Map that onto a visible bar-height percentage with a floor, so a quiet
/// moment still shows a faint "listening" bar rather than a flat line.
function levelToHeightPercent(level: number): number {
  const clamped = Math.max(0, Math.min(1, level / 0.3));
  return 10 + clamped * 90;
}

export default function Overlay() {
  const [view, setView] = useState<ViewState>({ kind: "hidden" });
  // Independent of `view`: spec 15's normalization-availability badge is not
  // a pipeline state, and must not replace whatever `view` is already
  // showing (see the `OverlayEvent` union's comment).
  const [degraded, setDegraded] = useState<string | null>(null);
  const levelsRef = useRef<number[]>([]);
  const hideTimer = useRef<number | undefined>(undefined);
  // Whether the window is currently shown-and-positioned. `recording` events
  // arrive at spec 7.1's ~50ms cadence (~20/s) for the whole duration of a
  // dictation; calling `showAndPosition` -- a `win.show()` IPC round trip
  // plus `position_overlay`, itself documented as a Wayland no-op today --
  // on every single one of them bought nothing (the window was already
  // shown and positioned) for real IPC cost. `showAndPosition` is the "show
  // it" primitive; `showOnce` below is "show it, only if it isn't already".
  const visible = useRef(false);

  useEffect(() => {
    const win = getCurrentWindow();
    // Belt-and-braces: tauri.conf.json already creates this window hidden.
    win.hide().catch(() => {});

    const clearHideTimer = () => {
      if (hideTimer.current !== undefined) {
        window.clearTimeout(hideTimer.current);
        hideTimer.current = undefined;
      }
    };

    const showOnce = () => {
      if (visible.current) return;
      visible.current = true;
      showAndPosition(win);
    };

    const hide = () => {
      visible.current = false;
      setView({ kind: "hidden" });
      win.hide().catch(() => {});
    };

    const scheduleHide = (ms: number) => {
      hideTimer.current = window.setTimeout(hide, ms);
    };

    const unlistenPromise = listen<OverlayEvent>("overlay-event", ({ payload }) => {
      // Fix 1/4: `clearHideTimer()` used to run here, unconditionally,
      // before the switch below -- so *every* event, including a plain
      // `idle`, cancelled whatever flash timer `done`/`error`/`busy_rejected`
      // had just armed, before a single frame painted. It is now called only
      // from the branches below that actually need it: the ones that set a
      // new `view` (a stale timer left ticking would otherwise fire mid-way
      // through that new state and wrongly hide the window), and explicitly
      // *not* from `idle` -- a redundant `Idle` racing in behind a terminal
      // event (or one that slips past the daemon-side fix in `owf-daemon.rs`)
      // must not be able to cut a flash short. It is also not called from
      // the two `normalize_*` branches or the `default` case, neither of
      // which touch `view` at all.
      switch (payload.event) {
        case "warming":
          clearHideTimer();
          levelsRef.current = [];
          setView({ kind: "warming" });
          showOnce();
          break;

        case "idle":
          // Only act when nothing is currently flashing: a pending timer
          // means `done`/`error`/`busy_rejected` already decided when this
          // window hides next, and a redundant `idle` must not override that
          // (fix 1).
          if (hideTimer.current === undefined) {
            levelsRef.current = [];
            hide();
          }
          break;

        case "recording": {
          clearHideTimer();
          const levels = [...levelsRef.current, payload.level].slice(-RECORDING_HISTORY);
          levelsRef.current = levels;
          setView({ kind: "recording", levels, elapsedMs: payload.elapsed_ms });
          showOnce();
          break;
        }

        case "transcribing":
          clearHideTimer();
          setView({ kind: "transcribing" });
          showOnce();
          break;

        case "normalizing":
          clearHideTimer();
          setView({ kind: "normalizing" });
          showOnce();
          break;

        case "injecting":
          clearHideTimer();
          setView({ kind: "injecting" });
          showOnce();
          break;

        case "done":
          clearHideTimer();
          setView({ kind: "done", preview: truncate(payload.preview, PREVIEW_MAX_CHARS) });
          showOnce();
          scheduleHide(DONE_FLASH_MS);
          break;

        case "error":
          clearHideTimer();
          setView({ kind: "error", reason: payload.reason });
          showOnce();
          scheduleHide(ERROR_FLASH_MS);
          break;

        case "busy_rejected":
          clearHideTimer();
          setView({ kind: "busy" });
          showOnce();
          scheduleHide(BUSY_FLASH_MS);
          break;

        case "normalize_degraded":
          setDegraded(payload.reason);
          break;

        case "normalize_recovered":
          setDegraded(null);
          break;

        default:
          // Forward-compatible (fix 4): an event this build doesn't know
          // about yet must be a no-op, never fall through to cancelling a
          // pending timer or otherwise touching `view` -- a build that
          // predates a new variant must not risk a stuck, unclosable pill
          // over one it can't render.
          break;
      }
    });

    return () => {
      unlistenPromise.then((unlisten) => unlisten());
      clearHideTimer();
    };
  }, []);

  // Idle renders nothing at all: the window is natively hidden (spec 12 --
  // "hidden when idle", not a transparent rectangle sitting there able to
  // catch clicks), and an empty page means no stray paint while it's
  // transitioning to hidden.
  if (view.kind === "hidden") return null;

  return (
    <div className="pill-wrap">
      <div className={`pill pill--${view.kind}`} role="status" aria-live="polite">
        <PillContent view={view} />
      </div>
      {/* Spec 15: a persistent badge alongside whatever the state-driven
          pill above is already showing, not a replacement for it -- see the
          `OverlayEvent` union's comment. */}
      {degraded !== null && (
        <span className="badge badge--degraded" role="status" title={degraded} aria-label={degraded} />
      )}
    </div>
  );
}

function PillContent({ view }: { view: ViewState }) {
  switch (view.kind) {
    case "warming":
      return (
        <>
          <Spinner />
          <span className="pill__label">loading models</span>
        </>
      );

    case "recording":
      return (
        <>
          <Bars levels={view.levels} />
          <span className="pill__timer">{(view.elapsedMs / 1000).toFixed(1)}s</span>
        </>
      );

    case "transcribing":
      return (
        <>
          <Spinner />
          <span className="pill__label">transcribing</span>
        </>
      );

    case "normalizing":
      return (
        <>
          <Spinner />
          <span className="pill__label">cleaning</span>
        </>
      );

    case "injecting":
      return (
        <>
          <Spinner />
          <span className="pill__label">typing</span>
        </>
      );

    case "done":
      return <span className="pill__preview">{view.preview}</span>;

    case "error":
      return <span className="pill__label">{view.reason}</span>;

    case "busy":
      return <span className="pill__label">busy</span>;

    case "hidden":
      return null;
  }
}

function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

function Bars({ levels }: { levels: number[] }) {
  return (
    <div className="bars" aria-hidden="true">
      {levels.map((level, i) => (
        <span
          // Index is stable and order-preserving for this fixed-length
          // rolling window, so it's an acceptable key here.
          key={i}
          className="bar"
          style={{ height: `${levelToHeightPercent(level)}%` }}
        />
      ))}
    </div>
  );
}
