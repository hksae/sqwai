//! Compaction policy (opencode-style).
//!
//! Three stages, cheapest first:
//!
//! 1. **prune** — shrink old tool output in place. No LLM, no structural
//!    change: the model still sees every turn, just less payload.
//! 2. **summarize** — ask the model to compress the oldest turns into one
//!    summary message, keeping the recent tail verbatim.
//! 3. **hard trim** — drop the oldest turns entirely. Never fails.
//!
//! Stage 2 depends on a provider round-trip and can fail; it then falls back
//! to stage 3. A session that hits the ceiling therefore keeps working with a
//! degraded memory instead of dying on a failed request.
//!
//! The durable plan is not touched by any stage: it lives in
//! `.sqwai/plans/<id>.json` and is re-injected into the system
//! block every request, so compaction can never cost the agent
//! its goal or next step.

use crate::providers::{Message, Role};

/// Host-generated state anchor for the moments when the transcript really has
/// lost detail: after a compaction and on a restored session. It contains only
/// durable plan data and bounded journal-derived facts; the model never writes
/// or rewrites this block.
///
/// It used to ride every request while `system.md` told the model to treat it
/// as the source of truth "because earlier history may be gone" — which taught
/// a live session to distrust the conversation in front of it and re-answer its
/// own previous turn.
pub fn anchor(root: &std::path::Path, session_id: &str) -> String {
    let mut out =
        String::from("ANCHOR (host-generated working memo; journal facts, not a summary)\n");
    // No plan content here by design: goal/constraints/step state ride
    // live on every request (rebuilt per request, cache-hot when
    // unchanged), so a turn-start snapshot would only resurrect the
    // two-plan problem. The anchor keeps what the journal proves got done.
    let mut changed = Vec::new();
    let mut recent_reads: Vec<String> = Vec::new();
    let mut last_verification = None;
    if let Ok(records) = crate::agent::journal::Journal::records_for(root, session_id) {
        for record in records {
            if record.kind == "file_diff"
                && let Some(path) = record.fields.get("path").and_then(|value| value.as_str())
            {
                changed.push(bounded(path, 160));
            }
            // An `/undo` takes back what it names: records arrive in sequence
            // order, so a later write re-lists the path and the memo follows
            // the tree instead of the session's whole history. Without this
            // the anchor tells the model, exactly when it is re-grounded after
            // a compaction, that reverted files still hold the agent's work.
            if record.kind == "undo"
                && let Some(files) = record.fields.get("files").and_then(|v| v.as_array())
            {
                for file in files.iter().filter_map(|v| v.as_str()) {
                    let name = bounded(file, 160);
                    changed.retain(|path: &String| *path != name);
                }
            }
            // A `/redo` puts the reverted files back into the worktree.
            if record.kind == "redo"
                && let Some(files) = record.fields.get("files").and_then(|v| v.as_array())
            {
                for file in files.iter().filter_map(|v| v.as_str()) {
                    changed.push(bounded(file, 160));
                }
            }
            if record.kind == "tool_call"
                && record
                    .fields
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| matches!(t, "read" | "outline"))
            {
                let path_opt = record
                    .fields
                    .get("args")
                    .and_then(|a| {
                        a.get("file_path")
                            .or_else(|| a.get("path"))
                            .and_then(|v| v.as_str())
                    })
                    .or_else(|| record.fields.get("args_digest").and_then(|v| v.as_str()));
                if let Some(raw_path) = path_opt {
                    let clean = raw_path
                        .split_whitespace()
                        .next()
                        .unwrap_or(raw_path)
                        .trim();
                    if !clean.is_empty() {
                        let name = bounded(clean, 160);
                        recent_reads.retain(|p| *p != name);
                        recent_reads.push(name);
                    }
                }
            }
            if record.kind == "tool_result"
                && record
                    .fields
                    .get("tool")
                    .and_then(|value| value.as_str())
                    .is_some_and(|tool| matches!(tool, "bash" | "git_diff" | "git_commit"))
                && record.fields.get("ok").and_then(|value| value.as_bool()) == Some(true)
            {
                last_verification = Some(record.seq);
            }
        }
    }
    changed.sort();
    changed.dedup();
    if changed.is_empty() {
        out.push_str("files changed this session: none\n");
    } else {
        out.push_str(&format!(
            "files changed this session: {}\n",
            changed.iter().take(24).cloned().collect::<Vec<_>>().join(" · ")
        ));
    }
    recent_reads.retain(|p| !changed.contains(p));
    if !recent_reads.is_empty() {
        let start = recent_reads.len().saturating_sub(5);
        out.push_str(&format!(
            "recent files read: {}\n",
            recent_reads[start..].join(" · ")
        ));
    }
    out.push_str(&format!(
        "last verification: {}\n",
        last_verification
            .map(|seq| format!("successful exec j#{seq}"))
            .unwrap_or_else(|| "none".to_string())
    ));
    // asks the anchor to surface open assumptions: compaction is
    // exactly when an unresolved assumption would otherwise vanish from the
    // conversation while still holding up the work. Scoped to this session:
    // sequence numbers restart per journal file.
    let open = crate::agent::journal::Journal::open_assumptions_in(root, session_id, None)
        .unwrap_or_default();
    if open.is_empty() {
        out.push_str("open assumptions: none\n");
    } else {
        out.push_str(&format!(
            "open assumptions: {}\n",
            open.iter()
                .take(6)
                .map(|item| item.label(120))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    // decisions, lessons and rejected approaches survive for the same
    // reason: post-compaction "why did we do it this way" is otherwise
    // answered from model memory rather than from facts.
    let decisions =
        crate::agent::journal::Journal::decision_notes_in(root, session_id).unwrap_or_default();
    if decisions.is_empty() {
        out.push_str("decisions: none\n");
    } else {
        let start = decisions.len().saturating_sub(4);
        out.push_str(&format!(
            "decisions: {}\n",
            decisions[start..]
                .iter()
                .map(|text| bounded(text, 160))
                .collect::<Vec<_>>()
                .join(" · ")
        ));
    }
    // recent failures with their own words: exact error texts the next turn
    // needs, without re-running the command. Cancelled runs are the user's
    // doing, not failures, and stay out.
    let mut failures = Vec::new();
    if let Ok(records) = crate::agent::journal::Journal::records_for(root, session_id) {
        for record in records.iter().rev() {
            if failures.len() >= 2 {
                break;
            }
            if record.kind != "tool_result" {
                continue;
            }
            if record.fields.get("ok").and_then(|value| value.as_bool()) != Some(false) {
                continue;
            }
            if record.fields.get("code").and_then(|value| value.as_str()) == Some("cancelled") {
                continue;
            }
            let tool = record
                .fields
                .get("tool")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let summary = record
                .fields
                .get("summary")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            failures.push(format!(
                "{tool} j#{}: {}",
                record.seq,
                bounded(summary, 200)
            ));
        }
    }
    if failures.is_empty() {
        out.push_str("recent failures: none\n");
    } else {
        out.push_str(&format!("recent failures: {}\n", failures.join(" · ")));
    }
    // No imperatives here. The anchor describes what the journal proves; the
    // rule that durable facts belong in `memory_write` lives in system.md, where
    // it is stated once. Riding every request as a command is what made a live
    // session call `memory_write` in the middle of an unrelated loop.
    out
}

/// Return the host-owned instruction used when a session resumes mid-step.
/// A plan `start` without a later `finish`, `block`, or `cancel` is evidence
/// that the process stopped while work was in progress.
///
/// The journal signal alone is not trusted: steps from older, abandoned
/// plans leave `start` records too, and a plan file can be revised after the
/// crash. The open step is only reported when it belongs to the currently
/// active plan for this session and the plan's own state agrees that the
/// step is in progress.
pub fn resume_notice(root: &std::path::Path, session_id: &str) -> Option<String> {
    let records = crate::agent::journal::Journal::records_for(root, session_id).ok()?;
    let mut open_step = None;
    for record in records.iter().filter(|record| record.kind == "plan") {
        // Rejected ops never took effect; only accepted intents resume work.
        if record.fields.get("ok") == Some(&serde_json::Value::Bool(false)) {
            continue;
        }
        let op = record.fields.get("op").and_then(|value| value.as_str());
        let id = record.fields.get("id").and_then(|value| value.as_str());
        match (op, id) {
            (Some("start"), Some(id)) => open_step = Some((id.to_string(), record.seq)),
            (Some("finish" | "block" | "cancel"), Some(id))
                if open_step.as_ref().is_some_and(|(open, _)| open == id) =>
            {
                open_step = None;
            }
            _ => {}
        }
    }
    let (step_id, start_seq) = open_step?;
    // cross-check against the active plan: a step that the plan does not
    // list, or lists as not started, must not produce a phantom "resume"
    let active = crate::plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten();
    if let Some(plan) = &active {
        let in_progress = plan.steps.iter().any(|step| {
            step.id == step_id && matches!(step.status, crate::plan::StepStatus::InProgress)
        });
        if !in_progress {
            return None;
        }
    }
    let recent = records
        .iter()
        .filter(|record| record.seq > start_seq)
        .rev()
        .take(2)
        .map(|record| format!("j#{} {}", record.seq, record.kind))
        .collect::<Vec<_>>();
    let suffix = if recent.is_empty() {
        String::new()
    } else {
        format!(
            "; last events: {}",
            recent.into_iter().rev().collect::<Vec<_>>().join(", ")
        )
    };
    Some(format!(
        "Session resumed. Step {step_id} was in progress{suffix}. Continue it, block it with a reason, or ask the user."
    ))
}

fn bounded(text: &str, max_chars: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max_chars {
        text
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
    }
}

/// messages kept verbatim by the local (no-LLM) compaction path
const RECENT_MESSAGE_COUNT: usize = 8;
const MAX_TOOL_OUTPUT_CHARS: usize = 12_000;
const SUMMARY_SNIPPET_CHARS: usize = 420;

pub const PRUNE_NOTE: &str =
    "…(old tool output compacted; rerun the tool to see the full result again)";

/// messages kept verbatim after a summarization
const SUMMARY_KEEP_RECENT: usize = 6;
/// share of the context kept free for the answer
const RESERVE_RATIO: f64 = 0.2;
const RESERVE_MIN: u64 = 8_000;
const RESERVE_MAX: u64 = 32_000;

/// System block for the summarization request. Deliberately tiny: this request
/// competes for the same context it is trying to free.
pub const SUMMARY_SYSTEM: &str = "You summarize a coding-agent conversation. Output plain text \
only, no preamble, no markdown headings beyond the ones requested. Be dense and factual: the \
summary replaces the messages it covers.";

/// What the context ceiling asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// history fits
    Ok,
    /// history no longer fits: summarize or prune/trim when summary is off
    Summarize,
}

/// Budget policy derived from the model's context limit.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub context_limit: u64,
    /// Fraction of context reserved for the next answer. `None` keeps the
    /// historical defaults; configuration can override it without changing
    /// the host-generated anchor policy.
    pub anchor_ratio: Option<f64>,
    pub keep_turns: Option<usize>,
    pub summary_enabled: bool,
    /// Fraction of the context at which compaction triggers
    /// (`[compaction].threshold`, 0.80). Combined with the reserve below:
    /// the trigger is whichever bites first, so a large reserve on a small
    /// model still leaves room for the answer.
    pub threshold: Option<f64>,
}

