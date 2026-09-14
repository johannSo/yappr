/**
 * Config normalization and URL building for the yappr realtime transcription
 * provider.
 *
 * Everything that reads user config lives here, because the path it arrives on
 * is explicitly temporary: today OpenClaw feeds dictation from the Voice Call
 * streaming config (`plugins.entries.voice-call.config.streaming.providers.yappr`,
 * see `getVoiceCallProviderConfig` in the host's `talk-*.mjs`), and a dedicated
 * Talk transcription surface is announced but unshipped. When it lands, only
 * this file should have to change.
 *
 * Two deliberate deviations from the Deepgram reference this plugin is modelled
 * on, both forced by the no-import rule (see the header of `index.js`):
 *
 *   - The host's coercion helpers (`normalizeOptionalString`, `parseFiniteNumber`,
 *     `asOptionalRecord`) are reimplemented below rather than imported. They are
 *     ten lines each and their semantics are pinned by the tests in this file's
 *     consumers; importing them would require a bare specifier that cannot
 *     resolve from the materialized plugin directory.
 *   - `normalizeResolvedSecretInputString` is *not* used, so a SecretRef cannot
 *     be resolved for `token`. That helper is only reachable through an import
 *     this plugin is not allowed to make, and there is no injected equivalent on
 *     `PluginCapabilityCatalogContext`. A loopback shared secret is not a
 *     credential worth a vault, but the gap must not be silent: `normalizeToken`
 *     below *detects* an unresolved SecretRef and throws, naming the env var
 *     that does work. Delivering `"${YAPPR_REALTIME_TOKEN}"` to yappr as a
 *     literal token would fail auth with no hint as to why.
 */

/** Loopback only. yappr binds nothing routable; a remote yappr is not a thing. */
export const YAPPR_DEFAULT_HOST = "127.0.0.1";
/** Fixed by the Rust side. Changing it here alone breaks every default install. */
export const YAPPR_DEFAULT_PORT = 17869;
export const YAPPR_DEFAULT_PATH = "/v1/transcribe";

/**
 * 8 kHz G.711 mu-law, mono. **Not a choice -- the relay's contract.**
 *
 * This started as 16 kHz `linear16`, on the reasoning that the consumer is the
 * browser dictation mic rather than a telephone, and that yappr's own pipeline
 * is built around 16 kHz. Both halves of that are true and it was still wrong:
 * the Gateway's browser transcription relay *emits* mu-law at 8 kHz and
 * verifies the provider agrees before it will start. From the host's
 * `talk-*.mjs`:
 *
 *     const RELAY_INPUT_ENCODING = "g711_ulaw";
 *     const RELAY_INPUT_SAMPLE_RATE_HZ = 8e3;
 *     function assertRelayInputAudioConfig(providerConfig) { ... }
 *
 * `assertRelayInputAudioConfig` throws "Gateway transcription relay requires
 * g711_ulaw/8000 audio" for anything else, which is exactly what a dictation
 * attempt reported on 2026-09-14. There is no transcode path to negotiate
 * around it.
 *
 * So Deepgram's "telephony" default was never about telephony: 8 kHz mu-law is
 * what this seam speaks, for Twilio and for the browser mic alike. yappr expands
 * the mu-law and resamples 8 -> 16 kHz on its side (`PcmDecoder` in
 * `realtime.rs`), which recovers the samples but not the bandwidth.
 *
 * What that costs was measured rather than assumed: the same fixture through
 * the same socket in both formats produced the *identical* transcript
 * ("Alles hat ein Ende, nur die Wurst hat zwei."), so on clean speech the
 * narrowband path is not visibly worse. It is still half the bandwidth, and
 * harder audio has more room to lose -- but do not repeat a degradation claim
 * nobody has measured. See `realtime.rs`'s two `#[ignore]`d socket tests.
 */
export const YAPPR_DEFAULT_SAMPLE_RATE = 8000;
export const YAPPR_DEFAULT_ENCODING = "mulaw";

/** Full `ws://`/`wss://` URL. Overrides host/port entirely. */
export const YAPPR_URL_ENV_VAR = "YAPPR_REALTIME_URL";
/** Optional shared token. There is no API key -- see `normalizeToken`. */
export const YAPPR_TOKEN_ENV_VAR = "YAPPR_REALTIME_TOKEN";

