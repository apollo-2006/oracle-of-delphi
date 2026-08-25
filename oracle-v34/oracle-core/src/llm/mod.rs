//! LLM runtime abstraction (architecture §2.1).
//!
//! `oracle-core` doesn't care whether tokens come from an in-process
//! llama.cpp/HIP context or a sidecar HTTP server; it needs a stream of token
//! deltas and a cooperative cancellation signal (for barge-in, §1.2.3).
//!
//! Two backends ship here:
//!   * [`MockLlm`] — deterministic, offline; used in tests and the demo REPL so
//!     the whole agent loop runs without a GPU or model download.
//!   * [`LlamaServer`] — talks to a llama.cpp `server` OpenAI-compatible endpoint.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub mod llama_server;
pub mod mock;

pub use llama_server::LlamaServer;
pub use mock::MockLlm;

/// A single streamed step from the model.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmDelta {
    /// A chunk of assistant-visible text (goes to TTS clause chunker + HUD).
    Text(String),
    /// A completed tool-call request. Emitted whole (grammar-constrained
    /// decoding guarantees the JSON is complete before we surface it).
    ToolCall { id: u32, name: String, args: Value },
    /// End of this generation.
    Done { stop_reason: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    ToolCalls,
    Length,
    Cancelled,
}

/// The prompt handed to the backend. Kept structured so backends can map it to
/// their own chat format; the context manager (§5.1) assembles it.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub system: String,
    /// Ordered conversation turns (already windowed/summarized upstream).
    pub messages: Vec<ChatMessage>,
    /// Tool manifest (JSON schema array). Presence switches on tool decoding.
    pub tools: Value,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Nucleus sampling cutoff. Qwen2.5 wants a tighter 0.8 than llama-server's
    /// permissive 0.95 default — the wide tail is what lets a stray foreign-
    /// language token slip in before the model settles into its answer.
    pub top_p: f32,
    /// Top-k cutoff (Qwen2.5 recommends 20).
    pub top_k: u32,
    /// Minimum-probability floor relative to the top token; a second guard on
    /// the long tail.
    pub min_p: f32,
    /// Repetition penalty (Qwen2.5 recommends ~1.05).
    pub repeat_penalty: f32,
}

/// Qwen2.5-Instruct's recommended sampling. Centralized so every construction
/// site (and the defaults) stay consistent.
impl LlmRequest {
    pub const DEFAULT_TOP_P: f32 = 0.8;
    pub const DEFAULT_TOP_K: u32 = 20;
    pub const DEFAULT_MIN_P: f32 = 0.05;
    pub const DEFAULT_REPEAT_PENALTY: f32 = 1.05;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

#[async_trait]
pub trait Llm: Send + Sync {
    /// Stream deltas. The returned stream must terminate promptly when `cancel`
    /// is triggered, emitting a final `Done{Cancelled}` — this is the barge-in
    /// abort path, and its latency is part of the interrupt budget.
    async fn generate(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<BoxStream<'static, LlmDelta>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_equality() {
        assert_eq!(LlmDelta::Text("hi".into()), LlmDelta::Text("hi".into()));
        assert_ne!(
            LlmDelta::Done {
                stop_reason: StopReason::Stop
            },
            LlmDelta::Done {
                stop_reason: StopReason::Cancelled
            }
        );
    }
}
