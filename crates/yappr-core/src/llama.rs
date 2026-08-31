//! S1-mini, in-process.
//!
//! This module used to spawn and supervise a `llama-server` child: pick a
//! port, poll `/health`, restart it with backoff when it died, reap it on
//! shutdown. All of that is gone. The model is loaded into this process
//! through `llama-cpp-2`, which links llama.cpp and a ggml CPU backend
//! statically -- `ldd` on the built binary resolves no `libllama` or
//! `libggml` at all -- so the `llama-cpp` and `ggml-cpu` system packages are
//! no longer prerequisites, and ggml's opaque "no backends are loaded"
//! failure is no longer reachable.
//!
//! What that trade costs, stated plainly: a crash inside llama.cpp now takes
//! the whole app down, where a crashing child process used to degrade to
//! `UnavailableNormalizer` and leave dictation working on raw ASR text.
//! `sherpa-onnx` has always had exactly this property on the ASR side, which
//! runs on *every* utterance rather than only the cleanup pass, so this adds
//! no new class of risk -- but it does widen an existing one.
//!
//! What it buys, beyond the packages: the model is resident in ~440 ms
//! rather than the ~1050 ms a spawn-and-wait-for-`/health` took, which is
//! paid on every lazy load (invariant 12); the normalization timeout is a
//! deadline check inside the decode loop rather than a worker thread raced
//! against `recv_timeout`, so a slow generation can no longer leak a thread
//! and a socket; and the model's lifetime is exactly the pipeline's, so
//! there is no longer a child process to keep in step with `models_loaded`.

use anyhow::{anyhow, bail, Context, Result};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use crate::config::NormalizeConfig;
use crate::normalize::{max_tokens_for, render_chat_prompt, Normalizer};

/// The one model file this engine loads, pinned by sha256 in
/// `models.lock.toml` alongside Parakeet and Silero.
pub const MODEL_FILE: &str = "s1-mini-q4_k_m-de-v3.gguf";

/// The process-global llama.cpp backend.
///
/// `llama_backend_init` is process-global and the crate enforces that with a
/// one-shot flag: a second `LlamaBackend::init()` returns
/// `BackendAlreadyInitialized` rather than a second handle. Because
/// `[models] idle_unload_seconds` makes load-unload-load an ordinary cycle
/// (invariant 12), a per-engine `init()` would fail on the *second*
/// dictation of any session that had gone idle -- so it is initialised once
/// here and never dropped. Dropping it would call `llama_backend_free` while
/// a later engine might still want it.
///
/// The stored value is a `Result` rather than the backend itself because
/// `OnceLock` has no fallible initialiser on stable: a failure is cached and
/// re-reported to every later caller, which is correct -- if the backend
/// could not come up once, it will not come up later either.
fn backend() -> Result<&'static LlamaBackend> {
    static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();
    match BACKEND.get_or_init(|| {
        LlamaBackend::init()
            .map(|mut b| {
                // llama.cpp is chatty on stderr -- model metadata, tensor
                // tables, timings -- and this process shares stderr with
                // `tracing`. Silence it at the source rather than filtering
                // it downstream.
                b.void_logs();
                b
            })
            .map_err(|e| e.to_string())
    }) {
        Ok(b) => Ok(b),
        Err(e) => Err(anyhow!("llama.cpp backend failed to initialise: {e}")),
    }
}

/// A loaded S1-mini, ready to normalize.
///
/// Owns the ~480 MB of weights. There is no explicit unload: dropping this
/// frees them, and because the engine is owned by the `Pipeline`'s
/// normalizer, `unload_models` setting `pipeline = None` is what releases
/// the model. That is the whole of the lifetime management the supervised
/// child used to need `daemon.llama`, `kill_llama` and a `Drop` impl for.
pub struct LlamaEngine {
    model: LlamaModel,
    n_ctx: u32,
    n_threads: i32,
    timeout: Duration,
}

impl LlamaEngine {
    /// Loads the model at `path`.
    ///
    /// The existence check is not merely a nicer error: `LlamaModel::
    /// load_from_file` carries a `debug_assert!` on the path existing, so in
    /// a debug or test build a missing file would abort the process before
    /// it could return `Err`. Checking first is what makes a missing model a
    /// recoverable error in every profile.
    pub fn load(path: &Path, cfg: &NormalizeConfig) -> Result<Self> {
        if !path.exists() {
            bail!("missing model file {}", path.display());
        }
        let backend = backend()?;
        let model = LlamaModel::load_from_file(backend, path, &LlamaModelParams::default())
            .with_context(|| format!("loading {}", path.display()))?;
        tracing::info!(path = %path.display(), "S1-mini loaded in-process");
        Ok(Self {
            model,
            n_ctx: cfg.context_size,
            n_threads: cfg.threads as i32,
            timeout: Duration::from_millis(cfg.timeout_ms),
        })
    }

