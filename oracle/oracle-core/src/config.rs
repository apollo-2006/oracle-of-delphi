//! Typed, validated configuration (architecture §7).
//!
//! One `oracle.toml` drives every process. Structural keys (sockets, ports,
//! model paths) require a restart; non-structural keys (thresholds, voices) can
//! be hot-reloaded via a file watch. Validation happens at load time with
//! precise, user-facing error messages — a bad config fails fast and loud, not
//! at 3 a.m. when a tool call hits a missing field.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub actd: ActdConfig,
    #[serde(default)]
    pub hud: HudConfig,
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub google: GoogleConfig,
    #[serde(default)]
    pub supervise: SuperviseConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleConfig {
    /// Path to the Google Desktop-app credentials.json. Empty = Google disabled.
    #[serde(default)]
    pub credentials_path: String,
    /// Account label used when sealing/loading the token (matches `auth --account`).
    #[serde(default = "default_account")]
    pub account: String,
}

fn default_account() -> String {
    "default".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    /// Directory for runtime sockets (defaults to $XDG_RUNTIME_DIR/oracle).
    pub runtime_dir: String,
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub vad_onset_fast: f32,
    pub vad_release: f32,
    pub hangover_ms: u32,
    pub tts_voice: String,
    /// Capture device name (as the OS reports it), or "default".
    #[serde(default = "default_device")]
    pub input_device: String,
    /// Output device name, or "default".
    #[serde(default = "default_device")]
    pub output_device: String,
}

