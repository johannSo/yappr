use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Styling {
    Casual,
    SemiCasual,
    SemiFormal,
    Formal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Structure {
    Prose,
    Lists,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Context {
    General,
    Email,
}

impl fmt::Display for Styling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Styling::Casual => "casual",
            Styling::SemiCasual => "semi-casual",
            Styling::SemiFormal => "semi-formal",
            Styling::Formal => "formal",
        };
        f.write_str(s)
    }
}

impl fmt::Display for Structure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Structure::Prose => "prose",
            Structure::Lists => "lists",
        })
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Context::General => "general",
            Context::Email => "email",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleAxes {
    #[serde(default = "default_styling")]
    pub styling: Styling,
    #[serde(default = "default_structure")]
    pub structure: Structure,
    #[serde(default = "default_context")]
    pub context: Context,
}

fn default_styling() -> Styling {
    Styling::SemiCasual
}
fn default_structure() -> Structure {
    Structure::Prose
}
fn default_context() -> Context {
    Context::General
}

impl Default for StyleAxes {
    fn default() -> Self {
        Self {
            styling: default_styling(),
            structure: default_structure(),
            context: default_context(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleRule {
    pub match_class: String,
    #[serde(default)]
    pub styling: Option<Styling>,
    #[serde(default)]
    pub structure: Option<Structure>,
    #[serde(default)]
    pub context: Option<Context>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    #[serde(default = "d_device")]
    pub device: String,
    #[serde(default = "d_max_seconds")]
    pub max_seconds: u32,
    #[serde(default = "d_vad_padding")]
    pub vad_padding_ms: u32,
}

fn d_device() -> String {
    "default".into()
}
fn d_max_seconds() -> u32 {
    120
}
fn d_vad_padding() -> u32 {
    200
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: d_device(),
            max_seconds: d_max_seconds(),
            vad_padding_ms: d_vad_padding(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsrConfig {
    #[serde(default = "d_threads")]
    pub num_threads: i32,
}

fn d_threads() -> i32 {
    4
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self { num_threads: d_threads() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_port")]
    pub port: u16,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "d_llama_path")]
    pub llama_server_path: String,
    #[serde(default = "d_ctx")]
    pub context_size: u32,
    #[serde(default = "d_threads_u32")]
    pub threads: u32,
}

fn d_true() -> bool {
    true
}
fn d_port() -> u16 {
    8730
}
fn d_timeout() -> u64 {
    6000
}
fn d_llama_path() -> String {
    "llama-server".into()
}
fn d_ctx() -> u32 {
    2048
}
fn d_threads_u32() -> u32 {
    4
}

impl Default for NormalizeConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(),
            port: d_port(),
            timeout_ms: d_timeout(),
            llama_server_path: d_llama_path(),
            context_size: d_ctx(),
            threads: d_threads_u32(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardrailConfig {
    #[serde(default = "d_min_wr")]
    pub min_word_ratio: f64,
    #[serde(default = "d_max_wr")]
    pub max_word_ratio: f64,
    #[serde(default = "d_ov_en")]
    pub min_overlap_english: f64,
    #[serde(default = "d_ov_other")]
    pub min_overlap_other: f64,
    #[serde(default = "d_short")]
    pub short_input_words: usize,
    #[serde(default = "d_ngram")]
    pub ngram_size: usize,
    #[serde(default = "d_ngram_rep")]
    pub ngram_max_repeats: usize,
}

fn d_min_wr() -> f64 {
    0.55
}
fn d_max_wr() -> f64 {
    1.80
}
fn d_ov_en() -> f64 {
    0.55
}
fn d_ov_other() -> f64 {
    0.70
}
fn d_short() -> usize {
    4
}
fn d_ngram() -> usize {
    6
}
fn d_ngram_rep() -> usize {
    3
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            min_word_ratio: d_min_wr(),
            max_word_ratio: d_max_wr(),
            min_overlap_english: d_ov_en(),
            min_overlap_other: d_ov_other(),
            short_input_words: d_short(),
            ngram_size: d_ngram(),
            ngram_max_repeats: d_ngram_rep(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjectBackend {
    Wtype,
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectConfig {
    #[serde(default = "d_backend")]
    pub backend: InjectBackend,
    #[serde(default = "d_true")]
    pub trailing_space: bool,
    #[serde(default = "d_keydelay")]
    pub keystroke_delay_ms: u32,
}

fn d_backend() -> InjectBackend {
    InjectBackend::Wtype
}
fn d_keydelay() -> u32 {
    2
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            backend: d_backend(),
            trailing_space: true,
            keystroke_delay_ms: d_keydelay(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugConfig {
    #[serde(default = "d_debug_enabled")]
    pub enabled: bool,
    #[serde(default = "d_debug_dir")]
    pub dir: String,
    #[serde(default = "d_true")]
    pub save_audio: bool,
}

fn d_debug_enabled() -> bool {
    false
}
fn d_debug_dir() -> String {
    "~/owf".into()
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self { enabled: d_debug_enabled(), dir: d_debug_dir(), save_audio: d_true() }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub asr: AsrConfig,
    #[serde(default)]
    pub normalize: NormalizeConfig,
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    #[serde(default)]
    pub inject: InjectConfig,
    #[serde(default)]
    pub style_default: StyleAxes,
    #[serde(default)]
    pub style_rules: Vec<StyleRule>,
    #[serde(default)]
    pub debug: DebugConfig,
}

impl Config {
    // Intentionally an inherent method, not `std::str::FromStr`: it needs to
    // return `anyhow::Result`, not an associated `Err` type, and it's always
    // called as `Config::from_str(..)` rather than through the trait.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self> {
        // `anyhow::Error`'s `Display` only shows the top-level context, not the
        // wrapped source, so fold the underlying toml error text (which names the
        // offending key/value) into the message itself rather than attaching it
        // as a `.context()` layer that `to_string()` would hide.
        let c: Config = toml::from_str(s)
            .map_err(|e| anyhow::anyhow!("parsing config.toml: {e}"))?;
        c.validate()?;
        Ok(c)
    }

    /// Loads the config, writing a commented default file if none exists.
    pub fn load() -> Result<Self> {
        let p = paths::config_file();
        if !p.exists() {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, DEFAULT_CONFIG_TOML)?;
        }
        let s = std::fs::read_to_string(&p)
            .with_context(|| format!("reading {}", p.display()))?;
        Self::from_str(&s)
    }

    fn validate(&self) -> Result<()> {
        if self.audio.max_seconds == 0 {
            bail!("audio.max_seconds must be greater than 0");
        }
        if self.asr.num_threads < 1 {
            bail!("asr.num_threads must be at least 1");
        }
        if self.normalize.timeout_ms == 0 {
            bail!("normalize.timeout_ms must be greater than 0");
        }
        if self.normalize.context_size < 512 {
            bail!("normalize.context_size must be at least 512");
        }
        if self.normalize.port == 0 {
            bail!("normalize.port must be greater than 0");
        }
        if self.normalize.threads == 0 {
            bail!("normalize.threads must be at least 1");
        }
        for (name, v) in [
            ("guardrail.min_overlap_english", self.guardrail.min_overlap_english),
            ("guardrail.min_overlap_other", self.guardrail.min_overlap_other),
        ] {
            if !(0.0..=1.0).contains(&v) {
                bail!("{name} must be between 0.0 and 1.0, got {v}");
            }
        }
        // Only the ordering (min < max) used to be checked here, so
        // `min_word_ratio = -0.5` validated cleanly and silently disabled the
        // floor entirely (every ratio is >= any negative number). Bounding
        // each to a sane range independently catches that, and a nonsense
        // `max_word_ratio` (e.g. from a typo like `18.0` for `1.80`), before
        // the ordering check ever runs.
        if !(0.0..=1.0).contains(&self.guardrail.min_word_ratio) {
            bail!(
                "guardrail.min_word_ratio must be between 0.0 and 1.0, got {}",
                self.guardrail.min_word_ratio
            );
        }
        if !(0.0..=10.0).contains(&self.guardrail.max_word_ratio) {
            bail!(
                "guardrail.max_word_ratio must be between 0.0 and 10.0, got {}",
                self.guardrail.max_word_ratio
            );
        }
        if self.guardrail.min_word_ratio >= self.guardrail.max_word_ratio {
            bail!(
                "guardrail.min_word_ratio ({}) must be less than max_word_ratio ({})",
                self.guardrail.min_word_ratio,
                self.guardrail.max_word_ratio
            );
        }
        if self.guardrail.ngram_size < 2 {
            bail!("guardrail.ngram_size must be at least 2");
        }
        if self.guardrail.ngram_max_repeats < 2 {
            bail!("guardrail.ngram_max_repeats must be at least 2");
        }
        for rule in &self.style_rules {
            regex::Regex::new(&rule.match_class).with_context(|| {
                format!("invalid regex in style_rules match_class: {}", rule.match_class)
            })?;
        }
        if self.debug.dir.trim().is_empty() {
            bail!("debug.dir must not be empty");
        }
        Ok(())
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# OpenWhisprFlow configuration

[audio]
device = "default"
max_seconds = 120
vad_padding_ms = 200

[asr]
num_threads = 4

[normalize]
# Cleanup runs on S1-mini by Superwhisper.
enabled = true
port = 8730
timeout_ms = 6000
llama_server_path = "llama-server"
context_size = 2048
threads = 4

[guardrail]
min_word_ratio = 0.55
max_word_ratio = 1.80
min_overlap_english = 0.55
min_overlap_other = 0.70
short_input_words = 4
ngram_size = 6
ngram_max_repeats = 3

[inject]
backend = "wtype"
trailing_space = true
keystroke_delay_ms = 2

[style_default]
styling = "semi-casual"    # casual | semi-casual | semi-formal | formal
structure = "prose"        # prose | lists
context = "general"        # general | email

# First matching rule wins; unset axes inherit from [style_default].
# [[style_rules]]
# match_class = "(?i)thunderbird|^Mail$"
# styling = "semi-formal"
# context = "email"

[debug]
# Diagnostics for tracking down capture/VAD/normalization bugs: per-utterance
# WAV dumps and a JSON record under `dir`, plus the daemon's tracing output
# mirrored to `<dir>/logs/daemon.log`. Off by default -- nothing here is on
# the critical path when disabled.
enabled = false
dir = "~/owf"
save_audio = true    # only meaningful when enabled = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_documented_defaults() {
        let c = Config::from_str("").unwrap();
        assert_eq!(c.audio.max_seconds, 120);
        assert_eq!(c.audio.vad_padding_ms, 200);
        assert_eq!(c.asr.num_threads, 4);
        assert!(c.normalize.enabled);
        assert_eq!(c.normalize.port, 8730);
        assert_eq!(c.normalize.timeout_ms, 6000);
        assert_eq!(c.normalize.context_size, 2048);
        assert_eq!(c.guardrail.min_word_ratio, 0.55);
        assert_eq!(c.guardrail.max_word_ratio, 1.80);
        assert_eq!(c.guardrail.min_overlap_english, 0.55);
        assert_eq!(c.guardrail.min_overlap_other, 0.70);
        assert_eq!(c.guardrail.short_input_words, 4);
        assert_eq!(c.guardrail.ngram_size, 6);
        assert_eq!(c.guardrail.ngram_max_repeats, 3);
        assert!(c.inject.trailing_space);
        assert_eq!(c.style_default.styling, Styling::SemiCasual);
        assert_eq!(c.style_default.structure, Structure::Prose);
        assert_eq!(c.style_default.context, Context::General);
        assert!(!c.debug.enabled);
        assert_eq!(c.debug.dir, "~/owf");
        assert!(c.debug.save_audio);
    }

    #[test]
    fn unknown_debug_key_is_a_load_error() {
        let err = Config::from_str("[debug]\nfoo = 1\n").unwrap_err();
        assert!(err.to_string().contains("foo"), "got: {err}");
    }

    #[test]
    fn axis_enums_deserialize_from_their_wire_spellings() {
        let c = Config::from_str(
            r#"
            [style_default]
            styling = "semi-formal"
            structure = "lists"
            context = "email"
            "#,
        )
        .unwrap();
        assert_eq!(c.style_default.styling, Styling::SemiFormal);
        assert_eq!(c.style_default.structure, Structure::Lists);
        assert_eq!(c.style_default.context, Context::Email);
    }

    #[test]
    fn invalid_axis_value_is_a_load_error() {
        let err = Config::from_str(
            r#"
            [style_default]
            styling = "shouty"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("shouty") || err.to_string().contains("styling"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn unknown_key_is_a_load_error() {
        let err = Config::from_str("[audio]\nmax_secondz = 5\n").unwrap_err();
        assert!(err.to_string().contains("max_secondz"), "got: {err}");
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for bad in [
            "[audio]\nmax_seconds = 0\n",
            "[normalize]\ntimeout_ms = 0\n",
            "[guardrail]\nmin_overlap_english = 1.5\n",
            "[guardrail]\nmin_word_ratio = 2.0\nmax_word_ratio = 1.0\n",
            // A negative floor used to validate cleanly (only the ordering
            // was checked) and silently disable the ratio floor entirely.
            "[guardrail]\nmin_word_ratio = -0.5\n",
            "[guardrail]\nmax_word_ratio = 50.0\n",
            "[asr]\nnum_threads = 0\n",
            "[normalize]\nport = 0\n",
            "[normalize]\nthreads = 0\n",
            "[debug]\ndir = \"\"\n",
            "[debug]\ndir = \"   \"\n",
        ] {
            assert!(Config::from_str(bad).is_err(), "should have rejected: {bad}");
        }
    }

    #[test]
    fn style_rules_parse_with_partial_axes() {
        let c = Config::from_str(
            r#"
            [[style_rules]]
            match_class = "(?i)thunderbird"
            context = "email"
            "#,
        )
        .unwrap();
        assert_eq!(c.style_rules.len(), 1);
        assert_eq!(c.style_rules[0].context, Some(Context::Email));
        assert_eq!(c.style_rules[0].styling, None);
    }

    #[test]
    fn default_config_toml_round_trips_to_config_default() {
        // DEFAULT_CONFIG_TOML is the file written to disk on first run and the
        // file a user actually edits. It must parse, and it must agree exactly
        // with the compiled-in defaults -- otherwise the shipped file and the
        // documented behaviour silently diverge.
        let from_file = Config::from_str(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(from_file, Config::default());
    }

    #[test]
    fn invalid_regex_in_a_style_rule_is_a_load_error() {
        let err = Config::from_str(
            r#"
            [[style_rules]]
            match_class = "([unclosed"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("regex"), "got: {err}");
    }
}