function isRecord(value) {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function asOptionalRecord(value) {
	return isRecord(value) ? value : undefined;
}

function normalizeOptionalString(value) {
	if (typeof value !== "string") return undefined;
	const trimmed = value.trim();
	return trimmed ? trimmed : undefined;
}

/**
 * True when the user wrote the key and deliberately emptied it (`null`, `""`,
 * whitespace) rather than leaving it out.
 *
 * This distinction is the whole of `isConfigured` for this provider. Every key
 * has a working default, so "absent" can never mean unconfigured; the only way
 * a user can say "do not use yappr" through config alone is to blank one of
 * `url`/`host`/`port`, and a blank that silently fell back to the default would
 * make that instruction unexpressible.
 */
function isExplicitlyBlank(raw, key) {
	if (!Object.hasOwn(raw, key)) return false;
	const value = raw[key];
	if (value === null) return true;
	return typeof value === "string" && value.trim() === "";
}

/**
 * The three nesting shapes seen in the wild, same order of preference as the
 * reference implementation's `readNestedDeepgramConfig`.
 *
 * The host hands us the already-unwrapped `streaming.providers.yappr` record, so
 * in practice the third arm is the live one; the first two exist because
 * `resolveConfig` is also callable with a whole `streaming` block or a
 * provider-keyed map, and a provider that only understood one shape would read
 * an empty config and look unconfigured rather than misconfigured.
 */
function readNestedYapprConfig(rawConfig) {
	const raw = asOptionalRecord(rawConfig);
	const providers = asOptionalRecord(raw?.providers);
	return asOptionalRecord(providers?.yappr ?? raw?.yappr ?? raw) ?? {};
}

/**
 * This plugin's own entry config -- `plugins.entries.yappr.config`, the record
 * behind the Configuration form on its Control UI page.
 *
 * The host never hands this to a realtime transcription provider: it builds
 * `rawConfig` from `getVoiceCallStreamingConfig(config)` and nothing else (see
 * `resolveConfiguredRealtimeTranscriptionProvider` in the host's `talk-*.mjs`).
 * `resolveConfig` does receive the whole `cfg`, though, so reading it here is
 * what turns that form from decoration into a working settings page.
 *
 * Defensive at every step: `cfg` is optional on this path (`createSession` is
 * reached with a `req` that carries it, but a caller constructing a session by
 * hand need not), and a malformed branch anywhere yields `{}` rather than
 * throwing inside what the host calls during auto-selection.
 */
function readOwnEntryConfig(cfg) {
	const plugins = asOptionalRecord(asOptionalRecord(cfg)?.plugins);
	const entries = asOptionalRecord(plugins?.entries);
	const entry = asOptionalRecord(entries?.yappr);
	return asOptionalRecord(entry?.config) ?? {};
}

/**
 * yappr decodes exactly two encodings. `alaw` is rejected rather than passed
 * through: the Rust side has no A-law decoder, so accepting it would mean
 * shipping bytes that transcribe as noise with no error anywhere.
 */
export function normalizeEncoding(value) {
	const normalized = normalizeOptionalString(value)?.toLowerCase();
	if (!normalized) return undefined;
	if (
		normalized === "pcm" ||
		normalized === "pcm16" ||
		normalized === "pcm_s16le" ||
		normalized === "s16le" ||
		normalized === "linear16"
	) {
		return "linear16";
	}
	if (
		normalized === "mulaw" ||
		normalized === "ulaw" ||
		normalized === "mu-law" ||
		normalized === "g711_ulaw" ||
		normalized === "g711-ulaw" ||
		normalized === "g711-mulaw"
	) {
		return "mulaw";
	}
	throw new Error(
		`Invalid yappr realtime transcription encoding: "${normalized}" (expected linear16 or mulaw)`,
	);
}

function normalizeSampleRate(value) {
	if (value === undefined || value === null) return undefined;
	const parsed = typeof value === "number" ? value : Number(String(value).trim());
	if (!Number.isInteger(parsed) || parsed <= 0) {
		throw new Error(
			`Invalid yappr realtime transcription sampleRate: ${JSON.stringify(value)} (expected a positive integer, for example 16000)`,
		);
	}
	return parsed;
}

function normalizePort(value) {
	if (value === undefined || value === null) return undefined;
	const parsed = typeof value === "number" ? value : Number(String(value).trim());
	if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
		throw new Error(
			`Invalid yappr realtime transcription port: ${JSON.stringify(value)} (expected an integer between 1 and 65535)`,
		);
	}
	return parsed;
}

