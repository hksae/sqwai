use super::TurnOutcome;
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
/// L0 capture-nudge (deterministic): a fresh user message stating a
/// restriction ("don't touch X") while a plan is active, where no plan
/// constraint covers it, earns a host-owned tail line telling the model to
/// record the restriction or justify why it needs none. Unformalized lore
/// dies at the first hard trim; this is the cheapest place to catch it.
pub fn capture_nudge(user_text: &str, constraints: &[String]) -> Option<String> {
    let lower = user_text.to_lowercase();
    if !has_restriction_marker(&lower) {
        return None;
    }
    if constraint_covers(constraints, &lower) {
        return None;
    }
    Some(
        "\n[host note: this message states a restriction that is not among the \
         active plan's constraints — record it with plan constraints add, or \
         note why it needs no protection.]"
            .to_string(),
    )
}

/// Substring scan for negative directives (EN + RU). Checked before any
/// disk access so ordinary turns never pay for the plan lookup.
pub fn has_restriction_marker(lower_text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "don't touch",
        "do not touch",
        "не трогай",
        "don't change",
        "do not change",
        "не меняй",
        "don't modify",
        "do not modify",
        "don't break",
        "do not break",
        "не ломай",
        "don't delete",
        "do not delete",
        "не удаляй",
        "оставь как есть",
        "нельзя",
        "запрещ",
    ];
    MARKERS.iter().any(|m| lower_text.contains(m))
}

/// Crude deterministic coverage: a constraint covers the directive when they
/// share a significant token (4+ chars). Deliberately dumb — L0.
fn constraint_covers(constraints: &[String], lower_directive: &str) -> bool {
    let words: std::collections::HashSet<&str> = lower_directive
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .collect();
    if words.is_empty() {
        return false;
    }
    constraints.iter().any(|c| {
        c.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.chars().count() >= 4)
            .any(|w| words.contains(w))
    })
}

/// Preformatted durable-plan block for the restricted summary (§3.3.2):
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

