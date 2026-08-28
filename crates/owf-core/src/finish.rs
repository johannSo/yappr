//! The last thing that happens to dictated text before it is injected:
//! capitalise the start of every sentence, and make sure the utterance ends
//! on a sentence terminator.
//!
//! This exists as its own stage because it must hold for *every* path out of
//! `pipeline::process_with_capture`, not just one. Before it, only
//! `guardrail::rule_based_fallback` did this work, so the fallback paths
//! (normalizer dead, cleanup rejected) produced properly finished text while
//! the *success* path -- an accepted S1-mini cleanup, assigned straight to
//! `text` -- did not. Real captures show what that costs: S1-mini reliably
//! returns `"...werden. ich fix das auch gleich mit"` for
//! `"...werden. Ich fix das auch gleich mit."`, lowercasing every sentence
//! start and dropping the final stop, and the guardrail cannot see it --
//! `guardrail::tokenize` lowercases and strips punctuation *before*
//! comparing, so case and terminator damage is invisible to it by
//! construction.
//!
//! Applied at a single choke point just before injection rather than at each
//! site that assigns `text`, so a future path added to the pipeline is
//! finished too without anyone remembering to call it -- the same structural
//! argument `DebugRecordGuard` makes for debug records. Idempotent, so the
//! paths that already went through `rule_based_fallback` are unharmed.

/// Sentence terminators. `\u{2026}` is a real single-character ellipsis,
/// which ASR emits for a trailing-off utterance.
const TERMINATORS: [char; 4] = ['.', '!', '?', '\u{2026}'];

/// Characters that may legitimately sit *after* a terminator: closing
/// quotes and brackets. `Er sagte: "nein."` is already terminated even
/// though its last character is a quotation mark.
const CLOSERS: [char; 10] = ['"', '\'', '\u{201d}', '\u{201c}', '\u{2019}', '\u{bb}', '\u{203a}', ')', ']', '}'];

/// Capitalises the opening letter and guarantees a sentence terminator.
///
/// Idempotent by construction: capitalising an already-capital letter is a
/// no-op, and already-terminated text gains nothing.
/// Abbreviations whose closing period is not a sentence break, restricted to
/// the ones `is_abbreviation`'s structural rules do *not* already catch: no
/// entry here carries an internal period, is a bare number, or is a single
/// initial. Lowercase; matching is case-insensitive.
///
/// Intended to become user-extensible through the settings UI -- a dictation
/// vocabulary is personal, and no built-in list survives contact with a real
/// user's jargon.
const ABBREVIATIONS: [&str; 28] = [
    // German
    "ca.", "bzw.", "vgl.", "usw.", "evtl.", "ggf.", "inkl.", "exkl.", "zzgl.", "bspw.",
    "bzgl.", "nr.", "abb.", "tab.", "str.", "tel.", "sog.", "geb.", "mio.", "mrd.",
    "hrsg.", "jhd.",
    // English
    "mr.", "mrs.", "dr.", "prof.", "vs.", "etc.",
];

pub fn finish(s: &str) -> String {
    let trimmed = s.trim_end();
    if trimmed.trim_start().is_empty() {
        return String::new();
    }
    let mut out = capitalise_sentence_starts(trimmed);
    if !is_terminated(&out) {
        out.push('.');
    }
    out
}

/// Uppercases the first letter of every sentence: the opening one, and each
/// one following a terminator that is genuinely a sentence break.
///
/// Looks past leading brackets and quotes but stops at a digit -- an
/// utterance that opens with a number opens with that number, and
/// capitalising the word after it (`42 Things happened`) is worse than
/// leaving it alone.
///
/// Char-indexed rather than byte-indexed throughout, because the opening
/// letter of German dictation is routinely multi-byte and its uppercase form
/// need not be the same width (`\u{fc}` -> `\u{dc}`), or even a single char.
fn capitalise_sentence_starts(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut at_start = true;

    for i in 0..chars.len() {
        let c = chars[i];
        if at_start {
            if c.is_alphabetic() {
                out.extend(c.to_uppercase());
                at_start = false;
                continue;
            }
            if c.is_alphanumeric() {
                at_start = false;
            }
        }
        out.push(c);
        // A terminator only opens a new sentence when whitespace follows it:
        // the period inside `z.B.` is followed by a letter, so it never gets
        // this far, and one at the very end of the utterance has no next
        // sentence to capitalise.
        if TERMINATORS.contains(&c)
            && chars.get(i + 1).is_some_and(|n| n.is_whitespace())
            && !is_abbreviation(&token_ending_at(&chars, i))
        {
            at_start = true;
        }
    }
    out
}

