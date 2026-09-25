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
    /// provider-owned opaque state attached to this turn (reasoning items, phase)
    provider_state: Option<serde_json::Value>,
}

const MAX_SUBAGENTS_PER_CALL: usize = 8;
const MAX_PARALLEL_SUBAGENTS: usize = 4;
/// A child that produces nothing in this long is stuck (provider retry
/// loops run far longer): cancel cooperatively, wait out a short grace,
/// then tear the turn down. Without this a hung child stalls the parent
/// turn forever — Esc only stops what comes *after* running children.
const SUBAGENT_TIMEOUT_SECS: u64 = 600;
const SUBAGENT_CANCEL_GRACE_SECS: u64 = 5;

/// One task's text from any shape the model may send: a bare string, or
/// an object carrying it under `task`/`prompt`/`text`/`description` (the
/// last is Claude-Code convention for the short label — better than
/// refusing the whole batch). Trims; empty means absent.
fn subagent_task_text(item: &serde_json::Value) -> Option<String> {
    match item {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Object(map) => ["task", "prompt", "text", "description"]
            .into_iter()
            .filter_map(|key| map.get(key)?.as_str())
            .map(str::trim)
            .find(|text| !text.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

/// One parsed child task. Children are read-only by default; `write` with
/// `paths` declares a writer scoped to those roots. Paths are normalized
/// here so spawn-time overlap checks and execute-time enforcement agree.
#[derive(Debug, PartialEq)]
struct SubagentTask {
    label: String,
    write: bool,
    paths: Vec<String>,
}

fn subagent_task_spec(item: &serde_json::Value) -> Option<SubagentTask> {
    let label = subagent_task_text(item)?;
    let (write, paths) = match item {
        serde_json::Value::Object(map) => {
            let write = map.get("write").and_then(|v| v.as_bool()).unwrap_or(false);
            let paths = map
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|p| {
                            p.replace('\\', "/")
                                .trim_start_matches("./")
                                .trim_end_matches('/')
                                .to_string()
                        })
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            (write, paths)
        }
        _ => (false, Vec::new()),
    };
    Some(SubagentTask {
        label,
        write,
        paths,
    })
}

/// Two writer scopes overlap when a path is equal or nested either way.
/// Sibling writers must not overlap: concurrent writes to one file tangle
/// attribution and undo beyond what epochs can separate.
fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| {
        b.iter().any(|y| {
            x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/"))
        })
    })
}

fn subagent_tasks_from_args(args: &serde_json::Value) -> Result<Vec<SubagentTask>, String> {
    let mut tasks: Vec<SubagentTask> = match args.get("tasks") {
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(subagent_task_spec).collect()
        }
        // a lone string is one task, not a malformed array
        Some(single) => subagent_task_spec(single).into_iter().collect(),
        None => Vec::new(),
    };
    if tasks.is_empty()
        && let Some(task) = args.get("task").and_then(subagent_task_spec)
    {
        tasks.push(task);
    }
    if tasks.is_empty() {
        return Err(
            "subagent task is required: pass task (string) or tasks (array of strings or objects with task|prompt)"
                .into(),
        );
    }
    if tasks.len() > MAX_SUBAGENTS_PER_CALL {
        return Err(format!(
            "too many subagents: maximum is {MAX_SUBAGENTS_PER_CALL}"
        ));
    }
    // writers declare their scope up front; overlapping siblings refuse
    // before anything spawns, naming both sides.
    for task in &tasks {
        if task.write && task.paths.is_empty() {
            return Err(format!(
                "subagent write needs paths: task '{}' declares write without a scope — name the roots it may touch",
                task.label.chars().take(80).collect::<String>()
            ));
        }
    }
    for (i, a) in tasks.iter().enumerate() {
        if !a.write {
            continue;
        }
        for b in tasks.iter().skip(i + 1) {
            if b.write && scopes_overlap(&a.paths, &b.paths) {
                return Err(format!(
                    "subagent writer scopes overlap: '{}' and '{}' share paths — split the scopes or serialize the work",
                    a.label.chars().take(60).collect::<String>(),
                    b.label.chars().take(60).collect::<String>()
                ));
            }
        }
    }
    Ok(tasks)
}

