# openclaw-yappr

An OpenClaw **realtime transcription provider** backed by a local [yappr](../README.md)
instance. Microphone PCM streams to yappr over a loopback websocket; yappr runs its own
VAD, sherpa-onnx ASR and S1-mini rewrite and hands back finished, punctuated utterances.

Nothing leaves the machine, and there is no API key.

This plugin backs the same seam Deepgram and OpenAI's realtime transcription plug into:
the Control UI composer mic, the native Android/iOS/macOS mics, and Voice Call's streaming
transcription. It does **not** handle uploaded voice notes — those are media understanding,
a different contract.

Verified against openclaw **2026.9.4**. All plugin APIs are experimental; re-test on upgrade.

## Install

yappr's settings window does this for you: open the **AI** pane and use the OpenClaw
button. It writes these files to `~/.local/share/yappr/openclaw-plugin/` and runs the link
install below. (yappr's own UI is German; everything this plugin puts in front of OpenClaw
is English, because OpenClaw is an English product.)

By hand:

```bash
openclaw plugins install --link ~/.local/share/yappr/openclaw-plugin --force
openclaw plugins list
```

`--link` installs from disk, so editing a file and restarting the Gateway is enough to pick
up a change. The plugin is `enabledByDefault: false`: enable it in your OpenClaw config
before it takes part in anything.

There is no build step and there are no dependencies. These are plain ESM files that
import nothing outside their own directory — yappr materializes them from `include_str!`
data at the moment you click the button, where there is no npm, no `node_modules` and no
compiler.

## Wire protocol

`ws://127.0.0.1:17869/v1/transcribe`, with `sample_rate`, `encoding`, `channels=1`, and
`model`/`token` when they apply, as query parameters.

Client to server:

| Frame | Meaning |
| --- | --- |
| binary | Raw PCM in the negotiated encoding, mono, at the declared sample rate |
| `{"type":"finalize"}` | Cut the open utterance, transcribe it, emit it. Sent on close |

Server to client, one JSON object per text frame:

| yappr event | OpenClaw callback | Notes |
| --- | --- | --- |
| `{"type":"ready","sample_rate":…,"encoding":…,"normalize":…,"model":…}` | `transport.markReady()` | Once per connection, when the models are resident. Audio queues until it arrives |
| `{"type":"speech_start"}` | `onSpeechStart()` | Once per utterance |
| `{"type":"final","text":"…"}` | `onTranscript(text)` | One finished utterance, already capitalised and punctuated by yappr |
| `{"type":"finalized"}` | — | Everything captured before `finalize` has been emitted. Disarms the fallback timer |
| `{"type":"error","message":"…"}` | `onError(new Error(…))` | Before ready it fails the connect instead, so `connect()` does not hang |

`ready` is checked against what this session is about to send. A `sample_rate` or
`encoding` that disagrees fails the connect with a message naming both values — it is the
one failure in this protocol that otherwise produces no error at all, just a transcript of
plausible-looking nonsense.

### There are no interim results

**`onPartial` is never called.** yappr emits finals only, one per VAD-detected utterance:
its pipeline runs VAD, ASR, the S1-mini rewrite and the guardrail over a whole utterance
and produces one finished string. There is no upstream event that could feed `onPartial`,
and fabricating partials from finals would emit each sentence twice — once as a
`transcript.delta`, once as a `transcript.done`.

In practice this means `transcript.delta` never fires on the `talk.event` channel for this
provider. A client that only renders on `transcript.done` sees no difference; a client that
renders live word-by-word text will simply see the sentence appear at once.

## Audio format

**8 kHz G.711 mu-law, mono.** This is not a preference — it is the Gateway
transcription relay's contract:

```js
// the host's talk-*.mjs
const RELAY_INPUT_ENCODING = "g711_ulaw";
const RELAY_INPUT_SAMPLE_RATE_HZ = 8e3;
function assertRelayInputAudioConfig(providerConfig) { /* throws otherwise */ }
```

The relay emits mu-law at 8 kHz and refuses to start a session against a provider
config declaring anything else, with `Gateway transcription relay requires
g711_ulaw/8000 audio`. There is no transcode path to negotiate around it, and it
applies to the browser dictation mic exactly as it does to a Twilio media stream —
which is why Deepgram's "telephony" default was never about telephony.

