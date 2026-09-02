use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult, ToolCallReq};
use crate::config::{ResolvedProvider, ThinkingLevel};

#[derive(Clone)]
pub struct AnthropicProvider {
    http: reqwest::Client,
    url: String,
    api_key: String,
}

fn budget(level: ThinkingLevel) -> u32 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High => 16384,
        ThinkingLevel::Max => 32768,
    }
}

/// content blocks for one message (anthropic wire format)
fn content_blocks(m: &super::Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    // a tool result's payload lives inside the tool_result block itself
    if !m.content.is_empty() && m.role != Role::Tool {
        blocks.push(json!({"type": "text", "text": m.content}));
    }
    for c in &m.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": c.id,
            "name": c.name,
            "input": c.args,
        }));
    }
    if m.role == Role::Tool {
        blocks.push(json!({
            "type": "tool_result",
            "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.content,
            "is_error": m.is_error,
        }));
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    blocks
}

/// Build the /v1/messages request body (unit-tested).
///
/// `cache_breakpoints` mirrors
/// [`ProviderCapabilities::prompt_cache_documented`](super::ProviderCapabilities):
/// breakpoints are only emitted for providers that document an addressable
/// cache key. Breakpoints land on the stable prefix
/// ([`SystemPart::cacheable`](super::SystemPart)); volatile parts follow them
/// so a changed date or git status cannot invalidate the cached prefix.
pub fn build_body(req: &ChatRequest, base_max_tokens: u32, cache_breakpoints: bool) -> Value {
    let system: Vec<Value> = req
        .system
        .iter()
        .map(|part| {
            let mut block = json!({"type": "text", "text": part.text});
            if cache_breakpoints && part.cacheable {
                block["cache_control"] = json!({"type": "ephemeral"});
            }
            block
        })
        .collect();
    let mut msgs: Vec<Value> = Vec::new();
    for m in &req.messages {
        if m.role == Role::System
            || (m.content.is_empty() && m.tool_calls.is_empty() && m.role != Role::Tool)
        {
            continue;
        }
        let role = match m.role {
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
            Role::System => unreachable!(),
        };
        // merge consecutive same-role turns (API requires alternation);
        // tool results must land in a user turn right after the assistant
        let same_role = msgs
            .last()
            .and_then(|p| p.get("role"))
            .and_then(|r| r.as_str())
            == Some(role);
        if same_role {
            if let Some(arr) = msgs
                .last_mut()
                .and_then(|p| p.get_mut("content"))
                .and_then(|c| c.as_array_mut())
            {
                arr.extend(content_blocks(m));
                continue;
            }
        }
        msgs.push(json!({
            "role": role,
            "content": content_blocks(m),
        }));
    }

    let mut body = json!({
        "model": req.model_id,
        "max_tokens": base_max_tokens,
        "stream": true,
        "messages": msgs,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }

    if let Some(level) = req.thinking.filter(|l| *l != ThinkingLevel::Off) {
        let b = budget(level);
        body["thinking"] = json!({"type": "enabled", "budget_tokens": b});
        body["max_tokens"] = json!((base_max_tokens + b).min(64_000));
    }

    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                }))
                .collect::<Vec<_>>()
        );
    }
    body
}

impl AnthropicProvider {
    pub fn new(p: &ResolvedProvider) -> Result<Self> {
        let key = p
            .api_key
            .clone()
            .ok_or_else(|| anyhow!("anthropic: api key missing"))?;
        let http = reqwest::ClientBuilder::new()
            .http1_only()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(180))
            .build()?;
        let base = p.base_url.trim_end_matches('/').to_string();
        let url = if base.ends_with("/messages") {
            base
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        Ok(Self {
            http,
            url,
            api_key: key,
        })
    }
}