/**
 * Validates a user-supplied endpoint and normalizes it to a websocket scheme.
 *
 * `http`/`https` are accepted and mapped because that is what a user who copied
 * the address out of a browser will paste, and rejecting it would be pedantry;
 * anything else throws, because a `file:` or `tcp:` URL here fails much later,
 * inside `ws`, with a message that names neither the config key nor the plugin.
 */
export function normalizeEndpointUrl(value, source) {
	const resolved = normalizeOptionalString(value);
	if (!resolved) return undefined;
	let parsed;
	try {
		parsed = new URL(resolved);
	} catch {
		throw new Error(`Invalid yappr realtime transcription ${source}: value is not a valid URL`);
	}
	if (parsed.protocol === "http:") parsed.protocol = "ws:";
	else if (parsed.protocol === "https:") parsed.protocol = "wss:";
	else if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
		throw new Error(
			`Invalid yappr realtime transcription ${source}: unsupported scheme "${parsed.protocol}" (expected ws, wss, http, or https)`,
		);
	}
	return parsed.toString();
}

/**
 * There is no API key here, and that is not an oversight.
 *
 * The upstream is yappr on loopback -- the user's own process, on the user's own
 * machine, reachable by nobody else. The optional token exists only so that a
 * second local account or a sandboxed process cannot drive the microphone, and
 * "no token configured" is a supported, ordinary state, not a broken one. That
 * is why `isConfigured` never looks at it.
 */
function normalizeToken(value) {
	if (value === undefined || value === null) return undefined;
	// A SecretRef survives config load as an object (`{ $secret: ... }`) or as an
	// unexpanded `${VAR}` string. We cannot resolve either -- see the file header
	// -- and sending one verbatim would be a 401 with nothing to read.
	if (isRecord(value)) {
		throw new Error(
			`yappr realtime transcription token: SecretRef values cannot be resolved by this plugin; set ${YAPPR_TOKEN_ENV_VAR} instead`,
		);
	}
	const normalized = normalizeOptionalString(value);
	if (normalized && /\$\{[^}]+\}/.test(normalized)) {
		throw new Error(
			`yappr realtime transcription token: "${normalized}" was not expanded; set ${YAPPR_TOKEN_ENV_VAR} instead`,
		);
	}
	return normalized;
}

/**
 * The provider config as the rest of the plugin sees it.
 *
 * Throws on anything unrecognisable rather than defaulting, per the brief: a
 * typo'd port that quietly became 17869 is a support case nobody can diagnose.
 * `language` is accepted and carried but never sent -- see `toYapprRealtimeWsUrl`.
 */
export function normalizeProviderConfig(rawConfig, cfg) {
	// Two sources, deliberately in this order. `rawConfig` is what the host
	// resolves from `plugins.entries.voice-call.config.streaming.providers.yappr`
	// -- the only place it looks, and the place yappr's own settings window
	// writes. `plugins.entries.yappr.config` is the form the Control UI renders
	// on this plugin's page, which the host does *not* pass to `resolveConfig`
	// at all; without this merge, every row in that form would be inert.
	//
	// The form wins. A user editing a field in front of them and seeing nothing
	// happen is the worse failure by far, and the two copies only diverge if
	// someone edits one of them: yappr's "Set up" writes both, with the same
	// values, in the same run.
	const raw = {
		...readNestedYapprConfig(rawConfig),
		...readOwnEntryConfig(cfg),
	};
	return {
		url: normalizeOptionalString(raw.url ?? raw.baseUrl ?? raw.base_url),
		urlBlank:
			isExplicitlyBlank(raw, "url") ||
			isExplicitlyBlank(raw, "baseUrl") ||
			isExplicitlyBlank(raw, "base_url"),
		host: normalizeOptionalString(raw.host),
		hostBlank: isExplicitlyBlank(raw, "host"),
		port: normalizePort(raw.port),
		portBlank: isExplicitlyBlank(raw, "port"),
		token: normalizeToken(raw.token ?? raw.authToken ?? raw.auth_token),
		sampleRate: normalizeSampleRate(raw.sampleRate ?? raw.sample_rate),
		encoding: normalizeEncoding(raw.encoding),
		model: normalizeOptionalString(raw.model ?? raw.asrModel ?? raw.asr_model),
		language: normalizeOptionalString(raw.language),
	};
}