yappr expands the mu-law and resamples 8 → 16 kHz for the model. What that costs
was measured rather than assumed: the same fixture through the same socket in both
formats returned the identical transcript. Half the bandwidth is still half the
bandwidth, so harder audio has more room to lose, but there is no measured
degradation to report on clean speech.

`linear16` at any rate is still supported by both sides — it is simply not what
this consumer sends.

`alaw` is rejected with an error rather than passed through: yappr has no A-law
decoder, so accepting it would mean shipping bytes that transcribe as noise with
nothing reporting a problem.

## Config

Today the provider's config arrives from the Voice Call streaming section — per
`docs/nodes/talk.md`, "the current Gateway relay uses the Voice Call streaming provider
config until a dedicated Talk transcription config surface ships". All of that reading is
isolated in `config.js`; when the dedicated surface lands, only that file should change.

```json5
{
  plugins: {
    entries: {
      "voice-call": {
        config: {
          streaming: {
            enabled: true,
            provider: "yappr",      // optional; omit to auto-select
            providers: {
              yappr: {
                host: "127.0.0.1",
                port: 17869,
                // Not a preference: the relay emits exactly this and
                // refuses a provider declaring anything else.
                sampleRate: 8000,
                encoding: "mulaw",
              },
            },
          },
        },
      },
    },
  },
}
```

| Key | Aliases | Default | Meaning |
| --- | --- | --- | --- |
| `url` | `baseUrl`, `base_url` | — | Full websocket URL; overrides `host`/`port`. `ws`/`wss`/`http`/`https`; `http`→`ws`, `https`→`wss`. Any other scheme throws |
| `host` | — | `127.0.0.1` | yappr binds loopback; a remote yappr is not a thing |
| `port` | — | `17869` | Must be an integer in 1–65535, or it throws |
| `token` | `authToken`, `auth_token` | — | Optional shared token. Sent as `Authorization: Bearer …` **and** `?token=` |
| `sampleRate` | `sample_rate` | `8000` | Positive integer, or it throws |
| `encoding` | — | `mulaw` | `linear16` or `mulaw` plus the usual aliases. Unknown values throw |
| `model` | `asrModel`, `asr_model` | `parakeet-tdt-v3` | Advisory; see below |
| `language` | — | — | **Accepted and ignored** — see below |

Every key has a working default, so `isConfigured` is true out of the box. The only way to
declare this provider unconfigured through config alone is to blank `url`, `host` or `port`
(`null` or `""`); a blank that silently fell back to the default would make that
instruction unexpressible.

### Environment variables

| Variable | Effect |
| --- | --- |
| `YAPPR_REALTIME_URL` | Full websocket URL, used when no `url` is configured |
| `YAPPR_REALTIME_TOKEN` | Shared token, used when no `token` is configured |

Both are honoured in `isConfigured` and in `createSession`, and both are re-read on every
connect attempt — the URL is built from a thunk, so a variable exported after a failed
attempt takes effect on the reconnect.

There is deliberately **no `YAPPR_API_KEY`**. The upstream is the user's own process on
loopback. The optional token exists so a second local account or a sandboxed process cannot
drive the microphone, and "no token" is an ordinary supported state — which is why
`isConfigured` never looks at it.

### Model selection

`models` is yappr's ASR catalogue, read from `crates/yappr-core/src/models.rs`
(`ASR_MODELS`) and `crates/yappr-core/src/config.rs` (`AsrModel`):

- `parakeet-tdt-v3` — multilingual, the default
- `parakeet-unified-en` — English only
- `nemotron-3.5` — multilingual, cache-aware streaming export
- `parakeet-primeline-de` — German only, the most accurate German model in the catalogue

**A session cannot switch models.** The active model is chosen in yappr's own settings
window and owned by the resident pipeline. A session-level `model` override is forwarded as
a `model` query parameter so yappr can log "OpenClaw asked for X, running Y"; it does not
change what runs. Pass one anyway if you want that mismatch recorded rather than silent.

### `language` is accepted and ignored

The wire protocol has no language field. yappr's `[asr] language` setting decides, and only
the cache-aware streaming model reads it at all. The key is accepted so a config written
for another provider needs no editing to point here; the value is dropped. Per-session
language requires a query parameter on yappr's side before it can mean anything — the same
accepted-and-ignored idiom yappr already uses for `[normalize] port`.

### SecretRef limitation

`token` is read as a plain string. **A SecretRef cannot be resolved by this plugin.** The
host's `normalizeResolvedSecretInputString` — the helper that makes `"${VAR}"` and
`{ $secret: … }` work — is only reachable through an import this plugin is not allowed to
make (see "no imports" above), and there is no equivalent on the injected
`PluginCapabilityCatalogContext`.

