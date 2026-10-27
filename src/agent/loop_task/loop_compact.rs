use crate::agent::context;
use crate::providers::{
    ChatRequest, ContextTransport, Message, Role, SharedProvider, StreamEvent, SystemPart,
};

/// Run the compaction policy, cheapest stage first.
///
/// 1. prune aged-out tool output (no LLM);
/// 2. summarize the oldest turns through the model;
/// 3. if the summary could not be obtained, summarize locally;
/// 4. hard-trim if the history still does not fit.
///
/// Returns `(before, after, summarized_by_the_model)` when something changed.
/// A failure never propagates: stage 4 always leaves a usable transcript.
///
/// All stages measure the HISTORY estimate, never the provider's full
/// request size: the system prefix and tool schemas are fixed costs no
/// compaction can remove, and gating on them fires futile compactions
/// every turn (history already fits, trim no-ops, `after == before`).
/// Real overflows still force-compact via the overflow path.
/// Preformatted durable-plan block for the restricted summary:
/// the summarizer can only exclude what it can see. Empty when no plan is
/// active — then every user ask counts as uncovered.
pub(crate) fn plan_hint_for_summary(root: &std::path::Path, session_id: &str) -> String {
    let Some(plan) = crate::plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten()
    else {
        return String::new();
    };
    let mut hint = format!("Goal: {}", plan.goal.text);
    if !plan.constraints.is_empty() {
        hint.push_str("\nConstraints:\n- ");
        hint.push_str(&plan.constraints.join("\n- "));
    }
    hint
}

/// EventLog-first compaction record: tokens and messages
/// before/after, whether a summary was written, and its text
/// (secret-screened by `append`). The `/compact` path already wrote
/// begin/end records; the automatic loop cuts wrote nothing — so G0's
/// post-hoc lore question ("what survived the cut?") was unanswerable.
/// It is now; bytes-freed is `before - after`.
pub(crate) fn record_compaction(
    events: &mut Option<crate::agent::events::EventLog>,
    before: u64,
    after: u64,
    msgs_before: usize,
    msgs_after: usize,
    summarized: bool,
    summary_text: &str,
) {
    let Some(writer) = events.as_mut() else {
        return;
    };
    let _ = writer.append(
        "compaction",
        serde_json::json!({
            "before": before,
            "after": after,
            "msgs_before": msgs_before,
            "msgs_after": msgs_after,
            "summarized": summarized,
            "summary": summary_text,
        }),
    );
}

/// Parent prefix handed to compaction so the summary request can reuse it:
/// system parts and tool schemas go on the wire byte-identical, and the old
/// history reads at cache-read price instead of full price.
pub struct CompactionPrefix<'a> {
    pub system: &'a [crate::providers::SystemPart],
    pub tools: &'a [crate::providers::ToolSpec],
}

/// Saved thinking blocks replay only while thinking is on for the request.
/// The summary request thinks nothing (effort off, tiny budget), so a
/// history that carries thinking blocks cannot travel as structured
/// messages — the API would refuse the tool_use blocks without their
/// thinking. Such histories compact through the standalone request, whose
/// transcript is text.
fn history_has_thinking(messages: &[Message]) -> bool {
    messages.iter().any(|m| {
        m.provider_state
            .as_ref()
            .and_then(|s| s.get("thinking_blocks"))
            .and_then(|v| v.as_array())
            .is_some_and(|blocks| !blocks.is_empty())
    })
}