/// Run a pure-subagent batch concurrently: every call gets its pre-phase
/// (cancel check, journal row, ToolStart row) in call order, the children
/// run overlapped, and results are processed back in call order, so rows,
/// journal and transcript look exactly like a fast sequential batch — only
/// the waits overlap. Mixed batches keep the sequential loop: interleaving
/// arbitrary tools would tangle journal attribution and mutation order.
///
/// Esc semantics match one running subagent: the wait loop polls the
/// parent flag, so a stop request cooperatively stops the children (each
/// gets its own cancel first) instead of waiting out the timeout.
#[allow(clippy::too_many_arguments)]
async fn run_subagent_batch(
    calls: &[ToolCallReq],
    session_id: &str,
    root: &Path,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    journal: &mut Option<crate::agent::journal::Journal>,
    tx: &mpsc::Sender<AgentEvent>,
    current_step: Option<String>,
    provider: &SharedProvider,
    model_id: &str,
    blocked_patterns: &[String],
    plan_mode: bool,
    context_limit: u64,
    effort: Option<EffortLevel>,
    effort_support: crate::config::EffortSupport,
    max_tokens: Option<u32>,
    system: &[SystemPart],
    mcp: &crate::config::McpConfig,
    lsp: &crate::config::LspConfig,
    read_only: bool,
    shadow_store: crate::config::ShadowStore,
    messages: &mut Vec<Message>,
    diary: crate::config::DiaryConfig,
    memory: crate::config::MemoryConfig,
    compaction: crate::config::CompactionConfig,
    plan_limits: crate::config::PlanConfig,
    fallback_chain: Vec<FallbackCandidate>,
    timeout: std::time::Duration,
) -> bool {
    use futures::{StreamExt, stream};
    // pre-phase in order; stops at the first pre-cancelled call exactly
    // like the sequential loop (that call still gets its cancelled row,
    // later ones only their protocol tool_result)
    let mut dispatch: Vec<(usize, ToolCallReq, std::time::Instant)> = Vec::new();
    let mut cutoff = calls.len();
    let mut interrupted = false;
    for (index, call) in calls.iter().enumerate() {
        let pre_cancelled = cancel.swap(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(writer) = journal.as_mut() {
            let active = plan::open_active_for_session(root, Some(session_id))
                .ok()
                .flatten();
            let plan_id = active.as_ref().map(|p| p.id.clone());
            let step = current_step.clone().or_else(|| {
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
                    "path": tools::call_path(&call.name, &call.args),
                }),
            );
        }
        let _ = tx
            .send(AgentEvent::ToolStart {
                name: call.name.clone(),
                summary: tools::call_summary(&call.name, &call.args),
                call_id: call.id.clone(),
            })
            .await;
        if pre_cancelled {
            cutoff = index;
            break;
        }
        dispatch.push((index, call.clone(), std::time::Instant::now()));
    }
    // fan out; results collected out of order, processed back in order
    let mut done: Vec<(usize, ToolCallReq, std::time::Instant, tools::Outcome)> =
        stream::iter(dispatch)
            .map(|(index, call, started)| {
                let session_id = session_id.to_string();
                let provider = provider.clone();
                let model_id = model_id.to_string();
                let root = root.to_path_buf();
                let blocked_patterns = blocked_patterns.to_vec();
                let system = system.to_vec();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
                let diary = diary.clone();
                let memory = memory.clone();
                let compaction = compaction.clone();
                let fallback_chain = fallback_chain.clone();
                let parent_cancel = cancel.clone();
                async move {
                    let outcome = run_subagent(
                        &call,
                        &session_id,
                        tx,
                        &parent_cancel,
                        &provider,
                        &model_id,
                        &root,
                        &blocked_patterns,
                        plan_mode,
                        context_limit,
                        effort,
                        effort_support,
                        max_tokens,
                        system,
                        mcp,
                        lsp,
                        read_only,
                        shadow_store,
                        diary,
                        memory,
                        compaction,
                        plan_limits,
                        fallback_chain,
                        timeout,
                    )
                    .await;
                    (index, call, started, outcome)
                }
            })
            .buffer_unordered(MAX_PARALLEL_SUBAGENTS)
            .collect()
            .await;
    done.sort_by_key(|(index, _, _, _)| *index);
    for (_index, call, tool_started, outcome) in done {
        let _ = tx
            .send(AgentEvent::ToolNotice {
                name: call.name.clone(),
                summary: outcome.output.clone(),
                ok: outcome.ok,
                diff: outcome.diff.clone(),
                call_id: call.id.clone(),
            })
            .await;
        if let Some(writer) = journal.as_mut() {
            let _ = writer
                .append_evidence(
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
                .ok();
        }
        if outcome.cancelled {
            interrupted = true;
        }
        messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
    }
    if cutoff < calls.len() {
        // the call that saw the flag: a cancelled result row, exactly like
        // the sequential loop — then plain protocol results for the rest
        let call = &calls[cutoff];
        let outcome = tools::Outcome::cancelled();
        let _ = tx
            .send(AgentEvent::ToolNotice {
                name: call.name.clone(),
                summary: outcome.output.clone(),
                ok: outcome.ok,
                diff: outcome.diff.clone(),
                call_id: call.id.clone(),
            })
            .await;
        if let Some(writer) = journal.as_mut() {
            let _ = writer
                .append_evidence(
                    "tool_result",
                    serde_json::json!({
                        "tool": call.name,
                        "call_id": call.id,
                        "ok": outcome.ok,
                        "duration_ms": 0,
                        "summary": outcome.output.chars().take(200).collect::<String>(),
                        "trust": "high",
                        "code": Some("cancelled"),
                    }),
                )
                .ok();
        }
        interrupted = true;
        messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
        for rest in &calls[cutoff + 1..] {
            messages.push(Message::tool_result(
                &rest.id,
                "cancelled by user — this tool call was not run".to_string(),
                true,
            ));
        }
    }
    interrupted
}

/// The step a session should hold: whatever the plan keeps in progress,
/// or idle. Used at startup (adopt the interrupted step after a crash,
/// §3.4) and after a subagent returns (the child may have retired or
/// moved the held step, §2.2.3). `ctx.current_step` stays the single live
/// copy; the TUI persists it via `StepCurrent` events.
fn adopt_in_progress_step(root: &Path, session_id: &str) -> Option<String> {
    plan::open_active_for_session(root, Some(session_id))
        .ok()
        .flatten()
        .and_then(|plan| {
            plan.steps
                .into_iter()
                .find(|step| step.status == plan::StepStatus::InProgress)
                .map(|step| step.id)
        })
}

#[allow(clippy::too_many_arguments)] // all parameters are required for subagent configuration
async fn run_subagent(
    call: &ToolCallReq,
    parent_session: &str,
    parent_tx: &mpsc::Sender<AgentEvent>,
    parent_cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    shadow_store: crate::config::ShadowStore,
    diary: crate::config::DiaryConfig,
    memory: crate::config::MemoryConfig,
    compaction: crate::config::CompactionConfig,
    plan_limits: crate::config::PlanConfig,
    fallback_chain: Vec<FallbackCandidate>,
    timeout: std::time::Duration,
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
                // re-parseable single: the recursive call re-derives the
                // same write scope from these flags
                one.args = serde_json::json!({
                    "task": task.label,
                    "write": task.write,
                    "paths": task.paths,
                });
                let system = system.clone();
                let mcp = mcp.clone();
                let lsp = lsp.clone();
                let diary = diary.clone();
                let memory = memory.clone();
                let compaction = compaction.clone();
                let fallback_chain = fallback_chain.clone();
                async move {
                    let outcome = run_subagent(
                        &one,
                        parent_session,
                        parent_tx,
                        parent_cancel,
                        provider,
                        model_id,
                        root,
                        blocked_patterns,
                        plan_mode,
                        context_limit,
                        effort,
                        effort_support,
                        max_tokens,
                        system,
                        mcp,
                        lsp,
                        read_only,
                        shadow_store,
                        diary,
                        memory,
                        compaction,
                        plan_limits,
                        fallback_chain,
                        timeout,
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
    let child_session = next_subagent_session();
    // Read-only by default: a writer child needs the explicit flag (with
    // paths, validated above) on top of the session lock.
    let child_read_only = read_only || !task.write;
    if task.write {
        tools::register_subagent_scope(&child_session, task.paths.clone());
    }
    // NOTE (§2.2.4): children always complete inside this tool call — the
    // event loop below is awaited before the outcome returns. There is no
    // fire-and-forget spawn, so `plan finish` can never race still-running
    // children of the same step; no extra gate is needed for that.
    // Inherit the spawning step, if any (§2.2.4): the child stamps this
    // context on its records and stops mutating once the epoch moves on.
    // Resolved against the PARENT session, never the global fallback: the
    // most recent active plan may belong to another session, and the child
    // must work its parent's step or none at all.
    let parent_step = plan::open_active_for_session(root, Some(parent_session))
        .ok()
        .flatten()
        .and_then(|plan| {
            plan.steps
                .iter()
                .find(|step| step.status == plan::StepStatus::InProgress)
                .map(|step| plan::StepContext {
                    plan_id: plan.id.clone(),
                    step_id: step.id.clone(),
                    step_epoch: step.step_epoch,
                })
        });
    let _ = parent_tx
        .send(AgentEvent::SubagentStart {
            id,
            task: task.label.clone(),
        })
        .await;
    // #171: the child works its parent's step, so it joins the parent plan
    // explicitly. Without membership its evidence cannot attach under
    // session-strict resolution — and silent fallback is gone on purpose.
    // Serialized: concurrent siblings must not read-modify-write the plan
    // file over each other and drop a join.
    {
        let _guard = PLAN_JOIN_LOCK.lock().await;
        join_plan_session(root, parent_step.as_ref(), parent_session, &child_session);
    }
    let child = spawn_agent(AgentInput {
        provider: provider.clone(),
        model_id: model_id.to_string(),
        model_key: format!("sub-{id}"),
        effort,
        effort_support,
        max_tokens,
        system,
        messages: vec![Message::new(Role::User, task.label.clone())],
        root: root.to_path_buf(),
        // #190: NOT `sub-{id}` — the numeric counter resets on restart
        // and would append to a previous run's journal file
        session_id: child_session.clone(),
        blocked_patterns: blocked_patterns.to_vec(),
        plan_mode,
        context_limit,
        enable_tools: true,
        read_only: child_read_only,
        previous_response_id: None,
        summary: None,
        mcp,
        lsp,
        compact_only: false,
        diary,
        memory,
        compaction,
        plan_limits,
        shadow_store,
        subagent_depth: 1,
        parent_step,
        parent_session: Some(parent_session.to_string()),
        fallback_chain,
    });
    let mut child = child;
    // S1: visible to undo's cancellation signal until it joins. The guard
    // leaves the map on every exit path below, including early returns.
    let _child_slot = super::undo_guard::track_child(id, child.child_control());
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    // Esc polling: the parent cancel flag is the only stop signal visible
    // inside this wait — without it a stop request sits unobserved until
    // the timeout, and the TUI shows "cancelling…" forever (§3.7).
    let mut cancel_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        // Checked at the top of every iteration, not only on the interval
        // tick. The select below is `biased`, so while the child keeps
        // producing events its `recv` branch is always the ready one and the
        // interval branch is never polled — a fast child streaming a long
        // answer would starve the check and Esc would look ignored. The
        // loop always consumes one event per iteration, so this check can
        // never starve; the interval branch still covers a silent child
        // parked in `recv`.
        if parent_cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return stop_child(&mut child, parent_tx, id, "cancelled by user").await;
        }
        let event = tokio::select! {
            biased;
            res = tokio::time::timeout_at(deadline, child.rx.recv()) => match res {
            Ok(Some(event)) => event,
            Ok(None) => {
                let result = tools::Outcome::err("subagent disconnected");
                let _ = parent_tx
                    .send(AgentEvent::SubagentDone {
                        id,
                        ok: false,
                        output: result.output.clone(),
                    })
                    .await;
                return result;
            }
            Err(_) => {
                return stop_child(&mut child, parent_tx, id, &format!("timed out after {}s", timeout.as_secs())).await;
            }
            },
            _ = cancel_poll.tick() => {
                continue;
            }
        };
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
            AgentEvent::ToolStart { name, summary, .. } => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentToolStart { id, name, summary })
                    .await;
            }
            AgentEvent::ToolNotice {
                name,
                summary,
                ok,
                diff,
                ..
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
            // interactive child events have no parent surface; answer them
            // immediately so the child never blocks on an answer nobody can
            // give (a declined proposal tells it to ask the user directly)
            AgentEvent::AskUser { id, .. } => {
                let _ = child.control.try_send(ControlMsg::AskAnswer {
                    id,
                    text: "subagents cannot reach the user; decide yourself and continue"
                        .to_string(),
                });
            }
            AgentEvent::Approval { id, .. } => {
                let _ = child.control.try_send(ControlMsg::ApprovalAnswer {
                    id,
                    decision: ApprovalDecision::Deny,
                });
            }
            AgentEvent::PlanProposal { id, .. } => {
                let _ = child
                    .control
                    .try_send(ControlMsg::PlanAnswer { id, accept: false });
            }
            _ => {}
        }
    }
}

