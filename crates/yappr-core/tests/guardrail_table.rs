//! Table-driven fixtures for `yappr_core::guardrail::evaluate`.
//!
//! Each row pins exactly one branch of the guardrail (accept, or one of the
//! five `RejectReason` variants) against the default `GuardrailConfig`. The
//! comment on each row gives the hand-computed word-ratio and/or overlap so
//! the fixture's intent is auditable without re-deriving it from scratch --
//! see task-6-report.md for the full worked arithmetic.
//!
//! This complements (rather than replaces) the inline unit tests in
//! `src/guardrail.rs`, which also exercise `tokenize`/`overlap` directly as
//! standalone helpers.

use yappr_core::config::GuardrailConfig;
use yappr_core::guardrail::{evaluate, RejectReason, Verdict};
use yappr_core::lang::Lang;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    Accept,
    RejectEmpty,
    RejectWordRatio,
    RejectOverlap,
    RejectLoop,
    RejectBleed,
}

fn matches_expectation(v: &Verdict, expect: Expect) -> bool {
    matches!(
        (v, expect),
        (Verdict::Accept, Expect::Accept)
            | (Verdict::Reject(RejectReason::Empty), Expect::RejectEmpty)
            | (Verdict::Reject(RejectReason::WordRatio { .. }), Expect::RejectWordRatio)
            | (Verdict::Reject(RejectReason::Overlap { .. }), Expect::RejectOverlap)
            | (Verdict::Reject(RejectReason::Loop { .. }), Expect::RejectLoop)
            | (Verdict::Reject(RejectReason::TemplateBleed { .. }), Expect::RejectBleed)
    )
}

struct Case {
    name: &'static str,
    raw: &'static str,
    cleaned: &'static str,
    lang: Lang,
    expect: Expect,
}

#[test]
fn guardrail_fixture_table() {
    let cfg = GuardrailConfig::default();

    let cases = vec![
        Case {
            name: "empty cleaned output is always rejected",
            raw: "hello there friend",
            cleaned: "   ",
            lang: Lang::English,
            expect: Expect::RejectEmpty,
        },
        Case {
            // ratio = 9/11 = 0.818 (in [0.55, 1.80]); overlap = 7/11 = 0.636 (>= 0.55).
            name: "a faithful cleanup is accepted",
            raw: "um so the meeting is at uh four thirty on tuesday",
            cleaned: "So the meeting is at 4:30 on Tuesday.",
            lang: Lang::English,
            expect: Expect::Accept,
        },
        Case {
            // ratio = 4/15 = 0.267, below min_word_ratio (0.55).
            name: "a cleanup that drops most of the content is rejected",
            raw: "the quarterly numbers came in higher than we forecast across every region except the nordics",
            cleaned: "The numbers came in.",
            lang: Lang::English,
            expect: Expect::RejectWordRatio,
        },
        Case {
            // raw = 4 tokens (clears short_input_words); ratio = 11/4 = 2.75,
            // above max_word_ratio (1.80).
            name: "a cleanup that invents content is rejected",
            raw: "send it tomorrow please",
            cleaned: "Please make sure that you send it tomorrow morning without fail.",
            lang: Lang::English,
            expect: Expect::RejectWordRatio,
        },
        Case {
            // ratio = 8/8 = 1.0 (passes); overlap = 0/8 = 0.0, below 0.55.
            name: "a confidently wrong rewrite is rejected on overlap",
            raw: "alpha bravo charlie delta echo foxtrot golf hotel",
            cleaned: "One two three four five six seven eight.",
            lang: Lang::English,
            expect: Expect::RejectOverlap,
        },
        Case {
            // overlap = 6/10 = 0.6: clears the English floor (0.55).
            name: "non-english fixture accepted under the looser English floor",
            raw: "eins zwei drei vier funf sechs sieben acht neun zehn",
            cleaned: "Eins zwei drei vier funf sechs alpha bravo charlie delta.",
            lang: Lang::English,
            expect: Expect::Accept,
        },
        Case {
            // Same pair, same overlap (0.6), but below the Other floor (0.70).
            name: "non-english fixture rejected under the stricter Other floor",
            raw: "eins zwei drei vier funf sechs sieben acht neun zehn",
            cleaned: "Eins zwei drei vier funf sechs alpha bravo charlie delta.",
            lang: Lang::Other,
            expect: Expect::RejectOverlap,
        },
        Case {
            // "please send the report to the team" (a 6-gram) repeats 3x in
            // the cleaned text; the n-gram loop check fires before the
            // ratio/overlap checks are ever reached.
            name: "a degenerate loop is rejected",
            raw: "please send the report to the team by friday afternoon at the latest",
            cleaned: "Please send the report to the team please send the report to the team please send the report to the team.",
            lang: Lang::English,
            expect: Expect::RejectLoop,
        },
        Case {
            name: "template bleed marker [Styling: is rejected",
            raw: "hello there friend",
            cleaned: "[Styling: casual] Hello there friend.",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            name: "template bleed marker [Structure: is rejected",
            raw: "hello there friend",
            cleaned: "[Structure: prose] Hello there friend.",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            name: "template bleed marker [Context: is rejected",
            raw: "hello there friend",
            cleaned: "[Context: email] Hello there friend.",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            name: "template bleed marker <think> is rejected",
            raw: "hello there friend",
            cleaned: "<think>hmm</think> Hello there friend.",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            name: "template bleed marker <|im_start|> is rejected",
            raw: "hello there friend",
            cleaned: "<|im_start|>Hello there friend.",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            // raw = 2 tokens, below short_input_words (4): ratio/overlap
            // checks are skipped entirely.
            name: "short inputs skip the ratio and overlap checks",
            raw: "k thx",
            cleaned: "Okay, thanks!",
            lang: Lang::English,
            expect: Expect::Accept,
        },
        Case {
            name: "short inputs still reject empty output",
            raw: "k thx",
            cleaned: "",
            lang: Lang::English,
            expect: Expect::RejectEmpty,
        },
        Case {
            name: "short inputs still reject template bleed",
            raw: "k thx",
            cleaned: "[Styling: casual] Okay",
            lang: Lang::English,
            expect: Expect::RejectBleed,
        },
        Case {
            // Heavy digit normalisation ("four thirty" -> "4:30", "fifteenth"
            // -> "15th") with enough surrounding words to stay inside both
            // floors: ratio = 10/10 = 1.0; overlap = 7/10 = 0.7 (>= 0.55).
            name: "number normalisation survives the overlap check",
            raw: "the meeting is at four thirty on tuesday the fifteenth",
            cleaned: "The meeting is at 4:30 on Tuesday the 15th.",
            lang: Lang::English,
            expect: Expect::Accept,
        },
        Case {
            // KNOWN LIMITATION (M3 threshold-tuning should revisit): a pure
            // digit-sequence dictation compresses too far under aggressive
            // digit rewriting. ratio = 5/10 = 0.5, below min_word_ratio
            // (0.55), even though the rewrite is completely faithful.
            name: "known limitation: dense digit sequences trip word ratio",
            raw: "call me at five five five one two three four",
            cleaned: "Call me at 555-1234.",
            lang: Lang::English,
            expect: Expect::RejectWordRatio,
        },
    ];

    for case in cases {
        let verdict = evaluate(case.raw, case.cleaned, case.lang, &cfg);
        assert!(
            matches_expectation(&verdict, case.expect),
            "case {:?}: expected {:?}, got {verdict:?}",
            case.name,
            case.expect
        );
    }
}
