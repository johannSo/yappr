use std::collections::HashMap;

use crate::config::GuardrailConfig;
use crate::lang::Lang;

const BLEED_MARKERS: [&str; 5] = [
    "[Styling:",
    "[Structure:",
    "[Context:",
    "<think>",
    "<|im_start|>",
];

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    Empty,
    WordRatio { ratio: f64 },
    Overlap { overlap: f64 },
    Loop { ngram: String },
    TemplateBleed { marker: &'static str },
}

impl RejectReason {
    /// Stable short name used in rejections.jsonl.
    pub fn code(&self) -> &'static str {
        match self {
            RejectReason::Empty => "empty",
            RejectReason::WordRatio { .. } => "word_ratio",
            RejectReason::Overlap { .. } => "overlap",
            RejectReason::Loop { .. } => "loop",
            RejectReason::TemplateBleed { .. } => "template_bleed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Accept,
    Reject(RejectReason),
}

/// Case-folded alphanumeric tokens. Punctuation is a separator, so a cleanup
/// that only adds punctuation produces an identical token bag.
pub fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Fraction of raw tokens that also appear in the cleaned text, counted with
/// multiplicity. 1.0 means every raw token survived.
pub fn overlap(raw: &[String], cleaned: &[String]) -> f64 {
    if raw.is_empty() {
        return 1.0;
    }
    let mut budget: HashMap<&str, usize> = HashMap::new();
    for t in cleaned {
        *budget.entry(t.as_str()).or_insert(0) += 1;
    }
    let mut hits = 0usize;
    for t in raw {
        if let Some(n) = budget.get_mut(t.as_str()) {
            if *n > 0 {
                *n -= 1;
                hits += 1;
            }
        }
    }
    hits as f64 / raw.len() as f64
}

fn repeated_ngram(tokens: &[String], n: usize, max_repeats: usize) -> Option<String> {
    if n == 0 || tokens.len() < n {
        return None;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for w in tokens.windows(n) {
        let key = w.join(" ");
        let c = counts.entry(key.clone()).or_insert(0);
        *c += 1;
        if *c >= max_repeats {
            return Some(key);
        }
    }
    None
}

/// Decides whether S1-mini's output is safe to type.
///
/// Order matters: the always-on checks (empty, template bleed, n-gram loop)
/// run first, then the length-sensitive ones, which are skipped for very
/// short inputs where the ratios carry no signal. See spec 9.1.
pub fn evaluate(raw: &str, cleaned: &str, lang: Lang, cfg: &GuardrailConfig) -> Verdict {
    if cleaned.trim().is_empty() {
        return Verdict::Reject(RejectReason::Empty);
    }

    for marker in BLEED_MARKERS {
        if cleaned.contains(marker) {
            return Verdict::Reject(RejectReason::TemplateBleed { marker });
        }
    }

    let raw_tokens = tokenize(raw);
    let clean_tokens = tokenize(cleaned);

    if let Some(ngram) = repeated_ngram(&clean_tokens, cfg.ngram_size, cfg.ngram_max_repeats) {
        return Verdict::Reject(RejectReason::Loop { ngram });
    }

    if raw_tokens.len() < cfg.short_input_words {
        return Verdict::Accept;
    }

    let ratio = clean_tokens.len() as f64 / raw_tokens.len() as f64;
    if ratio < cfg.min_word_ratio || ratio > cfg.max_word_ratio {
        return Verdict::Reject(RejectReason::WordRatio { ratio });
    }

    let ov = overlap(&raw_tokens, &clean_tokens);
    let floor = match lang {
        Lang::English => cfg.min_overlap_english,
        Lang::Other => cfg.min_overlap_other,
    };
    if ov < floor {
        return Verdict::Reject(RejectReason::Overlap { overlap: ov });
    }

    Verdict::Accept
}

/// The minimal cleanup applied to raw ASR text when normalization is skipped
/// or rejected. Deliberately tiny: the user should be able to tell at a glance
/// that S1-mini did not run. See spec 9.2.
///
/// Collapsing runs of whitespace is all that is left here; capitalisation and
/// terminal punctuation moved to [`crate::finish`], which the pipeline applies
/// to *every* path rather than only this one. Keeping a second copy of that
/// logic here is what let the two implementations drift in the first place --
/// this one capitalised and terminated, the accepted-cleanup path did neither.
pub fn rule_based_fallback(raw: &str) -> String {
    crate::finish::finish(&raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GuardrailConfig;
    use crate::lang::Lang;

    fn cfg() -> GuardrailConfig {
        GuardrailConfig::default()
    }

    #[test]
    fn tokenize_lowercases_and_drops_punctuation() {
        assert_eq!(
            tokenize("Hello, World! It's 4:30."),
            vec!["hello", "world", "it", "s", "4", "30"]
        );
    }

    #[test]
    fn overlap_is_one_for_identical_token_bags() {
        let a = tokenize("the meeting is at four thirty");
        assert_eq!(overlap(&a, &a), 1.0);
    }

    #[test]
    fn overlap_is_zero_for_disjoint_bags() {
        let a = tokenize("alpha bravo charlie");
        let b = tokenize("delta echo foxtrot");
        assert_eq!(overlap(&a, &b), 0.0);
    }

    #[test]
    fn overlap_counts_with_multiplicity() {
        let raw = tokenize("yes yes yes yes");
        let cleaned = tokenize("yes yes");
        assert_eq!(overlap(&raw, &cleaned), 0.5);
    }

    #[test]
    fn overlap_of_an_empty_raw_bag_is_one() {
        assert_eq!(overlap(&[], &tokenize("anything")), 1.0);
    }

    #[test]
    fn empty_cleaned_output_is_rejected() {
        let v = evaluate("hello there friend", "   ", Lang::English, &cfg());
        assert!(matches!(v, Verdict::Reject(RejectReason::Empty)));
    }

    #[test]
    fn a_faithful_cleanup_is_accepted() {
        let raw = "um so the meeting is at uh four thirty on tuesday";
        let cleaned = "So the meeting is at 4:30 on Tuesday.";
        assert!(matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept));
    }

    #[test]
    fn a_cleanup_that_drops_most_of_the_content_is_rejected() {
        let raw = "the quarterly numbers came in higher than we forecast \
                   across every region except the nordics";
        let cleaned = "The numbers came in.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::WordRatio { .. })
        ));
    }

