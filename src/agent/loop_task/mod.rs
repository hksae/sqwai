//! The agent loop (phase 2): drives LLM turns plus tool execution until the
//! model produces a final text answer with no more tool calls.
//!
//! Runs in its own tokio task, publishing [`AgentEvent`]s to the TUI and
//! receiving user interaction answers (ask_user, dangerous-command approval)
//! back through the [`ControlMsg`] channel. Aborting the task stops the agent.

use std::path::PathBuf;
use std::time::Instant;

use sha2::Digest;
use tokio::sync::mpsc;

use crate::config::EffortLevel;
use crate::providers::{
    ChatRequest, Message, RequestBreakdown, Role, SharedProvider, SystemPart, ToolCallReq, Usage,
};

use crate::agent::checkpoints;
use crate::agent::context;
use crate::agent::tools::{self, ToolCtx};
use crate::plan;
use loop_ask::{ask_user, bash_call, run_tool_blocking};
use loop_compact::{
    CompactionPrefix, compact_history, effort_ignored_reason, plan_hint_for_summary,
    record_compaction, turn_shows_no_reasoning,
};
pub(crate) use loop_compact::{capture_nudge, has_restriction_marker};
use loop_subagent::{
    SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step, run_subagent, run_subagent_batch,
};
pub(crate) use loop_turn::TurnOutcome;
use loop_turn::{request_messages_with_limit, run_turn, turn_transport};

mod loop_ask;
mod loop_compact;
mod loop_subagent;
mod loop_turn;

