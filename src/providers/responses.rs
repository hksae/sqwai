use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult};
use crate::config::{EffortLevel, ResolvedProvider};

#[derive(Clone)]
pub struct ResponsesProvider {
    http: reqwest::Client,
    url: String,
    api_key: Option<String>,
}

/// One input item per transcript entry, in the shapes the Responses API
/// documents (see `openai.types.responses`): messages carry plain string
/// content, a model's tool call is a `function_call` item keyed by `call_id`,
/// and its result is a separate `function_call_output` item referring to that
/// same id. Before this, results rode as ordinary user turns, so the model had
/// nothing to match them against.
fn input_items(req: &ChatRequest) -> Vec<Value> {
    // the system block leads the input; it is rebuilt per request and never
    // stored in the transcript
    let mut input: Vec<Value> = req
        .system
        .iter()
        .map(|part| json!({"role": "system", "content": part.text}))
        .collect();

    for m in &req.messages {
        match m.role {
            // legacy: a persisted system message would still map correctly
            Role::System => input.push(json!({"role": "system", "content": m.content})),
            Role::User => input.push(json!({"role": "user", "content": m.content})),
            Role::Assistant => {
                // Replay reasoning items ahead of the assistant message they
                // belong to, in order. Under previous_response_id continuation,
                // the provider already holds them, so they must not be resent.
                if req.previous_response_id.is_none()
                    && let Some(state) = &m.provider_state
                    && let Some(items) = state.get("reasoning_items").and_then(|v| v.as_array())
                {
                    for item in items {
                        input.push(item.clone());
                    }
                }
                if !m.content.is_empty() {
                    let mut msg = json!({"role": "assistant", "content": m.content});
                    if let Some(phase) = m
                        .provider_state
                        .as_ref()
                        .and_then(|s| s.get("phase"))
                        .and_then(|p| p.as_str())
                    {
                        msg["phase"] = json!(phase);
                    }
                    input.push(msg);
                }
                for call in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        // arguments travel as a JSON *string*, as on the way out
                        "arguments": call.args.to_string(),
                    }));
                }
            }
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": m.tool_call_id.clone().unwrap_or_default(),
                "output": m.content,
            })),
        }
    }
    input
}

pub fn build_body(req: &ChatRequest) -> Value {
    let mut body = json!({
        "model": req.model_id,
        "input": input_items(req),
        "stream": true,
    });
    if let Some(level) = req.effort.filter(|l| *l != EffortLevel::Off)
        && let super::effort::Wire::Level(e) = super::effort::plan(level, req.effort_support).wire
    {
        body["reasoning"] = json!({"effort": e});
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    // the field is required by the schema and nullable; we do
                    // not generate strict schemas, so it is explicitly false
                    "strict": false,
                }))
                .collect::<Vec<_>>()
        );
    }
    // Only set when the provider documented the field: sanitize() has already
    // cleared it for providers that did not.
    if let Some(id) = &req.previous_response_id {
        body["previous_response_id"] = json!(id);
    }
    if let Some(mt) = req.max_tokens {
        body["max_output_tokens"] = json!(mt);
    }
    body
}

/// A function call being assembled from the stream, keyed by the item id the
/// events carry. `call_id` is what the result must quote later, and it is not
/// the same value as the item id.
#[derive(Default)]
struct PartialCall {
    call_id: String,
    name: String,
    args: String,
}

impl PartialCall {
    fn finish(self) -> Option<super::ToolCallReq> {
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
        Some(super::ToolCallReq::new(self.call_id, self.name, args))
    }
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
            let mut req = req;
            this.sanitize(&mut req);
            let body = build_body(&req);
            let mut r = this.http.post(&this.url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            let resp = match r.json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    super::log_http(&format!("POST {} failed: {e}", this.url));
                    yield Err(super::network_error(e));
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let t = resp.text().await.unwrap_or_default();
                super::log_http(&format!("POST {} -> {status}: {t}", this.url));
                yield Err(super::response_error(status.as_u16(), &short(&t)));
                return;
            }