impl Policy {
    #[allow(dead_code)]
    pub fn new(context_limit: u64) -> Self {
        Self {
            context_limit,
            anchor_ratio: None,
            keep_turns: None,
            summary_enabled: false,
            threshold: None,
        }
    }

    pub fn with_compaction(
        context_limit: u64,
        anchor_ratio: f64,
        keep_turns: usize,
        threshold: f64,
        summary_enabled: bool,
    ) -> Self {
        Self {
            context_limit,
            anchor_ratio: Some(anchor_ratio),
            keep_turns: Some(keep_turns),
            summary_enabled,
            threshold: Some(threshold),
        }
    }

    /// Tokens kept free for the answer so a compaction request never lands
    /// after the model has already run out of room.
    pub fn reserve(&self) -> u64 {
        if self.context_limit == 0 {
            return 0;
        }
        let ratio = self.anchor_ratio.unwrap_or(RESERVE_RATIO).clamp(0.0, 0.95);
        let raw = (self.context_limit as f64 * ratio).round() as u64;
        raw.clamp(RESERVE_MIN, RESERVE_MAX).min(self.context_limit)
    }

    /// how much history may accumulate before compaction is triggered:
    /// the configured threshold fraction, but never more than the limit
    /// minus the answer reserve — whichever bites first. No threshold
    /// configured (legacy `Policy::new`) means reserve-only, as before.
    pub fn budget(&self) -> u64 {
        if self.context_limit == 0 {
            return u64::MAX;
        }
        let by_reserve = self.context_limit.saturating_sub(self.reserve());
        match self.threshold {
            None => by_reserve,
            Some(t) => ((self.context_limit as f64 * t.clamp(0.0, 1.0)).round() as u64)
                .min(self.context_limit)
                .min(by_reserve),
        }
    }

