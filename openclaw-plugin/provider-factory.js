/**
 * The yappr realtime transcription provider: the provider descriptor, and the
 * mapping from yappr's websocket events onto OpenClaw's four session callbacks.
 *
 * Exported as a *factory* taking `PluginCapabilityCatalogContext`, never as a
 * built object, because the only supported way to reach
 * `createRealtimeTranscriptionWebSocketSession` from an external plugin is the
 * host injecting it here -- `openclaw/plugin-sdk/realtime-transcription-session`
 * is private-local (docs/plugins/sdk-subpaths.md) and importing it works for
 * bundled plugins and breaks for installed ones.
 *
 * The shared helper owns proxy capture, reconnect backoff, audio queueing before
 * ready, close flushing and close diagnostics. What is left here, and the only
 * thing that is genuinely yappr's, is turn assembly.
 *
 * ## Event mapping
 *
 *   ready        -> transport.markReady()   (audio queues until this lands)
 *   speech_start -> onSpeechStart()         (once per utterance)
 *   final        -> onTranscript(text)
 *   finalized    -> closes the finalize handshake; emits nothing of its own
 *   error        -> onError(Error)
 *
 * **`onPartial` is never called, and that is correct.** yappr has no interim
 * results: `Pipeline::process_with_capture` runs VAD, ASR, the S1-mini rewrite
 * and the guardrail over a whole utterance and emits one finished string. A
 * reader arriving from the Deepgram reference will look for the `is_final`
 * branch; there is none, because there is no upstream event that could feed it.
 * Fabricating partials from finals would mean emitting the same sentence twice,
 * once as a `transcript.delta` and once as a `transcript.done`.
 */

import {
	YAPPR_DEFAULT_ENCODING,
	YAPPR_DEFAULT_SAMPLE_RATE,
	buildHeaders,
	hasResolvableEndpoint,
	normalizeProviderConfig,
	toYapprRealtimeWsUrl,
} from "./config.js";

const YAPPR_PROVIDER_ID = "yappr";

/**
 * The `[asr] model` spellings from `crates/yappr-core/src/config.rs`'s `AsrModel`
 * serde renames, catalogued in `crates/yappr-core/src/models.rs`'s `ASR_MODELS`.
 * These are config values, not the lock-file artifact keys next to them -- the
 * two are deliberately different and only these are user-facing.
 */
const YAPPR_ASR_MODELS = Object.freeze([
	"parakeet-tdt-v3",
	"parakeet-unified-en",
	"nemotron-3.5",
	"parakeet-primeline-de",
]);

/** `d_asr_model()` in `config.rs`, and the only model an existing install has on disk. */
const YAPPR_DEFAULT_MODEL = "parakeet-tdt-v3";

/**
 * Lower wins. Taken from the occupied numbers in this host build -- OpenAI 10,
 * Deepgram 35, ElevenLabs 40 -- and set below all of them, because this provider
 * costs nothing per minute and no audio leaves the machine. Anything higher
 * would mean a laptop that happens to export `OPENAI_API_KEY` streams its
 * microphone to a cloud vendor while a loaded local model sits idle.
 *
 * The cost of winning: `isConfigured` is true by default, so yappr also wins
 * when yappr is not running, and the user gets a connect error rather than a
 * cloud fallback. That is deliberate and bounded -- the plugin is
 * `enabledByDefault: false`, so it is only in the race at all once someone
 * installed and enabled it, and naming another provider in `streaming.provider`
 * bypasses ordering entirely. 5 leaves 1-4 for anything even closer to the mic.
 */
const YAPPR_AUTO_SELECT_ORDER = 5;

/**
 * Generous, because `readyOnOpen` is false and yappr's `[models]
 * preload_at_startup` defaults to `false`: the first connection after a cold
 * start waits for sherpa-onnx *and* S1-mini (~480 MB) to load before `ready` is
 * sent. Deepgram's 10 s is a network timeout; this is a model-load timeout.
 */
const YAPPR_CONNECT_TIMEOUT_MS = 30000;
/**
 * Must outlast the finalize handshake: yappr still has to VAD-trim, transcribe
 * and rewrite the last utterance after `close()`. One second over Deepgram's
 * 5000, and deliberately longer than the fallback below.
 */
