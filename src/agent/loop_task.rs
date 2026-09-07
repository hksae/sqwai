//! The agent loop (phase 2): drives LLM turns plus tool execution until the
//! model produces a final text answer with no more tool calls.
//!
//! Runs in its own tokio task, publishing [`AgentEvent`]s to the TUI and
//! receiving user interaction answers (ask_user, dangerous-command approval)
//! back through the [`ControlMsg`] channel. Aborting the task stops the agent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::Digest;
use tokio::sync::mpsc;

use crate::config::EffortLevel;
use crate::providers::{
    ChatRequest, ContextTransport, Message, RequestBreakdown, Role, SharedProvider, StreamEvent,
    SystemPart, ToolCallReq, Usage,
};

use crate::agent::context;
use crate::agent::tools::{self, ToolCtx};
use crate::agent::{checkpoints, safety};
use crate::plan;

/// Output token cap for the summarization request. It only has to be long
/// enough for a dense summary; anything more wastes the context we just freed.
const SUMMARY_MAX_TOKENS: u32 = 2_048;

#[derive(Debug, Clone)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
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
    /// a tool just started: name + short arguments, spinner in the TUI
    ToolStart {
        name: String,
        summary: String,
    },
    /// a tool finished (ok=True/False); carries the unified diff for mutations
    ToolNotice {
        name: String,
        summary: String,
        ok: bool,
        diff: Option<String>,
    },
    /// the model asked the user a structured question; answer via ControlMsg
    AskUser {
        id: u64,
        question: String,
        options: Vec<AskOption>,
        multiple: bool,
        allow_free: bool,
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
    Completed(Result<AgentOutcome, String>),
}

