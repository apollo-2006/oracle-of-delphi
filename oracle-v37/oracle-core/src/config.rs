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
    pub proactive: ProactiveConfig,
    #[serde(default)]
    pub briefing: BriefingConfig,
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
    #[serde(default)]
    pub browser: BrowserSettings,
    #[serde(default)]
    pub ambient: AmbientConfig,
    #[serde(default)]
    pub workwindow: WorkWindowConfig,
    #[serde(default)]
    pub consolidate: ConsolidateConfig,
}

/// `[consolidate]` — promoting episodes into the knowledge graph.
///
/// The pass that makes `ambient.retain_days` a promotion deadline instead of a
/// plain delete: durable facts are lifted out of episodes before the episodes
/// themselves are swept. Needs `[llm.small]`, and runs only in the idle work
/// window — nothing here is time-sensitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidateConfig {
    #[serde(default)]
    pub enabled: bool,
    /// How often to check for a batch, in seconds. The work window gates it
    /// anyway, so this only sets how soon after going idle it starts.
    #[serde(default = "default_consolidate_poll_secs")]
    pub poll_secs: u64,
    /// Episodes per pass. Large enough that facts spanning a few entries are
    /// visible together; small enough to fit a 2B's context alongside the
    /// instruction.
    #[serde(default = "default_consolidate_batch")]
    pub batch_size: usize,
    #[serde(default = "default_consolidate_max_tokens")]
    pub max_tokens: u32,
    /// Mine ambient observations for facts, not just conversation.
    ///
    /// This is the setting with a real trade behind it. Observations are the
    /// high-volume source and the only one with an expiry, so they are the
    /// reason this pass exists — but they are also descriptions of screens
    /// whose contents other people choose. Off means the graph only ever
    /// learns from what the user said and did.
    #[serde(default = "default_true")]
    pub from_observations: bool,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        ConsolidateConfig {
            enabled: false,
            poll_secs: default_consolidate_poll_secs(),
            batch_size: default_consolidate_batch(),
            max_tokens: default_consolidate_max_tokens(),
            from_observations: true,
        }
    }
}

/// `[workwindow]` — when background work may use the GPU.
///
/// See `crate::workwindow`. The one setting that matters in practice is
/// `foreign_vram_budget_mb`: it is what makes the assistant get out of the way
/// when you launch something that wants the card.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkWindowConfig {
    /// Seconds of user inactivity before deferrable background work starts.
    /// Longer than the model's idle-unload threshold on purpose: unloading is
    /// cheap to get wrong, starting to work is not.
    #[serde(default = "default_ww_after_secs")]
    pub after_secs: i64,
    /// VRAM in MiB that other processes may hold before the window closes. A
    /// desktop compositor holds a few hundred MB on any machine, so this is not
    /// zero; a game holds gigabytes, which is what it is here to notice.
    #[serde(default = "default_foreign_vram_budget_mb")]
    pub foreign_vram_budget_mb: u64,
    /// Our own expected VRAM footprint, subtracted from the total reading.
    /// Without it the planner's own 11 GB reads as foreign pressure and the
    /// window never opens.
    #[serde(default = "default_own_vram_mb")]
    pub own_vram_mb: u64,
}

impl Default for WorkWindowConfig {
    fn default() -> Self {
        WorkWindowConfig {
            after_secs: default_ww_after_secs(),
            foreign_vram_budget_mb: default_foreign_vram_budget_mb(),
            own_vram_mb: default_own_vram_mb(),
        }
    }
}

