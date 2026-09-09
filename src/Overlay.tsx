import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { applyTheme, type ConfiguredTheme } from "./theme";
import { getCurrentWindow, type Window as TauriWindow } from "@tauri-apps/api/window";
import { AnimatePresence, MotionConfig, motion, type Transition } from "motion/react";
import "./Overlay.css";

// Wire shape emitted by the Rust backend: a hand-maintained mirror of
// `yappr_core::proto::OverlayEvent` (crates/yappr-core/src/proto.rs), forwarded
// here as a Tauri event by `TauriSink::emit` (src-tauri/src/lib.rs) -- the
// in-process daemon serialises its own enum straight to JSON, with no
// intermediate `wire.rs` copy any more (invariant 3: this union is now the
// *only* other hand-maintained copy, plus the checked-in replay fixture
// `src-tauri/fixtures/replay-full.ndjson`). This is the *only* place the
// frontend knows about the pipeline: it never decides state transitions,
// timing budgets, or retries -- it renders whatever it's told (spec 12).
type OverlayEvent =
  | { event: "warming" }
  | { event: "idle" }
  // Task 13: the tray's "Diktat pausieren" is checked. No rendering exists
  // for this yet (future UI work); the `default` case below is what keeps
  // an unhandled event a safe no-op rather than a stuck capsule.
  | { event: "paused" }
  // ptt-start accepted, but the mic has not produced a sample yet (~55 ms
  // through PipeWire here). Rendered as "wait" so the user does not start
  // talking into a microphone that is not capturing; the next `recording`
  // event is the moment capture genuinely went live.
  | { event: "opening" }
  | { event: "recording"; level: number; elapsed_ms: number }
  | { event: "transcribing" }
  | { event: "normalizing" }
  | { event: "injecting" }
  | { event: "done"; preview: string }
  | { event: "error"; reason: string }
  | { event: "busy_rejected" }
  // Spec 15 (M2 Task 3): normalization availability, independent of the
  // pipeline's own state -- rendered as a small persistent badge alongside
  // whatever the state-driven capsule is already showing, never in place of
  // it.
  | { event: "normalize_degraded"; reason: string }
  | { event: "normalize_recovered" };

// How many of the most recent `recording` levels to keep for the meter.
// At the spec's ~50ms cadence this is roughly 1.4s of history -- enough to
// look alive without the bars feeling like a full waveform.
const RECORDING_HISTORY = 28;

// Presentation durations from spec 12's table. These are UI-only timers
// (how long a flash stays on screen), not pipeline behaviour, so they live
// here rather than in Rust.
const DONE_FLASH_MS = 800;
const ERROR_FLASH_MS = 2000;
const BUSY_FLASH_MS = 400;
const PREVIEW_MAX_CHARS = 96;

// How long the capsule's exit animation is given before the native window
// is actually hidden. The window used to vanish on the same frame the React
// tree emptied, which threw away the exit half of every transition: a thing
// that arrives by rising and materialising should leave the same way it came
// (Apple's spatial-consistency rule), and it cannot if the surface it is
// drawn on disappears first. Kept deliberately short, and armed on a timer
// that `showOnce` clears rather than one that can outlive its own state --
// the failure this must never reintroduce is a window left on screen with
// nothing in it.
const EXIT_MS = 260;

type ViewState =
  | { kind: "hidden" }
  | { kind: "warming" }
  | { kind: "opening" }
  | { kind: "recording"; levels: number[]; elapsedMs: number }
  | { kind: "transcribing" }
  | { kind: "normalizing" }
  | { kind: "injecting" }
  | { kind: "done"; preview: string }
  | { kind: "error"; reason: string }
  | { kind: "busy" };

// Motion, in the two shapes this UI has any use for.
//
// Apple parameterises a spring as *damping ratio* (how much it overshoots)
// and *response* (how quickly it reaches the target) rather than as
// mass/stiffness/damping, and Motion's `bounce`/`duration` pair is the same
// two knobs under different names: `bounce: 0` is critically damped, and
// `duration` here is a settle time, not a fixed playback length -- an
// interrupted spring re-targets from wherever it currently is.
//
// SETTLE is the default for everything: no overshoot, nothing distracting.
// LAND is used in exactly one place, the moment the pipeline finishes and
// the capsule grows to show what was typed, because that transition is the
// one that has momentum behind it -- overshoot on an arrival reads as
// arrival; overshoot on a label change reads as a wobble.
const SETTLE = { type: "spring", bounce: 0, duration: 0.4 } as const;
const LAND = { type: "spring", bounce: 0.22, duration: 0.45 } as const;
// `busy_rejected` is on screen for BUSY_FLASH_MS, which spec 12 sets at 400 ms.
// A 400 ms settle inside a 400 ms flash means the capsule spends its whole life
// arriving and never actually lands -- the state reads as a smear rather than
// as a word. The spec's duration is the constraint, so the spring gives way:
// this one settles in well under half the time it is allowed.
const SNAP = { type: "spring", bounce: 0, duration: 0.22 } as const;