The gap is not silent: an unresolved SecretRef is *detected* and throws an error naming
`YAPPR_REALTIME_TOKEN`, rather than being sent to yappr verbatim and failing auth with no
hint as to why. Use the environment variable.

### Two places, one record

This provider's config arrives from **two** paths, and both are written — with identical
values, in one run — by yappr's "Set up" button:

| Path | Who reads it | What it is |
| --- | --- | --- |
| `plugins.entries.voice-call.config.streaming.providers.yappr` | the host | what `resolveConfig` is handed as `rawConfig` |
| `plugins.entries.yappr.config` | this plugin, via `readOwnEntryConfig` | the Configuration form on this plugin's Control UI page |

The host builds a realtime transcription provider's `rawConfig` from the first path and
nowhere else — see `resolveConfiguredRealtimeTranscriptionProvider` in the host's
`talk-*.mjs`. `resolveConfig` does receive the whole `cfg`, though, and reading the second
path out of it is what makes that settings form real instead of decorative. **The form
wins**, because a field edited in front of you that does nothing is the worse failure.

The two copies can only diverge if one is edited by hand; yappr's settings window reports
that as *veraltete Zugangsdaten* and offers to rewrite both.

### What the manifest's `configSchema` declares

One row per canonical key, and only keys that change behaviour — `host`, `port`, `token`,
`sampleRate`, `encoding`, `url`.

Not the aliases. The first version declared every accepted spelling (`asrModel` beside
`asr_model`, `authToken` beside `auth_token`); the Control UI renders one row per declared
property and humanises the key, so each alias arrived as a second row with the same label
in a different case — "Asr model" above "Asr Model". The aliases are still accepted by
`config.js`, they are just not advertised as settings.

Not `model` or `language` either. `model` is advisory (yappr's active ASR model is
`[asr] model` in its own settings; a session cannot switch it) and `language` is accepted
and ignored. A form row that changes nothing is worse than no row.

`additionalProperties: false` therefore means a hand-written alias under
`plugins.entries.yappr.config` is rejected. Use the canonical spelling there; the aliases
remain available on the streaming path, which no manifest schema governs.

JSON has no comments, which is why this section is here.

## Auto-selection

`autoSelectOrder` is **5** — below OpenAI's 10, Deepgram's 35 and ElevenLabs' 40 in this
host build. Lower wins. This provider costs nothing per minute and no audio leaves the
machine, so anything higher would mean a laptop that happens to export `OPENAI_API_KEY`
streams its microphone to a cloud vendor while a loaded local model sits idle.

The cost: `isConfigured` is true by default, so yappr also wins when yappr is not running,
and you get a connect error rather than a cloud fallback. That is bounded — the plugin is
`enabledByDefault: false`, so it is only in the race once you enabled it, and naming another
provider in `streaming.provider` bypasses ordering entirely.

## Behaviour under failure

- **Close flush.** `close()` sends `{"type":"finalize"}` and arms a 4900 ms fallback timer,
  just inside the 6000 ms close timeout. Without it the last sentence disappears the moment
  the mic is released: yappr still holds an open utterance that only `finalize` cuts.
- **Reconnect.** First open starts clean; a re-open flushes whatever was staged when the
  socket dropped, exactly once, before the new connection starts a turn.
- **Overflow.** Retained transcript bytes are capped at 262144. Overflow clears the turn,
  reports the limit by name through `onError`, and closes — an unbounded accumulator on a
  socket that stopped draining is a memory leak.
- **Connect timeout is 30 s**, not Deepgram's 10. `ready` is only sent once yappr's models
  are resident, and yappr's `[models] preload_at_startup` defaults to `false`, so the first
  connection after a cold start waits for sherpa-onnx and S1-mini (~480 MB) to load.
- Every failure is delivered through `onError(Error)`. Nothing throws asynchronously out of
  a callback.

## Files

| File | Role |
| --- | --- |
| `index.js` | Plugin entry; registers the provider factory |
| `capability-catalog.js` | Cold-start catalog entry, so the provider is discoverable without activating the runtime |
| `provider-factory.js` | Provider descriptor and the yappr-event → callback mapping |
| `config.js` | Config normalization, URL building, token and header resolution |
| `openclaw.plugin.json` | Manifest |
| `package.json` | Package metadata; no dependencies |