/// `[ambient]` — the screen index.
///
/// Off by default, and it should be: an assistant that photographs your screen
/// on a timer is a thing you switch on deliberately, not something you discover
/// in a changelog. It also needs the vision tier (`[llm.small]`) to be on;
/// without it there is nothing to read the frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AmbientConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between captures. Capture is cheap; interpretation is not, and
    /// the change filter drops most frames anyway, so this can be short.
    #[serde(default = "default_sample_secs")]
    pub sample_secs: u64,
    /// How often the interpreter checks for work.
    #[serde(default = "default_interpret_poll_secs")]
    pub interpret_poll_secs: u64,
    /// Width the frame is scaled to before it reaches the model. 1024 is a
    /// reasonable ceiling for a 2B VLM; smaller loses small text, larger costs
    /// context for detail the model will not use.
    #[serde(default = "default_ambient_max_width")]
    pub max_width: u32,
    /// Hamming distance (0-64) above which a frame counts as a new scene.
    /// Lower indexes more and repeats more; higher misses short-lived screens.
    #[serde(default = "default_change_threshold")]
    pub change_threshold: u32,
    /// Frames held between capture and interpretation. Bounded, in memory,
    /// never on disk.
    #[serde(default = "default_queue_len")]
    pub queue_len: usize,
    /// Interpret while the user is active, rather than waiting for an idle
    /// machine. True is the useful setting: frames are produced *because* the
    /// user is working, and waiting for idleness would discard most of them.
    /// GPU pressure and live turns still hold it back either way.
    #[serde(default = "default_true")]
    pub interpret_while_active: bool,
    /// Token budget for one frame's summary.
    #[serde(default = "default_ambient_max_tokens")]
    pub max_tokens: u32,
    /// Salience for stored observations. Deliberately below a conversation's:
    /// what the user said should outrank what was on screen behind them.
    #[serde(default = "default_ambient_salience")]
    pub salience: f32,
    /// Days to keep observations. 0 disables expiry.
    #[serde(default = "default_retain_days")]
    pub retain_days: u32,
}

impl Default for AmbientConfig {
    fn default() -> Self {
        AmbientConfig {
            enabled: false,
            sample_secs: default_sample_secs(),
            interpret_poll_secs: default_interpret_poll_secs(),
            max_width: default_ambient_max_width(),
            change_threshold: default_change_threshold(),
            queue_len: default_queue_len(),
            interpret_while_active: true,
            max_tokens: default_ambient_max_tokens(),
            salience: default_ambient_salience(),
            retain_days: default_retain_days(),
        }
    }
}

/// `[browser]` — Delphi's Chrome-over-CDP web browser. All optional; defaults
/// launch a dedicated Chrome profile (the user signs into sites there once,
/// since Chrome forbids remote-debugging the real default profile).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSettings {
    /// Path to chrome.exe. Empty → auto-detect common install locations.
    #[serde(default)]
    pub chrome_path: String,
    /// Dedicated profile dir. Empty → %LOCALAPPDATA%\oracle\chrome.
    #[serde(default)]
    pub user_data_dir: String,
    /// Remote-debugging port (0 → 9222).
    #[serde(default)]
    pub port: u16,
    /// Run Chrome without a visible window.
    #[serde(default)]
    pub headless: bool,
}

impl BrowserSettings {
    /// Map to the runtime browser config, filling platform defaults.
    pub fn to_browser_config(&self) -> crate::browser::BrowserConfig {
        let mut cfg = crate::browser::BrowserConfig::default();
        if !self.chrome_path.trim().is_empty() {
            cfg.chrome_path = self.chrome_path.clone();
        }
        if !self.user_data_dir.trim().is_empty() {
            cfg.user_data_dir = self.user_data_dir.clone();
        }
        if self.port != 0 {
            cfg.port = self.port;
        }
        cfg.headless = self.headless;
        cfg
    }
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
    /// `[llm.small]` — the resident small/vision tier. See [`SmallLlmConfig`].
    #[serde(default)]
    pub small: SmallLlmConfig,
}