/// Stop a child that will not stop itself: ask cooperatively first (a
/// `bash` child kills its own process tree on this), wait out a short
/// grace, then tear down hard. Always ends with `SubagentDone(ok:false)`
/// and the error outcome — the parent turn must never hang on a child.
/// `reason` names the trigger ("timed out after Ns" / "cancelled by
/// user"); a child that finishes inside the grace is still discarded: its
/// result belongs to a turn that already moved on.
async fn stop_child(
    child: &mut AgentHandle,
    parent_tx: &mpsc::Sender<AgentEvent>,
    id: u64,
    reason: &str,
) -> tools::Outcome {
    child.request_tool_cancel();
    let grace = std::time::Duration::from_secs(SUBAGENT_CANCEL_GRACE_SECS);
    let finished = tokio::time::timeout(grace, child.rx.recv()).await;
    child.abort();
    let result = if matches!(finished, Ok(Some(AgentEvent::Completed(_)))) {
        tools::Outcome::err(format!(
            "subagent {reason} (finished during cancel, result discarded)"
        ))
    } else {
        tools::Outcome::err(format!("subagent {reason}"))
    };
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

/// Serializes plan-membership joins: concurrent sibling subagents share one
/// plan file, and an unlocked read-modify-write would drop all but one join.
static PLAN_JOIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Join one child session into its parent plan (#171: without membership
/// its evidence cannot attach under session-strict resolution).
/// Journal-first like every plan mutation, in the PARENT's journal: the
/// startup replay only scans the cursor session's suffix, so an intent in
/// the child's file would never replay. The cursor advances, so a crash
/// heals by replay instead of leaving a store write the journal never saw.
fn join_plan_session(
    root: &Path,
    parent_step: Option<&plan::StepContext>,
    parent_session: &str,
    child_session: &str,
) {
    if let Some(step) = parent_step
        && let Ok(plan) = plan::open(root, &step.plan_id)
        && !plan.sessions.iter().any(|s| s == child_session)
    {
        let mut plan = plan;
        // apply first: commit only persists the already-mutated plan behind
        // the journal intent (same order as the plan tool path)
        if plan::apply(
            &mut plan,
            plan::Op::Join {
                session: child_session.to_string(),
            },
            &plan::Limits::default(),
            None,
        )
        .is_ok()
        {
            let _ = plan::commit(
                root,
                parent_session,
                &mut plan,
                "join",
                "host",
                true,
                serde_json::json!({"session": child_session}),
            );
        }
    }
}

/// Journal/shadow identity for a child agent. Unique across process
/// restarts (#190): a bare per-process counter resets to 1, so a resumed
/// session's new subagent would append to the previous run's `sub-1.jsonl`.
/// Wall ms + pid + counter cannot repeat (same pid means same process,
/// where the counter differs).
fn next_subagent_session() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("sub-{ms}-{}-{n}", std::process::id())
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
        Some(format!(
            "\n[host: you already ran this exact command earlier in this session with byte-identical output — reuse that observation instead of re-running it. If the underlying state may have changed since, ignore this note.]"
        ))
    } else {
        None
    }
}

