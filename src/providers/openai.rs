use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult, ToolCallReq};
use crate::config::{EffortLevel, ResolvedProvider};

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
            reasoning_tokens: u
                .pointer("/completion_tokens_details/reasoning_tokens")
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
                        let mut call = json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // arguments must be a JSON *string* on the wire
                                "arguments": c.args.to_string(),
                            },
                        });
                        // replayed verbatim: Gemini validates it, other
                        // providers never sent one and so never see the field
                        if let Some(extra) = &c.extra_content {
                            call["extra_content"] = extra.clone();
                        }
                        call
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

    pub fn build_body(req: &ChatRequest) -> Value {
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
        if let Some(level) = req.effort.filter(|l| *l != EffortLevel::Off)
            && let super::effort::Wire::Level(effort) =
                super::effort::plan(level, req.effort_support).wire
        {
            body["reasoning_effort"] = json!(effort);
        }
        let uses_max_completion =
            uses_max_completion_tokens(&req.model_id, body.get("reasoning_effort").is_some());
        if let Some(mt) = req.max_tokens {
            if uses_max_completion {
                body["max_completion_tokens"] = json!(mt);
            } else {
                body["max_tokens"] = json!(mt);
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        },
                    }))
                    .collect::<Vec<_>>()
            );
            // explicit auto nudges small local models (ollama) into
            // emitting structured tool_calls instead of plain-text JSON
            body["tool_choice"] = json!("auto");
        }
        body
    }
}

pub fn uses_max_completion_tokens(model: &str, has_reasoning: bool) -> bool {
    if has_reasoning {
        return true;
    }
    let lower = model.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    name.starts_with("o1")
        || name.starts_with("o3")
        || name.starts_with("o4")
        || name.starts_with("o-")
        || name.contains("gpt-5")
}