/// `[llm.small]` — the second, always-warm model tier.
///
/// The 14B planner is the wrong tool for continuous background work: it holds
/// 10-12 GB and is only worth loading for a real turn. But the ambient index and
/// the consolidation pass need a model that is *always there*, cheap to call,
/// and never worth unloading. That is a different model, not a different mode of
/// the same one.
///
/// A 2B-class VLM at Q4 is ~2.5 GB resident. That is a defensible standing cost
/// in a way that 11 GB is not, and it is what lets the big model stay strictly
/// on-demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmallLlmConfig {
    /// Off by default: this is a second model download and a second server.
    #[serde(default)]
    pub enabled: bool,
    /// "mock", or an http(s) URL to a second llama.cpp server. Must not be the
    /// same port as `llm.backend` — they are two separate processes.
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub model: String,
    /// Background summaries are short by construction; a large budget here just
    /// invites the small model to ramble.
    #[serde(default = "default_small_max_tokens")]
    pub max_tokens: u32,
    /// Low by default. This tier extracts and summarizes rather than converses,
    /// and creativity in an index is called a hallucination.
    #[serde(default = "default_small_temperature")]
    pub temperature: f32,
    /// Keep this tier loaded through idle. The whole point of a small model is
    /// that it is cheap enough to leave running; set false only to debug.
    #[serde(default = "default_true")]
    pub resident: bool,
}

