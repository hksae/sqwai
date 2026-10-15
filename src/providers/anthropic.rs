use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult, ToolCallReq};

/// History length from which the middle anchor marker pays off: below this
/// the end marker's ~20-block lookback already covers the whole history.
const MID_HISTORY_MARKER_MSGS: usize = 10;
use crate::config::{EffortLevel, ResolvedProvider};

#[derive(Clone)]
pub struct AnthropicProvider {
    http: reqwest::Client,
    url: String,
    api_key: String,
}

/// content blocks for one message (anthropic wire format)
/// Thinking the model did on an earlier turn, replayed verbatim: the API
/// demands the exact text+signature back (or the redacted block untouched)
/// whenever a turn that thought is followed by tool use, and answers 400
/// without them. Only while thinking is still on for this request — blocks
/// from an effort that is off now are not legal input.
fn thinking_blocks(m: &super::Message, thinking_on: bool) -> Vec<Value> {
    if thinking_on
        && m.role == Role::Assistant
        && let Some(state) = &m.provider_state
        && let Some(saved) = state.get("thinking_blocks").and_then(|v| v.as_array())
    {
        return saved.to_vec();
    }
    Vec::new()
}

fn content_blocks(m: &super::Message, thinking_on: bool) -> Vec<Value> {
    let mut blocks = thinking_blocks(m, thinking_on);
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
/// cache key. Up to four markers, inside the budget: tools, the end
/// of the stable system prefix, the last history message — plus a middle
/// history anchor on long histories (see below). Volatile parts
/// travel unmarked after the history, so a changed date or git status costs
/// only the tail instead of the cached prefix behind it.
pub fn build_body(req: &ChatRequest, default_max_tokens: u32, cache_breakpoints: bool) -> Value {
    // What the caller asked for, falling back to the provider default. This
    // used to ignore `req.max_tokens` entirely and always send the default, so
    // a request for a larger answer was silently capped.
    let base_max_tokens = req.max_tokens.unwrap_or(default_max_tokens);

    // Anthropic accepts at most MAX_CACHE_BREAKPOINTS `cache_control` markers
    // per request and answers 400 beyond that. The stable region is one
    // prefix — tools plus cacheable system parts, byte-identical every
    // turn — so one marker after each region end suffices; per-part markers
    // only made sense while volatile parts rode inside `system`.
    let tools_breakpoint = cache_breakpoints && !req.tools.is_empty();

    // Saved thinking replays only while thinking is on for this request.
    // Same condition as the `thinking` param below: blocks from an effort
    // that is off now are not legal input.
    let thinking_on = req
        .effort
        .filter(|l| *l != EffortLevel::Off)
        .is_some_and(|level| {
            matches!(
                super::effort::plan(level, req.effort_support).wire,
                super::effort::Wire::Budget(_)
            )
        });

    let stable: Vec<&super::SystemPart> = req.system.iter().filter(|part| part.cacheable).collect();
    let mut system: Vec<Value> = stable
        .iter()
        .map(|part| json!({"type": "text", "text": part.text}))
        .collect();
    if cache_breakpoints && !stable.is_empty() {
        let last = system.len() - 1;
        system[last]["cache_control"] = json!({"type": "ephemeral"});
    }
    let tail = super::volatile_system_text(&req.system);
    let last_user = req.messages.iter().rposition(|m| m.role == Role::User);
    if last_user.is_none() && !tail.trim().is_empty() {
        system.push(json!({"type": "text", "text": super::host_tail(&tail)}));
    }
    let mut msgs: Vec<Value> = Vec::new();
    for (i, m) in req.messages.iter().enumerate() {
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
        let mut m_blocks = content_blocks(m, thinking_on);
        if Some(i) == last_user && !tail.trim().is_empty() {
            m_blocks.insert(0, json!({"type": "text", "text": super::host_tail(&tail)}));
        }
        // merge consecutive same-role turns (API requires alternation);
        // tool results must land in a user turn right after the assistant
        let same_role = msgs
            .last()
            .and_then(|p| p.get("role"))
            .and_then(|r| r.as_str())
            == Some(role);
        if same_role
            && let Some(arr) = msgs
                .last_mut()
                .and_then(|p| p.get_mut("content"))
                .and_then(|c| c.as_array_mut())
        {
            // thinking-first survives the merge: saved thinking goes ahead
            // of the combined content, the rest extends it as before.
            arr.splice(..0, thinking_blocks(m, thinking_on));
            let mut rest = content_blocks(m, false);
            if Some(i) == last_user && !tail.trim().is_empty() {
                rest.insert(0, json!({"type": "text", "text": super::host_tail(&tail)}));
            }
            arr.extend(rest);
            continue;
        }
        msgs.push(json!({
            "role": role,
            "content": m_blocks,
        }));
    }
    // breakpoint on the last history message: everything before it —
    // tools, stable system, earlier history — is the cached prefix.
    if cache_breakpoints
        && let Some(last) = msgs.last_mut()
        && let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut())
        && let Some(block) = arr.last_mut()
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    // fourth marker, mid-history: a breakpoint reaches ~20 blocks back, so
    // one huge parallel tool batch between the stable prefix and the end
    // marker would orphan everything before it. A middle anchor keeps the
    // first half hittable. Only on long histories — adjacent markers buy
    // nothing, and every marker is a potential cache write.
    if cache_breakpoints && msgs.len() >= MID_HISTORY_MARKER_MSGS {
        let mid = msgs.len() / 2;
        if let Some(arr) = msgs[mid].get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(block) = arr.last_mut()
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
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

    // one mapping for the whole codebase: what goes on the wire and what the
    // UI reports come from the same plan
    if let Some(level) = req.effort.filter(|l| *l != EffortLevel::Off)
        && let super::effort::Wire::Budget(b) = super::effort::plan(level, req.effort_support).wire
        && b > 0
    {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": b});
        body["max_tokens"] = json!((base_max_tokens + b).min(64_000));
    }

    if !req.tools.is_empty() {
        let last = req.tools.len() - 1;
        body["tools"] = json!(
            req.tools
                .iter()
                .enumerate()
                .map(|(index, t)| {
                    let mut tool = json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    });
                    // puts the tool schemas in the stable prefix with a
                    // breakpoint after them. A marker on the last tool caches
                    // the whole array: they are byte-identical every turn, and
                    // without this they were re-sent uncached on every request.
                    if tools_breakpoint && index == last {
                        tool["cache_control"] = json!({"type": "ephemeral"});
                    }
                    tool
                })
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
            .user_agent(super::USER_AGENT)
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
            let resp = match super::with_opencode_session(
                this.http
                    .post(&this.url)
                    .header("x-api-key", &this.api_key)
                    .header("anthropic-version", "2023-06-01"),
                &this.url,
            )
            .json(&body)
            .send()
            .await
            {
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

            // index -> accumulating tool_use input
            let mut partials: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
            // index -> accumulating thinking block (text + its signature).
            // A turn can think several times (e.g. again after tool results),
            // so each block is kept under its own index, in wire order.
            let mut thinking: BTreeMap<i64, ThinkingBlock> = BTreeMap::new();

            let mut es = resp.bytes_stream().eventsource();
            let mut out_tokens: u64 = 0;
            while let Some(ev) = es.next().await {
                match ev {
                    Ok(ev) => {
                        let v: Value = match serde_json::from_str(&ev.data) {
                            Ok(v) => v,
                            // eventsource-stream already reassembled the frame, so
                            // this is a syntactically invalid payload, not a partial
                            // one. Dropping it silently loses whatever it carried —
                            // a tool-call delta included.
                            Err(e) => {
                                super::log_http(&format!(
                                    "anthropic: dropped unparsable SSE payload ({e}): {}",
                                    ev.data.chars().take(200).collect::<String>()
                                ));
                                continue;
                            }
                        };
                        match ev.event.as_str() {
                            "message_start" => {
                                if let Some(id) = v.pointer("/message/id").and_then(|x| x.as_str()) {
                                    yield Ok(StreamEvent::ResponseId(id.to_string()));
                                }
                                let inp = v.pointer("/message/usage/input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                                let cached = v.pointer("/message/usage/cache_read_input_tokens").and_then(|x| x.as_u64());
                                let created = v.pointer("/message/usage/cache_creation_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                                let total_prompt = inp + cached.unwrap_or(0) + created;
                                yield Ok(StreamEvent::Usage(super::Usage {
                                    prompt_tokens: total_prompt,
                                    completion_tokens: 0,
                                    cached_tokens: cached,
                                    cache_write_tokens: (created > 0).then_some(created),
                                    // the Messages API bills thinking inside
                                    // output_tokens and reports no separate
                                    // counter, so there is nothing to claim
                                    reasoning_tokens: None,
                                }));
                            }
                            "content_block_start" => {
                                let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                match v.pointer("/content_block/type").and_then(|x| x.as_str()) {
                                    Some("tool_use") => {
                                        let id = v.pointer("/content_block/id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                        let name = v.pointer("/content_block/name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                                        partials.insert(idx, (id, name, String::new()));
                                    }
                                    // a thinking block opens: its text and
                                    // signature arrive as deltas below
                                    Some("thinking") => {
                                        thinking.entry(idx).or_default();
                                    }
                                    // redacted thinking carries no text to
                                    // accumulate — the opaque payload must go
                                    // back verbatim, exactly as received
                                    Some("redacted_thinking") => {
                                        if let Some(data) = v
                                            .pointer("/content_block/data")
                                            .and_then(|x| x.as_str())
                                        {
                                            thinking.entry(idx).or_default().redacted =
                                                Some(data.to_string());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_delta" => {
                                let kind = v.pointer("/delta/type").and_then(|x| x.as_str()).unwrap_or("");
                                match kind {
                                    "text_delta" => {
                                        if let Some(t) = v.pointer("/delta/text").and_then(|x| x.as_str())
                                            && !t.is_empty()
                                        {
                                            yield Ok(StreamEvent::Text(t.to_string()));
                                        }
                                    }
                                    "thinking_delta" => {
                                        if let Some(t) = v.pointer("/delta/thinking").and_then(|x| x.as_str())
                                            && !t.is_empty()
                                        {
                                            let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                            thinking.entry(idx).or_default().text.push_str(t);
                                            yield Ok(StreamEvent::Reasoning(t.to_string()));
                                        }
                                    }
                                    // the signature is what makes a replayed
                                    // thinking block legal: without it the
                                    // next request with tool use is a 400.
                                    // It used to fall into the ignore arm.
                                    "signature_delta" => {
                                        if let Some(s) = v.pointer("/delta/signature").and_then(|x| x.as_str())
                                            && !s.is_empty()
                                        {
                                            let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                            thinking.entry(idx).or_default().signature.push_str(s);
                                        }
                                    }
                                    "input_json_delta" => {
                                        let idx = v.pointer("/index").and_then(|x| x.as_i64()).unwrap_or(0);
                                        if let Some(pj) = v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                                            && let Some(slot) = partials.get_mut(&idx)
                                        {
                                            slot.2.push_str(pj);
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
                                    yield Ok(StreamEvent::ToolCall(ToolCallReq::new(id, name, args_v)));
                                }
                            }
                            "message_delta" => {
                                if let Some(u) = v.get("usage") {
                                    out_tokens = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(out_tokens);
                                    yield Ok(StreamEvent::Usage(super::Usage {
                                        prompt_tokens: 0,
                                        completion_tokens: out_tokens,
                                        cached_tokens: None,
                                        cache_write_tokens: None,
                                        reasoning_tokens: None,
                                    }));
                                }
                            }
                            "message_stop" => break,
                            "error" => {
                                let msg = v.pointer("/error/message").and_then(|x| x.as_str()).unwrap_or("unknown");
                                super::log_http(&format!("POST {} stream error: {msg}", this.url));
                                yield Err(super::event_error(msg));
                                return;
                            }
                            _ => {}
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
            // Thinking the turn did travels with it as provider state and is
            // replayed verbatim ahead of its own assistant message (see
            // `content_blocks`): the next request replays the exact blocks or
            // the API refuses it. Same end-of-turn emission as the Responses
            // wire's reasoning items.
            let saved: Vec<Value> = thinking
                .into_values()
                .filter_map(ThinkingBlock::finish)
                .collect();
            if !saved.is_empty() {
                yield Ok(StreamEvent::ProviderState(json!({"thinking_blocks": saved})));
            }
        }
        .boxed()
    }
}

/// One thinking block accumulating in the stream, stored wire-ready: replay
/// is a verbatim clone (the Responses wire's reasoning-items philosophy —
/// the host never interprets provider-owned bytes, it just hands them back).
#[derive(Default)]
struct ThinkingBlock {
    text: String,
    signature: String,
    redacted: Option<String>,
}

impl ThinkingBlock {
    fn finish(self) -> Option<Value> {
        if let Some(data) = self.redacted {
            return Some(json!({"type": "redacted_thinking", "data": data}));
        }
        if self.text.is_empty() || self.signature.is_empty() {
            // Half a block is not replayable either way: thinking without a
            // signature is a 400, and a signature without text matches
            // nothing. The API always sends both; log and drop the anomaly
            // instead of poisoning the next request.
            super::log_http(&format!(
                "anthropic: dropped incomplete thinking block (text={} signature={} bytes)",
                self.text.len(),
                self.signature.len()
            ));
            return None;
        }
        Some(json!({
            "type": "thinking",
            "thinking": self.text,
            "signature": self.signature,
        }))
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
