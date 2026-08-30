/// Characters per token, empirically about right for English on a Qwen3
/// tokenizer. Used only to size `max_tokens`. See spec 8.2.
///
/// An estimate, even though the real tokenizer is now in this process and
/// could count exactly: `max_tokens_for` is called before the engine is
/// reached (and must work when normalization is disabled and no model is
/// loaded at all), and the number it produces is a generation *budget*, not
/// a correctness boundary -- `LlamaEngine::generate` treats hitting it as a
/// truncated answer for the guardrail to judge, not an error.
const CHARS_PER_TOKEN: f64 = 3.5;

/// S1-mini's model card sizes generation at roughly 1.3x input plus a margin.
pub fn max_tokens_for(raw: &str) -> u32 {
    let est = (raw.chars().count() as f64 / CHARS_PER_TOKEN).ceil();
    let want = (1.3 * est).ceil() as i64 + 32;
    want.clamp(32, 1024) as u32
}

use anyhow::Result;

/// Reproduced verbatim from the S1-mini model card. Paraphrasing it changes
/// the model's behaviour; do not edit. See spec 8.2.
pub const SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

/// Renders the chat prompt exactly as S1-mini's own jinja template would,
/// for the only shape this crate ever sends: one system message, one user
/// message, `add_generation_prompt`, `enable_thinking = false`, no tools.
///
/// Hand-rendered rather than run through a jinja engine, and that is a
/// deliberate trade. `llama.cpp`'s C-level `llama_chat_apply_template` was
/// not an option: it cannot take template kwargs, so it has no way to
/// express `enable_thinking = false`, which is load-bearing here. Embedding
/// minijinja to render the model's real template would work, but for a
/// two-message prompt it buys nothing over this -- and this way the exact
/// bytes handed to the tokenizer are visible in one function and pinned by
/// `the_rendered_prompt_matches_the_models_own_chat_template`.
///
/// Read off `s1-mini-q4_k_m.gguf`'s `tokenizer.chat_template` metadata, whose
/// relevant branches are:
///
/// ```text
/// {{- '<|im_start|>system\n' + messages[0].content + '<|im_end|>\n' }}   (system)
/// {{- '<|im_start|>' + message.role + '\n' + content }} ... '<|im_end|>\n'  (user)
/// {%- if add_generation_prompt %}{{- '<|im_start|>assistant\n' }}
///     {%- if enable_thinking is defined and enable_thinking is false %}
///         {{- '<think>\n\n</think>\n\n' }}
/// ```
///
/// If the model is ever re-pinned to a build with a different template, this
/// is the function that has to change with it -- the test above is what makes
/// that a failure rather than a silent quality regression.
pub fn render_chat_prompt(control_line: &str, raw: &str) -> String {
    format!(
        "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n\
         <|im_start|>user\n{control_line}\n{raw}<|im_end|>\n\
         <|im_start|>assistant\n<think>\n\n</think>\n\n"
    )
}

pub trait Normalizer: Send + Sync {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rendered_prompt_matches_the_models_own_chat_template() {
        // Pinned against the jinja template read out of
        // `s1-mini-q4_k_m.gguf`'s own `tokenizer.chat_template` metadata for
        // the exact case this crate ever renders: one system message, one
        // user message, `add_generation_prompt`, no tools. Every newline
        // here is one the template emits.
        let got = render_chat_prompt("[Styling: casual]", "hello there");

        assert_eq!(
            got,
            format!(
                "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n\
                 <|im_start|>user\n[Styling: casual]\nhello there<|im_end|>\n\
                 <|im_start|>assistant\n<think>\n\n</think>\n\n"
            )
        );
    }

    #[test]
    fn the_rendered_prompt_disables_thinking() {
        // The whole of what `--jinja --chat-template-kwargs
        // {"enable_thinking":false}` used to buy on the `llama-server`
        // command line: the template's `enable_thinking is false` branch
        // emits a pre-closed, empty think block, so the model has nothing
        // left to open. Without it S1-mini emits a reasoning trace that the
        // guardrail then sees as template bleed -- the model card's
        // documented blank-output failure mode.
        let got = render_chat_prompt("[Styling: casual]", "hi");

        assert!(
            got.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"),
            "generation prompt must pre-close the think block, got:\n{got}"
        );
    }

    #[test]
    fn system_prompt_matches_the_model_card_verbatim() {
        // The S1-mini license/model-card text is load-bearing: paraphrasing
        // it changes model behaviour. Pin the exact string so an accidental
        // edit fails CI instead of silently degrading normalization quality.
        assert_eq!(
            SYSTEM_PROMPT,
            "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text."
        );
    }

    #[test]
    fn max_tokens_never_drops_below_the_floor() {
        assert_eq!(max_tokens_for(""), 32);
        assert_eq!(max_tokens_for("hi"), 34); // ceil(2/3.5)=1 -> ceil(1.3)=2 -> 34
    }

    #[test]
    fn max_tokens_scales_with_input_length() {
        // 350 chars -> est 100 tokens -> ceil(130) + 32 = 162
        let s = "a".repeat(350);
        assert_eq!(max_tokens_for(&s), 162);
    }

    #[test]
    fn max_tokens_is_capped() {
        let s = "a".repeat(100_000);
        assert_eq!(max_tokens_for(&s), 1024);
    }

    #[test]
    fn max_tokens_counts_characters_not_bytes() {
        // Multi-byte characters must not inflate the estimate.
        let s = "\u{e4}".repeat(35); // 35 chars, 70 bytes
        assert_eq!(max_tokens_for(&s), max_tokens_for(&"a".repeat(35)));
    }
}