// Enter and exit are the same path in reverse (rise + materialise / sink +
// dissolve), not two different animations. The blur is what makes the
// surface read as a *material* arriving rather than a rectangle fading up.
const CAPSULE_IN = { opacity: 1, y: 0, scale: 1, filter: "blur(0px)" };
const CAPSULE_OUT = { opacity: 0, y: 12, scale: 0.94, filter: "blur(10px)" };

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
/// Map that onto a `scaleY` factor with a floor, so a quiet moment still
/// shows a faint "listening" line rather than nothing at all.
///
/// A scale rather than a height: the bars are a fixed-size box the
/// compositor can transform on its own thread, where animating `height`
/// would put a layout pass on the main thread twenty times a second for the
/// whole duration of a dictation.
function levelToScale(level: number): number {
  const clamped = Math.max(0, Math.min(1, level / 0.3));
  return 0.06 + clamped * 0.94;
}

export default function Overlay() {
  const [view, setView] = useState<ViewState>({ kind: "hidden" });
  // Independent of `view`: spec 15's normalization-availability badge is not
  // a pipeline state, and must not replace whatever `view` is already
  // showing (see the `OverlayEvent` union's comment).
  const [degraded, setDegraded] = useState<string | null>(null);
  const levelsRef = useRef<number[]>([]);
  const hideTimer = useRef<number | undefined>(undefined);
  // Separate from `hideTimer` on purpose. `hideTimer` decides *when a state
  // stops being shown*; this one only defers the native `win.hide()` far
  // enough for the exit animation to play. Conflating them would put the
  // flash durations and the animation length in one number.
  const exitTimer = useRef<number | undefined>(undefined);
  // Whether the window is currently shown-and-positioned. `recording` events
  // arrive at spec 7.1's ~50ms cadence (~20/s) for the whole duration of a
  // dictation; calling `showAndPosition` -- a `win.show()` IPC round trip
  // plus `position_overlay`, itself documented as a Wayland no-op today --
  // on every single one of them bought nothing (the window was already
  // shown and positioned) for real IPC cost. `showAndPosition` is the "show
  // it" primitive; `showOnce` below is "show it, only if it isn't already".
  const visible = useRef(false);

  // The palette. `theme.ts` has already put the system-resolved shipped pair
  // on `<html>` at import time, so this is a correction, not a first paint --
  // and it lands while the window is still hidden at app startup, long before
  // a dictation puts the capsule on screen.
  //
  // `theme` rather than `get_config`: this window reads no other setting, and
  // `get_config` needs a daemon that `--replay` mode does not have. A failure
  // leaves the shipped pair, which is exactly what the HUD looked like before
  // themes existed.
  useEffect(() => {
    invoke<ConfiguredTheme>("theme").then(applyTheme).catch(() => {});

    const unlistenPromise = listen<ConfiguredTheme>("theme-changed", ({ payload }) =>
      applyTheme(payload),
    );
    return () => {
      unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);

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

    const clearExitTimer = () => {
      if (exitTimer.current !== undefined) {
        window.clearTimeout(exitTimer.current);
        exitTimer.current = undefined;
      }
    };

    const showOnce = () => {
      // A state arriving inside the exit window cancels the pending native
      // hide and lets the capsule spring back from wherever its exit had
      // got to, rather than waiting for it to finish and starting again --
      // an animation the user has overtaken must be redirectable, not
      // replayed from the top.
      clearExitTimer();
      if (visible.current) return;
      visible.current = true;
      showAndPosition(win);
    };

    const hide = () => {
      visible.current = false;
      setView({ kind: "hidden" });
      clearExitTimer();
      exitTimer.current = window.setTimeout(() => {
        exitTimer.current = undefined;
        win.hide().catch(() => {});
      }, EXIT_MS);
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
      // event (or one that slips past the daemon-side fix in `daemon.rs`)
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

        case "opening":
          clearHideTimer();
          levelsRef.current = [];
          setView({ kind: "opening" });
          showOnce();
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
          // Deliberately no showOnce(), and the visibility bookkeeping is
          // dropped: on a compositor without layer-shell (GNOME/Mutter) the
          // backend hides this window natively the moment it holds keyboard
          // focus at injection time (invariant 2 -- a focus-following
          // injector would otherwise type the transcript into this very
          // window), and nothing here may re-map it while the injector is
          // typing. On layer-shell compositors the window is already
          // visible from the states before this one, so not showing changes
          // nothing there -- and resetting `visible` only costs the next
          // state one redundant, idempotent showAndPosition().
          visible.current = false;
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
          // predates a new variant must not risk a stuck, unclosable
          // capsule over one it can't render.
          break;
      }
    });

    // Replays the daemon's current state once this listener is actually
    // registered -- closes the race where nothing shows during warm-up
    // (nothing ever broadcasts "warming"; the daemon only stores it) or a
    // warm-up failure broadcast outruns this listener. A no-op in --replay
    // mode, where no daemon is managed at all.
    unlistenPromise.then(() => invoke("overlay_ready").catch(() => {}));

    return () => {
      unlistenPromise.then((unlisten) => unlisten());
      clearHideTimer();
      clearExitTimer();
    };
  }, []);

  return (
    // `reducedMotion="user"` is the whole of this UI's reduced-motion
    // handling for transforms: Motion drops every translate/scale/rotate and
    // keeps opacity, which leaves each state's meaning -- carried by its
    // words and its glyph -- completely intact. The parts CSS animates
    // (the meter, the stage rail, the record dot) opt out in `Overlay.css`.
    <MotionConfig reducedMotion="user">
      <div className="stage">
        <AnimatePresence initial={false}>
          {view.kind !== "hidden" && (
            <motion.div
              key="capsule"
              layout
              className={`capsule capsule--${view.kind}`}
              role="status"
              aria-live="polite"
              initial={CAPSULE_OUT}
              animate={CAPSULE_IN}
              exit={CAPSULE_OUT}
              transition={CAPSULE_SPRING[view.kind] ?? SETTLE}
            >
              {/* `popLayout` pulls the outgoing content out of flow the
                  instant it starts leaving, so the capsule's `layout` spring
                  starts resizing towards the *new* content immediately
                  instead of waiting out a cross-fade at the old width. */}
              <AnimatePresence mode="popLayout" initial={false}>
                <motion.div
                  key={view.kind}
                  className="capsule__content"
                  initial={{ opacity: 0, y: 6, filter: "blur(4px)" }}
                  animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
                  exit={{ opacity: 0, y: -6, filter: "blur(4px)" }}
                  transition={SETTLE}
                >
                  <CapsuleContent view={view} />
                </motion.div>
              </AnimatePresence>

              {/* Spec 15: alongside whatever the state above is showing, not
                  in place of it -- see the `OverlayEvent` union's comment.
                  Inside the capsule rather than floating in the window's
                  corner, because a dot that belongs to this surface should
                  sit on it: proximity is what says the two are about the
                  same thing. */}
              <AnimatePresence initial={false}>
                {degraded !== null && (
                  <motion.span
                    key="degraded"
                    layout
                    className="degraded"
                    title={degraded}
                    aria-label={degraded}
                    role="status"
                    initial={{ opacity: 0, scale: 0.5 }}
                    animate={{ opacity: 1, scale: 1 }}
                    exit={{ opacity: 0, scale: 0.5 }}
                    transition={SETTLE}
                  >
                    <span className="degraded__dot" aria-hidden="true" />
                  </motion.span>
                )}
              </AnimatePresence>
            </motion.div>
          )}
        </AnimatePresence>
      </div>
    </MotionConfig>
  );
}

