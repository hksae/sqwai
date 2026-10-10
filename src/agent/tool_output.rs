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

/// Clearing begins once the clearable tool output on the wire passes this.
pub const TRIGGER_BYTES: usize = 128 * 1024;
/// The newest this much output is always left intact: it is what the model is
/// still reasoning over.
pub const KEEP_BYTES: usize = 40 * 1024;
/// A pass that would free less than this is not worth re-keying the cache.
pub const MIN_CLEAR_BYTES: usize = 20 * 1024;

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
pub fn cleared_indices(messages: &[Message]) -> Vec<usize> {
    let pool: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| clearable(m))
        .map(|(i, m)| (i, m.content.len()))
        .collect();
    let total: usize = pool.iter().map(|(_, bytes)| bytes).sum();
    if total <= TRIGGER_BYTES {
        return Vec::new();
    }
    // walk back from the newest until the fresh window is full
    let mut kept = 0usize;
    let mut keep_from = pool.len();
    for k in (0..pool.len()).rev() {
        kept += pool[k].1;
        keep_from = k;
        if kept >= KEEP_BYTES {
            break;
        }
    }
    let freed: usize = pool[..keep_from].iter().map(|(_, bytes)| bytes).sum();
    if freed < MIN_CLEAR_BYTES {
        return Vec::new();
    }
    pool[..keep_from].iter().map(|(i, _)| *i).collect()
}

/// Replace the old results with stubs, in place.
pub fn trim_for_wire(messages: &mut [Message]) {
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
    for index in cleared_indices(messages) {
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
pub fn is_cleared(messages: &[Message], call_id: &str) -> bool {
    cleared_indices(messages).iter().any(|i| {
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
pub fn recall(messages: &[Message], call_id: &str) -> crate::agent::tools::Outcome {
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
    if !is_cleared(messages, call_id) {
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
}
