use anyhow::Result;
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
    // the stable system block leads the input; it is rebuilt per request
    // and never stored in the transcript. Volatile parts travel last (see
    // below), so they cannot invalidate the cached history prefix.
    let mut input: Vec<Value> = req
        .system
        .iter()
        .filter(|part| part.cacheable)
        .map(|part| json!({"role": "system", "content": part.text}))
        .collect();

    let tail = crate::providers::volatile_system_text(&req.system);
    let last_user = req.messages.iter().rposition(|m| m.role == Role::User);
    if last_user.is_none() && !tail.trim().is_empty() {
        input.push(json!({
            "role": "system",
            "content": crate::providers::host_tail(&tail),
        }));
    }
    for (i, m) in req.messages.iter().enumerate() {
        if Some(i) == last_user && !tail.trim().is_empty() {
            input.push(json!({
                "role": "system",
                "content": crate::providers::host_tail(&tail),
            }));
        }
        match m.role {
            // legacy: a persisted system message would still map correctly
            Role::System => input.push(json!({"role": "system", "content": m.content})),
            Role::User => {
                if m.images.iter().any(|image| image.marker.is_some()) {
                    let mut content = Vec::new();
                    for part in super::user_parts(m) {
                        match part {
                            super::UserPart::Text(text) => {
                                if !text.is_empty() {
                                    content.push(json!({"type": "input_text", "text": text}));
                                }
                            }
                            super::UserPart::Image(image) => {
                                match crate::media::data_uri(image) {
                                    Ok(uri) => content.push(json!({
                                        "type": "input_image",
                                        "image_url": uri,
                                        "detail": "auto",
                                    })),
                                    Err(_) => content.push(json!({
                                        "type": "input_text",
                                        "text": format!("[image unavailable: {}]", image.name),
                                    })),
                                }
                            }
                        }
                    }
                    input.push(json!({"role": "user", "content": content}));
                } else {
                    input.push(json!({"role": "user", "content": m.content}));
                }
            }
            Role::Assistant => {
                // Replay reasoning items ahead of the assistant message they
                // belong to, in order. Under previous_response_id continuation,
                // the provider already holds them, so they must not be resent.
                // The transport decides, not the remembered id: a stateless
                // request resends the full transcript and must resend these
                // too, or the provider reasons without its own prior work.
                if req.context_transport != super::ContextTransport::PreviousResponse
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
            Role::Tool => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                if m.images.is_empty() {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": m.content,
                    }));
                } else {
                    // Responses documents an array of text/image/file items
                    // here, so no synthetic turn is needed.
                    let mut output = vec![json!({"type": "input_text", "text": m.content})];
                    for image in &m.images {
                        match crate::media::data_uri(image) {
                            Ok(uri) => output.push(json!({
                                "type": "input_image",
                                "image_url": uri,
                                "detail": "auto",
                            })),
                            Err(_) => output.push(json!({
                                "type": "input_text",
                                "text": format!("[image unavailable: {}]", image.name),
                            })),
                        }
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                }
            }
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
        // Reasoning text only streams when summaries are asked for: without
        // `summary` the wire carries reasoning items with empty summaries and
        // no summary deltas to render.
        body["reasoning"] = json!({"effort": e, "summary": "auto"});
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
    // The continuation reference travels only under the transport that
    // means "the provider holds the chain". A stateless request resends
    // the transcript itself: naming an id there duplicates the context,
    // and a stale one 400s a request that would otherwise succeed.
    // sanitize() has already cleared the id for providers that never
    // documented the field.
    if req.context_transport == super::ContextTransport::PreviousResponse
        && let Some(id) = &req.previous_response_id
    {
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
        let http = super::stream_http_client();
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
            // Console Go answers Responses calls statelessly: it retains
            // nothing server-side, so a previous_response_id always 400s
            // ("referenced response not found or expired"). Never offer
            // continuation there — the loop then resends the full
            // transcript in `input`, which loses nothing. Other hosts
            // keep the documented behavior.
            previous_response: !self.url.contains("opencode.ai"),
            ..Default::default()
        }
    }

    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult> {
        let this = self.clone();
        stream! {
            let mut req = req;
            this.sanitize(&mut req);
            let mut body = build_body(&req);
            // stable routing key, same convention as the OpenAI wire above
            if let Some(key) = super::prompt_cache_key(&this.url) {
                body["prompt_cache_key"] = serde_json::json!(key);
            }
            let mut r = this.http.post(&this.url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            r = super::with_opencode_session(r, &this.url);
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
                                        cache_write_tokens: u
                                            .pointer("/input_tokens_details/cache_write_tokens")
                                            .and_then(|x| x.as_u64())
                                            .filter(|&c| c > 0),
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
                                break;
                            }
                            "error" => {
                                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("unknown");
                                super::log_http(&format!("POST {} stream error: {msg}", this.url));
                                yield Err(super::event_error(msg));
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
                                yield Err(super::event_error(msg));
                                return;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        // the headers were fine and the body stopped: the
                        // request never got a complete answer, same verdict
                        // as a request that never completed at all
                        yield Err(super::network_error(e));
                        return;
                    }
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
mod tests;