impl Default for SmallLlmConfig {
    fn default() -> Self {
        SmallLlmConfig {
            enabled: false,
            backend: String::new(),
            model: String::new(),
            max_tokens: default_small_max_tokens(),
            temperature: default_small_temperature(),
            resident: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub db_path: String,
    pub retrieve_limit: usize,
    /// Inject relevant memory into the prompt automatically each turn, instead
    /// of waiting for the model to call `memory.recall` itself.
    #[serde(default = "default_true")]
    pub auto_recall: bool,
    /// Write each completed turn to memory automatically.
    #[serde(default = "default_true")]
    pub auto_record: bool,
    /// How many recalled episodes to inject at most.
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    /// Minimum retrieval score worth injecting; weak matches cost context and
    /// invite spurious connections.
    #[serde(default = "default_recall_min_score")]
    pub recall_min_score: f32,
    /// `[memory.embedder]` — the semantic embedding sidecar.
    #[serde(default)]
    pub embedder: EmbedderConfig,
}

/// `[memory.embedder]` — what turns memory from keyword matching into recall.
///
/// The built-in `HashEmbedder` matches on tokens, so "the borrow checker
/// complaint in dispatch.rs" does not retrieve "lifetime error in the
/// dispatcher". Pointing this at a llama.cpp sidecar serving BGE-small
/// (`--embedding --pooling mean`) is what makes retrieval semantic.
///
/// It is a third supervised child rather than an in-process ONNX Runtime: the
/// supervision, restart and logging already exist, and `oracle-core` keeps a
/// native-dependency surface of zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedderConfig {
    /// Off = the offline hash embedder. Retrieval still works; it is just
    /// lexical.
    #[serde(default)]
    pub enabled: bool,
    /// Sidecar root, e.g. "http://127.0.0.1:8082". Its own port: this is a
    /// third llama.cpp server, not a route on an existing one.
    #[serde(default)]
    pub backend: String,
    /// Model name, which doubles as the recorded vector-space id. Changing it
    /// makes existing rows vector-stale by design — see memory::embed.
    #[serde(default = "default_embed_model")]
    pub model: String,
    /// Expected output width. A mismatch is refused rather than stored, because
    /// two widths in one column turn cosine into a length check.
    #[serde(default = "default_embed_dim")]
    pub dim: usize,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        EmbedderConfig {
            enabled: false,
            backend: String::new(),
            model: default_embed_model(),
            dim: default_embed_dim(),
        }
    }
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
    /// Put the focused window (and a few other open ones) into the prompt each
    /// turn, so "close this" and "what is this error" have a referent.
    #[serde(default = "default_true")]
    pub screen_context: bool,
    /// How many non-focused windows to list alongside it.
    #[serde(default = "default_screen_other_windows")]
    pub screen_other_windows: usize,
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
    /// Unload the supervised LLM after this many seconds with no activity, and
    /// reload it on the next turn. 0 disables it (the model stays resident).
    ///
    /// The wake-word recognizer is a separate, small process, so nothing is
    /// lost by dropping the model between conversations -- only the VRAM.
    #[serde(default = "default_idle_unload_secs")]
    pub idle_unload_secs: i64,
    /// How long to wait for llama-server to report healthy after a reload.
    #[serde(default = "default_llm_ready_timeout_secs")]
    pub llm_ready_timeout_secs: u64,
    /// Launch the small/vision tier's llama-server as a second managed child.
    ///
    /// Separate from `autostart_llm` because the two tiers are two processes on
    /// two ports with two model files; one being supervised says nothing about
    /// the other. Ignored unless `[llm.small] enabled = true`.
    #[serde(default)]
    pub autostart_small_llm: bool,
    /// The small tier's server program. Empty = reuse `llm_program`, which is
    /// the common case: the same llama-server binary, a different model.
    #[serde(default)]
    pub small_llm_program: String,
    /// Arguments for the small tier's server (its own model path and port).
    #[serde(default)]
    pub small_llm_args: Vec<String>,
    /// Readiness timeout for the small tier. Lower than the big model's because
    /// a 2B loads in seconds; if it is not up by then something is wrong.
    #[serde(default = "default_small_ready_timeout_secs")]
    pub small_llm_ready_timeout_secs: u64,
    /// Launch the embedding sidecar as a third managed child.
    #[serde(default)]
    pub autostart_embedder: bool,
    /// The embedder's server program. Empty = reuse `llm_program`; a BGE GGUF
    /// is served by the same llama-server binary with `--embedding`.
    #[serde(default)]
    pub embedder_program: String,
    /// Arguments for the embedding sidecar (model path, its own port,
    /// `--embedding`, `--pooling mean`).
    #[serde(default)]
    pub embedder_args: Vec<String>,
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
            autostart_small_llm: false,
            small_llm_program: String::new(),
            small_llm_args: Vec::new(),
            small_llm_ready_timeout_secs: default_small_ready_timeout_secs(),
            autostart_embedder: false,
            embedder_program: String::new(),
            embedder_args: Vec::new(),
            browser: default_browser(),
            idle_unload_secs: default_idle_unload_secs(),
            llm_ready_timeout_secs: default_llm_ready_timeout_secs(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_screen_other_windows() -> usize {
    6
}
fn default_recall_limit() -> usize {
    5
}
fn default_recall_min_score() -> f32 {
    0.15
}
/// Which Chromium-based browser hosts the chromeless HUD window by default.
///
/// Edge ships with Windows, so it is the one browser guaranteed present there.
/// It is a rarity on macOS and Linux, where Chrome is far likelier to exist, so
/// defaulting to Edge everywhere meant the first candidate never resolved.
/// Ten minutes: long enough to cover a pause mid-task, short enough that an
/// afternoon away gives the GPU back.
fn default_idle_unload_secs() -> i64 {
    600
}
/// Normalize a backend URL for comparison: trailing slashes and case in the
/// host must not hide a port collision.
fn norm_backend(b: &str) -> String {
    b.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn default_consolidate_poll_secs() -> u64 {
    300
}
fn default_consolidate_batch() -> usize {
    12
}
fn default_consolidate_max_tokens() -> u32 {
    400
}
fn default_ww_after_secs() -> i64 {
    900
}
fn default_foreign_vram_budget_mb() -> u64 {
    1024
}
fn default_own_vram_mb() -> u64 {
    3_000
}
fn default_sample_secs() -> u64 {
    45
}
fn default_interpret_poll_secs() -> u64 {
    5
}
fn default_ambient_max_width() -> u32 {
    1024
}
fn default_change_threshold() -> u32 {
    6
}
fn default_queue_len() -> usize {
    24
}
fn default_ambient_max_tokens() -> u32 {
    120
}
fn default_ambient_salience() -> f32 {
    0.25
}
fn default_retain_days() -> u32 {
    21
}
fn default_embed_model() -> String {
    "bge-small-en-v1.5".into()
}
fn default_embed_dim() -> usize {
    crate::memory::embed::EMBED_DIM
}
fn default_small_max_tokens() -> u32 {
    512
}
fn default_small_temperature() -> f32 {
    0.2
}
fn default_small_ready_timeout_secs() -> u64 {
    60
}
fn default_llm_ready_timeout_secs() -> u64 {
    90
}
fn default_browser() -> String {
    if cfg!(windows) {
        "edge".into()
    } else {
        "chrome".into()
    }
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
            small: SmallLlmConfig::default(),
        }
    }
}
/// Whether and when Pythia may speak unprompted.
///
/// Off by default: an assistant that starts talking on its own is a behaviour
/// change the user should opt into, not discover.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProactiveConfig {
    #[serde(default)]
    pub enabled: bool,
    /// How often to poll the triggers, in seconds.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Announce a calendar event this many minutes before it starts.
    #[serde(default = "default_lead_minutes")]
    pub lead_minutes: i64,
    /// Watch the calendar for upcoming events.
    #[serde(default = "default_true")]
    pub calendar: bool,
    /// Watch for unread mail matching `mail_query`.
    #[serde(default)]
    pub mail: bool,
    /// Gmail search syntax. Deliberately narrow by default: every inbox message
    /// is noise, a starred one from a real person is not.
    #[serde(default = "default_mail_query")]
    pub mail_query: String,
    /// Local hour quiet hours begin (0-23).
    #[serde(default = "default_quiet_from")]
    pub quiet_from_hour: u32,
    /// Local hour quiet hours end (0-23).
    #[serde(default = "default_quiet_until")]
    pub quiet_until_hour: u32,
    /// Don't repeat the same nudge within this many seconds.
    #[serde(default = "default_repeat_after")]
    pub repeat_after_secs: i64,
    /// Ceiling on nudges spoken in any rolling hour.
    #[serde(default = "default_max_per_hour")]
    pub max_per_hour: usize,
    /// Process names to announce when they finish, matched case-insensitively
    /// by substring (e.g. ["cargo", "MSBuild", "ffmpeg"]). Needs actd.
    ///
    /// This is the trigger class a cloud assistant cannot have.
    #[serde(default)]
    pub watch_processes: Vec<String>,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        ProactiveConfig {
            enabled: false,
            poll_secs: default_poll_secs(),
            lead_minutes: default_lead_minutes(),
            calendar: true,
            mail: false,
            mail_query: default_mail_query(),
            quiet_from_hour: default_quiet_from(),
            quiet_until_hour: default_quiet_until(),
            repeat_after_secs: default_repeat_after(),
            max_per_hour: default_max_per_hour(),
            watch_processes: Vec::new(),
        }
    }
}

