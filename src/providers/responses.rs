use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult};
use crate::config::{ResolvedProvider, ThinkingLevel};

#[derive(Clone)]
pub struct ResponsesProvider {
    http: reqwest::Client,
    url: String,
    api_key: Option<String>,
}

/// Map thinking level to the `reasoning.effort` parameter.
pub fn effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::Max => Some("high"),
    }
}

pub fn build_body(req: &ChatRequest) -> Value {
    let input: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let ty = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                // tool results ride as user turns until full support lands
                Role::Tool => "user",
            };
            json!({
                "role": ty,
                "content": [{"type": "input_text", "text": m.content}],
            })
        })
        .collect();

    let mut body = json!({
        "model": req.model_id,
        "input": input,
        "stream": true,
    });
    if let Some(e) = req.thinking.and_then(effort) {
        body["reasoning"] = json!({"effort": e});
    }
    if let Some(id) = &req.previous_response_id {
        body["previous_response_id"] = json!(id);
    }
    body
}

impl ResponsesProvider {
    pub fn new(p: &ResolvedProvider) -> Result<Self> {
        let http = reqwest::ClientBuilder::new()
            .http1_only()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(180))
            .build()?;
        let base = p.base_url.trim_end_matches('/').to_string();
        let url = if base.ends_with("/responses") {
            base
        } else if base.ends_with("/v1") {
            format!("{base}/responses")
        } else {
            format!("{base}/v1/responses")
        };
        Ok(Self {
            http,
            url,
            api_key: p.api_key.clone(),
        })
    }
}

impl Provider for ResponsesProvider {
    fn capabilities(&self) -> super::ProviderCapabilities {
        super::ProviderCapabilities {
            previous_response: true,
            ..Default::default()
        }
    }

    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult> {
        let this = self.clone();
        stream! {
            if !req.tools.is_empty() {
                yield Err(anyhow!("responses format: tools are not supported yet (use openai or anthropic)"));
                return;
            }
            let body = build_body(&req);
            let mut r = this.http.post(&this.url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            let resp = match r.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    super::log_http(&format!("POST {} failed: {e}", this.url));
                    yield Err(anyhow!("request failed: {e}"));
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let t = resp.text().await.unwrap_or_default();
                super::log_http(&format!("POST {} -> {status}: {t}", this.url));
                yield Err(anyhow!("provider returned {status}: {}", short(&t)));
                return;
            }

            let mut es = resp.bytes_stream().eventsource();
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        if ev.data == "[DONE]" { break; }
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let response_id = v.pointer("/response/id").and_then(|x| x.as_str());
                        if let Some(id) = response_id {
                            yield Ok(StreamEvent::ResponseId(id.to_string()));
                        }
                        match ev.event.as_str() {
                            "response.output_text.delta" => {
                                if let Some(t) = v.get("delta").and_then(|x| x.as_str()) {
                                    if !t.is_empty() { yield Ok(StreamEvent::Text(t.to_string())); }
                                }
                            }
                            "response.reasoning_text.delta"
                            | "response.reasoning_summary_text.delta" => {
                                if let Some(t) = v.get("delta").and_then(|x| x.as_str()) {
                                    if !t.is_empty() { yield Ok(StreamEvent::Reasoning(t.to_string())); }
                                }
                            }
                            "response.completed" | "response.incomplete" => {
                                if let Some(u) = v.pointer("/response/usage") {
                                    yield Ok(StreamEvent::Usage(super::Usage {
                                        prompt_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                                        completion_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                                        cached_tokens: u.pointer("/input_tokens_details/cached_tokens").and_then(|x| x.as_u64()),
                                    }));
                                }
                                if ev.event.as_str() == "response.completed" { break; }
                            }
                            "error" => {
                                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("unknown");
                                super::log_http(&format!("POST {} stream error: {msg}", this.url));
                                yield Err(anyhow!("provider error: {msg}"));
                                return;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => { yield Err(anyhow!("stream error: {e}")); return; }
                }
            }
        }
        .boxed()
    }
}

fn short(s: &str) -> String {
    let mut cut = 500.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Message;

    #[test]
    fn effort_mapping() {
        assert_eq!(effort(ThinkingLevel::Off), None);
        assert_eq!(effort(ThinkingLevel::Low), Some("low"));
        assert_eq!(effort(ThinkingLevel::Max), Some("high"));
    }

    #[test]
    fn body_roles_and_reasoning() {
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            messages: vec![
                Message::new(Role::System, "s"),
                Message::new(Role::User, "hi"),
            ],
            thinking: Some(ThinkingLevel::Medium),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req);
        assert_eq!(b["model"], "gpt-x");
        assert_eq!(b["reasoning"]["effort"], "medium");
        assert_eq!(b["input"][0]["role"], "system");
        assert_eq!(b["input"][0]["content"][0]["type"], "input_text");
    }
}
