//! The agent loop (phase 2): drives LLM turns plus tool execution until the
//! model produces a final text answer with no more tool calls.
//!
//! Runs in its own tokio task, publishing [`AgentEvent`]s to the TUI and
//! receiving user interaction answers (ask_user, dangerous-command approval)
//! back through the [`ControlMsg`] channel. Aborting the task stops the agent.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::Digest;
use tokio::sync::mpsc;

use crate::config::EffortLevel;
use crate::providers::{
    ChatRequest, Message, RequestBreakdown, Role, SharedProvider,
    SystemPart, ToolCallReq, Usage,
};

use crate::agent::context;
use crate::agent::tools::{self, ToolCtx};
use crate::agent::checkpoints;
use crate::plan;
use loop_ask::{ask_user, bash_call, is_accepted_memory_answer, propose_plan, propose_reset, run_tool_blocking};
use loop_subagent::{SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step, run_subagent, run_subagent_batch};
use loop_turn::{request_messages, run_turn, turn_transport};
pub(crate) use loop_turn::TurnOutcome;
use loop_compact::{
    CompactionPrefix, compact_history, effort_ignored_reason,
    plan_hint_for_summary, record_compaction, turn_shows_no_reasoning,
};
pub(crate) use loop_compact::{capture_nudge, has_restriction_marker};

mod loop_ask;
mod loop_compact;
mod loop_subagent;
mod loop_turn;

#[derive(Debug, Clone)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    pub recommended: bool,
}

#[derive(Debug, Clone)]
pub struct AskQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<AskOption>,
    pub multiple: bool,
    pub allow_free: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApprovalDecision {
    RunOnce,
    AlwaysSession,
    Deny,
}

/// what a finished agent hands back to the TUI
#[derive(Debug)]
pub struct AgentOutcome {
    /// full history + tool turns + final assistant answer (never a system turn)
    pub messages: Vec<Message>,
    /// compaction summary covering everything dropped from `messages`
    pub summary: Option<String>,
    /// deprecated compatibility field; derived plan data is exposed separately
    pub todos: Vec<String>,
    /// checklist derived from the durable project plan
    pub plan_todos: Vec<String>,
    /// (sha, label) checkpoints created by this run's mutations
    pub journal: Vec<(String, String)>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // the event enum is moved through a channel; boxing the large payloads would force an extra allocation on every event
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Usage(Usage),
    /// The model did not act on the selected effort level, as a fact rather
    /// than a guess: either the provider counted zero reasoning tokens, or it
    /// rejected the parameter outright. The UI must stop implying the work.
    EffortIgnored {
        level: String,
        why: String,
    },
    ResponseId(String),
    RequestBreakdown(RequestBreakdown),
    /// a delegated child agent was created
    SubagentStart {
        id: u64,
        task: String,
    },
    /// a reasoning delta from a delegated child
    SubagentThinking {
        id: u64,
        text: String,
    },
    /// an answer delta from a delegated child
    SubagentText {
        id: u64,
        text: String,
    },
    /// a child tool started
    SubagentToolStart {
        id: u64,
        name: String,
        summary: String,
    },
    /// a child tool finished
    SubagentToolDone {
        id: u64,
        name: String,
        summary: String,
        ok: bool,
        diff: Option<String>,
    },
    /// a delegated child finished
    SubagentDone {
        id: u64,
        ok: bool,
        output: String,
    },
    /// a tool just started: name + short arguments, spinner in the TUI.
    /// `call_id` pins the matching notice to this exact row: same-name
    /// calls in one batch (parallel subagents) must not close each other.
    ToolStart {
        name: String,
        summary: String,
        call_id: String,
    },
    /// a tool finished (ok=True/False); carries the unified diff for mutations
    ToolNotice {
        name: String,
        summary: String,
        ok: bool,
        diff: Option<String>,
        call_id: String,
    },
    /// the model asked the user structured questions; answer via ControlMsg
    AskUser {
        id: u64,
        questions: Vec<AskQuestion>,
    },
    /// the model proposed a full plan draft; accept/decline via ControlMsg.
    /// Nothing is written until the user accepts.
    PlanProposal {
        id: u64,
        draft: plan::Plan,
    },
    /// the host stored an accepted proposal; carries the stored plan's id so
    /// the session re-links before the tool outcome is even processed
    PlanAccepted {
        id: String,
    },
    /// the session's current plan step changed (§2.2.3); `None` means idle.
    /// The TUI persists it on the session; the loop itself tracks it in
    /// `current_step` and never waits for this round-trip.
    StepCurrent {
        step: Option<String>,
    },
    /// a dangerous command needs approval; decide via ControlMsg
    Approval {
        id: u64,
        command: String,
        reason: String,
    },
    /// a shadow git checkpoint was taken before a mutation (design §6, §10)
    Checkpoint {
        label: String,
    },
    /// the compaction policy ran: token counts before/after
    Compaction {
        /// true when the model produced the summary, false for prune/trim only
        summarized: bool,
        before: u64,
        after: u64,
    },
    /// the agent revised the visible to-do list
    Todos(Vec<String>),
    /// latest diagnostics count reported after a file mutation
    Diagnostics {
        count: usize,
    },
    Retry {
        attempt: u32,
        delay_secs: u64,
        error: String,
    },
    /// switched to a fallback model on retry-exhausted network/5xx (§5.1, §7 T)
    FallbackSwitched {
        from: String,
        to: String,
    },
    Completed(Result<AgentOutcome, String>),
}

#[derive(Clone)]
pub struct FallbackCandidate {
    pub key: String,
    pub model_id: String,
    pub provider: SharedProvider,
    pub effort_support: crate::config::EffortSupport,
    pub context_limit: u64,
}

#[derive(Debug)]
#[allow(clippy::enum_variant_names)] // the "Answer" suffix reads better at the call sites than a forced rename
pub enum ControlMsg {
    AskAnswer { id: u64, text: String },
    PlanAnswer { id: u64, accept: bool },
    ApprovalAnswer { id: u64, decision: ApprovalDecision },
}

pub struct AgentHandle {
    pub rx: mpsc::Receiver<AgentEvent>,
    pub control: mpsc::Sender<ControlMsg>,
    abort: tokio::task::AbortHandle,
    /// §3.7 (§7 S): flipped by Esc while a tool is executing. Distinct from
    /// [`Self::abort`] — that tears the whole turn down via `AbortHandle`,
    /// which does not reach a child process spawned on a `spawn_blocking`
    /// thread (tokio cannot cancel a blocking closure), so pressing Esc
    /// during `bash` used to leave the command running, invisibly, until it
    /// finished on its own. This flag lets the tool notice the request at its
    /// own polling point and stop the process itself.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AgentHandle {
    pub fn abort(&self) {
        self.abort.abort();
    }

    /// Ask the tool currently running to stop cooperatively. The call still
    /// gets a normal `tool_result` (`ok:false`, cancelled) recorded in the
    /// journal and the transcript, but `run_agent` then stops requesting
    /// further model turns rather than letting the model react and try
    /// something else — Esc is the user saying "stop", not a hint for the
    /// model to keep going on its own.
    pub fn request_tool_cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a cooperative cancel is already pending. The TUI escalates
    /// a second Esc to a hard abort instead of requesting twice.
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The stop half of this handle for the S1 child registry
    /// (§2.2.4): undo cancels whatever is still registered before it
    /// reverts the tree. Cloned, never moved — the event stream stays here.
    pub fn child_control(&self) -> super::undo_guard::ChildControl {
        super::undo_guard::ChildControl::new(self.abort.clone(), self.cancel.clone())
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub struct AgentInput {
    pub provider: SharedProvider,
    pub model_id: String,
    pub model_key: String,
    pub effort: Option<EffortLevel>,
    /// what the target model does with that level (§5.1); threaded through so
    /// providers never have to guess and the UI never has to re-derive it
    pub effort_support: crate::config::EffortSupport,
    pub max_tokens: Option<u32>,
    /// System block for this request, ordered and split into stable/volatile
    /// parts. It is rebuilt by the caller for every turn and is never stored
    /// in the session transcript.
    pub system: Vec<SystemPart>,
    /// conversation history: user / assistant / tool turns only
    pub messages: Vec<Message>,
    /// project root where tool paths are jailed
    pub root: PathBuf,
    /// session id used for the host-owned journal file
    pub session_id: String,
    /// hard-blocked command patterns from [safety].blocked_patterns
    pub blocked_patterns: Vec<String>,
    /// PLAN mode: read-only tools only, mutations are refused (design §5)
    pub plan_mode: bool,
    /// model context limit used to enforce the plan size budget
    pub context_limit: u64,
    /// Whether this request may use agent tools.
    pub enable_tools: bool,
    /// Whether this project instance may perform mutations or write durable state.
    pub read_only: bool,
    /// Optional provider-native continuation from the previous completed turn.
    /// Only ever set for providers that document the field.
    pub previous_response_id: Option<String>,
    /// Summary of everything already compacted out of `messages`.
    pub summary: Option<String>,
    /// MCP servers available to this agent turn.
    pub mcp: crate::config::McpConfig,
    /// LSP servers available to this agent turn.
    pub lsp: crate::config::LspConfig,
    /// Run the compaction policy and finish without talking to the model
    /// otherwise (the `/compact` command).
    pub compact_only: bool,
    /// diary writer limits copied from configuration
    pub diary: crate::config::DiaryConfig,
    /// memory proposal limits copied from configuration
    pub memory: crate::config::MemoryConfig,
    /// compaction thresholds and summary policy copied from configuration
    pub compaction: crate::config::CompactionConfig,
    /// host limits on the structured plan copied from configuration
    pub plan_limits: crate::config::PlanConfig,
    /// where the shadow repository lives, from [undo].shadow configuration
    pub shadow_store: crate::config::ShadowStore,
    /// nesting guard for delegated subagents; the first generation may create
    /// children, but children cannot recursively create more children.
    pub subagent_depth: u8,
    /// Immutable step context inherited from the spawning session (§2.2.4).
    /// `None` for main agents. The child stamps it on its journal records
    /// and refuses mutations once the step moves to a newer epoch.
    pub parent_step: Option<plan::StepContext>,
    /// Session id of the spawning agent, if any. The child's shadow
    /// snapshots land on this chain so the parent's `/undo` sees them
    /// (§2.2.4); everything else (journal, jobs, read-guard) stays on the
    /// child's own session.
    pub parent_session: Option<String>,
    /// Optional fallback models to switch to if primary model fails with retry-exhausted network/5xx (§5.1, §7 T).
    pub fallback_chain: Vec<FallbackCandidate>,
}



pub fn spawn_agent(input: AgentInput) -> AgentHandle {
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(32);
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let abort = tokio::spawn(run_agent(input, tx, ctl_rx, cancel.clone())).abort_handle();
    AgentHandle {
        rx,
        control: ctl_tx,
        abort,
        cancel,
    }
}

/// Heuristic check for trivial user prompts (§2.1.9, §7 W).
///
/// In ACT mode, unprompted file mutations without an active plan require
/// a plan first (`plan_required`) unless the prompt is trivial:
/// e.g. single-file typo fixes, simple renames, comments, whitespace,
/// or very short, non-complex requests affecting <= 1 file.
/// Gate input: an active plan only counts when it carries acceptance.
/// A plan without criteria settles nothing, so mutating under one is the
/// same as mutating without a plan. #171 still applies — one active plan
/// per project (§2.1.1), a global check; session-scoped resolution stays
/// strict everywhere else.
fn plan_with_acceptance(root: &Path) -> bool {
    crate::plan::open_active(root)
        .ok()
        .flatten()
        .is_some_and(|plan| !plan.acceptance.is_empty())
}

/// Refusal body for the plan-first gate. The gate asks "is there a
/// criterion", not "is there a plan": when one already exists but settles
/// nothing, telling the model to create another sends it into a
/// plan_exists refusal followed by a plan-show reassurance loop (seen
/// live). Name the real next step instead.
fn plan_required_refusal(root: &Path) -> serde_json::Value {
    let bare_plan = crate::plan::open_active(root)
        .ok()
        .flatten()
        .filter(|plan| plan.acceptance.is_empty())
        .map(|plan| plan.id);
    let (reason, hint) = match bare_plan {
        Some(id) => (
            format!(
                "In ACT mode, mutating tools require acceptance criteria first. Plan {id} is active but settles nothing yet — add executable (cmd:) or human (manual:) criteria with 'plan add_acceptance' (free-text notes go to checklist) before modifying project files."
            ),
            "Call 'plan add_acceptance' with items for the active plan; do not create another one.".to_string(),
        ),
        None => (
            "In ACT mode, mutating tools require an active plan with acceptance criteria first. Create a plan with 'plan create' (acceptance: cmd: for executable checks, manual: for human checks, free-text notes go to checklist) before modifying project files.".to_string(),
            "Call 'plan create' with your goal, acceptance criteria, and initial steps.".to_string(),
        ),
    };
    serde_json::json!({
        "ok": false,
        "code": "plan_required",
        "reason": reason,
        "hint": hint,
    })
}

/// Advisory repeat note for a bash outcome: when the same command already
/// ran earlier in this session with byte-identical output, re-running
/// learned nothing — say so once, attached to this result, instead of
/// burning another turn on the same bytes. Advisory only: polled state
/// legitimately changes, and the note says to ignore it then. Compares full
/// outputs, not journal summaries (truncated to 200 chars), so same-headed
/// but different-tailed outputs never match.
fn repeat_bash_note(messages: &[Message], call: &ToolCallReq, output: &str) -> Option<String> {
    let command = call.args.get("command")?.as_str()?;
    if command.trim().is_empty() {
        return None;
    }
    // (command, output) pairs in order; the current call has no result yet,
    // so everything collected here is older
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for tc in &message.tool_calls {
            if tc.name != "bash" {
                continue;
            }
            let Some(cmd) = tc.args.get("command").and_then(|v| v.as_str()) else {
                continue;
            };
            let out = messages
                .iter()
                .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(&tc.id))
                .map(|m| m.content.as_str())
                .unwrap_or("");
            pairs.push((cmd, out));
        }
    }
    if pairs
        .iter()
        .any(|(cmd, out)| *cmd == command && *out == output)
    {
        Some(
            "\n[host: you already ran this exact command earlier in this session with byte-identical output — reuse that observation instead of re-running it. If the underlying state may have changed since, ignore this note.]"
                .to_string(),
        )
    } else {
        None
    }
}


/// Sessions this process already opened an agent run for. `run_agent` is
/// spawned per turn, but `session_start` plus the resume record describe a
/// run's beginning: writing them every turn journaled a phantom "Session
/// resumed" while any step was merely open — and the model, reading it in
/// the tail and via `journal`, concluded context is restored constantly and
/// re-verified the plan before each step. A fresh process (restart/crash)
/// starts empty, so genuine restores still record.
static AGENT_RUNS_STARTED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

fn mark_agent_run_started(session_id: &str) -> bool {
    AGENT_RUNS_STARTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.to_string())
}

