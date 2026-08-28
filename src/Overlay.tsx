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
  | { event: "busy_rejected" };

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
  const levelsRef = useRef<number[]>([]);
  const hideTimer = useRef<number | undefined>(undefined);

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

    const scheduleHide = (ms: number) => {
      hideTimer.current = window.setTimeout(() => {
        setView({ kind: "hidden" });
        win.hide().catch(() => {});
      }, ms);
    };

    const unlistenPromise = listen<OverlayEvent>("overlay-event", ({ payload }) => {
      clearHideTimer();

      switch (payload.event) {
        case "warming":
          levelsRef.current = [];
          setView({ kind: "warming" });
          showAndPosition(win);
          break;

        case "idle":
          levelsRef.current = [];
          setView({ kind: "hidden" });
          win.hide().catch(() => {});
          break;

        case "recording": {
          const levels = [...levelsRef.current, payload.level].slice(-RECORDING_HISTORY);
          levelsRef.current = levels;
          setView({ kind: "recording", levels, elapsedMs: payload.elapsed_ms });
          showAndPosition(win);
          break;
        }

        case "transcribing":
          setView({ kind: "transcribing" });
          showAndPosition(win);
          break;

        case "normalizing":
          setView({ kind: "normalizing" });
          showAndPosition(win);
          break;

        case "injecting":
          setView({ kind: "injecting" });
          showAndPosition(win);
          break;

        case "done":
          setView({ kind: "done", preview: truncate(payload.preview, PREVIEW_MAX_CHARS) });
          showAndPosition(win);
          scheduleHide(DONE_FLASH_MS);
          break;

        case "error":
          setView({ kind: "error", reason: payload.reason });
          showAndPosition(win);
          scheduleHide(ERROR_FLASH_MS);
          break;

        case "busy_rejected":
          setView({ kind: "busy" });
          showAndPosition(win);
          scheduleHide(BUSY_FLASH_MS);
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
