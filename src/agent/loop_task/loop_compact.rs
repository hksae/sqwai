use crate::agent::context;
use crate::providers::{
    ChatRequest, ContextTransport, Message, Role, SharedProvider, StreamEvent, SystemPart,
};

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

/// Why a compaction ran. Recorded on the event: a cut that looks premature is
/// only explainable by what fired it, and the three callers differ in exactly
/// that — `/compact` is the user asking, `Overflow` is the provider refusing,
/// `Pressure` is the gate below deciding the next request would not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionReason {
    Pressure,
    Manual,
    /// The provider refused the request for size. The tail allowance shrinks
    /// with each attempt: a cut that did not make the request fit needs a
    /// smaller tail, not the same cut repeated — one shot per oversized
    /// request was a dead turn whenever the first compaction came up short.
    Overflow {
        tail_budget: u64,
    },
}

impl CompactionReason {
    fn label(self) -> &'static str {
        match self {
            Self::Pressure => "pressure",
            Self::Manual => "manual",
            Self::Overflow { .. } => "overflow",
        }
    }

    /// Manual and overflow cuts shrink the kept tail when `keep_turns` would
    /// leave nothing to summarize; the pressure gate never does.
    fn force(self) -> bool {
        self != Self::Pressure
    }

    /// The tail this cut may keep. The pressure gate and `/compact` take the
    /// policy's; an overflow attempt passes the previous allowance divided by
    /// the attempt number.
    fn tail_budget(self, policy: &context::Policy) -> u64 {
        match self {
            Self::Overflow { tail_budget } => tail_budget,
            _ => policy.tail_budget(),
        }
    }
}

/// What the gate saw when it fired, recorded beside the cut so a later reading
/// of the session answers "why" without reconstructing the arithmetic from the
/// message list — which is how a compaction that was three times overdue came
/// to look three times premature.
#[derive(Debug, Clone)]
pub struct CompactionGate {
    pub trigger: &'static str,
    /// Tokens the gate priced the wire at, before the cut.
    pub priced: u64,
    /// `Policy::budget()` that price was judged against.
    pub budget: u64,
    /// The same price taken again on the compacted transcript: where this is
    /// still over `budget`, the hard trim below is what saved the request.
    pub priced_after: u64,
    pub context_limit: u64,
    pub bytes_per_token: u64,
    /// Whether `priced` came from the provider's own meter (`true`) or from a
    /// byte estimate (`false`).
    pub anchored: bool,
    pub tail_budget: u64,
    /// Messages kept verbatim by the split. Zero here is total collapse: the
    /// transcript became the summary and nothing else.
    pub tail_kept: usize,
}

/// EventLog-first compaction record: tokens and messages
/// before/after, whether a summary was written, and its text
/// (secret-screened by `append`). The `/compact` path already wrote
/// begin/end records; the automatic loop cuts wrote nothing — so G0's
/// post-hoc lore question ("what survived the cut?") was unanswerable.
/// It is now; bytes-freed is `before - after`.
pub(crate) fn record_compaction(
    events: &mut Option<crate::agent::events::EventLog>,
    outcome: &CompactionOutcome,
    msgs_before: usize,
    msgs_after: usize,
    summary_text: &str,
) {
    let Some(writer) = events.as_mut() else {
        return;
    };
    let mut fields = serde_json::json!({
        "before": outcome.before,
        "after": outcome.after,
        "msgs_before": msgs_before,
        "msgs_after": msgs_after,
        "summarized": outcome.summarized,
        "summary": summary_text,
    });
    record_gate_fields(&mut fields, &outcome.gate);
    // The one loss the summary cannot describe: it was written before the cut.
    record_trim_fields(&mut fields, outcome.trimmed.as_ref());
    let _ = writer.append("compaction", fields);
}

/// Attach the gate's own numbers to a compaction record. Shared with the
/// `/compact` path, which writes its own richer event.
pub(crate) fn record_gate_fields(fields: &mut serde_json::Value, gate: &CompactionGate) {
    let Some(map) = fields.as_object_mut() else {
        return;
    };
    map.insert("trigger".into(), gate.trigger.into());
    map.insert("priced".into(), gate.priced.into());
    map.insert("budget".into(), gate.budget.into());
    map.insert("priced_after".into(), gate.priced_after.into());
    map.insert("context_limit".into(), gate.context_limit.into());
    map.insert("bpt".into(), gate.bytes_per_token.into());
    map.insert("anchored".into(), gate.anchored.into());
    map.insert("tail_budget".into(), gate.tail_budget.into());
    map.insert("tail_kept".into(), gate.tail_kept.into());
}