#[derive(Debug)]
pub enum ControlMsg {
    AskAnswer { id: u64, text: String },
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
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub struct AgentInput {
    pub provider: SharedProvider,
    pub model_id: String,
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
    /// nesting guard for delegated subagents; the first generation may create
    /// children, but children cannot recursively create more children.
    pub subagent_depth: u8,
}

const RETRY_WINDOW: Duration = Duration::from_secs(3600);

fn backoff(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        4 => 15,
        5 => 30,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// Which part of the local transcript goes on the wire.
///
/// Only a documented continuation reference shortens it: the provider already
/// holds those turns, and resending them would duplicate the remote history.
/// The system block travels separately and is always sent.
/// What a request carries under each transport.
///
/// Continuing from a previous response means the provider already holds
/// everything up to and including its own last message, so we send only what
/// it has not seen: the trailing run of tool results and user turns after the
/// last assistant message.
///
/// This used to send "the last user message", which is right for a plain
/// exchange and wrong the moment tools are in play: a tool result is
/// `Role::Tool`, so the run of results the model is waiting for was dropped
/// and the turn continued as if the tools had never been called.
/// Transport for one request.
///
/// Continuation is dropped whenever the request answers a tool call: the
/// matching `function_call` items live in the provider's chain, and a provider
/// that does not really keep that chain rejects the outputs outright. Sending
/// the transcript ourselves costs a few hundred bytes and cannot fail that
/// way. `capabilities()` describes the wire format, not the endpoint behind
/// it, so it cannot answer this on its own — relays accept the field and
/// ignore it.
fn turn_transport(
    caps: crate::providers::ProviderCapabilities,
    previous_response_id: Option<&str>,
    messages: &[Message],
    continuation_usable: bool,
) -> ContextTransport {
    let answering_a_call = messages.last().is_some_and(|m| m.role == Role::Tool);
    if !continuation_usable || answering_a_call {
        let mut caps = caps;
        caps.previous_response = false;
        return crate::providers::select_transport(caps, None);
    }
    crate::providers::select_transport(caps, previous_response_id)
}

fn request_messages(messages: &[Message], transport: ContextTransport) -> Vec<Message> {
    if transport != ContextTransport::PreviousResponse {
        return messages.to_vec();
    }
    let unseen = messages
        .iter()
        .rev()
        .take_while(|m| matches!(m.role, Role::Tool | Role::User))
        .count();
    messages[messages.len() - unseen..].to_vec()
}

struct TurnOutcome {
    text: String,
    calls: Vec<ToolCallReq>,
    /// how many retries it took to get this answer, for the journal
    retries: u32,
    /// reasoning tokens the provider reported for this turn, when it counts
    /// them at all. `Some(0)` is evidence that a requested effort level was
    /// not acted on; `None` says nothing either way.
    reasoning_tokens: Option<u64>,
    /// reasoning content was streamed in this turn, which contradicts a zero
    /// count whatever the usage block says
    saw_reasoning: bool,
    /// the provider rejected the effort parameter outright, so the turn was
    /// retried without it and the session must stop sending it
    effort_rejected: bool,
}

const MAX_SUBAGENTS_PER_CALL: usize = 8;
const MAX_PARALLEL_SUBAGENTS: usize = 4;

fn subagent_tasks_from_args(args: &serde_json::Value) -> Result<Vec<String>, String> {
    let mut tasks: Vec<String> = args["tasks"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|task| !task.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if tasks.is_empty()
        && let Some(task) = args["task"]
            .as_str()
            .map(str::trim)
            .filter(|task| !task.is_empty())
    {
        tasks.push(task.to_string());
    }
    if tasks.is_empty() {
        return Err("subagent task is required".into());
    }
    if tasks.len() > MAX_SUBAGENTS_PER_CALL {
        return Err(format!(
            "too many subagents: maximum is {MAX_SUBAGENTS_PER_CALL}"
        ));
    }
    Ok(tasks)
}

#[allow(clippy::too_many_arguments)] // all parameters are required for subagent configuration
async fn run_subagent(
    call: &ToolCallReq,
    parent_tx: &mpsc::Sender<AgentEvent>,
    provider: &SharedProvider,
    model_id: &str,
    root: &Path,
    blocked_patterns: &[String],
    plan_mode: bool,
    context_limit: u64,
    effort: Option<EffortLevel>,
    effort_support: crate::config::EffortSupport,
    max_tokens: Option<u32>,
    system: Vec<SystemPart>,
    mcp: crate::config::McpConfig,
    lsp: crate::config::LspConfig,
    read_only: bool,
) -> tools::Outcome {
    let tasks = match subagent_tasks_from_args(&call.args) {
        Ok(tasks) => tasks,
        Err(error) => return tools::Outcome::err(error),
    };
    if tasks.len() > 1 {
        use futures::{StreamExt, stream};
        let outcomes = stream::iter(tasks.into_iter().enumerate())
            .map(|(index, task)| {
                let mut one = call.clone();
                one.args = serde_json::json!({"task": task});
                let system = system.clone();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
                async move {
                    let outcome = run_subagent(
                        &one,
                        parent_tx,
                        provider,
                        model_id,
                        root,
                        blocked_patterns,
                        plan_mode,
                        context_limit,
                        effort,
                        effort_support,
                        max_tokens,
                        system.clone(),
                        mcp.clone(),
                        lsp.clone(),
                        read_only,
                    )
                    .await;
                    (
                        index,
                        one.args["task"].as_str().unwrap_or_default().to_string(),
                        outcome,
                    )
                }
            })
            .buffer_unordered(MAX_PARALLEL_SUBAGENTS)
            .collect::<Vec<_>>()
            .await;
        let mut outcomes = outcomes;
        outcomes.sort_by_key(|(index, _, _)| *index);
        let all_ok = outcomes.iter().all(|(_, _, outcome)| outcome.ok);
        let output = outcomes
            .into_iter()
            .map(|(_, task, outcome)| format!("## {task}\n{}", outcome.output))
            .collect::<Vec<_>>()
            .join("\n\n");
        return if all_ok {
            tools::Outcome::ok(output)
        } else {
            tools::Outcome::err(output)
        };
    }
    let task = tasks.into_iter().next().unwrap();
    let id = next_subagent_id();
    let _ = parent_tx
        .send(AgentEvent::SubagentStart {
            id,
            task: task.clone(),
        })
        .await;
    let child = spawn_agent(AgentInput {
        provider: provider.clone(),
        model_id: model_id.to_string(),
        effort,
        effort_support,
        max_tokens,
        system,
        messages: vec![Message::new(Role::User, task)],
        root: root.to_path_buf(),
        session_id: format!("sub-{id}"),
        blocked_patterns: blocked_patterns.to_vec(),
        plan_mode,
        context_limit,
        enable_tools: true,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp,
        lsp,
        compact_only: false,
        diary: crate::config::DiaryConfig::default(),
        memory: crate::config::MemoryConfig::default(),
        compaction: crate::config::CompactionConfig::default(),
        plan_limits: crate::config::PlanConfig::default(),
        subagent_depth: 1,
    });
    let mut child = child;
    let mut output = String::new();
    while let Some(event) = child.rx.recv().await {
        match event {
            AgentEvent::TextDelta(text) => {
                output.push_str(&text);
                let _ = parent_tx.send(AgentEvent::SubagentText { id, text }).await;
            }
            AgentEvent::ThinkingDelta(text) => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentThinking { id, text })
                    .await;
            }
            AgentEvent::ToolStart { name, summary } => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentToolStart { id, name, summary })
                    .await;
            }
            AgentEvent::ToolNotice {
                name,
                summary,
                ok,
                diff,
            } => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentToolDone {
                        id,
                        name,
                        summary,
                        ok,
                        diff,
                    })
                    .await;
            }
            AgentEvent::Completed(result) => {
                let result = match result {
                    Ok(outcome) => {
                        if output.is_empty() {
                            output = outcome
                                .messages
                                .iter()
                                .rev()
                                .find(|m| m.role == Role::Assistant)
                                .map(|m| m.content.clone())
                                .unwrap_or_default();
                        }
                        tools::Outcome::ok(output.clone())
                    }
                    Err(error) => tools::Outcome::err(error),
                };
                let _ = parent_tx
                    .send(AgentEvent::SubagentDone {
                        id,
                        ok: result.ok,
                        output: result.output.clone(),
                    })
                    .await;
                return result;
            }
            _ => {}
        }
    }
    let result = tools::Outcome::err("subagent disconnected");
    let _ = parent_tx
        .send(AgentEvent::SubagentDone {
            id,
            ok: false,
            output: result.output.clone(),
        })
        .await;
    result
}

