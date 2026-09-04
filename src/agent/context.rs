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
//! `.sqwai/plan.md` and is re-injected into the system block every request
//! (design §5), so compaction can never cost the agent its goal or next step.

use crate::providers::{Message, Role};

/// Host-generated state anchor used after compaction and on session start.
/// It contains only durable plan data and bounded journal-derived facts; the
/// model never writes or rewrites this block.
pub fn anchor(root: &std::path::Path, session_id: &str) -> String {
    let mut out = String::from("ANCHOR (host-generated; source of truth after compaction)\n");
    if let Ok(Some(plan)) = crate::plan::open_active(root) {
        out.push_str(&format!("goal: {}\n", bounded(&plan.goal.text, 500)));
        if plan.constraints.is_empty() {
            out.push_str("constraints: none\n");
        } else {
            out.push_str("constraints: ");
            out.push_str(
                &plan
                    .constraints
                    .iter()
                    .map(|constraint| bounded(constraint, 240))
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            out.push('\n');
        }
        if plan.acceptance.is_empty() {
            out.push_str("acceptance: none\n");
        } else {
            out.push_str("acceptance: ");
            out.push_str(
                &plan
                    .acceptance
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        format!(
                            "[{index}] {}{}",
                            item.status.as_str(),
                            item.evidence
                                .last()
                                .map(|seq| format!(" j#{seq}"))
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            out.push('\n');
        }
        let counts = plan.counts();
        out.push_str(&format!(
            "plan {} rev {}: {} done · {} in_progress · {} blocked · {} pending · {} cancelled\n",
            bounded(&plan.id, 80),
            plan.revision,
            counts.done,
            counts.in_progress,
            counts.blocked,
            counts.pending,
            counts.cancelled
        ));
        for step in plan
            .steps
            .iter()
            .filter(|step| {
                matches!(
                    step.status,
                    crate::plan::StepStatus::InProgress
                        | crate::plan::StepStatus::Blocked
                        | crate::plan::StepStatus::Pending
                )
            })
            .take(8)
        {
            out.push_str(&format!(
                "  step {} {} {}\n",
                bounded(&step.id, 40),
                step.status.as_str(),
                bounded(&step.title, 240)
            ));
        }
        for folded in plan.folded.iter().take(8) {
            out.push_str(&format!("  folded: {}\n", bounded(&folded.text, 240)));
        }
    } else {
        out.push_str("goal: none\nconstraints: none\nacceptance: none\nplan: none\n");
    }

    let mut changed = Vec::new();
    let mut last_verification = None;
    if let Ok(records) = crate::agent::journal::Journal::records_for(root, session_id) {
        for record in records {
            if record.kind == "file_diff" {
                if let Some(path) = record.fields.get("path").and_then(|value| value.as_str()) {
                    changed.push(bounded(path, 160));
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
            changed.into_iter().take(24).collect::<Vec<_>>().join(" · ")
        ));
    }
    out.push_str(&format!(
        "last verification: {}\n",
        last_verification
            .map(|seq| format!("successful exec j#{seq}"))
            .unwrap_or_else(|| "none".to_string())
    ));
    out
}

/// Return the host-owned instruction used when a session resumes mid-step.
/// A plan `start` without a later `finish`, `block`, or `cancel` is evidence
/// that the process stopped while work was in progress.
pub fn resume_notice(root: &std::path::Path, session_id: &str) -> Option<String> {
    let records = crate::agent::journal::Journal::records_for(root, session_id).ok()?;
    let mut open_step = None;
    for record in records.iter().filter(|record| record.kind == "plan") {
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

/// tool results of the last N messages survive pruning untouched
const PRUNE_KEEP_RECENT: usize = 6;
/// older tool results are cut down to this
const PRUNE_TOOL_CHARS: usize = 2_000;
/// older tool results larger than this are reduced to a short head
const PRUNE_DROP_CHARS: usize = 20_000;
const PRUNE_HEAD_CHARS: usize = 800;
/// messages kept verbatim after a summarization
const SUMMARY_KEEP_RECENT: usize = 6;
/// share of the context kept free for the answer
const RESERVE_RATIO: f64 = 0.2;
const RESERVE_MIN: u64 = 8_000;
const RESERVE_MAX: u64 = 32_000;

pub const PRUNE_NOTE: &str =
    "…(old tool output compacted; rerun the tool to see the full result again)";

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
    pub stage_ratio: Option<f64>,
    pub summary_enabled: bool,
}

impl Policy {
    #[allow(dead_code)]
    pub fn new(context_limit: u64) -> Self {
        Self {
            context_limit,
            anchor_ratio: None,
            keep_turns: None,
            stage_ratio: None,
            summary_enabled: false,
        }
    }

    pub fn with_compaction(
        context_limit: u64,
        anchor_ratio: f64,
        keep_turns: usize,
        stage_ratio: f64,
        summary_enabled: bool,
    ) -> Self {
        Self {
            context_limit,
            anchor_ratio: Some(anchor_ratio),
            keep_turns: Some(keep_turns),
            stage_ratio: Some(stage_ratio),
            summary_enabled,
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

    /// how much history may accumulate before compaction is triggered
    pub fn budget(&self) -> u64 {
        if self.context_limit == 0 {
            return u64::MAX;
        }
        self.context_limit.saturating_sub(self.reserve())
    }

    pub fn keep_turns(&self) -> usize {
        self.keep_turns.unwrap_or(SUMMARY_KEEP_RECENT)
    }

    #[allow(dead_code)]
    pub fn stage_ratio(&self) -> f64 {
        self.stage_ratio.unwrap_or(0.60).clamp(0.0, 1.0)
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

/// Stage 1: shrink tool output that has aged out of the working set.
/// Returns the messages plus whether anything actually changed.
pub fn prune(messages: &[Message]) -> (Vec<Message>, bool) {
    let keep_from = messages.len().saturating_sub(PRUNE_KEEP_RECENT);
    let mut out = Vec::with_capacity(messages.len());
    let mut changed = false;
    for (idx, message) in messages.iter().enumerate() {
        let mut copy = message.clone();
        if idx < keep_from && message.role == Role::Tool {
            let chars = message.content.chars().count();
            let replacement = if chars > PRUNE_DROP_CHARS {
                let head: String = message.content.chars().take(PRUNE_HEAD_CHARS).collect();
                Some(format!("{head}\n{PRUNE_NOTE}"))
            } else if chars > PRUNE_TOOL_CHARS {
                let head: String = message.content.chars().take(PRUNE_TOOL_CHARS).collect();
                Some(format!("{head}\n{PRUNE_NOTE}"))
            } else {
                None
            };
            if let Some(next) = replacement {
                // only flag a change when the content truly differs, so a second
                // pass over already-pruned history is a no-op
                if next != message.content {
                    copy.content = next;
                    changed = true;
                }
            }
        }
        out.push(copy);
    }
    (out, changed)
}

/// Stage 2: split the history into (to_summarize, keep_verbatim).
///
/// The cut lands on a user turn and never separates an assistant tool call
/// from its results — providers reject orphaned tool results.
#[allow(dead_code)]
pub fn split_for_summary(messages: &[Message]) -> (&[Message], &[Message]) {
    split_for_summary_with_keep(messages, SUMMARY_KEEP_RECENT)
}

pub fn split_for_summary_with_keep(
    messages: &[Message],
    keep_recent: usize,
) -> (&[Message], &[Message]) {
    if messages.len() <= keep_recent {
        return (&[], messages);
    }
    let mut cut = messages.len() - keep_recent;
    while cut > 0 && !is_safe_cut(messages, cut) {
        cut -= 1;
    }
    if cut == 0 {
        return (&[], messages);
    }
    (&messages[..cut], &messages[cut..])
}

fn is_safe_cut(messages: &[Message], cut: usize) -> bool {
    if cut >= messages.len() || messages[cut].role != Role::User {
        return false;
    }
    // a pending assistant tool call must keep its results
    !matches!(
        messages.get(cut.saturating_sub(1)),
        Some(prev) if prev.role == Role::Assistant && !prev.tool_calls.is_empty()
    )
}

/// Render the part of the conversation that is about to be replaced.
pub fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
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

/// The user turn of the summarization request.
pub fn summary_input(older: &[Message], previous: Option<&str>) -> String {
    let mut out = String::from(
        "Summarize the conversation below so work can continue without it.\n\n\
         Cover, in this order:\n\
         1. Task: what the user asked for.\n\
         2. Done: concrete changes already made (files, commands, results).\n\
         3. Decided: choices and constraints that must survive.\n\
         4. Gotchas: errors, dead ends, things that must not be retried blindly.\n\
         5. Open: what is not finished yet.\n\n\
         Rules: no preamble, no questions, no instructions to the reader, no \
         invented facts. If something is unknown, say it is unknown.",
    );
    if let Some(prev) = previous.filter(|s| !s.trim().is_empty()) {
        out.push_str("\n\n<previous-summary>\n");
        out.push_str(prev.trim());
        out.push_str("\n</previous-summary>");
    }
    out.push_str("\n\n<conversation>\n");
    out.push_str(&transcript(older));
    out.push_str("</conversation>\n");
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
pub fn hard_trim(messages: &[Message], budget: u64) -> Vec<Message> {
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
        // nothing can be cut safely: keep the newest turn alone
        return messages.last().cloned().into_iter().collect();
    }
    messages[start..].to_vec()
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
    if let Some(previous) = existing.filter(|s| !s.trim().is_empty()) {
        lines.push(previous.trim().to_string());
    }
    for message in messages {
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
mod tests {
    use super::*;

    fn user(s: &str) -> Message {
        Message::new(Role::User, s)
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sqwai-context-{label}-{}", std::process::id()))
    }

    #[test]
    fn anchor_preserves_plan_state_and_journal_facts() {
        let root = temp_root("anchor");
        let mut plan = crate::plan::create(
            "ship the anchor".into(),
            vec!["keep the goal host-owned".into()],
            vec!["cmd: cargo test".into()],
            vec![crate::plan::NewStep {
                title: "implement anchor".into(),
                kind: Some(crate::plan::StepKind::Change),
                refs: Vec::new(),
            }],
            20_000,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();
        crate::plan::apply(
            &mut plan,
            crate::plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();

        let mut journal = crate::agent::journal::Journal::open(&root, "anchor-session").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan.id.clone()), "main");
        journal
            .append("file_diff", serde_json::json!({"path": "src/main.rs"}))
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true}),
            )
            .unwrap();

        let rendered = anchor(&root, "anchor-session");
        assert!(rendered.contains("goal: ship the anchor"));
        assert!(rendered.contains("constraints: keep the goal host-owned"));
        assert!(rendered.contains("step 1 in_progress implement anchor"));
        assert!(rendered.contains("files changed this session: src/main.rs"));
        assert!(rendered.contains("last verification: successful exec j#2"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resume_notice_reports_unfinished_plan_step() {
        let root = temp_root("resume");
        let mut plan = crate::plan::create(
            "resume safely".into(),
            Vec::new(),
            Vec::new(),
            vec![crate::plan::NewStep {
                title: "unfinished change".into(),
                kind: Some(crate::plan::StepKind::Change),
                refs: Vec::new(),
            }],
            20_000,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();
        crate::plan::apply(
            &mut plan,
            crate::plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&root, &plan).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&root, "resume-session").unwrap();
        journal.set_attribution(Some("1".into()), Some(plan.id), "main");
        journal
            .append("plan", serde_json::json!({"op": "start", "id": "1"}))
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "read", "ok": true}),
            )
            .unwrap();
        let notice = resume_notice(&root, "resume-session").unwrap();
        assert!(notice.contains("Step 1 was in progress"));
        assert!(notice.contains("j#2 tool_result"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn completed_plan_step_has_no_resume_notice() {
        let root = temp_root("resume-closed");
        let mut journal = crate::agent::journal::Journal::open(&root, "closed-session").unwrap();
        journal
            .append("plan", serde_json::json!({"op": "start", "id": "1"}))
            .unwrap();
        journal
            .append("plan", serde_json::json!({"op": "finish", "id": "1"}))
            .unwrap();
        assert!(resume_notice(&root, "closed-session").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn anchor_degrades_without_plan_or_journal() {
        let root = temp_root("empty");
        let rendered = anchor(&root, "missing-session");
        assert!(rendered.contains("goal: none"));
        assert!(rendered.contains("files changed this session: none"));
    }

    #[test]
    fn keeps_recent_tail_and_summarizes_old_messages() {
        let messages: Vec<_> = (0..12).map(|i| user(&format!("message {i}"))).collect();
        let result = compact(&messages, None);
        assert!(result.compacted);
        assert_eq!(result.messages.len(), RECENT_MESSAGE_COUNT);
        assert!(result.summary.contains("message 0"));
        assert!(!result.summary.contains("message 11"));
    }

    #[test]
    fn truncates_large_tool_results() {
        let messages = vec![Message::tool_result("call", "x".repeat(20_000), false)];
        let result = compact(&messages, None);
        assert!(result.messages[0].content.len() < 13_000);
        assert!(result.messages[0].content.contains("compacted"));
    }

    #[test]
    fn carries_previous_summary_only_once() {
        let messages: Vec<_> = (0..12).map(|i| user(&format!("message {i}"))).collect();
        let result = compact(&messages, Some("old facts"));
        assert_eq!(result.summary.matches("old facts").count(), 1);
    }

    #[test]
    fn reserve_is_clamped_and_budget_follows() {
        // small context: reserve floors at RESERVE_MIN
        let small = Policy::new(4_000);
        assert_eq!(small.reserve(), RESERVE_MIN.min(4_000));
        assert_eq!(small.budget(), 0);

        let huge = Policy::new(1_000_000);
        assert_eq!(huge.reserve(), RESERVE_MAX);
        assert_eq!(huge.budget(), 1_000_000 - RESERVE_MAX);

        let mid = Policy::new(100_000);
        assert_eq!(mid.reserve(), 20_000);
        assert_eq!(mid.pressure(80_000), Pressure::Ok);
        assert_eq!(mid.pressure(80_001), Pressure::Summarize);
        // unknown limit: never compact on a guess
        assert_eq!(Policy::new(0).pressure(10_000_000), Pressure::Ok);
    }

    #[test]
    fn configured_policy_controls_reserve_tail_and_summary() {
        let policy = Policy::with_compaction(100_000, 0.08, 4, 0.60, false);
        assert_eq!(policy.reserve(), 8_000);
        assert_eq!(policy.keep_turns(), 4);
        assert!(!policy.summary_enabled);
        assert!((policy.stage_ratio() - 0.60).abs() < f64::EPSILON);

        let messages: Vec<_> = (0..8).map(|i| user(&format!("message {i}"))).collect();
        let (older, keep) = split_for_summary_with_keep(&messages, policy.keep_turns());
        assert_eq!(older.len(), 4);
        assert_eq!(keep.len(), 4);
    }

    #[test]
    fn summary_off_is_the_default() {
        assert!(!Policy::new(100_000).summary_enabled);
    }

    #[test]
    fn prune_only_touches_aged_out_tool_output() {
        let mut messages: Vec<Message> = Vec::new();
        for i in 0..10 {
            messages.push(user(&format!("turn {i}")));
            messages.push(Message::tool_result(
                &format!("call{i}"),
                "y".repeat(6_000),
                false,
            ));
        }
        let (pruned, changed) = prune(&messages);
        assert!(changed);
        assert_eq!(pruned.len(), messages.len(), "pruning drops no message");
        // the newest tool results keep their full payload
        assert!(
            pruned[pruned.len() - 1]
                .content
                .contains(&"y".repeat(6_000))
        );
        // the oldest ones were cut down
        assert!(pruned[1].content.len() < 6_000);
        assert!(pruned[1].content.contains(PRUNE_NOTE));

        // pruning is idempotent
        let (again, changed_again) = prune(&pruned);
        assert!(!changed_again);
        assert_eq!(again.len(), pruned.len());
    }

    #[test]
    fn split_never_orphans_a_tool_call() {
        // a complete tool call (assistant request + its result) followed by
        // several text turns, so a safe cut can land after the result and
        // still leave the recent tail intact
        let messages = vec![
            user("first task"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq {
                    id: "c1".into(),
                    name: "read".into(),
                    args: serde_json::json!({"file_path": "a.rs"}),
                },
            ]),
            Message::tool_result("c1", "fn main() {}", false),
            user("second task"),
            Message::new(Role::Assistant, "did second"),
            user("third task"),
            Message::new(Role::Assistant, "did third"),
            user("fourth task"),
            Message::new(Role::Assistant, "did fourth"),
            user("fifth task"),
        ];
        let (older, keep) = split_for_summary(&messages);
        assert!(!older.is_empty());
        assert!(keep.first().is_some_and(|m| m.role == Role::User));
        // the split is a clean partition
        assert_eq!(older.len() + keep.len(), messages.len());
        // a tool call that was summarized keeps its result on the same side;
        // otherwise no assistant tool call may sit at the end of the part
        let call_idx = older
            .iter()
            .position(|m| m.role == Role::Assistant && !m.tool_calls.is_empty());
        if let Some(ci) = call_idx {
            assert!(
                older.get(ci + 1).is_some_and(|m| m.role == Role::Tool),
                "a summarized tool call must keep its result"
            );
        } else {
            assert!(!matches!(
                older.last(),
                Some(m) if m.role == Role::Assistant && !m.tool_calls.is_empty()
            ));
        }
    }

    #[test]
    fn split_of_a_short_history_summarizes_nothing() {
        let messages: Vec<_> = (0..4).map(|i| user(&format!("m{i}"))).collect();
        let (older, keep) = split_for_summary(&messages);
        assert!(older.is_empty());
        assert_eq!(keep.len(), messages.len());
    }

    #[test]
    fn summary_round_trip_starts_with_a_user_turn() {
        let keep = vec![user("latest question")];
        let out = apply_summary("did things", &keep);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, Role::User, "assistant-first breaks anthropic");
        assert!(out[0].content.contains("did things"));
        assert_eq!(out[1].content, "latest question");
    }

    #[test]
    fn summary_input_includes_the_previous_summary() {
        let older = vec![user("first"), Message::new(Role::Assistant, "second")];
        let input = summary_input(&older, Some("earlier facts"));
        assert!(input.contains("earlier facts"));
        assert!(input.contains("first"));
        assert!(input.contains("second"));

        let without = summary_input(&older, None);
        assert!(!without.contains("earlier facts"));
    }

    #[test]
    fn hard_trim_fits_the_budget_and_stays_safe() {
        let mut messages: Vec<Message> = Vec::new();
        for _ in 0..40 {
            messages.push(user(&"x".repeat(400))); // ~100 tokens each
        }
        let trimmed = hard_trim(&messages, 1_000); // ~10 messages
        assert!(estimated_tokens(&trimmed) <= 1_000);
        assert!(!trimmed.is_empty());
        assert!(
            trimmed.first().is_some_and(|m| m.role == Role::User),
            "a trimmed history must open on a user turn"
        );
        // the newest turn always survives
        assert_eq!(
            trimmed.last().unwrap().content,
            messages.last().unwrap().content
        );
    }
}
