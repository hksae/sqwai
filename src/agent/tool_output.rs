//! The wire is not the store.
//!
//! Tool results are the bulk of a long session — a measured run carried 776 KB
//! of them against 75 KB of conversation — and every one of them is re-sent
//! forever, because nothing says when an output has stopped being useful. This
//! module decides that, and decides it **about the request only**: the session
//! file keeps every byte, so a cleared result can be handed back on demand
//! (`recall_result`) without the model re-running the tool that produced it.
//!
//! The numbers are chosen against the provider's prefix cache, not against a
//! token budget. Rewriting an old message re-keys everything behind it, so a
//! pass that frees a little costs more than it saves: clearing starts late,
//! keeps a wide fresh window, and refuses to run for a small amount.

use crate::providers::{Message, Role};
use std::collections::HashMap;

/// Fallback trigger when `context_limit` is unset (`0`).
pub const TRIGGER_BYTES: usize = 128 * 1024;
/// Fallback fresh window when `context_limit` is unset (`0`).
pub const KEEP_BYTES: usize = 40 * 1024;
/// Fallback minimum clear step when `context_limit` is unset (`0`).
pub const MIN_CLEAR_BYTES: usize = 20 * 1024;

/// Maximum trigger ceiling (192k tokens ≈ 768 KB) so 1M-context models do not drag megabytes of stale tool output on every turn.
pub const WINDOW_TRIGGER_CAP_BYTES: usize = 768 * 1024;
/// Minimum fresh-window floor (56k tokens ≈ 224 KB) when scaling from a model's context window.
pub const WINDOW_KEEP_FLOOR_BYTES: usize = 224 * 1024;
/// Maximum fresh-window ceiling (96k tokens ≈ 384 KB).
pub const WINDOW_KEEP_CAP_BYTES: usize = 384 * 1024;
/// Minimum quantization step floor (16k tokens ≈ 64 KB) when scaling from a model's context window.
pub const WINDOW_STEP_FLOOR_BYTES: usize = 64 * 1024;
/// Maximum quantization step ceiling (32k tokens ≈ 128 KB).
pub const WINDOW_STEP_CAP_BYTES: usize = 128 * 1024;

/// Bytes per token until a provider says otherwise: the old constant, kept as
/// the cold-start value.
pub const DEFAULT_BYTES_PER_TOKEN: u64 = 4;
/// Lower clamp. Prose sits near 4 and code/JSON a little under it; below this a
/// request would have to be almost all short tokens.
pub const MIN_BYTES_PER_TOKEN: u64 = 3;
/// Upper clamp. Replayed ciphertext, HTML and escaped JSON measured 8.2 bytes
/// per token in a live session; a runaway value would let the pool grow past the
/// window the provider actually has.
pub const MAX_BYTES_PER_TOKEN: u64 = 12;
/// Smallest reported request whose ratio is trusted: a 10-token side call says
/// nothing about the shape of the agent's requests.
pub const RATIO_SAMPLE_TOKENS: u64 = 2_000;

