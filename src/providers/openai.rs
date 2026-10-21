use anyhow::Result;
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
        Ok(Self {
            http: super::stream_http_client(),
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
                .or_else(|| u.get("prompt_cache_hit_tokens"))
                .or_else(|| u.get("cache_read_input_tokens"))
                .and_then(|c| c.as_u64()),
            cache_write_tokens: u
                .pointer("/prompt_tokens_details/cache_write_tokens")
                .or_else(|| u.get("cache_creation_input_tokens"))
                .and_then(|c| c.as_u64())
                .filter(|&c| c > 0),
            reasoning_tokens: u
                .pointer("/completion_tokens_details/reasoning_tokens")
                .and_then(|c| c.as_u64()),
        })
    }

    /// Chat Completions allows only text in a `tool` message (the API schema
    /// says so outright), so tool-returned pixels and PDFs ride in one
    /// synthetic user message after the tool batch. This exists on the wire
    /// only; the stored transcript still holds them on the tool result.
    fn media_message(
        images: &[&super::ImageAttachment],
        documents: &[&super::DocumentAttachment],
    ) -> Value {
        let mut content = Vec::new();
        for image in images {
            content.push(json!({
                "type": "text",
                "text": format!("image returned by view_image: {}", image.name),
            }));
            match crate::media::data_uri(image) {
                Ok(uri) => content.push(json!({
                    "type": "image_url",
                    "image_url": {"url": uri, "detail": "auto"},
                })),
                Err(_) => content.push(json!({
                    "type": "text",
                    "text": format!("[image unavailable: {}]", image.name),
                })),
            }
        }
        for document in documents {
            content.push(json!({
                "type": "text",
                "text": format!("document returned by view_document: {}", document.name),
            }));
            match crate::media::data_uri_document(document) {
                // Chat Completions accepts only PDF here, which is exactly
                // what view_document produces.
                Ok(uri) => content.push(json!({
                    "type": "file",
                    "file": {"filename": document.name, "file_data": uri},
                })),
                Err(_) => content.push(json!({
                    "type": "text",
                    "text": format!("[document unavailable: {}]", document.name),
                })),
            }
        }
        json!({"role": "user", "content": content})
    }

    fn message_json(m: &super::Message) -> Value {
        match m.role {
            Role::System => json!({"role": "system", "content": m.content}),
            Role::User => {
                if m.images.iter().any(|image| image.marker.is_some()) {
                    let mut content = Vec::new();
                    for part in super::user_parts(m) {
                        match part {
                            super::UserPart::Text(text) => {
                                if !text.is_empty() {
                                    content.push(json!({"type": "text", "text": text}));
                                }
                            }
                            super::UserPart::Image(image) => {
                                match crate::media::data_uri(image) {
                                    Ok(uri) => content.push(json!({
                                        "type": "image_url",
                                        "image_url": {"url": uri, "detail": "auto"},
                                    })),
                                    Err(_) => content.push(json!({
                                        "type": "text",
                                        "text": format!("[image unavailable: {}]", image.name),
                                    })),
                                }
                            }
                        }
                    }
                    json!({"role": "user", "content": content})
                } else {
                    json!({"role": "user", "content": m.content})
                }
            }
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
        // stable prefix first and alone: the host block sits immediately before
        // the last user turn so the request always ends on the user's message
        // (at turn start) or on the tool result (during a tool loop) — never
        // with a trailing host block after a tool result.
        let system = super::stable_system_text(&req.system);
        if !system.trim().is_empty() {
            msgs.push(json!({"role": "system", "content": system}));
        }
        let tail = super::volatile_system_text(&req.system);
        let last_user = req.messages.iter().rposition(|m| m.role == Role::User);
        if last_user.is_none() && !tail.trim().is_empty() {
            msgs.push(json!({"role": "system", "content": super::host_tail(&tail)}));
        }
        let mut pending_images: Vec<&super::ImageAttachment> = Vec::new();
        let mut pending_documents: Vec<&super::DocumentAttachment> = Vec::new();
        for (i, m) in req.messages.iter().enumerate() {
            if Some(i) == last_user && !tail.trim().is_empty() {
                msgs.push(json!({"role": "system", "content": super::host_tail(&tail)}));
            }
            // Flush the media carrier only when the contiguous tool batch
            // ends: tool messages must stay consecutive after the assistant
            // call that requested them.
            if m.role != Role::Tool && !(pending_images.is_empty() && pending_documents.is_empty())
            {
                msgs.push(Self::media_message(&pending_images, &pending_documents));
                pending_images.clear();
                pending_documents.clear();
            }
            if m.role == Role::Tool {
                pending_images.extend(m.images.iter());
                pending_documents.extend(m.documents.iter());
            }
            msgs.push(Self::message_json(m));
        }
        if !(pending_images.is_empty() && pending_documents.is_empty()) {
            msgs.push(Self::media_message(&pending_images, &pending_documents));
        }
        let mut body = json!({
            "model": req.model_id,
            "messages": msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        // openai-compatible reasoning control; servers that do not know
        // the field simply ignore it
        let requested = req
            .effort
            .filter(|l| *l != EffortLevel::Off)
            .and_then(
                |level| match super::effort::plan(level, req.effort_support).wire {
                    super::effort::Wire::Level(effort) => Some(effort),
                    _ => None,
                },
            );
        // Gemini's OpenAI-compat layer takes its reasoning control in
        // `thinking_config` and rejects a body that also carries
        // `reasoning_effort` ("Expected one of either ... found both"), so
        // gemini ids get the level there and nowhere else. `include_thoughts`
        // is what makes the stream carry thought summaries at all; without it
        // the model thinks silently and the wire shows nothing to parse.
        if let Some(effort) = requested {
            if req.model_id.starts_with("gemini-") {
                body["extra_body"] = json!({
                    "google": {
                        "thinking_config": {
                            "thinking_level": effort,
                            "include_thoughts": true
                        }
                    }
                });
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }
        let uses_max_completion = uses_max_completion_tokens(&req.model_id, requested.is_some());
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

    /// Inject explicit `cache_control` breakpoints when talking to OpenRouter
    /// for model families that require explicit markers on Chat Completions
    /// (`anthropic/*`, `claude-*`, `google/*`, `gemini-*`, `qwen/*`).
    ///
    /// - For Anthropic models on OpenRouter, top-level `cache_control` enables
    ///   automatic advancing conversation caching, and marking the leading
    ///   stable `system` message guarantees the system prompt is cached even on
    ///   the first turn.
    /// - For Gemini and Qwen models on OpenRouter, marking the leading `system`
    ///   message and the last eligible conversation message activates explicit
    ///   prefix caching.
    pub fn apply_openrouter_cache_control(body: &mut Value, base_url: &str, model_id: &str) {
        if !base_url.contains("openrouter.ai") {
            return;
        }
        let lower = model_id.to_ascii_lowercase();
        let is_anthropic = lower.starts_with("anthropic/") || lower.contains("claude-");
        let is_explicit_family = is_anthropic
            || lower.starts_with("google/")
            || lower.contains("gemini-")
            || lower.starts_with("qwen/")
            || lower.contains("qwen3");
        if !is_explicit_family {
            return;
        }
        if is_anthropic {
            body["cache_control"] = json!({"type": "ephemeral"});
        }
        let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            return;
        };
        // 1. Mark the leading stable system message if present.
        if let Some(first) = msgs.first_mut()
            && first.get("role").and_then(|r| r.as_str()) == Some("system")
            && let Some(text) = first
                .get("content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty() && !s.starts_with(super::HOST_STATE_LABEL))
                .map(str::to_string)
        {
            first["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        // 2. Mark the last non-host message with plain string content.
        if let Some(last) = msgs.iter_mut().rev().find(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
            matches!(role, "user" | "tool" | "assistant")
                && !content.is_empty()
                && !content.starts_with(super::HOST_STATE_LABEL)
        }) && let Some(text) = last.get("content").and_then(|c| c.as_str()).map(str::to_string)
        {
            last["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
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
        || (name.starts_with("gpt-")
            && !name.starts_with("gpt-3")
            && !name.starts_with("gpt-4"))
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
        let mut body = OpenAiProvider::build_body(&req);
            OpenAiProvider::apply_openrouter_cache_control(
                &mut body,
                &this.base_url,
                &req.model_id,
            );
            // stable routing key: repeated prefixes of this session land on
            // the same worker (Codex / OpenRouter convention). Host-scoped,
            // ignored where unsupported.
            if let Some(key) = super::prompt_cache_key(&this.base_url) {
                body["prompt_cache_key"] = serde_json::json!(key);
            }
            // one-line request summary for gateway debugging (body itself
            // can be 50KB+ of system prompt, so only its shape is logged)
            if let Some(obj) = body.as_object() {
                let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
                keys.sort_unstable();
                super::log_http(&format!(
                    "POST {url} model={} keys=[{}] reasoning_effort={} tools={}",
                    body.get("model").and_then(|m| m.as_str()).unwrap_or("?"),
                    keys.join(","),
                    body.get("reasoning_effort")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".into()),
                    body.get("tools")
                        .and_then(|t| t.as_array())
                        .map(|a| a.len().to_string())
                        .unwrap_or_else(|| "-".into()),
                ));
            }

            let mut r = this.http.post(&url);
            if let Some(k) = &this.api_key { r = r.bearer_auth(k); }
            r = super::with_opencode_session(r, &this.base_url);
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
                        if let Some(err) = v.get("error")
                            && !err.is_null()
                        {
                            let msg = err
                                .get("message")
                                .and_then(|x| x.as_str())
                                .or_else(|| err.as_str())
                                .unwrap_or("stream error");
                            super::log_http(&format!("POST {url} stream error: {msg}"));
                            yield Err(super::event_error(msg));
                            return;
                        }
                        if let Some(id) = v.get("id").and_then(|x| x.as_str())
                            && !response_id_sent
                        {
                            response_id_sent = true;
                            yield Ok(StreamEvent::ResponseId(id.to_string()));
                        }
                        if let Some(u) = Self::map_usage(&v) { yield Ok(StreamEvent::Usage(u)); }
                        let choice = &v["choices"][0];
                        let Some(delta) = choice.get("delta") else { continue };

                        // diagnostic: a chunk that carries neither text nor
                        // tool calls is where an unparsed field hides. With
                        // the http log on, its key list names the field the
                        // stream actually uses (thoughts included).
                        if super::http_log_on()
                            && delta.get("content").is_none()
                            && delta.get("tool_calls").is_none()
                        {
                            super::log_http(&format!(
                                "openai: bare delta keys={:?}",
                                delta
                                    .as_object()
                                    .map(|o| o.keys().collect::<Vec<_>>())
                                    .unwrap_or_default()
                            ));
                        }

                        // streamed text / reasoning. Gemini wraps thought
                        // summaries in <thought> tags and flags their deltas
                        // with extra_content.google.thought; the closing tag
                        // rides on the first UNflagged delta, so the boundary
                        // is stripped here or it leaks into the answer.
                        let is_thought = delta
                            .pointer("/extra_content/google/thought")
                            .and_then(|t| t.as_bool())
                            .unwrap_or(false);
                        if let Some(c) = delta.get("content").and_then(|c| c.as_str())
                            && !c.is_empty()
                        {
                            if is_thought {
                                // which requests actually stream thoughts is
                                // the question every "thinking never shows"
                                // report boils down to; the log answers it
                                super::log_http(&format!("openai: thought delta {}B", c.len()));
                                let c = c.strip_prefix("<thought>").unwrap_or(c);
                                let c = c.strip_suffix("</thought>").unwrap_or(c);
                                if !c.is_empty() {
                                    yield Ok(StreamEvent::Reasoning(c.to_string()));
                                }
                            } else {
                                let c = c.strip_prefix("</thought>").unwrap_or(c);
                                if !c.is_empty() {
                                    yield Ok(StreamEvent::Text(c.to_string()));
                                }
                            }
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
                                // Once a higher tool_call index `idx` starts streaming, any earlier
                                // index `< idx` with complete valid JSON arguments is finished and
                                // can be emitted immediately mid-stream for eager execution.
                                let ready_earlier: Vec<i64> = partials
                                    .range(..idx)
                                    .filter(|(_, p)| {
                                        !p.name.is_empty()
                                            && !p.args.trim().is_empty()
                                            && serde_json::from_str::<Value>(&p.args).is_ok()
                                    })
                                    .map(|(&k, _)| k)
                                    .collect();
                                for k in ready_earlier {
                                    if let Some(p) = partials.remove(&k)
                                        && let Some(req) = p.finish()
                                    {
                                        yield Ok(StreamEvent::ToolCall(req));
                                    }
                                }
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
                    Err(e) => {
                        // headers were fine and the body stopped: same
                        // verdict as a request that never completed at all
                        yield Err(super::network_error(e));
                        return;
                    }
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
mod tests;
