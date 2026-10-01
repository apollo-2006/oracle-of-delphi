//! Shared wire types for the Oracle of Delphi process mesh.
//!
//! These structs are the typed contract between `oracle-audio` (C++, via a C ABI
//! shim + JSON on the control channel for non-hot-path messages), `oracle-core`,
//! `oracle-actd`, and the HUD gateway. Hot-path audio (PCM, FFT) travels as raw
//! bytes over shared memory; everything here is the *control plane*.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod actd;
pub mod audio;
#[cfg(feature = "transport")]
pub mod transport;

/// A correlation id threaded through a single user turn end-to-end, so audio,
/// core, actd audit logs, and HUD events can all be joined after the fact.
pub type TurnId = Uuid;

/// Control messages from the audio engine up to core. Mirrors the C++
/// `CtrlMsg` enum, but serialized as JSON on the (cold) control socket rather
/// than the shared-memory mailbox used for the <1ms barge-in path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioEvent {
    /// VAD detected speech onset. `t_mono_ns` is the producer's monotonic clock.
    SpeechStart { t_mono_ns: u64 },
    /// VAD endpointed. Utterance is complete; final ASR flush follows.
    SpeechEnd { t_mono_ns: u64 },
    /// User spoke over active TTS. `heard_upto_sample` marks how much of the
    /// assistant's audio actually reached the speaker (see chunk-table mapping).
    BargeIn {
        t_mono_ns: u64,
        stream_id: u64,
        heard_upto_sample: u64,
    },
    /// Incremental transcript. `stable` = committed prefix (LocalAgreement-2).
    Transcript {
        text: String,
        stable: bool,
        t_mono_ns: u64,
    },
    /// Output ring ran dry while TTS text was still pending.
    TtsUnderrun { stream_id: u64 },
}

/// Commands from core down to the audio engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioCommand {
    /// Feed a synthesizable clause into the TTS pipeline for a given stream.
    Speak {
        stream_id: u64,
        clause: String,
        /// True when this is the final clause of an assistant turn.
        final_clause: bool,
    },
    /// Abort synthesis + playback for a stream (barge-in confirmed, or new turn).
    FlushTts { stream_id: u64 },
    /// Resume a stream from a sample position after a false barge-in.
    ResumeTts { stream_id: u64, from_sample: u64 },
    /// Enter/exit lockdown (panic gesture, "freeze").
    SetLockdown { active: bool },
}

impl AudioEvent {
    pub fn monotonic(&self) -> u64 {
        match self {
            AudioEvent::SpeechStart { t_mono_ns }
            | AudioEvent::SpeechEnd { t_mono_ns }
            | AudioEvent::Transcript { t_mono_ns, .. }
            | AudioEvent::BargeIn { t_mono_ns, .. } => *t_mono_ns,
            AudioEvent::TtsUnderrun { .. } => 0,
        }
    }
}

/// Events streamed to the HUD (JSON side of the WebSocket). Binary telemetry
/// frames (FFT/SYS) are defined in `audio.rs` / emitted by the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HudEvent {
    State {
        turn: TurnId,
        state: String,
    },
    Transcript {
        text: String,
        stable: bool,
    },
    Caption {
        text: String,
    },
    Tool {
        id: u32,
        name: String,
        status: ToolStatus,
        detail: Option<String>,
    },
    Sys {
        gpu_util: f32,
        gpu_temp_c: f32,
        vram_mb: u32,
        tok_per_s: f32,
        asr_rtf: f32,
    },
    /// A ready-to-render status line for the System panel (model, backend
    /// health, throughput). Composed by core so the HUD stays a dumb display —
    /// this replaces the never-populated numeric `Sys` telemetry.
    Status {
        text: String,
    },
    /// Speak a completed reply. When `wav_b64` is present it's a base64 WAV that
    /// core synthesized with the local neural voice — the HUD plays it. When it's
    /// absent, the HUD falls back to browser speech on `text`. Either way the HUD
    /// honors its own mute toggle and barge-in.
    Speak {
        text: String,
        wav_b64: Option<String>,
    },
    /// Capability announcement (reply to `HudCommand::Hello`) so the HUD picks
    /// its input path: server-side Whisper vs the browser's speech engine.
    Config {
        /// Local speech recognition (Whisper) is active — the HUD should capture
        /// audio and send it for transcription.
        stt: bool,
        /// Server-side neural TTS is active (informational).
        tts: bool,
        /// The always-on wake-word listener is active (drives the wake chip).
        #[serde(default)]
        wake: bool,
    },
    /// A live (non-final) transcript line from the always-on listener, shown as
    /// a subtle "what I'm hearing" hint. Not a committed user message.
    Interim {
        text: String,
    },
    /// Cut off whatever the Oracle is currently saying (barge-in). The HUD stops
    /// the neural-voice clip and any browser speech immediately.
    StopAudio,
    /// A pending irreversible action awaiting the user's decree. The HUD raises
    /// the Apollo confirmation modal and replies with `HudCommand::Confirm`.
    Confirm {
        request_id: Uuid,
        /// Human-readable description of the action (e.g. "Terminate firefox (pid 1002)").
        prompt: String,
        /// Short risk label ("irreversible", "sensitive") for emphasis.
        severity: String,
    },
    /// Answer to `HudCommand::Search`. Events are broadcast to every connected
    /// HUD, so `id` is what lets the asking window ignore other windows'
    /// results and its own stale ones.
    SearchResults {
        id: u32,
        query: String,
        items: Vec<SearchHit>,
        /// Why the list is empty or partial, when that is worth saying
        /// ("the ambient index is off"), for the HUD to show in place of results.
        #[serde(default)]
        note: Option<String>,
    },
}