impl Provider for AnthropicProvider {
    fn capabilities(&self) -> super::ProviderCapabilities {
        // cache_control is a documented, addressable cache-key mechanism:
        // we decide where the breakpoints go and the server tells us what it
        // read back. Automatic prefix caching elsewhere does not qualify.
        super::ProviderCapabilities {
            prompt_cache_documented: true,
            ..Default::default()
        }
    }

    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult> {
        let this = self.clone();
        let cache_breakpoints = self.capabilities().prompt_cache_documented;
        stream! {
            let mut req = req;
            this.sanitize(&mut req);
            let body = build_body(&req, 8192, cache_breakpoints);
            let resp = match this
                .http
                .post(&this.url)
                .header("x-api-key", &this.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
                .send()
                .await
            {
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

            // index -> accumulating tool_use input
            let mut partials: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

            let mut es = resp.bytes_stream().eventsource();
            let mut out_tokens: u64 = 0;
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        match ev.event.as_str() {
                            "message_start" => {
                                if let Some(id) = v.pointer("/message/id").and_then(|x| x.as_str()) {
                                    yield Ok(StreamEvent::ResponseId(id.to_string()));
                                }
                                let inp = v.pointer("/message/usage/input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                                let cached = v.pointer("/message/usage/cache_read_input_tokens").and_then(|x| x.as_u64());
                                yield Ok(StreamEvent::Usage(super::Usage {
                                    prompt_tokens: inp,
                                    completion_tokens: 0,
                                    cached_tokens: cached,
                                }));
                            }
                            "content_block_start" => {
                                let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                if v.pointer("/content_block/type").and_then(|x| x.as_str()) == Some("tool_use") {
                                    let id = v.pointer("/content_block/id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                    let name = v.pointer("/content_block/name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                    partials.insert(idx, (id, name, String::new()));
                                }
                            }
                            "content_block_delta" => {
                                let kind = v.pointer("/delta/type").and_then(|x| x.as_str()).unwrap_or("");
                                match kind {
                                    "text_delta" => {
                                        if let Some(t) = v.pointer("/delta/text").and_then(|x| x.as_str()) {
                                            if !t.is_empty() { yield Ok(StreamEvent::Text(t.to_string())); }
                                        }
                                    }
                                    "thinking_delta" => {
                                        if let Some(t) = v.pointer("/delta/thinking").and_then(|x| x.as_str()) {
                                            if !t.is_empty() { yield Ok(StreamEvent::Reasoning(t.to_string())); }
                                        }
                                    }
                                    "input_json_delta" => {
                                        let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                        if let Some(pj) = v.pointer("/delta/partial_json").and_then(|x| x.as_str()) {
                                            if let Some(slot) = partials.get_mut(&idx) {
                                                slot.2.push_str(pj);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_stop" => {
                                let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                if let Some((id, name, args)) = partials.remove(&idx) {
                                    let args_v: Value = if args.trim().is_empty() {
                                        json!({})
                                    } else {
                                        serde_json::from_str(&args).unwrap_or_else(|_| {
                                            json!({"_raw": args, "_error": "arguments were not valid JSON"})
                                        })
                                    };
                                    yield Ok(StreamEvent::ToolCall(ToolCallReq { id, name, args: args_v }));
                                }
                            }
                            "message_delta" => {
                                if let Some(u) = v.get("usage") {
                                    out_tokens = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(out_tokens);
                                    yield Ok(StreamEvent::Usage(super::Usage {
                                        prompt_tokens: 0,
                                        completion_tokens: out_tokens,
                                        cached_tokens: None,
                                    }));
                                }
                            }
                            "message_stop" => break,
                            "error" => {
                                let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("unknown");
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
    fn body_has_cache_control_and_thinking() {
        let req = ChatRequest {
            model_id: "claude-x".into(),
            system: vec![
                crate::providers::SystemPart::cached("stable prefix"),
                crate::providers::SystemPart::volatile("git: on branch main"),
            ],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: Some(ThinkingLevel::High),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 8192, true);
        assert_eq!(b["model"], "claude-x");
        // breakpoint on the stable prefix only — the volatile tail must not
        // carry one, otherwise every new date/git state would re-cache
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(b["system"][1].get("cache_control").is_none());
        assert_eq!(b["thinking"]["budget_tokens"], 16384);
        assert_eq!(b["max_tokens"], 8192 + 16384);
        assert_eq!(b["messages"][0]["role"], "user");
    }

    #[test]
    fn body_without_documented_cache_has_no_breakpoints() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![crate::providers::SystemPart::cached("stable prefix")],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 1024, false);
        assert!(b["system"][0].get("cache_control").is_none());
        assert_eq!(b["system"][0]["text"], "stable prefix");
    }

    #[test]
    fn empty_system_block_is_omitted() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 1024, true);
        assert!(b.get("system").is_none());
    }

    #[test]
    fn body_without_thinking_omits_field() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 8192, true);
        assert!(b.get("thinking").is_none());
    }

    #[test]
    fn body_maps_tools_and_results() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "list"),
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq {
                    id: "tu_1".into(),
                    name: "ls".into(),
                    args: json!({"path": "."}),
                }]),
                Message::tool_result("tu_1", "a.txt", false),
            ],
            thinking: None,
            max_tokens: None,
            tools: vec![super::super::ToolSpec {
                name: "ls".into(),
                description: "list dir".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 8192, true);

        if std::env::var("SQWAI_DEBUG_BODY").is_ok() {
            eprintln!("{}", serde_json::to_string_pretty(&b).unwrap());
        }
        assert_eq!(b["tools"][0]["name"], "ls");
        assert_eq!(b["tools"][0]["input_schema"]["type"], "object");

        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "alternation preserved");
        // assistant carries the tool_use block
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "tu_1");
        assert_eq!(msgs[1]["content"][0]["input"]["path"], ".");
        // result rides in the following user turn
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "tu_1");
        assert_eq!(msgs[2]["content"][0]["content"], "a.txt");
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_turn() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::Assistant, "").with_tool_calls(vec![
                    ToolCallReq {
                        id: "a".into(),
                        name: "t".into(),
                        args: json!({}),
                    },
                    ToolCallReq {
                        id: "b".into(),
                        name: "t".into(),
                        args: json!({}),
                    },
                ]),
                Message::tool_result("a", "res-a", false),
                Message::tool_result("b", "res-b", false),
            ],
            thinking: None,
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 1024, true);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "results merged into single user turn");
        let arr = msgs[1]["content"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["tool_use_id"], "a");
        assert_eq!(arr[1]["tool_use_id"], "b");
    }
}
