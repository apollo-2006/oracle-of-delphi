//! Deterministic offline LLM for tests and the no-GPU demo REPL.
//!
//! It plays a scripted "policy" so the *agent loop* — parse, dispatch, observe,
//! speak, cancel — runs with no model weights. Each round emits the grammar
//! protocol as text (one `{"tool":…}` or `{"say":…}` object), exactly as the real
//! grammar-constrained backend would, so the agent's protocol parser is the same
//! code path in tests and in production.

use super::*;
use futures::stream::{self, StreamExt};
use std::sync::Mutex;
use std::time::Duration;

/// MockLlm plays one scripted response per `generate` call, so a multi-step agent
/// loop (call a tool, observe, then speak) can be simulated across rounds.
pub struct MockLlm {
    rounds: Mutex<std::collections::VecDeque<String>>,
    /// Per-emit delay to make streaming/cancellation observable in tests.
    step_delay: Duration,
}

impl MockLlm {
    /// A canned conversation: call two tools across successive rounds (one per
    /// response, as the grammar protocol requires), then speak a summary.
    pub fn demo() -> Self {
        Self::rounds(vec![
            r#"{"tool":"gmail.search","args":{"query":"from:advisor is:unread"}}"#.into(),
            r#"{"tool":"home_assistant.light","args":{"room":"bedroom","brightness_pct":30}}"#.into(),
            r#"{"say":"You have one unread email from your advisor, and I dimmed the bedroom lights."}"#.into(),
        ])
    }

    /// A one-shot that just speaks — the plain no-tool path.
    pub fn saying(text: &str) -> Self {
        Self::rounds(vec![serde_json::json!({ "say": text }).to_string()])
    }

    /// Build from explicit per-round protocol strings.
    pub fn rounds(rounds: Vec<String>) -> Self {
        MockLlm {
            rounds: Mutex::new(rounds.into()),
            step_delay: Duration::from_millis(1),
        }
    }
}

#[async_trait]
impl Llm for MockLlm {
    async fn generate(
        &self,
        _req: LlmRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<BoxStream<'static, LlmDelta>> {
        let reply = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| r#"{"say":"Done."}"#.to_string());
        let delay = self.step_delay;

        // Emit the reply as a few text chunks, then Done — mirroring streaming.
        let chunks: Vec<String> = chunk_text(&reply);
        let s = stream::unfold(
            (chunks.into_iter(), cancel, false),
            move |(mut it, cancel, mut done)| async move {
                if done {
                    return None;
                }
                if cancel.is_cancelled() {
                    done = true;
                    return Some((
                        LlmDelta::Done {
                            stop_reason: StopReason::Cancelled,
                        },
                        (it, cancel, done),
                    ));
                }
                match it.next() {
                    Some(t) => {
                        tokio::time::sleep(delay).await;
                        Some((LlmDelta::Text(t), (it, cancel, done)))
                    }
                    None => {
                        done = true;
                        Some((
                            LlmDelta::Done {
                                stop_reason: StopReason::Stop,
                            },
                            (it, cancel, done),
                        ))
                    }
                }
            },
        )
        .boxed();
        Ok(s)
    }
}

/// Split a reply into a handful of chunks so streaming/cancellation is exercised.
fn chunk_text(s: &str) -> Vec<String> {
    let bytes = s.len();
    if bytes <= 8 {
        return vec![s.to_string()];
    }
    // Three roughly-equal pieces on char boundaries.
    let mut cuts = [bytes / 3, 2 * bytes / 3];
    for c in &mut cuts {
        while !s.is_char_boundary(*c) {
            *c += 1;
        }
    }
    vec![
        s[..cuts[0]].to_string(),
        s[cuts[0]..cuts[1]].to_string(),
        s[cuts[1]..].to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> LlmRequest {
        LlmRequest {
            system: String::new(),
            messages: vec![],
            grammar: None,
            max_tokens: 256,
            temperature: 0.3,
            top_p: LlmRequest::DEFAULT_TOP_P,
            top_k: LlmRequest::DEFAULT_TOP_K,
            min_p: LlmRequest::DEFAULT_MIN_P,
            repeat_penalty: LlmRequest::DEFAULT_REPEAT_PENALTY,
        }
    }

    async fn collect_text(m: &MockLlm) -> String {
        let stream = m.generate(req(), CancellationToken::new()).await.unwrap();
        let deltas: Vec<_> = stream.collect().await;
        deltas
            .iter()
            .filter_map(|d| match d {
                LlmDelta::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn demo_round_one_is_a_tool_call() {
        let m = MockLlm::demo();
        let text = collect_text(&m).await;
        assert!(text.contains(r#""tool":"gmail.search""#));
    }

    #[tokio::test]
    async fn saying_round_is_a_say() {
        let m = MockLlm::saying("hello there");
        let text = collect_text(&m).await;
        assert!(text.contains(r#""say":"hello there""#));
    }

    #[tokio::test]
    async fn cancellation_short_circuits() {
        let m = MockLlm::saying("this is a fairly long sentence that should be cut off");
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled
        let stream = m.generate(req(), cancel).await.unwrap();
        let deltas: Vec<_> = stream.collect().await;
        assert!(matches!(
            deltas.first().unwrap(),
            LlmDelta::Done {
                stop_reason: StopReason::Cancelled
            }
        ));
    }
}
