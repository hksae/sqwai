use anyhow::{Result, anyhow};
use async_stream::stream;
use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{ChatRequest, Provider, Role, StreamEvent, StreamResult, ToolCallReq};

/// Anthropic accepts at most four `cache_control` markers in one request and
/// rejects the request beyond that.
const MAX_CACHE_BREAKPOINTS: usize = 4;
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
pub fn build_body(req: &ChatRequest, default_max_tokens: u32, cache_breakpoints: bool) -> Value {    // What the caller asked for, falling back to the provider default. This
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

    let stable: Vec<&super::SystemPart> =
        req.system.iter().filter(|part| part.cacheable).collect();
    let mut system: Vec<Value> = stable
        .iter()
        .map(|part| json!({"type": "text", "text": part.text}))
        .collect();
    if cache_breakpoints && !stable.is_empty() {
        let last = system.len() - 1;
        system[last]["cache_control"] = json!({"type": "ephemeral"});
    }
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
        if same_role
            && let Some(arr) = msgs
                .last_mut()
                .and_then(|p| p.get_mut("content"))
                .and_then(|c| c.as_array_mut())
        {
            // thinking-first survives the merge: saved thinking goes ahead
            // of the combined content, the rest extends it as before.
            arr.splice(..0, thinking_blocks(m, thinking_on));
            arr.extend(content_blocks(m, false));
            continue;
        }
        msgs.push(json!({
            "role": role,
            "content": content_blocks(m, thinking_on),
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
        if let Some(arr) = msgs[mid]
            .get_mut("content")
            .and_then(|c| c.as_array_mut())
            && let Some(block) = arr.last_mut()
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    // volatile tail, unmarked and last: date, git status, nudges. Merged
    // into a trailing user turn when the API's alternation allows it.
    let tail = super::volatile_system_text(&req.system);
    if !tail.trim().is_empty() {
        let text = super::host_tail(&tail);
        let tail_block = json!({"type": "text", "text": text});
        let merge = msgs
            .last()
            .and_then(|p| p.get("role"))
            .and_then(|r| r.as_str())
            == Some("user");
        if merge
            && let Some(arr) = msgs
                .last_mut()
                .and_then(|p| p.get_mut("content"))
                .and_then(|c| c.as_array_mut())
        {
            arr.push(tail_block);
        } else {
            msgs.push(json!({
                "role": "user",
                "content": [tail_block],
            }));
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
    // UI reports come from the same plan (§5.1)
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
                    // §3.2 puts the tool schemas in the stable prefix with a
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
                                yield Err(anyhow!("provider error: {msg}"));
                                return;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => { yield Err(anyhow!("stream error: {e}")); return; }
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

fn short(s: &str) -> String {    let mut cut = 500.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

#[cfg(test)]
mod tests {
    /// The Messages API expresses effort as a token budget, which is what a
    /// model on this provider resolves to in `ModelConfig::effort_support`.
    fn budget_support() -> crate::config::EffortSupport {
        crate::config::EffortSupport {
            control: crate::config::EffortControl::Budget,
            always_on: false,
        }
    }

    use super::*;
    use crate::providers::Message;

    /// The caller's own `max_tokens` has to reach the wire. It used to be
    /// ignored: `build_body` read only its default argument, so a request for
    /// a longer answer was silently capped at 8192.
    #[test]
    fn requested_max_tokens_is_honoured() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![],
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(32_000),
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        assert_eq!(build_body(&req, 8192, false)["max_tokens"], 32_000);

        // no request of its own: the provider default still applies
        let mut without = req.clone();
        without.max_tokens = None;
        assert_eq!(build_body(&without, 8192, false)["max_tokens"], 8192);

        // with effort on, the budget is added to what the caller asked for,
        // not to the default
        let mut with_effort = req.clone();
        with_effort.effort = Some(EffortLevel::Medium);
        with_effort.effort_support = budget_support();
        let body = build_body(&with_effort, 8192, false);
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert_eq!(
            body["max_tokens"].as_u64().unwrap(),
            (32_000 + budget).min(64_000)
        );
    }

    /// Tools and the stable system prefix carry breakpoints; volatile parts
    /// travel unmarked after the history. Markers stay within budget no
    /// matter how many stable parts exist, because only region ends are
    /// marked — never each part.
    #[test]
    fn tool_schemas_carry_the_first_cache_breakpoint() {
        let tool = |name: &str| crate::providers::ToolSpec {
            name: name.into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        };
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![
                crate::providers::SystemPart::cached("stable"),
                crate::providers::SystemPart::volatile("anchor"),
            ],
            messages: vec![],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![tool("read"), tool("write"), tool("bash")],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };

        let body = build_body(&req, 8192, true);
        let tools = body["tools"].as_array().unwrap();
        assert!(
            tools[0].get("cache_control").is_none() && tools[1].get("cache_control").is_none(),
            "only the last tool carries the marker: {tools:?}"
        );
        assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
        // stable system keeps one trailing marker; volatile rides the tail
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "stable");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert!(msgs[0]["content"][0].get("cache_control").is_none());
        assert!(
            msgs[0]["content"][0]["text"].as_str().unwrap().contains("anchor"),
            "volatile tail travels last: {}",
            msgs[0]["content"][0]["text"]
        );

        // and nothing is marked for a provider without a documented cache
        let uncached = build_body(&req, 8192, false);
        assert!(
            uncached["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t.get("cache_control").is_none())
        );
    }

    /// Markers stay within budget by construction: tools, end of stable
    /// system, end of history — three regions, never one per part, no
    /// matter how many parts exist.
    #[test]
    fn cache_breakpoints_stay_within_the_provider_limit() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: (0..6)
                .map(|i| crate::providers::SystemPart::cached(format!("part {i}")))
                .collect(),
            messages: vec![crate::providers::Message::new(
                crate::providers::Role::User,
                "hi",
            )],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "read".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let body = build_body(&req, 8192, true);
        let marked = body["system"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part.get("cache_control").is_some())
            .count()
            + body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool.get("cache_control").is_some())
                .count()
            + body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                .filter(|block| block.get("cache_control").is_some())
                .count();
        assert_eq!(marked, 3, "{body}");
        assert!(marked <= MAX_CACHE_BREAKPOINTS, "{body}");
    }

    /// Long histories earn the middle anchor: a breakpoint reaches ~20
    /// blocks back, so one huge tool batch would otherwise orphan the first
    /// half. Short histories stay at three markers.
    #[test]
    fn long_histories_carry_a_middle_anchor_marker() {
        fn count_marked(body: &serde_json::Value) -> usize {
            body["system"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|part| part.get("cache_control").is_some())
                .count()
                + body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|tool| tool.get("cache_control").is_some())
                    .count()
                + body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                    .filter(|block| block.get("cache_control").is_some())
                    .count()
        }
        fn history_req(n: usize) -> ChatRequest {
            ChatRequest {
                model_id: "m".into(),
                system: vec![crate::providers::SystemPart::cached("stable")],
                messages: (0..n)
                    .map(|i| {
                        // alternating roles: same-role turns merge, which
                        // would collapse the history under test
                        let role = if i % 2 == 0 {
                            crate::providers::Role::User
                        } else {
                            crate::providers::Role::Assistant
                        };
                        crate::providers::Message::new(role, format!("turn {i}"))
                    })
                    .collect(),
                effort: None,
                effort_support: Default::default(),
                max_tokens: None,
                tools: vec![crate::providers::ToolSpec {
                    name: "read".into(),
                    description: "d".into(),
                    parameters: json!({"type": "object"}),
                }],
                previous_response_id: None,
                context_transport: crate::providers::ContextTransport::Stateless,
            }
        }
        // short history: tools + system + end, no middle anchor
        let short = build_body(&history_req(4), 8192, true);
        assert_eq!(count_marked(&short), 3, "{short}");
        // long history: the middle message carries the fourth marker
        let long = build_body(&history_req(12), 8192, true);
        assert_eq!(count_marked(&long), 4, "{long}");
        let msgs = long["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 12);
        // consecutive user turns merge (alternation), so count distinct
        // marked messages rather than positions: exactly two of them
        let marked_msgs = msgs
            .iter()
            .filter(|m| {
                m["content"]
                    .as_array()
                    .is_some_and(|arr| arr.iter().any(|b| b.get("cache_control").is_some()))
            })
            .count();
        assert_eq!(marked_msgs, 2, "middle anchor plus end marker: {long}");
    }

    #[test]
    fn body_has_cache_control_and_thinking() {
        let req = ChatRequest {
            model_id: "claude-x".into(),
            system: vec![
                crate::providers::SystemPart::cached("stable prefix"),
                crate::providers::SystemPart::volatile("git: on branch main"),
            ],
            messages: vec![Message::new(Role::User, "hi")],
            effort: Some(EffortLevel::High),
            effort_support: budget_support(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let b = build_body(&req, 8192, true);
        assert_eq!(b["model"], "claude-x");
        // stable system keeps its trailing marker; volatile rides the tail;
        // the last history message carries the third marker
        assert_eq!(b["system"].as_array().unwrap().len(), 1);
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["system"][0]["text"], "stable prefix");
        let msgs = b["messages"].as_array().unwrap();
        // the tail merges into the trailing user turn (alternation holds);
        // the marker stays on the history block, the tail rides unmarked
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(msgs[0]["content"][0]["text"], "hi");
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
        assert!(msgs[0]["content"][1].get("cache_control").is_none());
        assert!(
            msgs[0]["content"][1]["text"]
                .as_str()
                .unwrap()
                .contains("git: on branch main"),
            "volatile tail travels last: {}",
            msgs[0]["content"][1]["text"]
        );
        assert_eq!(b["thinking"]["budget_tokens"], 16384);
        assert_eq!(b["max_tokens"], 8192 + 16384);
    }

    /// The cache bug in one assertion: a changed volatile tail must not
    /// move a single byte before it. Tools, stable system, and the history
    /// message are identical; only the trailing tail differs.
    #[test]
    fn volatile_change_keeps_the_prefix_bytes() {
        let body_for = |volatile: &str| {
            build_body(
                &ChatRequest {
                    model_id: "m".into(),
                    system: vec![
                        crate::providers::SystemPart::cached("stable prefix"),
                        crate::providers::SystemPart::volatile(volatile),
                    ],
                    messages: vec![Message::new(Role::User, "hi")],
                    effort: None,
                    effort_support: Default::default(),
                    max_tokens: None,
                    tools: vec![],
                    previous_response_id: None,
                    context_transport: crate::providers::ContextTransport::Stateless,
                },
                8192,
                true,
            )
        };
        let before = body_for("git: on branch main");
        let after = body_for("git: on branch other");
        assert_eq!(before["system"], after["system"]);
        // history block identical, tail block moved (merged into the same
        // trailing user turn)
        assert_eq!(before["messages"][0]["content"][0], after["messages"][0]["content"][0]);
        assert_ne!(before["messages"][0]["content"][1], after["messages"][0]["content"][1]);
    }

    #[test]
    fn body_without_documented_cache_has_no_breakpoints() {
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![crate::providers::SystemPart::cached("stable prefix")],
            messages: vec![Message::new(Role::User, "hi")],
            effort: None,
            effort_support: Default::default(),
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
            effort: None,
            effort_support: Default::default(),
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
            effort: None,
            effort_support: Default::default(),
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
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                    "tu_1",
                    "ls",
                    json!({"path": "."}),
                )]),
                Message::tool_result("tu_1", "a.txt", false),
            ],
            effort: None,
            effort_support: Default::default(),
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
                    ToolCallReq::new("a", "t", json!({})),
                    ToolCallReq::new("b", "t", json!({})),
                ]),
                Message::tool_result("a", "res-a", false),
                Message::tool_result("b", "res-b", false),
            ],
            effort: None,
            effort_support: Default::default(),
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

    #[test]
    fn thinking_delta_pointer_extracts_thinking_field() {
        let v: Value = serde_json::from_str(
            r#"{"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "step by step"}}"#,
        )
        .unwrap();
        let thinking = v.pointer("/delta/thinking").and_then(|x| x.as_str());
        assert_eq!(thinking, Some("step by step"));
    }

    /// Serves one canned SSE body and closes. The Messages API uses named
    /// events, so the helper writes `event:` lines verbatim.
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
        use futures::StreamExt;
        let p = AnthropicProvider::new(&crate::config::ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Anthropic,
            base_url: url,
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        p.stream_chat(req)
            .filter_map(|e| async move { e.ok() })
            .collect()
            .await
    }

    /// Thinking text and its signature are captured from the stream and
    /// emitted as provider state at the end of the turn: without both, the
    /// next request with tool use is a 400.
    #[tokio::test]
    async fn streamed_thinking_and_signature_are_captured_as_provider_state() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"check\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG_1\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        )
        .to_string();
        let (url, h) = sse_server(body);
        let events = collect(url).await;
        h.join().unwrap();

        let thinking: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Reasoning(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "let me check", "thinking still streams: {events:?}");

        let state = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::ProviderState(s) => Some(s.clone()),
                _ => None,
            })
            .expect("provider state must be emitted");
        let blocks = state["thinking_blocks"].as_array().expect("blocks present");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "let me check");
        assert_eq!(blocks[0]["signature"], "SIG_1");
    }

    /// A redacted block has no text or signature to accumulate: the opaque
    /// payload goes back exactly as received.
    #[tokio::test]
    async fn streamed_redacted_thinking_is_kept_verbatim() {
        let body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"OPAQUE\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
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
        let blocks = state["thinking_blocks"].as_array().expect("blocks present");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "redacted_thinking");
        assert_eq!(blocks[0]["data"], "OPAQUE");
    }

    fn thinking_request() -> ChatRequest {
        ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![
                Message::new(Role::User, "run check"),
                Message::new(Role::Assistant, "checking now")
                    .with_provider_state(Some(json!({
                        "thinking_blocks": [
                            {"type": "thinking", "thinking": "let me check", "signature": "SIG_1"}
                        ]
                    })))
                    .with_tool_calls(vec![ToolCallReq::new("c1", "check", json!({}))]),
                Message::tool_result("c1", "all ok", false),
            ],
            effort: Some(EffortLevel::Medium),
            effort_support: budget_support(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "check".into(),
                description: "run checks".into(),
                parameters: json!({"type": "object"}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        }
    }

    /// Saved thinking replays first in its own assistant turn, ahead of
    /// text and tool_use: that order is what the API accepts.
    #[test]
    fn saved_thinking_replays_first_with_its_signature() {
        let b = build_body(&thinking_request(), 8192, true);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        let content = msgs[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "thinking, text, tool_use: {content:#?}");
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "let me check");
        assert_eq!(content[0]["signature"], "SIG_1");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool_use");
    }

    /// Thinking off means the saved blocks stay home: they are not legal
    /// input for a request that does not think.
    #[test]
    fn saved_thinking_stays_home_when_effort_is_off() {
        let mut req = thinking_request();
        req.effort = None;
        let b = build_body(&req, 8192, true);
        let msgs = b["messages"].as_array().unwrap();
        let content = msgs[1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "text, tool_use, no thinking: {content:#?}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_use");
        assert!(b.get("thinking").is_none());
    }
}
