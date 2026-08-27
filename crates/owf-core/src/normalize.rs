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

#[cfg(test)]
mod tests {
    use super::*;

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