fn default_poll_secs() -> u64 {
    60
}
fn default_lead_minutes() -> i64 {
    10
}
fn default_mail_query() -> String {
    "is:unread is:starred".into()
}
fn default_quiet_from() -> u32 {
    22
}
fn default_quiet_until() -> u32 {
    8
}
fn default_repeat_after() -> i64 {
    6 * 3600
}
fn default_max_per_hour() -> usize {
    4
}

/// "While you were away" catch-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BriefingConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Only brief after an absence at least this long. Below it there is
    /// nothing to catch up on and the pause is just rude.
    #[serde(default = "default_brief_after")]
    pub after_secs: i64,
    /// Don't brief twice inside this window, however long the gaps are.
    #[serde(default = "default_brief_cooldown")]
    pub cooldown_secs: i64,
    /// Include unread mail that arrived during the absence.
    #[serde(default = "default_true")]
    pub include_mail: bool,
    /// Include calendar events starting within `lookahead_minutes`.
    #[serde(default = "default_true")]
    pub include_calendar: bool,
    #[serde(default = "default_brief_lookahead")]
    pub lookahead_minutes: i64,
}

impl Default for BriefingConfig {
    fn default() -> Self {
        BriefingConfig {
            enabled: true,
            after_secs: default_brief_after(),
            cooldown_secs: default_brief_cooldown(),
            include_mail: true,
            include_calendar: true,
            lookahead_minutes: default_brief_lookahead(),
        }
    }
}

