//! Deterministic offline LLM for tests and the no-GPU demo REPL.
//!
//! It runs a tiny scripted "policy" so the *agent loop* — planning, tool
//! dispatch, result feedback, cancellation — can be exercised end-to-end with
//! no model weights. The script is keyed on simple substring rules; it emits
//! real `ToolCall` deltas with `$result.N` references so the DAG dispatcher is
//! genuinely tested.

use super::*;
use futures::stream::{self, StreamExt};
use std::sync::Mutex;
use std::time::Duration;

/// A scripted step the mock can produce.
#[derive(Clone)]
enum Script {
    Say(String),
    Tool { id: u32, name: String, args: Value },
}

/// MockLlm plays a different script on each successive `generate` call, so a
/// multi-round agent loop (call tools, observe, then speak) can be simulated.
pub struct MockLlm {
    rounds: Mutex<std::collections::VecDeque<Vec<Script>>>,
    /// Per-token delay to make streaming/cancellation observable in tests.
    step_delay: Duration,
}

impl MockLlm {
    /// A canned two-round conversation for the demo: round 1 fans out two
    /// independent tools plus one dependent draft; round 2 speaks a summary.
    pub fn demo() -> Self {
        let round1 = vec![
            Script::Say("Let me check that for you. ".into()),
            Script::Tool {
                id: 1,
                name: "gmail.search".into(),
                args: serde_json::json!({"query": "from:advisor is:unread"}),
            },
            Script::Tool {
                id: 2,
                name: "calendar.free_slots".into(),
                args: serde_json::json!({"date": "2026-08-18", "duration_min": 30, "window": "afternoon"}),
            },
            Script::Tool {
                id: 3,
                name: "gmail.create_draft".into(),
                args: serde_json::json!({
                    "to": "$result.1.top_sender",
                    "subject": "Re: your email",
                    "body": "Proposing a time."
                }),
            },
            Script::Tool {
                id: 4,
                name: "home_assistant.light".into(),
                args: serde_json::json!({"room": "bedroom", "brightness_pct": 30}),
            },
        ];
        let round2 = vec![Script::Say(
            "You have one unread email from your advisor. I drafted a reply offering 2 PM tomorrow, and dimmed the bedroom lights to 30%.".into(),
        )];
        let mut q = std::collections::VecDeque::new();
        q.push_back(round1);
        q.push_back(round2);
        MockLlm {
            rounds: Mutex::new(q),
            step_delay: Duration::from_millis(1),
        }
    }

    /// A trivial one-shot that just speaks — used to test the plain path.
    pub fn saying(text: &str) -> Self {
        let mut q = std::collections::VecDeque::new();
        q.push_back(vec![Script::Say(text.into())]);
        MockLlm {
            rounds: Mutex::new(q),
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
        let script = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Script::Say("Done.".into())]);
        let delay = self.step_delay;

        let s = stream::unfold(
            (script.into_iter(), false, cancel),
            move |(mut it, mut had_tool, cancel)| async move {
                if cancel.is_cancelled() {
                    return Some((
                        LlmDelta::Done {
                            stop_reason: StopReason::Cancelled,
                        },
                        (it, had_tool, cancel),
                    ));
                }
                match it.next() {
                    Some(Script::Say(t)) => {
                        // stream word-by-word so TTS chunking + cancellation are exercised
                        tokio::time::sleep(delay).await;
                        Some((LlmDelta::Text(t), (it, had_tool, cancel)))
                    }
                    Some(Script::Tool { id, name, args }) => {
                        had_tool = true;
                        tokio::time::sleep(delay).await;
                        Some((
                            LlmDelta::ToolCall { id, name, args },
                            (it, had_tool, cancel),
                        ))
                    }
                    None => {
                        let reason = if had_tool {
                            StopReason::ToolCalls
                        } else {
                            StopReason::Stop
                        };
                        Some((
                            LlmDelta::Done {
                                stop_reason: reason,
                            },
                            (it, had_tool, cancel),
                        ))
                    }
                }
            },
        );

        // Terminate after the first Done.
        let s = s
            .scan(false, |done, d| {
                if *done {
                    return futures::future::ready(None);
                }
                if matches!(d, LlmDelta::Done { .. }) {
                    *done = true;
                }
                futures::future::ready(Some(d))
            })
            .boxed();
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> LlmRequest {
        LlmRequest {
            system: String::new(),
            messages: vec![],
            tools: Value::Array(vec![]),
            max_tokens: 256,
            temperature: 0.7,
        }
    }

    #[tokio::test]
    async fn demo_first_round_emits_tools_then_done() {
        let m = MockLlm::demo();
        let stream = m.generate(req(), CancellationToken::new()).await.unwrap();
        let deltas: Vec<_> = stream.collect().await;
        let tool_calls = deltas
            .iter()
            .filter(|d| matches!(d, LlmDelta::ToolCall { .. }))
            .count();
        assert_eq!(tool_calls, 4);
        assert!(matches!(
            deltas.last().unwrap(),
            LlmDelta::Done {
                stop_reason: StopReason::ToolCalls
            }
        ));
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