    /// Greedily decodes a completion for `prompt`, stopping at an
    /// end-of-generation token, at `max_tokens`, or at `deadline`.
    ///
    /// Greedy, not sampled, because that is what `temperature: 0` plus
    /// `top_k: 1` meant on the HTTP request this replaces: for a normalizer,
    /// reproducibility matters more than variety.
    ///
    /// A fresh context per call, rather than one cached on `self`: a
    /// `LlamaContext` borrows its model, so holding both in one struct would
    /// make it self-referential, and a context is neither `Send` nor `Sync`
    /// while `Normalizer` requires both. It costs about 75 ms against a
    /// call that runs for several hundred, and it means the KV cache is
    /// released between utterances instead of held for the life of the
    /// daemon.
    pub fn generate(&self, prompt: &str, max_tokens: u32, deadline: Instant) -> Result<String> {
        let tokens = self
            .model
            .str_to_token(prompt, AddBos::Never)
            .map_err(|e| anyhow!("tokenizing the prompt failed: {e}"))?;

        // Both halves have to fit: llama.cpp will not grow the context, and
        // a prompt that only just fits would leave no room to answer in.
        // `max_tokens_for` caps at 1024 and the default context is 2048, so
        // reaching this needs a genuinely long dictation against a shrunken
        // `context_size`.
        let needed = tokens.len() as u64 + u64::from(max_tokens);
        if needed > u64::from(self.n_ctx) {
            bail!(
                "prompt ({} tokens) plus reply ({max_tokens}) exceeds normalize.context_size ({})",
                tokens.len(),
                self.n_ctx
            );
        }

        let mut ctx = self
            .model
            .new_context(
                backend()?,
                LlamaContextParams::default()
                    .with_n_ctx(NonZeroU32::new(self.n_ctx))
                    .with_n_threads(self.n_threads)
                    .with_n_threads_batch(self.n_threads),
            )
            .map_err(|e| anyhow!("creating a llama context failed: {e}"))?;

        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        let last = tokens.len().saturating_sub(1);
        for (i, token) in tokens.iter().enumerate() {
            batch
                .add(*token, i as i32, &[0], i == last)
                .map_err(|e| anyhow!("building the prompt batch failed: {e}"))?;
        }
        ctx.decode(&mut batch).map_err(|e| anyhow!("prefill failed: {e}"))?;

        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
        // One decoder for the whole generation, deliberately: a BPE
        // tokenizer can split a multi-byte codepoint across two tokens, and
        // a decoder created per token would turn each half into U+FFFD.
        // German dictation -- umlauts and eszett -- is exactly where that
        // shows up.
        let mut decoder = encoding_rs::UTF_8.new_decoder();

        let mut out = String::new();
        for generated in 0..max_tokens {
            // Where the token about to be sampled will sit in the context:
            // the prompt occupies `0..tokens.len()`, and each accepted token
            // extends that by one.
            let pos = tokens.len() as i32 + generated as i32;
            if Instant::now() >= deadline {
                bail!("normalization timeout after {:?}", self.timeout);
            }
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                return Ok(out.trim().to_string());
            }
            out.push_str(
                &self
                    .model
                    .token_to_piece(token, &mut decoder, false, None)
                    .map_err(|e| anyhow!("detokenizing failed: {e}"))?,
            );
            batch.clear();
            batch
                .add(token, pos, &[0], true)
                .map_err(|e| anyhow!("building the decode batch failed: {e}"))?;
            ctx.decode(&mut batch).map_err(|e| anyhow!("decode failed: {e}"))?;
        }
        // Hit `max_tokens` without an end-of-generation token. Not an
        // error: the budget is an estimate (`max_tokens_for`), and a
        // truncated cleanup is still a cleanup -- the guardrail is what
        // decides whether it is trustworthy, and a truncation is exactly
        // the kind of damage its word-ratio check exists to catch.
        Ok(out.trim().to_string())
    }
}

impl Normalizer for LlamaEngine {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String> {
        // The deadline is checked between tokens rather than enforced by
        // racing a worker thread against `recv_timeout`, which is what the
        // HTTP client had to do. Nothing to leak: when this gives up, the
        // decode loop it gives up inside is this same thread.
        self.generate(
            &render_chat_prompt(control_line, raw),
            max_tokens_for(raw),
            Instant::now() + self.timeout,
        )
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    /// The load/unload cycle invariant 12 makes ordinary would hit
    /// `LlamaCppError::BackendAlreadyInitialized` on the *second* load if
    /// the backend were initialised per-engine: `llama_backend_init` is a
    /// process-global that the crate guards with a one-shot `AtomicBool`.
    /// An idle unload followed by the next press is exactly that second
    /// load, so this is the first thing that would break in normal use, not
    /// an edge case.
    #[test]
    fn the_backend_can_be_obtained_more_than_once_in_one_process() {
        let first = backend().expect("first backend init");
        let second = backend().expect("second backend init");

        assert!(
            std::ptr::eq(first, second),
            "both calls must hand back the one process-global backend"
        );
    }

