use crate::config::{Config, StyleAxes};

/// Builds the control line S1-mini is steered by.
///
/// The format is fixed by the model card: three bracketed axes on one line,
/// immediately above the raw transcript. See spec 8.3.
pub fn control_line(axes: &StyleAxes) -> String {
    format!(
        "[Styling: {}] [Structure: {}] [Context: {}]",
        axes.styling, axes.structure, axes.context
    )
}

/// Resolves style axes for the window that had focus when recording started.
///
/// Rules are evaluated in order and the first whose `match_class` regex is
/// found anywhere in the class wins. Axes the winning rule leaves unset
/// inherit from `[style_default]`.
///
/// Regexes are validated at config load (see `Config::validate`), so a compile
/// failure here means the config was constructed without validation; such a
/// rule is skipped rather than panicking.
pub fn resolve(cfg: &Config, window_class: Option<&str>) -> StyleAxes {
    let mut axes = cfg.style_default;
    let Some(class) = window_class else {
        return axes;
    };

    for rule in &cfg.style_rules {
        let Ok(re) = regex::Regex::new(&rule.match_class) else {
            tracing::warn!(pattern = %rule.match_class, "skipping unparseable style rule");
            continue;
        };
        if re.is_match(class) {
            if let Some(v) = rule.styling { axes.styling = v; }
            if let Some(v) = rule.structure { axes.structure = v; }
            if let Some(v) = rule.context { axes.context = v; }
            return axes;
        }
    }
    axes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Context, StyleAxes, Structure, Styling};

    #[test]
    fn control_line_covers_the_full_axis_matrix() {
        let stylings = [
            (Styling::Casual, "casual"),
            (Styling::SemiCasual, "semi-casual"),
            (Styling::SemiFormal, "semi-formal"),
            (Styling::Formal, "formal"),
        ];
        let structures = [(Structure::Prose, "prose"), (Structure::Lists, "lists")];
        let contexts = [(Context::General, "general"), (Context::Email, "email")];

        let mut seen = 0;
        for (sty, sty_s) in stylings {
            for (str_, str_s) in structures {
                for (ctx, ctx_s) in contexts {
                    let axes = StyleAxes { styling: sty, structure: str_, context: ctx };
                    assert_eq!(
                        control_line(&axes),
                        format!("[Styling: {sty_s}] [Structure: {str_s}] [Context: {ctx_s}]")
                    );
                    seen += 1;
                }
            }
        }
        assert_eq!(seen, 16, "the matrix is 4 x 2 x 2");
    }

    fn cfg(toml: &str) -> Config {
        Config::from_str(toml).unwrap()
    }

    #[test]
    fn no_window_class_yields_the_default_axes() {
        let c = cfg("");
        assert_eq!(resolve(&c, None), StyleAxes::default());
    }

    #[test]
    fn a_non_matching_class_yields_the_default_axes() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
        "#);
        assert_eq!(resolve(&c, Some("Alacritty")), StyleAxes::default());
    }

    #[test]
    fn a_matching_rule_overrides_only_the_axes_it_sets() {
        let c = cfg(r#"
            [style_default]
            styling = "casual"

            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
        "#);
        let got = resolve(&c, Some("thunderbird"));
        assert_eq!(got.context, Context::Email, "rule sets context");
        assert_eq!(got.styling, Styling::Casual, "unset axes inherit the default");
        assert_eq!(got.structure, Structure::Prose);
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)^slack$"
            styling = "casual"

            [[style_rules]]
            match_class = "(?i)slack"
            styling = "formal"
        "#);
        assert_eq!(resolve(&c, Some("Slack")).styling, Styling::Casual);
    }

    #[test]
    fn matching_is_a_search_not_a_full_match() {
        let c = cfg(r#"
            [[style_rules]]
            match_class = "(?i)mail"
            context = "email"
        "#);
        assert_eq!(resolve(&c, Some("org.gnome.Geary.Mail")).context, Context::Email);
    }
}