/// 20 minutes: a coffee, a meeting, lunch. Shorter than this and you did not
/// really go anywhere.
fn default_brief_after() -> i64 {
    1200
}
fn default_brief_cooldown() -> i64 {
    1800
}
fn default_brief_lookahead() -> i64 {
    120
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig {
            db_path: "oracle.db".into(),
            retrieve_limit: 6,
            auto_recall: true,
            auto_record: true,
            recall_limit: default_recall_limit(),
            recall_min_score: default_recall_min_score(),
            embedder: EmbedderConfig::default(),
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
        AgentSettings {
            step_budget: 12,
            screen_context: true,
            screen_other_windows: default_screen_other_windows(),
        }
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
        self.validate_small_tier()?;
        self.validate_embedder()?;
        self.validate_ambient()?;
        self.validate_consolidate()?;
        Ok(())
    }

    /// Rules for `[consolidate]`.
    fn validate_consolidate(&self) -> Result<(), ConfigError> {
        let c = &self.consolidate;
        if !c.enabled {
            return Ok(());
        }
        if !self.llm.small.enabled {
            return Err(ConfigError::Invalid(
                "consolidate.enabled = true requires [llm.small] enabled = true — \
                 the extraction pass runs on the small tier"
                    .into(),
            ));
        }
        if c.batch_size == 0 {
            return Err(ConfigError::Invalid(
                "consolidate.batch_size must be at least 1".into(),
            ));
        }
        // A batch that cannot fit the small tier's context produces a truncated
        // prompt and silently wrong extraction, which is worse than refusing.
        if c.batch_size > 100 {
            return Err(ConfigError::Invalid(
                "consolidate.batch_size above 100 will not fit a small model's context".into(),
            ));
        }
        // The pairing that quietly loses data: observations expire on a timer,
        // and if nothing promotes them first they are simply gone.
        if self.ambient.enabled && self.ambient.retain_days > 0 && !c.from_observations {
            tracing::warn!(
                "[config] ambient observations expire after {} days but \
                 consolidate.from_observations is false — nothing will promote them first",
                self.ambient.retain_days
            );
        }
        Ok(())
    }

    /// Rules for `[ambient]`.
    fn validate_ambient(&self) -> Result<(), ConfigError> {
        let a = &self.ambient;
        if !a.enabled {
            return Ok(());
        }
        // The hard one: without the vision tier there is no model to read the
        // frames, so the sampler would capture the screen on a timer and queue
        // it for nobody. That is all of the privacy cost and none of the
        // benefit, so it fails at load rather than running.
        if !self.llm.small.enabled {
            return Err(ConfigError::Invalid(
                "ambient.enabled = true requires [llm.small] enabled = true — \
                 the vision tier is what reads the frames"
                    .into(),
            ));
        }
        if a.sample_secs == 0 {
            return Err(ConfigError::Invalid(
                "ambient.sample_secs must be at least 1".into(),
            ));
        }
        if a.change_threshold > 64 {
            return Err(ConfigError::Invalid(
                "ambient.change_threshold is a Hamming distance over 64 bits; max is 64".into(),
            ));
        }
        if a.queue_len == 0 {
            return Err(ConfigError::Invalid(
                "ambient.queue_len must be at least 1".into(),
            ));
        }
        if !(0.0..=1.0).contains(&a.salience) {
            return Err(ConfigError::Invalid(
                "ambient.salience must be in 0.0..=1.0".into(),
            ));
        }
        Ok(())
    }

    /// Rules for `[memory.embedder]`.
    ///
    /// Same port-collision reasoning as the small tier, extended to three
    /// servers: an embedder pointed at the planner's port would send embedding
    /// requests to a chat model, which answers with prose rather than an error.
    fn validate_embedder(&self) -> Result<(), ConfigError> {
        let e = &self.memory.embedder;
        if !e.enabled {
            return Ok(());
        }
        if e.backend.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "memory.embedder.enabled = true requires memory.embedder.backend".into(),
            ));
        }
        if !e.backend.starts_with("http") {
            return Err(ConfigError::Invalid(
                "memory.embedder.backend must be an http(s) URL".into(),
            ));
        }
        for (label, other) in [
            ("llm.backend", &self.llm.backend),
            ("llm.small.backend", &self.llm.small.backend),
        ] {
            if *other != "mock" && norm_backend(&e.backend) == norm_backend(other) {
                return Err(ConfigError::Invalid(format!(
                    "memory.embedder.backend must not be the same endpoint as {label} — \
                     the embedder is its own llama.cpp server on its own port"
                )));
            }
        }
        if e.dim == 0 {
            return Err(ConfigError::Invalid(
                "memory.embedder.dim must be non-zero".into(),
            ));
        }
        if self.supervise.autostart_embedder {
            let program = if self.supervise.embedder_program.trim().is_empty() {
                self.supervise.llm_program.trim()
            } else {
                self.supervise.embedder_program.trim()
            };
            if program.is_empty() {
                return Err(ConfigError::Invalid(
                    "supervise.autostart_embedder = true needs supervise.embedder_program \
                     (or supervise.llm_program to fall back to)"
                        .into(),
                ));
            }
            if self.supervise.embedder_args.is_empty() {
                return Err(ConfigError::Invalid(
                    "supervise.autostart_embedder = true needs supervise.embedder_args \
                     (at least the model path, its own --port, and --embedding)"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Rules specific to `[llm.small]`.
    ///
    /// The sharp one is the shared-backend check. Two llama-server processes on
    /// one port do not error — the second simply fails to bind and the
    /// supervisor restart-loops it forever while every "small" request is
    /// silently answered by the 14B. That looks like the tier working, at 11 GB
    /// resident, which is the exact failure this whole feature exists to avoid.
    fn validate_small_tier(&self) -> Result<(), ConfigError> {
        let small = &self.llm.small;
        if !small.enabled {
            return Ok(());
        }
        if small.backend.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "llm.small.enabled = true requires llm.small.backend".into(),
            ));
        }
        if small.backend != "mock" && !small.backend.starts_with("http") {
            return Err(ConfigError::Invalid(
                "llm.small.backend must be \"mock\" or an http(s) URL".into(),
            ));
        }
        if small.backend != "mock"
            && norm_backend(&small.backend) == norm_backend(&self.llm.backend)
        {
            return Err(ConfigError::Invalid(
                "llm.small.backend must not be the same endpoint as llm.backend — \
                 the two tiers are two servers on two ports"
                    .into(),
            ));
        }
        if !(0.0..=2.0).contains(&small.temperature) {
            return Err(ConfigError::Invalid(
                "llm.small.temperature must be in 0.0..=2.0".into(),
            ));
        }
        if self.supervise.autostart_small_llm {
            let program = if self.supervise.small_llm_program.trim().is_empty() {
                self.supervise.llm_program.trim()
            } else {
                self.supervise.small_llm_program.trim()
            };
            if program.is_empty() {
                return Err(ConfigError::Invalid(
                    "supervise.autostart_small_llm = true needs supervise.small_llm_program \
                     (or supervise.llm_program to fall back to)"
                        .into(),
                ));
            }
            if self.supervise.small_llm_args.is_empty() {
                return Err(ConfigError::Invalid(
                    "supervise.autostart_small_llm = true needs supervise.small_llm_args \
                     (at least the model path and its own --port)"
                        .into(),
                ));
            }
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

    #[cfg(test)]
    mod proactive_config_tests {
        use super::*;

        #[test]
        fn proactive_is_off_unless_asked_for() {
            // Speaking unprompted is a behaviour change the user opts into, not
            // one they discover.
            assert!(!ProactiveConfig::default().enabled);
        }

        #[test]
        fn a_config_without_a_proactive_section_still_parses() {
            // Existing oracle.toml files predate this section.
            let cfg: Config = toml::from_str("").expect("empty config should parse");
            assert!(!cfg.proactive.enabled);
            assert_eq!(cfg.proactive.quiet_from_hour, 22);
            assert_eq!(cfg.proactive.max_per_hour, 4);
        }

        #[test]
        fn partial_proactive_section_keeps_the_other_defaults() {
            let cfg: Config = toml::from_str("[proactive]\nenabled = true\nmax_per_hour = 1\n")
                .expect("partial section should parse");
            assert!(cfg.proactive.enabled);
            assert_eq!(cfg.proactive.max_per_hour, 1);
            assert_eq!(cfg.proactive.quiet_until_hour, 8, "untouched default");
            assert!(cfg.proactive.calendar);
            assert!(!cfg.proactive.mail, "mail is opt-in on top of proactive");
        }
    }
}