/// Build the summary request. Cache-aware when the parent prefix is handed
/// over and the history carries no thinking: parent system, schemas and the
/// wire history go on the wire unchanged, the summarization prompt is
/// appended as the last user message. Otherwise the compact standalone
/// request: tiny system, transcript rendered as text, no schemas.
///
/// `keep_len` is how many trailing messages stay verbatim after compaction:
/// the prompt names them so the model summarizes the earlier part and does
/// not restate the kept tail.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compaction_request(
    prefix: Option<&CompactionPrefix<'_>>,
    older: &[Message],
    history: &[Message],
    keep_len: usize,
    previous: Option<&str>,
    plan_hint: &str,
    focus: Option<&str>,
    model_id: &str,
    retry: bool,
) -> (ChatRequest, bool) {
    // a summarization request needs no tools of its own — but the
    // cache-aware variant carries the parent's schemas to keep the prefix
    // byte-identical (the prompt answers in text regardless; see rules)
    let standalone = || {
        (
            ChatRequest {
                model_id: model_id.to_string(),
                system: vec![SystemPart::volatile(context::SUMMARY_SYSTEM)],
                messages: vec![Message::new(
                    Role::User,
                    context::summary_short_input(older, previous, plan_hint, focus, retry),
                )],
                effort: None,
                effort_support: Default::default(),
                max_tokens: Some(context::SUMMARY_SHORT_MAX_TOKENS),
                tools: Vec::new(),
                previous_response_id: None,
                context_transport: ContextTransport::Stateless,
            },
            false,
        )
    };
    let Some(prefix) = prefix else {
        return standalone();
    };
    if history_has_thinking(history) {
        return standalone();
    }
    let mut messages = history.to_vec();
    messages.push(Message::new(
        Role::User,
        context::summary_short_prompt(previous, plan_hint, focus, retry, keep_len),
    ));
    (
        ChatRequest {
            model_id: model_id.to_string(),
            system: prefix.system.to_vec(),
            messages,
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(context::SUMMARY_SHORT_MAX_TOKENS),
            tools: prefix.tools.to_vec(),
            previous_response_id: None,
            context_transport: ContextTransport::Stateless,
        },
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact_history(
    provider: &SharedProvider,
    model_id: &str,
    messages: &mut Vec<Message>,
    summary: &mut Option<String>,
    policy: &context::Policy,
    force: bool,
    plan_hint: &str,
    focus: Option<&str>,
    prefix: Option<&CompactionPrefix<'_>>,
) -> Option<(u64, u64, bool)> {
    let measured = |m: &[Message]| context::estimated_tokens(m);
    // Pressure is judged on the WIRE size, not the raw store: aged-out tool
    // results are already stubs on the request, so counting stored bytes keeps
    // firing compaction long after the request stopped growing.
    let wire_measured = |m: &[Message]| {
        context::wire_estimated_tokens(m, policy.context_limit, policy.bytes_per_token)
    };
    let before = measured(messages);

    // No per-turn pruning by design (Codex shape): what the model read
    // stays verbatim until real pressure. Silently rewriting old tool
    // output taught the model to re-read the same files in circles —
    // the prune note even instructed it to. History shrinks only below,
    // on pressure, via summary or hard trim.
    if !force && policy.pressure(wire_measured(messages)) == context::Pressure::Ok {
        // Pressure is fine: history is untouched, report nothing.
        return None;
    }

    // stage 2: summarize. The cut is safe by construction, so an assistant
    // tool call can never be separated from its results. `force` (manual
    // `/compact`) also shrinks the kept tail when the configured keep_turns
    // would leave nothing to summarize.
    let (older, keep) = context::split_for_summary_adaptive(messages, policy.keep_turns(), force);
    let older: Vec<Message> = older.to_vec();
    let keep: Vec<Message> = keep.to_vec();
    let keep_len = keep.len();
    let mut summarized = false;
    if !older.is_empty() {
        let mut wire_history = messages.clone();
        crate::agent::tool_output::trim_for_wire_with_limit(
            &mut wire_history,
            policy.context_limit,
            policy.bytes_per_token,
        );
        let wire_older = &wire_history[..older.len().min(wire_history.len())];
        let (request, cache_aware) = compaction_request(
            prefix,
            wire_older,
            &wire_history,
            keep_len,
            summary.as_deref(),
            plan_hint,
            focus,
            model_id,
            false,
        );
        if cache_aware {
            crate::providers::log_http("compaction: summary reuses the parent prefix");
        }
        let text = match collect_text(provider, &request).await {
            Ok(text) => text,
            Err(e) => {
                crate::providers::log_http(&format!("compaction: summarization failed: {e}"));
                // stage 3: extract a summary locally instead of losing the turns
                context::local_summary(wire_older, summary.as_deref())
            }
        };
        // the wire cap is advisory: a severely over-budget answer gets one
        // retry with an explicit hard limit before the host truncates.
        // The retry keeps the request shape (cache-aware or standalone) and
        // only rewords the prompt.
        let text = if text.chars().count() > 2 * context::SUMMARY_SHORT_MAX_CHARS {
            let (retry_request, _) = compaction_request(
                prefix,
                wire_older,
                &wire_history,
                keep_len,
                summary.as_deref(),
                plan_hint,
                focus,
                model_id,
                true,
            );
            match collect_text(provider, &retry_request).await {
                Ok(shorter) => shorter,
                Err(_) => text,
            }
        } else {
            text
        };
        // host-enforced cap (the wire cap is advisory at best): without it
        // chained summaries balloon every cycle (measured 33K-140K chars)
        // and late compactions free nothing at full call price.
        let text = context::truncate_summary(&text);
        *messages = context::apply_summary(&text, &keep);
        *summary = Some(text);
        summarized = true;
    }

    // stage 4: still too big — drop the oldest turns outright
    if policy.pressure(wire_measured(messages)) != context::Pressure::Ok {
        let budget = policy.budget();
        *messages = context::hard_trim(messages, budget);
    }
    let after = measured(messages);
    if after == before && !summarized {
        return None;
    }
    Some((before, after, summarized))
}

/// Stream one request and collect its text; every other event is discarded.
async fn collect_text(provider: &SharedProvider, req: &ChatRequest) -> Result<String, String> {
    use futures::StreamExt;
    let mut stream = provider.stream_chat(req.clone());
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(StreamEvent::Text(t)) => out.push_str(&t),
            Ok(_) => {}
            Err(e) => return Err(format!("{e:#}")),
        }
    }
    let text = out.trim().to_string();
    if text.is_empty() {
        Err("the model returned an empty summary".into())
    } else {
        Ok(text)
    }
}

