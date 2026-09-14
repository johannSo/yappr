/**
 * The plugin entry: the module named by `openclaw.extensions` in `package.json`.
 *
 * ## Why there are no imports from `openclaw` anywhere in this plugin
 *
 * yappr embeds these files in its Rust binary with `include_str!` and writes
 * them to `~/.local/share/yappr/openclaw-plugin/` the moment the user clicks the
 * button in the settings window's AI pane, then runs
 * `openclaw plugins install --link <that dir>`. There is no npm install on that
 * path, no `node_modules` beside the files, and nothing that walks up to the
 * global openclaw install -- so a bare specifier such as
 * `openclaw/plugin-sdk/plugin-entry` cannot resolve, and the plugin would fail
 * to load with a module-not-found error rather than anything diagnosable. That
 * is also why this is plain ESM with no TypeScript and no build step: there is
 * no compiler on that path either.
 *
 * The default export below is therefore the object `definePluginEntry(...)`
 * would have returned, written out by hand. Checked against the host's
 * `dist/plugin-entry-wK36KnFp.mjs` at openclaw 2026.9.4, that helper is
 * near-identity: it copies `id`, `name`, `description`, spreads `kind`,
 * `reload`, `nodeHostCommands` and `securityAuditCollectors` only when present,
 * exposes `configSchema` through a getter that resolves its argument at most
 * once (`createCachedLazyValueGetter`), and passes `register` straight through.
 * Nothing else. If a future host version adds a required field, that file is the
 * one to diff this literal against.
 *
 * ## What the manifest's `configSchema` may and may not declare
 *
 * One row per *canonical* key, and only keys that change behaviour.
 *
 * The first version of this plugin declared every accepted key **and every
 * alias** -- `asrModel` beside `asr_model`, `authToken` beside `auth_token` --
 * because `config.js` accepts all of them. The Control UI renders one row per
 * declared property and humanises the key, so each alias arrived as a second row
 * carrying the same label in a different case: "Asr model" above "Asr Model".
 * Reported 2026-09-14 as "almost every option is here twice". The aliases are
 * still accepted by `config.js` -- they cost nothing and a hand-written config
 * may use them -- they are simply not *advertised*, because an alias is not a
 * second setting.
 *
 * The same argument retires two more rows. `model` is advisory (yappr's active
 * ASR model is `[asr] model` in its own settings, not something a session can
 * switch) and `language` is accepted and ignored outright. A form row that does
 * nothing is worse than no row, so neither is declared.
 *
 * What makes the remaining rows real is `readOwnEntryConfig` in `config.js`: the
 * host builds a transcription provider's `rawConfig` from
 * `plugins.entries.voice-call.config.streaming.providers.yappr` and nowhere else,
 * so without that merge this whole form would be decoration. `resolveConfig`
 * does get the full `cfg`, which is the seam that fixes it.
 */

import { buildYapprRealtimeTranscriptionProvider } from "./provider-factory.js";

export default {
	id: "yappr",
	name: "yappr",
	description: "Realtime speech recognition via a local yappr instance.",
	configSchema: {
		safeParse: (value) => {
			if (value === undefined) return { success: true, data: value };
			if (!value || typeof value !== "object" || Array.isArray(value)) {
				return { success: false, error: { issues: [{ path: [], message: "expected config object" }] } };
			}
			return { success: true, data: value };
		},
		// Shape only. The authoritative gate for `plugins.entries.yappr.config` is
		// the JSON Schema in `openclaw.plugin.json` -- the loader validates against
		// the manifest record, not this one -- and a second copy of those six
		// properties here would be free to drift from the one actually enforced.
		jsonSchema: { type: "object", additionalProperties: false },
	},
	register(api) {
		// The FACTORY, not a built provider. The host calls it with the
		// capability-catalog context that carries
		// `createRealtimeTranscriptionWebSocketSession`; handing over an already
		// built object would mean having reached that helper some other way, and
		// there is no other supported way.
		api.registerRealtimeTranscriptionProvider(buildYapprRealtimeTranscriptionProvider);
	},
};
