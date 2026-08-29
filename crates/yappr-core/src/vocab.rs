//! The dictation vocabulary: names, jargon and acronyms the ASR has never
//! seen, corrected on the transcript before anything else reads it.
//!
//! Applied immediately after ASR rather than at the end of the pipeline, so
//! that S1-mini and the guardrail both see the corrected text -- the
//! normalizer produces better output when the terms in front of it are real
//! words, and the guardrail's overlap check compares like with like.
//!
//! Two mechanisms, because one does not cover the other's cases. See
//! [`crate::config::VocabularyConfig`].

use crate::config::VocabularyConfig;

/// One correction that actually fired, for the debug record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Substitution {
    pub from: String,
    pub to: String,
    /// `false` for an exact `[[vocabulary.replacements]]` entry.
    pub fuzzy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub text: String,
    pub substitutions: Vec<Substitution>,
}

/// Corrects `text` against the configured vocabulary.
///
/// Exact replacements run first and their output is what fuzzy matching then
/// sees, so a replacement is the way to override a term that would otherwise
/// be matched loosely.
pub fn apply(cfg: &VocabularyConfig, text: &str) -> Applied {
    let mut substitutions = Vec::new();
    if !cfg.enabled {
        return Applied { text: text.to_string(), substitutions };
    }
    let replaced = apply_replacements(cfg, text, &mut substitutions);
    let text = apply_terms(cfg, &replaced, &mut substitutions);
    Applied { text, substitutions }
}

fn apply_replacements(
    cfg: &VocabularyConfig,
    text: &str,
    substitutions: &mut Vec<Substitution>,
) -> String {
    let mut out = text.to_string();
    for r in &cfg.replacements {
        let pattern = bounded_pattern(&r.from);
        let re = match regex::Regex::new(&pattern) {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!(from = %r.from, error = %e, "skipping unbuildable replacement");
                continue;
            }
        };
        let mut hits = Vec::new();
        let replaced = re
            .replace_all(&out, |c: &regex::Captures| {
                hits.push(c[0].to_string());
                r.to.clone()
            })
            .into_owned();
        for hit in hits {
            substitutions.push(Substitution { from: hit, to: r.to.clone(), fuzzy: false });
        }
        out = replaced;
    }
    out
}

/// A literal, case-insensitive pattern for `from`, word-bounded only on the
/// sides where a boundary is meaningful.
///
/// The escaping is what keeps a replacement a replacement: without it, a
/// `from` of `c++` is a quantifier error at best and a pattern matching
/// something else entirely at worst. The conditional `\b` matters for the same
/// class of input -- `\bc\+\+\b` would never match `c++ ` at all, because
/// there is no word boundary after a `+` followed by a space.
fn bounded_pattern(from: &str) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let lead = if from.chars().next().is_some_and(word) { r"\b" } else { "" };
    let trail = if from.chars().next_back().is_some_and(word) { r"\b" } else { "" };
    format!("(?i){lead}{}{trail}", regex::escape(from))
}

fn apply_terms(
    cfg: &VocabularyConfig,
    text: &str,
    substitutions: &mut Vec<Substitution>,
) -> String {
    if cfg.max_error_ratio <= 0.0 || cfg.terms.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for piece in whitespace_runs(text) {
        let Some((lead, core, trail)) = split_off_punctuation(piece) else {
            out.push_str(piece);
            continue;
        };
        match best_term(cfg, core) {
            Some(term) => {
                substitutions.push(Substitution {
                    from: core.to_string(),
                    to: term.clone(),
                    fuzzy: true,
                });
                out.push_str(lead);
                out.push_str(term);
                out.push_str(trail);
            }
            None => out.push_str(piece),
        }
    }
    out
}

/// The closest term within its error budget, or `None` when the token is
/// already correct, is too far from everything, or matches nothing.
fn best_term<'a>(cfg: &'a VocabularyConfig, core: &str) -> Option<&'a String> {
    let core_lower = core.to_lowercase();
    let mut best: Option<(usize, &String)> = None;

    for term in &cfg.terms {
        let len = term.chars().count();
        if len < cfg.min_term_chars {
            continue;
        }
        let budget = (len as f64 * cfg.max_error_ratio).floor() as usize;
        if budget == 0 {
            continue;
        }
        let distance = strsim::levenshtein(&core_lower, &term.to_lowercase());
        // The ASR already got this one right. Not a correction, and reporting
        // it as one would bury the corrections that matter.
        if distance == 0 {
            return None;
        }
        if distance > budget {
            continue;
        }
        let better = best.is_none_or(|(best_distance, best_term)| {
            distance < best_distance
                || (distance == best_distance && len > best_term.chars().count())
        });
        if better {
            best = Some((distance, term));
        }
    }
    best.map(|(_, term)| term)
}

/// Splits `s` into alternating runs of whitespace and non-whitespace, so the
/// original spacing survives a substitution byte for byte.
fn whitespace_runs(s: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut previous: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let whitespace = c.is_whitespace();
        if previous.is_some_and(|p| p != whitespace) {
            runs.push(&s[start..i]);
            start = i;
        }
        previous = Some(whitespace);
    }
    if start < s.len() {
        runs.push(&s[start..]);
    }
    runs
}