/// One memory, as the HUD's search shows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    /// "observation", "conversation" or "action".
    pub kind: String,
    /// The window an observation was read from. Untrusted: a page picks it.
    #[serde(default)]
    pub title: Option<String>,
    pub text: String,
    pub t_unix: i64,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Started,
    Progress,
    Done,
    Error,
}

/// Control messages from the HUD back to core.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HudCommand {
    Mute {
        active: bool,
    },
    Interrupt,
    Confirm {
        request_id: Uuid,
        allow: bool,
    },
    /// A typed message from the HUD text box — drives a conversation turn.
    UserText {
        text: String,
    },
    /// A captured microphone utterance (base64 16 kHz mono WAV). Core transcribes
    /// it with the local recognizer (Whisper) and runs the transcript as a turn.
    Audio {
        wav_b64: String,
    },
    /// The HUD announcing itself on (re)connect. Core replies with a
    /// `HudEvent::Config` so the HUD knows whether server-side STT is active.
    Hello,
    /// Pause/resume the always-on wake-word listener (the wake chip). Pausing
    /// stops the streaming recognizer and releases the microphone.
    SetWake {
        active: bool,
    },
    /// The wake word ("Delphi") was heard while the window may be dismissed.
    /// Core raises a summon flag the native shell polls, bringing the window
    /// back to the foreground hands-free.
    Summon,
    /// The user asked (from the compact corner window) to return to the full
    /// window. Core raises an expand flag the native shell polls.
    Expand,
    /// The reverse of `Expand`: shrink the full window back into the compact
    /// corner panel (e.g. after a mis-click on Expand).
    Retract,
    /// Search memory directly: no planner, no turn, no speech. An empty query
    /// asks for the most recent screen observations instead.
    Search {
        id: u32,
        query: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_speaks_the_json_the_hud_sends_and_reads() {
        // The HUD is TypeScript; these literals are what oracle-hud/src sends
        // and switches on, so a rename here must break this test first.
        let cmd: HudCommand =
            serde_json::from_str(r#"{"type":"search","id":3,"query":"tokio"}"#).unwrap();
        assert!(matches!(cmd, HudCommand::Search { id: 3, ref query } if query == "tokio"));

        let ev = HudEvent::SearchResults {
            id: 3,
            query: "tokio".into(),
            items: vec![SearchHit {
                kind: "observation".into(),
                title: Some("docs.rs".into()),
                text: "tokio::select! docs".into(),
                t_unix: 1,
                score: 0.5,
            }],
            note: None,
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "search_results");
        assert_eq!(v["items"][0]["title"], "docs.rs");
        assert_eq!(v["items"][0]["t_unix"], 1);
    }

    #[test]
    fn audio_event_roundtrips() {
        let e = AudioEvent::BargeIn {
            t_mono_ns: 42,
            stream_id: 7,
            heard_upto_sample: 16000,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("barge_in"));
        assert_eq!(serde_json::from_str::<AudioEvent>(&s).unwrap(), e);
        assert_eq!(e.monotonic(), 42);
    }

    #[test]
    fn command_tag_is_snake_case() {
        let c = AudioCommand::Speak {
            stream_id: 1,
            clause: "hello".into(),
            final_clause: false,
        };
        assert!(serde_json::to_string(&c)
            .unwrap()
            .contains("\"kind\":\"speak\""));
    }
}
