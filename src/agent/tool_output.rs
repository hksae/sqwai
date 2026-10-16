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

/// Minimum trigger floor (96k tokens ≈ 384 KB) when scaling from a model's context window.
pub const WINDOW_TRIGGER_FLOOR_BYTES: usize = 384 * 1024;
/// Minimum fresh-window floor (56k tokens ≈ 224 KB) when scaling from a model's context window.
pub const WINDOW_KEEP_FLOOR_BYTES: usize = 224 * 1024;
/// Minimum quantization step floor (16k tokens ≈ 64 KB) when scaling from a model's context window.
pub const WINDOW_STEP_FLOOR_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireBudget {
    pub trigger_bytes: usize,
    pub keep_bytes: usize,
    pub min_clear_bytes: usize,
}

impl WireBudget {
    /// Derive byte thresholds from the model's context window (at ~4 bytes/token):
    /// - trigger: 60% of window (floor: 384 KB / 96k tok)
    /// - keep:    35% of window (floor: 224 KB / 56k tok)
    /// - step:    10% of window (floor: 64 KB / 16k tok)
    /// When `context_limit == 0`, falls back to the baseline constants.
    pub fn for_context_limit(context_limit: u64) -> Self {
        if context_limit == 0 {
            return Self {
                trigger_bytes: TRIGGER_BYTES,
                keep_bytes: KEEP_BYTES,
                min_clear_bytes: MIN_CLEAR_BYTES,
            };
        }
        let window_bytes = (context_limit as usize).saturating_mul(4);
        Self {
            trigger_bytes: (window_bytes * 60 / 100).max(WINDOW_TRIGGER_FLOOR_BYTES),
            keep_bytes: (window_bytes * 35 / 100).max(WINDOW_KEEP_FLOOR_BYTES),
            min_clear_bytes: (window_bytes * 10 / 100).max(WINDOW_STEP_FLOOR_BYTES),
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
    cleared_indices_with_limit(messages, 0)
}

/// Indices of the tool results to replace, scaled to `context_limit` tokens.
pub fn cleared_indices_with_limit(messages: &[Message], context_limit: u64) -> Vec<usize> {
    let budget = WireBudget::for_context_limit(context_limit);
    let pool: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| clearable(m))
        .map(|(i, m)| (i, m.content.len()))
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
    trim_for_wire_with_limit(messages, 0);
}

/// Replace the old results with stubs, scaled to `context_limit` tokens.
pub fn trim_for_wire_with_limit(messages: &mut [Message], context_limit: u64) {
    let indices = cleared_indices_with_limit(messages, context_limit);
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
        let bytes = message.content.len();
        messages[index].content = stub(&name, &target, bytes, &call_id);
    }
}

/// Whether this call's result would still be on the wire. The store holds
/// everything, so "cleared" is a property of the request: recompute the policy
/// over the stored transcript and anything it does not name is still visible —
/// and `recall_result` must not pretend otherwise, or it becomes a way to double the
/// context the model already has.
#[cfg(test)]
pub fn is_cleared(messages: &[Message], call_id: &str) -> bool {
    is_cleared_with_limit(messages, call_id, 0)
}

pub fn is_cleared_with_limit(messages: &[Message], call_id: &str, context_limit: u64) -> bool {
    cleared_indices_with_limit(messages, context_limit)
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

pub fn recall_with_limit(
    messages: &[Message],
    call_id: &str,
    context_limit: u64,
) -> crate::agent::tools::Outcome {
    use crate::agent::tools::Outcome;
    let call_id = call_id.trim();
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
    if !is_cleared_with_limit(messages, call_id, context_limit) {
        return Outcome::err(format!(
            "{call_id} is still in your context above — recall_result answers only for output \
             that was cleared from the request"
        ));
    }
    // Verbatim, wrapper and all: a recalled web page that arrived clean would
    // read as host-written truth.
    Outcome::ok(message.content.clone())
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
    fn wire_budget_scales_to_60_35_10_percent_of_context_window() {
        let b1m = WireBudget::for_context_limit(1_000_000);
        // 1M tokens * 4 bytes = 4,000,000 bytes: 60% = 2.4 MB, 35% = 1.4 MB, 10% = 400 KB
        assert_eq!(b1m.trigger_bytes, 2_400_000);
        assert_eq!(b1m.keep_bytes, 1_400_000);
        assert_eq!(b1m.min_clear_bytes, 400_000);

        let b200k = WireBudget::for_context_limit(200_000);
        // 200k tokens * 4 bytes = 800,000 bytes: 60% = 480 KB, 35% = 280 KB, 10% = 80 KB
        assert_eq!(b200k.trigger_bytes, 480_000);
        assert_eq!(b200k.keep_bytes, 280_000);
        assert_eq!(b200k.min_clear_bytes, 80_000);

        // 320 KB of tool outputs (40 * 8 KB) is untouched on a 1M or 200k window
        let body = "w".repeat(8 * 1024);
        let many: Vec<String> = (0..40).map(|_| body.clone()).collect();
        let messages = exchange(&many);
        assert!(cleared_indices_with_limit(&messages, 1_000_000).is_empty());
        assert!(cleared_indices_with_limit(&messages, 200_000).is_empty());
    }
}
