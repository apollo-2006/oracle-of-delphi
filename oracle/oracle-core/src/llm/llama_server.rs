//! Backend for a llama.cpp `server` (OpenAI-compatible `/v1/chat/completions`).
//!
//! This is the real inference path: point it at a locally-running
//! `llama-server -m qwen2.5-14b-instruct-q5_k.gguf --host 127.0.0.1 --port 8080`
//! built with the HIP backend. We stream SSE deltas, parse tool calls out of
//! the OpenAI `tool_calls` field, and honor cancellation by dropping the
//! response body (which aborts the HTTP request, which llama-server treats as a
//! client disconnect and stops decoding).
//!
//! Network access is not available in unit tests, so this module is validated
//! by its pure parsing helpers rather than a live round-trip.

use super::*;
use futures::stream::{self, StreamExt};

pub struct LlamaServer {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl LlamaServer {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        LlamaServer {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Map our structured request to the OpenAI chat body llama-server expects.
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut messages = vec![serde_json::json!({"role":"system","content": req.system})];
        for m in &req.messages {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            messages.push(serde_json::json!({"role": role, "content": m.content}));
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": true,
        });
        // Attach tools only when present; this switches the server into
        // grammar-constrained tool decoding.
        if req.tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            let tools: Vec<Value> = req
                .tools
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t["parameters"],
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
        }
        body
    }
}

/// Parse one SSE `data:` line body into a delta. Returns `None` for keepalives
/// and the `[DONE]` sentinel (handled by the caller).
pub(crate) fn parse_sse_chunk(json: &str) -> Option<LlmDelta> {
    let v: Value = serde_json::from_str(json).ok()?;
    let choice = v.get("choices")?.get(0)?;
    let delta = choice.get("delta")?;

    // Tool call?
    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        if let Some(tc) = tcs.first() {
            let id = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .cloned()
                .and_then(|a| match a {
                    // llama-server may send arguments as a JSON string.
                    Value::String(s) => serde_json::from_str(&s).ok(),
                    other => Some(other),
                })
                .unwrap_or(Value::Object(Default::default()));
            return Some(LlmDelta::ToolCall { id, name, args });
        }
    }

    // Plain text content?
    if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            return Some(LlmDelta::Text(t.to_string()));
        }
    }

    // Finish reason on the tail chunk.
    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        let reason = match fr {
            "tool_calls" => StopReason::ToolCalls,
            "length" => StopReason::Length,
            _ => StopReason::Stop,
        };
        return Some(LlmDelta::Done {
            stop_reason: reason,
        });
    }
    None
}

#[async_trait]
impl Llm for LlamaServer {
    async fn generate(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> anyhow::Result<BoxStream<'static, LlmDelta>> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = self.build_body(&req);
        let resp = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let byte_stream = Box::pin(resp.bytes_stream());
        // Turn the byte stream into line-buffered SSE deltas, honoring cancel.
        let s = stream::unfold(
            (byte_stream, String::new(), cancel, false),
            move |(mut bytes, mut buf, cancel, mut finished)| async move {
                loop {
                    if finished {
                        return None;
                    }
                    if cancel.is_cancelled() {
                        finished = true;
                        return Some((
                            LlmDelta::Done {
                                stop_reason: StopReason::Cancelled,
                            },
                            (bytes, buf, cancel, finished),
                        ));
                    }
                    // Emit any complete SSE event already in the buffer.
                    if let Some(pos) = buf.find('\n') {
                        let line = buf[..pos].trim().to_string();
                        buf.drain(..=pos);
                        if let Some(data) = line.strip_prefix("data:") {
                            let data = data.trim();
                            if data == "[DONE]" {
                                finished = true;
                                return Some((
                                    LlmDelta::Done {
                                        stop_reason: StopReason::Stop,
                                    },
                                    (bytes, buf, cancel, finished),
                                ));
                            }
                            if let Some(delta) = parse_sse_chunk(data) {
                                if matches!(delta, LlmDelta::Done { .. }) {
                                    finished = true;
                                }
                                return Some((delta, (bytes, buf, cancel, finished)));
                            }
                        }
                        continue;
                    }
                    // Need more bytes.
                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                        }
                        Some(Err(_)) | None => {
                            finished = true;
                            return Some((
                                LlmDelta::Done {
                                    stop_reason: StopReason::Stop,
                                },
                                (bytes, buf, cancel, finished),
                            ));
                        }
                    }
                }
            },
        )
        .boxed();
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let j = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_sse_chunk(j), Some(LlmDelta::Text("Hello".into())));
    }

    #[test]
    fn parses_tool_call_with_string_arguments() {
        let j = r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"cal.free","arguments":"{\"date\":\"2026-08-18\"}"}}]}}]}"#;
        match parse_sse_chunk(j).unwrap() {
            LlmDelta::ToolCall { id, name, args } => {
                assert_eq!(id, 2);
                assert_eq!(name, "cal.free");
                assert_eq!(args["date"], "2026-08-18");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn parses_finish_reason() {
        let j = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(
            parse_sse_chunk(j),
            Some(LlmDelta::Done {
                stop_reason: StopReason::ToolCalls
            })
        );
    }

    #[test]
    fn build_body_omits_tools_when_empty() {
        let ls = LlamaServer::new("http://127.0.0.1:8080", "qwen");
        let body = ls.build_body(&LlmRequest {
            system: "sys".into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "hi".into(),
            }],
            tools: Value::Array(vec![]),
            max_tokens: 128,
            temperature: 0.5,
        });
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }
}