/// The whitespace-delimited token ending at `i`, terminator included.
fn token_ending_at(chars: &[char], i: usize) -> String {
    let start = chars[..i].iter().rposition(|c| c.is_whitespace()).map_or(0, |p| p + 1);
    chars[start..=i].iter().collect()
}

/// Whether a terminator closes an abbreviation rather than a sentence.
///
/// Three structural rules do most of the work, so [`ABBREVIATIONS`] only has
/// to carry what they miss -- and `z.B.`, `d.h.`, `u.a.`, `u.s.w.`, `e.g.`
/// and `i.e.` are deliberately absent from it for exactly that reason. A list
/// that has to be complete is a list that is always incomplete.
fn is_abbreviation(token: &str) -> bool {
    let lower = token.to_lowercase();
    if ABBREVIATIONS.contains(&lower.as_str()) {
        return true;
    }
    let stem = lower.strip_suffix('.').unwrap_or(lower.as_str());
    if stem.is_empty() {
        return false;
    }
    // A period *inside* the token: `z.b.`, `d.h.`, `u.s.w.`, `e.g.`.
    stem.contains('.')
        // A German ordinal: `am 5. Mai`, `im 20. Jahrhundert`.
        || stem.chars().all(|c| c.is_ascii_digit())
        // An initial: `J. Soppa`.
        || (stem.chars().count() == 1 && stem.chars().all(char::is_alphabetic))
}

