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
    #[serde(default)]
    pub voice: VoiceConfig,
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

/// Server-side text-to-speech. When enabled and a program is set, `oracle-core`
/// synthesizes each spoken reply with a local neural voice (e.g. Piper) and
/// streams the audio to the HUD to play — replacing the browser's robotic TTS.
/// When disabled, empty, or the program fails, the HUD falls back to browser
/// speech, so the Oracle always talks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    /// Master switch for server-side neural TTS.
    #[serde(default)]
    pub tts_enabled: bool,
    /// TTS program: a full path to `piper.exe` (or any synth on PATH). Empty ⇒
    /// browser fallback.
    #[serde(default)]
    pub tts_program: String,
    /// Arguments for the TTS program. The literal token `{out}` is replaced with
    /// a temporary `.wav` path core reads back; the text to speak is written to
    /// the program's stdin. The default matches Piper's CLI.
    #[serde(default = "default_tts_args")]
    pub tts_args: Vec<String>,

    /// A persistent, OpenAI-compatible TTS HTTP endpoint (e.g. Kokoro-FastAPI's
    /// `/v1/audio/speech`). When set it takes precedence over `tts_program`: the
    /// model stays loaded in the server, so replies come back warm AND fast.
    /// Empty ⇒ use the command backend / browser fallback.
    #[serde(default)]
    pub tts_http_url: String,
    /// The voice name to request from the HTTP server (e.g. Kokoro's "af_heart",
    /// "af_bella", "bf_emma").
    #[serde(default = "default_tts_voice")]
    pub tts_voice: String,
    /// The model name to request from the HTTP server.
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    /// Optional: a TTS server to launch and keep alive (hidden), so one click
    /// brings the voice up too. E.g. a Kokoro server's exe/`python`/`docker`.
    /// Empty ⇒ you start the server yourself.
    #[serde(default)]
    pub tts_server_program: String,
    /// Arguments for `tts_server_program`.
    #[serde(default)]
    pub tts_server_args: Vec<String>,

    /// Master switch for local speech recognition (Whisper). When on, the HUD's
    /// microphone captures audio and sends it here to be transcribed by
    /// `stt_program`, replacing the browser's speech recognition.
    #[serde(default)]
    pub stt_enabled: bool,
    /// STT program: a full path to whisper.cpp's `whisper-cli.exe` (or any
    /// transcriber on PATH). Empty ⇒ STT disabled.
    #[serde(default)]
    pub stt_program: String,
    /// Arguments for the STT program. The literal token `{in}` is replaced with
    /// the path to the captured 16 kHz mono WAV; the transcript is read from the
    /// program's stdout. The default matches whisper.cpp's CLI (base English
    /// model, no timestamps, quiet).
    #[serde(default = "default_stt_args")]
    pub stt_args: Vec<String>,

    /// Always-on wake word. When enabled, core runs `wake_program` (whisper.cpp's
    /// streaming recognizer, `whisper-stream.exe`), reads its live transcript,
    /// shows it in the HUD, and starts a turn whenever it hears "Delphi". The
    /// streaming recognizer captures the microphone natively (no WebView2).
    #[serde(default)]
    pub wake_enabled: bool,
    /// Path to the streaming recognizer (`whisper-stream.exe`). Empty ⇒ no wake.
    #[serde(default)]
    pub wake_program: String,
    /// Arguments for the streaming recognizer. Use the ABSOLUTE model path (core
    /// runs it from a different directory). No `{...}` placeholders — it captures
    /// the mic itself and prints transcripts to stdout.
    #[serde(default = "default_wake_args")]
    pub wake_args: Vec<String>,
}

fn default_tts_args() -> Vec<String> {
    vec![
        "--model".into(),
        "voice.onnx".into(),
        "--output_file".into(),
        "{out}".into(),
    ]
}

fn default_tts_voice() -> String {
    "af_heart".into() // Kokoro's warm default
}
fn default_tts_model() -> String {
    "kokoro".into()
}

fn default_stt_args() -> Vec<String> {
    vec![
        "-m".into(),
        "ggml-base.en.bin".into(),
        "-f".into(),
        "{in}".into(),
        "-nt".into(), // no timestamps in the output
        "-np".into(), // no progress prints
    ]
}

fn default_wake_args() -> Vec<String> {
    // NOTE: no `-nt`. whisper-stream with timestamps prints one clean, newline-
    // terminated line per utterance (`[t0 --> t1]  text`), which core parses;
    // `-nt` makes it redraw a single line with carriage returns that don't split
    // into lines. Leave timestamps ON.
    vec![
        "-m".into(),
        "ggml-base.en.bin".into(),
        "-t".into(),
        "6".into(),
        "--step".into(),
        "0".into(), // 0 = VAD-driven segmentation (transcribe on speech pauses)
        "--length".into(),
        "5000".into(),
        "-vth".into(),
        "0.6".into(), // voice-activity threshold
    ]
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            tts_enabled: false,
            tts_program: String::new(),
            tts_args: default_tts_args(),
            tts_http_url: String::new(),
            tts_voice: default_tts_voice(),
            tts_model: default_tts_model(),
            tts_server_program: String::new(),
            tts_server_args: Vec::new(),
            stt_enabled: false,
            stt_program: String::new(),
            stt_args: default_stt_args(),
            wake_enabled: false,
            wake_program: String::new(),
            wake_args: default_wake_args(),
        }
    }
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
    fn voice_defaults_off_with_out_placeholder() {
        // TTS ships disabled (browser fallback), and the arg template carries the
        // {out} placeholder core substitutes with the temp WAV path.
        let v = VoiceConfig::default();
        assert!(!v.tts_enabled);
        assert!(v.tts_args.iter().any(|a| a.contains("{out}")));
        // And it round-trips through the example config.
        let parsed: Config = toml::from_str(&Config::example_toml()).unwrap();
        assert!(!parsed.voice.tts_enabled);
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
