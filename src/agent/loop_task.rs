//! The agent loop (phase 2): drives LLM turns plus tool execution until the
//! model produces a final text answer with no more tool calls.
//!
//! Runs in its own tokio task, publishing [`AgentEvent`]s to the TUI and
//! receiving user interaction answers (ask_user, dangerous-command approval)
//! back through the [`ControlMsg`] channel. Aborting the task stops the agent.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::ThinkingLevel;
use crate::providers::{
    ChatRequest, Message, Role, SharedProvider, StreamEvent, ToolCallReq, Usage,
};

use crate::agent::tools::{self, ToolCtx};
use crate::agent::{checkpoints, safety};

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
    /// system prompt + full history + tool turns + final assistant answer
    pub messages: Vec<Message>,
    /// current to-do list written via todowrite
    pub todos: Vec<String>,
    /// (sha, label) checkpoints created by this run's mutations
    pub journal: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Usage(Usage),
    /// a tool just started: name + short arguments, spinner in the TUI
    ToolStart { name: String, summary: String },
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
    Checkpoint { label: String },
    /// the agent revised the visible to-do list
    Todos(Vec<String>),
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

pub struct AgentInput {
    pub provider: SharedProvider,
    pub model_id: String,
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<u32>,
    /// system prompt + history + the new user message
    pub messages: Vec<Message>,
    /// project root where tool paths are jailed
    pub root: PathBuf,
    /// hard-blocked command patterns from [safety].blocked_patterns
    pub blocked_patterns: Vec<String>,
    /// PLAN mode: read-only tools only, mutations are refused (design §5)
    pub plan_mode: bool,
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

struct TurnOutcome {
    text: String,
    calls: Vec<ToolCallReq>,
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
        mut messages,
        root,
        blocked_patterns,
        plan_mode,
    } = input;

    let mut ctx = ToolCtx::new(&root);
    let mut todos: Vec<String> = Vec::new();
    let mut always_allow: Vec<String> = Vec::new();
    let mut next_id: u64 = 0;

    let tools: Vec<crate::providers::ToolSpec> = tools::tool_specs();

    // The very first turn should not already contain tool results when
    // resuming a pure text history, but replaying tool turns stored in the
    // session could; we keep messages as passed — providers handle them.

    loop {
        // one complete streaming turn
        let turn = match run_turn(&provider, &ChatRequest {
            model_id: model_id.clone(),
            messages: messages.clone(),
            thinking,
            max_tokens,
            tools: tools.clone(),
        }, &tx, &mut ctl).await {
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
        messages.push(
            Message::new(Role::Assistant, turn.text).with_tool_calls(turn.calls.clone()),
        );

        // execute each call, feeding results back into the conversation
        for call in &turn.calls {
            let journal_mark = ctx.journal.len();
            // live row first: the TUI shows the tool name and its arguments
            // with a spinner while it runs (design §10)
            let _ = tx
                .send(AgentEvent::ToolStart {
                    name: call.name.clone(),
                    summary: tools::call_summary(&call.name, &call.args),
                })
                .await;

            let outcome = if plan_mode && tools::is_mutating(&call.name) {
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
                    "todowrite" => {
                        let items: Vec<String> = call.args["todos"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        todos = items.clone();
                        let _ = tx
                            .send(AgentEvent::Todos(items.clone()))
                            .await;
                        tools::Outcome::ok(format!("to-do saved ({} items)", items.len()))
                    }
                    other => tools::execute(&mut ctx, other, &call.args),
                }
            };

            let _ = tx
                .send(AgentEvent::ToolNotice {
                    name: call.name.clone(),
                    summary: outcome.output.clone(),
                    ok: outcome.ok,
                    diff: outcome.diff.clone(),
                })
                .await;
            // report a checkpoint taken by the mutation, if any
            if let Some((_, label)) = ctx.journal.last() {
                if ctx.journal.len() > journal_mark {
                    let _ = tx
                        .send(AgentEvent::Checkpoint { label: label.clone() })
                        .await;
                }
            }
            messages.push(Message::tool_result(&call.id, outcome.output, !outcome.ok));
        }
    }

    let _ = tx
        .send(AgentEvent::Completed(Ok(AgentOutcome {
            messages,
            todos,
            journal: ctx.journal,
        })))
        .await;
}

/// stream one request, retrying clean failures with backoff until it succeeds
/// or the retry window elapses
async fn run_turn(
    provider: &SharedProvider,
    req: &ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
    _ctl: &mut mpsc::Receiver<ControlMsg>,
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
                    if tx.send(AgentEvent::Usage(u)).await.is_err() {
                        return Err("tui closed".into());
                    }
                }
                Ok(StreamEvent::ToolCall(tc)) => calls.push(tc),
                Err(e) => failed = Some(format!("{e:#}")),
            }
            if failed.is_some() {
                break;
            }
        }

        let Some(err) = failed else {
            return Ok(TurnOutcome { text, calls });
        };

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
                    description: o["description"]
                        .as_str()
                        .map(|s| s.to_string()),
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
                return tools::Outcome::ok(text)
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
        let Ok(re) = regex::Regex::new(pat) else { continue };
        if re.is_match(&command) || re.is_match(&lower) {
            return tools::Outcome::err(format!(
                "command blocked by [safety].blocked_patterns '{pat}'"
            ));
        }
    }

    // 1. heuristic dangerous-command detector
    let needs_approval = match safety::classify(&command) {
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
                        break decision
                    }
                    Some(_) => continue,
                    None => return tools::Outcome::err("agent cancelled awaiting approval"),
                }
            };
            match decision {
                ApprovalDecision::Deny => {
                    return tools::Outcome::err(format!(
                        "command denied by user ({reason})"
                    ))
                }
                ApprovalDecision::AlwaysSession => always_allow.push(command.clone()),
                ApprovalDecision::RunOnce => {}
            }
        }
        // checkpoint before a dangerous (approved) command
        if let Ok(sha) = checkpoints::snapshot(
            &ctx.root,
            &format!("bash(approved) {command}"),
        ) {
            ctx.journal
                .push((sha, format!("bash {command}")));
        }
    }

    // 2. run it through the normal bash handler
    tools::execute(ctx, "bash", &call.args)
}