fn default_device() -> String {
    "default".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// "mock" or an http(s) URL to a llama.cpp server.
    pub backend: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Directory holding GGUF model files (e.g. E:\\oracle-models on Windows).
    #[serde(default)]
    pub model_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub db_path: String,
    pub retrieve_limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActdConfig {
    pub socket: String,
    /// Standing grant of the "sensitive" tier for the session (else per-action
    /// confirmation). Default false — safe by default.
    pub grant_sensitive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HudConfig {
    pub bind: String,
    /// If empty, a random token is generated at launch.
    pub token: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub step_budget: u32,
}

/// Self-supervision: what `oracle-core run` should launch and manage on its own
/// so the whole assistant comes up from a single click instead of several
/// terminals. Each managed child is started hidden, restarted if it dies, and
/// killed when core exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuperviseConfig {
    /// Launch the local LLM server (e.g. llama-server) as a managed child.
    #[serde(default)]
    pub autostart_llm: bool,
    /// The LLM server program (name on PATH or a full path to the .exe).
    #[serde(default)]
    pub llm_program: String,
    /// Arguments passed to the LLM server (model path, port, ngl, etc.).
    #[serde(default)]
    pub llm_args: Vec<String>,
    /// Launch `oracle-actd` as a managed child (recommended: one-click startup).
    #[serde(default = "default_true")]
    pub autostart_actd: bool,
    /// Full path to the oracle-actd binary. Empty = look next to oracle-core,
    /// then fall back to `oracle-actd` on PATH.
    #[serde(default)]
    pub actd_program: String,
    /// Open the HUD in a chromeless app window on startup.
    #[serde(default = "default_true")]
    pub open_window: bool,
    /// Which browser hosts the app window: "edge", "chrome", a full path to a
    /// Chromium-based browser, or "default"/"" to just open the default browser.
    #[serde(default = "default_browser")]
    pub browser: String,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        SuperviseConfig {
            autostart_llm: false,
            llm_program: String::new(),
            llm_args: Vec::new(),
            autostart_actd: true,
            actd_program: String::new(),
            open_window: true,
            browser: default_browser(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_browser() -> String {
    "edge".into()
}

// --- Defaults -------------------------------------------------------------

impl Default for General {
    fn default() -> Self {
        General {
            runtime_dir: default_runtime_dir(),
            log_level: "info".into(),
        }
    }
}
impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            sample_rate: 48000,
            vad_onset_fast: 0.85,
            vad_release: 0.35,
            hangover_ms: 200,
            tts_voice: "kokoro".into(),
            input_device: default_device(),
            output_device: default_device(),
        }
    }
}
impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            backend: "mock".into(),
            model: "qwen2.5-14b-instruct".into(),
            max_tokens: 1024,
            temperature: 0.7,
            model_dir: String::new(),
        }
    }
}
impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            db_path: "oracle.db".into(),
            retrieve_limit: 6,
        }
    }
}
impl Default for ActdConfig {
    fn default() -> Self {
        ActdConfig {
            socket: format!("{}/actd.sock", default_runtime_dir()),
            grant_sensitive: false,
        }
    }
}
impl Default for HudConfig {
    fn default() -> Self {
        HudConfig {
            bind: "127.0.0.1:8770".into(),
            token: String::new(),
            enabled: true,
        }
    }
}
impl Default for AgentSettings {
    fn default() -> Self {
        AgentSettings { step_budget: 12 }
    }
}
fn default_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|d| format!("{d}/oracle"))
        .unwrap_or_else(|_| "/tmp/oracle".into())
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    /// Load and validate from a TOML file. Missing file → defaults (with a note
    /// left to the caller); present-but-invalid → a precise error.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `path` if it exists, else defaults.
    pub fn load_or_default(path: &Path) -> Result<Config, ConfigError> {
        if path.exists() {
            Config::load(path)
        } else {
            Ok(Config::default())
        }
    }

    /// Semantic validation beyond types.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(0.0..=1.0).contains(&self.audio.vad_onset_fast) {
            return Err(ConfigError::Invalid(
                "audio.vad_onset_fast must be in 0.0..=1.0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.audio.vad_release) {
            return Err(ConfigError::Invalid(
                "audio.vad_release must be in 0.0..=1.0".into(),
            ));
        }
        if self.audio.vad_release >= self.audio.vad_onset_fast {
            return Err(ConfigError::Invalid(
                "audio.vad_release must be below vad_onset_fast (hysteresis)".into(),
            ));
        }
        if !(0.0..=2.0).contains(&self.llm.temperature) {
            return Err(ConfigError::Invalid(
                "llm.temperature must be in 0.0..=2.0".into(),
            ));
        }
        if self.agent.step_budget == 0 || self.agent.step_budget > 100 {
            return Err(ConfigError::Invalid(
                "agent.step_budget must be in 1..=100".into(),
            ));
        }
        if self.llm.backend != "mock" && !self.llm.backend.starts_with("http") {
            return Err(ConfigError::Invalid(
                "llm.backend must be \"mock\" or an http(s) URL".into(),
            ));
        }
        Ok(())
    }

    /// Serialize a fully-populated default config, for `oracle-core write-config`.
    pub fn example_toml() -> String {
        toml::to_string_pretty(&Config::default()).unwrap()
    }

    /// Whether a change from `self` to `other` requires a restart (structural)
    /// vs. can be hot-applied. Sockets, ports, and paths are structural.
    pub fn requires_restart(&self, other: &Config) -> bool {
        self.general.runtime_dir != other.general.runtime_dir
            || self.actd.socket != other.actd.socket
            || self.hud.bind != other.hud.bind
            || self.memory.db_path != other.memory.db_path
            || self.llm.backend != other.llm.backend
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn example_roundtrips() {
        let toml_str = Config::example_toml();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert!(parsed.validate().is_ok());
        assert_eq!(parsed.audio.sample_rate, 48000);
    }

    #[test]
    fn rejects_inverted_hysteresis() {
        let mut c = Config::default();
        c.audio.vad_release = 0.9;
        c.audio.vad_onset_fast = 0.5;
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("hysteresis"));
    }

    #[test]
    fn rejects_bad_backend() {
        let mut c = Config::default();
        c.llm.backend = "ftp://nope".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let toml_str = "[general]\nruntime_dir = \"/tmp\"\nlog_level = \"info\"\nbogus = 1\n";
        let err = toml::from_str::<Config>(toml_str).unwrap_err();
        assert!(err.to_string().contains("bogus") || err.to_string().contains("unknown"));
    }

    #[test]
    fn restart_detection() {
        let a = Config::default();
        let mut b = a.clone();
        b.audio.tts_voice = "piper".into(); // non-structural
        assert!(!a.requires_restart(&b));
        b.actd.socket = "/tmp/other.sock".into(); // structural
        assert!(a.requires_restart(&b));
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = Path::new("/nonexistent/oracle.toml");
        let cfg = Config::load_or_default(path).unwrap();
        assert_eq!(cfg.llm.backend, "mock");
    }

    #[test]
    fn present_file_loads_and_validates() {
        let dir = std::env::temp_dir().join(format!("oracle-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oracle.toml");
        std::fs::write(&path, Config::example_toml()).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.agent.step_budget, 12);
        std::fs::remove_dir_all(&dir).ok();
    }
}