async fn run_agent(
    input: AgentInput,
    tx: mpsc::Sender<AgentEvent>,
    mut ctl: mpsc::Receiver<ControlMsg>,
    cancel_tool: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let AgentInput {
        mut provider,
        mut model_id,
        model_key,
        mut effort,
        mut effort_support,
        max_tokens,
        system,
        mut messages,
        root,
        session_id,
        blocked_patterns,
        plan_mode,
        mut context_limit,
        enable_tools,
        read_only,
        mut previous_response_id,
        mut summary,
        compact_only,
        diary,
        memory,
        compaction,
        plan_limits,
        mcp,
        lsp,
        shadow_store,
        subagent_depth,
        parent_step,
        parent_session,
        mut fallback_chain,
    } = input;
    let mut current_model_key = model_key;

    for pat in &blocked_patterns {
        if let Err(e) = regex::Regex::new(pat) {
            let _ = tx
                .send(AgentEvent::Completed(Err(format!(
                    "invalid [safety].blocked_patterns regex '{pat}': {e}"
                ))))
                .await;
            return;
        }
    }

    let mut lsp_manager = if enable_tools && !lsp.servers.is_empty() {
        match crate::lsp::Manager::start(&lsp, &root).await {
            Ok(manager) => Some(manager),
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Completed(Err(format!(
                        "LSP startup failed: {e:#}"
                    ))))
                    .await;
                return;
            }
        }
    } else {
        None
    };

    let mcp_registry = if enable_tools {
        match crate::mcp::Registry::from_config(&mcp).await {
            Ok(registry) => Some(registry),
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Completed(Err(format!(
                        "MCP startup failed: {e:#}"
                    ))))
                    .await;
                return;
            }
        }
    } else {
        None
    };

    let mut caps = provider.capabilities();
    let mut policy = context::Policy::with_compaction(
        context_limit,
        compaction.anchor_ratio,
        compaction.keep_turns,
        compaction.threshold,
        // G0 baseline (§8.2) always summarizes instead of anchoring
        crate::bench::baseline()
            || crate::bench::summary_short()
            || matches!(compaction.summary, crate::config::CompactionSummary::Short),
    );
    // Tools are part of the request prefix: sorted for stability, narrowed in
    // PLAN mode, and omitted entirely for requests that cannot call them.
    let tools: Vec<crate::providers::ToolSpec> = if enable_tools {
        let base = tools::tool_specs(plan_mode);
        if let Some(registry) = &mcp_registry {
            tools::merge_specs(base, registry.specs())
        } else {
            base
        }
    } else {
        Vec::new()
    };
    // A continuation reference is only usable while the provider is known to
    // honour it; the first request that proves otherwise turns it off for the
    // rest of the session.
    let mut continuation_usable = true;

    // `/compact` — write the mandatory pre-compaction diary entry first, then
    // run the policy and hand the transcript back without a chat turn.
    // No diary on the G0 baseline (§8.2).
    if compact_only {
        if !crate::bench::baseline() {
            let _ = crate::agent::diary::write_entry(
                &root,
                crate::agent::diary::today(),
                &session_id,
                "compaction",
                Some(&provider),
                &model_id,
                plan::open_active_for_session(&root, Some(&session_id))
                    .ok()
                    .flatten()
                    .map(|plan| plan::render(&plan))
                    .as_deref(),
                None,
                messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(|message| message.content.as_str()),
                Some(diary.token_budget),
                diary.effort,
                Some(Duration::from_secs(diary.timeout_secs)),
            )
            .await;
        }
        let mut compaction_journal = if !read_only {
            crate::agent::journal::Journal::open(&root, &session_id).ok()
        } else {
            None
        };
        if let Some(writer) = compaction_journal.as_mut() {
            writer.set_attribution(
                None,
                plan::open_active_for_session(&root, Some(&session_id))
                    .ok()
                    .flatten()
                    .map(|plan| plan.id),
                "main",
            );
            let _ = writer.append("compaction", serde_json::json!({"phase": "begin"}));
        }
        let message_count_before = messages.len();
        let plan_hint = plan_hint_for_summary(&root, &session_id);
        let prefix = CompactionPrefix {
            system: &system,
            tools: &tools,
        };
        let outcome = compact_history(
            &provider,
            &model_id,
            &mut messages,
            &mut summary,
            &policy,
            true,
            &plan_hint,
            Some(&prefix),
        )
        .await;
        if let Some((before, after, summarized)) = outcome.as_ref()
            && let Some(writer) = compaction_journal.as_mut()
        {
            let _ = writer.append(
                "compaction",
                serde_json::json!({
                    "phase": "end",
                    "before": before,
                    "after": after,
                    "dropped_msgs": message_count_before.saturating_sub(messages.len()),
                    "kept_msgs": messages.len(),
                    "anchor_tokens": context::anchor(&root, &session_id).len().div_ceil(4),
                    "diary_written": true,
                    "summarized": summarized,
                    "summary": summary.as_deref().unwrap_or(""),
                }),
            );
        }
        if let Some((before, after, summarized)) = outcome {
            let _ = tx
                .send(AgentEvent::Compaction {
                    summarized,
                    before,
                    after,
                })
                .await;
        } else {
            let _ = tx
                .send(AgentEvent::Compaction {
                    summarized: false,
                    before: context::estimated_tokens(&messages),
                    after: context::estimated_tokens(&messages),
                })
                .await;
        }
        let _ = tx
            .send(AgentEvent::Completed(Ok(AgentOutcome {
                messages,
                summary,
                todos: Vec::new(),
                plan_todos: Vec::new(),
                journal: Vec::new(),
            })))
            .await;
        return;
    }

    let mut ctx = ToolCtx::with_read_only(&root, read_only)
        .in_session(&session_id)
        .with_shadow_store(shadow_store)
        .with_plan_limits(plan_limits, context_limit)
        .with_blocked_patterns(blocked_patterns.clone())
        .with_cancel(cancel_tool);
    // @-mention bytes already in the opening message count as read, so an
    // edit afterwards needs no redundant read (and goes stale the same
    // way when the file moves underneath).
    for p in tools::take_mention_prereads(&session_id) {
        ctx.note_read(&p);
    }
    // A child's shadow snapshots belong on the parent chain (§2.2.4), so the
    // parent's `/undo` sees step boundaries and bash mutations; the journal,
    // job isolation and read-guard stay on the child's own session.
    ctx.checkpoint_session = parent_session.clone();
    // Subagents inherit their spawn context (§2.2.4): it stamps their
    // journal records and gates their mutations against reopen races.
    ctx.subagent_step = parent_step.clone();
    // ...and a writer child's declared scope, if any (read-only children
    // and the main agent take nothing).
    ctx.subagent_write_paths = tools::take_subagent_scope(&session_id);
    // ...and their shadow snapshots land on the parent chain, so the
    // parent's `/undo` sees them (ToolCtx::checkpoint_session).
    ctx.checkpoint_session = parent_session.clone();
    // The session's current step (§2.2.3). Adopt whatever the plan holds in
    // progress — after a crash that is the interrupted step (§3.4) — and
    // keep it in lockstep with plan outcomes below. `ctx.current_step` is
    // the single live copy; the TUI persists it via `StepCurrent` events.
    ctx.current_step = adopt_in_progress_step(&root, &session_id);
    if enable_tools && !read_only {
        // Heal a crash between a journal intent and its plan store (§2.1.4,
        // §3.7) before anything — including this session's writer — reads the
        // plan or the tail counter. No-op on a clean tree.
        let _ = plan::replay(&root);
    }
    let mut journal = if enable_tools && !read_only {
        crate::agent::journal::Journal::open(&root, &session_id).ok()
    } else {
        None
    };
    if let Some(writer) = journal.as_mut() {
        if let Some(inherited) = parent_step.as_ref() {
            // Stamp the inherited epoch on every record this writer produces
            // so post-reopen validation can tell stale work apart (§2.2.4).
            writer.set_epoch(Some(inherited.step_epoch));
        }
        let plan_id = plan::open_active_for_session(&root, Some(&session_id))
            .ok()
            .flatten()
            .map(|p| p.id);
        writer.set_attribution(None, plan_id, "main");
        // once per process+session: run_agent is spawned per turn, so an
        // unguarded write here journaled session_start + resume every turn
        if mark_agent_run_started(&session_id) {
            let resumed_from = context::resume_notice(&root, &session_id).map(|_| "journal");
            let _ = writer.session_start(
                &model_id,
                if plan_mode { "plan" } else { "act" },
                None,
                "unknown",
                resumed_from,
            );
            if let Some(notice) = context::resume_notice(&root, &session_id) {
                let _ = writer.append("resume", serde_json::json!({"notice": notice}));
            }
        }
        if let Some(user_message) = messages.iter().rev().find(|m| m.role == Role::User) {
            let _ = writer.append("user_msg", serde_json::json!({
                "hash": format!("{:x}", sha2::Sha256::digest(user_message.content.as_bytes())),
                "chars": user_message.content.chars().count(),
                "goal_like": user_message.content.starts_with("goal:") || user_message.content.starts_with("/goal"),
            }));
        }
    }
    let todos: Vec<String> = Vec::new();
    let mut plan_todos: Vec<String> = plan::open_active_for_session(&root, Some(&session_id))
        .ok()
        .flatten()
        .map(|active| {
            active
                .steps
                .iter()
                .map(|step| format!("[{}] {}", step.status.as_str(), step.title))
                .collect()
        })
        .unwrap_or_default();
    let mut always_allow: Vec<String> = Vec::new();
    let mut memory_proposals_this_turn: u8;
    let mut next_id: u64 = 0;
    // prompt size of the last request, as reported by the provider
    let mut prompt_size: u64 = 0;
    // One forced compaction per oversized request, reset after every turn that
    // gets through: without the guard a request that stays too large would
    // compact in a loop.
    let mut compacted_for_overflow = false;
    // one record and one status line per session, not per turn
    let mut effort_ignored_reported = false;
    // consecutive turns that asked for effort and came back with no reasoning
    let mut zero_reasoning_turns: u32 = 0;

    loop {
        // The proposal limit applies to one model request/turn, not the whole
        // session. A new request gets a fresh allowance.
        memory_proposals_this_turn = 0;
        // The diary is written before the compaction policy can discard any
        // transcript context. The writer has a hard timeout and host fallback.
        // Skipped on the G0 baseline (§8.2): no durable memory there.
        if !crate::bench::baseline()
            && messages.len() > 8
            && policy.pressure(prompt_size.max(context::estimated_tokens(&messages)))
                != context::Pressure::Ok
        {
            let _ = crate::agent::diary::write_entry(
                &root,
                crate::agent::diary::today(),
                &session_id,
                "compaction",
                Some(&provider),
                &model_id,
                plan::open_active_for_session(&root, Some(&session_id))
                    .ok()
                    .flatten()
                    .map(|plan| plan::render(&plan))
                    .as_deref(),
                None,
                messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(|message| message.content.as_str()),
                Some(diary.token_budget),
                diary.effort,
                Some(Duration::from_secs(diary.timeout_secs)),
            )
            .await;
        }
        // Compaction gate. History only: the provider's full request size
        // is observed for the diary trigger below, but the gate must use
        // what compaction can actually remove — otherwise a big fixed
        // prefix fires futile compactions every turn.
        if std::env::var("SQWAI_BENCH_DEBUG").is_ok() {
            eprintln!("[compact] calling compact_history, msgs={}", messages.len());
        }
        let msgs_before = messages.len();
        let plan_hint = plan_hint_for_summary(&root, &session_id);
        let prefix = CompactionPrefix {
            system: &system,
            tools: &tools,
        };
        if let Some((before, after, summarized)) = compact_history(
            &provider,
            &model_id,
            &mut messages,
            &mut summary,
            &policy,
            false,
            &plan_hint,
            Some(&prefix),
        )
        .await
        {
            record_compaction(
                &mut journal,
                before,
                after,
                msgs_before,
                messages.len(),
                summarized,
                summary.as_deref().unwrap_or(""),
            );
            // Same rule as the overflow retry below: the transcript the host
            // owns has changed, so the provider's copy of it is no longer the
            // context this turn is about (§3.3).
            previous_response_id = None;
            let _ = tx
                .send(AgentEvent::Compaction {
                    summarized,
                    before,
                    after,
                })
                .await;
        }

        let mut turn_system = system.clone();
        if let Ok(Some(nudge)) =
            crate::agent::journal::Journal::nudge(&root, Some(&session_id), plan_limits.nudge_after)
        {
            turn_system.push(crate::providers::SystemPart::volatile(nudge));
        }
        // claim-lint repetition removed with Y: no nag block rides here
        // Decided per request, not once per turn: a request that carries tool
        // results must never rely on the provider holding the calls they
        // answer. Field failure on an OpenAI-compatible relay that accepts
        // `previous_response_id` and does not chain by it:
        // `400 No tool call found for function call output with call_id ...`
        // — the outputs travelled, the calls stayed behind.
        let transport = turn_transport(
            caps,
            previous_response_id.as_deref(),
            &messages,
            continuation_usable,
        );
        let request_messages = request_messages(&messages, transport);
        let breakdown = RequestBreakdown::from_request(&ChatRequest {
            model_id: model_id.clone(),
            system: turn_system.clone(),
            messages: request_messages.clone(),
            effort,
            effort_support,
            max_tokens,
            tools: tools.clone(),
            previous_response_id: previous_response_id.clone(),
            context_transport: transport,
        });
        let _ = tx.send(AgentEvent::RequestBreakdown(breakdown)).await;

        // one complete streaming turn
        let turn = match run_turn(
            &provider,
            &ChatRequest {
                model_id: model_id.clone(),
                system: turn_system,
                messages: request_messages,
                effort,
                effort_support,
                max_tokens,
                tools: tools.clone(),
                previous_response_id: previous_response_id.clone(),
                context_transport: transport,
            },
            &tx,
            &mut ctl,
            &mut previous_response_id,
            &mut prompt_size,
            !fallback_chain.is_empty(),
        )
        .await
        {
            Ok(turn) => {
                // §5.1 asks the UI to say "(ignored by model)" when a level
                // does not land. Everything the config can tell us is a claim;
                // these two are evidence, so they are recorded like any other
                // fact the host observed (§2.2.2) and reported once.
                if turn_shows_no_reasoning(&turn) {
                    zero_reasoning_turns += 1;
                } else {
                    zero_reasoning_turns = 0;
                }
                if let Some(level) = effort
                    && !effort_ignored_reported
                    && let Some(why) = effort_ignored_reason(&turn, zero_reasoning_turns)
                {
                    effort_ignored_reported = true;
                    if let Some(writer) = journal.as_mut() {
                        let _ = writer.append(
                            "effort_ignored",
                            serde_json::json!({
                                "level": level.as_str(),
                                "model": model_id,
                                "source": if turn.effort_rejected {
                                    "rejected"
                                } else {
                                    "observed"
                                },
                                "reasoning_tokens": turn.reasoning_tokens,
                                "zero_reasoning_turns": zero_reasoning_turns,
                                "by": "host",
                            }),
                        );
                    }
                    let _ = tx
                        .send(AgentEvent::EffortIgnored {
                            level: level.as_str().to_string(),
                            why: why.clone(),
                        })
                        .await;
                    // A rejected parameter must not be sent again: the next
                    // turn would spend a failed request to learn the same
                    // thing.
                    if turn.effort_rejected {
                        effort = None;
                    }
                }
                if turn.retries > 0
                    && let Some(writer) = journal.as_mut()
                {
                    // The turn only succeeded because the host waited and
                    // asked again; "what happened?" should be able to say so.
                    let _ = writer.append(
                        "provider_error",
                        serde_json::json!({
                            "class": "retried",
                            "retries": turn.retries,
                            "recovered": true,
                        }),
                    );
                }
                turn
            }
            Err(failure) => {
                use crate::providers::ErrorClass;
                if let Some(writer) = journal.as_mut() {
                    let _ = writer.append(
                        "provider_error",
                        serde_json::json!({
                            "class": failure.class.map(|c| c.as_str()).unwrap_or("unclassified"),
                            "retries": failure.retries,
                            "recovered": false,
                        }),
                    );
                }
                // The reference was refused: the provider is not keeping the
                // chain it advertised. Send the transcript we own instead, and
                // stop using the reference for this session.
                if failure.continuation_rejected && continuation_usable {
                    continuation_usable = false;
                    previous_response_id = None;
                    if let Some(writer) = journal.as_mut() {
                        let _ = writer.append(
                            "provider_error",
                            serde_json::json!({
                                "class": "continuation_refused",
                                "retries": failure.retries,
                                "recovered": true,
                                "by": "host",
                            }),
                        );
                    }
                    continue;
                }

                // A request that does not fit gets one compaction and one more
                // try, which is the only thing that can make it fit (§5.1).
                // Asking the same oversized request again cannot.
                if failure.class == Some(ErrorClass::ContextOverflow) && !compacted_for_overflow {
                    compacted_for_overflow = true;
                    let overflow_msgs_before = messages.len();
                    let overflow_plan_hint = plan_hint_for_summary(&root, &session_id);
                    let prefix = CompactionPrefix {
                        system: &system,
                        tools: &tools,
                    };
                    if let Some((before, after, summarized)) = compact_history(
                        &provider,
                        &model_id,
                        &mut messages,
                        &mut summary,
                        &policy,
                        true,
                        &overflow_plan_hint,
                        Some(&prefix),
                    )
                    .await
                    {
                        record_compaction(
                            &mut journal,
                            before,
                            after,
                            overflow_msgs_before,
                            messages.len(),
                            summarized,
                            summary.as_deref().unwrap_or(""),
                        );
                        // The transcript this turn resends is not the one the
                        // provider holds any more, so the reference cannot be
                        // reused for the retry either (§3.3).
                        previous_response_id = None;
                        let _ = tx
                            .send(AgentEvent::Compaction {
                                summarized,
                                before,
                                after,
                            })
                            .await;
                        continue;
                    }
                    // Nothing left to compact: the request is oversized on its
                    // own, so say that rather than looping.
                    let _ = tx
                        .send(AgentEvent::Completed(Err(format!(
                            "{} — nothing left to compact; the request is too large on its own",
                            failure.message
                        ))))
                        .await;
                    break;
                }
                // Stage T: Provider Fallback Chain on retry-exhausted network / 5xx error (§5.1, §7 T)
                let can_fallback = matches!(
                    failure.class,
                    Some(ErrorClass::Network | ErrorClass::Server)
                ) || failure.retries > 0;
                if can_fallback && !fallback_chain.is_empty() {
                    let next = fallback_chain.remove(0);
                    if let Some(writer) = journal.as_mut() {
                        let _ = writer.append(
                            "provider_error",
                            serde_json::json!({
                                "class": failure.class.map(|c| c.as_str()).unwrap_or("unclassified"),
                                "retries": failure.retries,
                                "recovered": true,
                                "switched_to": next.key,
                                "by": "host",
                            }),
                        );
                    }
                    let _ = tx
                        .send(AgentEvent::FallbackSwitched {
                            from: current_model_key.clone(),
                            to: next.key.clone(),
                        })
                        .await;
                    current_model_key = next.key;
                    model_id = next.model_id;
                    provider = next.provider;
                    caps = provider.capabilities();
                    effort_support = next.effort_support;
                    context_limit = next.context_limit;
                    ctx.context_limit = next.context_limit;
                    policy = context::Policy::with_compaction(
                        context_limit,
                        compaction.anchor_ratio,
                        compaction.keep_turns,
                        compaction.threshold,
                        crate::bench::baseline()
                            || crate::bench::summary_short()
                            || matches!(
                                compaction.summary,
                                crate::config::CompactionSummary::Short
                            ),
                    );
                    previous_response_id = None;
                    continue;
                }

                let _ = tx.send(AgentEvent::Completed(Err(failure.message))).await;
                break;
            }
        };
        compacted_for_overflow = false;

        if turn.calls.is_empty() {
            // final answer
            let text = turn.text.clone();
            messages
                .push(Message::new(Role::Assistant, text).with_provider_state(turn.provider_state));
            break;
        }

        // assistant requested tools; record the call(s)
        messages.push(
            Message::new(Role::Assistant, turn.text)
                .with_tool_calls(turn.calls.clone())
                .with_provider_state(turn.provider_state),
        );

        // §3.7 / §7 S: once the user cancels one call, no further calls in
        // this batch run and no further model turns are requested — Esc means
        // stop, not "let the model decide what to do about it".
        let mut interrupted = false;

        // A turn that only delegates to subagents fans the calls out
        // concurrently (each awaits its children inside run_subagent);
        // every other batch keeps the sequential loop below. Pre/post
        // bookkeeping is identical in both shapes: rows, journal and
        // messages stay in call order, only the waits overlap.
        let pure_subagents = subagent_depth == 0
            && !turn.calls.is_empty()
            && turn.calls.iter().all(|call| call.name == "subagent");
        if pure_subagents {
            interrupted = run_subagent_batch(
                &turn.calls,
                &session_id,
                &root,
                &ctx.cancel,
                &mut journal,
                &tx,
                ctx.current_step.clone(),
                &provider,
                &model_id,
                &blocked_patterns,
                plan_mode,
                context_limit,
                effort,
                effort_support,
                max_tokens,
                &system,
                &mcp,
                &lsp,
                read_only,
                shadow_store,
                &mut messages,
                diary.clone(),
                memory.clone(),
                compaction.clone(),
                plan_limits,
                fallback_chain.clone(),
                std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
            )
            .await;
        } else {
            for (call_index, call) in turn.calls.iter().enumerate() {
                // A cancellation from the previous call must not leak into this
                // one: the flag is per-request, reset right before dispatch —
                // but the reset must not swallow an Esc that landed between
                // calls (#192): swap reports whether it was set, and if so this
                // call is recorded as cancelled without running. Downstream
                // (ToolNotice, journal, the interrupted path) treats it exactly
                // like a mid-tool cancel.
                let pre_cancelled = ctx.cancel.swap(false, std::sync::atomic::Ordering::Relaxed);
                let journal_mark = ctx.journal.len();
                let tool_started = Instant::now();
                if let Some(writer) = journal.as_mut() {
                    let active = plan::open_active_for_session(&root, Some(&session_id))
                        .ok()
                        .flatten();
                    let plan_id = active.as_ref().map(|p| p.id.clone());
                    // Explicit session step first (§2.2.3); the plan scan is only
                    // a fallback for records predating current-step tracking.
                    let step = if call.name == "plan" {
                        None
                    } else {
                        ctx.current_step.clone().or_else(|| {
                            active.as_ref().and_then(|p| {
                                p.steps
                                    .iter()
                                    .find(|s| s.status == plan::StepStatus::InProgress)
                                    .map(|s| s.id.clone())
                            })
                        })
                    };
                    writer.set_attribution(step, plan_id, "main");
                    let _ = writer.append(
                        "tool_call",
                        serde_json::json!({
                            "tool": call.name,
                            "call_id": call.id,
                            "args_digest": tools::call_summary(&call.name, &call.args),
                            "path": tools::call_path(&call.name, &call.args),
                        }),
                    );
                }
                // live row first: the TUI shows the tool name and its arguments
                // with a spinner while it runs (design §10)
                let _ = tx
                    .send(AgentEvent::ToolStart {
                        name: call.name.clone(),
                        summary: tools::call_summary(&call.name, &call.args),
                        call_id: call.id.clone(),
                    })
                    .await;

                // Soft plan discipline (§2.1.9): a single-file mutation
                // without an acceptance-bearing plan proceeds — the advisory
                // nudge attaches to a successful outcome below. Multi-file
                // and opaque mutations take the hard refusal above instead.
                let plan_nudge = !plan_mode
                    && subagent_depth == 0
                    && !crate::bench::baseline()
                    && plan_limits.plan_first == crate::config::PlanFirstMode::Soft
                    && tools::is_mutating_call(&call.name, &call.args)
                    && call.name != "plan"
                    && !tools::is_readonly_bash(&call.name, &call.args)
                    && !tools::is_multi_file_mutation(&call.name, &call.args)
                    && !plan_with_acceptance(&root);
                let mut outcome = if pre_cancelled {
                    tools::Outcome::cancelled()
                } else if read_only && tools::is_mutating_call(&call.name, &call.args) {
                    tools::Outcome::err(
                        "project is read-only because another sqwai instance owns the lock; use --force to enable writes",
                    )
                } else if plan_mode
                    && tools::is_mutating_call(&call.name, &call.args)
                    && call.name != "plan"
                {
                    tools::Outcome::err(format!(
                        "PLAN mode is read-only: '{}' is not allowed. Explore first, then ask the \
                     user to switch to ACT (Tab) before changing anything.",
                        call.name
                    ))
                } else if !plan_mode
                && subagent_depth == 0
                && !crate::bench::baseline()
                && plan_limits.plan_first == crate::config::PlanFirstMode::Soft
                && tools::is_mutating_call(&call.name, &call.args)
                && call.name != "plan"
                // read-only inspection needs no plan: Get-Process/netstat
                // style diagnostics run free (advisory classification —
                // approvals still guard real damage, see is_readonly_bash)
                && !tools::is_readonly_bash(&call.name, &call.args)
                // Hard path: multi-file or opaque-target mutations are still
                // refused without an acceptance-bearing plan. Single-file
                // writes fall through to dispatch with an advisory nudge
                // attached below (soft discipline, §2.1.9).
                && tools::is_multi_file_mutation(&call.name, &call.args)
                // The gate asks "is there a criterion", not "is there a
                // plan" and not "is the prose trivial": before the first
                // mutation an acceptance item must exist — executable or
                // human — so there is something to settle against. One
                // active plan per project (§2.1.1).
                && !plan_with_acceptance(&root)
                {
                    tools::Outcome::err(plan_required_refusal(&root).to_string())
                } else {
                    match call.name.as_str() {
                        "ask_user" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot interact with the user; make decisions autonomously",
                        ),
                        "ask_user" => ask_user(call, &tx, &mut ctl, &mut next_id).await,
                        "propose_plan" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot propose plans; plans belong to the primary session",
                        ),
                        "propose_plan" => {
                            propose_plan(
                                call,
                                &mut ctx,
                                &plan_limits,
                                context_limit,
                                read_only,
                                &mut journal,
                                &tx,
                                &mut ctl,
                                &mut next_id,
                                &session_id,
                            )
                            .await
                        }
                        "propose_reset" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot reset plans; plans belong to the primary session",
                        ),
                        "propose_reset" => {
                            propose_reset(
                                call,
                                &mut ctx,
                                read_only,
                                &tx,
                                &mut ctl,
                                &mut next_id,
                            )
                            .await
                        }
                        "bash" => {
                            bash_call(
                                call,
                                &mut ctx,
                                &tx,
                                &mut ctl,
                                &mut always_allow,
                                &blocked_patterns,
                                &mut next_id,
                                subagent_depth,
                            )
                            .await
                        }
                        "webfetch" | "websearch" => {
                            let mut outcome = if call.name == "webfetch" {
                                tools::web::fetch(&call.args).await
                            } else {
                                tools::web::search(&call.args).await
                            };
                            // R banner (§2.2): external bytes travel delimited
                            // so the boundary survives into context. Failures
                            // stay bare — a host error is not untrusted content.
                            if outcome.ok {
                                outcome.output = crate::agent::trust::banner_wrap(&outcome.output);
                            }
                            outcome
                        }
                        "subagent" if subagent_depth == 0 => {
                            let outcome = run_subagent(
                                call,
                                &ctx.session_id,
                                &tx,
                                &ctx.cancel,
                                &provider,
                                &model_id,
                                &root,
                                &blocked_patterns,
                                plan_mode,
                                context_limit,
                                effort,
                                effort_support,
                                max_tokens,
                                system.clone(),
                                mcp.clone(),
                                lsp.clone(),
                                read_only,
                                shadow_store,
                                diary.clone(),
                                memory.clone(),
                                compaction.clone(),
                                plan_limits,
                                fallback_chain.clone(),
                                std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
                            )
                            .await;
                            // The child shares this plan: a `plan finish`
                            // (or `start`) inside it retires (or moves) the
                            // step this session holds, and the next `plan
                            // start` in the same turn would be refused
                            // against the stale hold (§2.2.3). Re-adopt.
                            let held_before = ctx.current_step.clone();
                            ctx.current_step = adopt_in_progress_step(&root, &ctx.session_id);
                            if ctx.current_step != held_before {
                                let _ = tx
                                    .send(AgentEvent::StepCurrent {
                                        step: ctx.current_step.clone(),
                                    })
                                    .await;
                            }
                            outcome
                        }
                        "subagent" => tools::Outcome::err("nested subagents are not allowed"),
                        "memory_propose" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot propose durable memories; memories belong to the primary session",
                        ),
                        "memory_propose" => {
                            memory_proposals_this_turn =
                                memory_proposals_this_turn.saturating_add(1);
                            if memory_proposals_this_turn > memory.max_proposals_per_turn {
                                tools::Outcome::err("memory proposal limit reached for this turn")
                            } else {
                                let proposal =
                                    tools::execute(&mut ctx, "memory_propose", &call.args);
                                if !proposal.ok {
                                    proposal
                                } else {
                                    let prompt = format!(
                                        "Approve this durable memory proposal?\n{}\nChoose: accept, edit, or reject.",
                                        proposal.output
                                    );
                                    let question = ToolCallReq::new(
                                        call.id.clone(),
                                        "ask_user",
                                        serde_json::json!({
                                            "question": prompt,
                                            "options": [
                                                {"label": "accept", "description": "write the proposal"},
                                                {"label": "edit", "description": "provide replacement text"},
                                                {"label": "reject", "description": "do not write it"}
                                            ],
                                            "multiple": false,
                                            "allow_free": true
                                        }),
                                    );
                                    let answer =
                                        ask_user(&question, &tx, &mut ctl, &mut next_id).await;
                                    let raw_answer = answer.output.trim();
                                    let answer_lower = raw_answer.to_ascii_lowercase();
                                    if is_accepted_memory_answer(answer.ok, raw_answer) {
                                        let text = if answer_lower == "accept" {
                                            call.args["text"].as_str().unwrap_or_default()
                                        } else {
                                            raw_answer
                                        };
                                        let scope = crate::agent::memory::Scope::parse(
                                            call.args["scope"].as_str().unwrap_or("project"),
                                        );
                                        match scope.and_then(|scope| {
                                            crate::agent::memory::apply_proposal(
                                                &root,
                                                scope,
                                                call.args["section"].as_str().unwrap_or("Project"),
                                                text,
                                                call.args["replaces"].as_str(),
                                                &session_id,
                                                memory.max_tokens,
                                            )
                                            .map(|path| {
                                                format!("memory written: {}", path.display())
                                            })
                                            .map_err(|error| error.to_string())
                                        }) {
                                            Ok(output) => tools::Outcome::ok(output),
                                            Err(error) => tools::Outcome::err(error),
                                        }
                                    } else if answer.ok && answer_lower == "reject" {
                                        tools::Outcome::ok("memory proposal rejected")
                                    } else if answer.ok && answer_lower == "edit" {
                                        tools::Outcome::err(
                                            "memory proposal edit requires a follow-up proposal",
                                        )
                                    } else {
                                        tools::Outcome::err("memory proposal was not accepted")
                                    }
                                }
                            }
                        }
                        "plan" => {
                            let mut args = call.args.clone();
                            args["context_limit"] = serde_json::json!(context_limit);
                            let outcome = run_tool_blocking(&mut ctx, "plan", &args).await;
                            if outcome.ok
                                && let Ok(Some(saved)) =
                                    plan::open_active_for_session(&root, Some(&session_id))
                            {
                                plan_todos = saved
                                    .steps
                                    .iter()
                                    .map(|step| {
                                        format!("[{}] {}", step.status.as_str(), step.title)
                                    })
                                    .collect();
                                let _ = tx.send(AgentEvent::Todos(plan_todos.clone())).await;
                            }
                            outcome
                        }
                        other
                            if mcp_registry
                                .as_ref()
                                .is_some_and(|registry| registry.contains(other)) =>
                        {
                            match mcp_registry
                                .as_ref()
                                .unwrap()
                                .call(other, call.args.clone())
                                .await
                            {
                                Ok((output, is_error)) => tools::Outcome {
                                    // R banner (§2.2), same rule as web:
                                    // external bytes delimited, errors bare.
                                    output: if is_error {
                                        output
                                    } else {
                                        crate::agent::trust::banner_wrap(&output)
                                    },
                                    ok: !is_error,
                                    exit_code: None,
                                    diff: None,
                                    file_diff: None,
                                    file_diffs: Vec::new(),
                                    cancelled: false,
                                },
                                Err(e) => tools::Outcome::err(format!("MCP call failed: {e:#}")),
                            }
                        }
                        other => run_tool_blocking(&mut ctx, other, &call.args).await,
                    }
                };

                // §2.5 / §3.7: `bash` is the one tool whose targets are not known
                // in advance, so a cancelled run can have mutated files no
                // pre-checkpoint covered. `snapshot_session` already implements
                // "only if the tree changed since the previous snapshot" — that
                // is exactly the condition §3.7 asks for, so it is read from
                // there rather than reimplemented.
                // Soft discipline, kept visible: the mutation went through
                // without acceptance criteria backing it. First line, so it
                // lands in the journal summary and the model reads it before
                // the output. Failed calls mutated nothing — no nudge.
                if plan_nudge && outcome.ok {
                    outcome.output = format!(
                        "[host nudge: single-file mutation without an acceptance-bearing plan — \
                         create one with plan create (goal, steps, optional acceptance) so the \
                         work settles against criteria. Proceeding anyway.]\n{}",
                        outcome.output
                    );
                }
                if outcome.cancelled
                    && call.name == "bash"
                    && let Ok(Some(sha)) = checkpoints::snapshot_session(
                        &ctx.root,
                        ctx.shadow_store,
                        ctx.checkpoint_chain(),
                        "post_bash cancelled",
                    )
                {
                    ctx.journal.push((sha, "bash (cancelled)".to_string()));
                }

                if outcome.ok
                    && matches!(call.name.as_str(), "write" | "edit" | "multi_edit")
                    && let Some(manager) = lsp_manager.as_mut()
                    && let Some(path) = call.args.get("file_path").and_then(|v| v.as_str())
                {
                    let path = root.join(path);
                    if let Ok(text) = tokio::fs::read_to_string(&path).await {
                        let _ = manager.did_change(&path, &text).await;
                        let _ = manager.did_save(&path).await;
                        tokio::task::yield_now().await;
                        let diagnostics = manager.collect_diagnostics().await.unwrap_or_default();
                        let diagnostic_count = diagnostics
                            .iter()
                            .map(|item| item.diagnostics.len())
                            .sum::<usize>();
                        // LSP severity 1 is an error, 2 a warning; anything else
                        // is information or a hint and is not an outcome.
                        let severity = |item: &crate::lsp::PublishDiagnosticsParams, want: u8| {
                            item.diagnostics
                                .iter()
                                .filter(|d| d.severity == Some(want))
                                .count()
                        };
                        let errors: usize = diagnostics.iter().map(|i| severity(i, 1)).sum();
                        let warnings: usize = diagnostics.iter().map(|i| severity(i, 2)).sum();
                        let _ = tx
                            .send(AgentEvent::Diagnostics {
                                count: diagnostic_count,
                            })
                            .await;
                        // §2.2.2 defines this record and nothing was writing it,
                        // which left §2.1.4's "diagnostics with zero errors" route
                        // to closing a verify step unreachable: the branch existed
                        // on the read side only.
                        if let Some(writer) = journal.as_mut() {
                            let _ = writer.append_evidence(
                                "diagnostics",
                                serde_json::json!({
                                    "path": path.strip_prefix(&root).unwrap_or(&path)
                                        .to_string_lossy()
                                        .replace('\\', "/"),
                                    "errors": errors,
                                    "warnings": warnings,
                                    "server": diagnostics
                                        .iter()
                                        .flat_map(|item| item.diagnostics.iter())
                                        .find_map(|d| d.source.clone())
                                        .unwrap_or_else(|| "lsp".to_string()),
                                }),
                            );
                        }
                        if !diagnostics.is_empty() {
                            // any diagnostics are worth showing to the model, but
                            // only real errors (severity 1) fail the tool result:
                            // the file is already written, and a hint or
                            // information entry must not make the model believe
                            // the write failed and retry it.
                            outcome.output.push_str("\nLSP diagnostics:\n");
                            for item in diagnostics {
                                for diagnostic in item.diagnostics {
                                    outcome.output.push_str(&format!(
                                        "- {}:{}: {}\n",
                                        item.uri,
                                        diagnostic.range.start.line + 1,
                                        diagnostic.message
                                    ));
                                }
                            }
                            outcome.ok = outcome.ok && errors == 0;
                        }
                    }
                }

                let _ = tx
                    .send(AgentEvent::ToolNotice {
                        name: call.name.clone(),
                        summary: outcome.output.clone(),
                        ok: outcome.ok,
                        diff: outcome.diff.clone(),
                        call_id: call.id.clone(),
                    })
                    .await;
                // report a checkpoint taken by the mutation, if any
                if ctx.journal.len() > journal_mark {
                    if let Some(writer) = journal.as_mut() {
                        for (sha, label) in ctx.journal[journal_mark..].iter() {
                            let _ = writer.append(
                                "checkpoint",
                                serde_json::json!({
                                    "layer": "legacy",
                                    "id": sha,
                                    "reason": "post_mutation",
                                    "label": label,
                                }),
                            );
                        }
                    }
                    if let Some((_, label)) = ctx.journal.last() {
                        let _ = tx
                            .send(AgentEvent::Checkpoint {
                                label: label.clone(),
                            })
                            .await;
                    }
                }
                if let Some(writer) = journal.as_mut() {
                    if call.name == "note" && outcome.ok {
                        let _ = writer.append("note", serde_json::json!({
                        "by": "model",
                        "note": call.args.get("kind").and_then(|v| v.as_str()).unwrap_or("lesson"),
                        "text": call.args.get("note").and_then(|v| v.as_str()).unwrap_or_default(),
                        // present only when this note closes an assumption
                        // (§2.1.4); the dispatcher has already checked that
                        // the target is open
                        "resolves": call.args.get("resolves").and_then(|v| v.as_u64()),
                    }));
                    }
                    let result_seq = if call.name == "plan" {
                        writer
                        .append(
                            "tool_result",
                            serde_json::json!({
                                "tool": call.name,
                                "call_id": call.id,
                                "ok": outcome.ok,
                                "duration_ms": tool_started.elapsed().as_millis(),
                                "summary": outcome.output.chars().take(200).collect::<String>(),
                                "trust": "high",
                                "code": if outcome.cancelled { Some("cancelled") } else { None },
                            }),
                        )
                        .ok()
                    } else {
                        // R taint (§2.2): every content-bearing result carries
                        // its class; the trust flag goes low with it. Plan ops
                        // above stay high — host observations, not content.
                        let taint = crate::agent::trust::tool_taint(
                            &call.name,
                            mcp_registry
                                .as_ref()
                                .is_some_and(|registry| registry.contains(&call.name)),
                        );
                        writer
                        .append_evidence(
                            "tool_result",
                            serde_json::json!({
                                "tool": call.name,
                                "call_id": call.id,
                                "ok": outcome.ok,
                                "duration_ms": tool_started.elapsed().as_millis(),
                                "summary": outcome.output.chars().take(200).collect::<String>(),
                                "trust": if taint.is_some() { "low" } else { "high" },
                                "taint": match taint {
                                    Some(crate::agent::trust::TaintClass::External) => "external",
                                    Some(crate::agent::trust::TaintClass::Local) => "local",
                                    None => "none",
                                },
                                // §3.7: distinct from an ordinary failure, so the
                                // journal can say the user stopped this rather
                                // than that it went wrong on its own
                                "code": if outcome.cancelled { Some("cancelled") } else { None },
                            }),
                        )
                        .ok()
                    };
                    if call.name == "plan"
                        && outcome.ok
                        && let Some(seq) = result_seq
                    {
                        let _ = writer.append(
                        "plan_evidence",
                        serde_json::json!({
                            "op": call.args.get("op").and_then(|v| v.as_str()).unwrap_or("unknown"),
                            "evidence": [seq],
                        }),
                    );
                    }
                    let diffs: Vec<&tools::FileDiff> = if !outcome.file_diffs.is_empty() {
                        outcome.file_diffs.iter().collect()
                    } else {
                        outcome.file_diff.as_ref().into_iter().collect()
                    };
                    let mut invalidated_paths = Vec::new();
                    for metadata in diffs {
                        let _ = writer.append_evidence(
                            "file_diff",
                            serde_json::json!({
                                "path": metadata.path,
                                "added": metadata.added,
                                "removed": metadata.removed,
                                "hash_before": metadata.hash_before,
                                "hash_after": metadata.hash_after,
                                "mode": metadata.mode,
                                "checkpoint": metadata.checkpoint,
                                // links into the layer-1 blob store (§2.5), so a
                                // single step can be reverted from the journal
                                // alone; absent when nothing was stored
                                "blob_before": metadata.blob_before,
                                "blob_after": metadata.blob_after,
                            }),
                        );
                        invalidated_paths.push(
                            metadata
                                .path
                                .replace('\\', "/")
                                .trim_start_matches("./")
                                .to_string(),
                        );
                    }
                    // validation invalidation (§2.1.4): a mutation on traversed
                    // paths stales passed receipts. Best-effort like the
                    // evidence appends above; only commits when something
                    // actually went stale.
                    if !invalidated_paths.is_empty() {
                        let _ = plan::invalidate_on_diff(&root, &session_id, &invalidated_paths);
                    }
                    if call.name == "plan" {
                        // `plan_op` journals its own intent records ahead of every
                        // store (§2.1.4); resync this long-lived handle past them
                        // so the next append cannot reuse a sequence number.
                        // A failed resync is loud (#189): proceeding on a stale
                        // counter would corrupt evidence refs.
                        if let Err(e) = writer.resync() {
                            crate::providers::log_http(&format!(
                                "journal resync failed — sequence numbers may collide: {e:#}"
                            ));
                        }
                        let op = call
                            .args
                            .get("op")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let plan_step_id = call.args.get("id").and_then(|value| value.as_str());
                        let active = plan::open_active_for_session(&root, Some(&session_id))
                            .ok()
                            .flatten();
                        let plan_id = active.as_ref().map(|p| p.id.clone());
                        if outcome.ok
                            && op == "start"
                            && let Some(id) = plan_step_id
                        {
                            writer.set_attribution(Some(id.to_string()), plan_id, "main");
                            // The session now holds this step (§2.2.3).
                            ctx.current_step = Some(id.to_string());
                            if subagent_depth == 0 {
                                let _ = tx
                                    .send(AgentEvent::StepCurrent {
                                        step: Some(id.to_string()),
                                    })
                                    .await;
                            }
                            let tag = format!("step_{id}_start");
                            let sha = if let Some((s, t)) = ctx.journal.last()
                                && t == &tag
                            {
                                Some(s.clone())
                            } else {
                                checkpoints::snapshot_boundary(
                                    &root,
                                    shadow_store,
                                    parent_session.as_deref().unwrap_or(&session_id),
                                    &tag,
                                )
                                .ok()
                                .flatten()
                                .inspect(|s| {
                                    ctx.journal.push((s.clone(), tag.clone()));
                                })
                            };
                            if let Some(sha) = sha {
                                let _ = writer.append(
                                    "checkpoint",
                                    serde_json::json!({
                                        "layer": "shadow",
                                        "id": sha,
                                        "reason": "step_start",
                                        "step": id,
                                    }),
                                );
                            }
                        } else if outcome.ok && matches!(op, "finish" | "block" | "cancel") {
                            if op == "finish"
                                && let Some(id) = plan_step_id
                            {
                                let tag = format!("step_{id}_finish");
                                let sha = if let Some((s, t)) = ctx.journal.last()
                                    && t == &tag
                                {
                                    Some(s.clone())
                                } else {
                                    checkpoints::snapshot_boundary(
                                        &root,
                                        shadow_store,
                                        parent_session.as_deref().unwrap_or(&session_id),
                                        &tag,
                                    )
                                    .ok()
                                    .flatten()
                                    .inspect(|s| {
                                        ctx.journal.push((s.clone(), tag.clone()));
                                    })
                                };
                                if let Some(sha) = sha {
                                    let _ = writer.append(
                                        "checkpoint",
                                        serde_json::json!({
                                            "layer": "shadow",
                                            "id": sha,
                                            "reason": "step_finish",
                                            "step": id,
                                        }),
                                    );
                                }
                            }
                            writer.set_attribution(None, plan_id, "main");
                            // The session is idle again (§2.2.3).
                            ctx.current_step = None;
                            if subagent_depth == 0 {
                                let _ = tx.send(AgentEvent::StepCurrent { step: None }).await;
                            }
                        }
                        if outcome.ok
                            && matches!(op, "finish" | "block" | "cancel")
                            && !crate::bench::baseline()
                        {
                            let _ = crate::agent::diary::write_entry(
                                &root,
                                crate::agent::diary::today(),
                                &session_id,
                                "step_lifecycle",
                                Some(&provider),
                                &model_id,
                                plan::open_active_for_session(&root, Some(&session_id))
                                    .ok()
                                    .flatten()
                                    .map(|plan| plan::render(&plan))
                                    .as_deref(),
                                None,
                                messages
                                    .iter()
                                    .rev()
                                    .find(|message| message.role == Role::User)
                                    .map(|message| message.content.as_str()),
                                Some(diary.token_budget),
                                diary.effort,
                                Some(Duration::from_secs(diary.timeout_secs)),
                            )
                            .await;
                        }
                    }
                }
                if outcome.cancelled {
                    interrupted = true;
                }
                // repeat nudge (§7 W): an identical re-run learned nothing.
                // Advisory and single-shot — attached to this result only, so
                // it never nags twice about the same bytes.
                if call.name == "bash"
                    && outcome.ok
                    && let Some(note) = repeat_bash_note(&messages, call, &outcome.output)
                {
                    outcome.output.push_str(&note);
                }
                messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
                if interrupted {
                    // §3.7: Esc means stop — the remaining calls in this batch
                    // (if the model requested several) do not run. Each of them
                    // still needs a tool_result: a tool_use without a matching
                    // tool_result is a fatal protocol error for Anthropic
                    // (400 "Each tool_use must have a corresponding tool_result")
                    // and for OpenAI (orphan tool_call_id), which would lock the
                    // session permanently.
                    for rest in &turn.calls[call_index + 1..] {
                        messages.push(Message::tool_result(
                            &rest.id,
                            "cancelled by user — this tool call was not run".to_string(),
                            true,
                        ));
                    }
                    break;
                }
            }
        }
        if interrupted {
            // and no further model turn is requested this round; the turn
            // ends with what was accumulated, the same shape as a normal
            // final answer (§3.7: step stays in_progress, nothing reverted).
            break;
        }
    }

    let _ = tx
        .send(AgentEvent::Completed(Ok(AgentOutcome {
            messages,
            summary,
            todos,
            journal: ctx.journal,
            plan_todos,
        })))
        .await;
    if let Some(manager) = lsp_manager {
        let _ = manager.shutdown().await;
    }
}