/**
 * Can this machine be pointed at a yappr? With the defaults in place, yes --
 * which is the intended answer. yappr is local and free, so the useful failure
 * mode is "tried and could not connect", surfaced through `onError`, not
 * "silently skipped in favour of a cloud provider the user is billed for".
 *
 * Deliberately does *not* probe the socket: the brief requires this to be pure
 * and cheap, and the host calls it inside its own try/catch during auto-select.
 */
export function hasResolvableEndpoint(config) {
	if (config.urlBlank || config.hostBlank || config.portBlank) return false;
	// Past the blank check there is always an endpoint: `url`, else
	// `YAPPR_REALTIME_URL`, else `127.0.0.1:17869` -- the same order
	// `toYapprRealtimeWsUrl` resolves in, so the env fallback is honoured on this
	// path too. Spelling it out as a branch here would be a dead one, because no
	// arm can return false.
	return true;
}

export function resolveToken(config) {
	return config.token ?? normalizeOptionalString(process.env[YAPPR_TOKEN_ENV_VAR]);
}

/**
 * Builds the connection URL. Called through a thunk on every connect attempt,
 * not once at session creation, so a token rotated between reconnects is picked
 * up and a `YAPPR_REALTIME_URL` exported after the first failure takes effect.
 */
export function toYapprRealtimeWsUrl(config) {
	const configured =
		normalizeEndpointUrl(config.url, "url") ??
		normalizeEndpointUrl(process.env[YAPPR_URL_ENV_VAR], YAPPR_URL_ENV_VAR);
	const url = configured
		? new URL(configured)
		: new URL(
				`ws://${config.host ?? YAPPR_DEFAULT_HOST}:${config.port ?? YAPPR_DEFAULT_PORT}${YAPPR_DEFAULT_PATH}`,
			);

	url.searchParams.set("sample_rate", String(config.sampleRate));
	url.searchParams.set("encoding", config.encoding);
	// Always 1. yappr's VAD and both recognizer flavours are mono-only; a stereo
	// stream would be read as interleaved garbage at double the rate.
	url.searchParams.set("channels", "1");
	// Advisory. The ASR model is chosen in yappr's own settings window and owned
	// by the resident `Pipeline`; a session cannot swap it. Sending it anyway is
	// what lets yappr log "OpenClaw asked for X, running Y" instead of producing
	// a transcript from an unexpected model in silence.
	if (config.model) url.searchParams.set("model", config.model);
	// `token` rides the query string as well as the Authorization header because
	// some proxies drop headers on upgrade. yappr accepts either.
	const token = resolveToken(config);
	if (token) url.searchParams.set("token", token);
	// `config.language` is deliberately not sent. The wire protocol has no field
	// for it -- the same accepted-and-ignored idiom as `[normalize] port` in
	// yappr's own config. The key is accepted so a config written for another
	// provider needs no editing to point here; the value is dropped so the ASR
	// language stays whatever `[asr] language` says. Per-session language needs a
	// query parameter on the Rust side before it can mean anything.
	return url.toString();
}

/**
 * The Authorization half of the same optional token. Sent as a header *and* as
 * a query parameter above; yappr accepts either, and neither alone is reliable
 * through every proxy.
 *
 * Returns `{}` when no token is configured, which is the ordinary case: a
 * loopback socket that nobody else can reach needs no credential.
 */
export function buildHeaders(config) {
	const token = resolveToken(config);
	return token ? { Authorization: `Bearer ${token}` } : {};
}
