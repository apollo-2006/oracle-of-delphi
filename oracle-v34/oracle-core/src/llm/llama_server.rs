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
            // Constrain sampling to Qwen2.5's recommended window. Without these,
            // llama-server's permissive defaults let the long tail through — the
            // cause of the occasional foreign-language preamble before an English
            // answer. top_k/min_p/repeat_penalty are llama.cpp extensions the
            // OpenAI-compatible endpoint accepts alongside the standard fields.
            "top_p": req.top_p,
            "top_k": req.top_k,
            "min_p": req.min_p,
            "repeat_penalty": req.repeat_penalty,
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

/// One raw event parsed from an SSE chunk. Tool calls arrive as *fragments* when
/// llama-server streams with `--jinja` (Qwen's template): the name comes in the
/// first fragment, the arguments dribble in as string pieces across later ones.
/// We surface fragments raw and let the caller accumulate them into whole calls.
#[derive(Debug, PartialEq)]
pub(crate) enum Raw {
    Text(String),
    /// A tool-call fragment: `name` is empty on continuation fragments; `args`
    /// is the raw argument-string piece to concatenate.
    ToolFragment {
        index: u32,
        name: String,
        args: String,
    },
    Finish(StopReason),
}

/// Parse one SSE `data:` line body into a raw event. `None` for keepalives.
pub(crate) fn parse_sse_raw(json: &str) -> Option<Raw> {
    let v: Value = serde_json::from_str(json).ok()?;
    let choice = v.get("choices")?.get(0)?;

    if let Some(delta) = choice.get("delta") {
        // Tool-call fragment?
        if let Some(tc) = delta
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .and_then(|a| a.first())
        {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            // Arguments stream as a JSON *string* to be concatenated.
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or_default()
                .to_string();
            return Some(Raw::ToolFragment { index, name, args });
        }
        // Plain text content?
        if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
            if !t.is_empty() {
                return Some(Raw::Text(t.to_string()));
            }
        }
    }

    // Finish reason on the tail chunk.
    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        let reason = match fr {
            "tool_calls" => StopReason::ToolCalls,
            "length" => StopReason::Length,
            _ => StopReason::Stop,
        };
        return Some(Raw::Finish(reason));
    }
    None
}