#[cfg(test)]
mod subagent_tests {
    use super::loop_ask::is_accepted_memory_answer;
    use super::loop_subagent::{
        MAX_PARALLEL_SUBAGENTS, SubagentTask, next_subagent_session,
        subagent_tasks_from_args,
    };

    /// #190: every child session id is distinct, even back-to-back —
    /// journal files must never be shared between runs.
    #[test]
    fn subagent_session_ids_are_unique() {
        let a = next_subagent_session();
        let b = next_subagent_session();
        assert_ne!(a, b, "counter must disambiguate same-ms spawns");
        assert!(a.starts_with("sub-"), "{a}");
        assert!(
            !a.contains('/') && !a.contains('\\') && !a.contains(".."),
            "journal-file safe: {a}"
        );
    }

    #[test]
    fn accepts_one_or_many_subagent_tasks() {
        let labels = |tasks: Vec<SubagentTask>| {
            tasks.into_iter().map(|t| t.label).collect::<Vec<_>>()
        };
        assert_eq!(
            labels(
                subagent_tasks_from_args(&serde_json::json!({"task":" inspect "})).unwrap()
            ),
            vec!["inspect"]
        );
        assert_eq!(
            labels(
                subagent_tasks_from_args(&serde_json::json!({"tasks":["one","two"]})).unwrap()
            ),
            vec!["one", "two"]
        );
        // strings are read-only writers of nothing
        for task in subagent_tasks_from_args(&serde_json::json!({"tasks":["one"]})).unwrap() {
            assert!(!task.write);
            assert!(task.paths.is_empty());
        }
    }