/// The provider's own bytes-per-token for a request, clamped, or `None` when the
/// sample is too small to mean anything.
///
/// The context window is counted in tokens and the host only measures bytes, so
/// the conversion is a guess — and the guess was the whole size of two bugs: at
/// 4 bytes per token a 256k model got a 1 MB window while the provider billed its
/// 1,189,643-byte request as 144,424 tokens, so the real window was ~2 MB and the
/// pool started throwing away results at a third of the context.
pub fn observe_bytes_per_token(total_bytes: u64, prompt_tokens: u64) -> Option<u64> {
    if prompt_tokens < RATIO_SAMPLE_TOKENS || total_bytes == 0 {
        return None;
    }
    Some((total_bytes / prompt_tokens).clamp(MIN_BYTES_PER_TOKEN, MAX_BYTES_PER_TOKEN))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireBudget {
    pub trigger_bytes: usize,
    pub keep_bytes: usize,
    pub min_clear_bytes: usize,
}

impl WireBudget {
    /// Derive byte thresholds from the space the tool-output pool actually has.
    ///
    /// The request carries more than tool results, and most of it cannot be
    /// cleared: the conversation, tool call arguments, replayed provider
    /// reasoning, errored results. In a live run those immovable parts were
    /// 744 KB of a 1,024 KB window while the pool still claimed a 350 KB fresh
    /// window, so the two governors on the same window disagreed and
    /// compaction — the only one that saw the whole request — ate the
    /// conversation. Budget the pool against `window − fixed` instead.
    ///
    /// `bytes_per_token` converts the model's token window into bytes: the
    /// provider's own observed ratio when one is known
    /// ([`observe_bytes_per_token`]), [`DEFAULT_BYTES_PER_TOKEN`] on a cold
    /// start. Caps survive so a 1M-window model never drags megabytes of stale
    /// output; the floors survive as *minimums* only while the window can afford
    /// them, because keeping one recent result is never negotiable. When
    /// `context_limit == 0`, falls back to the baseline constants.
    pub fn for_request(context_limit: u64, fixed_bytes: usize, bytes_per_token: u64) -> Self {
        if context_limit == 0 {
            return Self {
                trigger_bytes: TRIGGER_BYTES,
                keep_bytes: KEEP_BYTES,
                min_clear_bytes: MIN_CLEAR_BYTES,
            };
        }
        let bpt = bytes_per_token.clamp(MIN_BYTES_PER_TOKEN, MAX_BYTES_PER_TOKEN);
        let window_bytes = (context_limit as usize).saturating_mul(bpt as usize);
        let free = window_bytes.saturating_sub(fixed_bytes);
        // The same 60/35/10 split the window used to get whole, now taken from
        // only what the pool may occupy. Floors apply while the free space can
        // afford them: a model whose request is already full must still keep
        // one recent result, never less than half of what is left.
        let keep_floor = WINDOW_KEEP_FLOOR_BYTES.min(free / 2);
        let step_floor = WINDOW_STEP_FLOOR_BYTES.min(keep_floor.max(1) / 2);
        Self {
            trigger_bytes: (free * 60 / 100).min(WINDOW_TRIGGER_CAP_BYTES).max(1),
            keep_bytes: (free * 35 / 100)
                .clamp(keep_floor, WINDOW_KEEP_CAP_BYTES.min(free.max(1)))
                .max(1),
            min_clear_bytes: (free * 10 / 100)
                .clamp(step_floor, WINDOW_STEP_CAP_BYTES)
                .max(1),
        }
    }

    /// Budget for a request whose only content is the transcript in `messages`.
    pub fn for_messages(context_limit: u64, messages: &[Message], bytes_per_token: u64) -> Self {
        Self::for_request(context_limit, nonclearable_bytes(messages), bytes_per_token)
    }
}

/// How many recent assistant turns keep their replayed reasoning on the wire.
///
/// One: the turn the model is answering right now. Older ciphertext repeats work
/// the transcript already shows in plain text — the model's own assistant text
/// and tool calls travel with the request anyway — and the encrypted items are
/// the largest single thing on it: in a live session 658 KB of a 1,189 KB
/// request was reasoning the host can neither read, summarize, nor compact.
pub const WIRE_REASONING_TURNS: usize = 1;

/// The provider-state keys that hold replayed chain-of-thought.
const REASONING_KEYS: [&str; 2] = ["reasoning_items", "thinking_blocks"];

fn reasoning_state(state: &serde_json::Value) -> bool {
    state
        .as_object()
        .is_some_and(|obj| REASONING_KEYS.iter().any(|key| obj.contains_key(*key)))
}

/// The assistant turns whose replayed reasoning still rides the wire: the most
/// recent [`WIRE_REASONING_TURNS`] that carry any.
fn carried_reasoning(messages: &[Message]) -> std::collections::HashSet<usize> {
    let mut keep = std::collections::HashSet::new();
    for (index, message) in messages.iter().enumerate().rev() {
        if message.role != Role::Assistant
            || !message
                .provider_state
                .as_ref()
                .is_some_and(reasoning_state)
        {
            continue;
        }
        if keep.len() >= WIRE_REASONING_TURNS {
            break;
        }
        keep.insert(index);
    }
    keep
}

/// What provider state costs the wire once [`prune_wire_reasoning`] has had it:
/// the reasoning keys are gone, everything else in the object still rides.
fn wire_state_bytes(state: &serde_json::Value) -> usize {
    let Some(map) = state.as_object() else {
        return crate::providers::json_byte_len(state) as usize;
    };
    if !REASONING_KEYS.iter().any(|key| map.contains_key(*key)) {
        return crate::providers::json_byte_len(state) as usize;
    }
    let mut rest = map.clone();
    for key in REASONING_KEYS {
        rest.remove(key);
    }
    if rest.is_empty() {
        return 0;
    }
    crate::providers::json_byte_len(&serde_json::Value::Object(rest)) as usize
}

/// Bytes of the request the pool cannot use: every message that `clearable`
/// refuses to clear, plus the conversation around it. Reasoning is counted as
/// [`prune_wire_reasoning`] will send it, so the budget and the request agree
/// about what rides. The instruction surface (system parts and tool schemas) is
/// deliberately absent — it is the same few tens of kilobytes on every request,
/// and the stubs clearing leaves behind cost about as much.
pub fn nonclearable_bytes(messages: &[Message]) -> usize {
    let carried = carried_reasoning(messages);
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let body = if clearable(message) {
                0
            } else {
                message.content.len() + media_wire_bytes(message)
            };
            let state = message
                .provider_state
                .as_ref()
                .map(|state| {
                    if carried.contains(&index) {
                        crate::providers::json_byte_len(state) as usize
                    } else {
                        wire_state_bytes(state)
                    }
                })
                .unwrap_or(0);
            let calls = message
                .tool_calls
                .iter()
                .map(|call| {
                    call.name.len()
                        + crate::providers::json_byte_len(&call.args) as usize
                        + call.extra_content
                            .as_ref()
                            .map(|extra| crate::providers::json_byte_len(extra) as usize)
                            .unwrap_or(0)
                })
                .sum::<usize>();
            body + state + calls
        })
        .sum()
}

