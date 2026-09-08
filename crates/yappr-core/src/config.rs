use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

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

/// Which speech-recognition model the pipeline loads.
///
/// The spelling here is the stable, user-visible name in `config.toml` and
/// the key in `models::ASR_MODELS`. It is deliberately *not* the lock-file
/// key: the Parakeet v3 entry is pinned under the bare name `parakeet`,
/// which can never change (see `models::Artifact::name`).
///
/// See `docs/superpowers/specs/2026-09-08-asr-model-selection-design.md` §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AsrModel {
    /// Parakeet TDT 0.6b v3 -- multilingual, the default, and the only model
    /// that existed before model selection did.
    #[serde(rename = "parakeet-tdt-v3")]
    ParakeetTdtV3,
    /// Parakeet Unified 0.6b -- English only.
    #[serde(rename = "parakeet-unified-en")]
    ParakeetUnifiedEn,
    /// Nemotron 3.5 ASR 0.6b, 560 ms cache-aware streaming export --
    /// multilingual, and the only entry that is not an `OfflineRecognizer`.
    #[serde(rename = "nemotron-3.5")]
    Nemotron35,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsrConfig {
    #[serde(default = "d_asr_model")]
    pub model: AsrModel,
    /// `"auto"`, or a two-letter code such as `"de"`. Only a
    /// `CacheAwareStreaming` model reads this; the offline models are either
    /// single-language or detect it themselves.
    #[serde(default = "d_asr_language")]
    pub language: String,
    #[serde(default = "d_threads")]
    pub num_threads: i32,
}

fn d_asr_model() -> AsrModel {
    AsrModel::ParakeetTdtV3
}
fn d_asr_language() -> String {
    "auto".to_string()
}
fn d_threads() -> i32 {
    4
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            model: d_asr_model(),
            language: d_asr_language(),
            num_threads: d_threads(),
        }
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
    /// `deny_unknown_fields` (invariant 4) -- deleting it here would make
    /// every pre-existing config unloadable, and `load_or_quarantine` would
    /// then move each one aside and reset that user's settings.
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

/// Which chord the `ydotool` backend presses to paste (spec 10.3).
///
/// `Auto` is the historical behaviour and still the default: Ctrl+Shift+V
/// when the focused window's class is in `terminal_classes`, plain Ctrl+V
/// otherwise. The override exists because that decision has exactly one
/// source -- `hypr::active_window_class()`, i.e. `hyprctl` -- and it is
/// unavailable in precisely the places this backend is *for*:
///
///   - On GNOME/Mutter there is no `hyprctl` at all, and `wtype` does not
///     work there, so ydotool is the only backend a GNOME user has.
///   - On Hyprland, `hyprctl` needs `HYPRLAND_INSTANCE_SIGNATURE` in the
///     daemon's environment. Without it (a systemd user unit or a `.desktop`
///     autostart on a session that never exported it) it prints
///     "HYPRLAND_INSTANCE_SIGNATURE not set!" -- on *stdout* -- and exits 1;
///     with a stale signature it cannot connect and exits 4.
///   - And on Hyprland proper, `hyprctl` answers `{}` with exit 0 whenever
///     nothing is focused.
///
/// All three collapse to the same `None`, which is the point: the caller
/// cannot tell "no compositor" from "nothing focused", and `wants_shift`
/// reads either as "not a terminal".
///
/// An unknown class means `Auto` sends plain Ctrl+V, which every terminal
/// ignores -- and `ydotool` exits 0, so nothing fails, no clipboard-fallback
/// notification fires and nothing is logged as an error. The user sees a
/// dictation that simply produced no text. `CtrlShiftV` is the answer for
/// "my compositor cannot tell yappr what is focused, and I dictate into a
/// terminal".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasteChord {
    /// Decide per window, from `terminal_classes`.
    Auto,
    /// Always Ctrl+V.
    CtrlV,
    /// Always Ctrl+Shift+V.
    CtrlShiftV,
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
    /// Which paste chord the `ydotool` backend presses. Only that backend
    /// reads this. See [`PasteChord`] for why an override is needed at all.
    #[serde(default = "d_paste_chord")]
    pub paste_chord: PasteChord,
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
        // GNOME's default terminal since Fedora 41, and the one the
        // accessibility provider is most likely to report on that desktop --
        // as the bare `ptyxis`, which is the AT-SPI application name, not the
        // app id. Both spellings, because a future class source may differ.
        "ptyxis",
        "org.gnome.Ptyxis",
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
fn d_paste_chord() -> PasteChord {
    PasteChord::Auto
}