    /// Regression: the model sent three task OBJECTS and the whole batch
    /// died with "subagent task is required" — objects carry the text
    /// under task|prompt|description, a lone string is one task.
    #[test]
    fn accepts_object_shaped_subagent_tasks() {
        let labels = |tasks: Vec<SubagentTask>| {
            tasks.into_iter().map(|t| t.label).collect::<Vec<_>>()
        };
        assert_eq!(
            labels(
                subagent_tasks_from_args(&serde_json::json!({"tasks":[
                    {"prompt": "research articles"},
                    {"task": "check commercial modes"},
                    {"description": "probe internals"},
                ]}))
                .unwrap()
            ),
            vec![
                "research articles",
                "check commercial modes",
                "probe internals"
            ]
        );
        assert_eq!(
            labels(
                subagent_tasks_from_args(&serde_json::json!({"tasks": "do it all"})).unwrap()
            ),
            vec!["do it all"]
        );
        // empties still refuse with the original message intact
        let error =
            subagent_tasks_from_args(&serde_json::json!({"tasks": [{}, "  "]})).unwrap_err();
        assert!(error.contains("subagent task is required"), "{error}");
    }

    #[test]
    fn subagent_writer_scope_rules() {
        // write without paths refuses: a scope must be declared
        let error = subagent_tasks_from_args(&serde_json::json!({
            "tasks": [{"task": "fix it", "write": true}]
        }))
        .unwrap_err();
        assert!(error.contains("needs paths"), "{error}");

        // overlapping writer scopes refuse, naming the conflict
        let error = subagent_tasks_from_args(&serde_json::json!({
            "tasks": [
                {"task": "fix a", "write": true, "paths": ["src/a"]},
                {"task": "fix b", "write": true, "paths": ["src/a/b.rs"]},
            ]
        }))
        .unwrap_err();
        assert!(error.contains("overlap"), "{error}");

        // disjoint writers pass with normalized scopes
        let tasks = subagent_tasks_from_args(&serde_json::json!({
            "tasks": [
                {"task": "fix a", "write": true, "paths": ["src/a/", "./src/b"]},
                {"task": "read all"},
            ]
        }))
        .unwrap();
        assert!(tasks[0].write);
        assert_eq!(tasks[0].paths, vec!["src/a", "src/b"]);
        assert!(!tasks[1].write);
    }

    #[test]
    fn rejects_more_than_eight_subagents() {
        let tasks: Vec<String> = (0..9).map(|n| format!("task {n}")).collect();
        let error = subagent_tasks_from_args(&serde_json::json!({"tasks":tasks})).unwrap_err();
        assert!(error.contains("maximum is 8"));
        assert_eq!(MAX_PARALLEL_SUBAGENTS, 4);
    }

    #[test]
    fn memory_proposal_answers_filter_subagent_responses() {
        assert!(is_accepted_memory_answer(true, "accept"));
        assert!(is_accepted_memory_answer(true, " ACCEPT "));
        assert!(is_accepted_memory_answer(true, "edited content by user"));
        assert!(!is_accepted_memory_answer(true, "reject"));
        assert!(!is_accepted_memory_answer(true, "edit"));
        assert!(!is_accepted_memory_answer(true, ""));
        assert!(!is_accepted_memory_answer(false, "accept"));
        assert!(!is_accepted_memory_answer(
            true,
            "subagents cannot reach the user; decide yourself and continue"
        ));
    }
}

#[cfg(test)]
mod transport_tests {
    use super::loop_compact::rejects_continuation;
    use super::loop_turn::{request_messages, turn_transport};
    use crate::providers::ContextTransport;
    use super::*;

    fn roles(messages: &[Message]) -> Vec<Role> {
        messages.iter().map(|m| m.role).collect()
    }

    /// Continuing from a previous response, the provider holds everything up
    /// to its own last message; we send what it has not seen.
    #[test]
    fn a_continuation_sends_the_tool_results_the_provider_is_waiting_for() {
        let messages = vec![
            Message::new(Role::User, "read the file"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({}),
            )]),
            Message::tool_result("c1", "fn main() {}", false),
        ];
        assert_eq!(
            roles(&request_messages(
                &messages,
                ContextTransport::PreviousResponse
            )),
            vec![Role::Tool],
            "the result of the call must reach the model"
        );

        // several calls in one turn: every result travels
        let mut many = messages.clone();
        many.push(Message::tool_result("c2", "other", false));
        assert_eq!(
            roles(&request_messages(&many, ContextTransport::PreviousResponse)),
            vec![Role::Tool, Role::Tool]
        );

        // a plain exchange still sends just the new user turn
        let plain = vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "hello"),
            Message::new(Role::User, "again"),
        ];
        assert_eq!(
            roles(&request_messages(
                &plain,
                ContextTransport::PreviousResponse
            )),
            vec![Role::User]
        );

        // and a user turn that follows tool results keeps both, in order
        let mixed = vec![
            Message::new(Role::Assistant, "done"),
            Message::tool_result("c1", "out", false),
            Message::new(Role::User, "now this"),
        ];
        assert_eq!(
            roles(&request_messages(
                &mixed,
                ContextTransport::PreviousResponse
            )),
            vec![Role::Tool, Role::User]
        );
    }

    fn caps() -> crate::providers::ProviderCapabilities {
        crate::providers::ProviderCapabilities {
            previous_response: true,
            ..Default::default()
        }
    }

    /// The live 400 this exists for, from an OpenAI-compatible relay that
    /// accepts `previous_response_id` and does not chain by it:
    /// `No tool call found for function call output with call_id ...`.
    /// A request answering a tool call must carry the calls itself.
    #[test]
    fn a_request_answering_a_tool_call_does_not_rely_on_the_chain() {
        let mid_turn = vec![
            Message::new(Role::User, "read the file"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({}),
            )]),
            Message::tool_result("c1", "fn main() {}", false),
        ];
        assert_eq!(
            turn_transport(caps(), Some("resp_1"), &mid_turn, true),
            ContextTransport::Stateless,
            "tool outputs must travel with the calls they answer"
        );
        // and the whole transcript goes with it, calls included
        assert_eq!(
            request_messages(&mid_turn, ContextTransport::Stateless).len(),
            3
        );

        // between turns there is nothing to match, so continuation is fine
        let between = vec![
            Message::new(Role::Assistant, "done"),
            Message::new(Role::User, "next"),
        ];
        assert_eq!(
            turn_transport(caps(), Some("resp_1"), &between, true),
            ContextTransport::PreviousResponse
        );
    }

    /// Once a provider has refused the reference, the session stops offering
    /// it — one failed request per session, not one per turn.
    #[test]
    fn a_refused_reference_is_not_offered_again() {
        let between = vec![
            Message::new(Role::Assistant, "done"),
            Message::new(Role::User, "next"),
        ];
        assert_eq!(
            turn_transport(caps(), Some("resp_1"), &between, false),
            ContextTransport::Stateless
        );
    }

    /// Prose matching again, so it is bounded on both sides: it must catch the
    /// refusal and must not fire on a provider that simply has no such field.
    #[test]
    fn only_a_real_refusal_disables_the_continuation() {
        assert!(rejects_continuation(
            "provider returned 400: No tool call found for function call output with call_id call_x0"
        ));
        assert!(rejects_continuation(
            "provider returned 400: previous response not found"
        ));
        assert!(!rejects_continuation(
            "openai-compatible: previous_response_id dropped (not supported by Chat Completions)"
        ));
        assert!(!rejects_continuation(
            "provider returned 429: rate limit exceeded"
        ));
        assert!(!rejects_continuation(
            "provider returned 400: messages must alternate"
        ));
    }

    /// Stateless is the default for a reason: everything is resent, and the
    /// shortening above must never leak into it.
    #[test]
    fn a_stateless_request_carries_the_whole_transcript() {
        let messages = vec![
            Message::new(Role::User, "one"),
            Message::new(Role::Assistant, "two"),
            Message::new(Role::User, "three"),
        ];
        assert_eq!(
            request_messages(&messages, ContextTransport::Stateless).len(),
            3
        );
        assert_eq!(
            request_messages(&messages, ContextTransport::ServerConversation).len(),
            3
        );
    }
}

