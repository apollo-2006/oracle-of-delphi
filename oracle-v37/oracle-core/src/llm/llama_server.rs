//! Backend for a llama.cpp `server` (OpenAI-compatible `/v1/chat/completions`).
//!
//! Point it at a locally-running
//! `llama-server -m qwen2.5-14b-instruct-q5_k.gguf --host 127.0.0.1 --port 8080`.
//! We send our GBNF **grammar** with each request so the model's output is
//! constrained to our JSON protocol (one tool call or one spoken line) — there is
//! no tool-call channel to parse and nothing to "recover". We stream SSE text
//! deltas and honor cancellation by dropping the response body (which aborts the
//! HTTP request; llama-server treats that as a client disconnect and stops).
//!
//! Network access is not available in unit tests, so this module is validated by
//! its pure helpers rather than a live round-trip.

use super::*;
use futures::stream::{self, StreamExt};
use serde_json::Value;

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
            // A message with no images keeps the plain-string content form.
            // Sending the multimodal array shape unconditionally would work on
            // llama-server but changes the prompt a text model sees, and there
            // is no reason to perturb the path that already works.
            if m.images.is_empty() {
                messages.push(serde_json::json!({"role": role, "content": m.content}));
            } else {
                let mut parts = Vec::new();
                // Images first: a VLM conditions the text on them, and putting
                // the instruction after the picture is what the chat templates
                // are trained on.
                for url in &m.images {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": url }
                    }));
                }
                if !m.content.is_empty() {
                    parts.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                messages.push(serde_json::json!({"role": role, "content": parts}));
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            // Qwen2.5's recommended sampling window (llama.cpp extensions the
            // OpenAI-compatible endpoint accepts).
            "top_p": req.top_p,
            "top_k": req.top_k,
            "min_p": req.min_p,
            "repeat_penalty": req.repeat_penalty,
            "stream": true,
        });
        // The grammar is what makes tool use reliable: the sampler can only emit
        // tokens this GBNF allows, so the reply is always valid protocol JSON.
        if let Some(g) = &req.grammar {
            if !g.trim().is_empty() {
                body["grammar"] = Value::String(g.clone());
            }
        }
        body
    }
}

/// Pull the text delta (and/or finish reason) out of one SSE `data:` JSON chunk.
/// Returns `(Option<text>, Option<StopReason>)`; `(None, None)` for keepalives.
pub(crate) fn parse_sse_chunk(json: &str) -> (Option<String>, Option<StopReason>) {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return (None, None);
    };
    let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
        return (None, None);
    };
    let text = choice
        .get("delta")
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let stop = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(|fr| match fr {
            "length" => StopReason::Length,
            _ => StopReason::Stop,
        });
    (text, stop)
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
        // Line-buffer the SSE stream into text deltas. `done` guards a single
        // terminal `Done`.
        let state = (byte_stream, String::new(), cancel, false);
        let s = stream::unfold(state, move |(mut bytes, mut buf, cancel, mut done)| async move {
            loop {
                if done {
                    return None;
                }
                if cancel.is_cancelled() {
                    done = true;
                    return Some((
                        LlmDelta::Done {
                            stop_reason: StopReason::Cancelled,
                        },
                        (bytes, buf, cancel, done),
                    ));
                }
                // Consume one complete SSE line if present.
                if let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf.drain(..=pos);
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            done = true;
                            return Some((
                                LlmDelta::Done {
                                    stop_reason: StopReason::Stop,
                                },
                                (bytes, buf, cancel, done),
                            ));
                        }
                        let (text, stop) = parse_sse_chunk(data);
                        if let Some(t) = text {
                            return Some((LlmDelta::Text(t), (bytes, buf, cancel, done)));
                        }
                        if let Some(reason) = stop {
                            done = true;
                            return Some((
                                LlmDelta::Done { stop_reason: reason },
                                (bytes, buf, cancel, done),
                            ));
                        }
                    }
                    continue;
                }
                // Need more bytes.
                match bytes.next().await {
                    Some(Ok(chunk)) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                    Some(Err(_)) | None => {
                        done = true;
                        return Some((
                            LlmDelta::Done {
                                stop_reason: StopReason::Stop,
                            },
                            (bytes, buf, cancel, done),
                        ));
                    }
                }
            }
        })
        .boxed();
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(grammar: Option<String>) -> LlmRequest {
        LlmRequest {
            system: "sys".into(),
            messages: vec![ChatMessage::text(Role::User, "hi")],
            grammar,
            max_tokens: 128,
            temperature: 0.3,
            top_p: LlmRequest::DEFAULT_TOP_P,
            top_k: LlmRequest::DEFAULT_TOP_K,
            min_p: LlmRequest::DEFAULT_MIN_P,
            repeat_penalty: LlmRequest::DEFAULT_REPEAT_PENALTY,
        }
    }

    #[test]
    fn a_text_only_message_keeps_the_plain_string_content_form() {
        // The path every existing turn takes must be byte-identical to before
        // vision existed: changing the content shape changes what a text model
        // is prompted with.
        let s = LlamaServer::new("http://x", "m");
        let body = s.build_body(&req(None));
        let content = &body["messages"][1]["content"];
        assert!(content.is_string(), "got {content}");
        assert_eq!(content.as_str().unwrap(), "hi");
    }

    #[test]
    fn an_image_message_becomes_a_multimodal_content_array() {
        let s = LlamaServer::new("http://x", "m");
        let mut r = req(None);
        r.messages = vec![ChatMessage::with_png(Role::User, "what is this?", "QUJD")];
        let body = s.build_body(&r);
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        // Image first: the chat templates a VLM is trained on put the picture
        // ahead of the instruction.
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(
            parts[0]["image_url"]["url"].as_str().unwrap(),
            "data:image/png;base64,QUJD"
        );
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[1]["text"], "what is this?");
    }

    #[test]
    fn an_image_with_no_prompt_text_emits_only_the_image_part() {
        let s = LlamaServer::new("http://x", "m");
        let mut r = req(None);
        r.messages = vec![ChatMessage::with_png(Role::User, "", "QUJD")];
        let body = s.build_body(&r);
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(
            parts.len(),
            1,
            "an empty text part would confuse the template"
        );
    }

    #[test]
    fn parses_text_delta() {
        let j = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        assert_eq!(parse_sse_chunk(j), (Some("Hello".into()), None));
    }

    #[test]
    fn parses_finish_reason() {
        let j = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;
        assert_eq!(parse_sse_chunk(j), (None, Some(StopReason::Stop)));
    }

    #[test]
    fn keepalive_is_ignored() {
        assert_eq!(parse_sse_chunk(r#"{"choices":[{"delta":{}}]}"#), (None, None));
    }

    #[test]
    fn body_carries_grammar_and_sampling() {
        let ls = LlamaServer::new("http://127.0.0.1:8080", "qwen");
        let body = ls.build_body(&req(Some("root ::= \"x\"".into())));
        assert_eq!(body["grammar"], "root ::= \"x\"");
        assert!((body["top_p"].as_f64().unwrap() - 0.8).abs() < 1e-4);
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn body_omits_grammar_when_none() {
        let ls = LlamaServer::new("http://127.0.0.1:8080", "qwen");
        let body = ls.build_body(&req(None));
        assert!(body.get("grammar").is_none());
    }
}