    pub fn keep_turns(&self) -> usize {
        self.keep_turns.unwrap_or(SUMMARY_KEEP_RECENT)
    }

    pub fn pressure(&self, tokens: u64) -> Pressure {
        if self.context_limit == 0 || tokens <= self.budget() {
            Pressure::Ok
        } else {
            Pressure::Summarize
        }
    }
}

/// Provider-free compaction utility (keep the recent tail, extractively
/// summarize the older turns). It is the standalone fallback an agent can use
/// when no model is available to rewrite the history; the inline
/// `compact_history` pipeline in `loop_task` reuses `local_summary` for the
/// same purpose. The active pipeline can disable model summaries and fall back
/// to pruning/trimming while preserving the host-generated anchor. Exercised
/// only by the tests at the bottom of this file, so the non-test build would
/// otherwise flag it dead.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct Compaction {
    pub summary: String,
    pub messages: Vec<Message>,
    pub compacted: bool,
}

/// Local compaction: keep the newest tail, summarize the rest by extraction.
///
/// Used as the fallback when the model could not summarize, and by callers
/// that have no provider at hand. Never fails and never needs the network.
#[allow(dead_code)]
pub fn compact(messages: &[Message], existing_summary: Option<&str>) -> Compaction {
    let boundary = retained_boundary(messages);
    let (older, recent) = messages.split_at(boundary);
    let mut out = Compaction {
        summary: local_summary(older, existing_summary),
        messages: recent.iter().map(prune_message).collect(),
        compacted: boundary > 0,
    };
    if !out.compacted {
        out.summary = existing_summary.unwrap_or_default().to_string();
    }
    out
}