/// How much one `ask_user` call may hold. These are the only numbers for it:
/// the tool schema the model reads is built from them, and the runtime guard
/// refuses a call that breaks them — they had already drifted (schema said 4
/// questions, the loop accepted 8 and dropped the rest without telling anyone).
pub const ASK_MAX_QUESTIONS: usize = 4;
pub const ASK_MIN_OPTIONS: usize = 2;
pub const ASK_MAX_OPTIONS: usize = 5;

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
    /// A turn streamed reasoning after an observed zero-reasoning run stamped
    /// the level as ignored: the observation is contradicted, so the notice
    /// must go. A rejected parameter is never cleared this way — that verdict
    /// came from the provider, and reasoning that arrives anyway is the
    /// model's own, not the level the user asked for.
    EffortHonoured,
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
    /// the model asked the user structured questions; answer via ControlMsg.
    /// `call_id` names the call whose `ToolStart` row the questionnaire fills —
    /// an ask is a tool call like any other, not a separate surface.
    AskUser {
        id: u64,
        call_id: String,
        questions: Vec<AskQuestion>,
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
    /// live transcript sync: the full conversation so far, sent at the top
    /// of every turn-loop iteration (main agent only). A hard abort kills
    /// the task mid-request, so without this the whole turn's tool traffic
    /// dies with it and resume starts amnesiac — the TUI persists every
    /// sync, keeping at most one in-flight request unrecorded.
    TranscriptSync {
        messages: Vec<Message>,
        summary: Option<String>,
    },
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
    /// Slow baseline capture runs in the background instead of blocking
    /// plan create/add_acceptance. True on the interactive TUI path only —
    /// bench, tests and subagents keep the synchronous capture so results
    /// stay deterministic.
    pub background_baselines: bool,
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
    /// memory proposal limits copied from configuration
    pub memory: crate::config::MemoryConfig,
    /// `[web].allow_hosts`: the user's opt-in loopback targets for webfetch.
    /// Empty means the SSRF gate refuses every local host, as before.
    pub web_allow_hosts: Vec<String>,
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

/// Marker of the advisory note appended to a first identical re-run.
const REPEAT_NOTE_MARK: &str = "\n[host: you already called";
/// Marker of the stub that replaces a repeated result entirely (§18.1.8).
const REPEAT_STUB_MARK: &str = "[host: not re-reported";

/// What the host decides to do with a result the model has already seen
/// byte-for-byte.
#[derive(Debug)]
enum RepeatVerdict {
    /// nothing like it in this session — report as-is
    Fresh,
    /// first identical re-run: report the bytes, say so once
    Note(String),
    /// second identical re-run after being told: the bytes are withheld, a
    /// pointer to the earlier copies stays in their place
    Withheld(String),
}

/// Repeat handling for any tool outcome (§18.1.8). Compares full outputs, not
/// journal summaries (truncated to 200 chars), so same-headed but
/// different-tailed outputs never match.
///
/// Two earlier versions let the loop through. The guard covered `bash` only,
/// which is how a live session spent ten turns on `git status` and five
/// byte-identical `git_diff` calls with no pushback. And it stopped at an
/// advisory note: the same session then produced four requests with identical
/// `completion=204 reasoning=146` — a note attached to 18,846 repeating bytes is
/// not a brake, the bytes are the fuel. Withholding them ends the loop on the
/// third look no matter how degenerately the model decodes.
#[cfg(test)]
fn repeat_verdict(messages: &[Message], call: &ToolCallReq, output: &str) -> RepeatVerdict {
    repeat_verdict_with_limit(messages, call, output, 0)
}

fn repeat_verdict_with_limit(
    messages: &[Message],
    call: &ToolCallReq,
    output: &str,
    context_limit: u64,
) -> RepeatVerdict {
    // Repeating is the mechanism here: polling a job and waiting are supposed
    // to be called again.
    if matches!(call.name.as_str(), "bash_output" | "sleep") {
        return RepeatVerdict::Fresh;
    }
    let key = call_key(call);
    if key.is_empty() || output.is_empty() {
        return RepeatVerdict::Fresh;
    }
    let cleared = crate::agent::tool_output::cleared_indices_with_limit(messages, context_limit);
    // (call key, output) pairs in order; the current call has no result yet,
    // so everything collected here is older. Only results still visible on the
    // wire count: never tell the model "see above" for output that was cleared.
    let mut matches = 0usize;
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for tc in &message.tool_calls {
            let Some(previous) = call_key_ref(tc) else {
                continue;
            };
            if previous != key {
                continue;
            }
            let out = messages
                .iter()
                .enumerate()
                .find(|(idx, m)| {
                    m.role == Role::Tool
                        && m.tool_call_id.as_deref() == Some(&tc.id)
                        && !cleared.contains(idx)
                })
                .map(|(_, m)| strip_host_repeat_marks(&m.content))
                .unwrap_or("");
            if out == output {
                matches += 1;
            }
        }
    }
    match matches {
        0 => RepeatVerdict::Fresh,
        1 => RepeatVerdict::Note(format!(
            "{REPEAT_NOTE_MARK} {} with these exact arguments earlier in this \
             session and got byte-identical output — reuse that observation instead of \
             re-running it. If the underlying state may have changed since, ignore this note.]",
            call.name
        )),
        seen => RepeatVerdict::Withheld(format!(
            "{REPEAT_STUB_MARK} — this ran, and its {} bytes are byte-identical to the {seen} \
             earlier results for the same arguments, which are already above. Reading the same \
             state a third time tells you the same thing: decide from what you have, or change \
             what you are looking at.]",
            output.len()
        )),
    }
}

/// Strip the host's own repeat markers before comparing outputs: the note rides
/// on the first identical re-run, so without this the second re-run would no
/// longer match the first and the counter would restart every iteration.
fn strip_host_repeat_marks(output: &str) -> &str {
    let cut = [REPEAT_NOTE_MARK, REPEAT_STUB_MARK]
        .iter()
        .filter_map(|mark| output.find(mark))
        .min();
    match cut {
        Some(at) => output[..at].trim_end(),
        None => output,
    }
}

/// One journal record per snapshot commit, named for what actually took it.
///
/// Every entry here is a shadow-git snapshot (`snapshot_session` for mutations,
/// `snapshot_boundary` for step edges), so the layer is stated once and the
/// reason carries the difference. Step tags arrive through the same vector as
/// mutations because `plan` snapshots its own boundaries — reading the label
/// is how the two stop being journaled twice, once as a step edge and once as
/// a phantom "post_mutation".
fn checkpoint_fields(sha: &str, label: &str) -> serde_json::Value {
    if let Some(step) = label.strip_prefix("step_") {
        for (suffix, reason) in [("_start", "step_start"), ("_finish", "step_finish")] {
            if let Some(id) = step.strip_suffix(suffix) {
                return serde_json::json!({
                    "layer": "shadow",
                    "id": sha,
                    "reason": reason,
                    "step": id,
                });
            }
        }
    }
    serde_json::json!({
        "layer": "shadow",
        "id": sha,
        "reason": "post_mutation",
        "label": label,
    })
}