/// Auto-reflector kill switch (PARKED — see the H0 block in run_agent).
/// The mechanism stays callable (manual /verify); the auto path runs only
/// with SQWAI_AUTO_REFLECTOR=1 in the environment.
fn auto_reflector_enabled() -> bool {
    std::env::var("SQWAI_AUTO_REFLECTOR").is_ok()
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
    // H0 (§12.7, PARKED — auto path off, see auto_reflector_enabled):
    // when armed by a fired criticism check, the fact block rides as a
    // volatile block-D part per request. Stays None while parked.
    let mut criticism_block: Option<String> = None;
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
            // H0 criticism detector (§12.7): PARKED — auto-detection (learned
            // + artifact) fires too imprecisely, so the whole auto path is
            // off. The mechanism stays in code (manual /verify); set
            // SQWAI_AUTO_REFLECTOR=1 to re-enable for experiments.
            // Main sessions only — a child's task prompt is a directive, not
            // user criticism — and never on the G0 baseline (mechanism
            // features stay out of it, §8.2).
            if auto_reflector_enabled()
                && parent_step.is_none()
                && !crate::bench::baseline()
            {
                // H1 slice 3 self-protection: objections since the last
                // verify pick the budget; the third disables the reflector
                // for the session (journal-first, not a hidden switch).
                if crate::agent::reflector::reflector_disabled(&root, &session_id) {
                    crate::providers::log_http("reflector: disabled for this session");
                } else {
                    let objections =
                        crate::agent::reflector::objections_after_last_verify(&root, &session_id);
                    if objections >= 2 {
                        let _ = writer.append(
                            "reflector_disabled",
                            serde_json::json!({"objections": objections}),
                        );
                        crate::providers::log_http(
                            "reflector: disabled for the session after repeated objections",
                        );
                    } else {
                        let budget = if objections >= 1 {
                            crate::agent::criticism::Budget::full()
                        } else {
                            crate::agent::criticism::Budget::auto()
                        };
                        let mut check = crate::agent::criticism::check_with_window(
                            &root,
                            &session_id,
                            &user_message.content,
                            budget.window,
                        );
                        // H0-maybe confirm (§12.7): a held-back Maybe gets one
                        // cheap model question; a confirmed target grounds the
                        // fire. Silent and already-fired checks pass through
                        // untouched (no I/O inside).
                        if !check.fire
                            && let Some(upgraded) = crate::agent::reflector::confirm_maybe(
                                &provider,
                                &model_id,
                                &root,
                                &check,
                                &user_message.content,
                            )
                            .await
                        {
                            check = upgraded;
                        }
                        if check.fire {
                            let _ = writer.append(
                                "criticism",
                                crate::agent::criticism::marker_fields(
                                    &check,
                                    &user_message.content,
                                ),
                            );
                            criticism_block =
                                crate::agent::criticism::block_text(&check, &user_message.content);
                            // H1 (§12.7): Scope + Neutralizer + Executor + Verdict,
                            // synchronous. Trigger is Fire+artifact (decided):
                            // generic fires keep the L0 block only. A failed stage
                            // degrades to what came before — the L0 block already
                            // carries the turn, so silence here never loses anything.
                            if !check.artifacts.is_empty()
                                && let Some(rctx) = crate::agent::reflector::scope(
                                    &root,
                                    &session_id,
                                    &check,
                                    &user_message.content,
                                )
                            {
                                match crate::agent::reflector::neutralize(
                                    &provider, &model_id, &rctx,
                                )
                                .await
                                {
                                    Ok(checks) => {
                                        let report = crate::agent::reflector::verify(
                                            &root,
                                            &session_id,
                                            &model_id,
                                            &provider,
                                            writer,
                                            &rctx,
                                            &checks,
                                            &budget,
                                        )
                                        .await;
                                        messages.push(crate::providers::Message::new(
                                            crate::providers::Role::Assistant,
                                            report.block,
                                        ));
                                        // the transcript the host owns now differs
                                        // from the provider's copy (same rule as
                                        // compaction/undo: send ours, not a
                                        // continuation).
                                        previous_response_id = None;
                                    }
                                    Err(error) => {
                                        crate::providers::log_http(&format!(
                                            "reflector: neutralize failed: {error:#}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
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
        // claim-lint repetition (Y, §12.9): nag only while mismatches are
        // fresh — a model that behaves stops seeing this by itself
        if let Ok(Some(nudge)) =
            crate::agent::journal::Journal::claim_nudge(&root, Some(&session_id))
        {
            turn_system.push(crate::providers::SystemPart::volatile(nudge));
        }
        // H0 fact block (§12.7): computed once above, visible to every
        // request of this turn while the caches for A and B survive.
        if let Some(block) = criticism_block.as_deref() {
            turn_system.push(crate::providers::SystemPart::volatile(block));
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
            // final answer — claim lint (Y, §12.9) checks it here, marking
            // contradictions without ever blocking the turn
            let text = lint_answer(&turn.text, &root, &session_id, &mut journal);
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
fn plan_hint_for_summary(root: &std::path::Path, session_id: &str) -> String {
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
fn record_compaction(
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
fn compaction_request(
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

async fn compact_history(
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
    has_fallback: bool,
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
        let mut provider_state: Option<serde_json::Value> = None;

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
                        "usage event: prompt={} completion={} cached={:?} written={:?} reasoning={:?}",
                        u.prompt_tokens,
                        u.completion_tokens,
                        u.cached_tokens,
                        u.cache_write_tokens,
                        u.reasoning_tokens
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
                Ok(StreamEvent::ProviderState(s)) => {
                    provider_state = Some(s);
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
                provider_state,
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
        if now >= dl || (has_fallback && attempt >= 1) {
            return Err(TurnFailure::new(
                if has_fallback && attempt >= 1 {
                    format!("{err} — giving up after {attempt} retries (fallback model configured)")
                } else {
                    format!("{err} — giving up after 1h of retries")
                },
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

/// Claim lint (Y, §12.9): post-generation check of result claims in the
/// final answer against this turn's journal window (records after the
/// latest user message). Only CONTRADICTIONS are marked — absence of
/// evidence is silence, never a flag. Never fails and never blocks:
/// any internal error returns the text untouched.
fn lint_answer(
    text: &str,
    root: &std::path::Path,
    session_id: &str,
    journal: &mut Option<crate::agent::journal::Journal>,
) -> String {
    // quoting the user is not claiming: drop markdown-quote lines first
    let visible: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n");
    let records = crate::agent::journal::Journal::records_for(root, session_id).unwrap_or_default();
    let start = records
        .iter()
        .filter(|r| r.kind == "user_msg")
        .map(|r| r.seq)
        .max()
        .unwrap_or(0);
    let results: Vec<(String, bool, String)> = records
        .iter()
        .filter(|r| r.kind == "tool_result" && r.seq > start)
        .map(|r| {
            (
                r.fields
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                r.fields
                    .get("ok")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                r.fields
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect();
    let any_fail = results.iter().any(|(_, ok, _)| !ok);
    let exec_ok = results.iter().any(|(tool, ok, _)| *ok && tool == "bash");
    let summaries = results
        .iter()
        .map(|(_, _, s)| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // candidate spans in first-seen order; each distinct span is marked once
    let mut spans: Vec<(&str, &str)> = Vec::new(); // (kind, span)
    for (kind, span) in extract_counts(&visible) {
        // an x/y shorthand ("10808/10809") is verified when both halves
        // were reported separately: the journal carries facts, not the
        // model's punctuation
        let verified = summaries.contains(span) || count_parts_verified(&summaries, span);
        if !verified && any_fail {
            push_span(&mut spans, kind, span);
        }
    }
    // sized claims near result words verify the same way: a number the
    // journal never reported beside tests/build/exit is a claim, whatever
    // language it wears.
    for (kind, span) in extract_sized_claims(&visible) {
        let verified = summaries.contains(span);
        if !verified && any_fail {
            push_span(&mut spans, kind, span);
        }
    }
    for (_, span) in extract_status_words(&visible) {
        if exec_ok {
            continue;
        }
        if any_fail {
            push_span(&mut spans, "status", span);
        }
    }
    for span in extract_paths(&visible) {
        if path_deleted_nearby(&visible, span) {
            continue;
        }
        // a path followed by an arrow ("services.msc -> its publisher") is
        // a usage pointer — advice to open something — not an existence
        // claim about the project tree
        if path_arrow_after(&visible, span) {
            continue;
        }
        if !path_exists(root, span) {
            push_span(&mut spans, "path", span);
        }
    }
    // symbols last and fewest: each costs a graph lookup
    if spans.len() < 40
        && let Ok(mut store) = crate::agent::graph::SqliteGraphStore::open(root)
    {
        for sym in extract_symbols(&visible) {
            if spans.len() >= 40 {
                break;
            }
            match store.resolve_ref(None, None, Some(sym)) {
                Ok(crate::agent::graph::ResolveRefResult::NotFound { .. }) => {
                    push_span(&mut spans, "symbol", sym)
                }
                Ok(_) => {}
                Err(_) => {} // infra failure: silence, not a verdict
            }
        }
    }
    if spans.is_empty() {
        return text.to_string();
    }
    if let Some(writer) = journal.as_mut() {
        for (kind, span) in &spans {
            let _ = writer.append(
                "claim_lint",
                serde_json::json!({
                    "span": span,
                    "kind": kind,
                    "by": "host",
                }),
            );
        }
    }
    // mark first occurrence of each span; offsets shift as we insert
    let mut marked = text.to_string();
    let mut done: Vec<&str> = Vec::new();
    for (_, span) in &spans {
        if done.contains(span) {
            continue;
        }
        done.push(span);
        if let Some(pos) = marked.find(span) {
            marked.insert_str(pos + span.len(), " [unverified]");
        }
    }
    marked
}

/// Capped dedup push for lint spans. A span already covered by a longer
/// collected one (e.g. "tests pass" inside "All tests pass") is skipped.
fn push_span<'x>(spans: &mut Vec<(&'static str, &'x str)>, kind: &'static str, span: &'x str) {
    if spans.len() < 40 && !spans.iter().any(|(_, s)| *s == span || s.contains(span)) {
        spans.push((kind, span));
    }
}

/// True when the byte before `pos` continues the same token: a digit span
/// starting right after a letter, dot, colon or slash is the tail of an
/// address, version or path ("127.0.0.1:10808", "v2.1"), not a count of
/// its own. Callers pass token starts, which are char boundaries.
fn token_char_before(text: &str, pos: usize) -> bool {
    text[..pos]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || ".:/".contains(c))
}

/// `12 passed`, `3 failed`, `280/280` — byte spans into `text`.
/// Char-walked: every index below is a char boundary by construction
/// (byte-walking multibyte text panicked here on Cyrillic input).
fn extract_counts(text: &str) -> Vec<(&'static str, &str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or('\0');
        if !c.is_ascii_digit() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        while i < text.len() && text[i..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        let mut j = i;
        while j < text.len() && text[j..].chars().next().is_some_and(|c| c.is_whitespace()) {
            j += text[j..].chars().next().unwrap().len_utf8();
        }
        // x/y form — but only standalone: "280/280" is a count, while
        // "127.0.0.1:10808/10809" is an address and "v2.1/3" a version.
        // A token char immediately before the first digit means the span is
        // the tail of something bigger, not a claim of its own.
        if text[j..].starts_with('/') && !token_char_before(text, start) {
            let mut k = j + 1;
            while k < text.len() && text[k..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
                k += 1;
            }
            if k > j + 1 {
                out.push(("count", &text[start..k]));
                i = k;
                continue;
            }
        }
        // word form: passed|failed
        let mut k = j;
        while k < text.len()
            && text[k..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
        {
            k += text[k..].chars().next().unwrap().len_utf8();
        }
        if &text[j..k] == "passed" || &text[j..k] == "failed" {
            out.push(("count", &text[start..k]));
            i = k;
        }
    }
    out
}

/// Sized claims near result words: "12 tests", "tests: 12", "exit 0",
/// "exit code 0", "0 errors", "12 ошибок" — English and Russian. A bare
/// number elsewhere ("3 files") is not a result claim. Spans run across
/// the number and the word (either order), allowing whitespace and
/// `:`, `#`, `,` between them. Verified against tool summaries exactly
/// like `extract_counts`.
fn extract_sized_claims(text: &str) -> Vec<(&'static str, &str)> {
    const WORDS: &[&str] = &[
        "test", "tests", "build", "exit", "error", "errors", "тест", "тесты", "тестов",
        "сборка", "сборки", "ошибка", "ошибки", "ошибок",
    ];
    // alphanumeric tokens with byte spans; every index below stays a char
    // boundary by construction (byte-walking multibyte text panics).
    let mut toks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        if c.is_alphanumeric() {
            let start = i;
            while i < text.len()
                && text[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric())
            {
                i += text[i..].chars().next().unwrap().len_utf8();
            }
            toks.push((start, i));
        } else {
            i += c.len_utf8();
        }
    }
    let word_at = |k: usize| toks.get(k).map(|(s, e)| &text[*s..*e]);
    let is_num =
        |k: usize| word_at(k).is_some_and(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()));
    let is_word =
        |k: usize| word_at(k).is_some_and(|w| WORDS.contains(&w.to_lowercase().as_str()));
    let gap_ok = |a_end: usize, b_start: usize| {
        text[a_end..b_start]
            .chars()
            .all(|c| c.is_whitespace() || ":,#№".contains(c))
    };
    let mut out = Vec::new();
    let mut k = 0;
    while k < toks.len() {
        // "exit code N" triple
        if k + 2 < toks.len()
            && word_at(k).is_some_and(|w| w.to_lowercase() == "exit")
            && word_at(k + 1).is_some_and(|w| w.to_lowercase() == "code")
            && is_num(k + 2)
            && gap_ok(toks[k].1, toks[k + 1].0)
            && gap_ok(toks[k + 1].1, toks[k + 2].0)
        {
            out.push(("count", &text[toks[k].0..toks[k + 2].1]));
            k += 3;
            continue;
        }
        // "N word" and "word N" pairs
        if k + 1 < toks.len()
            && gap_ok(toks[k].1, toks[k + 1].0)
            && ((is_num(k) && is_word(k + 1)) || (is_word(k) && is_num(k + 1)))
        {
            out.push(("count", &text[toks[k].0..toks[k + 1].1]));
            k += 2;
            continue;
        }
        k += 1;
    }
    out
}

/// status phrases that assert success without numbers. English plus
/// Russian: the model often answers in Russian, and a warn-layer that
/// only reads English is blind to half the claims. Conservative list —
/// success-asserting phrases only, since every hit marks text.
fn extract_status_words(text: &str) -> Vec<(&'static str, &str)> {    const PHRASES: &[&str] = &[
        "build succeeded",
        "builds succeeded",
        "all green",
        "tests pass",
        "test passes",
        "suite passes",
        "suites pass",
        "suite green",
        "all tests pass",
        "everything passes",
        "тесты прошли",
        "тест прошел",
        "тест прошёл",
        "все тесты прошли",
        "тесты зеленые",
        "тесты зелёные",
        "все зеленые",
        "все зелёные",
        "всё зелёное",
        "все зелено",
        "сборка прошла",
        "сборка успешна",
        "успешно собралось",
        "собралось",
        "исправлено",
        "баг исправлен",
        "ошибка исправлена",
    ];
    let lower = text.to_lowercase();
    let mut out = Vec::new();
    for phrase in PHRASES {
        // first occurrence span mapped back by byte search (phrases are ASCII).
        // `find` runs on the lowered copy, whose byte coordinates can drift
        // from the original when case-folding changes length — so the slice
        // is re-verified, not just boundary-checked.
        if let Some(pos) = lower.find(phrase) {
            let end = pos + phrase.len();
            if text.is_char_boundary(pos)
                && text.is_char_boundary(end)
                && text[pos..end].to_lowercase() == *phrase
            {
                out.push(("status", &text[pos..end]));
            }
        }
    }
    out
}

/// path-looking tokens: contain `/` and `.`, or backticked with a dot.
/// Trailing punctuation stripped. Returns spans into `text`.
/// Char-walked with a boundary invariant on every index (the byte version
/// panicked on multibyte input: a Cyrillic lead byte reads as alphanumeric
/// and stops the scan mid-char).
fn extract_paths(text: &str) -> Vec<&str> {
    fn is_tok(c: char) -> bool {
        c.is_alphanumeric() || "._-/".contains(c)
    }
    let mut out = Vec::new();
    let mut i = 0;
    // invariant: i, start and end below are always char boundaries
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        let backticked = c == '`';
        if backticked {
            i += 1;
        } else if !is_tok(c) {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        while i < text.len() {
            let c = text[i..].chars().next().unwrap();
            if !is_tok(c) {
                break;
            }
            i += c.len_utf8();
        }
        let mut end = i;
        while end > start {
            // end starts at a boundary and moves back by whole chars
            match text[..end].chars().next_back() {
                Some(c) if ",.:;!?".contains(c) => end -= c.len_utf8(),
                _ => break,
            }
        }
        let closed = backticked && text[end..].starts_with('`');
        if end > start {
            let tok = &text[start..end];
            if (tok.contains('/') && tok.contains('.'))
                || (backticked && closed && tok.contains('.'))
            {
                out.push(tok);
            }
        }
        if backticked && text[i..].starts_with('`') {
            i += 1; // skip the closing backtick so it is not rescanned
        }
    }
    out
}

/// guard for tombstone claims ("deleted x.rs"): a deletion verb in the
/// preceding 24 chars means a missing file is expected, not a lie.
fn path_deleted_nearby(text: &str, span: &str) -> bool {
    const VERBS: &[&str] = &[
        "delete",
        "deleted",
        "remove",
        "removed",
        "rm ",
        "unlink",
        "deleting",
        "removing",
        "удалил",
        "удали",
        "удалить",
        "удалено",
        "убрал",
        "стёр",
        "стер",
    ];
    let Some(pos) = text.find(span) else {
        return false;
    };
    // `from` is a raw byte rewind and can land mid-char on multibyte text;
    // walk forward to a boundary (keeps the ~48-byte window, never panics).
    let mut from = pos.saturating_sub(48);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let before = text[from..pos].to_lowercase();
    VERBS.iter().any(|v| before.contains(v))
}

/// An x/y count ("10808/10809") is verified when both halves were reported
/// separately: the journal carries facts, not the model's punctuation.
/// Plain numbers only — anything else falls back to exact matching.
fn count_parts_verified(summaries: &str, span: &str) -> bool {
    let mut halves = span.split('/');
    let (Some(a), Some(b), None) = (halves.next(), halves.next(), halves.next()) else {
        return false;
    };
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_digit())
        && b.chars().all(|c| c.is_ascii_digit())
        && summaries.contains(a)
        && summaries.contains(b)
}

/// A path followed by an arrow ("services.msc -> its publisher") is a usage
/// pointer, not an existence claim. Checks the text right after the span's
/// first occurrence (past a closing backtick, if the span was quoted).
fn path_arrow_after(text: &str, span: &str) -> bool {
    let Some(pos) = text.find(span) else {
        return false;
    };
    let rest = text[pos + span.len()..]
        .trim_start()
        .trim_start_matches(['`', '"', '\''])
        .trim_start();
    rest.starts_with("->") || rest.starts_with('→')
}

fn path_exists(root: &std::path::Path, span: &str) -> bool {
    let rel = std::path::Path::new(span);
    if rel.is_absolute() {
        return rel.exists();
    }
    root.join(rel).exists()
}

/// `path::symbol` tokens. Returns spans into `text`, capped by the caller.
fn extract_symbols(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '/')) {
        if tok.contains("::") && !tok.starts_with(':') && !tok.ends_with(':') {
            out.push(tok);
        }
    }
    out
}

async fn ask_user(
    call: &ToolCallReq,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
) -> tools::Outcome {
    let id = *next_id;
    *next_id += 1;
    // Support both single-question (legacy) and multi-question (questions array) modes.
    let questions: Vec<AskQuestion> =
        if let Some(arr) = call.args.get("questions").and_then(|v| v.as_array()) {
            arr.iter()
                .map(|q| {
                    let header = q
                        .get("header")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let question = q
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let options: Vec<AskOption> = q
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|o| AskOption {
                                    label: o
                                        .get("label")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    description: o
                                        .get("description")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string()),
                                    recommended: o
                                        .get("recommended")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false)
                                        || o.get("label")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .contains("(Recommended)"),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let multiple = q.get("multiple").and_then(|v| v.as_bool()).unwrap_or(false);
                    let allow_free = q
                        .get("allow_free")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
                    // Also check top-level custom flag as alias
                    let allow_free = if q.get("custom").and_then(|v| v.as_bool()).is_some() {
                        q.get("custom").and_then(|v| v.as_bool()).unwrap()
                    } else {
                        allow_free
                    };
                    AskQuestion {
                        header,
                        question,
                        options,
                        multiple,
                        allow_free,
                    }
                })
                .collect()
        } else {
            let question = call
                .args
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let options: Vec<AskOption> = call
                .args
                .get("options")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|o| AskOption {
                            label: o
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            recommended: o
                                .get("recommended")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                                || o.get("label")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .contains("(Recommended)"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multiple = call
                .args
                .get("multiple")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let allow_free = call
                .args
                .get("allow_free")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            vec![AskQuestion {
                header: "".to_string(),
                question,
                options,
                multiple,
                allow_free,
            }]
        };

    // Small and open models often emit ask_user with no arguments at all.
    // An empty popup is useless to the user, so refuse the call and hand the
    // model the exact shape to retry with instead of blocking on the UI.
    if questions.is_empty() || questions.iter().any(|q| q.question.is_empty()) {
        return tools::Outcome::err(
            "ask_user rejected: 'question' is empty. Call it again with a non-empty question, \
             e.g. {\"question\": \"Which web framework should I use?\", \"options\": \
             [{\"label\": \"FastAPI\"}, {\"label\": \"Flask\"}], \"multiple\": false, \
             \"allow_free\": true} or with \"questions\": [{\"header\": \"Q1\", \"question\": \"...\", \"options\": [...] }].",
        );
    }

    if tx
        .send(AgentEvent::AskUser { id, questions })
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

/// `propose_reset`: the agent claims the plan itself is wrong and asks the
/// user to abandon it — through the same approval dialog as dangerous
/// commands (deny preselected), never a text "yes". The quoted plan defect
/// is validated before the dialog opens, so the user never confirms a blank
/// surrender; the old plan stays on disk as Abandoned and the evidence stays
/// journaled. A replacement, if any, goes through a fresh `plan create`
/// with all its gates — reset alone cannot smuggle one in.
#[allow(clippy::too_many_arguments)]
async fn propose_reset(
    call: &ToolCallReq,
    ctx: &mut tools::ToolCtx,
    read_only: bool,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
) -> tools::Outcome {
    let root = ctx.root.clone();
    let root = root.as_path();
    // the call itself writes nothing, but an approved reset is stored by
    // the host — which a read-only session must never do (same rule as
    // propose_plan: present the problem in the answer instead)
    if read_only {
        return tools::Outcome::err(
            "project is read-only because another sqwai instance owns the lock; \
             plan reset cannot be stored — describe the plan defect in your answer instead",
        );
    }
    let session_id = ctx.session_id.clone();
    let reason = call
        .args
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let mut active = match plan::open_active_for_session(root, Some(&session_id)) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            return tools::Outcome::err("no active plan: nothing to reset".to_string());
        }
        Err(error) => {
            return tools::Outcome::err(format!("active plan unreadable: {error:#}"));
        }
    };
    if let Err(rejection) = plan::validate_surrender_reason(reason, "proposing a reset") {
        return tools::Outcome::err(format!(
            "plan reset rejected [{}]: {} — {}",
            rejection.code, rejection.reason, rejection.hint
        ));
    }
    let plan_id = active.id.clone();
    let discards = plan::reset_discards(&active);
    let aid = *next_id;
    *next_id += 1;
    if tx
        .send(AgentEvent::Approval {
            id: aid,
            command: format!("abandon plan {plan_id}"),
            reason: format!(
                "quoted plan defect: {reason}\nDiscards: {discards}.\nHistory stays on disk as Abandoned; evidence stays journaled."
            ),
        })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed awaiting reset confirm");
    }
    let decision = loop {
        match ctl.recv().await {
            Some(ControlMsg::ApprovalAnswer { id, decision }) if id == aid => break decision,
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled awaiting reset confirm"),
        }
    };
    // blanket pre-approval is never honored for abandonment: a deliberate
    // "always" click still approves only this reset, said out loud
    let downgraded = decision == ApprovalDecision::AlwaysSession;
    if decision == ApprovalDecision::Deny {
        return tools::Outcome::err(
            "plan reset denied by user — keep working the active plan, or block it with a quoted conflict",
        );
    }
    match plan::apply(
        &mut active,
        plan::Op::ProposeReset {
            reason: reason.to_string(),
        },
        &plan::Limits::default(),
        ctx.current_step.as_deref(),
    ) {
        Ok(_) => {
            if let Err(error) = plan::store(&ctx.root, &active) {
                return tools::Outcome::err(format!("plan write failed: {error:#}"));
            }
            if let Err(error) = plan::commit(
                &ctx.root,
                &ctx.session_id,
                &mut active,
                "propose_reset",
                "user",
                true,
                serde_json::json!({"reason": reason, "plan_id": plan_id}),
            ) {
                return tools::Outcome::err(format!("plan write failed: {error:#}"));
            }
            // the session held a step of a dead plan
            ctx.current_step = None;
            let _ = tx.send(AgentEvent::StepCurrent { step: None }).await;
            tools::Outcome::ok(format!(
                "plan {plan_id} abandoned by user approval{}; start over with plan create",
                if downgraded {
                    " (always-allow treated as one-time: abandonment is never pre-approved)"
                } else {
                    ""
                }
            ))
        }
        Err(rejection) => tools::Outcome::err(format!(
            "plan reset rejected [{}]: {} — {}",
            rejection.code, rejection.reason, rejection.hint
        )),
    }
}

/// `propose_plan`: the agent proposes a full plan draft but writes nothing.
/// Two-stage host gate: the draft is validated before the user sees it (a
/// malformed draft rejects this call), then the user accepts or declines.
/// Accept abandons the active plan (if any) and stores the draft; decline
/// returns ok so the agent asks what was wrong and continues.
#[allow(clippy::too_many_arguments)]
async fn propose_plan(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    plan_limits: &crate::config::PlanConfig,
    context_limit: u64,
    read_only: bool,
    journal: &mut Option<crate::agent::journal::Journal>,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    next_id: &mut u64,
    session_id: &str,
) -> tools::Outcome {
    let root = ctx.root.clone();
    let root = root.as_path();
    // the call itself writes nothing, but an accepted proposal is stored by
    // the host — which a read-only session must never do (lock owned elsewhere)
    if read_only {
        return tools::Outcome::err(
            "project is read-only because another sqwai instance owns the lock; \
             plan proposals cannot be stored — present the plan in your answer instead",
        );
    }
    let id = *next_id;
    *next_id += 1;
    let mut draft_args: plan::PlanDraftArgs = match serde_json::from_value(call.args.clone()) {
        Ok(args) => args,
        Err(e) => {
            return tools::Outcome::err(format!(
                "plan proposal rejected: bad arguments ({e}) — send goal and steps"
            ));
        }
    };
    // Named verify commands expand here, before the draft is shown: what the
    // user approves has to be what gets stored, and `plan create` already
    // expands them at the same point.
    match plan::substitute_verify_commands(
        draft_args.acceptance,
        &crate::config::Config::project_verify_commands(root),
    ) {
        Ok(expanded) => draft_args.acceptance = expanded,
        Err(unknown) => {
            let hint = if unknown.known.is_empty() {
                "no verify commands seeded — run /init or write the command out".to_string()
            } else {
                format!("known: {}", unknown.known.join(", "))
            };
            return tools::Outcome::err(format!(
                "plan proposal rejected [unknown_verify]: acceptance refers to unknown \
                 verify command(s): ${} — {hint}",
                unknown.names.join(", $")
            ));
        }
    }
    let limits = plan::Limits {
        max_steps: plan_limits.max_steps,
    };
    let budget_limit = plan_limits
        .budget_tokens(context_limit)
        .max(tools::MIN_PLAN_BUDGET_TOKENS);
    let active_plan = match plan::open_active_for_session(root, Some(session_id)) {
        Ok(plan) => plan,
        Err(e) => return tools::Outcome::err(format!("active plan unreadable: {e:#}")),
    };
    if let Err(r) = plan::validate_proposal_invariants(active_plan.as_ref(), &draft_args) {
        return tools::Outcome::err(format!(
            "plan proposal violates invariants [{}]: {} — {}",
            r.code, r.reason, r.hint
        ));
    }
    let draft = match draft_args.build(budget_limit, &limits) {
        Ok(draft) => draft,
        Err(r) => {
            return tools::Outcome::err(format!(
                "plan proposal rejected [{}]: {} — {}",
                r.code, r.reason, r.hint
            ));
        }
    };
    if let Some(writer) = journal.as_mut() {
        let _ = writer.append(
            "plan",
            serde_json::json!({"op": "propose", "goal": draft.goal.text, "by": "model"}),
        );
    }
    if tx
        .send(AgentEvent::PlanProposal {
            id,
            draft: draft.clone(),
        })
        .await
        .is_err()
    {
        return tools::Outcome::err("tui closed while proposing");
    }
    let accept = loop {
        match ctl.recv().await {
            Some(ControlMsg::PlanAnswer { id: aid, accept }) if aid == id => break accept,
            Some(_) => continue,
            None => return tools::Outcome::err("agent cancelled while proposing"),
        }
    };
    if !accept {
        if let Some(writer) = journal.as_mut() {
            let _ = writer.append(
                "plan",
                serde_json::json!({"op": "decline_proposal", "by": "user"}),
            );
        }
        return tools::Outcome::ok(
            "the user declined the proposed plan. Ask what was wrong with it, adjust, \
             and propose again — do not stop working.",
        );
    }
    // Rebuild defensively: same args, same limits, fresh id. An accept can
    // only fail here on state that changed while the user was deciding.
    let mut fresh = match draft_args.build(budget_limit, &limits) {
        Ok(fresh) => fresh,
        Err(r) => {
            return tools::Outcome::err(format!(
                "accepted plan failed re-validation [{}]: {} — {}",
                r.code, r.reason, r.hint
            ));
        }
    };
    let abandoned = match plan::open_active_for_session(root, Some(session_id)) {
        Ok(Some(active)) => Some(active.id.clone()),
        Ok(None) => None,
        Err(e) => return tools::Outcome::err(format!("plan store unreadable: {e:#}")),
    };
    // §12.12: the same proof `plan create` takes, at the same moment — the
    // tree is still the pre-change one while the user is deciding.
    let proof = tools::capture_baselines(ctx, &fresh);
    plan::set_baselines(&mut fresh, proof.slots.clone());
    plan::set_snapshots(&mut fresh, proof.frozen.clone());
    plan::set_shapes(&mut fresh, proof.shapes.clone());
    plan::set_inputs(&mut fresh, proof.inputs.clone());
    // Journal-first (§2.1.4): the intent carries the full draft so replay
    // can rebuild the new plan and retire the old one after a crash.
    let new_id = fresh.id.clone();
    let new_created = fresh.created.clone();
    let intent_seq = if let Some(writer) = journal.as_mut() {
        writer
            .append(
                "plan",
                serde_json::json!({
                    "op": "accept_proposal",
                    "by": "user",
                    "ok": true,
                    "plan_id": new_id,
                    "draft": draft_args,
                    "baselines": proof.slots,
                    "snapshots": proof.frozen,
                    "shapes": proof.shapes,
                    "inputs": proof.inputs,
                    "new_id": new_id,
                    "new_created": new_created,
                    "new_sessions": [session_id],
                    "abandoned": abandoned,
                }),
            )
            .ok()
    } else {
        None
    };
    if let Some(old) = abandoned.clone() {
        match plan::open_active_for_session(root, Some(session_id)) {
            Ok(Some(mut active)) if active.id == old => {
                plan::abandon(&mut active);
                if let Some(seq) = intent_seq {
                    active.applied_event = Some(format!("{session_id}:{seq}"));
                }
                if let Err(e) = plan::store(root, &active) {
                    return tools::Outcome::err(format!(
                        "abandoning the previous plan failed: {e:#}"
                    ));
                }
            }
            Ok(_) => {}
            Err(e) => return tools::Outcome::err(format!("plan store unreadable: {e:#}")),
        }
    }
    fresh.sessions = vec![session_id.to_string()];
    if let Some(seq) = intent_seq {
        fresh.applied_event = Some(format!("{session_id}:{seq}"));
    }
    let steps = fresh.steps.len();
    if let Err(e) = plan::store(root, &fresh) {
        return tools::Outcome::err(format!("plan write failed: {e:#}"));
    }
    // tell the TUI the stored plan's id so it re-links the session before the
    // tool outcome is processed
    let _ = tx
        .send(AgentEvent::PlanAccepted { id: new_id.clone() })
        .await;
    // the TUI derives its todos panel and plan label from disk; push the new
    // plan's steps so they refresh without waiting for the next plan op
    let plan_todos: Vec<String> = fresh
        .steps
        .iter()
        .map(|s| format!("[{}] {}", s.status.as_str(), s.title))
        .collect();
    let _ = tx.send(AgentEvent::Todos(plan_todos)).await;
    tools::Outcome::ok(match abandoned {
        Some(old) => format!(
            "plan {new_id} accepted with {steps} steps; previous plan {old} abandoned. \
             Start its first step.{}",
            proof.notes.join("")
        ),
        None => format!(
            "plan {new_id} accepted with {steps} steps. Start its first step.{}",
            proof.notes.join("")
        ),
    })
}

#[allow(clippy::too_many_arguments)]
async fn bash_call(
    call: &ToolCallReq,
    ctx: &mut ToolCtx,
    tx: &mpsc::Sender<AgentEvent>,
    ctl: &mut mpsc::Receiver<ControlMsg>,
    always_allow: &mut Vec<String>,
    blocked: &[String],
    next_id: &mut u64,
    subagent_depth: u8,
) -> tools::Outcome {
    let command = call.args["command"].as_str().unwrap_or("").to_string();
    let lower = command.to_lowercase();

    // -1. writer-subagent scope: the loop's bash path bypasses
    // `tools::execute`, so the scope gate lives here too. Explicit
    // out-of-scope write targets refuse before any policy prompt.
    if ctx.subagent_step.is_some()
        && let Some(allowed) = ctx.subagent_write_paths.as_ref()
        && let Some(target) = tools::bash_scope_hit(ctx, allowed, &command)
    {
        return tools::Outcome::err(
            serde_json::json!({
                "ok": false,
                "code": "subagent_scope",
                "reason": format!(
                    "'{target}' is outside this subagent's declared write scope ({})",
                    allowed.join(", ")
                ),
                "hint": "stay inside the spawned scope, or spawn with wider paths",
            })
            .to_string(),
        );
    }

    // 0. hard block from config — no questions
    for pat in blocked {
        match regex::Regex::new(pat) {
            Ok(re) => {
                if re.is_match(&command) || re.is_match(&lower) {
                    return tools::Outcome::err(format!(
                        "command blocked by [safety].blocked_patterns '{pat}'"
                    ));
                }
            }
            Err(e) => {
                return tools::Outcome::err(format!(
                    "invalid [safety].blocked_patterns regex '{pat}': {e} - command blocked"
                ));
            }
        }
    }

    // 1. heuristic dangerous-command detector
    let mut needs_approval =
        match safety::classify_for(crate::agent::shell::ShellKind::detect(), &command) {
            safety::Verdict::Safe => None,
            safety::Verdict::Blocked(reason) => {
                return tools::Outcome::err(format!("error: {reason}"));
            }
            safety::Verdict::NeedsApproval(reason) => Some(reason.to_string()),
        };

    // 1b. trust gate (R, §2.2): external taint + an egress-shaped command
    // needs the same approval with a trust reason; headless contexts deny
    // instead. Runs after the safety verdict so a Blocked command never
    // reaches here; a command both dangerous and exfiltrating carries one
    // combined reason into the single dialog.
    match crate::agent::trust::trust_gate(
        &command,
        crate::agent::trust::taint_level(&ctx.root, &ctx.session_id).external,
        subagent_depth > 0,
    ) {
        crate::agent::trust::Gate::Allow => {}
        crate::agent::trust::Gate::Deny(reason) => {
            return tools::Outcome::err(format!("command denied ({reason})"));
        }
        crate::agent::trust::Gate::Confirm(reason) => {
            needs_approval = Some(match needs_approval {
                Some(safety) => format!("{safety}; {reason}"),
                None => reason,
            });
        }
    }

    // 1c. frozen check inputs: a shell write to a test/fixture the plan
    // froze needs the same approval; headless contexts deny instead.
    // Best-effort token matching — what slips past still meets the
    // receipt-time hash comparison.
    if let Some(hit) = tools::frozen_input_command_hit(&ctx.root, &ctx.session_id, &command) {
        needs_approval = Some(match needs_approval {
            Some(prior) => format!("{prior}; {hit}"),
            None => hit,
        });
    }

    // 1d. typed constraints, live half: `forbid-cmd:` refuses outright
    // (no approval dialog — the waiver is the override). Host-run
    // acceptance commands never pass through here, only model calls.
    if let Ok(Some(plan)) = plan::open_active_for_session(&ctx.root, Some(&ctx.session_id)) {
        let waived = plan::waived_constraint_indices(&plan);
        let patterns: Vec<String> = plan
            .constraints
            .iter()
            .enumerate()
            .filter(|(index, _)| !waived.contains(index))
            .filter_map(|(_, text)| match plan::classify_constraint(text) {
                plan::ConstraintKind::ForbidCmd(pattern) => Some(pattern.to_string()),
                _ => None,
            })
            .collect();
        if let Some(hit) = tools::forbidden_command(&patterns, &command) {
            return tools::Outcome::err(format!(
                "constraint_violated: command matches forbidden pattern '{hit}' — have the user waive it (/plan waive-constraint <index> <reason>)"
            ));
        }
    }

    if let Some(reason) = &needs_approval {
        if !always_allow.contains(&command) {
            if subagent_depth > 0 {
                return tools::Outcome::err(format!(
                    "dangerous command requires user approval, but subagents cannot prompt for approval ({reason})"
                ));
            }
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
        // checkpoint before running: dangerous-approved commands always;
        // otherwise only if the tree moved since this chain's last snapshot
        // (hash-gate, §2.5). The classifier cannot see mutations, only risk:
        // formatters and checkout-class commands look safe and mutate
        // silently, and must not run uninsured.
        // §2.5: bash is the one case whose targets cannot be known in
        // advance, so this is where layer 2 earns its existence.
        let snapshot_wanted = needs_approval.is_some()
            || checkpoints::tree_changed(&ctx.root, ctx.shadow_store, ctx.checkpoint_chain());
        if snapshot_wanted
            && let Ok(Some(sha)) = checkpoints::snapshot_session(
                &ctx.root,
                ctx.shadow_store,
                ctx.checkpoint_chain(),
                &format!("pre_bash {command}"),
            )
        {
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

pub(crate) fn is_accepted_memory_answer(answer_ok: bool, raw_answer: &str) -> bool {
    let raw = raw_answer.trim();
    let lower = raw.to_ascii_lowercase();
    answer_ok
        && (lower == "accept"
            || (!raw.is_empty()
                && lower != "reject"
                && lower != "edit"
                && !lower.starts_with("subagents cannot")))
}

#[cfg(test)]
mod subagent_tests {
    use super::*;

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

    /// Auto-reflector is parked: the auto path runs only with explicit
    /// opt-in. Anyone exporting SQWAI_AUTO_REFLECTOR changes product
    /// behavior, so the suite assumes a clean environment here.
    #[test]
    fn auto_reflector_stays_parked_without_opt_in() {
        assert!(
            !auto_reflector_enabled(),
            "auto path needs SQWAI_AUTO_REFLECTOR=1"
        );
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
                        match ev {
                            Some(AgentEvent::Approval { id, command, reason }) => {
                                assert!(command.contains("abandon plan"), "{command}");
                                assert!(reason.contains("removed feature"), "{reason}");
                                tx_ui
                                    .send(ControlMsg::ApprovalAnswer { id, decision })
                                    .await
                                    .unwrap();
                            }
                            _ => {}
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

    #[test]
    fn claim_extractors_find_counts_status_paths_symbols() {
        let counts = extract_counts("12 passed, 3 failed, suite 280/280 ok, version 2 here");
        let texts: Vec<&str> = counts.iter().map(|(_, s)| *s).collect();
        assert!(texts.contains(&"12 passed"), "{texts:?}");
        assert!(texts.contains(&"3 failed"), "{texts:?}");
        assert!(texts.contains(&"280/280"), "{texts:?}");
        // bare version number is not a claim
        assert!(!texts.contains(&"2"), "{texts:?}");

        let words = extract_status_words("Build SUCCEEDED, all green. All tests pass!");
        // "tests pass" nests inside "All tests pass" (deduped later in push_span)
        assert_eq!(words.len(), 4, "{words:?}");

        let paths = extract_paths("see src/main.rs, also `config.toml`, not v2.0 or e.g. this");
        assert!(paths.contains(&"src/main.rs"), "{paths:?}");
        assert!(paths.contains(&"config.toml"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("v2")), "{paths:?}");

        let syms = extract_symbols("call foo::bar and crate::x, not a::b: trailing");
        assert!(syms.contains(&"foo::bar"), "{syms:?}");
    }

    /// Byte-walking these on multibyte text panicked mid-char (a Cyrillic
    /// lead byte reads as alphanumeric). Regression: Russian prose passes
    /// through every extractor without panicking and still finds ASCII
    /// claims inside it.
    #[test]
    fn claim_extractors_survive_cyrillic() {
        let text = "Проверил: всё сломалось. Тесты: 12 passed, смотри src/main.rs и `config.toml`!";
        let counts = extract_counts(text);
        let texts: Vec<&str> = counts.iter().map(|(_, s)| *s).collect();
        assert!(texts.contains(&"12 passed"), "{texts:?}");
        let paths = extract_paths(text);
        assert!(paths.contains(&"src/main.rs"), "{paths:?}");
        assert!(paths.contains(&"config.toml"), "{paths:?}");
        // pure Cyrillic, no claims: silence, not a crash
        assert!(extract_counts("Привет, всё упало").is_empty());
        assert!(extract_paths("Привет, всё упало").is_empty());
        assert!(extract_status_words("Всё сломалось").is_empty());
        // nbsp between number and word (multibyte whitespace trap)
        let nbsp = extract_counts("3\u{a0}passed");
        assert!(nbsp.iter().any(|(_, s)| *s == "3\u{a0}passed"), "{nbsp:?}");
        // deletion guard with a mid-char rewind window
        assert!(path_deleted_nearby(
            "удалил src/a.rs после правок",
            "src/a.rs"
        ));
        assert!(!path_deleted_nearby("Привет, смотри src/a.rs", "src/a.rs"));
    }

    #[test]
    fn lint_answer_marks_only_contradictions() {
        let root = std::env::temp_dir().join(format!("sqwai-lint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        journal.append("user_msg", serde_json::json!({})).unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true, "summary": "12 passed"}),
            )
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "boom"}),
            )
            .unwrap();
        let mut jh = Some(journal);
        // "12 passed" is in the window: untouched. "99 passed" is absent
        // while a failure exists: marked. Quoted text never counts.
        let out = lint_answer(
            "Done: 12 passed and 99 passed.\n> user said 77 passed",
            &root,
            "sess",
            &mut jh,
        );
        assert!(
            out.contains("12 passed and 99 passed [unverified]"),
            "{out}"
        );
        assert!(!out.contains("12 passed [unverified]"), "{out}");
        assert!(!out.contains("77 passed [unverified]"), "{out}");
        // record written for the marked span only
        let recs = crate::agent::journal::Journal::records(&root).expect("read");
        let lints: Vec<_> = recs.iter().filter(|r| r.kind == "claim_lint").collect();
        assert_eq!(lints.len(), 1);
        assert_eq!(lints[0].fields["span"], "99 passed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The user's crash: Russian model text through the whole lint. Must
    /// neither panic nor mark what's in the journal.
    #[test]
    fn lint_answer_survives_russian_prose() {
        let root = std::env::temp_dir().join(format!("sqwai-lint-ru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        journal.append("user_msg", serde_json::json!({})).unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true, "summary": "12 passed"}),
            )
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "бум"}),
            )
            .unwrap();
        let mut jh = Some(journal);
        // "12 passed" is backed by the journal: no mark, no crash.
        // (A path claim without file evidence WOULD mark — that is the
        // lint working, not a bug — so the probe carries none.)
        let out = lint_answer("Готово: 12 passed. Ты что сделал?", &root, "sess", &mut jh);
        assert!(!out.contains("[unverified]"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Russian success phrases and sized claims mark exactly like English
    /// ones — under the same gates: status words only with no successful
    /// bash in the window, everything only beside a failure.
    #[test]
    fn lint_answer_marks_russian_status_and_sized_claims() {
        let root = std::env::temp_dir().join(format!("sqwai-lint-ru2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // fail-only window: nothing backs anything
        let mut journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        journal.append("user_msg", serde_json::json!({})).unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "бум"}),
            )
            .unwrap();
        let mut jh = Some(journal);
        let out = lint_answer("Готово: тесты прошли, 12 тестов.", &root, "sess", &mut jh);
        assert!(out.contains("тесты прошли [unverified]"), "{out}");
        assert!(out.contains("12 тестов [unverified]"), "{out}");

        // backed Russian count: journal summary carries it verbatim
        let mut journal2 = crate::agent::journal::Journal::open(&root, "sess2").expect("open");
        journal2.append("user_msg", serde_json::json!({})).unwrap();
        journal2
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true, "summary": "12 тестов прогнал"}),
            )
            .unwrap();
        journal2
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "бум"}),
            )
            .unwrap();
        let mut jh2 = Some(journal2);
        let out = lint_answer("Готово: 12 тестов прогнал.", &root, "sess2", &mut jh2);
        assert!(!out.contains("[unverified]"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Observed false positives from a real report: "10808/10809" is a
    /// shorthand for two separately reported ports (boundary + parts
    /// rules), and "services.msc -> ..." is advice to open something, not
    /// a claim that it exists in the project (arrow rule).
    #[test]
    fn lint_answer_skips_shorthand_counts_and_usage_pointers() {
        let root = std::env::temp_dir().join(format!("sqwai-lint-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        journal.append("user_msg", serde_json::json!({})).unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true, "summary": "TCP 127.0.0.1:10808 xray\nTCP 127.0.0.1:10809 xray"}),
            )
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "бум"}),
            )
            .unwrap();
        let mut jh = Some(journal);
        // extractor level: the port pair is an address tail, not a count
        assert!(
            extract_counts("слушает 127.0.0.1:10808/10809").is_empty(),
            "address tail must not extract"
        );
        // answer level: shorthand verified by parts, pointer skipped
        let out = lint_answer(
            "Локальный прокси 127.0.0.1:10808/10809, нормально. Узнать владельцев: `services.msc` -> пути и издатель.",
            &root,
            "sess",
            &mut jh,
        );
        assert!(!out.contains("[unverified]"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The guards above must not swallow real lies: an unbacked x/y count
    /// and a missing project file still mark.
    #[test]
    fn lint_answer_still_marks_unbacked_counts_and_paths() {
        let root = std::env::temp_dir().join(format!("sqwai-lint-tp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
        journal.append("user_msg", serde_json::json!({})).unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": true, "summary": "пил кофе"}),
            )
            .unwrap();
        journal
            .append(
                "tool_result",
                serde_json::json!({"tool": "bash", "ok": false, "summary": "бум"}),
            )
            .unwrap();
        let mut jh = Some(journal);
        let out = lint_answer(
            "Упало 7/9 тестов. Подробности в src/missing.rs.",
            &root,
            "sess",
            &mut jh,
        );
        assert!(out.contains("7/9 [unverified]"), "{out}");
        assert!(out.contains("src/missing.rs [unverified]"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
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