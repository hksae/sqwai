use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult, ToolCallReq};
use crate::config::{ResolvedProvider, ThinkingLevel};

#[derive(Clone)]
pub struct OpenAiProvider {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(p: &ResolvedProvider) -> Result<Self> {
        let http = reqwest::ClientBuilder::new()
            .http1_only()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(180))
            .build()?;
        Ok(Self {
            http,
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone(),
        })
    }

    fn map_usage(v: &Value) -> Option<super::Usage> {
        let u = v.get("usage")?;
        if !u.is_object() || u.as_object().is_none_or(|o| o.is_empty()) {
            return None;
        }
        Some(super::Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            cached_tokens: u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(|c| c.as_u64()),
        })
    }

    fn message_json(m: &super::Message) -> Value {
        match m.role {
            Role::System => json!({"role": "system", "content": m.content}),
            Role::User => json!({"role": "user", "content": m.content}),
            Role::Tool => json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            }),
            Role::Assistant if !m.tool_calls.is_empty() => {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // arguments must be a JSON *string* on the wire
                                "arguments": c.args.to_string(),
                            },
                        })
                    })
                    .collect();
                let content = if m.content.is_empty() {
                    Value::Null
                } else {
                    json!(m.content)
                };
                json!({"role": "assistant", "content": content, "tool_calls": calls})
            }
            Role::Assistant => json!({"role": "assistant", "content": m.content}),
        }
    }
}

/// accumulated partial tool call keyed by the streaming index
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

impl PartialCall {
    fn finish(self) -> Option<ToolCallReq> {
        if self.name.is_empty() {
            return None;
        }
        let args: Value = if self.args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.args).unwrap_or_else(
                |_| json!({ "_raw": self.args, "_error": "arguments were not valid JSON" }),
            )
        };
        Some(ToolCallReq {
            id: self.id,
            name: self.name,
            args,
        })
    }
}

impl Provider for OpenAiProvider {
    fn capabilities(&self) -> super::ProviderCapabilities {
        // Some OpenAI-compatible servers do automatic prefix caching, but
        // there is no addressable cache key and no per-server guarantee, so we
        // claim nothing. Caching only counts once cached_tokens come back.
        super::ProviderCapabilities::default()
    }

    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult> {
        let this = self.clone();
        stream! {
            let mut req = req;
            this.sanitize(&mut req);
            let url = format!("{}/chat/completions", this.base_url.trim_end_matches('/'));
            // Chat Completions has no documented continuation field, so the
            // full transcript is always resent. sanitize() has already dropped
            // previous_response_id; this only records that it happened.
            if req.previous_response_id.is_some() {
                super::log_http(
                    "openai-compatible: previous_response_id dropped (not supported by Chat Completions)",
                );
            }
            let mut msgs: Vec<Value> = Vec::new();
            let system = super::system_text(&req.system);
            if !system.trim().is_empty() {
                msgs.push(json!({"role": "system", "content": system}));
            }
            msgs.extend(req.messages.iter().map(Self::message_json));
            let mut body = json!({
                "model": req.model_id,
                "messages": msgs,
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            // openai-compatible reasoning control; servers that do not know
            // the field simply ignore it
            if let Some(level) = req.thinking.filter(|l| *l != ThinkingLevel::Off) {
                let effort = match level {
                    ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High | ThinkingLevel::Max => "high",
                    ThinkingLevel::Off => unreachable!(),
                };
                body["reasoning_effort"] = json!(effort);
            }
            if let Some(mt) = req.max_tokens { body["max_tokens"] = json!(mt); }
            if !req.tools.is_empty() {
                body["tools"] = json!(req.tools.iter().map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })).collect::<Vec<_>>());
                // explicit auto nudges small local models (ollama) into
                // emitting structured tool_calls instead of plain-text JSON
                body["tool_choice"] = json!("auto");
            }

            let mut r = this.http.post(&url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            let resp = match r.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    super::log_http(&format!("POST {url} failed: {e}"));
                    yield Err(anyhow!("request failed: {e}"));
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                super::log_http(&format!("POST {url} -> {status}: {body}"));
                yield Err(anyhow!("provider returned {status}: {}", truncate(&body, 2000)));
                return;
            }

            // index -> accumulating call
            let mut partials: BTreeMap<i64, PartialCall> = BTreeMap::new();

            let mut es = resp.bytes_stream().eventsource();
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        if ev.data.trim() == "[DONE]" { break; }
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
                            yield Ok(StreamEvent::ResponseId(id.to_string()));
                        }
                        if let Some(u) = Self::map_usage(&v) { yield Ok(StreamEvent::Usage(u)); }
                        let choice = &v["choices"][0];
                        let Some(delta) = choice.get("delta") else { continue };