/// Peels the surrounding punctuation off a token, so `(sanity.venture).`
/// matches on `sanity.venture` and is rebuilt with its brackets intact.
/// `None` for a token holding no alphanumeric character at all.
fn split_off_punctuation(piece: &str) -> Option<(&str, &str, &str)> {
    let start = piece.find(char::is_alphanumeric)?;
    let (i, c) = piece.char_indices().rfind(|(_, c)| c.is_alphanumeric())?;
    let end = i + c.len_utf8();
    Some((&piece[..start], &piece[start..end], &piece[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Replacement;

    fn vocab(terms: &[&str], replacements: &[(&str, &str)]) -> VocabularyConfig {
        VocabularyConfig {
            terms: terms.iter().map(|s| s.to_string()).collect(),
            replacements: replacements
                .iter()
                .map(|(f, t)| Replacement { from: f.to_string(), to: t.to_string() })
                .collect(),
            ..VocabularyConfig::default()
        }
    }

    /// The real misrecognition from `~/yappr/logs/20260828-094008-643.json`.
    #[test]
    fn an_exact_replacement_is_applied() {
        let out = apply(
            &vocab(&[], &[("Settings-SQUI", "Settings-GUI")]),
            "Kannst du mir eine Settings-SQUI bauen?",
        );
        assert_eq!(out.text, "Kannst du mir eine Settings-GUI bauen?");
    }

    #[test]
    fn a_replacement_matches_regardless_of_case() {
        let out = apply(&vocab(&[], &[("squi", "GUI")]), "Eine SQUI bitte.");
        assert_eq!(out.text, "Eine GUI bitte.");
    }

    /// Without a word boundary, replacing `GUI` would corrupt `GUIDE`, and a
    /// vocabulary that rewrites the middle of unrelated words is worse than
    /// no vocabulary at all.
    #[test]
    fn a_replacement_does_not_fire_inside_a_longer_word() {
        let out = apply(&vocab(&[], &[("GUI", "gooey")]), "Lies den GUIDE.");
        assert_eq!(out.text, "Lies den GUIDE.");
    }

    #[test]
    fn a_replacement_source_containing_regex_metacharacters_is_taken_literally() {
        let out = apply(&vocab(&[], &[("c++", "C++")]), "Ich mag c++ sehr.");
        assert_eq!(out.text, "Ich mag C++ sehr.");
    }

    #[test]
    fn a_term_is_matched_despite_a_small_misrecognition() {
        let out = apply(&vocab(&["Hyprland"], &[]), "Ich nutze Hyperland taeglich.");
        assert_eq!(out.text, "Ich nutze Hyprland taeglich.");
    }

    #[test]
    fn a_token_too_far_from_every_term_is_left_alone() {
        let out = apply(&vocab(&["Hyprland"], &[]), "Ich nutze Windows.");
        assert_eq!(out.text, "Ich nutze Windows.");
    }

    /// Fuzzy matching is only safe above a length floor, so a short term must
    /// not silently start rewriting common words.
    #[test]
    fn a_term_below_the_minimum_length_never_matches_fuzzily() {
        let out = apply(&vocab(&["GUI"], &[]), "Das war gut.");
        assert_eq!(out.text, "Das war gut.");
    }

    #[test]
    fn punctuation_around_a_matched_token_survives() {
        let out = apply(&vocab(&["sanity.ventures"], &[]), "Schreib an (sanity.venture).");
        assert_eq!(out.text, "Schreib an (sanity.ventures).");
    }

    #[test]
    fn an_explicit_replacement_wins_over_a_fuzzy_term() {
        let out = apply(
            &vocab(&["Hyprland"], &[("Hyperland", "Sway")]),
            "Ich nutze Hyperland.",
        );
        assert_eq!(out.text, "Ich nutze Sway.");
    }

    #[test]
    fn a_disabled_vocabulary_changes_nothing() {
        let cfg = VocabularyConfig { enabled: false, ..vocab(&["Hyprland"], &[("a", "b")]) };
        let out = apply(&cfg, "Ich nutze Hyperland.");
        assert_eq!(out.text, "Ich nutze Hyperland.");
        assert!(out.substitutions.is_empty());
    }

    #[test]
    fn a_zero_error_ratio_keeps_replacements_but_stops_fuzzy_matching() {
        let cfg = VocabularyConfig {
            max_error_ratio: 0.0,
            ..vocab(&["Hyprland"], &[("taeglich", "täglich")])
        };
        let out = apply(&cfg, "Ich nutze Hyperland taeglich.");
        assert_eq!(out.text, "Ich nutze Hyperland täglich.");
    }

    #[test]
    fn every_applied_correction_is_reported() {
        let out = apply(
            &vocab(&["Hyprland"], &[("Settings-SQUI", "Settings-GUI")]),
            "Die Settings-SQUI unter Hyperland.",
        );
        assert_eq!(
            out.substitutions,
            vec![
                Substitution {
                    from: "Settings-SQUI".into(),
                    to: "Settings-GUI".into(),
                    fuzzy: false
                },
                Substitution { from: "Hyperland".into(), to: "Hyprland".into(), fuzzy: true },
            ]
        );
    }

    /// A term the ASR got right is not a correction, and reporting it as one
    /// would bury the corrections that matter in the debug record.
    #[test]
    fn a_term_transcribed_correctly_is_not_reported_as_a_correction() {
        let out = apply(&vocab(&["Hyprland"], &[]), "Ich nutze Hyprland.");
        assert_eq!(out.text, "Ich nutze Hyprland.");
        assert!(out.substitutions.is_empty());
    }
}