/// Strip replayed chain-of-thought from every assistant turn but the most recent
/// [`WIRE_REASONING_TURNS`] that carry it.
///
/// Providers replay opaque reasoning per assistant turn: Responses keeps
/// `encrypted_content`, Anthropic keeps signed `thinking_blocks`. Continuation
/// needs the turn being answered, not the whole chain, so everything older is
/// dropped here — on the way to the wire, never in the session, which keeps the
/// transcript whole for a resumed turn. Gemini is untouched: its signatures ride
/// per tool call in `extra_content` and belong to the call they authorised.
pub fn prune_wire_reasoning(messages: &mut [Message]) {
    let mut carried = 0usize;
    for message in messages.iter_mut().rev() {
        if message.role != Role::Assistant {
            continue;
        }
        let Some(obj) = message
            .provider_state
            .as_mut()
            .and_then(|state| state.as_object_mut())
        else {
            continue;
        };
        if !REASONING_KEYS.iter().any(|key| obj.contains_key(*key)) {
            continue;
        }
        if carried < WIRE_REASONING_TURNS {
            carried += 1;
            continue;
        }
        for key in REASONING_KEYS {
            obj.remove(key);
        }
        if obj.is_empty() {
            message.provider_state = None;
        }
    }
}

/// What a cleared result is replaced with. The prefix is the marker
/// [`is_stub`] recognises, so a second pass never counts a stub as output.
pub const STUB_MARK: &str = "[output cleared]";

pub fn is_stub(content: &str) -> bool {
    content.starts_with(STUB_MARK)
}

/// The line the model sees instead of the bytes. It names the tool, what it
/// was pointed at, how much was dropped and the id that brings it back — a
/// bare `[cleared]` is how an agent ends up speaking confidently about content
/// it was never shown.
pub fn stub(name: &str, target: &str, bytes: usize, call_id: &str) -> String {
    let target = if target.is_empty() {
        String::new()
    } else {
        format!(" {target}")
    };
    format!(
        "{STUB_MARK} {name}{target} · {} KB of output removed from this request · recall_result(call_id=\"{call_id}\") returns it",
        bytes.div_ceil(1024)
    )
}

/// An errored result is small and its reason is the whole point of it: it is
/// never cleared, and never counted as clearable.
fn clearable(message: &Message) -> bool {
    message.role == Role::Tool && !message.is_error && !is_stub(&message.content)
}

/// Indices of the tool results to replace, oldest first.
#[cfg(test)]
pub fn cleared_indices(messages: &[Message]) -> Vec<usize> {
    cleared_indices_with_limit(messages, 0, DEFAULT_BYTES_PER_TOKEN)
}

/// Indices of the tool results to replace, scaled to the space `context_limit`
/// leaves after the request's immovable bytes.
pub fn cleared_indices_with_limit(
    messages: &[Message],
    context_limit: u64,
    bytes_per_token: u64,
) -> Vec<usize> {
    let budget = WireBudget::for_messages(context_limit, messages, bytes_per_token);
    let pool: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| clearable(m))
        .map(|(i, m)| (i, m.content.len() + media_wire_bytes(m)))
        .collect();
    let total: usize = pool.iter().map(|(_, bytes)| bytes).sum();
    if total <= budget.trigger_bytes {
        return Vec::new();
    }
    // walk back from the newest until the fresh window is full
    let mut kept = 0usize;
    let mut keep_from = pool.len();
    for k in (0..pool.len()).rev() {
        kept += pool[k].1;
        keep_from = k;
        if kept >= budget.keep_bytes {
            break;
        }
    }
    // Quantize the clear boundary in steps of `min_clear_bytes` from the front
    // of `pool`: because `messages` in the session store is never mutated by
    // `trim_for_wire`, advancing `keep_from` by one tool result on every turn
    // would rewrite a message at the boundary and re-key the cached history
    // behind it on every single tool call. Stepping only when at least
    // `min_clear_bytes` more can be freed keeps the boundary frozen between steps.
    let mut committed = 0usize;
    let mut step_bytes = 0usize;
    for (idx, &(_, bytes)) in pool[..keep_from].iter().enumerate() {
        step_bytes += bytes;
        if step_bytes >= budget.min_clear_bytes {
            committed = idx + 1;
            step_bytes = 0;
        }
    }
    if committed == 0 {
        return Vec::new();
    }
    pool[..committed].iter().map(|(i, _)| *i).collect()
}