pub fn estimated_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            (m.content.len() as u64
                + m.tool_calls
                    .iter()
                    .map(|c| c.name.len() as u64 + c.args.to_string().len() as u64)
                    .sum::<u64>())
            .div_ceil(4)
        })
        .sum()
}

/// Stage 2: split the history into (to_summarize, keep_verbatim).
///
/// `keep_turns` counts user turns, not messages: the last `keep_turns` user
/// messages and everything after the earliest of them stays verbatim.
/// The cut lands on a user turn and never separates an assistant tool call
/// from its results — providers reject orphaned tool results.
/// Test-only shorthand: production always passes an explicit keep_turns.
#[allow(dead_code)]
pub fn split_for_summary(messages: &[Message]) -> (&[Message], &[Message]) {
    split_for_summary_with_keep(messages, SUMMARY_KEEP_RECENT)
}

pub fn split_for_summary_with_keep(
    messages: &[Message],
    keep_turns: usize,
) -> (&[Message], &[Message]) {
    // user-turn indices from the end; keep the last `keep_turns` of them
    // and drop everything before the earliest kept one
    let keep = keep_turns.max(1);
    let mut from_end: Vec<usize> = messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, _)| index)
        .take(keep + 1)
        .collect();
    if from_end.len() <= keep {
        // Few user turns but possibly a long autonomous tool loop: count
        // agent steps the same way, or a single-prompt run never summarizes
        // and grows until the provider rejects it.
        let mut steps: Vec<usize> = messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| message.role == Role::Assistant)
            .map(|(index, _)| index)
            .take(keep + 1)
            .collect();
        if steps.len() <= keep {
            return (&[], messages);
        }
        steps.truncate(keep);
        let mut cut = *steps.last().expect("keep >= 1");
        while cut > 0 && !is_safe_cut(messages, cut) {
            cut -= 1;
        }
        if cut == 0 {
            return (&[], messages);
        }
        return (&messages[..cut], &messages[cut..]);
    }
    from_end.truncate(keep);
    let mut cut = *from_end.last().expect("keep >= 1");
    while cut > 0 && !is_safe_cut(messages, cut) {
        cut -= 1;
    }
    if cut == 0 {
        return (&[], messages);
    }
    (&messages[..cut], &messages[cut..])
}