const YAPPR_CLOSE_TIMEOUT_MS = 6000;
const YAPPR_MAX_RECONNECT_ATTEMPTS = 5;
/**
 * Half Deepgram's 1000. The realistic reason a loopback socket drops is yappr
 * restarting itself (`Request::Restart`, which relaunches after `shutdown`
 * releases the lock), and a second of doubling backoff is a second of dictation
 * queued for no reason.
 */
const YAPPR_RECONNECT_DELAY_MS = 500;
/** ~65 s of 16 kHz linear16 -- comfortably more than a cold model load queues. */
const YAPPR_MAX_QUEUED_BYTES = 2097152;
/** Same cap as the reference implementation; see `stageFinal`. */
const YAPPR_MAX_RETAINED_TRANSCRIPT_BYTES = 262144;
/** Just inside `YAPPR_CLOSE_TIMEOUT_MS`, so it fires before the socket is torn down. */
const YAPPR_FINALIZE_FALLBACK_MS = 4900;

function readErrorDetail(value) {
	if (typeof value === "string" && value.trim()) return value.trim();
	if (value && typeof value === "object" && !Array.isArray(value)) {
		const message = typeof value.message === "string" ? value.message.trim() : "";
		if (message) return message;
		const code = typeof value.code === "string" ? value.code.trim() : "";
		if (code) return code;
	}
	return "yappr realtime transcription error";
}