#[cfg(test)]
mod effort_tests {
    use super::loop_ask::propose_reset;
    use super::loop_compact::{
        MIN_ZERO_TURNS_BEFORE_REPORTING, compact_history, compaction_request,
        effort_ignored_reason, record_compaction, rejects_effort_parameter,
        turn_shows_no_reasoning, CompactionPrefix,
    };
    use super::loop_subagent::{SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step};
    use super::loop_turn::{TurnOutcome, run_turn};
    use crate::providers::StreamEvent;
    use super::*;

    /// The prose match that replaces the one in the error classifier. It has
    /// to catch the shapes gateways actually use, and — more importantly — not
    /// fire on unrelated failures, since a false positive silently drops the
    /// user's effort level for the rest of the session.
    #[test]
    fn a_rejected_effort_parameter_is_recognised_across_gateways() {
        for err in [
            "provider returned 400: Function tools with reasoning_effort are not supported",
            "provider returned 400: {\"error\":{\"message\":\"Unrecognized request argument \
             supplied: reasoning_effort\"}}",
            "provider returned 400: unsupported parameter: 'reasoning.effort'",
            "provider returned 400: thinking is not supported for this model",
            "provider returned 400: invalid value for budget_tokens",
        ] {
            assert!(rejects_effort_parameter(err), "should have matched: {err}");
        }
    }

    /// The behaviour the string match exists for: one retry without the
    /// parameter, and a signal so the session stops sending it. Before this,
    /// the same 400 ended the turn with "provider returned 400".
    #[tokio::test]
    async fn a_rejected_parameter_is_retried_once_without_it() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        let server = std::thread::spawn(move || {
            for (n, stream) in listener.incoming().take(2).enumerate() {
                let stream = stream.unwrap();
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
                let mut buf = vec![0u8; len];
                reader.read_exact(&mut buf).ok();
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());

                let mut out = stream;
                if n == 0 {
                    let body = "{\"error\":{\"message\":\"Unrecognized request argument \
                                supplied: reasoning_effort\"}}";
                    write!(
                        out,
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                } else {
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                                data: [DONE]\n\n";
                    write!(
                        out,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
                let _ = out.flush();
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });

        let provider = crate::providers::create(&crate::config::ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Openai,
            base_url: format!("http://{addr}/v1"),
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "hi")],
            effort: Some(crate::config::EffortLevel::High),
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let (_ctl_tx, mut ctl) = mpsc::channel(4);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut response_id = None;
        let mut prompt_size = 0u64;
        let outcome = run_turn(
            &provider,
            &req,
            &tx,
            &mut ctl,
            &mut response_id,
            &mut prompt_size,
            false,
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the retry without the parameter must succeed: {}",
                e.message
            )
        });
        drop(tx);
        drain.await.unwrap();
        server.join().unwrap();