fn next_subagent_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
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

async fn run_agent(
    input: AgentInput,
    tx: mpsc::Sender<AgentEvent>,
    mut ctl: mpsc::Receiver<ControlMsg>,
    cancel_tool: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let AgentInput {
        provider,
        model_id,
        mut effort,
        effort_support,
        max_tokens,
        system,
        mut messages,
        root,
        session_id,
        blocked_patterns,
        plan_mode,
        context_limit,
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
        subagent_depth,
    } = input;

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

    let caps = provider.capabilities();
    let policy = context::Policy::with_compaction(
        context_limit,
        compaction.anchor_ratio,
        compaction.keep_turns,
        compaction.stage_ratio,
        matches!(compaction.summary, crate::config::CompactionSummary::Short),
    );
    // Tools are part of the request prefix: sorted for stability, narrowed in
    // PLAN mode, and omitted entirely for requests that cannot call them.
    let tools: Vec<crate::providers::ToolSpec> = if enable_tools {
        let mut specs = tools::tool_specs(plan_mode);
        if let Some(registry) = &mcp_registry {
            specs.extend_from_slice(registry.specs());
        }
        specs
    } else {
        Vec::new()
    };
    // A continuation reference is only usable while the provider is known to
    // honour it; the first request that proves otherwise turns it off for the
    // rest of the session.
    let mut continuation_usable = true;

    // `/compact` — write the mandatory pre-compaction diary entry first, then
    // run the policy and hand the transcript back without a chat turn.
    if compact_only {
        let _ = crate::agent::diary::write_entry(
            &root,
            crate::agent::diary::today(),
            &session_id,
            "compaction",
            Some(&provider),
            &model_id,
            plan::open_active(&root)
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
        let mut compaction_journal = if !read_only {
            crate::agent::journal::Journal::open(&root, &session_id).ok()
        } else {
            None
        };
        if let Some(writer) = compaction_journal.as_mut() {
            writer.set_attribution(
                None,
                plan::open_active(&root).ok().flatten().map(|plan| plan.id),
                "main",
            );
            let _ = writer.append("compaction", serde_json::json!({"phase": "begin"}));
        }
        let message_count_before = messages.len();
        let outcome = compact_history(
            &provider,
            &model_id,
            &mut messages,
            &mut summary,
            &policy,
            0,
            true,
        )
        .await;
        if let Some((_, _, summarized)) = outcome.as_ref()
            && let Some(writer) = compaction_journal.as_mut()
        {
            let _ = writer.append(
                "compaction",
                serde_json::json!({
                    "phase": "end",
                    "dropped_msgs": message_count_before.saturating_sub(messages.len()),
                    "kept_msgs": messages.len(),
                    "anchor_tokens": context::anchor(&root, &session_id).len().div_ceil(4),
                    "diary_written": true,
                    "summarized": summarized,
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
        .with_plan_limits(plan_limits, context_limit)
        .with_cancel(cancel_tool);
    let mut journal = if enable_tools && !read_only {
        crate::agent::journal::Journal::open(&root, &session_id).ok()
    } else {
        None
    };
    if let Some(writer) = journal.as_mut() {
        let plan_id = plan::open_active(&root).ok().flatten().map(|p| p.id);
        writer.set_attribution(None, plan_id, "main");
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
        if let Some(user_message) = messages.iter().rev().find(|m| m.role == Role::User) {
            let _ = writer.append("user_msg", serde_json::json!({
                "hash": format!("{:x}", sha2::Sha256::digest(user_message.content.as_bytes())),
                "chars": user_message.content.chars().count(),
                "goal_like": user_message.content.starts_with("goal:") || user_message.content.starts_with("/goal"),
            }));
        }
    }
    let todos: Vec<String> = Vec::new();
    let mut plan_todos: Vec<String> = plan::open_active(&root)
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
        if messages.len() > 8
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
                plan::open_active(&root)
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
        // Compaction gate. The provider's own prompt size is the honest
        // measurement — it includes the system block and the tool schemas the
        // estimate cannot see.
        if let Some((before, after, summarized)) = compact_history(
            &provider,
            &model_id,
            &mut messages,
            &mut summary,
            &policy,
            prompt_size,
            false,
        )
        .await
        {
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
            crate::agent::journal::Journal::nudge(&root, plan_limits.nudge_after)
        {
            turn_system.push(crate::providers::SystemPart::volatile(nudge));
        }
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
                    if let Some((before, after, summarized)) = compact_history(
                        &provider,
                        &model_id,
                        &mut messages,
                        &mut summary,
                        &policy,
                        prompt_size,
                        true,
                    )
                    .await
                    {
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
                let _ = tx.send(AgentEvent::Completed(Err(failure.message))).await;
                break;
            }
        };
        compacted_for_overflow = false;

        if turn.calls.is_empty() {
            // final answer
            messages.push(Message::new(Role::Assistant, turn.text));
            break;
        }

        // assistant requested tools; record the call(s)
        messages.push(Message::new(Role::Assistant, turn.text).with_tool_calls(turn.calls.clone()));

        // §3.7 / §7 S: once the user cancels one call, no further calls in
        // this batch run and no further model turns are requested — Esc means
        // stop, not "let the model decide what to do about it".
        let mut interrupted = false;

        // execute each call, feeding results back into the conversation
        for call in &turn.calls {
            // A cancellation from the previous call must not leak into this
            // one: the flag is per-request, reset right before dispatch.
            ctx.cancel
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let journal_mark = ctx.journal.len();
            let tool_started = Instant::now();
            if let Some(writer) = journal.as_mut() {
                let active = plan::open_active(&root).ok().flatten();
                let plan_id = active.as_ref().map(|p| p.id.clone());
                let step = call
                    .args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| {
                        if call.name == "plan"
                            && call.args.get("op").and_then(|v| v.as_str()) == Some("verify")
                        {
                            return active.as_ref().and_then(|p| {
                                p.steps
                                    .iter()
                                    .find(|s| s.kind == plan::StepKind::Verify)
                                    .map(|s| s.id.clone())
                            });
                        }
                        active.as_ref().and_then(|p| {
                            p.steps
                                .iter()
                                .find(|s| s.status == plan::StepStatus::InProgress)
                                .map(|s| s.id.clone())
                        })
                    });
                writer.set_attribution(step, plan_id, "main");
                let _ = writer.append(
                    "tool_call",
                    serde_json::json!({
                        "tool": call.name,
                        "call_id": call.id,
                        "args_digest": tools::call_summary(&call.name, &call.args),
                    }),
                );
            }
            // live row first: the TUI shows the tool name and its arguments
            // with a spinner while it runs (design §10)
            let _ = tx
                .send(AgentEvent::ToolStart {
                    name: call.name.clone(),
                    summary: tools::call_summary(&call.name, &call.args),
                })
                .await;

            let mut outcome = if read_only && tools::is_mutating_call(&call.name, &call.args) {
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
            } else {
                match call.name.as_str() {
                    "ask_user" => ask_user(call, &tx, &mut ctl, &mut next_id).await,
                    "bash" => {
                        bash_call(
                            call,
                            &mut ctx,
                            &tx,
                            &mut ctl,
                            &mut always_allow,
                            &blocked_patterns,
                            &mut next_id,
                        )
                        .await
                    }
                    "webfetch" => tools::web::fetch(&call.args).await,
                    "websearch" => tools::web::search(&call.args).await,
                    "subagent" if subagent_depth == 0 => {
                        run_subagent(
                            call,
                            &tx,
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
                        )
                        .await
                    }
                    "subagent" => tools::Outcome::err("nested subagents are not allowed"),
                    "memory_propose" => {
                        memory_proposals_this_turn = memory_proposals_this_turn.saturating_add(1);
                        if memory_proposals_this_turn > memory.max_proposals_per_turn {
                            tools::Outcome::err("memory proposal limit reached for this turn")
                        } else {
                            let proposal = tools::execute(&mut ctx, "memory_propose", &call.args);
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
                                let answer = ask_user(&question, &tx, &mut ctl, &mut next_id).await;
                                let answer_text = answer.output.trim().to_ascii_lowercase();
                                if answer.ok && answer_text == "accept" {
                                    let scope = crate::agent::memory::Scope::parse(
                                        call.args["scope"].as_str().unwrap_or("project"),
                                    );
                                    match scope.and_then(|scope| {
                                        crate::agent::memory::apply_proposal(
                                            &root,
                                            scope,
                                            call.args["section"].as_str().unwrap_or("Project"),
                                            call.args["text"].as_str().unwrap_or_default(),
                                            call.args["replaces"].as_str(),
                                            &session_id,
                                            memory.max_tokens,
                                        )
                                        .map(|path| format!("memory written: {}", path.display()))
                                        .map_err(|error| error.to_string())
                                    }) {
                                        Ok(output) => tools::Outcome::ok(output),
                                        Err(error) => tools::Outcome::err(error),
                                    }
                                } else if answer.ok && answer_text == "reject" {
                                    tools::Outcome::ok("memory proposal rejected")
                                } else if answer.ok && answer_text == "edit" {
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
                            && let Ok(Some(saved)) = plan::open_active(&root)
                        {
                            plan_todos = saved
                                .steps
                                .iter()
                                .map(|step| format!("[{}] {}", step.status.as_str(), step.title))
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
                                output,
                                ok: !is_error,
                                diff: None,
                                file_diff: None,
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
            if outcome.cancelled
                && call.name == "bash"
                && let Ok(Some(sha)) = checkpoints::snapshot_session(
                    &ctx.root,
                    crate::config::ShadowStore::Local,
                    &ctx.session_id,
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
                        outcome.ok = false;
                    }
                }
            }

            let _ = tx
                .send(AgentEvent::ToolNotice {
                    name: call.name.clone(),
                    summary: outcome.output.clone(),
                    ok: outcome.ok,
                    diff: outcome.diff.clone(),
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
                let result_seq = writer
                    .append_evidence(
                        "tool_result",
                        serde_json::json!({
                            "tool": call.name,
                            "call_id": call.id,
                            "ok": outcome.ok,
                            "duration_ms": tool_started.elapsed().as_millis(),
                            "summary": outcome.output.chars().take(200).collect::<String>(),
                            "trust": if matches!(call.name.as_str(), "webfetch" | "websearch") { "low" } else { "high" },
                            // §3.7: distinct from an ordinary failure, so the
                            // journal can say the user stopped this rather
                            // than that it went wrong on its own
                            "code": if outcome.cancelled { Some("cancelled") } else { None },
                        }),
                    )
                    .ok();
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
                if let Some(metadata) = outcome.file_diff.as_ref() {
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
                }
                if call.name == "plan" {
                    let op = call
                        .args
                        .get("op")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let _ = writer.append(
                        "plan",
                        serde_json::json!({
                            "op": op,
                            "id": call.args.get("id").and_then(|value| value.as_str()),
                            "ok": outcome.ok,
                        }),
                    );
                    if outcome.ok && matches!(op, "finish" | "block" | "cancel") {
                        let _ = crate::agent::diary::write_entry(
                            &root,
                            crate::agent::diary::today(),
                            &session_id,
                            "step_lifecycle",
                            Some(&provider),
                            &model_id,
                            plan::open_active(&root)
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
            messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
            if interrupted {
                // §3.7: Esc means stop — the remaining calls in this batch
                // (if the model requested several) do not run.
                break;
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

/// Run the compaction policy, cheapest stage first.
///
/// 1. prune aged-out tool output (no LLM);
/// 2. summarize the oldest turns through the model;
/// 3. if the summary could not be obtained, summarize locally;
/// 4. hard-trim if the history still does not fit.
///
/// Returns `(before, after, summarized_by_the_model)` when something changed.
/// A failure never propagates: stage 4 always leaves a usable transcript.
async fn compact_history(
    provider: &SharedProvider,
    model_id: &str,
    messages: &mut Vec<Message>,
    summary: &mut Option<String>,
    policy: &context::Policy,
    observed_prompt: u64,
    force: bool,
) -> Option<(u64, u64, bool)> {
    let measured = |m: &[Message]| observed_prompt.max(context::estimated_tokens(m));
    let before = measured(messages);

    // stage 1: prune. Cheap, lossless in structure, runs every turn.
    let (pruned, pruned_changed) = context::prune(messages);
    if pruned_changed {
        *messages = pruned;
    }
    if !force && policy.pressure(measured(messages)) == context::Pressure::Ok {
        // Pressure is fine, so no summarization or hard trim will run. Stage-1
        // `prune` may have shrunk the chat history, but that never moves
        // `measured`: it is pinned to `observed_prompt` (the provider's full
        // prompt size of the *previous* request), which prune cannot change.
        // Emitting "trimmed: 12k → 12k tok" here would be a no-op that reads as
        // if context compressed when it did not. Prune is lossy, but its effect
        // is already visible inline via PRUNE_NOTE on the trimmed tool result,
        // so stay silent instead of lying about the token count.
        return None;
    }

    // stage 2: summarize. The cut is safe by construction, so an assistant
    // tool call can never be separated from its results.
    let (older, keep) = context::split_for_summary_with_keep(messages, policy.keep_turns());
    let older: Vec<Message> = older.to_vec();
    let keep: Vec<Message> = keep.to_vec();
    let mut summarized = false;
    if !older.is_empty() && policy.summary_enabled {
        let request = ChatRequest {
            model_id: model_id.to_string(),
            system: vec![SystemPart::volatile(context::SUMMARY_SYSTEM)],
            messages: vec![Message::new(
                Role::User,
                context::summary_input(&older, summary.as_deref()),
            )],
            effort: None,
            effort_support: Default::default(),
            max_tokens: Some(SUMMARY_MAX_TOKENS),
            // a summarization request needs no tools
            tools: Vec::new(),
            previous_response_id: None,
            context_transport: ContextTransport::Stateless,
        };
        let text = match collect_text(provider, &request).await {
            Ok(text) => text,
            Err(e) => {
                crate::providers::log_http(&format!("compaction: summarization failed: {e}"));
                // stage 3: extract a summary locally instead of losing the turns
                context::local_summary(&older, summary.as_deref())
            }
        };
        *messages = context::apply_summary(&text, &keep);
        *summary = Some(text);
        summarized = true;
    }

    // stage 4: still too big — drop the oldest turns outright
    if policy.pressure(measured(messages)) != context::Pressure::Ok {
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
}

impl TurnFailure {
    fn new(
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

    fn continuation_rejected(message: impl Into<String>, retries: u32) -> Self {
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
const MIN_ZERO_TURNS_BEFORE_REPORTING: u32 = 2;

/// True when this turn asked for effort and came back with no reasoning at all.
fn turn_shows_no_reasoning(turn: &TurnOutcome) -> bool {
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
fn effort_ignored_reason(turn: &TurnOutcome, consecutive_zero_turns: u32) -> Option<String> {
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
fn rejects_continuation(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("no tool call found for function call output")
        || (lower.contains("previous_response") && !lower.contains("not supported"))
        || lower.contains("previous response not found")
}

fn rejects_effort_parameter(err: &str) -> bool {
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

async fn run_turn(
    provider: &SharedProvider,
    req: &ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
    _ctl: &mut mpsc::Receiver<ControlMsg>,
    response_id: &mut Option<String>,
    prompt_size: &mut u64,
) -> Result<TurnOutcome, TurnFailure> {
    use futures::StreamExt;

    let mut attempt: u32 = 0;
    let mut deadline: Option<Instant> = None;
    // The request can lose its effort parameter mid-flight (see below), so the
    // loop works on its own copy rather than on the caller's.
    let mut req = req.clone();
    // what the user asked for, kept separately: the parameter can be stripped
    // mid-flight, and a turn that only succeeded without it is exactly the
    // turn worth reporting
    let requested_effort = req.effort;
    let mut effort_rejected = false;

    loop {
        let mut reasoning_tokens: Option<u64> = None;
        // reasoning content actually arrived in this turn
        let mut saw_reasoning = false;
        let mut got_delta = false;
        let mut failed: Option<anyhow::Error> = None;
        let mut text = String::new();
        let mut calls: Vec<ToolCallReq> = Vec::new();

        let mut stream = provider.stream_chat(req.clone());
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::Text(t)) => {
                    if !t.is_empty() {
                        got_delta = true;
                        text.push_str(&t);
                        if tx.send(AgentEvent::TextDelta(t)).await.is_err() {
                            return Err(TurnFailure::new("tui closed", None, attempt));
                        }
                    }
                }
                Ok(StreamEvent::Reasoning(t)) => {
                    if !t.is_empty() {
                        got_delta = true;
                        saw_reasoning = true;
                        if tx.send(AgentEvent::ThinkingDelta(t)).await.is_err() {
                            return Err(TurnFailure::new("tui closed", None, attempt));
                        }
                    }
                }
                Ok(StreamEvent::Usage(u)) => {
                    // prompt size of *this* request: replaces, never accumulates.
                    // Some providers send a second output-only event with zero
                    // input — that must not reset the meter.
                    if u.prompt_tokens > 0 {
                        *prompt_size = u.prompt_tokens;
                    }
                    // Same hazard as the line above, and the one that bit us:
                    // a gateway can send a second usage event whose
                    // `completion_tokens_details` is a stub of zeros. Taking
                    // the last value let a real count decay to 0, and the host
                    // then reported the level as ignored. These counters only
                    // grow, so keep the largest one seen.
                    if let Some(n) = u.reasoning_tokens {
                        reasoning_tokens = Some(reasoning_tokens.unwrap_or(0).max(n));
                    }
                    // no-op unless the http log is on; this is the line that
                    // makes an inconsistent gateway diagnosable at all
                    crate::providers::log_http(&format!(
                        "usage event: prompt={} completion={} cached={:?} reasoning={:?}",
                        u.prompt_tokens, u.completion_tokens, u.cached_tokens, u.reasoning_tokens
                    ));
                    if tx.send(AgentEvent::Usage(u)).await.is_err() {
                        return Err(TurnFailure::new("tui closed", None, attempt));
                    }
                }
                Ok(StreamEvent::ResponseId(id)) => {
                    // A continuation chain moves forward: the next iteration
                    // must reference this response, not the one before it.
                    *response_id = Some(id.clone());
                    let _ = tx.send(AgentEvent::ResponseId(id)).await;
                }
                Ok(StreamEvent::ToolCall(tc)) => {
                    // only tool-capable requests may schedule tools; a call
                    // emitted for a tools-less request is ignored
                    if req.tool_capable() {
                        calls.push(tc);
                    }
                }
                Err(e) => failed = Some(e),
            }
            if failed.is_some() {
                break;
            }
        }

        let Some(error) = failed else {
            // The one line that makes the effort machinery inspectable: what
            // was asked for, what went on the wire, and what the provider
            // counted. Without it, "the level was honoured" and "the provider
            // says nothing about reasoning" look identical from outside.
            if let Some(level) = requested_effort {
                let sent = match req.effort {
                    Some(still) => format!(
                        "{:?}",
                        crate::providers::effort::plan(still, req.effort_support).wire
                    ),
                    // stripped by the rejection retry above
                    None => "nothing (parameter refused by the provider)".to_string(),
                };
                crate::providers::log_http(&format!(
                    "turn: effort requested={} sent={} reasoning_tokens={}",
                    level.as_str(),
                    sent,
                    match reasoning_tokens {
                        Some(n) => n.to_string(),
                        None => "not reported".into(),
                    }
                ));
            }
            return Ok(TurnOutcome {
                text,
                calls,
                reasoning_tokens,
                saw_reasoning,
                effort_rejected,
                retries: attempt,
            });
        };
        let class = crate::providers::class_of(&error);
        let err = format!("{error:#}");

        // A provider that took a continuation reference and cannot resolve it
        // leaves the caller a way out: resend the transcript. run_turn cannot
        // do that itself — the request it holds was already shortened — so it
        // reports the cause and stops.
        if req.previous_response_id.is_some() && rejects_continuation(&err) {
            crate::providers::log_http(&format!(
                "provider refused the continuation reference: {err}"
            ));
            return Err(TurnFailure::continuation_rejected(err, attempt));
        }

        // A gateway that refuses the reasoning parameter is telling us the
        // model's declared support is wrong. Retrying the same body cannot
        // help, but retrying without the parameter can — and the caller then
        // stops sending it for the rest of the session. This is checked before
        // the class, because the rejection arrives as an ordinary 400 that
        // would otherwise end the turn.
        if req.effort.is_some() && rejects_effort_parameter(&err) {
            crate::providers::log_http(&format!(
                "provider rejected the effort parameter, retrying without it: {err}"
            ));
            req.effort = None;
            effort_rejected = true;
            attempt += 1;
            continue;
        }

        // Decide from the class, not from the prose. Retrying an expired key
        // or an exhausted quota for an hour is as wrong as giving up on a 503.
        if let Some(class) = class {
            use crate::providers::ErrorClass;
            if !class.retryable() {
                // ContextOverflow lands here too: asking the same oversized
                // request again cannot help, so the caller compacts and runs
                // the turn once more.
                let advice = match class {
                    ErrorClass::Auth => " — check the API key for this provider",
                    ErrorClass::Quota => " — the account is out of quota or credit",
                    _ => "",
                };
                return Err(TurnFailure::new(
                    format!("{err}{advice}"),
                    Some(class),
                    attempt,
                ));
            }
        } else if err.contains("provider returned 400 Bad Request")
            || err.contains("invalid_request_error")
        {
            // Unclassified, but recognisably deterministic: a gateway that
            // answers 200 with an error body lands here.
            return Err(TurnFailure::new(err, None, attempt));
        }

        if got_delta {
            // partial answer already streamed; a retry would duplicate it
            return Err(TurnFailure::new(
                format!("{err} — partial answer kept, not retried"),
                class,
                attempt,
            ));
        }

        let now = Instant::now();
        let dl = *deadline.get_or_insert(now + RETRY_WINDOW);
        if now >= dl {
            return Err(TurnFailure::new(
                format!("{err} — giving up after 1h of retries"),
                class,
                attempt,
            ));
        }
        let delay = backoff(attempt);
        attempt += 1;
        if tx
            .send(AgentEvent::Retry {
                attempt,
                delay_secs: delay.as_secs(),
                error: err,
            })
            .await
            .is_err()
        {
            return Err(TurnFailure::new("tui closed", class, attempt));
        }
        tokio::time::sleep(delay).await;
    }
}

async fn ask_user(
    call: &ToolCallReq,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
) -> tools::Outcome {
    let id = *next_id;
    *next_id += 1;
    let question = call.args["question"].as_str().unwrap_or("").to_string();
    let options: Vec<AskOption> = call.args["options"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|o| AskOption {
                    label: o["label"].as_str().unwrap_or("").to_string(),
                    description: o["description"].as_str().map(|s| s.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();
    let multiple = call.args["multiple"].as_bool().unwrap_or(false);
    let allow_free = call.args["allow_free"].as_bool().unwrap_or(true);

    // Small and open models often emit ask_user with no arguments at all.
    // An empty popup is useless to the user, so refuse the call and hand the
    // model the exact shape to retry with instead of blocking on the UI.
    if question.is_empty() {
        return tools::Outcome::err(
            "ask_user rejected: 'question' is empty. Call it again with a non-empty question, \
             e.g. {\"question\": \"Which web framework should I use?\", \"options\": \
             [{\"label\": \"FastAPI\"}, {\"label\": \"Flask\"}], \"multiple\": false, \
             \"allow_free\": true}.",
        );
    }

    if tx
        .send(AgentEvent::AskUser {
            id,
            question,
            options,
            multiple,
            allow_free,
        })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed while asking");
    }
    loop {
        match ctl.recv().await {
            Some(ControlMsg::AskAnswer { id: aid, text }) if aid == id => {
                return tools::Outcome::ok(text);
            }
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled while asking"),
        }
    }
}

async fn bash_call(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    always_allow: &mut Vec<String>,
    blocked: &[String],
    next_id: &mut u64,
) -> tools::Outcome {
    let command = call.args["command"].as_str().unwrap_or("").to_string();
    let lower = command.to_lowercase();

    // 0. hard block from config — no questions
    for pat in blocked {
        let Ok(re) = regex::Regex::new(pat) else {
            continue;
        };
        if re.is_match(&command) || re.is_match(&lower) {
            return tools::Outcome::err(format!(
                "command blocked by [safety].blocked_patterns '{pat}'"
            ));
        }
    }

    // 1. heuristic dangerous-command detector
    let needs_approval =
        match safety::classify_for(crate::agent::shell::ShellKind::detect(), &command) {
            safety::Verdict::Safe => None,
            safety::Verdict::NeedsApproval(reason) => Some(reason),
        };

    if let Some(reason) = needs_approval {
        if !always_allow.contains(&command) {
            let id = *next_id;
            *next_id += 1;
            if tx
                .send(AgentEvent::Approval {
                    id,
                    command: command.clone(),
                    reason: reason.to_string(),
                })
                .await
                .is_err()
            {
                return tools::Outcome::err("tui closed awaiting approval");
            }
            let decision = loop {
                match ctl.recv().await {
                    Some(ControlMsg::ApprovalAnswer { id: aid, decision }) if aid == id => {
                        break decision;
                    }
                    Some(_) => continue,
                    None => return tools::Outcome::err("agent cancelled awaiting approval"),
                }
            };
            match decision {
                ApprovalDecision::Deny => {
                    return tools::Outcome::err(format!("command denied by user ({reason})"));
                }
                ApprovalDecision::AlwaysSession => always_allow.push(command.clone()),
                ApprovalDecision::RunOnce => {}
            }
        }
        // checkpoint before a dangerous (approved) command
        // §2.5: bash is the one case whose targets cannot be known in
        // advance, so this is where layer 2 earns its existence.
        if let Ok(Some(sha)) = checkpoints::snapshot_session(
            &ctx.root,
            crate::config::ShadowStore::Local,
            &ctx.session_id,
            &format!("pre_bash {command}"),
        ) {
            ctx.journal.push((sha, format!("bash {command}")));
        }
    }

    // 2. run it through the normal bash handler on a blocking thread so a long
    //    command never stalls the async runtime (and the TUI render loop)
    run_tool_blocking(ctx, "bash", &call.args).await
}

/// Execute a tool handler on a dedicated blocking thread.
///
/// `tools::execute` can run for a long time (e.g. `bash` up to its timeout), and
/// calling it directly inside this async task would occupy a tokio worker —
/// which, when the scheduler places `run_agent` on the `block_on` driver thread,
/// freezes the TUI for the whole call. `spawn_blocking` uses a separate thread
/// pool, so the runtime (and the UI) stay responsive. The cloned `ToolCtx` is
/// moved in and its journal/checkpoint bookkeeping is merged back afterwards so
/// callers observe every mutation the handler recorded.
async fn run_tool_blocking(
    ctx: &mut ToolCtx,
    name: &str,
    args: &serde_json::Value,
) -> tools::Outcome {
    let mut exec_ctx = ctx.clone();
    let fallback_ctx = exec_ctx.clone();
    let name = name.to_string();
    let args = args.clone();
    let (outcome, exec_ctx) = tokio::task::spawn_blocking(move || {
        let o = tools::execute(&mut exec_ctx, &name, &args);
        (o, exec_ctx)
    })
    .await
    .unwrap_or_else(|e| {
        (
            tools::Outcome::err(format!("tool thread failed: {e}")),
            fallback_ctx,
        )
    });
    ctx.journal = exec_ctx.journal;
    ctx.files_read = exec_ctx.files_read;
    outcome
}

#[cfg(test)]
mod subagent_tests {
    use super::*;

    #[test]
    fn accepts_one_or_many_subagent_tasks() {
        assert_eq!(
            subagent_tasks_from_args(&serde_json::json!({"task":" inspect "})).unwrap(),
            vec!["inspect"]
        );
        assert_eq!(
            subagent_tasks_from_args(&serde_json::json!({"tasks":["one","two"]})).unwrap(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn rejects_more_than_eight_subagents() {
        let tasks: Vec<String> = (0..9).map(|n| format!("task {n}")).collect();
        let error = subagent_tasks_from_args(&serde_json::json!({"tasks":tasks})).unwrap_err();
        assert!(error.contains("maximum is 8"));
        assert_eq!(MAX_PARALLEL_SUBAGENTS, 4);
    }
}

#[cfg(test)]
mod transport_tests {
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
}
