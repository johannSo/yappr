# Selectable ASR models — design

Status: approved, 2026-09-08. Extends `2026-08-27-openwhisprflow-design.md` §5
(model provisioning) and §17.1 (the `Transcriber` seam), and revises the
provisioning half of `2026-08-29-lazy-model-lifecycle-design.md`. Adds no new
process, no new binary, and no new wire-format variant.

Cite this document as `spec asr-model §N` in code comments, not bare `spec §N`:
the two pipeline-era design docs already collide on section numbers, and this
one would be a third claimant.

## Purpose

The ASR engine is one hardcoded artifact. `asr.rs` opens
`models_dir()/parakeet-tdt-0.6b-v3-int8` by name, `models.rs`' `ARTIFACTS` is
a fixed array of three that provisioning treats as all-required, and nothing
anywhere lets a user say which model they want.

Three models are worth offering, and each is a different kind of win:

| Model | Why | Languages |
|---|---|---|
| `primeline/parakeet-primeline` | 2.95 % avg WER vs the shipped v3's 3.64 %; 4.11 vs 7.05 on Tuda-De. The best German model available, and this app is German-first. | German |
| `nvidia/parakeet-unified-en-0.6b` | Newer English model than the v3 base. | English |
| `nvidia/nemotron-3.5-asr-streaming-0.6b` | Multilingual with per-stream language selection; the basis for the streaming mode planned later. | 35 languages |

None of the three upstream Hugging Face repositories ships ONNX — they are
`.nemo` checkpoints, which sherpa-onnx cannot load. Two have first-party
sherpa-onnx exports published as assets on the same
`k2-fsa/sherpa-onnx` `asr-models` release the current model already comes
from. The third does not, and §9 covers how it is produced.

sherpa-onnx v1.13.6 (released 2026-08-18, the version `sherpa-onnx-sys` pins)
postdates every one of these exports, so the linked runtime supports them.

## 1. The model catalogue

`models.rs` gains a catalogue. `Artifact` is unchanged; each catalogue entry
owns one.

```rust
pub enum AsrFlavor {
    /// sherpa-onnx `OfflineRecognizer`, as today.
    Offline,
    /// sherpa-onnx `OnlineRecognizer`. The whole utterance is fed at once
    /// and the final result taken — see §3.
    CacheAwareStreaming,
}

pub struct AsrModelSpec {
    /// The `[asr] model` spelling. Distinct from `artifact.name`, which is
    /// the lock-file key and can never change.
    pub key: &'static str,
    pub artifact: Artifact,
    pub flavor: AsrFlavor,
    /// German label for the dropdown; carries the language, because that is
    /// the only thing that decides the choice for most users.
    pub display: &'static str,
}
```

| `key` | lock key (`Artifact::name`) | `rel_path` | flavor |
|---|---|---|---|
| `parakeet-tdt-v3` | **`parakeet`** | `parakeet-tdt-0.6b-v3-int8` | Offline |
| `parakeet-unified-en` | `parakeet-unified-en` | `parakeet-unified-en-0.6b-int8` | Offline |
| `nemotron-3.5` | `nemotron-3.5-560ms` | `nemotron-3.5-asr-streaming-0.6b-560ms-int8` | CacheAwareStreaming |
| `parakeet-primeline-de` | `parakeet-primeline-de` | `parakeet-primeline-de-int8` | Offline |

The v3 entry's lock key stays the bare string `parakeet`. `Artifact::name`'s
doc comment forbids changing it, and it is what the committed
`models.lock.toml` is keyed on: renaming it would invalidate the pin on every
machine that already has the model.

URLs, all `.tar.bz2` under
`https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/`:

- `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2` (487 MB, unchanged)
- `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2` (501 MB)
- `sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11.tar.bz2` (475 MB)

primeline is hosted by us (§9). Every one of the four unpacks to the layout
`hash_target` already assumes — `encoder.int8.onnx`, `decoder.int8.onnx`,
`joiner.int8.onnx`, `tokens.txt` — so `download_to`, `extract_tar_bz2`,
`promote_dir` and the encoder-only hash convention all apply with no change.

nemotron-3.5 is published in five chunk sizes (80/160/320/560/1120 ms). 560 ms
is the one we bundle: enough right context that accuracy in the paste-in-one
mode of §3 is near the large-chunk ceiling, and still a usable perceived
latency when the real streaming mode is built, so the download is not thrown
away then. The chunk size is deliberately **not** a setting — it would mean
nothing to a user until streaming exists, and would multiply the pinned
artifacts by five.

## 2. Configuration

Two new keys in `[asr]`. Both take `#[serde(default = …)]`, so every
`config.toml` written before this change keeps loading under
`deny_unknown_fields` (invariant 4).