/// Replace the old results with stubs, in place.
#[cfg(test)]
pub fn trim_for_wire(messages: &mut [Message]) {
    trim_for_wire_with_limit(messages, 0, DEFAULT_BYTES_PER_TOKEN);
}

/// Replace the old results with stubs, scaled to `context_limit` tokens at
/// `bytes_per_token`.
pub fn trim_for_wire_with_limit(
    messages: &mut [Message],
    context_limit: u64,
    bytes_per_token: u64,
) {
    let indices = cleared_indices_with_limit(messages, context_limit, bytes_per_token);
    if indices.is_empty() {
        return;
    }
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    for message in messages.iter() {
        for call in &message.tool_calls {
            calls.insert(
                call.id.clone(),
                (
                    call.name.clone(),
                    crate::agent::tools::call_summary(&call.name, &call.args),
                ),
            );
        }
    }
    for index in indices {
        let message = &messages[index];
        let call_id = message.tool_call_id.clone().unwrap_or_default();
        let (name, target) = calls
            .get(&call_id)
            .cloned()
            .unwrap_or_else(|| ("tool".to_string(), String::new()));
        let bytes = message.content.len() + media_wire_bytes(message);
        messages[index].content = stub(&name, &target, bytes, &call_id);
        // Pixels and documents are part of the cleared result: the stored
        // transcript keeps them for display and recall, the request clone
        // must not.
        messages[index].images.clear();
        messages[index].documents.clear();
    }
}

/// Approximate wire weight of a result's attached media, in the byte units
/// this module otherwise thinks in. Only the magnitude matters for the budget.
fn media_wire_bytes(message: &Message) -> usize {
    message
        .images
        .iter()
        .map(|image| crate::media::pressure_tokens(image.width, image.height) as usize * 4)
        .sum::<usize>()
        + message
            .documents
            .iter()
            .map(|document| crate::pdf::document_tokens(document.pages) as usize * 4)
            .sum::<usize>()
}

/// Whether this call's result would still be on the wire. The store holds
/// everything, so "cleared" is a property of the request: recompute the policy
/// over the stored transcript and anything it does not name is still visible —
/// and `recall_result` must not pretend otherwise, or it becomes a way to double the
/// context the model already has.
#[cfg(test)]
pub fn is_cleared(messages: &[Message], call_id: &str) -> bool {
    is_cleared_with_limit(messages, call_id, 0, DEFAULT_BYTES_PER_TOKEN)
}

pub fn is_cleared_with_limit(
    messages: &[Message],
    call_id: &str,
    context_limit: u64,
    bytes_per_token: u64,
) -> bool {
    cleared_indices_with_limit(messages, context_limit, bytes_per_token)
        .iter()
        .any(|i| {
            messages
                .get(*i)
                .and_then(|m| m.tool_call_id.as_deref())
                .is_some_and(|id| id == call_id)
        })
}

/// The `recall_result` tool, served from the transcript the loop is holding.
///
/// It runs in the agent loop rather than the dispatcher because the answer is
/// the live conversation, not the disk: the store is written once per iteration
/// and can lag a call behind, and a tool that reads files would have to be
/// told which session file is currently true.
#[cfg(test)]
pub fn recall(messages: &[Message], call_id: &str) -> crate::agent::tools::Outcome {
    recall_with_limit(messages, call_id, 0)
}

#[cfg(test)]
pub fn recall_with_limit(
    messages: &[Message],
    call_id: &str,
    context_limit: u64,
) -> crate::agent::tools::Outcome {
    recall_args_with_limit(
        messages,
        &serde_json::json!({ "call_id": call_id }),
        context_limit,
        DEFAULT_BYTES_PER_TOKEN,
    )
}