    #[test]
    fn loading_a_model_that_is_not_there_fails_instead_of_panicking() {
        let cfg = NormalizeConfig::default();

        let err = match LlamaEngine::load(Path::new("/nonexistent/s1-mini-q4_k_m-de-v3.gguf"), &cfg) {
            Ok(_) => panic!("a missing model file must be an error"),
            Err(e) => e,
        };

        assert!(
            err.to_string().contains("/nonexistent/s1-mini-q4_k_m-de-v3.gguf"),
            "the error should name the path it looked for, got: {err}"
        );
    }

    #[test]
    #[ignore = "needs the downloaded S1-mini model"]
    fn generate_normalizes_a_raw_transcript() {
        let cfg = NormalizeConfig::default();
        let engine = LlamaEngine::load(&model_path(), &cfg).expect("loading S1-mini");
        let raw = "kannst du mir bitte bescheid geben ob das so passt";
        let prompt = render_chat_prompt("[Styling: casual]", raw);

        let out = engine
            .generate(&prompt, max_tokens_for(raw), Instant::now() + Duration::from_secs(60))
            .expect("generation");

        // Not an exact-string assertion: the point is that a real cleanup
        // came back, not that this particular model build words it one way.
        // What must hold is that the think block never leaks (spec 8.2's
        // blank-output failure mode) and the text is recognisably the input.
        assert!(!out.contains("<think>"), "think block leaked into output: {out}");
        assert!(!out.contains("<|im_"), "chat markup leaked into output: {out}");
        assert!(out.to_lowercase().contains("bescheid"), "unrecognisable output: {out}");
    }

    #[test]
    #[ignore = "needs the downloaded S1-mini model"]
    fn generate_gives_up_when_the_deadline_has_passed() {
        let cfg = NormalizeConfig::default();
        let engine = LlamaEngine::load(&model_path(), &cfg).expect("loading S1-mini");
        let prompt = render_chat_prompt("[Styling: casual]", "hallo welt");

        let err = engine
            .generate(&prompt, 512, Instant::now())
            .expect_err("an already-passed deadline must abort generation");

        assert!(err.to_string().contains("timeout"), "got: {err}");
    }

    /// The umlaut case the shared `encoding_rs` decoder in `generate`
    /// exists for. Pinned as a round trip through the real tokenizer rather
    /// than asserted about the decoder directly, because what matters is
    /// that German survives tokenize -> detokenize intact.
    #[test]
    #[ignore = "needs the downloaded S1-mini model"]
    fn detokenizing_preserves_german_multibyte_characters() {
        let cfg = NormalizeConfig::default();
        let engine = LlamaEngine::load(&model_path(), &cfg).expect("loading S1-mini");
        let text = "Grüße für Jörg, die Straße ist gesperrt.";

        let tokens = engine.model.str_to_token(text, AddBos::Never).expect("tokenizing");
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut back = String::new();
        for t in tokens {
            back.push_str(&engine.model.token_to_piece(t, &mut decoder, false, None).unwrap());
        }

        assert_eq!(back, text);
    }

    /// The integration point the two tests above each cover half of:
    /// `Normalizer::normalize` is what `Pipeline` actually calls, and it is
    /// what wires `render_chat_prompt`, `max_tokens_for` and the configured
    /// `timeout_ms` together into one `generate`. A mistake in that wiring
    /// -- the wrong control line, a budget of zero, a deadline already in
    /// the past -- would leave both of the tests above passing.
    #[test]
    #[ignore = "needs the downloaded S1-mini model"]
    fn the_normalizer_trait_cleans_up_a_transcript_end_to_end() {
        let cfg = NormalizeConfig::default();
        let engine = LlamaEngine::load(&model_path(), &cfg).expect("loading S1-mini");

        let out = Normalizer::normalize(
            &engine,
            "[Styling: casual] [Structure: prose] [Context: general]",
            "also das meeting ist morgen um zehn uhr",
        )
        .expect("normalizing through the trait");

        assert!(!out.is_empty(), "the normalizer returned nothing");
        assert!(!out.contains("<think>"), "think block leaked: {out}");
        assert!(!out.contains("<|im_"), "chat markup leaked: {out}");
        assert!(out.to_lowercase().contains("meeting"), "unrecognisable output: {out}");
    }

    fn model_path() -> std::path::PathBuf {
        crate::paths::models_dir().join(MODEL_FILE)
    }
}