/// Why a turn could not be completed, and what class of failure it was.
///
/// The class is what lets the caller tell "compact and try again" from "stop
/// and tell the user", instead of both arriving as the same string.
pub struct TurnFailure {
    pub message: String,
    pub class: Option<crate::providers::ErrorClass>,
    /// how many times the request was retried before giving up
    pub retries: u32,
    /// the provider refused the continuation reference; the caller can retry
    /// once with the transcript it owns
    pub continuation_rejected: bool,
    /// text for this turn was already streamed to the user before the
    /// failure. Retrying or falling back would show a second answer on top
    /// of the partial one (audit H8) — the caller must stop instead.
    pub partial: bool,
}

impl TurnFailure {
    pub(crate) fn new(
        message: impl Into<String>,
        class: Option<crate::providers::ErrorClass>,
        retries: u32,
    ) -> Self {
        Self {
            message: message.into(),
            class,
            retries,
            continuation_rejected: false,
            partial: false,
        }
    }

    pub(crate) fn with_partial(mut self, yes: bool) -> Self {
        self.partial = yes;
        self
    }

    pub(crate) fn continuation_rejected(message: impl Into<String>, retries: u32) -> Self {
        Self {
            continuation_rejected: true,
            ..Self::new(message, None, retries)
        }
    }
}

/// stream one request, retrying failures that waiting can fix, with backoff,
/// until it succeeds or the retry window elapses
/// A provider rejecting the continuation reference, in the shapes seen so far.
/// The first is what a relay says when it accepted `previous_response_id` and
/// kept nothing behind it: the tool outputs arrive with no calls to match.
pub(crate) fn rejects_continuation(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no tool call found for function call output")
        || (lower.contains("previous_response") && !lower.contains("not supported"))
        || lower.contains("previous response not found")
}