fn is_safe_cut(messages: &[Message], cut: usize) -> bool {
    if cut >= messages.len() {
        return false;
    }
    if messages[cut].role == Role::User {
        // a pending assistant tool call must keep its results
        return !matches!(
            messages.get(cut.saturating_sub(1)),
            Some(prev) if prev.role == Role::Assistant && !prev.tool_calls.is_empty()
        );
    }
    // An autonomous tool loop has no further user turns: cutting before an
    // assistant step is the only way to bound it. Safe as long as no kept
    // tool result is orphaned, i.e. every kept result's call is kept too.
    // Never cut before a tool result or a system note.
    if messages[cut].role != Role::Assistant {
        return false;
    }
    let kept: std::collections::HashSet<&str> = messages[cut..]
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
        .collect();
    messages[cut..]
        .iter()
        .filter(|m| m.role == Role::Tool)
        .all(|m| match &m.tool_call_id {
            None => true,
            Some(id) => kept.contains(id.as_str()),
        })
}

/// A previously written summary block. Such a message never goes back
/// through summarization inline: its text rides via `<previous-summary>`
/// (see `summary_input`), so rendering it into the transcript would nest
/// tags and duplicate tokens on every further compaction.
fn is_summary_message(message: &Message) -> bool {
    message.content.contains("<conversation-summary>")
}

/// Render the part of the conversation that is about to be replaced.
pub fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        if is_summary_message(message) {
            continue;
        }
        let label = match message.role {
            Role::User => "User",
            Role::Assistant => "Agent",
            Role::Tool => "Tool",
            Role::System => "System",
        };
        let snippet = compact_whitespace(&message.content);
        if !snippet.is_empty() {
            out.push_str(&format!("- {label}: {}\n", truncate(&snippet, 2_000)));
        }
        for call in &message.tool_calls {
            let args = compact_whitespace(&call.args.to_string());
            out.push_str(&format!(
                "- Agent called {} with {}\n",
                call.name,
                truncate(&args, 400)
            ));
        }
    }
    out
}

/// Cap for the restricted short summary: the only summary mode
/// (`compaction.summary: off|short`). Anything the plan already owns must
/// not be re-listed — the budget exists for what the plan does not hold.
pub const SUMMARY_SHORT_MAX_TOKENS: u32 = 300;

/// Host-enforced character cap for stored/applied summaries. Endpoints
/// ignore `max_output_tokens` (measured 33K–140K chars against a 300-token
/// cap), so without this the chained summary grows every cycle and late
/// compactions free ~nothing at full call price.
pub const SUMMARY_SHORT_MAX_CHARS: usize = 1_200;

/// Enforce the cap: truncate to whole characters, mark the cut so a
/// reader knows the tail is missing (truncation drops the END — goal and
/// constraints ride first in these summaries, fresh work survives in the
/// verbatim keep-tail and the journal).
pub fn truncate_summary(text: &str) -> String {
    const MARKER: &str = "\n[…truncated to fit the compaction budget]";
    if text.chars().count() <= SUMMARY_SHORT_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text
        .chars()
        .take(SUMMARY_SHORT_MAX_CHARS - MARKER.chars().count())
        .collect();
    format!("{head}{MARKER}")
}