/// The spring each state arrives on, where it is not the house default.
/// `done` is the one arrival with momentum behind it and gets a little
/// overshoot; `busy` has to fit inside a 400 ms flash and gets less time.
const CAPSULE_SPRING: Partial<Record<ViewState["kind"], Transition>> = {
  done: LAND,
  busy: SNAP,
};

/// Which segment of the three-step rail is currently running. The pipeline
/// can skip the middle one (normalization off, or degraded to a rule-based
/// cleanup), which is why this is a lookup rather than a counter -- the rail
/// jumps straight to the last segment and the spring carries it there in one
/// continuous move.
const STAGE_INDEX: Record<string, number> = {
  transcribing: 0,
  normalizing: 1,
  injecting: 2,
};

function CapsuleContent({ view }: { view: ViewState }) {
  switch (view.kind) {
    case "warming":
      return (
        <>
          <Breather />
          <span className="label">loading models</span>
        </>
      );

    case "opening":
      // No spinner and no rail: both read as "the machine is busy, sit
      // tight", which is the same thing `transcribing`/`cleaning` say. This
      // is an instruction to the *user*, so it is a word plus a steady dot.
      return (
        <>
          <span className="dot dot--wait" aria-hidden="true" />
          <span className="label">wait…</span>
        </>
      );

    case "recording":
      return (
        <>
          <span className="dot dot--rec" aria-hidden="true" />
          <Meter levels={view.levels} />
          <span className="timer">{(view.elapsedMs / 1000).toFixed(1)}s</span>
        </>
      );

    case "transcribing":
    case "normalizing":
    case "injecting": {
      const labels = {
        transcribing: "transcribing",
        normalizing: "cleaning",
        injecting: "typing",
      } as const;
      return (
        <div className="run">
          <span className="label">{labels[view.kind]}</span>
          <StageRail index={STAGE_INDEX[view.kind]} />
        </div>
      );
    }

    case "done":
      return (
        <>
          <Check />
          <span className="preview">{view.preview}</span>
        </>
      );

    case "error":
      return (
        <>
          <Warn />
          <span className="label label--wrap">{view.reason}</span>
        </>
      );

    case "busy":
      return (
        <>
          <span className="dot dot--wait" aria-hidden="true" />
          <span className="label">busy</span>
        </>
      );

    case "hidden":
      return null;
  }
}

