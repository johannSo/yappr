/// Characters per token, empirically about right for English on a Qwen3
/// tokenizer. Used only to size `max_tokens`; a local estimate avoids a second
/// round trip to /tokenize on the critical path. See spec 8.2.
const CHARS_PER_TOKEN: f64 = 3.5;

/// S1-mini's model card sizes generation at roughly 1.3x input plus a margin.
pub fn max_tokens_for(raw: &str) -> u32 {
    let est = (raw.chars().count() as f64 / CHARS_PER_TOKEN).ceil();
    let want = (1.3 * est).ceil() as i64 + 32;
    want.clamp(32, 1024) as u32
}

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Reproduced verbatim from the S1-mini model card. Paraphrasing it changes
/// the model's behaviour; do not edit. See spec 8.2.
pub const SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

pub trait Normalizer: Send + Sync {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String>;
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

/// Passed per-request as well as on the `llama-server` command line: either
/// alone has been a reported source of blank output (the model card's
/// documented `enable_thinking` failure mode).
#[derive(Serialize)]
struct ThinkingFlag {
    enable_thinking: bool,
}

#[derive(Serialize)]
struct ChatRequest {
    messages: Vec<ChatMessage>,
    // An integer literal, not `0.0`: serde_json serializes an `f32`/`f64` as
    // a float (`0.0`), which is a different JSON-diff leaf than the plain
    // `0` real llama-server clients send. Wire-format JSON numbers don't
    // otherwise distinguish "int" from "float", so this loses nothing.
    temperature: i32,
    top_k: u32,
    stream: bool,
    max_tokens: u32,
    chat_template_kwargs: ThinkingFlag,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

/// Blocking HTTP client for S1-mini by Superwhisper's OpenAI-compatible
/// `/v1/chat/completions` endpoint, as served by a supervised `llama-server`
/// (see `crate::llama::LlamaServer`).
pub struct S1MiniClient {
    base_url: String,
    timeout: Duration,
}

impl S1MiniClient {
    pub fn new(base_url: String, timeout_ms: u64) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

impl Normalizer for S1MiniClient {
    fn normalize(&self, control_line: &str, raw: &str) -> Result<String> {
        let body = ChatRequest {
            messages: vec![
                ChatMessage { role: "system", content: SYSTEM_PROMPT.to_string() },
                ChatMessage { role: "user", content: format!("{control_line}\n{raw}") },
            ],
            temperature: 0,
            top_k: 1,
            stream: false,
            max_tokens: max_tokens_for(raw),
            chat_template_kwargs: ThinkingFlag { enable_thinking: false },
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        // The *authoritative* timeout is `self.timeout`, enforced below by
        // racing the worker against `recv_timeout` -- that stays exactly as
        // it was, per spec 8.2. But `ureq` 3.4's own `Timeouts::default()` is
        // all `None` (verified), so without a timeout configured on the
        // request itself, a `llama-server` that accepts the connection and
        // then never replies parks the worker thread -- and its socket --
        // forever: `recv_timeout` gives up on *waiting* for that thread, but
        // nothing ever stops the thread itself, so one such utterance leaks
        // a thread and an fd, unbounded (I2). Configuring `timeout_global`
        // at roughly double the caller's budget means the worker can always
        // eventually finish (with an error) and exit cleanly on its own,
        // even on a request `recv_timeout` has already stopped waiting for.
        let ureq_timeout = self.timeout * 2;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<String> {
                let mut resp = ureq::post(&url)
                    .config()
                    .timeout_global(Some(ureq_timeout))
                    .build()
                    .send_json(&body)
                    .map_err(|e| anyhow!("request to llama-server failed: {e}"))?;
                if resp.status() != 200 {
                    bail!("llama-server returned status {}", resp.status());
                }
                resp.body_mut()
                    .read_to_string()
                    .map_err(|e| anyhow!("reading llama-server response body: {e}"))
            })();
            let _ = tx.send(result);
        });

        let text = match rx.recv_timeout(self.timeout) {
            Ok(r) => r?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!("normalization timeout after {:?}", self.timeout)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("normalization worker thread died without a reply")
            }
        };

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("malformed response body from llama-server: {e}"))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("llama-server response contained no choices"))?
            .message
            .content;
        Ok(content.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