/// The user turn of the restricted summarization request: ONLY
/// what the user explicitly asked, forbade, or stated as fact in the
/// dropped messages that is NOT already covered by the durable plan.
/// `plan_hint` is preformatted "Goal: …\nConstraints: …" (possibly empty:
/// with no plan, every user request counts as uncovered). `retry` appends
/// the hard-limit line for the second attempt after a blown budget.
/// Shared head: what to summarize and the plan to exclude. `setting` names
/// where the conversation lives — rendered inline below (standalone
/// request) or travelling as the request's own messages above (cache-aware
/// request, where repeating it would pay twice).
fn summary_head(previous: Option<&str>, plan_hint: &str, setting: &str) -> String {
    let mut out = format!(
        "Summarize ONLY what the user explicitly asked, forbade, or stated \
         as fact {setting} — and ONLY what is not \
         already covered by the durable plan. The plan owns goals, \
         constraints, and decisions; re-listing them wastes the budget this \
         summary exists to save.\n\n<durable-plan>\n",
    );
    if plan_hint.trim().is_empty() {
        out.push_str("(no durable plan: include all user requests)\n");
    } else {
        out.push_str(plan_hint.trim());
        out.push('\n');
    }
    out.push_str("</durable-plan>\n");
    if let Some(prev) = previous.filter(|s| !s.trim().is_empty()) {
        out.push_str("\n<previous-summary>\n");
        out.push_str(prev.trim());
        out.push_str("\n</previous-summary>\n");
    }
    out
}

/// Shared rules tail. The no-tools line matters wherever schemas travel
/// with the request: the cache-aware variant carries the parent's tools to
/// keep the prefix byte-identical, and the model must still answer in text.
fn summary_tail(retry: bool) -> String {
    let mut out = String::from(
        "Rules: no preamble, no questions, no instructions \
         to the reader, no invented facts, no tool calls — respond with text \
         only. If something is unknown, say it \
         is unknown.",
    );
    if retry {
        out.push_str(&format!(
            "\n\nHARD LIMIT: the previous answer blew the budget. Answer again \
             in at most {SUMMARY_SHORT_MAX_CHARS} characters, plain prose."
        ));
    }
    out
}

pub fn summary_short_input(
    older: &[Message],
    previous: Option<&str>,
    plan_hint: &str,
    retry: bool,
) -> String {
    let mut out = summary_head(previous, plan_hint, "in the dropped conversation below");
    out.push_str("\n<conversation>\n");
    out.push_str(&transcript(older));
    out.push_str("</conversation>\n\n");
    out.push_str(&summary_tail(retry));
    out
}

/// The summarization prompt without the transcript: for the cache-aware
/// request, where the conversation travels as the request's own messages
/// and the prompt is appended last. Same head, same rules, nothing repeated.
pub fn summary_short_prompt(previous: Option<&str>, plan_hint: &str, retry: bool) -> String {
    let mut out = summary_head(previous, plan_hint, "in the conversation above");
    out.push_str(
        "\nThat conversation is the messages of this request, above this \
         prompt. Do not repeat any of it; summarize only.\n\n",
    );
    out.push_str(&summary_tail(retry));
    out
}

/// Stage 2 result: a fresh history that opens with the summary.
///
/// The summary rides in a **user** turn: several providers reject a
/// conversation that starts with an assistant message.
pub fn apply_summary(summary: &str, keep: &[Message]) -> Vec<Message> {
    let summary = summary.trim();
    let mut out = Vec::with_capacity(keep.len() + 1);
    out.push(Message::new(Role::User, summary_message(summary)));
    out.extend(keep.iter().cloned());
    out
}

fn summary_message(summary: &str) -> String {
    format!(
        "<conversation-summary>\n{summary}\n</conversation-summary>\n\
         This is the summarized earlier part of our conversation. Continue the \
         task from here. The durable project plan is in your system block; \
         re-read a file before editing it."
    )
}