/// Warming has no progress to report and no stages to walk, so it gets the
/// one shape that honestly means "alive, nothing to count": a slow breath.
function Breather() {
  return <span className="breather" aria-hidden="true" />;
}

/// The live meter -- the user's proof the microphone is hearing them.
///
/// Always renders its full width of slots and fills them from the right, so
/// the meter reads as history scrolling past a fixed window instead of a
/// block that grows sideways for the first 1.4 s of every dictation and
/// drags the capsule's width along with it.
function Meter({ levels }: { levels: number[] }) {
  const pad = RECORDING_HISTORY - levels.length;
  return (
    <div className="meter" aria-hidden="true">
      {Array.from({ length: RECORDING_HISTORY }, (_, i) => {
        const level = i < pad ? 0 : levels[i - pad];
        return (
          <span
            // Index is stable and order-preserving for this fixed-length
            // rolling window, so it's an acceptable key here.
            key={i}
            className="meter__bar"
            style={{ transform: `scaleY(${levelToScale(level)})` }}
          />
        );
      })}
    </div>
  );
}

/// The three post-release stages as a rail rather than a spinner.
///
/// A spinner says "wait, indefinitely". This says the same thing a spinner
/// does about *being busy*, and additionally where in a known sequence the
/// work is -- which is the difference between a wait that feels open-ended
/// and one that visibly has an end.
function StageRail({ index }: { index: number }) {
  return (
    <div className="rail" aria-hidden="true">
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className={`rail__seg${i < index ? " is-done" : ""}${i === index ? " is-live" : ""}`}
        />
      ))}
    </div>
  );
}

/// Drawn rather than faded in: a check that draws itself along its own
/// stroke is the shape of the action it reports -- something completing.
function Check() {
  return (
    <svg className="glyph glyph--ok" viewBox="0 0 24 24" aria-hidden="true">
      <motion.path
        d="M5 12.5 10 17.5 19.5 7"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.4"
        strokeLinecap="round"
        strokeLinejoin="round"
        initial={{ pathLength: 0 }}
        animate={{ pathLength: 1 }}
        transition={{ duration: 0.28, ease: [0.22, 1, 0.36, 1] }}
      />
    </svg>
  );
}

function Warn() {
  return (
    <svg className="glyph glyph--warn" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M12 3.6 1.9 20.4h20.2L12 3.6Z"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinejoin="round"
      />
      <path
        d="M12 10v4.6M12 17.7v.4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinecap="round"
      />
    </svg>
  );
}