pub fn recall_args_with_limit(
    messages: &[Message],
    args: &serde_json::Value,
    context_limit: u64,
    bytes_per_token: u64,
) -> crate::agent::tools::Outcome {
    use crate::agent::tools::Outcome;
    let call_id = args["call_id"].as_str().unwrap_or_default().trim();
    if call_id.is_empty() {
        return Outcome::err("recall_result needs the call_id a cleared result names");
    }
    let stored = messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call_id));
    let Some(message) = stored else {
        return Outcome::err(format!(
            "no stored result for {call_id}: compaction dropped it from the transcript, \
             or it belongs to another session"
        ));
    };
    if !is_cleared_with_limit(messages, call_id, context_limit, bytes_per_token) {
        return Outcome::err(format!(
            "{call_id} is still in your context above — recall_result answers only for output \
             that was cleared from the request"
        ));
    }
    // A cleared media result is recalled with its pixels/documents: the stub
    // promises that recall returns the result, and the text record alone
    // would not.
    let with_media = |outcome: Outcome| {
        outcome
            .with_images(message.images.clone())
            .with_documents(message.documents.clone())
    };
    let query = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let offset = args["offset"].as_u64().map(|n| n.max(1) as usize);
    let limit = args["limit"].as_u64().map(|n| n.clamp(1, 2000) as usize);

    if query.is_none() && offset.is_none() && limit.is_none() {
        // Verbatim, wrapper and all: a recalled web page that arrived clean would
        // read as host-written truth.
        return with_media(Outcome::ok(message.content.clone()));
    }

    let all_lines: Vec<&str> = message.content.lines().collect();
    let total_lines = all_lines.len();
    if let Some(q) = query {
        let q_lower = q.to_lowercase();
        let mut keep = vec![false; total_lines];
        let mut matches = 0usize;
        for (i, line) in all_lines.iter().enumerate() {
            if line.to_lowercase().contains(&q_lower) {
                matches += 1;
                let from = i.saturating_sub(2);
                let to = (i + 2).min(total_lines.saturating_sub(1));
                for flag in &mut keep[from..=to] {
                    *flag = true;
                }
            }
        }
        if matches == 0 {
            return with_media(Outcome::ok(format!(
                "recall_result {call_id}: 0 matches for '{q}' across {total_lines} lines"
            )));
        }
        let max_lines = limit.unwrap_or(200);
        let mut out = format!(
            "recall_result {call_id}: {matches} match(es) for '{q}' in {total_lines} lines\n"
        );
        let mut shown = 0usize;
        let mut last_idx: Option<usize> = None;
        for (i, line) in all_lines.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            if shown >= max_lines {
                out.push_str(&format!("\n... [truncated after {max_lines} lines]"));
                break;
            }
            if let Some(prev) = last_idx
                && i > prev + 1
            {
                out.push_str("...\n");
            }
            out.push_str(&format!("{}: {}\n", i + 1, line));
            last_idx = Some(i);
            shown += 1;
        }
        return with_media(Outcome::ok(out.trim_end().to_string()));
    }

    let start_idx = offset.unwrap_or(1).saturating_sub(1).min(total_lines);
    let count = limit.unwrap_or(200);
    let end_idx = (start_idx + count).min(total_lines);
    let slice = &all_lines[start_idx..end_idx];
    let mut out = format!(
        "[recall_result {call_id}: lines {}..{} of {total_lines}]\n",
        if total_lines == 0 { 0 } else { start_idx + 1 },
        end_idx
    );
    out.push_str(&slice.join("\n"));
    with_media(Outcome::ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{Message, Role, ToolCallReq};
    use serde_json::json;

    fn exchange<S: AsRef<str>>(results: &[S]) -> Vec<Message> {
        let mut out = Vec::new();
        for (i, body) in results.iter().enumerate() {
            let body = body.as_ref();
            let id = format!("call_{i}");
            out.push(
                Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                    id.clone(),
                    "read",
                    json!({"file_path": format!("f{i}.rs")}),
                )]),
            );
            out.push(Message::tool_result(&id, body, false));
        }
        out
    }

    fn image_exchange() -> Vec<Message> {
        let image = crate::media::test_image(1280, 720, "shot.png");
        vec![
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "old",
                "view_image",
                json!({"path": "shot.png"}),
            )]),
            // Big enough to cross the quantized clear step on its own: the
            // budget never rewrites a result for a handful of bytes.
            Message::tool_result(
                "old",
                format!("view_image: shot.png\n{}", "x".repeat(30 * 1024)),
                false,
            )
            .with_images(vec![image]),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "mid",
                "read",
                json!({"file_path": "a.rs"}),
            )]),
            Message::tool_result("mid", "y".repeat(300 * 1024), false),
            Message::new(Role::User, "next"),
        ]
    }

    #[test]
    fn clearing_a_result_drops_its_pixels_from_the_wire() {
        let mut messages = image_exchange();
        trim_for_wire_with_limit(&mut messages, 0, DEFAULT_BYTES_PER_TOKEN);
        assert!(is_stub(&messages[1].content));
        assert!(
            messages[1].images.is_empty(),
            "cleared result must not carry pixels on the wire"
        );
    }

    #[test]
    fn recall_reattaches_pixels_of_a_cleared_image_result() {
        let messages = image_exchange();
        let out = recall_with_limit(&messages, "old", 0);
        assert!(out.ok, "{}", out.output);
        assert_eq!(out.images.len(), 1, "recall must return the pixels it names");
    }

    fn document_exchange() -> Vec<Message> {
        let document = crate::media::test_document(3, "report.pdf");
        vec![
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "old",
                "view_document",
                json!({"path": "report.pdf"}),
            )]),
            Message::tool_result(
                "old",
                format!("view_document: report.pdf\n{}", "x".repeat(30 * 1024)),
                false,
            )
            .with_documents(vec![document]),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "mid",
                "read",
                json!({"file_path": "a.rs"}),
            )]),
            Message::tool_result("mid", "y".repeat(300 * 1024), false),
            Message::new(Role::User, "next"),
        ]
    }

    #[test]
    fn clearing_a_result_drops_its_documents_from_the_wire() {
        let mut messages = document_exchange();
        trim_for_wire_with_limit(&mut messages, 0, DEFAULT_BYTES_PER_TOKEN);
        assert!(is_stub(&messages[1].content));
        assert!(
            messages[1].documents.is_empty(),
            "cleared result must not carry documents on the wire"
        );
    }

    #[test]
    fn recall_reattaches_documents_of_a_cleared_document_result() {
        let messages = document_exchange();
        let out = recall_with_limit(&messages, "old", 0);
        assert!(out.ok, "{}", out.output);
        assert_eq!(
            out.documents.len(),
            1,
            "recall must return the document it names"
        );
    }

    fn contents(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|m| m.content.clone()).collect()
    }

    /// Below the trigger nothing moves: the wire is the model's memory, and an
    /// early trim buys bytes nobody is short of while costing the prefix cache.
    #[test]
    fn small_sessions_are_left_alone() {
        let body = "x".repeat(4 * 1024);
        let mut messages = exchange(&[&body, &body, &body]);
        let before = contents(&messages);
        trim_for_wire(&mut messages);
        assert_eq!(before, contents(&messages), "12 KB must not be trimmed");
    }

    #[test]
    fn the_fresh_window_survives_and_the_old_output_becomes_a_pointer() {
        // 40 results of 8 KB: 320 KB clearable, the newest ~40 KB stay
        let body = "y".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let mut messages = exchange(&many);
        trim_for_wire(&mut messages);
        let stubs: Vec<&Message> = messages.iter().filter(|m| is_stub(&m.content)).collect();
        assert!(
            stubs.len() >= 30 && stubs.len() <= 36,
            "cleared {} of 40",
            stubs.len()
        );
        let last = messages.last().unwrap();
        assert!(!is_stub(&last.content), "the newest result must stay whole");
        let first_stub = stubs.first().unwrap();
        let text = &first_stub.content;
        assert!(text.contains("read f0.rs"), "{text}");
        assert!(text.contains("8 KB"), "{text}");
        assert!(text.contains("call_id=\"call_0\""), "{text}");
    }

    /// A second pass must not re-count the stubs it wrote, or the policy would
    /// keep trimming its own bookkeeping.
    #[test]
    fn trimming_is_idempotent() {
        let body = "z".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let mut messages = exchange(&many);
        trim_for_wire(&mut messages);
        let once = contents(&messages);
        trim_for_wire(&mut messages);
        assert_eq!(once, contents(&messages));
    }

    #[test]
    fn clear_boundary_is_quantized_to_preserve_prefix_cache() {
        let body = "q".repeat(8 * 1024);
        let mut many: Vec<String> = (0..38).map(|_| body.clone()).collect();
        let base = cleared_indices(&exchange(&many));
        assert!(!base.is_empty());
        // Adding one more 8 KB result (< MIN_CLEAR_BYTES = 20 KB) must not
        // advance the clear boundary and re-key the last 40 KB of history.
        many.push(body);
        let next = cleared_indices(&exchange(&many));
        assert_eq!(
            base, next,
            "clear boundary must stay frozen until a full MIN_CLEAR_BYTES step accumulates"
        );
    }

    /// An error is not filler: the reason it failed is the content.
    #[test]
    fn failed_results_are_never_cleared() {
        let body = "e".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let mut messages = exchange(&many);
        for message in messages.iter_mut() {
            if message.role == Role::Tool {
                message.is_error = true;
            }
        }
        trim_for_wire(&mut messages);
        assert!(
            messages.iter().all(|m| !is_stub(&m.content)),
            "every result errored, so nothing is clearable"
        );
    }

    #[test]
    fn recall_answers_only_for_what_left_the_wire() {
        let body = "r".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let messages = exchange(&many);
        assert!(
            is_cleared(&messages, "call_0"),
            "the oldest result is clearable at this size"
        );
        assert!(
            !is_cleared(&messages, "call_39"),
            "the newest result is still in front of the model"
        );

        // the tool itself: whole bytes back for what left, a refusal for what
        // did not, and never a silent miss
        let got = recall(&messages, "call_0");
        assert!(got.ok, "{}", got.output);
        assert_eq!(got.output, body, "recall returns the stored bytes verbatim");
        let visible = recall(&messages, "call_39");
        assert!(!visible.ok, "{}", visible.output);
        assert!(
            visible.output.contains("still in your context"),
            "{}",
            visible.output
        );
        let unknown = recall(&messages, "call_9999");
        assert!(!unknown.ok && unknown.output.contains("no stored result"));
        let blank = recall(&messages, "   ");
        assert!(!blank.ok && blank.output.contains("call_id"));
    }

    #[test]
    fn wire_budget_scales_and_clamps_to_ceiling() {
        let b1m = WireBudget::for_request(1_000_000, 0, DEFAULT_BYTES_PER_TOKEN);
        // 1M tokens clamps to ceiling (768 KB / 384 KB / 128 KB)
        assert_eq!(b1m.trigger_bytes, WINDOW_TRIGGER_CAP_BYTES);
        assert_eq!(b1m.keep_bytes, WINDOW_KEEP_CAP_BYTES);
        assert_eq!(b1m.min_clear_bytes, WINDOW_STEP_CAP_BYTES);

        let b200k = WireBudget::for_request(200_000, 0, DEFAULT_BYTES_PER_TOKEN);
        // 200k tokens * 4 bytes = 800,000 bytes: 60% = 480 KB, 35% = 280 KB, 10% = 80 KB
        assert_eq!(b200k.trigger_bytes, 480_000);
        assert_eq!(b200k.keep_bytes, 280_000);
        assert_eq!(b200k.min_clear_bytes, 80_000);

        // 320 KB of tool outputs (40 * 8 KB) is untouched on a 1M or 200k window
        let body = "w".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let messages = exchange(&many);
        assert!(cleared_indices_with_limit(&messages, 1_000_000, DEFAULT_BYTES_PER_TOKEN).is_empty());
        assert!(cleared_indices_with_limit(&messages, 200_000, DEFAULT_BYTES_PER_TOKEN).is_empty());

        // recall_args_with_limit supports query filtering
        let mut searchable = many;
        searchable[0] = "line 1\nneedle_token_xyz\nline 3\n".to_string() + &body;
        let messages = exchange(&searchable);
        let filtered = recall_args_with_limit(
            &messages,
            &json!({"call_id": "call_0", "query": "needle_token_xyz"}),
            0,
            DEFAULT_BYTES_PER_TOKEN,
        );
        assert!(filtered.ok, "{}", filtered.output);
        assert!(filtered.output.contains("2: needle_token_xyz"));
    }

    /// The live session that forced this change: a 256k window whose request was
    /// 744 KB of conversation and replayed reasoning, while the tool-output pool
    /// still claimed the fresh window it would have had with nothing else there.
    #[test]
    fn wire_budget_shrinks_with_the_bytes_it_cannot_clear() {
        assert_eq!(
            (
                WireBudget::for_request(256_000, 0, DEFAULT_BYTES_PER_TOKEN).trigger_bytes,
                WireBudget::for_request(256_000, 0, DEFAULT_BYTES_PER_TOKEN).keep_bytes,
                WireBudget::for_request(256_000, 0, DEFAULT_BYTES_PER_TOKEN).min_clear_bytes,
            ),
            (614_400, 358_400, 102_400),
            "an empty request keeps the old window split"
        );
        let full = WireBudget::for_request(256_000, 744_000, DEFAULT_BYTES_PER_TOKEN);
        assert_eq!(
            (
                full.trigger_bytes,
                full.keep_bytes,
                full.min_clear_bytes
            ),
            (168_000, 140_000, 65_536),
            "a nearly full request leaves the pool a quarter of the window"
        );

        let body = "w".repeat(8 * 1024);
        let many: Vec<String> = (0..25).map(|_| body.clone()).collect();
        let messages = exchange(&many);
        assert!(
            cleared_indices_with_limit(&messages, 256_000, DEFAULT_BYTES_PER_TOKEN).is_empty(),
            "200 KB of output is comfortably inside an unencumbered request"
        );

        // The same pool against a request whose window is already mostly spent
        // on conversation the host cannot clear.
        let mut crowded = vec![Message::new(Role::User, "c".repeat(760_000))];
        crowded.extend(messages);
        let cleared = cleared_indices_with_limit(&crowded, 256_000, DEFAULT_BYTES_PER_TOKEN);
        assert!(
            !cleared.is_empty(),
            "760 KB of history must cost the oldest results their place"
        );
        assert!(
            cleared
                .iter()
                .all(|index| crowded[*index].role == Role::Tool),
            "only results the pool may clear are named: {cleared:?}"
        );
        assert!(
            !cleared.contains(&(crowded.len() - 1)),
            "the newest result always keeps its place: {cleared:?}"
        );
    }

    /// The window is counted in tokens and the host only measures bytes, so the
    /// conversion decides when results get thrown away. At the flat 4 a live 256k
    /// model was budgeted as a 1 MB request while its provider billed
    /// 1,189,643 bytes as 144,424 tokens — a ~2 MB window. Same request, and the
    /// pool was being cleared at a third of the space it actually had.
    #[test]
    fn the_byte_window_follows_the_providers_ratio() {
        assert_eq!(observe_bytes_per_token(1_189_643, 144_424), Some(8));
        assert_eq!(
            observe_bytes_per_token(23_472, 10),
            None,
            "a 10-token side call says nothing about the shape of an agent request"
        );
        assert_eq!(
            observe_bytes_per_token(1_189_643, 2_000),
            Some(MAX_BYTES_PER_TOKEN),
            "a runaway ratio is clamped, not believed"
        );

        let assumed = WireBudget::for_request(256_000, 744_000, DEFAULT_BYTES_PER_TOKEN);
        let observed = WireBudget::for_request(256_000, 744_000, 8);
        assert_eq!(
            (
                assumed.trigger_bytes,
                assumed.keep_bytes,
                assumed.min_clear_bytes
            ),
            (168_000, 140_000, 65_536)
        );
        assert_eq!(
            (
                observed.trigger_bytes,
                observed.keep_bytes,
                observed.min_clear_bytes
            ),
            (782_400, 393_216, 130_400),
            "at the real ratio the same request is nowhere near full"
        );
    }

    /// Replayed chain-of-thought is priced exactly as it is sent: one turn's
    /// worth. Live session before the rule: 658 KB of reasoning in a 1,189 KB
    /// request, untrimmable and unreadable by the host.
    #[test]
    fn only_the_newest_turns_reasoning_rides_the_wire() {
        let blob = "A".repeat(30_000);
        let body = "w".repeat(100);
        let many: Vec<String> = (0..4).map(|_| body.clone()).collect();
        let mut messages = exchange(&many);
        for message in &mut messages {
            if message.role == Role::Assistant {
                message.provider_state = Some(json!({
                    "reasoning_items": [{"type": "reasoning", "encrypted_content": blob}],
                    "phase": "commentary"
                }));
            }
        }
        let mut wire = messages.clone();
        prune_wire_reasoning(&mut wire);

        let carried: Vec<&Message> = wire
            .iter()
            .filter(|m| {
                m.provider_state
                    .as_ref()
                    .is_some_and(|s| s.get("reasoning_items").is_some())
            })
            .collect();
        assert_eq!(
            carried.len(),
            WIRE_REASONING_TURNS,
            "one turn's ciphertext survives, the rest is dropped"
        );
        let survivor = carried[0]
            .provider_state
            .as_ref()
            .expect("carried turn");
        assert_eq!(
            survivor.get("phase").and_then(|v| v.as_str()),
            Some("commentary"),
            "only the reasoning keys are stripped, other provider state rides on"
        );
        assert_eq!(
            nonclearable_bytes(&messages),
            nonclearable_bytes(&wire),
            "the budget prices the request as the wire will carry it"
        );
        let one_turn = blob.len() + 200;
        let four_turns = blob.len() * 4;
        assert!(
            nonclearable_bytes(&messages) < four_turns,
            "all four turns were counted: {}",
            nonclearable_bytes(&messages)
        );
        assert!(
            nonclearable_bytes(&messages) > one_turn,
            "the carried turn's ciphertext was not counted at all"
        );
    }
}