/// Whether the text already ends on a sentence terminator, ignoring any
/// closing quotes or brackets that follow it.
fn is_terminated(s: &str) -> bool {
    s.trim_end_matches(&CLOSERS[..])
        .chars()
        .next_back()
        .is_some_and(|c| TERMINATORS.contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalises_the_opening_letter() {
        assert_eq!(finish("hallo welt."), "Hallo welt.");
    }

    #[test]
    fn leaves_an_already_capitalised_opening_letter_alone() {
        assert_eq!(finish("Hallo welt."), "Hallo welt.");
    }

    #[test]
    fn capitalises_past_leading_punctuation() {
        assert_eq!(finish("\"hallo welt.\""), "\"Hallo welt.\"");
    }

    /// German dictation capitalises nouns, so a wrongly-lowercased opening
    /// letter has to survive a multi-byte neighbour: `\u{00e4}` is two bytes,
    /// and `\u{00fc}` uppercases to a two-byte `\u{00dc}`. A byte-indexed
    /// implementation passes the ASCII tests above and corrupts this one.
    #[test]
    fn capitalises_a_multi_byte_opening_letter() {
        assert_eq!(finish("\u{fc}ber morgen."), "\u{dc}ber morgen.");
        assert_eq!(finish("au\u{df}erdem m\u{f6}chte ich."), "Au\u{df}erdem m\u{f6}chte ich.");
    }

    /// The one behaviour deliberately *dropped* from the old
    /// `rule_based_fallback`, which searched for the first *alphabetic*
    /// character and so turned `42 things happened` into
    /// `42 Things happened.` -- capitalising a word in mid-sentence.
    /// Leading brackets and quotes must still be skipped (above); a leading
    /// digit means the sentence genuinely starts with a number.
    #[test]
    fn does_not_capitalise_when_the_utterance_opens_with_a_digit() {
        assert_eq!(finish("42 things happened."), "42 things happened.");
    }

    #[test]
    fn appends_a_full_stop_to_an_unterminated_utterance() {
        assert_eq!(finish("Hallo welt"), "Hallo welt.");
    }

    #[test]
    fn leaves_every_terminator_alone() {
        assert_eq!(finish("Schon fertig."), "Schon fertig.");
        assert_eq!(finish("Wirklich?"), "Wirklich?");
        assert_eq!(finish("Hey!"), "Hey!");
        assert_eq!(finish("Und dann\u{2026}"), "Und dann\u{2026}");
    }

    #[test]
    fn treats_a_terminator_before_a_closing_quote_as_terminated() {
        assert_eq!(finish("Er sagte: \"nein.\""), "Er sagte: \"nein.\"");
        assert_eq!(finish("Ein Satz (so wie dieser.)"), "Ein Satz (so wie dieser.)");
    }

    #[test]
    fn drops_trailing_whitespace_before_terminating() {
        assert_eq!(finish("Hallo welt "), "Hallo welt.");
    }

    #[test]
    fn an_empty_or_blank_utterance_stays_empty() {
        assert_eq!(finish(""), "");
        assert_eq!(finish("   "), "");
    }

    /// The property that makes it safe to apply this at one choke point that
    /// every path flows through, including the paths `rule_based_fallback`
    /// has already finished.
    #[test]
    fn is_idempotent() {
        for s in [
            "hallo welt",
            "Schon fertig.",
            "\"hallo\"",
            "42 dinge",
            "",
            "Wirklich?",
            "werden. ich fix das auch gleich mit",
            "Das ist z.B. das Problem.",
        ] {
            let once = finish(s);
            assert_eq!(finish(&once), once, "not idempotent for {s:?}");
        }
    }

    /// Documented, not fixed: an utterance that ends on a comma gains a
    /// full stop after it rather than having the comma replaced. Replacing
    /// it would be guessing at intent -- a dictated `"und zwar,"` may well
    /// be continued by the next utterance. Pinned so the behaviour is a
    /// decision rather than an accident.
    #[test]
    fn known_limitation_a_trailing_comma_gains_a_full_stop() {
        assert_eq!(finish("Also dann,"), "Also dann,.");
    }

    #[test]
    fn capitalises_a_sentence_start_inside_the_utterance() {
        assert_eq!(
            finish("werden. ich fix das auch gleich mit."),
            "Werden. Ich fix das auch gleich mit."
        );
    }

    #[test]
    fn capitalises_after_every_terminator_not_just_the_full_stop() {
        assert_eq!(finish("Wirklich? ja! also gut."), "Wirklich? Ja! Also gut.");
        assert_eq!(finish("Und dann\u{2026} kam er."), "Und dann\u{2026} Kam er.");
    }

    /// `z.B.` needs no list entry: a token carrying a period *inside* it is
    /// an abbreviation by construction, which covers `d.h.`, `u.a.`,
    /// `u.s.w.`, `e.g.` and `i.e.` too without anyone maintaining them.
    #[test]
    fn does_not_capitalise_after_an_abbreviation_with_internal_periods() {
        assert_eq!(finish("Das ist z.B. das Problem."), "Das ist z.B. das Problem.");
        assert_eq!(finish("Das hei\u{df}t d.h. genau das."), "Das hei\u{df}t d.h. genau das.");
    }

    #[test]
    fn does_not_capitalise_after_a_listed_abbreviation() {
        assert_eq!(finish("Das kostet ca. drei Euro."), "Das kostet ca. drei Euro.");
        assert_eq!(finish("Siehe Nr. sieben unten."), "Siehe Nr. sieben unten.");
    }

    /// German ordinals (`am 5. Mai`, `im 20. Jahrhundert`) put a period
    /// after a bare number mid-sentence.
    #[test]
    fn does_not_capitalise_after_an_ordinal_number() {
        assert_eq!(finish("Am 5. und 6. Mai."), "Am 5. und 6. Mai.");
    }

    #[test]
    fn does_not_capitalise_after_an_initial() {
        assert_eq!(finish("Von J. Soppa geschrieben."), "Von J. Soppa geschrieben.");
    }

    #[test]
    fn capitalises_past_an_opening_quote_at_a_sentence_start() {
        assert_eq!(finish("Erst das. \"dann\" das."), "Erst das. \"Dann\" das.");
    }

    #[test]
    fn does_not_capitalise_a_digit_at_a_sentence_start() {
        assert_eq!(finish("Erst das. 42 dinge kamen."), "Erst das. 42 dinge kamen.");
    }

    /// Documented, not fixed: a dictated domain or filename carries internal
    /// periods, so the rule that makes `z.B.` work also swallows the sentence
    /// break after it. Rarer than the abbreviation case it buys, and the
    /// alternative -- capitalising after every `.` -- is the worse trade.
    #[test]
    fn known_limitation_a_domain_before_a_sentence_break_reads_as_an_abbreviation() {
        assert_eq!(
            finish("Schreib an joni.zor.de. dann melde dich."),
            "Schreib an joni.zor.de. dann melde dich."
        );
    }
}