            let mut es = resp.bytes_stream().eventsource();
            // the Responses API echoes the response object on every event
            let mut response_id_sent = false;
            // item id -> call being assembled
            let mut partials: BTreeMap<String, PartialCall> = BTreeMap::new();
            let mut reasoning_items: Vec<Value> = Vec::new();
            let mut phase: Option<String> = None;
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        if ev.data == "[DONE]" { break; }
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            // eventsource-stream already reassembled the frame, so
                            // this is a syntactically invalid payload, not a partial
                            // one. Dropping it silently loses whatever it carried —
                            // a tool-call delta included.
                            Err(e) => {
                                super::log_http(&format!(
                                    "responses: dropped unparsable SSE payload ({e}): {}",
                                    ev.data.chars().take(200).collect::<String>()
                                ));
                                continue;
                            }
                        };
                        let response_id = v.pointer("/response/id").and_then(|x| x.as_str());
                        if let Some(id) = response_id
                            && !response_id_sent
                        {
                            response_id_sent = true;
                            yield Ok(StreamEvent::ResponseId(id.to_string()));
                        }
                        match ev.event.as_str() {
                            "response.output_text.delta" => {
                                if let Some(t) = v.get("delta").and_then(|x| x.as_str())
                                    && !t.is_empty()
                                {
                                    yield Ok(StreamEvent::Text(t.to_string()));
                                }
                            }
                            "response.reasoning_text.delta"
                            | "response.reasoning_summary_text.delta" => {
                                if let Some(t) = v.get("delta").and_then(|x| x.as_str())
                                    && !t.is_empty()
                                {
                                    yield Ok(StreamEvent::Reasoning(t.to_string()));
                                }
                            }
                            // A call arrives as three events: the item is
                            // announced, its arguments stream in fragments,
                            // and the item is closed. Only the last one is a
                            // complete call.
                            "response.output_item.added" => {
                                if v.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call")
                                    && let Some(id) = v.pointer("/item/id").and_then(|x| x.as_str())
                                {
                                    let slot = partials.entry(id.to_string()).or_default();
                                    if let Some(call_id) = v.pointer("/item/call_id").and_then(|x| x.as_str()) {
                                        slot.call_id = call_id.to_string();
                                    }
                                    if let Some(name) = v.pointer("/item/name").and_then(|x| x.as_str()) {
                                        slot.name = name.to_string();
                                    }
                                    if let Some(args) = v.pointer("/item/arguments").and_then(|x| x.as_str()) {
                                        slot.args.push_str(args);
                                    }
                                } else if v.pointer("/item/type").and_then(|t| t.as_str()) == Some("message")
                                    && let Some(p) = v.pointer("/item/phase").and_then(|x| x.as_str())
                                {
                                    phase = Some(p.to_string());
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                if let Some(id) = v.get("item_id").and_then(|x| x.as_str())
                                    && let Some(delta) = v.get("delta").and_then(|x| x.as_str())
                                {
                                    partials.entry(id.to_string()).or_default().args.push_str(delta);
                                }
                            }
                            "response.function_call_arguments.done" => {
                                // authoritative full string; replaces whatever
                                // the fragments accumulated
                                if let Some(id) = v.get("item_id").and_then(|x| x.as_str())
                                    && let Some(args) = v.get("arguments").and_then(|x| x.as_str())
                                {
                                    partials.entry(id.to_string()).or_default().args = args.to_string();
                                }
                            }
                            "response.output_item.done" => {
                                match v.pointer("/item/type").and_then(|t| t.as_str()) {
                                    Some("function_call") => {
                                        if let Some(id) = v.pointer("/item/id").and_then(|x| x.as_str()) {
                                            let mut slot = partials.remove(id).unwrap_or_default();
                                            if let Some(call_id) = v.pointer("/item/call_id").and_then(|x| x.as_str()) {
                                                slot.call_id = call_id.to_string();
                                            }
                                            if let Some(name) = v.pointer("/item/name").and_then(|x| x.as_str()) {
                                                slot.name = name.to_string();
                                            }
                                            if let Some(args) = v.pointer("/item/arguments").and_then(|x| x.as_str())
                                                && !args.is_empty()
                                            {
                                                slot.args = args.to_string();
                                            }
                                            if let Some(call) = slot.finish() {
                                                yield Ok(StreamEvent::ToolCall(call));
                                            }
                                        }
                                    }
                                    Some("reasoning") => {
                                        if let Some(item) = v.get("item") {
                                            reasoning_items.push(item.clone());
                                        }
                                    }
                                    Some("message") => {
                                        if let Some(p) = v.pointer("/item/phase").and_then(|x| x.as_str()) {
                                            phase = Some(p.to_string());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "response.completed" | "response.incomplete" => {
                                if let Some(u) = v.pointer("/response/usage") {
                                    yield Ok(StreamEvent::Usage(super::Usage {
                                        prompt_tokens: u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                                        completion_tokens: u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                                        cached_tokens: u.pointer("/input_tokens_details/cached_tokens").and_then(|x| x.as_u64()),
                                        reasoning_tokens: u.pointer("/output_tokens_details/reasoning_tokens").and_then(|x| x.as_u64()),
                                    }));
                                }
                                // safety net: a server that closes the
                                // response without an output_item.done must
                                // not swallow the call
                                for (_, p) in std::mem::take(&mut partials) {
                                    if let Some(call) = p.finish() {
                                        yield Ok(StreamEvent::ToolCall(call));
                                    }
                                }
                                if ev.event.as_str() == "response.completed" { break; }
                            }
                            "error" => {
                                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("unknown");
                                super::log_http(&format!("POST {} stream error: {msg}", this.url));
                                yield Err(anyhow!("provider error: {msg}"));
                                return;
                            }
                            "response.failed" => {
                                let msg = v
                                    .pointer("/response/error/message")
                                    .or_else(|| v.pointer("/error/message"))
                                    .and_then(|x| x.as_str())
                                    .or_else(|| {
                                        v.pointer("/response/error/code")
                                            .and_then(|x| x.as_str())
                                    })
                                    .unwrap_or("response.failed");
                                super::log_http(&format!("POST {} response failed: {msg}", this.url));
                                yield Err(anyhow!("provider error: {msg}"));
                                return;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => { yield Err(anyhow!("stream error: {e}")); return; }
                }
            }
            if !reasoning_items.is_empty() || phase.is_some() {
                let mut state = json!({});
                if !reasoning_items.is_empty() {
                    state["reasoning_items"] = json!(reasoning_items);
                }
                if let Some(p) = phase {
                    state["phase"] = json!(p);
                }
                yield Ok(StreamEvent::ProviderState(state));
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

    /// Serves one canned SSE body and closes. Named events, unlike the Chat
    /// Completions stream, so the helper writes `event:` lines verbatim.
    fn sse_server(body: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
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
            // the request body must be drained or the client sees a reset
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok();
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        (format!("http://{addr}/v1"), h)
    }

    async fn collect(url: String) -> Vec<StreamEvent> {
        let p = ResponsesProvider::new(&ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Responses,
            base_url: url,
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object"}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        p.stream_chat(req)
            .filter_map(|e| async move { e.ok() })
            .collect()
            .await
    }

    fn request_at(level: EffortLevel, control: crate::config::EffortControl) -> ChatRequest {
        ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: Some(level),
            effort_support: crate::config::EffortSupport {
                control,
                always_on: false,
            },
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        }
    }

    /// Named levels go on the wire literally (transparent slider).
    #[test]
    fn effort_reaches_the_body_at_the_declared_level() {
        use crate::config::EffortControl;
        let b = build_body(&request_at(EffortLevel::Max, EffortControl::Named));
        assert_eq!(b["reasoning"]["effort"], "max");
        let b = build_body(&request_at(EffortLevel::Xhigh, EffortControl::Named));
        assert_eq!(b["reasoning"]["effort"], "xhigh");
        let b = build_body(&request_at(EffortLevel::Low, EffortControl::Named));
        assert_eq!(b["reasoning"]["effort"], "low");
        // a model with no reasoning control gets no parameter at all
        let b = build_body(&request_at(EffortLevel::High, EffortControl::None));
        assert!(b.get("reasoning").is_none(), "unexpected reasoning: {b}");
        let b = build_body(&request_at(EffortLevel::Off, EffortControl::Named));
        assert!(b.get("reasoning").is_none());
    }

    #[test]
    fn body_roles_and_reasoning() {
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![crate::providers::SystemPart::cached("s")],
            messages: vec![Message::new(Role::User, "hi")],
            effort: Some(EffortLevel::Medium),
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req);
        assert_eq!(b["model"], "gpt-x");
        assert_eq!(b["reasoning"]["effort"], "medium");
        assert_eq!(b["input"][0]["role"], "system");
        assert_eq!(b["input"][1]["role"], "user");
        // Content is a plain string now. The structured form this used to
        // build tagged *every* role with `input_text`, which is only correct
        // for input roles — an assistant item takes `output_text`, so the
        // moment assistant turns had to be replayed for a tool loop the old
        // shape was wrong. A string is documented for every role and has no
        // such trap.
        assert_eq!(b["input"][0]["content"], "s");
        assert_eq!(b["input"][1]["content"], "hi");
    }

    /// The round trip that #47 was about: a call the model made and the result
    /// we send back have to be two items joined by `call_id`. As user turns
    /// (what this did before) the model has nothing to match a result against.
    #[test]
    fn a_tool_call_and_its_result_are_joined_by_call_id() {
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "read the file"),
                Message::new(Role::Assistant, "on it").with_tool_calls(vec![
                    crate::providers::ToolCallReq::new(
                        "call_1",
                        "read",
                        json!({"file_path": "a.rs"}),
                    ),
                ]),
                Message::tool_result("call_1", "fn main() {}", false),
            ],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req);

        // tools are flat here, not nested under `function` as in Chat Completions
        assert_eq!(b["tools"][0]["type"], "function");
        assert_eq!(b["tools"][0]["name"], "read");
        assert!(b["tools"][0].get("parameters").is_some());
        assert_eq!(b["tools"][0]["strict"], false);

        let input = b["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            4,
            "user, assistant text, call, output: {input:#?}"
        );
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "read");
        // arguments are a JSON string on the wire, not an object
        assert_eq!(input[2]["arguments"], "{\"file_path\":\"a.rs\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "fn main() {}");
    }

    /// An assistant turn that is only a tool call must not produce an empty
    /// message item: a blank assistant message is a wasted item at best and
    /// rejected at worst.
    #[test]
    fn a_silent_tool_call_produces_no_empty_message() {
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq::new("c1", "ls", json!({})),
            ])],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let input = build_body(&req)["input"].as_array().unwrap().clone();
        assert_eq!(input.len(), 1, "{input:#?}");
        assert_eq!(input[0]["type"], "function_call");
        // and a request without tools carries no `tools` key at all
        assert!(build_body(&req).get("tools").is_none());
    }

    /// Assembling a call from the three events the API sends for it. The item
    /// id and the `call_id` are different values, and it is the `call_id` the
    /// result has to quote — mixing them up breaks the loop on the next turn.
    #[tokio::test]
    async fn a_streamed_function_call_is_assembled_from_its_events() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_9\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"item_id\":\"fc_1\",\"delta\":\"{\\\"file_path\\\":\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"item_id\":\"fc_1\",\"delta\":\"\\\"src/main.rs\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_9\",\"name\":\"read\",\"arguments\":\"{\\\"file_path\\\":\\\"src/main.rs\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        )
        .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();

        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "exactly one call: {events:?}");
        assert_eq!(calls[0].name, "read");
        assert_eq!(
            calls[0].id, "call_9",
            "the result must quote call_id, not the item id"
        );
        assert_eq!(calls[0].args["file_path"], "src/main.rs");
    }

    /// A server that closes the response without an `output_item.done` must
    /// not swallow the call it already announced.
    #[tokio::test]
    async fn a_call_left_open_at_completion_is_still_emitted() {
        let body = concat!(
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_7\",\"name\":\"ls\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.done\n",
            "data: {\"item_id\":\"fc_2\",\"arguments\":\"{}\"}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_2\"}}\n\n",
        )
        .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{events:?}");
        assert_eq!(calls[0].id, "call_7");
        assert_eq!(calls[0].name, "ls");
    }

    #[test]
    fn previous_response_id_only_reaches_the_wire_when_documented() {
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: Some("resp_1".into()),
            context_transport: crate::providers::ContextTransport::PreviousResponse,
        };
        assert_eq!(build_body(&req)["previous_response_id"], "resp_1");

        // a provider that does not document the field strips it in sanitize()
        let mut stripped = req.clone();
        stripped.previous_response_id = None;
        stripped.context_transport = crate::providers::ContextTransport::Stateless;
        assert!(build_body(&stripped).get("previous_response_id").is_none());
    }

    #[test]
    fn requested_max_tokens_mapped_to_max_output_tokens() {
        let req = ChatRequest {
            model_id: "gpt-4.5".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(4096),
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let body = build_body(&req);
        assert_eq!(body["max_output_tokens"], 4096);
    }

    #[tokio::test]
    async fn response_failed_event_yields_err() {
        use futures::StreamExt;
        let body = concat!(
            "event: response.failed\n",
            "data: {\"response\":{\"error\":{\"message\":\"server overloaded\"}}}\n\n",
        )
        .to_string();
        let (url, h) = sse_server(body);
        let p = ResponsesProvider::new(&ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Responses,
            base_url: url,
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let mut stream = p.stream_chat(req);
        let first = stream.next().await;
        h.join().unwrap();
        assert!(first.is_some());
        let err = first.unwrap().unwrap_err();
        assert!(err.to_string().contains("server overloaded"), "{err}");
    }

    /// Reasoning items (with encrypted_content) and phase are captured from
    /// output_item.done and emitted as ProviderState at the end of the turn.
    #[tokio::test]
    async fn streamed_reasoning_items_and_phase_are_captured_in_provider_state() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[]}}\n\n",
            "event: response.reasoning_text.delta\n",
            "data: {\"item_id\":\"rs_1\",\"delta\":\"thinking\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking\"}],\"encrypted_content\":\"ENC_SECRET_BLOB\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"item_id\":\"msg_1\",\"delta\":\"I am looking into it\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{\"type\":\"output_text\",\"text\":\"I am looking into it\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        )
        .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();

        let state = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ProviderState(s) => Some(s.clone()),
                _ => None,
            })
            .expect("provider state must be emitted");

        let reasoning_items = state["reasoning_items"]
            .as_array()
            .expect("reasoning items present");
        assert_eq!(reasoning_items.len(), 1);
        assert_eq!(reasoning_items[0]["type"], "reasoning");
        assert_eq!(reasoning_items[0]["id"], "rs_1");
        assert_eq!(reasoning_items[0]["encrypted_content"], "ENC_SECRET_BLOB");
        assert_eq!(state["phase"], "commentary");
    }

    /// Replaying reasoning items ahead of the assistant message in order, and
    /// preserving phase on the assistant message.
    #[test]
    fn reasoning_items_and_phase_are_replayed_ahead_of_assistant_message() {
        let req = ChatRequest {
            model_id: "gpt-5.3-codex".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "run check"),
                Message::new(Role::Assistant, "checking now")
                    .with_provider_state(Some(json!({
                        "reasoning_items": [
                            {
                                "type": "reasoning",
                                "id": "rs_42",
                                "encrypted_content": "ENCRYPTED_OPAQUE_DATA",
                                "summary": [{"type": "summary_text", "text": "let me verify"}]
                            }
                        ],
                        "phase": "commentary"
                    })))
                    .with_tool_calls(vec![crate::providers::ToolCallReq::new(
                        "c1",
                        "check",
                        json!({}),
                    )]),
                Message::tool_result("c1", "all ok", false),
            ],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "check".into(),
                description: "run checks".into(),
                parameters: json!({"type": "object"}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req);
        let input = b["input"].as_array().unwrap();
        assert_eq!(
            input.len(),
            5,
            "user, reasoning, assistant with phase, call, output: {input:#?}"
        );

        // input[0]: user
        assert_eq!(input[0]["role"], "user");

        // input[1]: reasoning item replayed verbatim ahead of assistant
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_42");
        assert_eq!(input[1]["encrypted_content"], "ENCRYPTED_OPAQUE_DATA");

        // input[2]: assistant message with phase
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"], "checking now");
        assert_eq!(input[2]["phase"], "commentary");

        // input[3]: function_call
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");

        // input[4]: function_call_output
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "c1");
    }

    /// Under previous_response_id continuation, reasoning items must not be
    /// replayed because the provider already holds them.
    #[test]
    fn reasoning_items_are_not_replayed_under_previous_response_id_continuation() {
        let req = ChatRequest {
            model_id: "gpt-5.3-codex".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::Assistant, "done").with_provider_state(Some(json!({
                    "reasoning_items": [
                        {
                            "type": "reasoning",
                            "id": "rs_100",
                            "encrypted_content": "OPAQUE"
                        }
                    ],
                    "phase": "final_answer"
                }))),
                Message::new(Role::User, "next question"),
            ],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: Some("resp_999".into()),
            context_transport: crate::providers::ContextTransport::PreviousResponse,
        };
        let b = build_body(&req);
        let input = b["input"].as_array().unwrap();
        assert!(
            !input
                .iter()
                .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
        );
    }

    /// Serializing a message without provider_state omits the field, and
    /// legacy JSON without provider_state deserializes with None.
    #[test]
    fn message_without_provider_state_omits_field_and_deserializes() {
        let m = Message::new(Role::Assistant, "hello");
        let serialized = serde_json::to_string(&m).unwrap();
        assert!(
            !serialized.contains("provider_state"),
            "none field must be omitted: {serialized}"
        );

        let parsed: Message =
            serde_json::from_str(r#"{"role":"assistant","content":"hello"}"#).unwrap();
        assert!(parsed.provider_state.is_none());

        let with_state = m.with_provider_state(Some(json!({"phase": "final_answer"})));
        let serialized_state = serde_json::to_string(&with_state).unwrap();
        assert!(serialized_state.contains("provider_state"));
        let parsed_state: Message = serde_json::from_str(&serialized_state).unwrap();
        assert_eq!(
            parsed_state.provider_state.unwrap()["phase"],
            "final_answer"
        );
    }
}