/// Attach the hard-trim report to a compaction record. Shared with the
/// `/compact` path, which writes its own richer event.
pub(crate) fn record_trim_fields(
    fields: &mut serde_json::Value,
    trimmed: Option<&context::TrimReport>,
) {
    let Some(trimmed) = trimmed else {
        return;
    };
    let Some(map) = fields.as_object_mut() else {
        return;
    };
    map.insert("trimmed_msgs".into(), trimmed.dropped.into());
    map.insert("trimmed_span".into(), trimmed.span.clone().into());
    if !trimmed.paths.is_empty() {
        map.insert(
            "trimmed_paths".into(),
            serde_json::Value::Array(
                trimmed
                    .paths
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
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

/// Run the compaction policy, cheapest stage first.
///
/// 1. prune aged-out tool output (no LLM);
/// 2. summarize the oldest turns through the model;
/// 3. if the summary could not be obtained, summarize locally;
/// 4. hard-trim if the history still does not fit.
///
/// Returns what the cut did, including the numbers the gate fired on.
/// A failure never propagates: stage 4 always leaves a usable transcript.
///
/// Pressure is judged on the WIRE, not the raw store: aged-out tool results are
/// stubs on the request, so stored bytes would keep firing compaction after the
/// request stopped growing. Once the provider has metered a whole-transcript
/// request, its token count anchors the measure and only the bytes gained or
/// lost since are estimated (`context::wire_estimated_tokens`) — which makes the
/// threshold mean the provider's window, prefix included, instead of a
/// history-only guess. The instruction prefix is a few tens of kilobytes against
/// a 204,800-token gate, so pricing it cannot make compaction fire for nothing.
/// Real overflows still force-compact via the overflow path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact_history(
    provider: &SharedProvider,
    model_id: &str,
    messages: &mut Vec<Message>,
    summary: &mut Option<String>,
    policy: &context::Policy,
    reason: CompactionReason,
    plan_hint: &str,
    focus: Option<&str>,
    prefix: Option<&CompactionPrefix<'_>>,
) -> Option<CompactionOutcome> {
    let force = reason.force();
    let measured = |m: &[Message]| context::estimated_tokens(m);
    let mut wire = crate::agent::tool_output::Wire {
        context_limit: policy.context_limit,
        bytes_per_token: policy.bytes_per_token,
        // The run's clear floor is not passed down: compaction judges pressure,
        // and pricing the wire as if nothing had been cleared yet estimates it
        // larger than it is — which triggers earlier, never later.
        cleared_floor: 0,
        measured: policy.measured,
    };
    // Takes the wire as an argument: the gate below re-prices the transcript
    // after the anchor has been dropped, and a closure holding `&wire` would
    // freeze it.
    let wire_measured =
        |m: &[Message], w: &crate::agent::tool_output::Wire| context::wire_estimated_tokens(m, w);
    let before = measured(messages);
    // The billing anchor prices the whole request, prefix included; a byte
    // estimate prices only the transcript. Until a measurement lands (session
    // start, and again right after every compaction) the prefix has to be
    // added by hand, or a large system block on a small window stays invisible
    // until the provider overflows.
    let fixed_tokens = prefix.map_or(0, |prefix| prefix_tokens(prefix, wire.bytes_per_token));
    let priced = |m: &[Message], w: &crate::agent::tool_output::Wire| {
        let tokens = wire_measured(m, w);
        if w.measured.is_none() {
            tokens.saturating_add(fixed_tokens)
        } else {
            tokens
        }
    };
    let gate_budget = policy.budget();
    let tail_budget = reason.tail_budget(policy);
    let priced_before = priced(messages, &wire);

    // No per-turn pruning by design (Codex shape): what the model read
    // stays verbatim until real pressure. Silently rewriting old tool
    // output taught the model to re-read the same files in circles —
    // the prune note even instructed it to. History shrinks only below,
    // on pressure, via summary or hard trim.
    if !force && policy.pressure(priced_before) == context::Pressure::Ok {
        // Pressure is fine: history is untouched, report nothing.
        return None;
    }

    // stage 2: summarize. The cut is safe by construction, so an assistant
    // tool call can never be separated from its results. `force` (manual
    // `/compact`) also shrinks the kept tail when the configured keep_turns
    // would leave nothing to summarize.
    let (older, keep) =
        context::split_for_summary_adaptive(messages, policy.keep_turns(), force, tail_budget);
    let older: Vec<Message> = older.to_vec();
    let keep: Vec<Message> = keep.to_vec();
    let keep_len = keep.len();
    let mut summarized = false;
    if !older.is_empty() {
        let mut wire_history = messages.clone();
        crate::agent::tool_output::trim_for_wire(&mut wire_history, &wire);
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
        // The billed anchor described the transcript that was just replaced.
        // What the check below prices is new, so it is priced from bytes.
        wire.measured = None;
    }

    // stage 4: still too big — drop the oldest turns outright. The prefix
    // keeps its share of the window, and the drop is reported: it is the one
    // fact the summary, written before the cut, cannot carry.
    let mut trimmed = None;
    let priced_after = priced(messages, &wire);
    if policy.pressure(priced_after) != context::Pressure::Ok {
        let budget = gate_budget.saturating_sub(if wire.measured.is_none() {
            fixed_tokens
        } else {
            0
        });
        let (kept, report) = context::hard_trim_with_report(messages, budget);
        *messages = kept;
        trimmed = report;
    }
    let after = measured(messages);
    if after == before && !summarized {
        return None;
    }
    Some(CompactionOutcome {
        before,
        after,
        summarized,
        trimmed,
        gate: CompactionGate {
            trigger: reason.label(),
            priced: priced_before,
            budget: gate_budget,
            priced_after,
            context_limit: policy.context_limit,
            bytes_per_token: policy.bytes_per_token,
            anchored: wire.measured.is_some(),
            tail_budget,
            tail_kept: keep_len,
        },
    })
}

/// What one compaction did, for the caller's journal record.
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub before: u64,
    pub after: u64,
    pub summarized: bool,
    pub trimmed: Option<context::TrimReport>,
    pub gate: CompactionGate,
}

/// Tokens the fixed part of the request occupies: system parts plus tool
/// schemas. Priced by bytes at the same ratio the transcript uses; the billing
/// anchor supersedes it once one exists.
fn prefix_tokens(prefix: &CompactionPrefix<'_>, bytes_per_token: u64) -> u64 {
    let bytes = prefix
        .system
        .iter()
        .map(|part| part.text.len())
        .sum::<usize>()
        + prefix
            .tools
            .iter()
            .map(|spec| {
                spec.name.len()
                    + spec.description.len()
                    + serde_json::to_string(&spec.parameters)
                        .map(|text| text.len())
                        .unwrap_or(0)
            })
            .sum::<usize>();
    (bytes as u64).div_ceil(bytes_per_token.max(1))
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