/// What makes two calls the same call: for `bash` the command line alone (a
/// different `timeout` is the same observation attempt), for everything else
/// the tool name plus its canonicalized arguments — and the name alone when it
/// takes no arguments. `git_diff({})` five times is exactly the loop the guard
/// exists for, so an argument-less call is a repeat risk like any other; only
/// an empty result has nothing to match on.
fn call_key(call: &ToolCallReq) -> String {
    call_key_ref(call).unwrap_or_default()
}

fn call_key_ref(call: &ToolCallReq) -> Option<String> {
    if call.name == "bash" {
        let command = call.args.get("command")?.as_str()?.trim();
        return (!command.is_empty()).then(|| format!("bash {command}"));
    }
    let args = call.args.as_object()?;
    if args.is_empty() {
        return Some(call.name.clone());
    }
    Some(format!(
        "{} {}",
        call.name,
        serde_json::to_string(&call.args).ok()?
    ))
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
        mut system,
        mut messages,
        root,
        session_id,
        blocked_patterns,
        web_allow_hosts,
        plan_mode,
        mut context_limit,
        enable_tools,
        background_baselines,
        read_only,
        mut previous_response_id,
        mut summary,
        compact_only,
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

    // Compile the hard-block patterns once per run. The gate used to call
    // `Regex::new` on every bash command (#184); a pattern that does not
    // compile still stops the run right here, exactly as before, so the gate
    // never has to decide what an invalid config means mid-turn.
    let mut blocked: Vec<(String, regex::Regex)> = Vec::with_capacity(blocked_patterns.len());
    for pat in &blocked_patterns {
        match regex::Regex::new(pat) {
            Ok(re) => blocked.push((pat.clone(), re)),
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Completed(Err(format!(
                        "invalid [safety].blocked_patterns regex '{pat}': {e}"
                    ))))
                    .await;
                return;
            }
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
        matches!(compaction.summary, crate::config::CompactionSummary::Short),
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

    if !compact_only && !system.iter().any(|p| p.text.contains("<turn_environment>")) {
        let probe_root = root.clone();
        if let Ok(facts) =
            tokio::task::spawn_blocking(move || crate::prompts::env::turn_facts(&probe_root)).await
        {
            let pos = system
                .iter()
                .position(|p| {
                    !p.cacheable && (p.text.starts_with("ANCHOR ") || p.text.starts_with("Note: "))
                })
                .unwrap_or(system.len());
            system.insert(pos, crate::providers::SystemPart::volatile(facts));
        }
    }

    // `/compact` — run the policy and hand the transcript back without a chat
    // turn. It used to write a mandatory diary entry first; the entry was the
    // only thing that made the compaction visible to the next session, and the
    // writer never produced a line of prose (§20).
    if compact_only {
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
        .with_cancel(cancel_tool);
    // slow baseline capture defers to the background worker on interactive
    // turns; everywhere else the tool call captures synchronously
    ctx.background_baselines = background_baselines;
    // A child's shadow snapshots belong on the parent chain (§2.2.4), so the
    // parent's `/undo` sees step boundaries and bash mutations; the journal,
    // job isolation and read-guard stay on the child's own session.
    ctx.checkpoint_session = parent_session.clone();
    // Subagents inherit their spawn context (§2.2.4): it stamps their
    // journal records and gates their mutations against reopen races.
    ctx.subagent_step = parent_step.clone();
    // ...and their parent's trust state: a fresh child journal reads
    // clean, and the task text itself may be shaped by what the parent
    // consumed (audit H1).
    ctx.parent_session = parent_session.clone();
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
    let mut always_allow: Vec<loop_ask::Grant> = Vec::new();
    let mut memory_writes_this_turn: u8;
    let mut next_id: u64 = 0;
    // prompt size of the last request, as reported by the provider
    let mut prompt_size: u64 = 0;
    // One forced compaction per oversized request, reset after every turn that
    // gets through: without the guard a request that stays too large would
    // compact in a loop.
    let mut compacted_for_overflow = false;
    // one record and one status line per session, not per turn
    let mut effort_ignored_reported = false;
    // the reported verdict was an observation (zero reasoning), not a refused
    // parameter: only an observation can be contradicted by a later turn
    let mut effort_ignored_observed = false;
    // consecutive turns that asked for effort and came back with no reasoning
    let mut zero_reasoning_turns: u32 = 0;

    loop {
        // Transcript sync for hard-abort survival (main agent only — a
        // child's transcript must never overwrite its parent's): every
        // iteration persists the conversation so far, so killing the task
        // loses at most the in-flight request.
        if subagent_depth == 0 {
            let _ = tx
                .send(AgentEvent::TranscriptSync {
                    messages: messages.clone(),
                    summary: summary.clone(),
                })
                .await;
        }
        // The write limit applies to one model request/turn, not the whole
        // session. A new request gets a fresh allowance.
        memory_writes_this_turn = 0;
        // Compaction gate. History only: the provider's full request size
        // is observed here, but the gate must use
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
        // The plan is the model's only plan source, rebuilt live on every
        // request: a turn-start snapshot would predate this turn's plan
        // mutations. Both halves ride the volatile tail. The goal used to be a
        // cached part on the theory that it churns only on rewrite — but
        // `create` and `complete` are rewrites of a kind, and each one
        // re-keyed the whole cached prefix: measured at ±531 B per plan
        // lifecycle, and behind that key sits the entire conversation.
        if let Some(plan) = crate::prompts::plan_block(&root, Some(&session_id)) {
            turn_system.push(crate::providers::SystemPart::volatile(plan));
        }
        if let Some(status) = crate::prompts::plan_status_block(&root, Some(&session_id)) {
            turn_system.push(crate::providers::SystemPart::volatile(status));
        }
        // No active plan: the host says nothing about plans. The convention
        // "no plan block means no plan, do not reconstruct one from history" is
        // a system.md rule, stated once — a per-request note naming the closed
        // plan would inject plan vocabulary exactly where the plan is gone
        // (§18.1.6).
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
        let request_messages = request_messages_with_limit(&messages, transport, context_limit);
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
                    // streamed reasoning contradicts an observed verdict: the
                    // chrome must stop saying "ignored" while thoughts are
                    // visibly arriving
                    if effort_ignored_observed {
                        effort_ignored_observed = false;
                        let _ = tx.send(AgentEvent::EffortHonoured).await;
                    }
                }
                if let Some(level) = effort
                    && !effort_ignored_reported
                    && let Some(why) = effort_ignored_reason(&turn, zero_reasoning_turns)
                {
                    effort_ignored_reported = true;
                    effort_ignored_observed = !turn.effort_rejected;
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
                // Stage T: Provider Fallback Chain on retry-exhausted network / 5xx error (§5.1, §7 T).
                // A turn whose text was already streamed to the user never
                // falls back: re-running it on another provider would show a
                // second answer on top of the partial one (audit H8).
                let can_fallback = !failure.partial
                    && (matches!(
                        failure.class,
                        Some(ErrorClass::Network | ErrorClass::Server)
                    ) || failure.retries > 0);
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
                        matches!(compaction.summary, crate::config::CompactionSummary::Short),
                    );
                    previous_response_id = None;
                    // The new provider gets its own overflow budget: its
                    // context_limit may be smaller and the already-compacted
                    // history may still not fit — without this reset the
                    // forced compaction is refused and the turn dies (audit
                    // H7). The continuation refusal belonged to the OLD
                    // provider; the fallback may well support references
                    // (audit M8).
                    compacted_for_overflow = false;
                    continuation_usable = true;
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
            messages.push(
                Message::new(Role::Assistant, text)
                    .with_provider_state(turn.provider_state)
                    .with_thinking(turn.reasoning_text, turn.reasoning_ms),
            );
            break;
        }

        // assistant requested tools; record the call(s)
        messages.push(
            Message::new(Role::Assistant, turn.text)
                .with_tool_calls(turn.calls.clone())
                .with_provider_state(turn.provider_state)
                .with_thinking(turn.reasoning_text, turn.reasoning_ms),
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
                &web_allow_hosts,
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
                            "args": tools::call_args_for_journal(&call.args),
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
                } else {
                    match call.name.as_str() {
                        "ask_user" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot interact with the user; make decisions autonomously",
                        ),
                        "ask_user" => ask_user(call, &tx, &mut ctl, &mut next_id).await,
                        "bash" => {
                            bash_call(
                                call,
                                &mut ctx,
                                &tx,
                                &mut ctl,
                                &mut always_allow,
                                &blocked,
                                &mut next_id,
                                subagent_depth,
                                &mut journal,
                            )
                            .await
                        }
                        "webfetch" | "websearch" => {
                            let mut outcome = if call.name == "webfetch" {
                                tools::web::fetch(&call.args, &web_allow_hosts).await
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
                                &web_allow_hosts,
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
                        "memory_write" if subagent_depth > 0 => tools::Outcome::err(
                            "subagents cannot write durable memories; memories belong to the primary session",
                        ),
                        "memory_write" => {
                            memory_writes_this_turn = memory_writes_this_turn.saturating_add(1);
                            if memory_writes_this_turn > memory.max_writes_per_turn {
                                tools::Outcome::err("memory write limit reached for this turn")
                            } else {
                                let text = call.args["text"].as_str().unwrap_or_default().trim();
                                match crate::agent::memory::Scope::parse(
                                    call.args["scope"].as_str().unwrap_or("project"),
                                )
                                .and_then(|scope| {
                                    crate::agent::memory::apply_proposal(
                                        &root,
                                        scope,
                                        call.args["section"].as_str().unwrap_or("Project"),
                                        text,
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
                            }
                        }
                        "recall_result" => crate::agent::tool_output::recall_with_limit(
                            &messages,
                            call.args["call_id"].as_str().unwrap_or_default(),
                            context_limit,
                        ),
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
                                // R banner (§2.2): server-authored bytes are
                                // untrusted content even when the server
                                // labels them an error (audit H11) — unlike
                                // webfetch failures, which the host
                                // generates. Only the transport Err below
                                // stays bare.
                                Ok((output, is_error)) => {
                                    crate::mcp::result_outcome(output, is_error)
                                }
                                Err(e) => tools::Outcome::err(format!("MCP call failed: {e:#}")),
                            }
                        }
                        other => {
                            if other == "read" {
                                let cleared =
                                    crate::agent::tool_output::cleared_indices_with_limit(
                                        &messages,
                                        context_limit,
                                    );
                                let visible_ids: std::collections::HashSet<String> = messages
                                    .iter()
                                    .enumerate()
                                    .filter(|(idx, m)| {
                                        m.role == Role::Tool && !cleared.contains(idx)
                                    })
                                    .filter_map(|(_, m)| m.tool_call_id.clone())
                                    .collect();
                                ctx.retain_visible_read_windows(&visible_ids);
                            }
                            ctx.current_call_id = Some(call.id.clone());
                            let out = run_tool_blocking(&mut ctx, other, &call.args).await;
                            ctx.current_call_id = None;
                            out
                        }
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
                        ctx.shadow_store,
                        ctx.checkpoint_chain(),
                        "post_bash cancelled",
                    )
                {
                    ctx.journal.push((sha, "bash (cancelled)".to_string()));
                }

                // #185: which files to re-lint is taken from what the call
                // reported touching, not from a list of tool names. Every
                // file-mutating tool hands back a `FileDiff` — `write`, `edit`,
                // `multi_edit`, and `patch` for one or many files — so a tool
                // added later cannot be forgotten here the way `patch` was.
                // `bash`, `git_commit`, `git_stage` and `plan` have no per-call
                // path (their effects are attributed through the shadow head, or
                // they touch no source file); the specs test makes that split an
                // explicit, reviewable decision.
                if outcome.ok
                    && crate::agent::tools::is_mutating_call(&call.name, &call.args)
                    && let Some(manager) = lsp_manager.as_mut()
                {
                    let touched = lint_targets(&outcome, &root);
                    if !touched.is_empty() {
                        for path in &touched {
                            if let Ok(text) = tokio::fs::read_to_string(path).await {
                                let _ = manager.did_change(path, &text).await;
                                let _ = manager.did_save(path).await;
                            }
                        }
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
                            let rel = |p: &std::path::Path| {
                                p.strip_prefix(&root)
                                    .unwrap_or(p)
                                    .to_string_lossy()
                                    .replace('\\', "/")
                            };
                            let _ = writer.append_evidence(
                                "diagnostics",
                                serde_json::json!({
                                    // `path` stays the first linted file for the
                                    // readers that were written against one file
                                    // per record; `paths` carries all of them.
                                    "path": rel(&touched[0]),
                                    "paths": touched.iter().map(|p| rel(p)).collect::<Vec<_>>(),
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
                            let _ = writer.append("checkpoint", checkpoint_fields(sha, label));
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
                            // `plan` snapshots its own boundaries and pushes the
                            // commit into ctx.journal, which is where the record
                            // above was written. Re-appending here journaled the
                            // same commit twice under two different layers.
                            let already = ctx.journal.last().is_some_and(|(_, t)| t == &tag);
                            if !already
                                && let Some(sha) = checkpoints::snapshot_boundary(
                                    &root,
                                    shadow_store,
                                    parent_session.as_deref().unwrap_or(&session_id),
                                    &tag,
                                )
                                .ok()
                                .flatten()
                            {
                                ctx.journal.push((sha.clone(), tag.clone()));
                                let _ = writer.append("checkpoint", checkpoint_fields(&sha, &tag));
                            }
                        } else if outcome.ok && matches!(op, "finish" | "block" | "cancel") {
                            if op == "finish"
                                && let Some(id) = plan_step_id
                            {
                                let tag = format!("step_{id}_finish");
                                // Same as the start boundary: one commit, one
                                // record, whatever layer name reads nicer.
                                let already = ctx.journal.last().is_some_and(|(_, t)| t == &tag);
                                if !already
                                    && let Some(sha) = checkpoints::snapshot_boundary(
                                        &root,
                                        shadow_store,
                                        parent_session.as_deref().unwrap_or(&session_id),
                                        &tag,
                                    )
                                    .ok()
                                    .flatten()
                                {
                                    ctx.journal.push((sha.clone(), tag.clone()));
                                    let _ =
                                        writer.append("checkpoint", checkpoint_fields(&sha, &tag));
                                }
                            }
                            writer.set_attribution(None, plan_id, "main");
                            // The session is idle again (§2.2.3).
                            ctx.current_step = None;
                            if subagent_depth == 0 {
                                let _ = tx.send(AgentEvent::StepCurrent { step: None }).await;
                            }
                        }
                        // The diary entry that used to fire on finish/block/cancel
                        // is gone (§20): `journal op=recap` reads these same
                        // records on demand, and the writer never produced prose.
                    }
                }
                if outcome.cancelled {
                    interrupted = true;
                }
                // Repeat handling (§18.1.8): the first identical re-run is
                // reported with a note, the next one loses its bytes — the note
                // alone did not stop a live session from calling the same
                // `git_diff` five times over the same 18KB.
                if outcome.ok {
                    match repeat_verdict_with_limit(&messages, call, &outcome.output, context_limit)
                    {
                        RepeatVerdict::Fresh => {}
                        RepeatVerdict::Note(note) => outcome.output.push_str(&note),
                        RepeatVerdict::Withheld(stub) => outcome.output = stub,
                    }
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

/// The files a finished call reports having touched: `file_diff` for the
/// single-file tools, `file_diffs` for a multi-file `patch`. Deduplicated, in
/// the order the call reported them.
///
/// An empty list means the tool declares no per-call path — for `bash` that is
/// the H4 attribution gap, not a claim that nothing changed. Callers must not
/// read it as "the worktree is untouched".
pub(crate) fn lint_targets(
    outcome: &crate::agent::tools::Outcome,
    root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut reported: Vec<&str> = Vec::new();
    if let Some(d) = &outcome.file_diff {
        reported.push(&d.path);
    }
    reported.extend(outcome.file_diffs.iter().map(|d| d.path.as_str()));
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for rel in reported {
        let p = root.join(rel.replace('\\', "/"));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod subagent_tests;

#[cfg(test)]
mod transport_tests;

#[cfg(test)]
mod effort_tests;

/// The gates the simplification pass removed: what must now simply work.
#[cfg(test)]
mod freedom_tests;

#[cfg(test)]
mod trust_gate_tests;

#[cfg(test)]
mod checkpoint_journal_tests;
