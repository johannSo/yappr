//! Lints a dictation finetuning dataset against the *real* pipeline rules.
//!
//! Not part of the app: a development tool for building training data for a
//! German S1-mini finetune. A training target that the guardrail would reject
//! at runtime teaches the model to produce text the pipeline then throws
//! away, so every pair is run through `guardrail::evaluate` itself rather
//! than a reimplementation of it.
//!
//! Input is JSONL, one object per line:
//! `{"styling":"casual","structure":"prose","context":"general",
//!   "raw":"...","cleaned":"..."}`
//!
//!   cargo run -p yappr-core --example validate_dataset -- data.jsonl

use yappr_core::config::{Context, GuardrailConfig, StyleAxes, Structure, Styling};
use yappr_core::guardrail::{self, Verdict};
use yappr_core::lang::{Lang, LanguageDetector, WhatlangDetector};
use yappr_core::normalize::max_tokens_for;
use yappr_core::{finish, style};

/// Same estimate `normalize::max_tokens_for` uses to size the reply budget.
const CHARS_PER_TOKEN: f64 = 3.5;

fn styling(s: &str) -> Option<Styling> {
    Some(match s {
        "casual" => Styling::Casual,
        "semi-casual" => Styling::SemiCasual,
        "semi-formal" => Styling::SemiFormal,
        "formal" => Styling::Formal,
        _ => return None,
    })
}

fn structure(s: &str) -> Option<Structure> {
    Some(match s {
        "prose" => Structure::Prose,
        "lists" => Structure::Lists,
        _ => return None,
    })
}

fn context(s: &str) -> Option<Context> {
    Some(match s {
        "general" => Context::General,
        "email" => Context::Email,
        _ => return None,
    })
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: validate_dataset <dataset.jsonl>");
            std::process::exit(2);
        }
    };
    let text = std::fs::read_to_string(&path).expect("reading dataset");
    let cfg = GuardrailConfig::default();
    let detector = WhatlangDetector;

    let mut seen = std::collections::HashSet::new();
    let mut combos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let (mut ok, mut bad) = (0usize, 0usize);

    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                println!("L{n}: BAD JSON: {e}");
                bad += 1;
                continue;
            }
        };
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let (raw, cleaned) = (get("raw"), get("cleaned"));

        let axes = match (styling(&get("styling")), structure(&get("structure")), context(&get("context"))) {
            (Some(a), Some(b), Some(c)) => StyleAxes { styling: a, structure: b, context: c },
            _ => {
                println!("L{n}: BAD AXES: {:?}/{:?}/{:?}", get("styling"), get("structure"), get("context"));
                bad += 1;
                continue;
            }
        };
        let control = style::control_line(&axes);
        *combos.entry(control.clone()).or_insert(0) += 1;

        let mut errs: Vec<String> = Vec::new();

        if !seen.insert(raw.clone()) {
            errs.push("duplicate raw".into());
        }

        // S1-mini's card calls an empty reply valid for filler-only input.
        // In yappr it is not reachable: `evaluate` rejects `Empty` before
        // any other check, unconditionally, so such a target would always be
        // thrown away and the raw transcript injected instead (invariant 1).
        // Training it teaches a behaviour the pipeline discards.
        if cleaned.trim().is_empty() {
            errs.push("empty target: the guardrail rejects Empty unconditionally".into());
        } else {
            // 1. The guardrail that will judge this at runtime.
            let lang = detector.detect(&raw);
            match guardrail::evaluate(&raw, &cleaned, lang, &cfg) {
                Verdict::Accept => {}
                Verdict::Reject(r) => errs.push(format!("GUARDRAIL {:?}", r)),
            }
            // 2. German must classify as Other, or the looser English overlap
            //    floor was used and the row is not testing what it claims.
            if lang != Lang::Other {
                errs.push("raw not detected as non-English (too short, or actually English)".into());
            }
            // 3. `finish` runs after the model on every path. A target it
            //    changes is a target the model is being taught wrong.
            let finished = finish::finish(&cleaned);
            if finished != cleaned {
                errs.push(format!("not in finished form; finish() -> {finished:?}"));
            }
            // 4. The reply budget is derived from the *raw* length. A target
            //    longer than that budget is unreachable at inference.
            let budget = max_tokens_for(&raw);
            let need = (cleaned.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as u32;
            if need > budget {
                errs.push(format!("target needs ~{need} tokens, budget is {budget}"));
            }
        }

        let rt = guardrail::tokenize(&raw);
        let ct = guardrail::tokenize(&cleaned);
        let ov = guardrail::overlap(&rt, &ct);
        let wr = if rt.is_empty() { 1.0 } else { ct.len() as f64 / rt.len() as f64 };

        if errs.is_empty() {
            ok += 1;
        } else {
            bad += 1;
            println!("L{n}: overlap={ov:.2} ratio={wr:.2}");
            for e in errs {
                println!("      - {e}");
            }
            println!("      raw:     {raw}");
            println!("      cleaned: {cleaned}");
        }
    }

    println!("\n=== {ok} ok, {bad} bad ===");
    println!("control-line coverage ({} of 16 combinations):", combos.len());
    let mut keys: Vec<_> = combos.iter().collect();
    keys.sort();
    for (k, c) in keys {
        println!("  {c:4}  {k}");
    }
    if bad > 0 {
        std::process::exit(1);
    }
}