/// Stage 3, last resort: drop the oldest turns until the history fits.
/// Never fails and never orphans a tool call.
///
/// A leading summary block is the only record of already-dropped turns: it
/// is exempt from trimming (the tail behind it is trimmed instead), or the
/// next compaction has nothing to chain onto. This bites exactly when the
/// kept tail alone exceeds the budget — e.g. a forced-low threshold with
/// heavy tool traffic — where an unprotected trim would eat the summary it
/// has just written.
pub fn hard_trim(messages: &[Message], budget: u64) -> Vec<Message> {
    let (head, rest) = match messages.first() {
        Some(first) if first.content.contains("<conversation-summary>") => {
            (Some(first.clone()), &messages[1..])
        }
        _ => (None, messages),
    };
    let trimmed = hard_trim_inner(rest, budget);
    match head {
        Some(h) => std::iter::once(h).chain(trimmed).collect(),
        None => trimmed,
    }
}

fn hard_trim_inner(messages: &[Message], budget: u64) -> Vec<Message> {
    if estimated_tokens(messages) <= budget {
        return messages.to_vec();
    }
    let mut start = 0usize;
    while start < messages.len() && estimated_tokens(&messages[start..]) > budget {
        start += 1;
    }
    while start < messages.len() && !is_safe_cut(messages, start) {
        start += 1;
    }
    if start >= messages.len() {
        if let Some(pos) = messages.iter().rposition(|m| m.role == Role::User) {
            return messages[pos..].to_vec();
        }
        return vec![Message::new(Role::User, "Continue.")];
    }
    let mut kept = messages[start..].to_vec();
    // Anthropic rejects an assistant-first transcript with a 400, and the
    // only safe cut in an autonomous tool loop is before an assistant step
    // (audit H6). A host-authored marker opens the transcript honestly.
    if kept.first().is_some_and(|m| m.role != Role::User) {
        kept.insert(0, Message::new(Role::User, TRIM_MARKER));
    }
    kept
}

/// Host note replacing the trimmed head when the safe cut lands mid tool
/// loop. Host-authored, not model text: nothing here claims anything.
const TRIM_MARKER: &str = "[earlier turns were trimmed to fit the context window]";

/// True when `messages` or `summary` shows that earlier turns were compacted or
/// hard-trimmed, so the host-generated `ANCHOR` should ride on requests.
pub fn has_compacted_history(summary: Option<&str>, messages: &[Message]) -> bool {
    summary.is_some()
        || messages.first().is_some_and(|m| {
            m.content.starts_with(TRIM_MARKER) || m.content.contains("<conversation-summary>")
        })
}

#[allow(dead_code)]
/// Boundary for the local compaction path: keep the most recent
/// `RECENT_MESSAGE_COUNT` messages verbatim and summarize everything older.
fn retained_boundary(messages: &[Message]) -> usize {
    messages.len().saturating_sub(RECENT_MESSAGE_COUNT)
}

#[allow(dead_code)]
fn prune_message(message: &Message) -> Message {
    if message.role != Role::Tool || message.content.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return message.clone();
    }
    let head: String = message
        .content
        .chars()
        .take(MAX_TOOL_OUTPUT_CHARS)
        .collect();
    let mut copy = message.clone();
    copy.content = format!("{head}\n{PRUNE_NOTE}");
    copy
}

/// Extractive summary of `older`, merged with what was already known.
///
/// Used when the model cannot summarize (no provider, failed request,
/// cancellation). It keeps facts verbatim instead of rewriting them, so it is
/// worse to read but cannot invent anything.
pub fn local_summary(messages: &[Message], existing: Option<&str>) -> String {
    let mut lines = Vec::new();
    let previous = existing.filter(|s| !s.trim().is_empty());
    if let Some(text) = previous {
        lines.push(text.trim().to_string());
    }
    for message in messages {
        // a previous summary block rides via `existing`, never inline —
        // otherwise every fallback nests the old tags one level deeper
        if previous.is_some() && is_summary_message(message) {
            continue;
        }
        let label = match message.role {
            Role::User => "User",
            Role::Assistant => "Agent",
            Role::Tool => "Tool",
            Role::System => "System",
        };
        let snippet = compact_whitespace(&message.content);
        if !snippet.is_empty() {
            lines.push(format!(
                "- {label}: {}",
                truncate(&snippet, SUMMARY_SNIPPET_CHARS)
            ));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("## Earlier conversation summary\n{}", lines.join("\n"))
}

fn compact_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests;