    #[test]
    fn a_cleanup_that_invents_content_is_rejected() {
        // Raw must clear `short_input_words` (4) or the ratio check never
        // runs. "send it tomorrow" is only 3 tokens, so it was bumped to
        // 4 with "please" -- see task-6-report.md for the corrected trace:
        // ratio = 11 cleaned / 4 raw = 2.75 > max_word_ratio (1.80).
        let raw = "send it tomorrow please";
        let cleaned = "Please make sure that you send it tomorrow morning without fail.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::WordRatio { .. })
        ));
    }

    #[test]
    fn a_confidently_wrong_rewrite_is_rejected_on_overlap() {
        let raw = "alpha bravo charlie delta echo foxtrot golf hotel";
        // Same length, entirely different words.
        let cleaned = "One two three four five six seven eight.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Overlap { .. })
        ));
    }

    #[test]
    fn non_english_uses_the_stricter_overlap_threshold() {
        // Overlap here is 0.6: above the English floor (0.55), below Other (0.70).
        let raw = "eins zwei drei vier funf sechs sieben acht neun zehn";
        let cleaned = "Eins zwei drei vier funf sechs alpha bravo charlie delta.";
        assert!(
            matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept),
            "English threshold should accept this"
        );
        assert!(
            matches!(
                evaluate(raw, cleaned, Lang::Other, &cfg()),
                Verdict::Reject(RejectReason::Overlap { .. })
            ),
            "Other threshold should reject this"
        );
    }

    #[test]
    fn a_degenerate_loop_is_rejected() {
        let raw = "please send the report to the team by friday afternoon at the latest";
        let cleaned = "Please send the report to the team \
                       please send the report to the team \
                       please send the report to the team.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Loop { .. })
        ));
    }

    #[test]
    fn template_bleed_is_rejected() {
        for bleed in [
            "[Styling: casual] Hello there friend.",
            "[Structure: prose] Hello there friend.",
            "[Context: email] Hello there friend.",
            "<think>hmm</think> Hello there friend.",
            "<|im_start|>Hello there friend.",
        ] {
            assert!(
                matches!(
                    evaluate("hello there friend", bleed, Lang::English, &cfg()),
                    Verdict::Reject(RejectReason::TemplateBleed { .. })
                ),
                "should have rejected: {bleed}"
            );
        }
    }

    #[test]
    fn short_inputs_skip_the_ratio_and_overlap_checks() {
        // 2 raw words; a normalizer may legitimately expand "gonna" or fix a
        // homophone, and the ratios are meaningless at this length.
        let v = evaluate("k thx", "Okay, thanks!", Lang::English, &cfg());
        assert!(matches!(v, Verdict::Accept), "got {v:?}");
    }

    #[test]
    fn short_inputs_still_reject_empty_and_bleed() {
        assert!(matches!(
            evaluate("k thx", "", Lang::English, &cfg()),
            Verdict::Reject(RejectReason::Empty)
        ));
        assert!(matches!(
            evaluate("k thx", "[Styling: casual] Okay", Lang::English, &cfg()),
            Verdict::Reject(RejectReason::TemplateBleed { .. })
        ));
    }

    #[test]
    fn number_normalisation_survives_the_overlap_check() {
        // S1-mini rewrites spoken numbers into digits, which legitimately
        // costs token overlap and word-count ratio. This fixture keeps
        // enough surrounding words that both floors still clear:
        // ratio = 10 cleaned / 10 raw = 1.0; overlap = 7/10 = 0.7 (english
        // floor 0.55) -- see task-6-report.md for the full trace. The
        // original fixture ("call me at five five five one two three
        // four" -> "Call me at 555-1234.") does NOT survive; see
        // `known_limitation_dense_digit_sequences_trip_word_ratio` below.
        let raw = "the meeting is at four thirty on tuesday the fifteenth";
        let cleaned = "The meeting is at 4:30 on Tuesday the 15th.";
        assert!(
            matches!(evaluate(raw, cleaned, Lang::English, &cfg()), Verdict::Accept),
            "digit rewriting must not trip the guardrail"
        );
    }

    #[test]
    fn known_limitation_dense_digit_sequences_trip_word_ratio() {
        // KNOWN LIMITATION (revisit in M3 threshold tuning): a dictation
        // that is *nothing but* a spoken digit sequence compresses too much
        // under aggressive digit normalisation to survive min_word_ratio.
        // raw: 10 tokens ("call me at five five five one two three four").
        // cleaned: "Call me at 555-1234." -> 5 tokens.
        // ratio = 5 / 10 = 0.5, below min_word_ratio (0.55) -> rejected
        // before overlap is ever consulted, even though the rewrite is
        // completely faithful. Falls back to raw ASR text, which is safe
        // but not ideal for phone numbers dictated digit-by-digit.
        let raw = "call me at five five five one two three four";
        let cleaned = "Call me at 555-1234.";
        assert!(matches!(
            evaluate(raw, cleaned, Lang::English, &cfg()),
            Verdict::Reject(RejectReason::WordRatio { .. })
        ));
    }

    #[test]
    fn fallback_collapses_whitespace_capitalises_and_terminates() {
        assert_eq!(rule_based_fallback("  hello   there  "), "Hello there.");
        assert_eq!(rule_based_fallback("already done."), "Already done.");
        assert_eq!(rule_based_fallback("what about this?"), "What about this?");
        assert_eq!(rule_based_fallback("hey!"), "Hey!");
        assert_eq!(rule_based_fallback(""), "");
        assert_eq!(rule_based_fallback("   "), "");
        // Leading brackets and quotes must not block capitalisation...
        assert_eq!(rule_based_fallback("\"hello there\""), "\"Hello there\".");
        // ...but a leading digit must, since the sentence really does start
        // with that number. This used to produce `42 Things happened.`,
        // capitalising a word in mid-sentence, because the search was for the
        // first *alphabetic* character rather than the first alphanumeric
        // one. See `crate::finish`, which now owns this rule.
        assert_eq!(rule_based_fallback("42 things happened"), "42 things happened.");
    }
}