        assert_eq!(outcome.text, "ok");
        assert!(
            outcome.effort_rejected,
            "the caller has to learn that the parameter was refused"
        );
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2, "exactly one retry: {bodies:?}");
        assert!(
            bodies[0].contains("reasoning_effort"),
            "first attempt carried the parameter: {}",
            bodies[0]
        );
        assert!(
            !bodies[1].contains("reasoning_effort"),
            "the retry must drop it: {}",
            bodies[1]
        );
    }

    fn turn(reasoning_tokens: Option<u64>, effort_rejected: bool) -> TurnOutcome {
        TurnOutcome {
            text: String::new(),
            calls: Vec::new(),
            retries: 0,
            reasoning_tokens,
            saw_reasoning: false,
            effort_rejected,
            provider_state: None,
        }
    }

    /// Silence is not evidence: a provider that reports no reasoning counter
    /// at all (Anthropic) must never be read as "it ignored you".
    #[test]
    fn a_missing_or_positive_counter_is_never_evidence() {
        for tokens in [None, Some(1), Some(4096)] {
            assert!(!turn_shows_no_reasoning(&turn(tokens, false)), "{tokens:?}");
            assert_eq!(effort_ignored_reason(&turn(tokens, false), 9), None);
        }
        assert!(turn_shows_no_reasoning(&turn(Some(0), false)));
    }

    /// One zero turn says nothing: a reasoning model may spend nothing on a
    /// trivial question, and a relay may stub the usage details. Only a run of
    /// them is worth reporting, and the wording says what was measured rather
    /// than passing a verdict on the model.
    #[test]
    fn one_zero_turn_is_not_enough_to_report() {
        let t = turn(Some(0), false);
        assert_eq!(effort_ignored_reason(&t, 1), None);
        assert_eq!(
            effort_ignored_reason(&t, MIN_ZERO_TURNS_BEFORE_REPORTING),
            Some("no reasoning reported on 2 turns in a row".to_string())
        );
    }

    /// A refusal is the provider saying so out loud, so it needs no run-up and
    /// keeps the stronger wording.
    #[test]
    fn a_refused_parameter_is_reported_at_once() {
        assert_eq!(
            effort_ignored_reason(&turn(None, true), 0),
            Some("the provider rejected the effort parameter".to_string())
        );
    }

    /// Field data from a third-party OpenAI-compatible gateway: the same
    /// model (`gpt-5.6-terra`, which reasons) at the same level reported no
    /// counter on one turn and zero on the next, 40 seconds apart. A zero that
    /// arrives next to streamed reasoning content is the gateway's bookkeeping,
    /// not an ignored request, and must not be reported as one.
    /// The root cause of the false positive: two usage events in one turn,
    /// the second a stub of zeros. Taking the last value let a real count of
    /// 1088 decay to 0 and the host called the level ignored.
    #[tokio::test]
    async fn a_later_zero_usage_event_cannot_erase_a_real_count() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
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
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok();
            // first the real numbers, then the stub a gateway can append
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"4\"}}]}\n\n\
                        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":900,\
                        \"completion_tokens_details\":{\"reasoning_tokens\":1088}}}\n\n\
                        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":900,\
                        \"completion_tokens_details\":{\"reasoning_tokens\":0}}}\n\n\
                        data: [DONE]\n\n";
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(150));
        });

        let provider = crate::providers::create(&crate::config::ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Openai,
            base_url: format!("http://{addr}/v1"),
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "m".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "2+2")],
            effort: Some(crate::config::EffortLevel::High),
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let (_ctl_tx, mut ctl) = mpsc::channel(4);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let mut response_id = None;
        let mut prompt_size = 0u64;
        let outcome = run_turn(
            &provider,
            &req,
            &tx,
            &mut ctl,
            &mut response_id,
            &mut prompt_size,
            false,
        )
        .await
        .unwrap_or_else(|e| panic!("turn failed: {}", e.message));
        drop(tx);
        drain.await.unwrap();
        server.join().unwrap();

        assert_eq!(
            outcome.reasoning_tokens,
            Some(1088),
            "the stub event must not erase the real count"
        );
        assert!(
            !turn_shows_no_reasoning(&outcome),
            "and the turn must not count as a zero-reasoning turn"
        );
        assert_eq!(
            prompt_size, 30,
            "the same must hold for the prompt meter it sits next to"
        );
    }

    #[test]
    fn a_zero_count_beside_streamed_reasoning_is_not_evidence() {
        let mut t = turn(Some(0), false);
        t.saw_reasoning = true;
        assert!(
            !turn_shows_no_reasoning(&t),
            "streamed reasoning content contradicts the counter"
        );
        assert_eq!(effort_ignored_reason(&t, 9), None);

        // with no reasoning content, the same zero does count towards the run
        assert!(turn_shows_no_reasoning(&turn(Some(0), false)));

        // and a refusal is evidence either way
        let mut t = turn(None, true);
        t.saw_reasoning = true;
        assert_eq!(
            effort_ignored_reason(&t, 0),
            Some("the provider rejected the effort parameter".to_string())
        );
    }

    #[test]
    fn unrelated_failures_do_not_drop_the_effort_level() {
        for err in [
            "provider returned 401: invalid api key",
            "provider returned 400: messages must alternate",
            "provider returned 429: rate limit exceeded",
            "request failed: error sending request",
            // names the parameter, but not as the thing that failed
            "provider returned 500: internal error while streaming thinking blocks",
            "provider returned 400: prompt is too long: 300000 tokens > 200000 maximum",
        ] {
            assert!(!rejects_effort_parameter(err), "should not match: {err}");
        }
    }

    #[test]
    fn test_plan_with_acceptance_gate_input() {
        // the gate asks "is there a criterion", not "is there a plan" and
        // not "is the prose trivial": an active plan counts only with
        // acceptance items on it.
        let dir = std::env::temp_dir().join(format!("sqwai-gate-{}", crate::plan::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!plan_with_acceptance(&dir), "no plan at all");

        let mut plan = crate::plan::create(
            "goal".to_string(),
            Vec::new(),
            Vec::new(),
            vec![crate::plan::NewStep {
                title: "step".to_string(),
                refs: Vec::new(),
            }],
            0,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&dir, &plan).unwrap();
        assert!(
            !plan_with_acceptance(&dir),
            "a plan without acceptance settles nothing"
        );

        plan.acceptance.push(crate::plan::Acceptance {
            text: "cmd: cargo test".to_string(),
            status: crate::plan::AcceptanceStatus::Pending,
            evidence: Vec::new(),
            validation: Default::default(),
            baseline: None,
            snapshot: None,
            shape: None,
            inputs: Vec::new(),
            by: None,
            reason: None,
        });
        crate::plan::store(&dir, &plan).unwrap();
        assert!(plan_with_acceptance(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    struct MockTestProvider {
        events: std::sync::Mutex<Vec<Vec<crate::providers::StreamResult>>>,
    }

    impl crate::providers::Provider for MockTestProvider {
        fn stream_chat(
            &self,
            _req: crate::providers::ChatRequest,
        ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
            use futures::StreamExt;
            let mut guard = self.events.lock().unwrap();
            let batch = if !guard.is_empty() {
                guard.remove(0)
            } else {
                vec![Err(anyhow::anyhow!(
                    "provider returned 500: internal server error"
                ))]
            };
            futures::stream::iter(batch).boxed()
        }
    }

    #[tokio::test]
    async fn test_provider_fallback_chain_switch_on_failure() {
        let primary_provider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Err(anyhow::anyhow!(
                "provider returned 500: internal server error"
            ))]]),
        });

        let fallback_provider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
                "fallback response".into(),
            ))]]),
        });

        let fallback = FallbackCandidate {
            key: "fallback-model".into(),
            model_id: "m-fallback".into(),
            provider: fallback_provider,
            effort_support: crate::config::EffortSupport::default(),
            context_limit: 10000,
        };

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-fallback-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider: primary_provider,
            model_id: "m-primary".into(),
            model_key: "primary-model".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "hello")],
            root: temp_dir.clone(),
            session_id: "test-fallback-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: false,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: Default::default(),
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![fallback],
        };

        let mut handle = spawn_agent(input);
        let mut switched = false;
        let mut got_fallback_text = false;

        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::FallbackSwitched { from, to } => {
                    assert_eq!(from, "primary-model");
                    assert_eq!(to, "fallback-model");
                    switched = true;
                }
                AgentEvent::TextDelta(text) => {
                    if text == "fallback response" {
                        got_fallback_text = true;
                    }
                }
                AgentEvent::Completed(Ok(_)) => break,
                AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
                _ => {}
            }
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(switched, "should have received FallbackSwitched event");
        assert!(
            got_fallback_text,
            "should have received text from fallback provider"
        );
    }

    fn big(text: &str) -> String {
        text.repeat(600)
    }

    fn three_turns() -> Vec<Message> {
        vec![
            Message::new(Role::User, big("first task ")),
            Message::new(Role::Assistant, big("did first ")),
            Message::new(Role::User, big("second task ")),
            Message::new(Role::Assistant, "reading").with_tool_calls(vec![
                crate::providers::ToolCallReq::new(
                    "c1",
                    "read",
                    serde_json::json!({"file_path": "a.rs"}),
                ),
            ]),
            Message::tool_result("c1", big("file bytes "), false),
            Message::new(Role::User, big("third task ")),
            Message::new(Role::Assistant, big("doing third ")),
        ]
    }

    fn summary_policy() -> context::Policy {
        // limit 16k: threshold binds (0.8 × 16k = 12800 < 16k − 8k
        // reserve), the ~9900-token fixture is over, the ~6600-token kept
        // tail plus summary still fits — so stage 4 must not run
        context::Policy::with_compaction(16_000, 0.08, 2, 0.80, true)
    }

    /// An identical re-run is caught: same command plus byte-identical
    /// output fires the advisory note; a changed output, a different
    /// command, or a first run stays silent.
    #[test]
    fn repeat_bash_note_fires_only_on_identical_reruns() {
        use crate::providers::ToolCallReq;
        let bash_call = |id: &str, command: &str| {
            ToolCallReq::new(id, "bash", serde_json::json!({"command": command}))
        };
        let messages = vec![
            Message::new(Role::User, "check"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![bash_call("c1", "netstat")]),
            Message::tool_result("c1", "TCP 1.2.3.4:443", false),
            Message::new(Role::User, "and?"),
        ];
        let again = bash_call("c2", "netstat");
        let note = repeat_bash_note(&messages, &again, "TCP 1.2.3.4:443");
        assert!(note.clone().is_some_and(|n| n.contains("already ran")), "{note:?}");
        // changed output: fresh state, no note
        assert!(repeat_bash_note(&messages, &again, "TCP 9.9.9.9:80").is_none());
        // different command: no note
        let other = bash_call("c3", "Get-Process");
        assert!(repeat_bash_note(&messages, &other, "TCP 1.2.3.4:443").is_none());
        // first run ever: no note
        assert!(repeat_bash_note(&[], &again, "TCP 1.2.3.4:443").is_none());
    }


    /// `propose_reset` through the approval dialog: RunOnce abandons (plan
    /// stays on disk as Abandoned, session hold cleared), Deny keeps the
    /// plan working with a pointer to block_plan.
    #[tokio::test]
    async fn propose_reset_abandons_on_approval_and_keeps_on_deny() {
        use tokio::sync::mpsc;
        async fn run_reset(
            dir: &std::path::Path,
            session: &str,
            decision: ApprovalDecision,
        ) -> tools::Outcome {
            let call = ToolCallReq::new(
                "c1",
                "propose_reset",
                serde_json::json!({
                    "reason": "goal targets removed feature X, steps assume the deleted API",
                }),
            );
            let mut ctx = tools::ToolCtx::new(dir).in_session(session.to_string());
            let (tx_agent, mut rx_ui) = mpsc::channel::<AgentEvent>(8);
            let (tx_ui, mut rx_agent) = mpsc::channel::<ControlMsg>(8);
            let mut next_id = 0u64;
            let future = propose_reset(&call, &mut ctx, false, &tx_agent, &mut rx_agent, &mut next_id);
            tokio::pin!(future);
            loop {
                tokio::select! {
                    out = &mut future => break out,
                    ev = rx_ui.recv() => {
                        if let Some(AgentEvent::Approval { id, command, reason }) = ev {
                            assert!(command.contains("abandon plan"), "{command}");
                            assert!(reason.contains("removed feature"), "{reason}");
                            tx_ui
                                .send(ControlMsg::ApprovalAnswer { id, decision })
                                .await
                                .unwrap();
                        }
                    }
                }
            }
        }
        let dir = std::env::temp_dir().join(format!("sqwai-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = "reset-sess";
        let mut plan = plan::create(
            "goal".to_string(),
            Vec::new(),
            vec!["manual: eyeball it".to_string()],
            vec![plan::NewStep {
                title: "work".into(),
                refs: Vec::new(),
            }],
            1000,
            &plan::Limits::default(),
        )
        .unwrap();
        plan.sessions = vec![session.to_string()];
        plan::store(&dir, &plan).unwrap();
        let plan_id = plan.id.clone();

        let outcome = run_reset(&dir, session, ApprovalDecision::RunOnce).await;
        assert!(outcome.ok, "{}", outcome.output);
        assert!(outcome.output.contains("abandoned by user approval"), "{}", outcome.output);
        let after = plan::open(&dir, &plan_id).unwrap();
        assert_eq!(after.status, plan::PlanStatus::Abandoned);

        // deny: a fresh active plan stays active
        let mut plan2 = plan::create(
            "goal2".to_string(),
            Vec::new(),
            Vec::new(),
            vec![plan::NewStep {
                title: "work".into(),
                refs: Vec::new(),
            }],
            1000,
            &plan::Limits::default(),
        )
        .unwrap();
        plan2.sessions = vec![session.to_string()];
        plan::store(&dir, &plan2).unwrap();
        let denied = run_reset(&dir, session, ApprovalDecision::Deny).await;
        assert!(!denied.ok, "{}", denied.output);
        assert!(denied.output.contains("denied by user"), "{}", denied.output);
        // the first plan stays abandoned; the second stays active
        assert_eq!(plan::open(&dir, &plan_id).unwrap().status, plan::PlanStatus::Abandoned);
        assert_eq!(
            plan::open(&dir, &plan2.id).unwrap().status,
            plan::PlanStatus::Active
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn parent_prefix<'a>(
        system: &'a [crate::providers::SystemPart],
        tools: &'a [crate::providers::ToolSpec],
    ) -> CompactionPrefix<'a> {
        CompactionPrefix { system, tools }
    }

    /// Cache-aware layout: parent system and schemas go on the wire
    /// byte-identical, the full history stays in order, the prompt is the
    /// last message. That is the whole trick — everything the provider
    /// already holds reads at cache price.
    #[test]
    fn compaction_request_reuses_the_parent_prefix() {
        let system = vec![
            crate::providers::SystemPart::cached("rules"),
            crate::providers::SystemPart::volatile("today"),
        ];
        let tools = vec![crate::providers::ToolSpec {
            name: "read".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let history = vec![
            Message::new(Role::User, "do it"),
            Message::new(Role::Assistant, "done"),
        ];
        let prefix = parent_prefix(&system, &tools);
        let (req, cache_aware) =
            compaction_request(Some(&prefix), &history[..1], &history, None, "", "m", false);
        assert!(cache_aware);
        assert_eq!(req.system, system, "prefix must be byte-identical");
        let req_tools: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(req_tools, vec!["read"], "schemas travel to hold the prefix");
        assert_eq!(req.messages.len(), history.len() + 1);
        for (got, want) in req.messages.iter().zip(history.iter()) {
            assert_eq!(got.role, want.role);
            assert_eq!(got.content, want.content, "history intact and in order");
        }
        let prompt = req.messages.last().unwrap();
        assert_eq!(prompt.role, Role::User);
        assert!(prompt.content.contains("Rules:"), "{}", prompt.content);
        assert!(
            !prompt.content.contains("do it"),
            "the transcript must not ride twice: {}",
            prompt.content
        );
        assert_eq!(req.effort, None, "the summarizer thinks nothing");
    }

    /// No prefix handed over (tests, side calls): the compact standalone
    /// request — tiny system, transcript as text, no schemas.
    #[test]
    fn compaction_request_falls_back_to_standalone_without_a_prefix() {
        let older = vec![Message::new(Role::User, "do it")];
        let (req, cache_aware) = compaction_request(None, &older, &older, None, "", "m", false);
        assert!(!cache_aware);
        assert!(req.tools.is_empty());
        assert_eq!(req.messages.len(), 1);
        assert!(req.messages[0].content.contains("do it"));
    }

    /// A history with thinking blocks cannot travel as structured messages
    /// on an effort-off request: the tool_use blocks would arrive without
    /// their thinking and the API refuses them. Such histories compact
    /// through the standalone text transcript instead.
    #[test]
    fn compaction_request_dodges_thinking_histories() {
        let history = vec![
            Message::new(Role::Assistant, "checking")
                .with_provider_state(Some(serde_json::json!({
                    "thinking_blocks": [
                        {"type": "thinking", "thinking": "hmm", "signature": "SIG"}
                    ]
                })))
                .with_tool_calls(vec![crate::providers::ToolCallReq::new(
                    "c1",
                    "check",
                    serde_json::json!({}),
                )]),
            Message::tool_result("c1", "ok", false),
        ];
        let system = vec![crate::providers::SystemPart::cached("rules")];
        let tools = vec![];
        let prefix = parent_prefix(&system, &tools);
        let (req, cache_aware) =
            compaction_request(Some(&prefix), &history, &history, None, "", "m", false);
        assert!(!cache_aware, "thinking histories take the text path");
        assert!(req.tools.is_empty());
        assert_eq!(req.messages.len(), 1);
    }

    /// Live A/B of the compaction request shape: prime the cache with a
    /// regular turn, then send the cache-aware summary request (parent
    /// prefix + history + prompt) and the standalone one (tiny system +
    /// transcript). The aware request should read the history at cache
    /// price; the standalone one pays full price for the transcript.
    /// Unique padding per run so older probes cannot warm this prefix.
    #[tokio::test]
    #[ignore = "live wire; requires configured provider"]
    async fn live_compaction_request_reuses_prefix() {
        use futures::StreamExt;
        crate::providers::set_conversation_id("cache-probe-compact");
        let bench = crate::agent::bench_harness::bench_model()
            .expect("bench_model must resolve: check config + SQWAI_BENCH_MODEL");

        async fn send(
            bench: &crate::agent::bench_harness::BenchModel,
            req: ChatRequest,
        ) -> crate::providers::Usage {
            let mut wait_secs = 5u64;
            for _ in 0..6 {
                let mut usage = crate::providers::Usage::default();
                let mut rate_limited = false;
                let mut stream = bench.provider.stream_chat(req.clone());
                while let Some(ev) = stream.next().await {
                    match ev {
                        Ok(StreamEvent::Text(_)) => {}
                        Ok(StreamEvent::Usage(u)) => {
                            if u.prompt_tokens > 0 {
                                usage = u;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let msg = format!("{e:#}");
                            if msg.contains("429") || msg.contains("rate_limit") {
                                rate_limited = true;
                                break;
                            }
                            panic!("stream failed: {msg}");
                        }
                    }
                }
                if !rate_limited {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    return usage;
                }
                eprintln!("  429: waiting {wait_secs}s");
                tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                wait_secs = (wait_secs * 2).min(60);
            }
            panic!("still rate-limited after retries");
        }

        fn report(turn: &str, u: &crate::providers::Usage) {
            let cached = u.cached_tokens.unwrap_or(0);
            let frac = if u.prompt_tokens > 0 {
                cached as f64 / u.prompt_tokens as f64
            } else {
                0.0
            };
            println!(
                "{turn}: prompt={} cached={} completion={} cached_frac={:.2}",
                u.prompt_tokens, cached, u.completion_tokens, frac
            );
        }

        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64;
        let system = vec![
            crate::providers::SystemPart::cached(format!(
                "Probe rules {nonce}. {}",
                "Obey the project instructions. ".repeat(30)
            )),
            crate::providers::SystemPart::cached(format!(
                "Probe AGENTS.md {nonce}. {}",
                "Use rustfmt. ".repeat(30)
            )),
        ];
        let tools = vec![crate::providers::ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let history = three_turns();
        let older = &history[..4];

        // prime: a regular turn on the same prefix + history
        let mut prime_msgs = history.clone();
        prime_msgs.push(Message::new(Role::User, "go on"));
        let prime = ChatRequest {
            model_id: bench.model_id.clone(),
            system: system.clone(),
            messages: prime_msgs,
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(64),
            tools: tools.clone(),
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        // keep the prime small enough to leave room: reuse three_turns as-is
        let u = send(&bench, prime).await;
        report("prime", &u);

        // cache-aware: same prefix + history, prompt appended
        let prefix = CompactionPrefix {
            system: &system,
            tools: &tools,
        };
        let (aware, is_aware) =
            compaction_request(Some(&prefix), older, &history, None, "", &bench.model_id, false);
        assert!(is_aware);
        let u = send(&bench, aware).await;
        report("aware", &u);

        // standalone: tiny system + rendered transcript
        let (alone, is_aware) =
            compaction_request(None, older, &history, None, "", &bench.model_id, false);
        assert!(!is_aware);
        let u = send(&bench, alone).await;
        report("alone", &u);
    }

    /// Prod-shape replication: 95 mixed messages (~46k tokens, like the
    /// T1 shakedown transcript), 1M limit, 0.01 threshold, summary off.
    /// compact_history must shrink and report — not silently pass through.

    #[tokio::test]
    async fn compact_history_trims_a_long_plain_transcript() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(Vec::new()),
        });
        let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
        assert_eq!(policy.budget(), 10_000);
        let mut messages = Vec::new();
        for i in 0..30 {
            // user prose dominates: masking the tool results must NOT
            // resolve pressure alone, or stage 4 never fires below
            messages.push(Message::new(
                Role::User,
                format!("task {i} {}", "q".repeat(2000)),
            ));
            messages.push(Message::new(Role::Assistant, format!("work {i}")));
            messages.push(Message::tool_result(
                format!("c{i}"),
                "r".repeat(3000),
                false,
            ));
        }
        // +5 stray user messages to reach 95 like the field transcript
        for i in 0..5 {
            messages.push(Message::new(Role::User, format!("extra {i}")));
        }
        assert_eq!(messages.len(), 95);
        let before = context::estimated_tokens(&messages);
        assert!(before > 10_000, "fixture must be over budget, got {before}");
        let mut summary = None;
        let out = compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .expect("must compact a 4x-over-budget transcript");
        assert!(out.1 < out.0, "must shrink: {out:?}");
        assert!(messages.len() < 95, "messages must drop");
    }

    /// Incremental replication of the live loop: grow the transcript turn
    /// by turn, calling compact_history before each (like run_agent does).
    /// Trims must show up as drops in length — never 40 silent passes.
    #[tokio::test]
    async fn compact_history_fires_repeatedly_as_history_grows() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(Vec::new()),
        });
        let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
        let mut messages = Vec::new();
        let mut summary = None;
        let mut trims = 0;
        let mut peak_len = 0;
        for i in 0..40 {
            messages.push(Message::new(
                Role::User,
                format!("task {i} {}", "q".repeat(500)),
            ));
            messages.push(Message::new(Role::Assistant, format!("work {i}")));
            messages.push(Message::tool_result(
                format!("c{i}"),
                "r".repeat(3000),
                false,
            ));
            let before_len = messages.len();
            if compact_history(
                &provider,
                "m",
                &mut messages,
                &mut summary,
                &policy,
                false,
                "",
                None,
            )
            .await
            .is_some()
            {
                trims += 1;
                assert!(
                    messages.len() < before_len,
                    "a reported trim must drop messages"
                );
            }
            peak_len = peak_len.max(messages.len());
        }
        assert!(
            trims >= 3,
            "40 over-budget turns must trim repeatedly, got {trims}"
        );
        assert!(
            peak_len < 120,
            "history must stay bounded near the budget, peak len {peak_len}"
        );
    }

    /// Bench shape: ONE user prompt followed by assistant/tool cycles, summary
    /// off. Regression test: the user-boundary-only cut never fired here, so
    /// compactions stayed 0 while measured grew past the budget every turn.
    #[tokio::test]
    async fn compact_history_trims_a_single_prompt_tool_loop() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(Vec::new()),
        });
        let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
        let mut messages = vec![Message::new(Role::User, "do the thing")];
        let mut summary = None;
        let mut trims = 0;
        let mut peak_len = 0;
        for i in 0..40 {
            // user prose per cycle: masking tool results must NOT resolve
            // pressure alone, or no trim is ever reported below
            messages.push(Message::new(
                Role::User,
                format!("task {i} {}", "q".repeat(2000)),
            ));
            messages.push(Message::new(Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq::new(
                    format!("c{i}"),
                    "bash",
                    serde_json::json!({"command": "ls"}),
                ),
            ]));
            messages.push(Message::tool_result(
                format!("c{i}"),
                "r".repeat(3000),
                false,
            ));
            let before_len = messages.len();
            if compact_history(
                &provider,
                "m",
                &mut messages,
                &mut summary,
                &policy,
                false,
                "",
                None,
            )
            .await
            .is_some()
            {
                trims += 1;
                assert!(
                    messages.len() < before_len,
                    "a reported trim must drop messages"
                );
            }
            peak_len = peak_len.max(messages.len());
        }
        assert!(
            trims >= 3,
            "40 over-budget cycles must trim repeatedly, got {trims}"
        );
        assert!(
            peak_len < 81,
            "history must stay bounded near the budget, peak len {peak_len}"
        );
    }

    #[test]
    fn compaction_record_carries_tokens_and_summary() {
        let root = std::env::temp_dir().join(format!("sqwai-compact-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        record_compaction(
            &mut Some(journal),
            10_000,
            2_500,
            40,
            10,
            true,
            "did things",
        );
        // None journal: silent no-op, never panics
        record_compaction(&mut None, 1, 1, 1, 1, false, "");
        let recs = crate::agent::journal::Journal::records(&root).expect("read");
        let c = recs
            .iter()
            .find(|r| r.kind == "compaction")
            .expect("record");
        assert_eq!(c.fields["before"], 10_000);
        assert_eq!(c.fields["after"], 2_500);
        assert_eq!(c.fields["msgs_before"], 40);
        assert_eq!(c.fields["msgs_after"], 10);
        assert_eq!(c.fields["summarized"], true);
        assert_eq!(c.fields["summary"], "did things");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn capture_nudge_fires_only_on_uncovered_restrictions() {
        let c = |s: &str| vec![s.to_string()];
        // no marker → silent, whatever the plan holds
        assert!(capture_nudge("please refactor the engine", &[]).is_none());
        assert!(capture_nudge("please refactor the engine", &c("keep API stable")).is_none());
        // marker + empty constraints → nudge
        let tail = capture_nudge("don't touch storage/btree.rs", &[]).expect("must nudge");
        assert!(
            tail.contains("host note"),
            "nudge must be marked host-owned"
        );
        // marker + covering constraint (shared token "touch"/"btree") → silent
        assert!(
            capture_nudge(
                "don't touch storage/btree.rs",
                &c("do not touch `storage/btree.rs` (deprecated)")
            )
            .is_none()
        );
        // marker + unrelated constraints → nudge
        assert!(capture_nudge("don't touch storage/btree.rs", &c("keep API stable")).is_some());
        // RU markers behave the same
        assert!(capture_nudge("не трогай btree", &[]).is_some());
        assert!(capture_nudge("не трогай btree", &c("btree не трогать")).is_none());
    }

    fn count_summaries(messages: &[Message]) -> usize {
        messages
            .iter()
            .map(|m| m.content.matches("<conversation-summary>").count())
            .sum()
    }

    /// Stage 2 end-to-end (mock model): old turns collapse into one summary
    /// message, the recent tail stays verbatim, tool pairs stay joined.
    #[tokio::test]
    async fn compact_history_summarizes_through_the_model() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
                "MOCK-SUMMARY".into(),
            ))]]),
        });
        let policy = summary_policy();
        let mut messages = three_turns();
        let mut summary = None;
        let out = compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .expect("pressure is over: must compact");
        assert!(out.0 > out.1, "must shrink: {:?}", out);
        assert!(out.2, "model summary must be reported");
        assert_eq!(summary.as_deref(), Some("MOCK-SUMMARY"));
        assert_eq!(count_summaries(&messages), 1);
        assert!(messages[0].content.contains("MOCK-SUMMARY"));
        assert!(
            messages.iter().any(|m| m.content.contains("third task")),
            "recent turns stay verbatim"
        );
    }

    /// A second compaction must not stack a second summary block: the old
    /// one is re-summarized through `<previous-summary>`, not duplicated.
    #[tokio::test]
    async fn compact_history_never_stacks_two_summaries() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::Text("SECOND".into()))],
                vec![Ok(crate::providers::StreamEvent::Text("THIRD".into()))],
            ]),
        });
        let policy = summary_policy();
        let mut messages = three_turns();
        let mut summary = None;
        compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await;
        // force again: history is small now, but the old summary message
        // plus kept tail still exceed keep_turns
        compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            true,
            "",
            None,
        )
        .await;
        assert_eq!(
            count_summaries(&messages),
            1,
            "exactly one summary block may exist"
        );
    }

    /// A failed summary call falls back to the extractive local summary.
    /// (`summarized` stays true: a summary block was prepended, just not by
    /// the model — the event reports the transcript shape, not the author.)
    #[tokio::test]
    async fn compact_history_falls_back_to_local_summary() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(Vec::new()),
        });
        let policy = summary_policy();
        let mut messages = three_turns();
        let mut summary = None;
        let out = compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .expect("pressure is over: must compact");
        assert!(out.2);
        assert_eq!(count_summaries(&messages), 1);
        assert!(
            summary
                .as_deref()
                .unwrap_or_default()
                .contains("## Earlier conversation summary"),
            "unexpected summary: {summary:?}"
        );
    }

    /// A severely over-budget summary gets one retry with an explicit hard
    /// limit; the retry answer wins even though the mock serves it second.
    #[tokio::test]
    async fn compact_history_retries_a_blown_summary_budget() {
        let big = "x".repeat(3 * crate::agent::context::SUMMARY_SHORT_MAX_CHARS);
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::Text(big))],
                vec![Ok(crate::providers::StreamEvent::Text("SHORT".into()))],
            ]),
        });
        let policy = summary_policy();
        let mut messages = three_turns();
        let mut summary = None;
        let out = compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .expect("pressure is over: must compact");
        assert!(out.2);
        assert_eq!(summary.as_deref(), Some("SHORT"));
    }

    /// A moderately over-budget summary (under 2× the cap) is truncated
    /// without spending a retry: the mock has only one answer scripted, so
    /// a retry would fall back to the local summary instead.
    #[tokio::test]
    async fn compact_history_truncates_without_retry_near_the_cap() {
        let over = "y".repeat(
            crate::agent::context::SUMMARY_SHORT_MAX_CHARS
                + crate::agent::context::SUMMARY_SHORT_MAX_CHARS / 2,
        );
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Ok(
                crate::providers::StreamEvent::Text(over),
            )]]),
        });
        let policy = summary_policy();
        let mut messages = three_turns();
        let mut summary = None;
        compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await;
        let kept = summary.expect("a summary must be stored");
        assert!(
            kept.chars().count() <= crate::agent::context::SUMMARY_SHORT_MAX_CHARS,
            "host cap applies: {} chars",
            kept.chars().count()
        );
        assert!(kept.contains("truncated to fit"), "{kept:?}");
    }

    #[tokio::test]
    async fn test_plan_first_gate_blocks_and_allows_mutations() {
        // 1. Single-file write without a plan: proceeds with an advisory
        // nudge (soft discipline) instead of the old refusal
        let blocked_provider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "write",
                        serde_json::json!({
                            "file_path": "new_feature.rs",
                            "content": "pub fn hello() {}"
                        }),
                    ),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
            ]),
        });

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-planfirst-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider: blocked_provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(
                Role::User,
                "implement new authentication feature",
            )],
            root: temp_dir.clone(),
            session_id: "test-plan-first-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: crate::config::PlanConfig {
                plan_first: crate::config::PlanFirstMode::Soft,
                ..Default::default()
            },
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };

        let mut handle = spawn_agent(input);
        let mut saw_tool_notice = false;

        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolNotice { name, ok, .. } => {
                    assert_eq!(name, "write");
                    assert!(ok, "single-file write proceeds with a nudge, not a refusal");
                    saw_tool_notice = true;
                }
                AgentEvent::Completed(Ok(outcome)) => {
                    if let Some(tool_msg) = outcome.messages.iter().find(|m| m.role == Role::Tool) {
                        assert!(
                            tool_msg.content.contains("host nudge"),
                            "advisory must ride the result: {}",
                            tool_msg.content
                        );
                    }
                    break;
                }
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(saw_tool_notice, "should have seen tool notice");
    }

    /// Soft discipline: single-file writes proceed with a nudge whether or
    /// not an acceptance-bearing plan exists. The hard refusal survives
    /// only for multi-file/opaque mutations (covered by
    /// `is_multi_file_mutation` unit tests and the patch refusal below).
    #[tokio::test]
    async fn test_plan_first_gate_nudges_single_file_writes() {
        // fresh filename per session: the read-before-edit guard would
        // otherwise refuse the second write (file exists, never read here)
        // and mask the plan-gate verdict under test
        async fn run_write(root: &std::path::Path, session: &str) -> bool {
            let file = format!("gated-{session}.rs");
            let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
                events: std::sync::Mutex::new(vec![
                    vec![Ok(crate::providers::StreamEvent::ToolCall(
                        crate::providers::ToolCallReq::new(
                            "c1",
                            "write",
                            serde_json::json!({
                                "file_path": file,
                                "content": "pub fn hello() {}"
                            }),
                        ),
                    ))],
                    vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
                ]),
            });
            let temp_dir = root.to_path_buf();
            let input = AgentInput {
                provider,
                model_id: "m".into(),
                model_key: "primary".into(),
                effort: None,
                effort_support: crate::config::EffortSupport::default(),
                max_tokens: None,
                system: vec![],
                messages: vec![Message::new(Role::User, "fix typo")],
                root: temp_dir.clone(),
                session_id: session.into(),
                blocked_patterns: vec![],
                plan_mode: false,
                context_limit: 10000,
                enable_tools: true,
                read_only: false,
                previous_response_id: None,
                summary: None,
                mcp: Default::default(),
                lsp: Default::default(),
                compact_only: false,
                diary: Default::default(),
                memory: Default::default(),
                compaction: Default::default(),
                plan_limits: crate::config::PlanConfig {
                    plan_first: crate::config::PlanFirstMode::Soft,
                    ..Default::default()
                },
                shadow_store: crate::config::ShadowStore::Off,
                subagent_depth: 0,
                parent_step: None,
                parent_session: None,
                fallback_chain: vec![],
            };
            let mut handle = spawn_agent(input);
            let mut wrote = false;
            while let Some(ev) = handle.rx.recv().await {
                match ev {
                    AgentEvent::ToolNotice { name, ok, .. } => {
                        assert_eq!(name, "write");
                        wrote = ok;
                    }
                    AgentEvent::Completed(Ok(_)) => break,
                    AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
            wrote
        }

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-gate-acc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        // soft discipline: a single-file write without any plan proceeds
        // (with a nudge) instead of refusing
        assert!(run_write(&temp_dir, "sess-no-plan").await);
        assert!(
            temp_dir.join("gated-sess-no-plan.rs").exists(),
            "nudged mutation still lands"
        );

        // a plan without acceptance settles nothing — but the write is
        // single-file, so it still proceeds (nudge, not refusal)
        let mut plan = crate::plan::create(
            "goal".to_string(),
            Vec::new(),
            Vec::new(),
            vec![crate::plan::NewStep {
                title: "step".to_string(),
                refs: Vec::new(),
            }],
            0,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&temp_dir, &plan).unwrap();
        assert!(run_write(&temp_dir, "sess-empty-plan").await);

        // any single item opens the gate — even a human one
        plan.acceptance.push(crate::plan::Acceptance {
            text: "manual: eyeball it".to_string(),
            status: crate::plan::AcceptanceStatus::Pending,
            evidence: Vec::new(),
            validation: Default::default(),
            baseline: None,
            snapshot: None,
            shape: None,
            inputs: Vec::new(),
            by: None,
            reason: None,
        });
        crate::plan::store(&temp_dir, &plan).unwrap();
        assert!(run_write(&temp_dir, "sess-with-plan").await);
        assert!(
            temp_dir.join("gated-sess-with-plan.rs").exists(),
            "allowed mutation must land on disk"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// The refusal names the real next step: with no plan at all it says
    /// create; with an active plan that settles nothing it must say
    /// add_acceptance — saying create there ends in plan_exists followed
    /// by a plan-show reassurance loop (seen live in the COMODO session).
    #[test]
    fn plan_required_refusal_names_add_acceptance_for_bare_plan() {
        let empty = std::env::temp_dir().join(format!(
            "sqwai-gate-noplan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        let refusal = plan_required_refusal(&empty);
        assert_eq!(refusal["code"], "plan_required");
        assert!(
            refusal["hint"].as_str().unwrap().contains("plan create"),
            "{refusal}"
        );

        let dir = std::env::temp_dir().join(format!(
            "sqwai-gate-bareplan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let plan = crate::plan::create(
            "bare goal".into(),
            Vec::new(),
            Vec::new(),
            vec![crate::plan::NewStep {
                title: "work".into(),
                refs: Vec::new(),
            }],
            1000,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        crate::plan::store(&dir, &plan).unwrap();
        let refusal = plan_required_refusal(&dir);
        assert_eq!(refusal["code"], "plan_required");
        let hint = refusal["hint"].as_str().unwrap();
        assert!(hint.contains("add_acceptance"), "{refusal}");
        assert!(!hint.contains("plan create"), "must not suggest create: {refusal}");
        assert!(
            refusal["reason"].as_str().unwrap().contains(&plan.id),
            "names the plan to extend: {refusal}"
        );
        let _ = std::fs::remove_dir_all(&empty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hard path survives: a two-file patch without any plan is still
    /// refused with plan_required (the gate fires before git ever runs,
    /// so the fixture needs no real repo state).
    #[tokio::test]
    async fn test_plan_first_gate_still_refuses_multi_file_patch() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "patch",
                        serde_json::json!({
                            "patch": "diff --git a/one.rs b/one.rs\n--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/two.rs b/two.rs\n--- a/two.rs\n+++ b/two.rs\n@@ -1 +1 @@\n-a\n+b\n"
                        }),
                    ),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
            ]),
        });
        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-gate-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "patch two files")],
            root: temp_dir.clone(),
            session_id: "sess-patch-gate".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: crate::config::PlanConfig {
                plan_first: crate::config::PlanFirstMode::Soft,
                ..Default::default()
            },
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };
        let mut handle = spawn_agent(input);
        let mut refused = false;
        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolNotice { name, ok, .. } => {
                    assert_eq!(name, "patch");
                    refused = !ok;
                }
                AgentEvent::Completed(Ok(outcome)) => {
                    if let Some(tool_msg) = outcome.messages.iter().find(|m| m.role == Role::Tool) {
                        assert!(
                            tool_msg.content.contains("plan_required"),
                            "hard refusal keeps its code: {}",
                            tool_msg.content
                        );
                    }
                    break;
                }
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(refused, "multi-file patch without a plan must refuse");
    }

    /// `forbid-cmd:` refuses live in the turn: a forbidden shell command
    /// never executes, with a structured code pointing at the waiver.
    #[tokio::test]
    async fn test_forbid_cmd_refuses_matching_shell_calls() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "bash",
                        serde_json::json!({"command": "rm -rf /tmp/scratch"}),
                    ),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
            ]),
        });
        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-forbid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        let mut plan = crate::plan::create(
            "goal".to_string(),
            vec!["forbid-cmd: rm -rf".to_string()],
            vec!["manual: eyeball it".to_string()],
            vec![crate::plan::NewStep {
                title: "step".to_string(),
                refs: Vec::new(),
            }],
            0,
            &crate::plan::Limits::default(),
        )
        .unwrap();
        plan.sessions = vec!["sess-forbid".to_string()];
        crate::plan::store(&temp_dir, &plan).unwrap();

        let input = AgentInput {
            provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "clean up")],
            root: temp_dir.clone(),
            session_id: "sess-forbid".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: crate::config::PlanConfig {
                plan_first: crate::config::PlanFirstMode::Soft,
                ..Default::default()
            },
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };
        let mut handle = spawn_agent(input);
        let mut refused = false;
        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolNotice { name, ok, .. } => {
                    assert_eq!(name, "bash");
                    assert!(!ok, "forbidden command must not run");
                    refused = true;
                }
                AgentEvent::Completed(Ok(outcome)) => {
                    if let Some(tool_msg) =
                        outcome.messages.iter().find(|m| m.role == Role::Tool)
                    {
                        assert!(
                            tool_msg.content.contains("constraint_violated"),
                            "outcome message: {}",
                            tool_msg.content
                        );
                    }
                    break;
                }
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        assert!(refused, "should have seen the refusal");
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// G0 baseline (§8.2): with the durable machinery off, the plan-first
    /// gate is lifted (there is no plan to require) and mutations run.
    #[tokio::test]
    async fn baseline_arm_skips_plan_gate_and_runs_mutations() {
        struct BaselineGuard;
        impl Drop for BaselineGuard {
            fn drop(&mut self) {
                crate::bench::set_baseline_override(None);
            }
        }
        let _guard = BaselineGuard;
        crate::bench::set_baseline_override(Some(true));

        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "write",
                        serde_json::json!({
                            "file_path": "baseline_feature.rs",
                            "content": "pub fn hello() {}"
                        }),
                    ),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
            ]),
        });

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-baseline-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(
                Role::User,
                "implement new authentication feature",
            )],
            root: temp_dir.clone(),
            session_id: "test-baseline-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: crate::config::PlanConfig {
                plan_first: crate::config::PlanFirstMode::Soft,
                ..Default::default()
            },
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };

        let mut handle = spawn_agent(input);
        let mut saw_tool_notice = false;

        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolNotice { name, ok, .. } => {
                    assert_eq!(name, "write");
                    assert!(ok, "baseline must run the mutation, not gate it");
                    saw_tool_notice = true;
                }
                AgentEvent::Completed(Ok(_)) => break,
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        assert!(saw_tool_notice, "should have seen tool notice");
        assert!(
            temp_dir.join("baseline_feature.rs").exists(),
            "baseline mutation must land on disk"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// #192: an Esc that lands between two tool calls must stop the batch.
    /// The per-request cancel flag is reset before each dispatch; the reset
    /// used to swallow a flag set between calls, so the next call ran
    /// anyway. Whatever the schedule, the second call must never start,
    /// while both calls still get a tool_result (protocol shape).
    #[tokio::test]
    async fn esc_between_tool_calls_stops_the_batch() {
        let provider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![
                    Ok(crate::providers::StreamEvent::ToolCall(
                        crate::providers::ToolCallReq::new(
                            "c1",
                            "think",
                            serde_json::json!({"thought": "one"}),
                        ),
                    )),
                    Ok(crate::providers::StreamEvent::ToolCall(
                        crate::providers::ToolCallReq::new(
                            "c2",
                            "think",
                            serde_json::json!({"thought": "two"}),
                        ),
                    )),
                ],
                vec![Ok(crate::providers::StreamEvent::Text(
                    "should not get here".into(),
                ))],
            ]),
        });

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-esccancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider: provider.clone(),
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            root: temp_dir.clone(),
            session_id: "test-esc-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: Default::default(),
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };

        let mut handle = spawn_agent(input);
        // Esc lands while the batch is dispatching (or just before it).
        handle.request_tool_cancel();
        let mut think_starts = 0;
        let mut outcome_msgs = Vec::new();
        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolStart { name, .. } if name == "think" => {
                    think_starts += 1;
                }
                AgentEvent::Completed(Ok(outcome)) => {
                    outcome_msgs = outcome.messages;
                    break;
                }
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        assert!(
            think_starts <= 1,
            "the second call must never run after Esc, started {think_starts}"
        );
        let results: Vec<&str> = outcome_msgs
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            results.len(),
            2,
            "both calls need a tool_result (protocol shape): {results:?}"
        );
        assert_eq!(
            provider.events.lock().unwrap().len(),
            1,
            "the scripted follow-up turn must stay unconsumed"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// #171: a child spawned for its parent's step joins the parent plan
    /// explicitly, so its evidence attaches under session-strict
    /// resolution instead of relying on the removed silent fallback.
    #[tokio::test]
    async fn subagent_joins_its_parent_plan() {
        let root = std::env::temp_dir().join(format!("sqwai-subjoin-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let mut plan = plan::create(
            "parent goal".into(),
            Vec::new(),
            Vec::new(),
            vec![plan::NewStep {
                title: "step".into(),
                refs: Vec::new(),
            }],
            20_000,
            &plan::Limits::default(),
        )
        .unwrap();
        plan.sessions = vec!["parent-sess".into()];
        plan::store(&root, &plan).unwrap();
        plan::apply(
            &mut plan,
            plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &plan::Limits::default(),
            None,
        )
        .unwrap();
        plan::store(&root, &plan).unwrap();
        let plan_id = plan.id.clone();

        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
                "child done".into(),
            ))]]),
        });
        let (parent_tx, _parent_rx) = mpsc::channel(64);
        let call = crate::providers::ToolCallReq::new(
            "c1",
            "subagent",
            serde_json::json!({"task": "do it"}),
        );
        let outcome = run_subagent(
            &call,
            "parent-sess",
            &parent_tx,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &provider,
            "m",
            &root,
            &[],
            false,
            10_000,
            None,
            crate::config::EffortSupport::default(),
            None,
            Vec::new(),
            crate::config::McpConfig::default(),
            crate::config::LspConfig::default(),
            false,
            crate::config::ShadowStore::Off,
            crate::config::DiaryConfig::default(),
            crate::config::MemoryConfig::default(),
            crate::config::CompactionConfig::default(),
            crate::config::PlanConfig::default(),
            Vec::new(),
            std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);

        let reloaded = plan::open(&root, &plan_id).unwrap();
        let members: Vec<&str> = reloaded.sessions.iter().map(String::as_str).collect();
        assert!(members.contains(&"parent-sess"), "{members:?}");
        assert!(
            members
                .iter()
                .any(|s| s.starts_with("sub-") && *s != "parent-sess"),
            "child joined explicitly: {members:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a subagent finishing its parent's step retired the step
    /// globally while the parent session still held it, so the next `plan
    /// start` in the same turn was refused as "one step at a time".
    /// The parent re-adopts from the plan when the child returns.
    #[test]
    fn adopt_follows_the_plans_in_progress_step() {
        let root = std::env::temp_dir().join(format!("sqwai-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        // no plan at all: idle
        assert_eq!(adopt_in_progress_step(&root, "sess"), None);
        let mut plan = plan::create(
            "goal".into(),
            Vec::new(),
            Vec::new(),
            vec![plan::NewStep {
                title: "step".into(),
                refs: Vec::new(),
            }],
            20_000,
            &plan::Limits::default(),
        )
        .unwrap();
        plan.sessions = vec!["sess".into()];
        plan::store(&root, &plan).unwrap();
        // pending, nothing in progress: idle
        assert_eq!(adopt_in_progress_step(&root, "sess"), None);
        plan::apply(
            &mut plan,
            plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &plan::Limits::default(),
            None,
        )
        .unwrap();
        plan::store(&root, &plan).unwrap();
        assert_eq!(adopt_in_progress_step(&root, "sess"), Some("1".to_string()));
        // the child retires the held step behind the parent's back
        plan.steps[0].status = plan::StepStatus::Done;
        plan::store(&root, &plan).unwrap();
        assert_eq!(adopt_in_progress_step(&root, "sess"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parent session resolves strictly: when it joined no plan, the
    /// child must not inherit the most recent *other* session's plan via
    /// the old global fallback.
    #[tokio::test]
    async fn subagent_ignores_a_foreign_active_plan() {
        let root = std::env::temp_dir().join(format!("sqwai-subforeign-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let mut plan = plan::create(
            "foreign goal".into(),
            Vec::new(),
            Vec::new(),
            vec![plan::NewStep {
                title: "step".into(),
                refs: Vec::new(),
            }],
            20_000,
            &plan::Limits::default(),
        )
        .unwrap();
        plan.sessions = vec!["other-sess".into()];
        plan::store(&root, &plan).unwrap();
        plan::apply(
            &mut plan,
            plan::Op::Start {
                id: "1".into(),
                confirm: None,
            },
            &plan::Limits::default(),
            None,
        )
        .unwrap();
        plan::store(&root, &plan).unwrap();
        let plan_id = plan.id.clone();

        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
                "child done".into(),
            ))]]),
        });
        let (parent_tx, _parent_rx) = mpsc::channel(64);
        let call = crate::providers::ToolCallReq::new(
            "c1",
            "subagent",
            serde_json::json!({"task": "do it"}),
        );
        let outcome = run_subagent(
            &call,
            "lonely-sess",
            &parent_tx,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &provider,
            "m",
            &root,
            &[],
            false,
            10_000,
            None,
            crate::config::EffortSupport::default(),
            None,
            Vec::new(),
            crate::config::McpConfig::default(),
            crate::config::LspConfig::default(),
            false,
            crate::config::ShadowStore::Off,
            crate::config::DiaryConfig::default(),
            crate::config::MemoryConfig::default(),
            crate::config::CompactionConfig::default(),
            crate::config::PlanConfig::default(),
            Vec::new(),
            std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);

        let reloaded = plan::open(&root, &plan_id).unwrap();
        assert_eq!(
            reloaded.sessions,
            vec!["other-sess".to_string()],
            "no foreign join: {:?}",
            reloaded.sessions
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same-turn subagent calls overlap: both ToolStarts land before the
    /// A hung child must not stall the parent turn forever: the timeout
    /// cancels cooperatively, waits out the grace, aborts, and reports.
    #[tokio::test]
    async fn hung_subagent_times_out_and_aborts() {
        struct HangingProvider;
        impl crate::providers::Provider for HangingProvider {
            fn stream_chat(
                &self,
                _req: crate::providers::ChatRequest,
            ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
                use futures::StreamExt;
                futures::stream::pending().boxed()
            }
        }

        let root = std::env::temp_dir().join(format!("sqwai-subtimeout-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let provider: SharedProvider = std::sync::Arc::new(HangingProvider);
        let (parent_tx, _parent_rx) = mpsc::channel(64);
        let call = crate::providers::ToolCallReq::new(
            "c1",
            "subagent",
            serde_json::json!({"task": "hang forever"}),
        );
        let outcome = run_subagent(
            &call,
            "parent-sess",
            &parent_tx,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &provider,
            "m",
            &root,
            &[],
            false,
            10_000,
            None,
            crate::config::EffortSupport::default(),
            None,
            Vec::new(),
            crate::config::McpConfig::default(),
            crate::config::LspConfig::default(),
            false,
            crate::config::ShadowStore::Off,
            crate::config::DiaryConfig::default(),
            crate::config::MemoryConfig::default(),
            crate::config::CompactionConfig::default(),
            crate::config::PlanConfig::default(),
            Vec::new(),
            std::time::Duration::from_millis(150),
        )
        .await;
        assert!(!outcome.ok, "a hung child must fail, not hang");
        assert!(
            outcome.output.contains("timed out"),
            "unexpected outcome: {}",
            outcome.output
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Esc during a subagent wait: the parent flag stops the wait on the
    /// next poll instead of sitting out the whole timeout ("cancelling…"
    /// forever). A pre-set flag with a hanging child must return fast.
    #[tokio::test]
    async fn esc_stops_a_subagent_wait_without_the_timeout() {
        struct HangingProvider;
        impl crate::providers::Provider for HangingProvider {
            fn stream_chat(
                &self,
                _req: crate::providers::ChatRequest,
            ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
                use futures::StreamExt;
                futures::stream::pending().boxed()
            }
        }

        let root = std::env::temp_dir().join(format!("sqwai-subcancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let provider: SharedProvider = std::sync::Arc::new(HangingProvider);
        let (parent_tx, _parent_rx) = mpsc::channel(64);
        let call = crate::providers::ToolCallReq::new(
            "c1",
            "subagent",
            serde_json::json!({"task": "hang forever"}),
        );
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let started = std::time::Instant::now();
        let outcome = run_subagent(
            &call,
            "parent-sess",
            &parent_tx,
            &cancel,
            &provider,
            "m",
            &root,
            &[],
            false,
            10_000,
            None,
            crate::config::EffortSupport::default(),
            None,
            Vec::new(),
            crate::config::McpConfig::default(),
            crate::config::LspConfig::default(),
            false,
            crate::config::ShadowStore::Off,
            crate::config::DiaryConfig::default(),
            crate::config::MemoryConfig::default(),
            crate::config::CompactionConfig::default(),
            crate::config::PlanConfig::default(),
            Vec::new(),
            std::time::Duration::from_secs(60),
        )
        .await;
        assert!(!outcome.ok, "cancelled child must fail");
        assert!(
            outcome.output.contains("cancelled by user"),
            "unexpected outcome: {}",
            outcome.output
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "cancel must not wait out the timeout"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// first ToolNotice (a sequential loop would interleave start/notice
    /// pairs). Rows, journal and messages still come out in call order.
    #[tokio::test]
    async fn same_turn_subagent_calls_overlap() {
        let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![
                    Ok(crate::providers::StreamEvent::ToolCall(
                        crate::providers::ToolCallReq::new(
                            "c1",
                            "subagent",
                            serde_json::json!({"task": "first"}),
                        ),
                    )),
                    Ok(crate::providers::StreamEvent::ToolCall(
                        crate::providers::ToolCallReq::new(
                            "c2",
                            "subagent",
                            serde_json::json!({"task": "second"}),
                        ),
                    )),
                ],
                // one single-text turn per child; interchangeable
                vec![Ok(crate::providers::StreamEvent::Text(
                    "child one done".into(),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text(
                    "child two done".into(),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("all done".into()))],
            ]),
        });

        let temp_dir = std::env::temp_dir().join(format!("sqwai-subbatch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            root: temp_dir.clone(),
            session_id: "test-subbatch-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: Default::default(),
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };

        let mut handle = spawn_agent(input);
        let mut seq: Vec<String> = Vec::new();
        let mut dones = 0;
        let mut results = Vec::new();
        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolStart { call_id, .. } => seq.push(format!("start:{call_id}")),
                AgentEvent::ToolNotice { call_id, .. } => seq.push(format!("notice:{call_id}")),
                AgentEvent::SubagentDone { .. } => dones += 1,
                AgentEvent::Completed(Ok(outcome)) => {
                    results = outcome
                        .messages
                        .iter()
                        .filter(|m| m.role == Role::Tool)
                        .filter_map(|m| m.tool_call_id.clone())
                        .collect();
                    break;
                }
                AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
                _ => {}
            }
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert_eq!(dones, 2, "both children finished: {seq:?}");
        assert!(
            seq.len() >= 4 && seq[0] == "start:c1" && seq[1] == "start:c2",
            "both starts precede any notice: {seq:?}"
        );
        assert_eq!(results.len(), 2, "both calls got results");
        assert!(
            results.contains(&"c1".to_string()) && results.contains(&"c2".to_string()),
            "results address both calls: {results:?}"
        );
    }

    #[tokio::test]
    async fn test_plan_first_gate_allows_when_mode_is_off() {
        let provider = std::sync::Arc::new(MockTestProvider {
            events: std::sync::Mutex::new(vec![
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "write",
                        serde_json::json!({
                            "file_path": "foo.txt",
                            "content": "hello"
                        }),
                    ),
                ))],
                vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
            ]),
        });

        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-planfirst-off-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let input = AgentInput {
            provider,
            model_id: "m".into(),
            model_key: "primary".into(),
            effort: None,
            effort_support: crate::config::EffortSupport::default(),
            max_tokens: None,
            system: vec![],
            messages: vec![Message::new(Role::User, "implement full rewrite")],
            root: temp_dir.clone(),
            session_id: "test-plan-first-off-sess".into(),
            blocked_patterns: vec![],
            plan_mode: false,
            context_limit: 10000,
            enable_tools: true,
            read_only: false,
            previous_response_id: None,
            summary: None,
            mcp: Default::default(),
            lsp: Default::default(),
            compact_only: false,
            diary: Default::default(),
            memory: Default::default(),
            compaction: Default::default(),
            plan_limits: crate::config::PlanConfig {
                plan_first: crate::config::PlanFirstMode::Off,
                ..Default::default()
            },
            shadow_store: crate::config::ShadowStore::Off,
            subagent_depth: 0,
            parent_step: None,
            parent_session: None,
            fallback_chain: vec![],
        };

        let mut handle = spawn_agent(input);
        let mut saw_tool_notice = false;

        while let Some(ev) = handle.rx.recv().await {
            match ev {
                AgentEvent::ToolNotice { name, ok, .. } => {
                    assert_eq!(name, "write");
                    assert!(ok, "mutation should be allowed when plan_first is Off");
                    saw_tool_notice = true;
                }
                AgentEvent::Completed(Ok(_)) => break,
                AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
                _ => {}
            }
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
        assert!(saw_tool_notice, "should have seen tool notice");
    }
}

#[cfg(test)]
mod trust_gate_tests {
    use super::loop_ask::bash_call;
    use super::*;

    fn tainted_project(name: &str) -> (std::path::PathBuf, String) {
        let dir =
            std::env::temp_dir().join(format!("sqwai-trust-gate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = "trust-sess".to_string();
        let mut journal =
            crate::agent::journal::Journal::open(&dir, &session).expect("journal opens");
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "webfetch", "ok": true, "taint": "external"}),
            )
            .unwrap();
        (dir, session)
    }

    fn bash_call_parts(
        dir: &std::path::Path,
        session: &str,
    ) -> (
        ToolCallReq,
        tools::ToolCtx,
        tokio::sync::mpsc::Sender<AgentEvent>,
        tokio::sync::mpsc::Receiver<AgentEvent>,
        tokio::sync::mpsc::Sender<ControlMsg>,
        tokio::sync::mpsc::Receiver<ControlMsg>,
    ) {
        let (tx_agent, rx_ui) = tokio::sync::mpsc::channel(8);
        let (tx_ui, rx_agent) = tokio::sync::mpsc::channel(8);
        let call = ToolCallReq::new(
            "c1",
            "bash",
            serde_json::json!({"command": "git push origin main"}),
        );
        let ctx = tools::ToolCtx::new(dir).in_session(session.to_string());
        (call, ctx, tx_agent, rx_ui, tx_ui, rx_agent)
    }

    /// Headless + external taint + egress: immediate denial, no prompt.
    #[tokio::test]
    async fn trust_gate_denies_egress_for_subagents_under_taint() {
        let (dir, session) = tainted_project("deny");
        let (call, mut ctx, tx_agent, _rx_ui, _tx_ui, mut rx_agent) =
            bash_call_parts(&dir, &session);
        let mut always_allow = Vec::new();
        let mut next_id = 0u64;
        let outcome = bash_call(
            &call,
            &mut ctx,
            &tx_agent,
            &mut rx_agent,
            &mut always_allow,
            &[],
            &mut next_id,
            1,
        )
        .await;
        assert!(!outcome.ok, "must refuse");
        assert!(
            outcome.output.contains("no user to confirm"),
            "{}",
            outcome.output
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Phase 0 probe — safe-bash checkpoint gap: a Safe-classified
    /// mutating command on a clean tree. The hash-gate
    /// (`needs_approval || tree_changed`) skips the pre-snapshot; the
    /// probe pins that AND checks the other half — whether the mutation
    /// stays revertible through the chain head (full-undo path). If both
    /// hold, the gap is benign by design (the head predates the mutation,
    /// so nothing is lost); if the revert fails, the gap is a hole.
    #[tokio::test]
    async fn safe_mutating_bash_on_clean_tree_skips_snapshot_but_stays_revertible() {
        use crate::config::ShadowStore;
        let dir = std::env::temp_dir().join(format!(
            "sqwai-probe-gap-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = "probe-gap";
        // clean tree, established head
        let head = crate::agent::checkpoints::snapshot_session(
            &dir,
            ShadowStore::Local,
            session,
            "probe base",
        )
        .unwrap()
        .expect("first snapshot commits");
        assert!(
            !crate::agent::checkpoints::tree_changed(&dir, ShadowStore::Local, session),
            "tree is clean against its head"
        );
        // Safe-classified, but mutates (verified Safe above this test)
        assert!(matches!(
            crate::agent::safety::classify("echo x > probe.txt"),
            crate::agent::safety::Verdict::Safe
        ));
        let call = crate::providers::ToolCallReq::new(
            "c1",
            "bash",
            serde_json::json!({"command": "echo x > probe.txt"}),
        );
        let (tx_agent, _rx_ui) = tokio::sync::mpsc::channel(8);
        let (_tx_ui, mut rx_agent) = tokio::sync::mpsc::channel(8);
        let mut ctx = tools::ToolCtx::new(&dir).in_session(session.to_string());
        let mut always_allow = Vec::new();
        let mut next_id = 0u64;
        let outcome = bash_call(
            &call,
            &mut ctx,
            &tx_agent,
            &mut rx_agent,
            &mut always_allow,
            &[],
            &mut next_id,
            0,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.output);
        assert!(dir.join("probe.txt").exists(), "command mutated");
        // the gate skipped: no pre_bash snapshot journaled…
        assert!(
            !ctx.journal.iter().any(|(_, label)| label.starts_with("bash ")),
            "no snapshot must be journaled for Safe-on-clean: {:?}",
            ctx.journal
        );
        // …but the mutation stays revertible through the head, which is
        // exactly what full /undo diffs against (unscoped fallback)
        let diff =
            crate::agent::checkpoints::changed_files(&dir, ShadowStore::Local, &head).unwrap();
        assert!(diff.iter().any(|p| p == "probe.txt"), "{diff:?}");
        let report = crate::agent::checkpoints::restore_paths_in(
            &dir,
            ShadowStore::Local,
            &head,
            &[crate::agent::checkpoints::Target {
                path: "probe.txt".to_string(),
                agent_hash: None,
            }],
        )
        .unwrap();
        assert!(!dir.join("probe.txt").exists(), "{report:?}");
        assert_eq!(report.deleted, vec!["probe.txt".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Same command, clean session: no gate involved (git push then fails
    /// on its own — no remote — proving the gate let it through).
    #[tokio::test]
    async fn trust_gate_stays_quiet_without_taint() {
        let dir = std::env::temp_dir().join(format!("sqwai-trust-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (call, mut ctx, tx_agent, _rx_ui, _tx_ui, mut rx_agent) =
            bash_call_parts(&dir, "clean-sess");
        let mut always_allow = Vec::new();
        let mut next_id = 0u64;
        let outcome = bash_call(
            &call,
            &mut ctx,
            &tx_agent,
            &mut rx_agent,
            &mut always_allow,
            &[],
            &mut next_id,
            0,
        )
        .await;
        assert!(
            !outcome.output.contains("no user to confirm"),
            "{}",
            outcome.output
        );
        assert!(
            !outcome.output.contains("external content"),
            "{}",
            outcome.output
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Tainted main session: the approval dialog carries the trust reason,
    /// and a denial stops the command.
    #[tokio::test]
    async fn trust_gate_prompts_with_reason_and_honors_deny() {
        let (dir, session) = tainted_project("prompt");
        let (call, mut ctx, tx_agent, mut rx_ui, tx_ui, mut rx_agent) =
            bash_call_parts(&dir, &session);
        let mut always_allow = Vec::new();
        let mut next_id = 0u64;
        let future = bash_call(
            &call,
            &mut ctx,
            &tx_agent,
            &mut rx_agent,
            &mut always_allow,
            &[],
            &mut next_id,
            0,
        );
        tokio::pin!(future);
        let outcome = loop {
            tokio::select! {
                out = &mut future => break out,
                ev = rx_ui.recv() => {
                    if let Some(AgentEvent::Approval { id, reason, .. }) = ev {
                        assert!(
                            reason.contains("external content"),
                            "trust reason missing: {reason}"
                        );
                        tx_ui
                            .send(ControlMsg::ApprovalAnswer {
                                id,
                                decision: ApprovalDecision::Deny,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        };
        assert!(!outcome.ok, "denied command must fail");
        assert!(
            outcome.output.contains("denied by user"),
            "{}",
            outcome.output
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
