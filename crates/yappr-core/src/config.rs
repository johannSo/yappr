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

/// How long the models stay in memory, and whether they are there before
/// anyone asks. See `docs/superpowers/specs/2026-08-29-lazy-model-lifecycle-design.md`.
///
/// The default is lazy: nothing model-shaped is loaded until the first
/// `ptt-start`, and both the ASR/VAD models and the `llama-server` child are
/// released `idle_unload_seconds` after the last dictation. Measured on the
/// development machine, that is ~1.4 GB not held while the app sits idle.
///
/// `preload_at_startup = true` with `idle_unload_seconds = 0` reproduces the
/// behaviour that predates this section: everything loaded during startup and
/// never released. That pair is the configuration to point a user at if any
/// of the lazy path misbehaves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    #[serde(default = "d_preload_at_startup")]
    pub preload_at_startup: bool,
    /// `0` disables unloading entirely; there is deliberately no lower bound
    /// above that, because a user who wants the models gone the moment a
    /// dictation ends is asking for something coherent.
    #[serde(default = "d_idle_unload_seconds")]
    pub idle_unload_seconds: u32,
}

fn d_preload_at_startup() -> bool {
    false
}
fn d_idle_unload_seconds() -> u32 {
    60
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            preload_at_startup: d_preload_at_startup(),
            idle_unload_seconds: d_idle_unload_seconds(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Accepted and ignored.
    ///
    /// S1-mini runs in this process now; there is no `llama-server` to
    /// listen on a port. The field stays because every `config.toml`
    /// written before that change has this key in it, and `[normalize]` is
    /// `deny_unknown_fields` (invariant 4) -- deleting it here would turn
    /// an existing, valid config into a hard startup failure.
    ///
    /// `skip_serializing` is what actually retires it (spec §2.1). While the
    /// writer merged into the user's own document, a key it never touched
    /// simply stayed where it was; a canonical dump would instead write this
    /// into *every* file, which is the opposite of retiring it. Now the first
    /// save drops it, and once no file on disk still names it the field can
    /// go too.
    ///
    /// Hidden in the settings GUI by `schema.ts`'s `OBSOLETE_FIELDS`, which
    /// exists for exactly these two keys -- showing a control that changes
    /// nothing would be worse than showing nothing.
    #[serde(default = "d_port", skip_serializing)]
    pub port: u16,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    /// Accepted and ignored, for the same reason and on the same terms as
    /// [`NormalizeConfig::port`] -- `skip_serializing` included.
    #[serde(default = "d_llama_path", skip_serializing)]
    pub llama_server_path: String,
    /// `n_ctx` for the in-process context, and still load-bearing: the
    /// prompt plus the reply budget must fit inside it or
    /// `LlamaEngine::generate` refuses the utterance.
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
    /// Spec 10.3, no longer deferred -- but no longer `ydotool type` either.
    /// Since 2026-09-02 this backend pastes: `wl-copy` the transcript, then
    /// have ydotool press a single Ctrl+V by raw keycode (Ctrl+Shift+V when
    /// the target window's class is in `terminal_classes`). `ydotool type`
    /// maps characters through a hard-coded US-QWERTY table -- on a German
    /// layout it swaps z/y and silently drops umlauts and ß entirely --
    /// while key *positions* (Ctrl, Shift, V) are layout-independent, and
    /// the pasted text arrives as whatever UTF-8 the clipboard holds. Still
    /// the backend for surfaces `wtype` cannot reach (GNOME/Mutter,
    /// XWayland, some Electron windows), still at the cost of a running
    /// `ydotoold` with write access to `/dev/uinput` -- plus `wl-copy`,
    /// which the clipboard fallback needs anyway. Not the default because
    /// `wtype` needs no setup at all.
    Ydotool,
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
    /// Window classes (matched case-insensitively against the class captured
    /// at recording start) whose paste chord is Ctrl+Shift+V rather than
    /// Ctrl+V -- terminals reserve plain Ctrl+V for the applications running
    /// inside them. Only the `ydotool` backend reads this.
    #[serde(default = "d_terminal_classes")]
    pub terminal_classes: Vec<String>,
}

fn d_backend() -> InjectBackend {
    InjectBackend::Wtype
}
fn d_keydelay() -> u32 {
    2
}
/// The window classes of the terminals a Wayland user plausibly runs, as the
/// compositor reports them (matching is case-insensitive, so "Alacritty" and
/// "org.gnome.Terminal" are covered by their lowercase spellings). This list is
/// the only copy: a written `config.toml` is rendered *from* it (spec §2), so
/// there is no shipped template left for it to drift against.
fn d_terminal_classes() -> Vec<String> {
    [
        "alacritty",
        "kitty",
        "foot",
        "footclient",
        "wezterm",
        "org.wezfurlong.wezterm",
        "ghostty",
        "com.mitchellh.ghostty",
        "konsole",
        "org.kde.konsole",
        "org.gnome.terminal",
        "gnome-terminal-server",
        "org.gnome.console",
        "kgx",
        "xterm",
        "urxvt",
        "st-256color",
        "terminator",
        "tilix",
        "xfce4-terminal",
    ]
    .map(str::to_string)
    .to_vec()
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            backend: d_backend(),
            trailing_space: true,
            keystroke_delay_ms: d_keydelay(),
            terminal_classes: d_terminal_classes(),
        }
    }
}