/// Flush accumulated tool-call fragments into whole `LlmDelta::ToolCall`s.
fn flush_tools(
    accum: &mut std::collections::BTreeMap<u32, (String, String)>,
    pending: &mut std::collections::VecDeque<LlmDelta>,
) {
    for (index, (name, args_str)) in std::mem::take(accum) {
        if name.is_empty() {
            continue; // never a real call without a name
        }
        let args = if args_str.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&args_str).unwrap_or_else(|_| Value::Object(Default::default()))
        };
        pending.push_back(LlmDelta::ToolCall {
            id: index,
            name,
            args,
        });
    }
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
        // Turn the byte stream into line-buffered deltas. Tool-call fragments are
        // accumulated in `accum` (index -> (name, args-string)) and only emitted
        // as whole `LlmDelta::ToolCall`s when the turn finishes — so a streamed
        // call ("gmail.search" + dribbled arguments) becomes one complete call,
        // not a pile of nameless fragments. `pending` is the emit queue.
        use std::collections::{BTreeMap, VecDeque};
        let state = (
            byte_stream,
            String::new(),
            cancel,
            false, // done
            BTreeMap::<u32, (String, String)>::new(),
            VecDeque::<LlmDelta>::new(),
        );
        let s = stream::unfold(
            state,
            move |(mut bytes, mut buf, cancel, mut done, mut accum, mut pending)| async move {
                loop {
                    // Always drain anything already queued first.
                    if let Some(d) = pending.pop_front() {
                        return Some((d, (bytes, buf, cancel, done, accum, pending)));
                    }
                    if done {
                        return None;
                    }
                    if cancel.is_cancelled() {
                        done = true;
                        return Some((
                            LlmDelta::Done {
                                stop_reason: StopReason::Cancelled,
                            },
                            (bytes, buf, cancel, done, accum, pending),
                        ));
                    }
                    // Consume one complete SSE line from the buffer if present.
                    if let Some(pos) = buf.find('\n') {
                        let line = buf[..pos].trim().to_string();
                        buf.drain(..=pos);
                        if let Some(data) = line.strip_prefix("data:") {
                            let data = data.trim();
                            if data == "[DONE]" {
                                flush_tools(&mut accum, &mut pending);
                                pending.push_back(LlmDelta::Done {
                                    stop_reason: StopReason::Stop,
                                });
                                done = true;
                                continue; // drain pending on the next lap
                            }
                            match parse_sse_raw(data) {
                                Some(Raw::Text(t)) => {
                                    return Some((
                                        LlmDelta::Text(t),
                                        (bytes, buf, cancel, done, accum, pending),
                                    ));
                                }
                                Some(Raw::ToolFragment { index, name, args }) => {
                                    let e = accum.entry(index).or_default();
                                    if !name.is_empty() {
                                        e.0 = name;
                                    }
                                    e.1.push_str(&args);
                                }
                                Some(Raw::Finish(reason)) => {
                                    flush_tools(&mut accum, &mut pending);
                                    pending.push_back(LlmDelta::Done {
                                        stop_reason: reason,
                                    });
                                    done = true;
                                }
                                None => {}
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
                            flush_tools(&mut accum, &mut pending);
                            pending.push_back(LlmDelta::Done {
                                stop_reason: StopReason::Stop,
                            });
                            done = true;
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
        assert_eq!(parse_sse_raw(j), Some(Raw::Text("Hello".into())));
    }

    #[test]
    fn parses_tool_fragment() {
        let j = r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"cal.free","arguments":"{\"date\":"}}]}}]}"#;
        assert_eq!(
            parse_sse_raw(j),
            Some(Raw::ToolFragment {
                index: 2,
                name: "cal.free".into(),
                args: "{\"date\":".into(),
            })
        );
    }

    #[test]
    fn parses_finish_reason() {
        let j = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        assert_eq!(parse_sse_raw(j), Some(Raw::Finish(StopReason::ToolCalls)));
    }

    /// The real bug: a streamed tool call arrives as fragments (name first, then
    /// argument pieces). Accumulation must reassemble ONE complete call — not a
    /// pile of nameless ones — which is what broke `--jinja` tool use.
    #[test]
    fn accumulates_streamed_tool_call_fragments() {
        use std::collections::{BTreeMap, VecDeque};
        // Simulate llama-server's `--jinja` streaming: name in the first frag,
        // arguments dribbled across the rest.
        let frags = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"gmail.search","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"{\"query\":\"is:"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"","arguments":"unread\"}"}}]}}]}"#,
        ];
        let mut accum: BTreeMap<u32, (String, String)> = BTreeMap::new();
        for f in frags {
            if let Some(Raw::ToolFragment { index, name, args }) = parse_sse_raw(f) {
                let e = accum.entry(index).or_default();
                if !name.is_empty() {
                    e.0 = name;
                }
                e.1.push_str(&args);
            } else {
                panic!("expected a tool fragment for {f}");
            }
        }
        let mut pending: VecDeque<LlmDelta> = VecDeque::new();
        flush_tools(&mut accum, &mut pending);
        assert_eq!(pending.len(), 1, "fragments must collapse to ONE call");
        match pending.pop_front().unwrap() {
            LlmDelta::ToolCall { id, name, args } => {
                assert_eq!(id, 0);
                assert_eq!(name, "gmail.search");
                assert_eq!(args["query"], "is:unread");
            }
            other => panic!("expected one whole tool call, got {other:?}"),
        }
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
            top_p: LlmRequest::DEFAULT_TOP_P,
            top_k: LlmRequest::DEFAULT_TOP_K,
            min_p: LlmRequest::DEFAULT_MIN_P,
            repeat_penalty: LlmRequest::DEFAULT_REPEAT_PENALTY,
        });
        assert!(body.get("tools").is_none());
        // Sampling window is present and constrained (f32→f64 widening means we
        // compare approximately, not for exact equality).
        assert!((body["top_p"].as_f64().unwrap() - 0.8).abs() < 1e-4);
        assert_eq!(body["top_k"], 20);
        assert!((body["min_p"].as_f64().unwrap() - 0.05).abs() < 1e-4);
        assert!((body["repeat_penalty"].as_f64().unwrap() - 1.05).abs() < 1e-4);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }
}