```toml
[asr]
# Welches Spracherkennungs-Modell verwendet wird.
model = "parakeet-tdt-v3"
# Nur für mehrsprachige Modelle: "auto" oder ein Sprachkürzel wie "de".
language = "auto"
num_threads = 4
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `model` | enum | `parakeet-tdt-v3` | Which catalogue entry §1 to load and provision. |
| `language` | string | `"auto"` | Passed to a `CacheAwareStreaming` model per stream. Inert for `Offline` models. |

`AsrModel` is a plain serde enum with one explicit `rename` per spelling —
the same shape as `InjectBackend` and `PasteChord`, down to the
one-round-trip-test-per-spelling that `config.rs` already has for those.

The default deliberately stays `parakeet-tdt-v3`: it is what every existing
install has on disk and pinned, so nothing about a fresh install's download or
an upgrade's behaviour moves. The German-optimal choice is one dropdown away,
not imposed.

`language` validates as `"auto"` or two lowercase ASCII letters. `Config::validate`
rejects anything else, as it already does for `num_threads < 1`.

## 3. Two transcriber flavours behind the existing trait

`Pipeline` takes `Box<dyn Transcriber>` already, and `Transcriber` is
documented as the seam for exactly this (spec §17.1). Both flavours implement
it, so the guardrail, vocabulary, `finish`, injection, the state machine and
every `OverlayEvent` are untouched. "Paste in one go" is not a special case
to build — it is what falls out of keeping this seam.

```rust
pub fn build(models_dir: &Path, cfg: &AsrConfig) -> Result<Box<dyn Transcriber>>
```

replaces the direct `SherpaTranscriber::new` at `server.rs`'s `load_models`.
It resolves `cfg.model` to its `AsrModelSpec` and dispatches on `flavor`.

**`SherpaTranscriber`** (existing) stops hardcoding the directory name and
takes the spec's `rel_path`. Its `model_type = "nemo_transducer"` and the rest
of `OfflineRecognizerConfig` are unchanged; all three Offline entries are
NeMo transducers.

**`SherpaStreamingTranscriber`** (new) wraps `OnlineRecognizer` with
`OnlineTransducerModelConfig` over the same three `.onnx` files. `transcribe`
is the whole utterance at once:

```
let stream = recognizer.create_stream();
stream.set_option("language", &self.language);   // see §8
stream.accept_waveform(SAMPLE_RATE, samples);
stream.input_finished();
while recognizer.is_ready(&stream) { recognizer.decode(&stream); }
recognizer.get_result(&stream).map(|r| r.text).unwrap_or_default()
```

`enable_endpoint` stays `false`: endpointing exists to cut a live stream into
utterances, and the VAD has already decided where this one ends. The empty-input
early return that `SherpaTranscriber::transcribe` has is kept, for the same
reason.

## 4. Provisioning becomes selection-driven

The one contract that genuinely changes. Today `verify`, `looks_present` and
`download_all` each iterate all of `ARTIFACTS` and treat every entry as
required. With four ASR models in the tree and one selected, "required" has to
mean the selected one.

```rust
/// Silero, S1-mini, and the one ASR model `cfg` selects. The only thing
/// provisioning may treat as required.
pub fn required_artifacts(cfg: &Config) -> Vec<&'static Artifact>
```

`verify`, `looks_present` and `download_all` take a `&[&Artifact]` and the
callers pass `required_artifacts`. Consequences, each of which is a place this
can go wrong:

- **Unselected models on disk are never touched.** Not verified, not
  redownloaded, and — importantly — never deleted. Switching back to a model
  you downloaded last month is instant and offline. Reclaiming that disk space
  is out of scope (§10).
- **`provision.rs`'s `missing_models_cached` must be invalidated on an
  `[asr] model` change.** It caches for the whole process lifetime, which was
  correct when the answer could not change. Once the answer depends on config,
  a stale cache reports a freshly selected, entirely absent model as ready —
  and the failure surfaces as a failed dictation, not a missing-model message.
  The cache key becomes the selected model.
- **`wizard.rs`'s `should_open` needs no change and gains a useful behaviour
  for free.** It already opens the wizard at the models step when
  `is_ready_or_assume_not` is false, so a restart after selecting an
  undownloaded model lands the user exactly where the download button is.

## 5. Settings GUI

`schema.ts` only:

- `CHOICES["asr.model"]` — the four `key` spellings, in catalogue order.
- `FIELD_LABELS["asr.model"] = "Modell"`, `"asr.language" = "Sprache"`.
- `FIELD_HELP` naming each model's language and that a switch downloads
  roughly 500 MB.
- `asr` is already in `RESTART_SECTIONS`, which stays correct: changing the
  model requires a model reload.

Selecting a model autosaves immediately — it is a dropdown, and invariant 9
gives dropdowns no debounce. So the moment after the save, the Sprache pane
may be showing a selected model that is not on disk. The row then renders
*"Nicht heruntergeladen — jetzt laden"* with a button, reusing
`setup_status` / `run_setup` and the existing `setup-progress` listener that
the wizard's Modelle step already owns.

The rejected alternative was reopening the wizard on a dropdown change. It
reuses more code, but the wizard takes over the entire window while active
(invariant 13's `should_open` behaviour), and having a dropdown swallow the
settings window reads as a bug.

## 6. First-run wizard

The Modelle step gains the same dropdown, above the download button.
Changing it patches `{"asr": {"model": …}}` through the config path
`wizard_finish`'s `backend_patch` already uses — a single leaf, which
`config_write::save_config` merges rather than renders (invariant 9) — and
then re-runs `setup_status` so the missing-model list reflects the new choice.

A running download blocks the step, exactly as it does now, and for the reason
already recorded in `wizard.tsx`: walking on mid-download reads as "this is
finished" at 3 %.

## 7. The lock file

`models.lock.toml` gains a pin for **every** catalogue entry, installed or
not. `download_all` refuses an unpinned artifact unless `--update-lock` was
given, so an entry with no pin is an entry no user can ever download, and the
failure appears only for whoever selects it.

Pins are produced the documented way — `yappr --update-lock` — and committed.
A test asserts every `AsrModelSpec` has a pin in the compiled-in lock, so
adding a fifth model without pinning it fails the build's test gate rather
than one user's first dictation.

## 8. Verification items that cannot be settled without the models

Three, all in the streaming path, and all of which fail as *wrong text*
rather than as an error — which makes them exactly the kind of thing that must be pinned by a
test rather than reasoned about:

1. **`feat_config.feature_dim`.** The Rust crate's
   `OnlineRecognizerConfig::default` sets `80`; these NeMo models are
   `feat_dim: 128`. The offline path never had to care, because
   `OfflineRecognizer` reads the dimension from the encoder's ONNX metadata.
2. **`model_config.model_type`.** The offline path sets `"nemo_transducer"`
   explicitly. Whether the online path auto-detects from encoder metadata or
   needs a string is unverified.
3. **The language option's key.** The upstream export's README says "use
   per-stream language strings such as `en`, `ja`, or `auto`";
   `OnlineStream::set_option` is the plausible mechanism but the key name is
   unconfirmed.

All three are settled by a new `#[ignore]`d test in `tests/asr_fixture.rs`
that transcribes the checked-in fixture with the streaming model and asserts
real German words come back — the `cargo test --workspace -- --ignored` half
of the gate, which CLAUDE.md already makes non-optional.