function createYapprRealtimeTranscriptionSession(config, createRealtimeTranscriptionWebSocketSession) {
	let speechStarted = false;
	/**
	 * Text that arrived in a `final` but has not been handed to the consumer.
	 *
	 * In the ordinary case this is filled and drained inside one call to
	 * `handleEvent`, so it is observably always empty -- yappr sends whole
	 * utterances, so there is nothing to accumulate the way Deepgram's
	 * `is_final`-then-`speech_final` sequence has. It is kept rather than folded
	 * into a direct `onTranscript(text)` for three reasons, none hypothetical:
	 *
	 *   1. The byte cap has to weigh text *before* it reaches the consumer. A
	 *      wedged yappr that emits one enormous `final` must be cut off with
	 *      `onError` + `closeNow()`, not have it pushed into session history.
	 *   2. `onTranscript` is the consumer's code and may throw. Clearing only
	 *      after a delivery that returned means the text survives into the close
	 *      path instead of vanishing into a catch block.
	 *   3. It is what the §6.4 fallback timer and the §6.5 reconnect flush both
	 *      drain. The case that leaves it non-empty at close is a `final` whose
	 *      delivery failed, or that arrived while the socket was already dropping
	 *      -- `close()` runs the finalize handshake with the socket still open,
	 *      so that window is real. Delete the accumulator and both paths have
	 *      nothing to flush and no way to notice.
	 */
	let finalizedTranscript = "";
	let finalizeRequested = false;
	let finalizeFallbackFired = false;
	let finalizeFallbackTimer;
	let openedOnce = false;

	const collapseWhitespace = (value) => value.replace(/\s+/g, " ").trim();
	const joinTranscript = (left, right) =>
		collapseWhitespace(left && right ? `${left} ${right}` : left || right);

	const clearFinalizeFallback = () => {
		if (finalizeFallbackTimer) {
			clearTimeout(finalizeFallbackTimer);
			finalizeFallbackTimer = undefined;
		}
	};

	const clearTurn = () => {
		clearFinalizeFallback();
		finalizedTranscript = "";
		speechStarted = false;
	};

	/**
	 * Stages a finished utterance, refusing it if the retained bytes would exceed
	 * the cap. An unbounded accumulator on a socket that has stopped draining is
	 * a memory leak, so overflow clears the turn, reports the limit by name and
	 * closes -- a truncated dictation the user can see beats a process that grows
	 * until it is killed.
	 */
	const stageFinal = (text, transport) => {
		const next = joinTranscript(finalizedTranscript, text);
		if (Buffer.byteLength(next, "utf8") > YAPPR_MAX_RETAINED_TRANSCRIPT_BYTES) {
			clearTurn();
			config.onError?.(
				new Error(
					`yappr realtime retained transcript exceeded ${YAPPR_MAX_RETAINED_TRANSCRIPT_BYTES} bytes`,
				),
			);
			transport.closeNow();
			return false;
		}
		finalizedTranscript = next;
		return true;
	};

	/**
	 * Hands the staged utterance to the consumer and resets the turn.
	 *
	 * The reset happens only on a delivery that returned, so a throwing
	 * `onTranscript` leaves the text staged for the close-path fallback instead
	 * of dropping it. Nothing escapes this function: §6.7 forbids throwing
	 * asynchronously out of a callback, and the two places this is called from
	 * are a socket event and a timer.
	 */
	const flushFinalized = () => {
		const full = collapseWhitespace(finalizedTranscript);
		if (!full) {
			clearTurn();
			return;
		}
		try {
			config.onTranscript?.(full);
		} catch (error) {
			try {
				config.onError?.(error instanceof Error ? error : new Error(String(error)));
			} catch {
				// The consumer's own error handler threw. There is nowhere left to report.
			}
			return;
		}
		clearTurn();
	};

	/**
	 * The negotiated format must match what we are about to send. yappr echoes
	 * both in `ready`, and a mismatch is the one failure in this protocol that
	 * produces no error at all: 16 kHz PCM read as 8 kHz mulaw transcribes as
	 * plausible-looking nonsense. Failing the connect is the only honest answer.
	 */
	const checkNegotiatedFormat = (event, transport) => {
		const sampleRate = event.sample_rate;
		if (typeof sampleRate === "number" && sampleRate !== config.sampleRate) {
			transport.failConnect(
				new Error(
					`yappr negotiated sample_rate ${sampleRate} but this session sends ${config.sampleRate}`,
				),
			);
			return false;
		}
		const encoding = typeof event.encoding === "string" ? event.encoding : undefined;
		if (encoding && encoding !== config.encoding) {
			transport.failConnect(
				new Error(`yappr negotiated encoding "${encoding}" but this session sends "${config.encoding}"`),
			);
			return false;
		}
		return true;
	};

	const handleEvent = (event, transport) => {
		switch (event?.type) {
			case "ready": {
				if (!checkNegotiatedFormat(event, transport)) return;
				// Without this the helper queues audio until `maxQueuedBytes` and the
				// session stalls with no error. `readyOnOpen` is false because yappr
				// accepts the upgrade before its models are resident.
				transport.markReady();
				return;
			}
			case "speech_start": {
				if (speechStarted) return;
				speechStarted = true;
				config.onSpeechStart?.();
				return;
			}
			case "final": {
				// The fallback timer already emitted this turn; re-emitting would
				// duplicate the user's last sentence in session history.
				if (finalizeFallbackFired) return;
				const text = typeof event.text === "string" ? collapseWhitespace(event.text) : "";
				// A `final` ends the turn even when it carries nothing -- yappr heard
				// speech and resolved it to no words. Returning without clearing would
				// leave `speechStarted` latched and swallow the *next* utterance's
				// `speech_start`.
				if (!text) {
					clearTurn();
					return;
				}
				if (!stageFinal(text, transport)) return;
				flushFinalized();
				return;
			}
			case "finalized": {
				// Everything captured before the finalize request has been emitted, so
				// there is nothing to flush -- but the fallback timer must be disarmed
				// or it fires against a turn that is already complete.
				clearFinalizeFallback();
				return;
			}
			case "error": {
				const error = new Error(readErrorDetail(event.message ?? event.error));
				// Before ready, `onError` alone would leave `connect()` hanging until
				// the 30 s connect timeout. `failConnect` reports and rejects at once.
				if (transport.isReady()) config.onError?.(error);
				else transport.failConnect(error);
				return;
			}
			default:
				// Unknown event types are ignored on purpose: yappr may add one, and an
				// older plugin refusing to run is worse than an older plugin ignoring it.
				return;
		}
	};

	return createRealtimeTranscriptionWebSocketSession({
		providerId: YAPPR_PROVIDER_ID,
		callbacks: config,
		// A thunk, not a string: rebuilt per attempt so a token rotated or a
		// `YAPPR_REALTIME_URL` exported between reconnects is actually used.
		url: () => toYapprRealtimeWsUrl(config),
		headers: () => buildHeaders(config),
		readyOnOpen: false,
		connectTimeoutMs: YAPPR_CONNECT_TIMEOUT_MS,
		closeTimeoutMs: YAPPR_CLOSE_TIMEOUT_MS,
		maxReconnectAttempts: YAPPR_MAX_RECONNECT_ATTEMPTS,
		reconnectDelayMs: YAPPR_RECONNECT_DELAY_MS,
		maxQueuedBytes: YAPPR_MAX_QUEUED_BYTES,
		connectTimeoutMessage:
			"yappr realtime transcription connection timeout (is yappr running, and are its models loaded?)",
		connectClosedBeforeReadyMessage: "yappr realtime transcription connection closed before ready",
		reconnectLimitMessage: "yappr realtime transcription reconnect limit reached",
		onOpen: () => {
			// `onOpen` fires on reconnects too. A first open starts clean; a re-open
			// must emit whatever was staged when the socket dropped, exactly once,
			// before the new connection starts a turn of its own.
			if (openedOnce) flushFinalized();
			else {
				openedOnce = true;
				clearTurn();
			}
			finalizeRequested = false;
			finalizeFallbackFired = false;
		},
		sendAudio: (audio, transport) => {
			transport.sendBinary(audio);
		},
		onClose: (transport) => {
			// Guarded so the timer arms at most once per session; `onClose` can be
			// reached again through the helper's own teardown paths.
			if (finalizeRequested) return;
			finalizeRequested = true;
			// Without this the user's last sentence disappears the moment they
			// release the mic: yappr still holds an open utterance that only
			// `finalize` will cut. The timer is what covers yappr never answering --
			// a model still loading, or a process that died mid-handshake.
			finalizeFallbackTimer = setTimeout(() => {
				finalizeFallbackTimer = undefined;
				finalizeFallbackFired = true;
				try {
					flushFinalized();
				} catch (error) {
					try {
						config.onError?.(error instanceof Error ? error : new Error(String(error)));
					} catch {
						// Nowhere left to report; swallowing beats an unhandled rejection.
					}
				}
			}, YAPPR_FINALIZE_FALLBACK_MS);
			// Armed unconditionally, unlike the reference implementation, which arms
			// only when it already holds text: here the text that needs rescuing is
			// produced *by* the finalize round trip, so at this moment there is
			// legitimately nothing staged yet. `unref` is the price of that -- a
			// closed session must not keep the Gateway's event loop alive for five
			// seconds, and a process that is exiting has no consumer left to flush to.
			finalizeFallbackTimer.unref?.();
			try {
				transport.sendJson({ type: "finalize" });
			} catch (error) {
				config.onError?.(error instanceof Error ? error : new Error(String(error)));
			}
		},
		onMessage: (event, transport) => handleEvent(event, transport),
	});
}