/// Spec 13's `[overlay]` section. Load-bearing on its own just by existing:
/// before this type existed, `Config`'s `#[serde(deny_unknown_fields)]` had
/// no `overlay` field to match against, so a user who copied the spec's own
/// documented `[overlay]` block into their config got a hard `Config::load()`
/// failure and the daemon refused to start (F5). Wiring these fields to the
/// overlay's actual position/size is separate, future work -- Wayland's
/// `xdg_shell` has no client-settable window position (see `hypr.rs`'s doc
/// comment), so `position` isn't even actionable from the daemon today; only
/// `width`/`height` describe values `tauri.conf.json` already hardcodes. What
/// matters here is that the section parses instead of bricking startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayConfig {
    #[serde(default = "d_overlay_position")]
    pub position: String,
    #[serde(default = "d_overlay_width")]
    pub width: u32,
    #[serde(default = "d_overlay_height")]
    pub height: u32,
}

fn d_overlay_position() -> String {
    "bottom-center".into()
}
fn d_overlay_width() -> u32 {
    280
}
fn d_overlay_height() -> u32 {
    72
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            position: d_overlay_position(),
            width: d_overlay_width(),
            height: d_overlay_height(),
        }
    }
}

/// One hand-written correction, applied before any fuzzy matching. This is
/// the mechanism for short acronyms, which fuzzy matching cannot serve: a
/// three-character term is within edit distance 2 of most three-letter words
/// in the language, so matching `GUI` loosely enough to catch a mis-heard
/// `SQUI` would also catch `gut` and `gib`. Length is what makes fuzzy
/// matching safe, so anything short belongs here instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

/// Spec 7.3's dictation vocabulary: names, jargon and acronyms the ASR has
/// never seen, corrected after transcription.
///
/// Two mechanisms rather than one, because neither covers the other's cases.
/// `terms` are matched fuzzily, so a term survives a small misrecognition
/// (`Hyperland` -> `Hyprland`) without anyone enumerating how it might be got
/// wrong; `replacements` are exact, for the short strings fuzzy matching is
/// structurally unsafe for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VocabularyConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub terms: Vec<String>,
    #[serde(default)]
    pub replacements: Vec<Replacement>,
    /// The share of a term that may be wrong and still match: 0.25 allows two
    /// wrong characters in an eight-character term. `0.0` disables fuzzy
    /// matching without disabling `replacements`.
    #[serde(default = "d_max_error_ratio")]
    pub max_error_ratio: f64,
    /// Terms shorter than this are matched exactly only -- see `Replacement`.
    #[serde(default = "d_min_term_chars")]
    pub min_term_chars: usize,
}

fn d_max_error_ratio() -> f64 {
    0.25
}
fn d_min_term_chars() -> usize {
    5
}