## 9. Producing the primeline export

primeline-parakeet is a German finetune of `nvidia/parakeet-tdt-0.6b-v3` —
the same FastConformer-TDT architecture yappr already ships — so sherpa-onnx's
own first-party export script for that model applies unmodified.
`scripts/nemo/parakeet-tdt-0.6b-v3/export_onnx.py` prefers a local `.nemo`
next to it over the Hugging Face download, so pointing it at primeline's
`2_95_WER.nemo` is the whole change. It writes `tokens.txt`, exports encoder /
decoder / joiner, dynamic-quantises each to int8, and stamps the ONNX metadata
sherpa reads.

Steps, outside this repository:

1. `pip install nemo_toolkit['asr'] "numpy<2" onnx==1.17.0 onnxruntime==1.17.1 kaldi-native-fbank librosa soundfile`
2. Fetch `2_95_WER.nemo`, rename to `parakeet-tdt-0.6b-v3.nemo`, run `export_onnx.py`.
3. Sanity-check with the script's `test_onnx.py` against a German wav.
4. `tar cjf` the four files under **one top-level directory** — `extract_tar_bz2`
   flattens exactly one and fails otherwise.
5. Upload beside `s1-mini-de-v3` on the project's Hugging Face account; pin the
   sha256 via `--update-lock`.

`x-ian/sherpa-onnx-parakeet-primeline-de-int8` is a pre-existing third-party
export of the same model. It is not used — we do not want an unaudited
third party in the supply chain for a model the app runs locally — but it is a
useful correctness check to diff our export against.

Everything else in this design ships without this step; the primeline
catalogue entry simply lands last.

## 10. Out of scope

- Real streaming: partial results, an overlay that grows text as you speak,
  endpointing. The nemotron model is the groundwork, not the feature.
- Deleting unselected models to reclaim disk.
- Matching the normalizer to the ASR language. S1-mini is a German finetune;
  selecting an English-only ASR model does not change that, and the guardrail's
  existing English/other split (`lang.rs`) already governs what happens next.
- Hotword / `bpe.vocab` support, which the newer exports ship and the current
  one does not.

## 11. Documentation to update

`CLAUDE.md` states Parakeet TDT as *the* engine in its opening paragraph and
in the architecture section, and `docs/HANDOVER.md` inherits that. Both need
to describe a selected model instead. `config_write.rs`'s annotated default
fixture gains the two new keys.