/**
 * Builds the provider descriptor. Pure: no sockets, no stores, no filesystem --
 * it is called while assembling the capability catalog, which the host does
 * *without* activating the plugin runtime so `talk.catalog.transcription` can
 * list the provider on a cold start.
 */
export function buildYapprRealtimeTranscriptionProvider({
	createRealtimeTranscriptionWebSocketSession,
}) {
	return {
		id: YAPPR_PROVIDER_ID,
		label: "yappr (local)",
		aliases: ["yappr-local", "yappr-realtime"],
		defaultModel: YAPPR_DEFAULT_MODEL,
		models: YAPPR_ASR_MODELS,
		autoSelectOrder: YAPPR_AUTO_SELECT_ORDER,
		// `cfg` is threaded through all three, not just `resolveConfig`: this
		// plugin's own entry config is a second source (see `readOwnEntryConfig`),
		// and a readiness check that could not see it would report "not
		// configured" for a machine configured entirely through the Control UI
		// form.
		resolveConfig: ({ cfg, rawConfig }) => normalizeProviderConfig(rawConfig, cfg),
		isConfigured: ({ cfg, providerConfig }) =>
			hasResolvableEndpoint(normalizeProviderConfig(providerConfig, cfg)),
		createSession: (req) => {
			const config = normalizeProviderConfig(req.providerConfig, req.cfg);
			// `req` IS the callbacks object merged with `{ cfg, providerConfig }`.
			// Spreading it is what wires onPartial/onTranscript/onSpeechStart/onError
			// through; nesting it under a key silently produces a session that
			// transcribes and reports nothing.
			return createYapprRealtimeTranscriptionSession(
				{
					...req,
					...config,
					sampleRate: config.sampleRate ?? YAPPR_DEFAULT_SAMPLE_RATE,
					encoding: config.encoding ?? YAPPR_DEFAULT_ENCODING,
					model: config.model ?? YAPPR_DEFAULT_MODEL,
				},
				createRealtimeTranscriptionWebSocketSession,
			);
		},
	};
}