/// accumulated partial tool call keyed by the streaming index
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
    /// provider state carried alongside this call (Gemini: thought_signature)
    extra_content: Option<Value>,
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
        Some(ToolCallReq::new(self.id, self.name, args).with_extra_content(self.extra_content))
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
            let body = Self::build_body(&req);

            let mut r = this.http.post(&url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            let resp = match r.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    super::log_http(&format!("POST {url} failed: {e}"));
                    yield Err(super::network_error(e));
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                super::log_http(&format!("POST {url} -> {status}: {body}"));
                yield Err(super::response_error(status.as_u16(), &truncate(&body, 2000)));
                return;
            }

            // index -> accumulating call
            let mut partials: BTreeMap<i64, PartialCall> = BTreeMap::new();

            let mut es = resp.bytes_stream().eventsource();
            // every chunk of a Chat Completions stream repeats the response id;
            // the event means "this response started", so it is emitted once
            let mut response_id_sent = false;
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        if ev.data.trim() == "[DONE]" { break; }
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            // eventsource-stream already reassembled the frame, so
                            // this is a syntactically invalid payload, not a partial
                            // one. Dropping it silently loses whatever it carried —
                            // a tool-call delta included.
                            Err(e) => {
                                super::log_http(&format!(
                                    "openai: dropped unparsable SSE payload ({e}): {}",
                                    ev.data.chars().take(200).collect::<String>()
                                ));
                                continue;
                            }
                        };
                        if let Some(id) = v.get("id").and_then(|x| x.as_str())
                            && !response_id_sent
                        {
                            response_id_sent = true;
                            yield Ok(StreamEvent::ResponseId(id.to_string()));
                        }
                        if let Some(u) = Self::map_usage(&v) { yield Ok(StreamEvent::Usage(u)); }
                        let choice = &v["choices"][0];
                        let Some(delta) = choice.get("delta") else { continue };

                        // streamed text / reasoning
                        if let Some(c) = delta.get("content").and_then(|c| c.as_str())
                            && !c.is_empty()
                        {
                            yield Ok(StreamEvent::Text(c.to_string()));
                        }
                        let reasoning = delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning"))
                            .and_then(|r| r.as_str());
                        if let Some(rr) = reasoning
                            && !rr.is_empty()
                        {
                            yield Ok(StreamEvent::Reasoning(rr.to_string()));
                        }

                        // streamed tool calls
                        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                            for tc in calls {
                                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                                let slot = partials.entry(idx).or_default();
                                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                    slot.id.push_str(id);
                                }
                                // Gemini sends this once, on the delta that
                                // opens the call; never overwrite it with a
                                // later delta that does not carry it
                                if let Some(extra) = tc.get("extra_content")
                                    && !extra.is_null()
                                    && slot.extra_content.is_none()
                                {
                                    slot.extra_content = Some(extra.clone());
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

    /// Serves one canned SSE body and closes. `id` is repeated on every chunk,
    /// the way Chat Completions actually streams.
    fn sse_server(body: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            let mut len = 0usize;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    len = v;
                }
            }
            // the request body must be drained or the client sees a reset
            let mut buf = vec![0u8; len];
            std::io::Read::read_exact(&mut reader, &mut buf).ok();
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(300));
        });
        (format!("http://{addr}/v1"), h)
    }

    /// Like [`sse_server`] but keeps the request bodies it received.
    fn capturing_sse_server() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut len = 0usize;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    len = v;
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok();
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        (format!("http://{addr}/v1"), bodies, h)
    }

    fn plain_request() -> ChatRequest {
        ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        }
    }

    async fn collect(url: String) -> Vec<StreamEvent> {
        use futures::StreamExt;
        let p = OpenAiProvider::new(&crate::config::ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Openai,
            base_url: url,
            api_key: Some("k".into()),
        })
        .unwrap();
        p.stream_chat(plain_request())
            .filter_map(|e| async move { e.ok() })
            .collect()
            .await
    }

    /// The id identifies the response, not the chunk. Emitting it per chunk
    /// makes every consumer of `ResponseId` see a fresh response each time.
    #[tokio::test]
    async fn response_id_is_emitted_once_per_response() {
        let body = "data: {\"id\":\"resp_1\",\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
                    data: {\"id\":\"resp_1\",\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n\
                    data: {\"id\":\"resp_1\",\"choices\":[{\"delta\":{\"content\":\"c\"}}]}\n\n\
                    data: [DONE]\n\n"
            .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();
        let ids: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ResponseId(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["resp_1"], "one response, one id: {events:?}");
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "abc", "text must still stream in full");
    }

    /// The Gemini preset speaks this wire format, so this is the body its
    /// requests actually carry. Asserted here rather than only in
    /// `providers::effort`, because the mapping being right and the body being
    /// right are two different claims.
    #[tokio::test]
    async fn the_body_carries_the_declared_level() {
        use crate::config::{EffortControl, EffortLevel, EffortSupport};
        for (level, control, expected) in [
            (EffortLevel::Max, EffortControl::Levels, Some("high")),
            (EffortLevel::Max, EffortControl::Xhigh, Some("xhigh")),
            (EffortLevel::High, EffortControl::Levels, Some("high")),
            (EffortLevel::Low, EffortControl::Levels, Some("low")),
            (EffortLevel::High, EffortControl::None, None),
            (EffortLevel::Off, EffortControl::Levels, None),
        ] {
            let (url, bodies, h) = capturing_sse_server();
            let mut req = plain_request();
            req.effort = Some(level);
            req.effort_support = EffortSupport {
                control,
                always_on: false,
            };
            let p = OpenAiProvider::new(&crate::config::ResolvedProvider {
                name: "p".into(),
                format: crate::config::WireFormat::Openai,
                base_url: url,
                api_key: Some("k".into()),
            })
            .unwrap();
            let _: Vec<_> = {
                use futures::StreamExt;
                p.stream_chat(req).collect().await
            };
            h.join().unwrap();
            let body = bodies.lock().unwrap().first().cloned().unwrap_or_default();
            let sent: Option<String> = serde_json::from_str::<Value>(&body).ok().and_then(|v| {
                v.get("reasoning_effort")
                    .and_then(|e| e.as_str().map(String::from))
            });
            assert_eq!(
                sent.as_deref(),
                expected,
                "{level:?} under {control:?} produced: {body}"
            );
        }
    }

    /// Gemini 3 attaches a `thought_signature` to the first function call of a
    /// turn and rejects the next request of that same turn with a 400 when it
    /// is missing. The host cannot regenerate it, so the only correct
    /// behaviour is to carry it through untouched.
    #[tokio::test]
    async fn a_tool_call_keeps_the_provider_state_attached_to_it() {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"fc_1\",\"type\":\"function\",\"extra_content\":{\"google\":{\"thought_signature\":\"SIG_A\"}},\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n\
                    data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                    data: [DONE]\n\n"
            .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();
        let call = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .expect("a tool call was streamed");
        assert_eq!(call.args["path"], "a.rs", "arguments still accumulate");
        assert_eq!(
            call.extra_content
                .as_ref()
                .and_then(|e| e.pointer("/google/thought_signature"))
                .and_then(|s| s.as_str()),
            Some("SIG_A"),
            "the signature must survive the stream: {:?}",
            call.extra_content
        );

        // and it must go back out on the wire, verbatim, in the assistant turn
        let replayed = OpenAiProvider::message_json(
            &Message::new(Role::Assistant, "").with_tool_calls(vec![call]),
        );
        assert_eq!(
            replayed["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "SIG_A"
        );
    }

    /// The counter the observed-ignored check reads. `0` is a claim the
    /// provider made; a missing field must stay `None` rather than become 0,
    /// or every non-reasoning gateway would look like it ignored the request.
    #[test]
    fn reasoning_tokens_are_read_when_the_provider_reports_them() {
        let with = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5,
            "completion_tokens_details": {"reasoning_tokens": 0}}});
        assert_eq!(
            OpenAiProvider::map_usage(&with).unwrap().reasoning_tokens,
            Some(0)
        );
        let without = json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5}});
        assert_eq!(
            OpenAiProvider::map_usage(&without)
                .unwrap()
                .reasoning_tokens,
            None,
            "no counter is not the same as a zero counter"
        );
    }

    /// A provider that never sent state must not grow an empty field: Gemini is
    /// the only one that validates it, and other gateways reject unknown keys.
    #[test]
    fn a_call_without_provider_state_carries_no_extra_content() {
        let m = Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "c1",
            "ls",
            json!({}),
        )]);
        let v = OpenAiProvider::message_json(&m);
        assert!(
            v["tool_calls"][0].get("extra_content").is_none(),
            "unexpected extra_content: {v}"
        );
    }

    /// An invalid payload is dropped (there is nothing else to do with it), but
    /// it must not end the stream: the deltas after it still arrive.
    #[tokio::test]
    async fn an_unparsable_payload_does_not_end_the_stream() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
                    data: {not json at all\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n\
                    data: [DONE]\n\n"
            .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "ab");
    }

    #[test]
    fn request_body_includes_tools_and_tool_messages() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "list files"),
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                    "call_1",
                    "ls",
                    json!({"path": "."}),
                )]),
                Message::tool_result("call_1", "a.txt\nb.txt", false),
            ],
            effort: None,
            effort_support: Default::default(),
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
            effort: None,
            effort_support: Default::default(),
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
            effort: None,
            effort_support: Default::default(),
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
        let s = "héllò wörld";
        let t = truncate(s, 8);
        assert!(t.ends_with('…'));
        // no partial multibyte sequence: must round-trip as valid UTF-8
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert!(s.starts_with(t.trim_end_matches('…')));
    }

    #[test]
    fn requested_max_tokens_uses_completion_tokens_for_o_series_or_reasoning() {
        let mut req = ChatRequest {
            model_id: "gpt-4o".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(1024),
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let body = OpenAiProvider::build_body(&req);
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("max_completion_tokens").is_none());

        req.model_id = "o3-mini".into();
        let body = OpenAiProvider::build_body(&req);
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(body.get("max_tokens").is_none());

        req.model_id = "gpt-5-turbo".into();
        let body = OpenAiProvider::build_body(&req);
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(body.get("max_tokens").is_none());

        req.model_id = "custom-reasoning-model".into();
        req.effort = Some(crate::providers::EffortLevel::Medium);
        req.effort_support = crate::config::EffortSupport {
            control: crate::config::EffortControl::Levels,
            always_on: false,
        };
        let body = OpenAiProvider::build_body(&req);
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(body.get("max_tokens").is_none());
    }
}