impl Default for VocabularyConfig {
    fn default() -> Self {
        Self {
            enabled: d_true(),
            terms: Vec::new(),
            replacements: Vec::new(),
            max_error_ratio: d_max_error_ratio(),
            min_term_chars: d_min_term_chars(),
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
    "~/yappr".into()
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
    pub models: ModelsConfig,
    #[serde(default)]
    pub normalize: NormalizeConfig,
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    #[serde(default)]
    pub inject: InjectConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
    #[serde(default)]
    pub style_default: StyleAxes,
    #[serde(default)]
    pub style_rules: Vec<StyleRule>,
    #[serde(default)]
    pub vocabulary: VocabularyConfig,
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

    /// Loads the config, writing a default file if none exists.
    pub fn load() -> Result<Self> {
        Self::load_from(&paths::config_file())
    }

    /// [`Config::load`] against an explicit path.
    ///
    /// The path is a parameter so the daemon can be pointed at a scratch file
    /// in tests. `set-config` *writes* the config, and a test exercising it
    /// against `paths::config_file()` would overwrite the real config of
    /// whoever ran the suite -- the same hazard `paths::rejections_file()`
    /// already carries a warning about.
    pub fn load_from(p: &std::path::Path) -> Result<Self> {
        if !p.exists() {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(p, render(&Config::default()))?;
        }
        let s = std::fs::read_to_string(p)
            .with_context(|| format!("reading {}", p.display()))?;
        Self::from_str(&s)
    }

    /// `pub(crate)` for `config_write::save_config`, which validates a merged
    /// patch *before* rendering it so a bad change is reported against the
    /// change rather than against the generated text.
    pub(crate) fn validate(&self) -> Result<()> {
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
        if self.overlay.width == 0 {
            bail!("overlay.width must be greater than 0");
        }
        if self.overlay.height == 0 {
            bail!("overlay.height must be greater than 0");
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
        if !(0.0..=1.0).contains(&self.vocabulary.max_error_ratio) {
            bail!(
                "vocabulary.max_error_ratio must be between 0.0 and 1.0, got {}",
                self.vocabulary.max_error_ratio
            );
        }
        if self.vocabulary.min_term_chars == 0 {
            bail!("vocabulary.min_term_chars must be at least 1");
        }
        for r in &self.vocabulary.replacements {
            if r.from.trim().is_empty() {
                bail!("vocabulary.replacements: `from` must not be empty");
            }
        }
        for t in &self.vocabulary.terms {
            if t.trim().is_empty() {
                bail!("vocabulary.terms must not contain empty entries");
            }
        }
        if self.debug.dir.trim().is_empty() {
            bail!("debug.dir must not be empty");
        }
        Ok(())
    }
}

/// What every written `config.toml` starts with. Two lines, fixed: the file is
/// the app's now (spec §1), and the one thing a human who opens it needs to
/// know is that editing it is pointless.
const HEADER: &str = "\
# Automatisch erzeugt von yappr. Änderungen über die Einstellungen
# (Tray-Symbol anklicken oder `yappr --settings`) -- Handedits gehen verloren.
";

/// The config as it is written to disk: a pure function of [`Config`], which is
/// the whole of invariant 9's no-op-save guarantee now (spec §2). The same
/// config cannot render two different files, so a save that changes nothing
/// cannot change the file -- free, where `toml_edit` had to work for it.
///
/// `to_string_pretty` cannot fail for this type. `Config` is a plain tree of
/// structs, `Vec`s and scalars with no map keys that could be anything but
/// strings, and the value-before-table ordering TOML requires is handled by the
/// serializer itself rather than by field order here -- both verified against
/// this tree before the design was written (spec §2.2). An `expect` rather than
/// a `Result` keeps every caller from carrying an error case that cannot occur.
pub fn render(cfg: &Config) -> String {
    let body = toml::to_string_pretty(cfg).expect("Config is always serializable to TOML");
    format!("{HEADER}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_defaults_cover_common_terminals() {
        let c = Config::from_str("").unwrap();
        for class in ["alacritty", "kitty", "foot", "org.wezfurlong.wezterm", "konsole"] {
            assert!(
                c.inject.terminal_classes.iter().any(|t| t == class),
                "{class} missing from the default terminal list: {:?}",
                c.inject.terminal_classes
            );
        }
    }

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
        assert_eq!(c.overlay.position, "bottom-center");
        assert_eq!(c.overlay.width, 280);
        assert_eq!(c.overlay.height, 72);
        assert_eq!(c.style_default.styling, Styling::SemiCasual);
        assert_eq!(c.style_default.structure, Structure::Prose);
        assert_eq!(c.style_default.context, Context::General);
        assert!(!c.debug.enabled);
        assert_eq!(c.debug.dir, "~/yappr");
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

    /// F5: before `OverlayConfig` existed, `Config`'s `deny_unknown_fields`
    /// had no `overlay` field to match against, so pasting spec 13's own
    /// documented `[overlay]` block into a config file was a hard
    /// `Config::load()` failure -- the daemon refused to start over a
    /// section its own spec tells the user to add. Pinned to the exact
    /// block spec 13 documents, not a paraphrase of it.
    #[test]
    fn spec_13s_documented_overlay_block_loads() {
        let c = Config::from_str(
            r#"
            [overlay]
            position = "bottom-center"
            width = 280
            height = 72
            "#,
        )
        .unwrap();
        assert_eq!(c.overlay.position, "bottom-center");
        assert_eq!(c.overlay.width, 280);
        assert_eq!(c.overlay.height, 72);
    }

    #[test]
    fn unknown_overlay_key_is_a_load_error() {
        let err = Config::from_str("[overlay]\nfoo = 1\n").unwrap_err();
        assert!(err.to_string().contains("foo"), "got: {err}");
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
            "[overlay]\nwidth = 0\n",
            "[overlay]\nheight = 0\n",
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
    fn a_rendered_default_config_parses_back_as_the_default() {
        // The guard that derived `Default` agrees with the serde `default =
        // "d_*"` functions -- the only whole-`Config` comparison in the
        // workspace, and load-bearing beyond this file: `GetConfig` ships
        // `Config::default()` on the wire as the settings GUI's reset targets
        // (invariant 9), so a drift here is a reset button that restores the
        // wrong value. Used to run against the hand-written
        // `DEFAULT_CONFIG_TOML`; the template is gone, the property is not.
        let rendered = render(&Config::default());
        assert_eq!(Config::from_str(&rendered).unwrap(), Config::default());

        // Every section must appear. A `Config` field that rendered to nothing
        // would be a group of settings the GUI shows as absent -- structural
        // now that the file is generated, but cheap to keep honest.
        for section in [
            "[audio]",
            "[asr]",
            "[models]",
            "[normalize]",
            "[guardrail]",
            "[inject]",
            "[overlay]",
            "[style_default]",
            "[vocabulary]",
            "[debug]",
        ] {
            assert!(rendered.contains(section), "{section} missing from the rendered default");
        }
    }

    #[test]
    fn rendering_the_same_config_twice_is_byte_identical() {
        // What replaces invariant 9's old byte-identical guarantee. The
        // renderer is a pure function of `Config`, so the same config cannot
        // produce two different files and a save that changes nothing cannot
        // change the file. `toml_edit` had to work for that property; purity
        // gives it away.
        let cfg = Config::default();
        assert_eq!(render(&cfg), render(&cfg));
    }

    #[test]
    fn a_fully_populated_config_round_trips_through_the_renderer() {
        // Exercises the array-of-tables and the `Option` axes rather than
        // assuming them: `StyleRule`'s unset axes must come back absent, since
        // TOML has no null to write them as.
        let cfg = Config::from_str(
            r#"
[[style_rules]]
match_class = "term.*"
styling = "formal"

[vocabulary]
terms = ["Kubernetes"]

[[vocabulary.replacements]]
from = "kdd"
to = "KDD"
"#,
        )
        .unwrap();
        assert_eq!(Config::from_str(&render(&cfg)).unwrap(), cfg);
    }

    #[test]
    fn the_two_obsolete_normalize_keys_are_read_but_never_written() {
        // Spec §2.1. Under the old comment-preserving merge these survived
        // only in files that already had them. A canonical dump would write
        // them into every user's file, which is the opposite of retiring them
        // -- so they are `skip_serializing`. Reading one must still work: the
        // section is `deny_unknown_fields`, so a pre-existing file naming them
        // would otherwise become a hard startup failure.
        let rendered = render(&Config::default());
        assert!(!rendered.contains("port"), "normalize.port must not be written any more");
        assert!(!rendered.contains("llama_server_path"));

        assert!(
            Config::from_str("[normalize]\nport = 8730\nllama_server_path = \"llama-server\"\n")
                .is_ok(),
            "a pre-existing file naming the retired keys must still load"
        );
    }

    #[test]
    fn every_inject_backend_is_spelled_the_way_config_toml_spells_it() {
        // `ENUMS["inject.backend"]` and `HELP["inject.backend"]` in
        // `src/settings/schema.ts` both hand-repeat these strings; this pins
        // what they have to agree with. (There used to be a third copy, the
        // `# "wtype" | "ydotool" | "clipboard"` annotation in the shipped
        // template -- generated files carry no annotations, so that one is
        // gone.) `deny_unknown_fields` makes a misspelling a hard startup
        // failure, not a silent fallback.
        for (spelling, expected) in [
            ("wtype", InjectBackend::Wtype),
            ("ydotool", InjectBackend::Ydotool),
            ("clipboard", InjectBackend::Clipboard),
        ] {
            let c = Config::from_str(&format!("[inject]\nbackend = \"{spelling}\"\n")).unwrap();
            assert_eq!(c.inject.backend, expected, "for {spelling}");
        }
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

    #[test]
    fn the_vocabulary_section_loads_with_documented_defaults() {
        let c = Config::from_str("").unwrap();
        assert!(c.vocabulary.enabled);
        assert!(c.vocabulary.terms.is_empty());
        assert!(c.vocabulary.replacements.is_empty());
        assert_eq!(c.vocabulary.max_error_ratio, 0.25);
        assert_eq!(c.vocabulary.min_term_chars, 5);
    }

    #[test]
    fn a_vocabulary_section_with_terms_and_replacements_loads() {
        let c = Config::from_str(
            r#"
            [vocabulary]
            terms = ["Hyprland", "sherpa-onnx"]

            [[vocabulary.replacements]]
            from = "Settings-SQUI"
            to = "Settings-GUI"
            "#,
        )
        .unwrap();
        assert_eq!(c.vocabulary.terms, ["Hyprland", "sherpa-onnx"]);
        assert_eq!(c.vocabulary.replacements.len(), 1);
        assert_eq!(c.vocabulary.replacements[0].from, "Settings-SQUI");
        assert_eq!(c.vocabulary.replacements[0].to, "Settings-GUI");
    }

    #[test]
    fn unknown_vocabulary_key_is_a_load_error() {
        let err = Config::from_str("[vocabulary]\nfoo = 1\n").unwrap_err();
        assert!(err.to_string().contains("foo"), "got: {err}");
    }

    /// A ratio above 1.0 would let a term match a token sharing not a single
    /// character with it, turning the vocabulary into a text shredder. Caught
    /// at load rather than producing nonsense at dictation time.
    #[test]
    fn an_out_of_range_max_error_ratio_is_a_load_error() {
        let err = Config::from_str("[vocabulary]\nmax_error_ratio = 1.5\n").unwrap_err();
        assert!(err.to_string().contains("max_error_ratio"), "got: {err}");
        let err = Config::from_str("[vocabulary]\nmax_error_ratio = -0.1\n").unwrap_err();
        assert!(err.to_string().contains("max_error_ratio"), "got: {err}");
    }

    #[test]
    fn an_empty_replacement_source_is_a_load_error() {
        let err = Config::from_str(
            r#"
            [[vocabulary.replacements]]
            from = ""
            to = "something"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("from"), "got: {err}");
    }

    #[test]
    fn models_defaults_to_lazy_loading_with_a_sixty_second_idle_timeout() {
        let c = ModelsConfig::default();
        assert!(!c.preload_at_startup);
        assert_eq!(c.idle_unload_seconds, 60);
    }

    #[test]
    fn a_config_written_before_the_models_section_existed_still_parses() {
        // Every user upgrading into this feature has one of these on disk.
        let c = Config::from_str("[audio]\ndevice = \"default\"\n").unwrap();
        assert_eq!(c.models, ModelsConfig::default());
    }

    #[test]
    fn an_unknown_key_in_models_is_a_hard_error() {
        // Invariant 4: `deny_unknown_fields`, so a typo fails loudly at startup
        // rather than being silently ignored.
        let err = Config::from_str("[models]\nidle_unload_secs = 30\n").unwrap_err().to_string();
        assert!(err.contains("idle_unload_secs"), "unhelpful error: {err}");
    }

    #[test]
    fn zero_seconds_is_accepted_and_means_never_unload() {
        let c = Config::from_str("[models]\nidle_unload_seconds = 0\n").unwrap();
        assert_eq!(c.models.idle_unload_seconds, 0);
    }

}