impl Default for InjectConfig {
    fn default() -> Self {
        Self {
            backend: d_backend(),
            trailing_space: true,
            keystroke_delay_ms: d_keydelay(),
            terminal_classes: d_terminal_classes(),
            paste_chord: d_paste_chord(),
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

    /// Loads the *real* config, migrating a pre-2026-09-02 file across first
    /// and writing a default if neither exists.
    ///
    /// The migration lives here, not only in `server::start`, because
    /// [`Config::load_from`] **creates** the file when it is absent -- so any
    /// caller that touched the new path before the daemon did would make
    /// `current.exists()` true and strand the legacy config forever. Two such
    /// callers exist and neither needs a daemon: `setup.rs`'s `debug_summary`
    /// (`yappr --debug`) and `wizard.rs`'s `wizard_state`.
    ///
    /// Best-effort and silent here on purpose. `server::start` calls
    /// [`migrate_from_legacy`] itself, before this, precisely so it can report
    /// the outcome; by the time anything else reaches this path the migration
    /// has already happened or already failed, and a second opinion about it
    /// would only be noise.
    pub fn load() -> Result<Self> {
        let current = paths::config_file();
        let _ = migrate_from_legacy(&paths::legacy_config_file(), &current);
        Self::load_from(&current)
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
        let lang_ok = self.asr.language == "auto"
            || (self.asr.language.len() == 2
                && self.asr.language.bytes().all(|b| b.is_ascii_lowercase()));
        if !lang_ok {
            bail!(
                "asr.language must be \"auto\" or a two-letter lowercase code such as \"de\", got {:?}",
                self.asr.language
            );
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

/// What [`load_or_quarantine`] moved aside, and why.
#[derive(Debug, Clone)]
pub struct Quarantine {
    /// Where the unloadable file went, or `None` when even the rename failed
    /// and it is still sitting at its original path. The distinction reaches
    /// the user: a banner saying a file was moved somewhere it demonstrably
    /// is not would send them looking in the wrong place.
    pub moved_to: Option<PathBuf>,
    /// The load error as text rather than as an `anyhow::Error`, because this
    /// is stored behind a `Mutex` for the life of the process and
    /// `anyhow::Error` is not `Clone`.
    pub error: String,
}

/// [`Config::load_from`] for the one caller that must never fail: startup
/// (spec §3).
///
/// `server::start` loads the config eagerly and `?`s it, and `setup()` in
/// `src-tauri/src/lib.rs` calls that -- so before this existed, one typo'd key
/// aborted Tauri setup and left the user with no tray, no settings window, and
/// nothing but a text editor to repair it with. That was defensible while
/// `config.toml` was a documented human interface. Now that the settings window
/// is the interface, a config that stops the window from opening is a config
/// that cannot be fixed at all.
///
/// `deny_unknown_fields` (invariant 4) is untouched -- a typo, a downgrade or a
/// half-written file is still *detected*. Only the consequence changed: the
/// file is moved aside, a fresh default takes its place, and the reason travels
/// back to the GUI. Every other caller stays strict on purpose: `Request::Reload`
/// answering "your file is broken" is the honest answer, where a `Reload` that
/// silently reset a user's settings would not be.
pub fn load_or_quarantine(path: &Path) -> (Config, Option<Quarantine>) {
    match Config::load_from(path) {
        Ok(cfg) => (cfg, None),
        Err(e) => {
            let error = format!("{e:#}");
            match quarantine_file(path) {
                Ok(moved_to) => {
                    // Best-effort: a default file is a convenience, and failing
                    // to write one still leaves a daemon that runs on the
                    // in-memory defaults. The next successful save creates it.
                    let _ = std::fs::write(path, render(&Config::default()));
                    (Config::default(), Some(Quarantine { moved_to: Some(moved_to), error }))
                }
                // Nothing left to do but carry on: refusing to start is the
                // exact failure mode this function exists to remove, so a
                // rename that fails must not resurrect it.
                Err(rename_err) => (
                    Config::default(),
                    Some(Quarantine {
                        moved_to: None,
                        error: format!(
                            "{error} (konnte auch nicht beiseitegelegt werden: {rename_err})"
                        ),
                    }),
                ),
            }
        }
    }
}

/// Renames `path` to `<name>.broken-<unix seconds>`, adding a counter if that
/// name is somehow taken -- two quarantines in the same second must not let the
/// second destroy the first one's evidence.
fn quarantine_file(path: &Path) -> std::io::Result<PathBuf> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut candidate = dir.join(format!("{name}.broken-{secs}"));
    let mut n = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{name}.broken-{secs}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &candidate)?;
    Ok(candidate)
}

/// What [`migrate_from_legacy`] did, which startup has to be able to tell
/// apart -- "there was nothing to move" and "there was something and it was
/// unreadable" look identical from the outside and mean opposite things to the
/// user.
#[derive(Debug, Clone)]
pub enum Migration {
    /// No legacy file, or the new one already exists.
    NotNeeded,
    /// Moved across. The old file is at this path now.
    Moved(PathBuf),
    /// A legacy config exists and will not load, so it was left exactly where
    /// it is. Startup reports this: the user is about to be handed defaults and
    /// would otherwise have no idea why.
    LegacyUnreadable { left_at: PathBuf, error: String },
}

/// Moves a pre-2026-09-02 config out of `~/.config` and into the state dir
/// (spec §4).
///
/// Returns the path the legacy file was renamed to, or `None` when there was
/// nothing to do. Both paths are parameters rather than `paths::` calls for the
/// same reason [`Config::load_from`]'s is: a test that ran this against the
/// real pair would move the config of whoever ran the suite.
///
/// Three rules, and the last one is the one worth stating:
///
/// - the state-dir file wins if it already exists, so a legacy file left behind
///   by a downgrade-and-upgrade cannot silently overwrite newer settings;
/// - the legacy file is *renamed*, never deleted -- if this migration gets
///   something wrong, the original is still sitting there;
/// - a legacy file that will not load is left entirely alone. Migrating it
///   would only move the problem into the new location, where startup's
///   quarantine has to deal with it anyway, and it would destroy the evidence
///   in a place the user knows to look for it.
pub fn migrate_from_legacy(legacy: &Path, current: &Path) -> Result<Migration> {
    if current.exists() || !legacy.exists() {
        return Ok(Migration::NotNeeded);
    }
    let cfg = match Config::load_from(legacy) {
        Ok(cfg) => cfg,
        // Not `NotNeeded`. This is the case that silently cost a user every
        // setting they had: the legacy file is left alone (right), but the new
        // path does not exist yet, so `load_or_quarantine` creates a clean
        // default there and reports nothing wrong (wrong). Startup has to be
        // told, or the upgrade is a silent factory reset.
        Err(e) => {
            return Ok(Migration::LegacyUnreadable {
                left_at: legacy.to_path_buf(),
                error: format!("{e:#}"),
            })
        }
    };
    if let Some(parent) = current.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Atomic, not a bare `write`. This is a one-shot: a torn write leaves a
    // file at the new path, which makes `current.exists()` true forever, which
    // means the migration never runs again and the user's real settings stay
    // stranded in `~/.config` where nothing will ever look for them.
    crate::config_write::write_atomically(current, &render(&cfg))?;

    // Built from the file name rather than `with_extension`, which would
    // replace `.toml` instead of following it.
    let name = legacy.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let renamed = legacy.with_file_name(format!("{name}.migrated"));
    std::fs::rename(legacy, &renamed)
        .with_context(|| format!("renaming {}", legacy.display()))?;
    Ok(Migration::Moved(renamed))
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

    /// A fresh, collision-free scratch directory. Never `paths::config_file()`
    /// or `paths::legacy_config_file()`: this test module *moves and renames*
    /// files, and running that against the real pair would migrate the config
    /// of whoever ran the suite.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("yappr-config-{tag}-{}-{n}", std::process::id()))
    }

    /// Invariant 4 rewritten (spec §3). Before this, one unrecognised key
    /// aborted Tauri setup: no tray, no settings window, and the only repair
    /// tool a text editor pointed at the very file the design has stopped
    /// inviting anyone to open.
    #[test]
    fn an_unloadable_config_is_quarantined_and_startup_gets_defaults() {
        let dir = scratch_dir("quarantine");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[audio]\nthis_key_does_not_exist = 1\n").unwrap();

        let (cfg, quarantine) = load_or_quarantine(&path);

        assert_eq!(cfg, Config::default(), "startup continues, on defaults");
        let q = quarantine.expect("the failure must be reported, never swallowed");
        let moved_to = q.moved_to.expect("the file must have been moved");
        assert!(moved_to.exists(), "the user's file must survive for inspection");
        assert!(q.error.contains("this_key_does_not_exist"), "and name what was wrong");
        assert_eq!(
            Config::load_from(&path).unwrap(),
            Config::default(),
            "a fresh default file takes its place"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `validate` failures are as fatal at startup as parse failures, so they
    /// take the same path. `max_seconds = 0` is the case worth naming:
    /// invariant 11 makes it the sole terminator of a forgotten recording.
    #[test]
    fn a_semantically_invalid_config_is_quarantined_too() {
        let dir = scratch_dir("quarantine-validate");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[audio]\nmax_seconds = 0\n").unwrap();

        let (cfg, quarantine) = load_or_quarantine(&path);

        assert_eq!(cfg.audio.max_seconds, Config::default().audio.max_seconds);
        assert!(quarantine.is_some(), "a config that fails validate is quarantined");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The healthy path must stay boring: nothing moved, nothing reported.
    #[test]
    fn a_loadable_config_is_returned_untouched_with_no_notice() {
        let dir = scratch_dir("quarantine-clean");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[audio]\nmax_seconds = 45\n").unwrap();

        let (cfg, quarantine) = load_or_quarantine(&path);

        assert_eq!(cfg.audio.max_seconds, 45);
        assert!(quarantine.is_none());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "nothing was moved aside");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two bad starts in the same second must not let the second one overwrite
    /// what the first moved aside -- that file is the user's only copy.
    #[test]
    fn a_second_quarantine_does_not_destroy_the_first_ones_evidence() {
        let dir = scratch_dir("quarantine-twice");
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(&path, "[audio]\nfirst_bad_key = 1\n").unwrap();
        let first = load_or_quarantine(&path).1.unwrap().moved_to.unwrap();
        std::fs::write(&path, "[audio]\nsecond_bad_key = 1\n").unwrap();
        let second = load_or_quarantine(&path).1.unwrap().moved_to.unwrap();

        assert_ne!(first, second);
        assert!(std::fs::read_to_string(&first).unwrap().contains("first_bad_key"));
        assert!(std::fs::read_to_string(&second).unwrap().contains("second_bad_key"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec §4. A user upgrading has a real config in the old place; losing it
    /// silently would be the worst possible first impression of this change.
    #[test]
    fn a_legacy_config_is_migrated_and_the_old_file_is_kept_under_a_new_name() {
        let dir = scratch_dir("migrate");
        let legacy = dir.join("old/config.toml");
        let current = dir.join("new/config.toml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "[audio]\nmax_seconds = 45\n").unwrap();

        let moved = migrate_from_legacy(&legacy, &current).unwrap();

        assert!(matches!(moved, Migration::Moved(_)), "got {moved:?}");
        assert_eq!(Config::load_from(&current).unwrap().audio.max_seconds, 45);
        assert!(!legacy.exists(), "the legacy file is renamed out of the way");
        assert!(dir.join("old/config.toml.migrated").exists(), "and never deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The state-dir file is the truth once it exists. A legacy file left
    /// behind by a downgrade-and-upgrade must not overwrite newer settings.
    #[test]
    fn migration_is_a_no_op_when_the_new_file_already_exists() {
        let dir = scratch_dir("migrate-noop");
        let legacy = dir.join("old/config.toml");
        let current = dir.join("new/config.toml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "[audio]\nmax_seconds = 45\n").unwrap();
        std::fs::write(&current, "[audio]\nmax_seconds = 99\n").unwrap();

        assert!(matches!(
            migrate_from_legacy(&legacy, &current).unwrap(),
            Migration::NotNeeded
        ));
        assert_eq!(Config::load_from(&current).unwrap().audio.max_seconds, 99);
        assert!(legacy.exists(), "an ignored legacy file is left exactly as it was");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec §4: migrating a broken file would only move the problem, and would
    /// move it somewhere the user does not know to look. Leave it, and let
    /// startup's quarantine deal with the new path.
    #[test]
    fn an_unloadable_legacy_config_is_left_alone_rather_than_migrated() {
        let dir = scratch_dir("migrate-broken");
        let legacy = dir.join("old/config.toml");
        let current = dir.join("new/config.toml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "[audio]\nnope = 1\n").unwrap();

        // Not `NotNeeded`: startup has to be able to tell "nothing to move"
        // from "something to move that I could not read", because the second
        // one means the user is about to lose every setting they had.
        let outcome = migrate_from_legacy(&legacy, &current).unwrap();
        assert!(matches!(outcome, Migration::LegacyUnreadable { .. }), "got {outcome:?}");
        assert!(!current.exists(), "nothing may be written from a file that will not load");
        assert!(legacy.exists(), "and the user's file stays where they left it");

        let _ = std::fs::remove_dir_all(&dir);
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
        // would otherwise be quarantined and have its settings reset.
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
    fn every_asr_model_spelling_round_trips_from_toml() {
        for (spelling, expected) in [
            ("parakeet-tdt-v3", AsrModel::ParakeetTdtV3),
            ("parakeet-unified-en", AsrModel::ParakeetUnifiedEn),
            ("nemotron-3.5", AsrModel::Nemotron35),
        ] {
            let c = Config::from_str(&format!("[asr]\nmodel = \"{spelling}\"\n")).unwrap();
            assert_eq!(c.asr.model, expected, "for {spelling}");
        }
    }

    #[test]
    fn a_config_written_before_model_selection_existed_still_loads() {
        // Invariant 4: [asr] is deny_unknown_fields, and every pre-existing
        // config.toml has num_threads in this section and nothing else.
        let c = Config::from_str("[asr]\nnum_threads = 7\n").unwrap();
        assert_eq!(c.asr.num_threads, 7);
        assert_eq!(c.asr.model, AsrModel::ParakeetTdtV3, "the default must not move");
        assert_eq!(c.asr.language, "auto");
    }

    #[test]
    fn an_unknown_asr_model_spelling_is_rejected_rather_than_silently_defaulted() {
        let err = Config::from_str("[asr]\nmodel = \"whisper\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("model"), "unhelpful error: {err:#}");
    }

    #[test]
    fn asr_language_must_be_auto_or_a_two_letter_code() {
        // `from_str` validates, so a bad value never becomes a `Config` at
        // all -- same as every other range check (`out_of_range_values_are_rejected`).
        for good in ["auto", "de", "en", "ja"] {
            let c = Config::from_str(&format!("[asr]\nlanguage = \"{good}\"\n"))
                .unwrap_or_else(|e| panic!("{good} should be accepted: {e:#}"));
            assert_eq!(c.asr.language, good);
        }
        for bad in ["Deutsch", "DE", "d", "de-DE", ""] {
            let err = Config::from_str(&format!("[asr]\nlanguage = \"{bad}\"\n"))
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("asr.language"),
                "{bad:?} was rejected, but not for a reason that names the key: {err:#}"
            );
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