/// Journal-first compaction record: tokens and messages
/// before/after, whether a summary was written, and its text
/// (secret-screened by `append`). The `/compact` path already wrote
/// begin/end records; the automatic loop cuts wrote nothing — so G0's
/// post-hoc lore question ("what survived the cut?") was unanswerable.
/// It is now; bytes-freed is `before - after`.
pub(crate) fn record_compaction(
    journal: &mut Option<crate::agent::journal::Journal>,
    before: u64,
    after: u64,
    msgs_before: usize,
    msgs_after: usize,
    summarized: bool,
    summary_text: &str,
) {
    let Some(writer) = journal.as_mut() else {
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
/// full history go on the wire unchanged, the summarization prompt is
/// appended as the last user message. Otherwise the compact standalone
/// request: tiny system, transcript rendered as text, no schemas.
pub(crate) fn compaction_request(
    prefix: Option<&CompactionPrefix<'_>>,
    older: &[Message],
    history: &[Message],
    previous: Option<&str>,
    plan_hint: &str,
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
                    context::summary_short_input(older, previous, plan_hint, retry),
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
        context::summary_short_prompt(previous, plan_hint, retry),
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

pub(crate) async fn compact_history(
    provider: &SharedProvider,
    model_id: &str,
    messages: &mut Vec<Message>,
    summary: &mut Option<String>,
    policy: &context::Policy,
    force: bool,
    plan_hint: &str,
    prefix: Option<&CompactionPrefix<'_>>,
) -> Option<(u64, u64, bool)> {
    let measured = |m: &[Message]| context::estimated_tokens(m);
    let before = measured(messages);

    // stage 1: prune. Cheap, lossless in structure, runs every turn.
    let (pruned, pruned_changed) = context::prune(messages);
    if pruned_changed {
        *messages = pruned;
    }
    if std::env::var("SQWAI_BENCH_DEBUG").is_ok() {
        let measured_now = measured(messages);
        eprintln!(
            "bench-debug: pressure check: measured={} budget={} limit={} msgs={}",
            measured_now,
            policy.budget(),
            policy.context_limit,
            messages.len(),
        );
        eprintln!(
            "[compact] check: measured={} budget={} triggered={}",
            measured_now,
            policy.budget(),
            measured_now >= policy.budget()
        );
    }
    if !force && policy.pressure(measured(messages)) == context::Pressure::Ok {
        // Pressure is fine, so no summarization or hard trim will run.
        // Stage-1 `prune` may have shrunk the chat history; its effect is
        // already visible inline via PRUNE_NOTE on the trimmed tool result,
        // so stay silent instead of lying about the token count.
        return None;
    }

    // stage 2: summarize. The cut is safe by construction, so an assistant
    // tool call can never be separated from its results.
    let (older, keep) = context::split_for_summary_with_keep(messages, policy.keep_turns());
    let older: Vec<Message> = older.to_vec();
    let keep: Vec<Message> = keep.to_vec();
    let mut summarized = false;
    let debug = std::env::var("SQWAI_BENCH_DEBUG").is_ok();
    if debug {
        eprintln!(
            "bench-debug: stages: older={} summary_enabled={}",
            older.len(),
            policy.summary_enabled,
        );
    }
    if !older.is_empty() && policy.summary_enabled {
        let (request, cache_aware) = compaction_request(
            prefix,
            &older,
            messages,
            summary.as_deref(),
            plan_hint,
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
                context::local_summary(&older, summary.as_deref())
            }
        };
        // the wire cap is advisory: a severely over-budget answer gets one
        // retry with an explicit hard limit before the host truncates.
        // The retry keeps the request shape (cache-aware or standalone) and
        // only rewords the prompt.
        let text = if text.chars().count() > 2 * context::SUMMARY_SHORT_MAX_CHARS {
            let (retry_request, _) = compaction_request(
                prefix,
                &older,
                messages,
                summary.as_deref(),
                plan_hint,
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
        if std::env::var("SQWAI_BENCH_DEBUG").is_ok() {
            eprintln!(
                "bench-debug: summary {} chars after cap ({} before)",
                context::truncate_summary(&text).chars().count(),
                text.chars().count()
            );
        }
        let text = context::truncate_summary(&text);
        *messages = context::apply_summary(&text, &keep);
        *summary = Some(text);
        summarized = true;
    }

    // stage 4: still too big — drop the oldest turns outright
    if policy.pressure(measured(messages)) != context::Pressure::Ok {
        let budget = policy.budget();
        let before_len = messages.len();
        *messages = context::hard_trim(messages, budget);
        if debug {
            eprintln!(
                "bench-debug: stage4: {} -> {} msgs (budget {})",
                before_len,
                messages.len(),
                budget,
            );
        }
    } else if debug {
        eprintln!("bench-debug: stage4 skipped (fits)");
    }
    let after = measured(messages);
    if std::env::var("SQWAI_BENCH_DEBUG").is_ok() {
        eprintln!("[compact] done, new msgs={}", messages.len());
    }
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
        }
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
/// A provider refusing the reasoning parameter, in the several shapes the
/// gateways phrase it. This is prose matching, which is unreliable by nature —
/// but the alternative is dropping the effort level for the whole session on
/// any 400, which is worse. When it misses, the turn fails as before.
/// Evidence that a requested effort level was not acted on, or `None` when
/// there is none. The distinction that matters: `reasoning_tokens: None` means
/// the provider said nothing about reasoning, which is not the same as saying
/// it did none — only an explicit zero is evidence (§5.1, §1.1).
/// How many consecutive zero-reasoning turns it takes before the host says
/// anything. One is not enough: a reasoning model may legitimately spend
/// nothing on a trivial question, and a gateway may stub its usage details.
pub(crate) const MIN_ZERO_TURNS_BEFORE_REPORTING: u32 = 2;

/// True when this turn asked for effort and came back with no reasoning at all.
pub(crate) fn turn_shows_no_reasoning(turn: &TurnOutcome) -> bool {
    // Streamed reasoning content contradicts a zero count whatever the usage
    // block says, so it clears the turn outright.
    turn.reasoning_tokens == Some(0) && !turn.saw_reasoning
}

/// What the host can honestly say about a level that did not land, or `None`
/// when the evidence is not there yet.
///
/// The two sources are not equally strong and are no longer worded as if they
/// were. A refused parameter is proof: the provider said so. A zero counter is
/// an observation that can also mean "the model chose not to think here" or
/// "this gateway stubs the details block" — field data from a third-party
/// OpenAI-compatible gateway showed both `cached` and `reasoning` pinned at
/// zero on a 10k-token prompt, alongside a 24-second turn that produced six
/// output tokens. So it is reported only after several turns, and phrased as
/// what was measured rather than as a verdict about the model.
pub(crate) fn effort_ignored_reason(turn: &TurnOutcome, consecutive_zero_turns: u32) -> Option<String> {
    if turn.effort_rejected {
        return Some("the provider rejected the effort parameter".to_string());
    }
    // The run has to include this turn: a turn that produced reasoning ends
    // the run, and the caller resets the count for exactly that reason.
    if turn_shows_no_reasoning(turn) && consecutive_zero_turns >= MIN_ZERO_TURNS_BEFORE_REPORTING {
        return Some(format!(
            "no reasoning reported on {consecutive_zero_turns} turns in a row"
        ));
    }
    None
}

/// A provider rejecting the continuation reference, in the shapes seen so far.
/// The first is what a relay says when it accepted `previous_response_id` and
/// kept nothing behind it: the tool outputs arrive with no calls to match.
pub(crate) fn rejects_continuation(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no tool call found for function call output")
        || (lower.contains("previous_response") && !lower.contains("not supported"))
        || lower.contains("previous response not found")
}

pub(crate) fn rejects_effort_parameter(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    (lower.contains("reasoning_effort")
        || lower.contains("reasoning.effort")
        || lower.contains("thinking")
        || lower.contains("budget_tokens"))
        && (lower.contains("not supported")
            || lower.contains("unsupported")
            || lower.contains("unrecognized")
            || lower.contains("unknown")
            || lower.contains("not allowed")
            || lower.contains("invalid"))
}

