/**
 * The cold-start entry point, named by `capabilityCatalogEntry` in
 * `openclaw.plugin.json`.
 *
 * The host loads this module *without* activating the plugin runtime, which is
 * what lets `talk.catalog.transcription` list yappr while `activation.onStartup`
 * stays `false`. Nothing here may open a socket, read a store or touch the
 * filesystem: `buildYapprRealtimeTranscriptionProvider` is pure, and this file
 * stays one line so it cannot stop being.
 *
 * `context` is the `PluginCapabilityCatalogContext` the host injects. It carries
 * `createRealtimeTranscriptionWebSocketSession` -- the only supported way an
 * installed plugin reaches that helper.
 */

import { buildYapprRealtimeTranscriptionProvider } from "./provider-factory.js";

export default (context) => ({
	realtimeTranscriptionProviders: [buildYapprRealtimeTranscriptionProvider(context)],
});
