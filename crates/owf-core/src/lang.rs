/// Coarse language classification. The pipeline only needs to know whether
/// S1-mini is operating in-domain (English) or out of it — see spec 7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    English,
    Other,
}

/// Coarse language classification for the guardrail.
///
/// Deliberately a trait: `whatlang` is chosen for its ~1 MB footprint, and
/// `lingua` restricted to the configured languages is the documented upgrade
/// if rejection logs show misclassification. See spec 7.4.
pub trait LanguageDetector: Send + Sync {
    fn detect(&self, text: &str) -> Lang;
}

/// Below this length, trigram detection is noise.
const MIN_CHARS_FOR_DETECTION: usize = 12;

/// Below this confidence, treat the guess as unusable.
const MIN_CONFIDENCE: f64 = 0.6;

#[derive(Debug, Default, Clone, Copy)]
pub struct WhatlangDetector;

impl LanguageDetector for WhatlangDetector {
    fn detect(&self, text: &str) -> Lang {
        if text.chars().count() < MIN_CHARS_FOR_DETECTION {
            return Lang::English;
        }
        match whatlang::detect(text) {
            Some(info)
                if info.lang() == whatlang::Lang::Eng && info.confidence() >= MIN_CONFIDENCE =>
            {
                Lang::English
            }
            Some(_) => Lang::Other,
            None => Lang::English,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confident_english_is_english() {
        let d = WhatlangDetector::default();
        assert_eq!(
            d.detect("the meeting is at four thirty on tuesday and i will send the notes"),
            Lang::English
        );
    }

    #[test]
    fn confident_german_is_other() {
        let d = WhatlangDetector::default();
        assert_eq!(
            d.detect("das treffen ist um halb funf am dienstag und ich schicke die notizen"),
            Lang::Other
        );
    }

    #[test]
    fn very_short_text_defaults_to_english() {
        // Trigram detection is unreliable under ~12 chars, and English is the
        // configured primary language. See spec 7.4.
        let d = WhatlangDetector::default();
        assert_eq!(d.detect("ok thanks"), Lang::English);
        assert_eq!(d.detect(""), Lang::English);
    }

    #[test]
    fn detection_is_deterministic() {
        let d = WhatlangDetector::default();
        let s = "please forward the invoice to accounting before the end of the week";
        assert_eq!(d.detect(s), d.detect(s));
    }
}