                        // streamed text / reasoning
                        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                            if !c.is_empty() { yield Ok(StreamEvent::Text(c.to_string())); }
                        }
                        let reasoning = delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning"))
                            .and_then(|r| r.as_str());
                        if let Some(rr) = reasoning {
                            if !rr.is_empty() { yield Ok(StreamEvent::Reasoning(rr.to_string())); }
                        }

                        // streamed tool calls
                        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                            for tc in calls {
                                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                                let slot = partials.entry(idx).or_default();
                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    slot.id.push_str(id);
                                }
                                if let Some(f) = tc.get("function") {
                                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                                        slot.name.push_str(n);
                                    }
                                    if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                                        slot.args.push_str(a);
                                    }
                                }
                            }
                        }

                        // completion reason: flush accumulated calls in order
                        if choice.get("finish_reason").and_then(|f| f.as_str())
                            == Some("tool_calls")
                        {
                            for (_, p) in std::mem::take(&mut partials) {
                                if let Some(req) = p.finish() {
                                    yield Ok(StreamEvent::ToolCall(req));
                                }
                            }
                        }
                    }
                    Err(e) => { yield Err(anyhow!("stream error: {e}")); return; }
                }
            }
            // safety net: some servers never send finish_reason=tool_calls
            for (_, p) in std::mem::take(&mut partials) {
                if let Some(req) = p.finish() {
                    yield Ok(StreamEvent::ToolCall(req));
                }
            }
        }
        .boxed()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::super::Message;
    use super::*;

    #[test]
    fn request_body_includes_tools_and_tool_messages() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "list files"),
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq {
                    id: "call_1".into(),
                    name: "ls".into(),
                    args: json!({"path": "."}),
                }]),
                Message::tool_result("call_1", "a.txt\nb.txt", false),
            ],
            thinking: None,
            max_tokens: None,
            tools: vec![super::super::ToolSpec {
                name: "ls".into(),
                description: "list directory".into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let msgs: Vec<Value> = req
            .messages
            .iter()
            .map(OpenAiProvider::message_json)
            .collect();
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "ls");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"."}"#
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");

        let body_tools = json!(req.tools.iter().map(|t| json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.parameters},
        })).collect::<Vec<_>>());
        assert_eq!(body_tools[0]["function"]["name"], "ls");
    }

    #[test]
    fn sanitize_drops_fields_a_gateway_does_not_support() {
        let p = OpenAiProvider::new(&crate::config::ResolvedProvider {
            name: "local".into(),
            format: crate::config::WireFormat::Openai,
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
        })
        .unwrap();
        let mut req = ChatRequest {
            model_id: "m".into(),
            system: vec![crate::providers::SystemPart::cached("sys")],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: Some("resp_1".into()),
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        p.sanitize(&mut req);
        assert!(
            req.previous_response_id.is_none(),
            "Chat Completions has no continuation field — it must never reach the wire"
        );
    }

    #[test]
    fn system_block_precedes_history() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![
                crate::providers::SystemPart::cached("A"),
                crate::providers::SystemPart::volatile("B"),
            ],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let system = crate::providers::system_text(&req.system);
        assert_eq!(system, "A\n\nB");
        let msgs: Vec<Value> = std::iter::once(json!({"role": "system", "content": system}))
            .chain(req.messages.iter().map(OpenAiProvider::message_json))
            .collect();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn stream_parses_tool_call_deltas() {
        // simulate two argument deltas + finish_reason
        let chunks = [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"src/main.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut buf = String::new();
        let mut partials: BTreeMap<i64, PartialCall> = BTreeMap::new();
        for c in chunks {
            let v: Value = serde_json::from_str(c).unwrap();
            if let Some(calls) = v["choices"][0]["delta"]
                .get("tool_calls")
                .and_then(|x| x.as_array())
            {
                for tc in calls {
                    let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                    let slot = partials.entry(idx).or_default();
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        slot.id.push_str(id);
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                            slot.name.push_str(n);
                        }
                        if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                            slot.args.push_str(a);
                        }
                    }
                }
            }
            if v["choices"][0]["finish_reason"] == "tool_calls" {
                for (_, p) in std::mem::take(&mut partials) {
                    if let Some(r) = p.finish() {
                        use std::fmt::Write;
                        let _ = write!(buf, "{} {:?}", r.name, r.args);
                    }
                }
            }
        }
        assert!(buf.contains("read"), "{buf}");
        assert!(buf.contains("src/main.rs"), "{buf}");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "привет мир";
        let t = truncate(s, 8);
        assert!(t.ends_with('…'));
        // no partial multibyte sequence: must round-trip as valid UTF-8
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert!(s.starts_with(t.trim_end_matches('…')));
    }
}
