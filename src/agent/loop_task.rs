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

use crate::config::ThinkingLevel;
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
}

impl AgentHandle {
    pub fn abort(&self) {
        self.abort.abort();
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
    pub thinking: Option<ThinkingLevel>,
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
fn request_messages(messages: &[Message], transport: ContextTransport) -> Vec<Message> {
    if transport != ContextTransport::PreviousResponse {
        return messages.to_vec();
    }
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .cloned()
        .into_iter()
        .collect()
}

struct TurnOutcome {
    text: String,
    calls: Vec<ToolCallReq>,
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
    if tasks.is_empty() {
        if let Some(task) = args["task"]
            .as_str()
            .map(str::trim)
            .filter(|task| !task.is_empty())
        {
            tasks.push(task.to_string());
        }
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

async fn run_subagent(
    call: &ToolCallReq,
    parent_tx: &mpsc::Sender<AgentEvent>,
    provider: &SharedProvider,
    model_id: &str,
    root: &Path,
    blocked_patterns: &[String],
    plan_mode: bool,
    context_limit: u64,
    thinking: Option<ThinkingLevel>,
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
                        thinking,
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
        thinking,
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

pub fn spawn_agent(input: AgentInput) -> AgentHandle {
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    let (ctl_tx, ctl_rx) = mpsc::channel::<ControlMsg>(32);
    let abort = tokio::spawn(run_agent(input, tx, ctl_rx)).abort_handle();
    AgentHandle {
        rx,
        control: ctl_tx,
        abort,
    }
}

async fn run_agent(
    input: AgentInput,
    tx: mpsc::Sender<AgentEvent>,
    mut ctl: mpsc::Receiver<ControlMsg>,
) {
    let AgentInput {
        provider,
        model_id,
        thinking,
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
    let policy = context::Policy::new(context_limit);
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
    let transport = crate::providers::select_transport(caps, previous_response_id.as_deref());

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
        if let Some((_, _, summarized)) = outcome.as_ref() {
            if let Some(writer) = compaction_journal.as_mut() {
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

    let mut ctx = ToolCtx::with_read_only(&root, read_only);
    let mut journal = if enable_tools && !read_only {
        crate::agent::journal::Journal::open(&root, &session_id).ok()
    } else {
        None
    };
    if let Some(writer) = journal.as_mut() {
        let plan_id = plan::open_active(&root).ok().flatten().map(|p| p.id);
        writer.set_attribution(None, plan_id, "main");
        let _ = writer.session_start(
            &model_id,
            if plan_mode { "plan" } else { "act" },
            None,
            "unknown",
            None,
        );
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
            let _ = tx
                .send(AgentEvent::Compaction {
                    summarized,
                    before,
                    after,
                })
                .await;
        }

        let mut turn_system = system.clone();
        if let Ok(Some(nudge)) = crate::agent::journal::Journal::nudge(&root, 8) {
            turn_system.push(crate::providers::SystemPart::volatile(nudge));
        }
        let request_messages = request_messages(&messages, transport);
        let breakdown = RequestBreakdown::from_request(&ChatRequest {
            model_id: model_id.clone(),
            system: turn_system.clone(),
            messages: request_messages.clone(),
            thinking,
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
                thinking,
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
            Ok(t) => t,
            Err(e) => {
                let _ = tx.send(AgentEvent::Completed(Err(e))).await;
                break;
            }
        };

        if turn.calls.is_empty() {
            // final answer
            messages.push(Message::new(Role::Assistant, turn.text));
            break;
        }

        // assistant requested tools; record the call(s)
        messages.push(Message::new(Role::Assistant, turn.text).with_tool_calls(turn.calls.clone()));

        // execute each call, feeding results back into the conversation
        for call in &turn.calls {
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
                        active.and_then(|p| {
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
                    "ask_user" => ask_user(&call, &tx, &mut ctl, &mut next_id).await,
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
                            thinking,
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
                                let question = ToolCallReq {
                                    id: call.id.clone(),
                                    name: "ask_user".into(),
                                    args: serde_json::json!({
                                        "question": prompt,
                                        "options": [
                                            {"label": "accept", "description": "write the proposal"},
                                            {"label": "edit", "description": "provide replacement text"},
                                            {"label": "reject", "description": "do not write it"}
                                        ],
                                        "multiple": false,
                                        "allow_free": true
                                    }),
                                };
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
                        if outcome.ok {
                            if let Ok(Some(saved)) = plan::open_active(&root) {
                                plan_todos = saved
                                    .steps
                                    .iter()
                                    .map(|step| {
                                        format!("[{}] {}", step.status.as_str(), step.title)
                                    })
                                    .collect();
                                let _ = tx.send(AgentEvent::Todos(plan_todos.clone())).await;
                            }
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
                            },
                            Err(e) => tools::Outcome::err(format!("MCP call failed: {e:#}")),
                        }
                    }
                    other => run_tool_blocking(&mut ctx, other, &call.args).await,
                }
            };

            if outcome.ok && matches!(call.name.as_str(), "write" | "edit" | "multi_edit") {
                if let Some(manager) = lsp_manager.as_mut() {
                    if let Some(path) = call.args.get("file_path").and_then(|v| v.as_str()) {
                        let path = root.join(path);
                        if let Ok(text) = tokio::fs::read_to_string(&path).await {
                            let _ = manager.did_change(&path, &text).await;
                            let _ = manager.did_save(&path).await;
                            tokio::task::yield_now().await;
                            let diagnostics =
                                manager.collect_diagnostics().await.unwrap_or_default();
                            let diagnostic_count = diagnostics
                                .iter()
                                .map(|item| item.diagnostics.len())
                                .sum::<usize>();
                            let _ = tx
                                .send(AgentEvent::Diagnostics {
                                    count: diagnostic_count,
                                })
                                .await;
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
                    }));
                }
                let result_seq = writer.append("tool_result", serde_json::json!({
                    "tool": call.name,
                    "call_id": call.id,
                    "ok": outcome.ok,
                    "duration_ms": tool_started.elapsed().as_millis(),
                    "summary": outcome.output.chars().take(200).collect::<String>(),
                    "trust": if matches!(call.name.as_str(), "webfetch" | "websearch") { "low" } else { "high" },
                })).ok();
                if call.name == "plan" && outcome.ok {
                    if let Some(seq) = result_seq {
                        let _ = writer.append("plan_evidence", serde_json::json!({
                            "op": call.args.get("op").and_then(|v| v.as_str()).unwrap_or("unknown"),
                            "evidence": [seq],
                        }));
                    }
                }
                if let Some(metadata) = outcome.file_diff.as_ref() {
                    let _ = writer.append(
                        "file_diff",
                        serde_json::json!({
                            "path": metadata.path,
                            "added": metadata.added,
                            "removed": metadata.removed,
                            "hash_before": metadata.hash_before,
                            "hash_after": metadata.hash_after,
                            "mode": metadata.mode,
                            "checkpoint": metadata.checkpoint,
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
                            Some(Duration::from_secs(diary.timeout_secs)),
                        )
                        .await;
                    }
                }
            }
            messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
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
    let (older, keep) = context::split_for_summary(messages);
    let older: Vec<Message> = older.to_vec();
    let keep: Vec<Message> = keep.to_vec();
    let mut summarized = false;
    if !older.is_empty() {
        let request = ChatRequest {
            model_id: model_id.to_string(),
            system: vec![SystemPart::volatile(context::SUMMARY_SYSTEM)],
            messages: vec![Message::new(
                Role::User,
                context::summary_input(&older, summary.as_deref()),
            )],
            thinking: None,
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

/// stream one request, retrying clean failures with backoff until it succeeds
/// or the retry window elapses
async fn run_turn(
    provider: &SharedProvider,
    req: &ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
    _ctl: &mut mpsc::Receiver<ControlMsg>,
    response_id: &mut Option<String>,
    prompt_size: &mut u64,
) -> Result<TurnOutcome, String> {
    use futures::StreamExt;

    let mut attempt: u32 = 0;
    let mut deadline: Option<Instant> = None;

    loop {
        let mut got_delta = false;
        let mut failed: Option<String> = None;
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
                            return Err("tui closed".into());
                        }
                    }
                }
                Ok(StreamEvent::Reasoning(t)) => {
                    if !t.is_empty() {
                        got_delta = true;
                        if tx.send(AgentEvent::ThinkingDelta(t)).await.is_err() {
                            return Err("tui closed".into());
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
                    if tx.send(AgentEvent::Usage(u)).await.is_err() {
                        return Err("tui closed".into());
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
                Err(e) => failed = Some(format!("{e:#}")),
            }
            if failed.is_some() {
                break;
            }
        }

        let Some(err) = failed else {
            return Ok(TurnOutcome { text, calls });
        };

        // A deterministic 4xx request/schema error cannot be fixed by retrying.
        // In particular, some OpenAI-compatible chat endpoints reject
        // function tools together with reasoning_effort and require /responses
        // or reasoning disabled.
        if err.contains("provider returned 400 Bad Request")
            || err.contains("invalid_request_error")
            || err.contains("Function tools with reasoning_effort are not supported")
        {
            return Err(err);
        }

        if got_delta {
            // partial answer already streamed; a retry would duplicate it
            return Err(format!("{err} — partial answer kept, not retried"));
        }

        let now = Instant::now();
        let dl = *deadline.get_or_insert(now + RETRY_WINDOW);
        if now >= dl {
            return Err(format!("{err} — giving up after 1h of retries"));
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
            return Err("tui closed".into());
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
        if let Ok(sha) = checkpoints::snapshot(&ctx.root, &format!("bash(approved) {command}")) {
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
