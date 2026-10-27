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

use crate::config::{ApprovalMode, EffortLevel};
use crate::providers::{
    ChatRequest, Message, RequestBreakdown, Role, SharedProvider, SystemPart, ToolCallReq, Usage,
};

use crate::agent::checkpoints;
use crate::agent::context;
use crate::agent::tools::{self, ToolCtx};
use crate::plan;
use loop_ask::{ask_user, bash_call, mutating_tool_gate, run_tool_blocking};
use loop_compact::{
    CompactionPrefix, compact_history, plan_hint_for_summary, record_compaction,
};
use loop_subagent::{
    SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step, run_subagent, run_subagent_batch,
};
use loop_turn::{request_messages_for_wire, run_turn_with_prefetch, turn_transport};

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

impl ApprovalDecision {
    /// EventLog spelling of the answer.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunOnce => "run_once",
            Self::AlwaysSession => "always_session",
            Self::Deny => "deny",
        }
    }
}

/// "Always allow this session" answers, kept for the life of the session.
///
/// `run_agent` is spawned per user submit, so the grants cannot live inside
/// it: the TUI stores this value between runs (in with [`AgentInput`], out
/// with [`AgentOutcome`]) — that is what makes "this session" true. Opaque
/// on purpose: the TUI never inspects the matching rules, it only holds the
/// box. Subagents run with an empty set; they cannot answer a prompt, so
/// they cannot earn grants.
#[derive(Debug, Clone, Default)]
pub struct SessionGrants {
    /// bash grants: structured command shapes (`git push *`, `rm -rf target`)
    pub(crate) bash: Vec<loop_ask::Grant>,
    /// mutating tools approved wholesale for the session (`edit`)
    pub(crate) tools: Vec<String>,
    /// directories outside the root opened for the session ("always"),
    /// canonical prefixes
    pub(crate) dirs: Vec<PathBuf>,
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
    pub checkpoints: Vec<(String, String)>,
    /// approval grants earned during this run, so "always allow this
    /// session" survives the turn that earned it
    pub approval_grants: SessionGrants,
    /// delta-read state, updated by this run's reads and edits
    pub read_ledger: crate::agent::tools::ReadLedger,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // the event enum is moved through a channel; boxing the large payloads would force an extra allocation on every event
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
    /// the session's current plan step changed; `None` means idle.
    /// The TUI persists it on the session; the loop itself tracks it in
    /// `current_step` and never waits for this round-trip. `plan` names the
    /// session's active plan when it has one: a `close` leaves the finished plan
    /// attached so the surface can still show it, and the next `create` must
    /// move that pointer, or the TUI keeps rendering the dead plan the agent
    /// stopped working on.
    StepCurrent {
        step: Option<String>,
        plan: Option<String>,
    },
    /// a command or mutating tool call needs approval; decide via
    /// ControlMsg. `subject` is what the user reads (the shell command
    /// line, or `tool + arguments` for a mutating tool), `grant` is the
    /// short preview of what an AlwaysSession answer would cover.
    Approval {
        id: u64,
        subject: String,
        reason: String,
        grant: String,
    },
    /// a shadow git or layer-1 checkpoint was taken before a mutation
    #[allow(dead_code)]
    Checkpoint {
        sha: String,
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
    Retry {
        attempt: u32,
        delay_secs: u64,
        error: String,
    },
    /// switched to a fallback model on retry-exhausted network/5xx
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
    /// flipped by Esc while a tool is executing. Distinct from
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
    /// events and the transcript, but `run_agent` then stops requesting
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
    ///: undo cancels whatever is still registered before it
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
    /// what the target model does with that level; threaded through so
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
    /// session id used for the host-owned events file
    pub session_id: String,
    /// hard-blocked command patterns from [safety].blocked_patterns
    pub blocked_patterns: Vec<String>,
    /// when tool calls need a confirmation dialog ([safety].approval_mode);
    /// governs prompts only, the deny-side gates hold in every mode
    pub approval_mode: crate::config::ApprovalMode,
    /// "always allow this session" answers from earlier turns of this
    /// session; the TUI carries them across per-turn `run_agent` spawns
    pub approval_grants: SessionGrants,
    /// PLAN mode: read-only tools only, mutations are refused
    pub plan_mode: bool,
    /// model context limit used to enforce the plan size budget
    pub context_limit: u64,
    /// Whether the target model accepts image input; unknown counts as yes.
    pub vision: bool,
    /// Whether the target model accepts PDF documents; unknown counts as no.
    pub pdf: bool,
    /// `[tools]` caps for the built-in tools.
    pub tools: crate::config::ToolsConfig,
    /// `[limits]` per-turn and concurrency guardrails.
    pub limits: crate::config::LimitsConfig,
    /// Skill names already active in the session (their bodies are in the
    /// system prompt); the `skill` tool answers a re-load with a pointer.
    pub active_skills: Vec<String>,
    /// Session-persisted delta-read state carried across per-turn runs.
    pub read_ledger: crate::agent::tools::ReadLedger,
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
    /// Skill discovery configuration (`[skills]`): the directories the `skill`
    /// tool searches. Without it the tool would only ever see the project
    /// skills and silently miss every configured global directory.
    pub skills: crate::config::SkillsConfig,
    /// Run the compaction policy and finish without talking to the model
    /// otherwise (the `/compact` command).
    pub compact_only: bool,
    /// Optional `/compact <focus>` tail, folded into the summarizer prompt.
    pub compact_focus: String,
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
    /// Immutable step context inherited from the spawning session.
    /// `None` for main agents. The child stamps it on its event records
    /// and refuses mutations once the step moves to a newer epoch.
    pub parent_step: Option<plan::StepContext>,
    /// Session id of the spawning agent, if any. The child's shadow
    /// snapshots land on this chain so the parent's `/undo` sees them
    ///; everything else (events, jobs, read-guard) stays on the
    /// child's own session.
    pub parent_session: Option<String>,
    /// Optional fallback models to switch to if primary model fails with retry-exhausted network/5xx.
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
/// Marker of the stub that replaces a repeated result entirely.
const REPEAT_STUB_MARK: &str = "[host: not re-reported";

/// Turn-local intervention when the agent gets stuck in an unproductive
/// `edit -> bash (fail)` loop (`TokenEconomizer` / `DeskCheck`).
pub(crate) fn failure_loop_nudge(consecutive_failures: usize) -> Option<&'static str> {
    if consecutive_failures >= 3 && consecutive_failures.is_multiple_of(3) {
        Some(
            "\n[host note: 3 consecutive edit/check cycles have failed. Stop guessing edits — \
             inspect the exact symbol/type definitions with `read` (or `grep`) and verify the \
             root cause before making another edit.]",
        )
    } else {
        None
    }
}

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

/// Repeat handling for any tool outcome. Compares full outputs, not
/// events summaries (truncated to 200 chars), so same-headed but
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
#[cfg(test)]
fn repeat_verdict(messages: &[Message], call: &ToolCallReq, output: &str) -> RepeatVerdict {
    repeat_verdict_with_limit(
        messages,
        call,
        output,
        &crate::agent::tool_output::Wire::default(),
    )
}

fn repeat_verdict_with_limit(
    messages: &[Message],
    call: &ToolCallReq,
    output: &str,
    wire: &crate::agent::tool_output::Wire,
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
    // Collect matching tool call ids first (filtering by tool name before
    // serializing arguments) so we only compute `cleared_indices`
    // and scan tool outputs when a prior identical call actually exists.
    let mut matching_ids: Vec<&str> = Vec::new();
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        for tc in &message.tool_calls {
            if tc.name != call.name {
                continue;
            }
            if call_key_ref(tc).as_deref() == Some(key.as_str()) {
                matching_ids.push(tc.id.as_str());
            }
        }
    }
    if matching_ids.is_empty() {
        return RepeatVerdict::Fresh;
    }
    let cleared = crate::agent::tool_output::cleared_indices(messages, wire);
    // (call key, output) pairs in order; the current call has no result yet,
    // so everything collected here is older. Only results still visible on the
    // wire count: never tell the model "see above" for output that was cleared.
    let mut matches = 0usize;
    for tc_id in matching_ids {
        let out = messages
            .iter()
            .enumerate()
            .find(|(idx, m)| {
                m.role == Role::Tool
                    && m.tool_call_id.as_deref() == Some(tc_id)
                    && !cleared.contains(idx)
            })
            .map(|(_, m)| strip_host_repeat_marks(&m.content))
            .unwrap_or("");
        if out == output {
            matches += 1;
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

/// A refused reach into host-owned state (`file tools` jail, or `bash`
/// protected path). Repeated refusals mean the model is guessing `.sqwai`
/// paths instead of looking at the project root, so the second one carries
/// the real inventory.
fn is_host_state_denial(output: &str) -> bool {
    output.contains("host-owned state") || output.contains("protected_path")
}

fn root_entries_note(root: &std::path::Path) -> String {
    let mut rows: Vec<(bool, String)> = match std::fs::read_dir(root) {
        Ok(rd) => rd
            .flatten()
            .map(|entry| {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let name = entry.file_name().to_string_lossy().into_owned();
                let label = if is_dir { format!("{name}/") } else { name };
                (is_dir, label)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = rows.len();
    let shown: Vec<String> = rows.into_iter().take(40).map(|(_, name)| name).collect();
    let mut list = shown.join(", ");
    if total > 40 {
        list.push_str(&format!(", … (+{} more)", total - 40));
    }
    format!(
        "[host: repeated attempt to open host-owned state under .sqwai/ — that directory holds \
         bookkeeping (events, checkpoints, plans, spilled output), never project inputs. Project \
         root '{}' contains: {list}. Use exactly these names; run ls \".\" for the current view.]",
        root.display()
    )
}

/// One event record per snapshot commit, named for what actually took it.
///
/// Every entry here is a shadow-git snapshot (`snapshot_session` for mutations,
/// `snapshot_boundary` for step edges), so the layer is stated once and the
/// reason carries the difference. Step tags arrive through the same vector as
/// mutations because `plan` snapshots its own boundaries — reading the label
/// is how the two stop being recorded twice, once as a step edge and once as
/// a phantom "post_mutation".
#[cfg(test)]
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

/// Media ids still visible on the wire, for `view_image`/`view_document`
/// repeat suppression. Recomputed at a tool-batch boundary: results attached
/// inside the batch are noted into `ctx` as they land.
fn refresh_visible_media(
    ctx: &mut ToolCtx,
    messages: &[Message],
    wire: &crate::agent::tool_output::Wire,
) {
    let cleared =
        crate::agent::tool_output::cleared_indices(messages, wire);
    let mut images = std::collections::HashSet::new();
    let mut documents = std::collections::HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::Tool && !cleared.contains(&index) {
            for image in &message.images {
                images.insert(image.id.clone());
            }
            for document in &message.documents {
                documents.insert(document.id.clone());
            }
        }
    }
    ctx.set_visible_images(images);
    ctx.set_visible_documents(documents);
}

/// Whether `call` is a pure read-only tool that can safely execute in parallel
/// with adjacent read-only calls (Codex-style shared read lock).
fn is_parallel_read_only_call(call: &ToolCallReq) -> bool {
    matches!(
        call.name.as_str(),
        "read"
            | "grep"
            | "glob"
            | "outline"
            | "ls"
            | "git_status"
            | "git_diff"
            | "webfetch"
            | "websearch"
    )
}

/// Find the end index `batch_end` of a parallelizable consecutive read-only run
/// starting at `start_idx`. Stops before any mutating/interactive tool, before
/// a duplicate `call_key` (so `repeat_verdict` can catch identical re-runs
/// sequentially), or before a second `read` on the same `file_path` (so window
/// coverage from the first `read` is visible to the second).
pub(crate) fn parallel_read_only_batch_end(
    calls: &[ToolCallReq],
    start_idx: usize,
    max_parallel: usize,
) -> usize {
    let mut keys: Vec<String> = Vec::new();
    let mut read_paths: Vec<String> = Vec::new();
    let mut end = start_idx;
    while end < calls.len() && (end - start_idx) < max_parallel {
        let call = &calls[end];
        if !is_parallel_read_only_call(call) {
            break;
        }
        let k = call_key(call);
        if !k.is_empty() && keys.contains(&k) {
            break;
        }
        if call.name == "read" {
            let mut targets: Vec<String> = Vec::new();
            if let Some(p) = call.args["file_path"].as_str().filter(|s| !s.is_empty()) {
                targets.push(p.replace('\\', "/"));
            }
            if let Some(arr) = call.args["paths"].as_array() {
                for v in arr.iter().filter_map(|x| x.as_str()) {
                    if !v.is_empty() {
                        targets.push(v.replace('\\', "/"));
                    }
                }
            }
            if targets.iter().any(|t| read_paths.contains(t)) {
                break;
            }
            read_paths.extend(targets);
        }
        if !k.is_empty() {
            keys.push(k);
        }
        end += 1;
    }
    end
}

pub(crate) async fn execute_parallel_read_only_call(
    idx: usize,
    c: ToolCallReq,
    mut child_ctx: ToolCtx,
    allow_hosts: Vec<String>,
) -> (usize, u128, tools::Outcome, Option<ToolCtx>) {
    let started = Instant::now();
    match c.name.as_str() {
        "webfetch" | "websearch" => {
            let mut out = if c.name == "webfetch" {
                tools::web::fetch(&c.args, &mut child_ctx, &allow_hosts).await
            } else {
                tools::web::search(&c.args, &child_ctx.tools).await
            };
            if out.ok {
                out.output = crate::agent::trust::banner_wrap(&out.output);
            }
            // The page ledger the worker built has to reach the session, or every
            // parallel fetch of one page looks like the first.
            child_ctx.current_call_id = None;
            (idx, started.elapsed().as_millis(), out, Some(child_ctx))
        }
        _ => {
            let out = run_tool_blocking(&mut child_ctx, &c.name, &c.args).await;
            child_ctx.current_call_id = None;
            (idx, started.elapsed().as_millis(), out, Some(child_ctx))
        }
    }
}

/// Sessions this process already opened an agent run for. `run_agent` is
/// spawned per turn, but `session_start` plus the resume record describe a
/// run's beginning: writing them every turn recorded a phantom "Session
/// resumed" while any step was merely open — and the model, reading it in
/// the tail and via `events`, concluded context is restored constantly and
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
        effort,
        mut effort_support,
        max_tokens,
        mut system,
        mut messages,
        root,
        session_id,
        blocked_patterns,
        approval_mode,
        approval_grants,
        web_allow_hosts,
        plan_mode,
        mut context_limit,
        vision,
        pdf,
        tools: tool_caps,
        limits,
        active_skills,
        read_ledger,
        enable_tools,
        background_baselines,
        read_only,
        mut previous_response_id,
        mut summary,
        compact_only,
        compact_focus,
        compaction,
        plan_limits,
        mcp,
        skills,
        shadow_store,
        subagent_depth,
        parent_step,
        parent_session,
        mut fallback_chain,
    } = input;
    let events_session = parent_session.clone().unwrap_or_else(|| session_id.clone());
    let events_agent = if parent_session.is_some() {
        session_id.clone()
    } else {
        "main".to_string()
    };
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
    );
    // Tools are part of the request prefix: sorted for stability, one set in
    // both modes, and omitted entirely for requests that cannot call them.
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
    // writer never produced a line of prose.
    if compact_only {
        let mut compaction_events = if !read_only {
            crate::agent::events::EventLog::open(&root, &events_session).ok()
        } else {
            None
        };
        if let Some(writer) = compaction_events.as_mut() {
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
            Some(compact_focus.as_str()).filter(|s| !s.trim().is_empty()),
            Some(&prefix),
        )
        .await;
        if let Some((before, after, summarized)) = outcome.as_ref()
            && let Some(writer) = compaction_events.as_mut()
        {
            let _ = writer.append(
                "compaction",
                serde_json::json!({
                    "phase": "end",
                    "before": before,
                    "after": after,
                    "dropped_msgs": message_count_before.saturating_sub(messages.len()),
                    "kept_msgs": messages.len(),
                    "anchor_tokens": context::anchor(&root, &events_session).len().div_ceil(4),
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
                checkpoints: Vec::new(),
                // compaction runs no tool calls, so the grants and read
                // ledger pass through exactly as they arrived
                approval_grants,
                read_ledger,
            })))
            .await;
        return;
    }

    let mut ctx = ToolCtx::with_read_only(&root, read_only)
        .in_session(&session_id)
        .with_shadow_store(shadow_store)
        .with_plan_limits(plan_limits, context_limit)
        .with_cancel(cancel_tool)
        .with_vision(vision)
        .with_pdf(pdf);
    // session-wide approval state rides the context: full access resolves
    // every path, approved directories (from this session's grants) open
    // for reads and writes under them
    ctx.outside_all = approval_mode == ApprovalMode::Full;
    ctx.outside_roots = approval_grants.dirs.clone();
    ctx.outside_granted = approval_grants.dirs.clone();
    ctx.tools = tool_caps.clone();
    ctx.limits = limits.clone();
    ctx.activated_skills.extend(active_skills.iter().cloned());
    ctx.read_ledger = read_ledger;
    crate::agent::tools::web::router::set_max_body_bytes(tool_caps.web.fetch_bytes.max(1024));
    // slow baseline capture defers to the background worker on interactive
    // turns; everywhere else the tool call captures synchronously
    ctx.background_baselines = background_baselines;
    // A child's shadow snapshots belong on the parent chain, so the
    // parent's `/undo` sees step boundaries and bash mutations; the event log,
    // job isolation and read-guard stay on the child's own session.
    ctx.checkpoint_session = parent_session.clone();
    // Subagents inherit their spawn context: it stamps their
    // event records and gates their mutations against reopen races.
    ctx.subagent_step = parent_step.clone();
    // ...and their parent's trust state: a fresh child events reads
    // clean, and the task text itself may be shaped by what the parent
    // consumed (audit H1).
    ctx.parent_session = parent_session.clone();
    // ...and a writer child's declared scope, if any (read-only children
    // and the main agent take nothing).
    ctx.subagent_write_paths = tools::take_subagent_scope(&session_id);
    // The `skill` tool searches the same directories the TUI catalog does —
    // otherwise a skill visible in `/skills` would be invisible to the model.
    ctx.skills_cfg = skills.clone();
    // ...and their shadow snapshots land on the parent chain, so the
    // parent's `/undo` sees them (ToolCtx::checkpoint_session).
    ctx.checkpoint_session = parent_session.clone();
    ctx.events_session = events_session.clone();
    // The session's current step. Adopt whatever the plan holds in
    // progress — after a crash that is the interrupted step — and
    // keep it in lockstep with plan outcomes below. `ctx.current_step` is
    // the single live copy; the TUI persists it via `StepCurrent` events.
    ctx.current_step = adopt_in_progress_step(&root, &session_id);
    let mut edited_since_green_bash = false;
    let mut consecutive_edit_bash_failures = 0usize;
    if enable_tools && !read_only {
        // Heal a crash between an event log intent and its plan store before anything — including this session's writer — reads the
        // plan or the tail counter. No-op on a clean tree.
        let _ = plan::replay(&root);
    }
    let mut events = if enable_tools && !read_only {
        crate::agent::events::EventLog::open(&root, &events_session).ok()
    } else {
        None
    };
    if let Some(writer) = events.as_mut() {
        if let Some(inherited) = parent_step.as_ref() {
            // Stamp the inherited epoch on every record this writer produces
            // so post-reopen validation can tell stale work apart.
            writer.set_epoch(Some(inherited.step_epoch));
        }
        let plan_id = plan::open_active_for_session(&root, Some(&session_id))
            .ok()
            .flatten()
            .map(|p| p.id);
        writer.set_attribution(None, plan_id, &events_agent);
        // once per process+session: run_agent is spawned per turn, so an
        // unguarded write here recorded session_start + resume every turn
        if mark_agent_run_started(&session_id) {
            let resumed_from = context::resume_notice(&root, &events_session).map(|_| "events");
            let _ = writer.session_start(
                &model_id,
                if plan_mode { "plan" } else { "act" },
                approval_mode.as_str(),
                None,
                "unknown",
                resumed_from,
            );
            if let Some(notice) = context::resume_notice(&root, &events_session) {
                let _ = writer.append("resume", serde_json::json!({"notice": notice}));
            }
        }
        if let Some(user_message) = messages.iter().rev().find(|m| m.role == Role::User) {
            // per-turn attribution: the event log once credited this session
            // to a model it was not running on, because session_start names
            // only the first run's model. The turn record names the one
            // that actually answers, plus the approval axis it ran under.
            let _ = writer.append("user_msg", serde_json::json!({
                "hash": format!("{:x}", sha2::Sha256::digest(user_message.content.as_bytes())),
                "chars": user_message.content.chars().count(),
                "goal_like": user_message.content.starts_with("goal:") || user_message.content.starts_with("/goal"),
                "model": model_id,
                "approval": approval_mode.as_str(),
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
    // Host-state denials since the last successful call: two in a row mean the
    // model is guessing paths, and the next denial carries the real root list.
    let mut host_state_denials = 0u32;
    // Grants live in the TUI between turns (see `SessionGrants`); this run
    // starts from whatever earlier turns earned and hands the grown set back
    // in `AgentOutcome`.
    let mut always_allow: Vec<loop_ask::Grant> = approval_grants.bash;
    let mut tool_grants: Vec<String> = approval_grants.tools;
    // present-tense plan nudge, at most one per model request
    let mut plan_nudged;
    // "no plan at all" nudge, once per run: one reminder per user submit
    let mut no_plan_nudged = false;
    let mut next_id: u64 = 0;
    // prompt size of the last request, as reported by the provider
    let mut prompt_size: u64 = 0;
    // One forced compaction per oversized request, reset after every turn that
    // gets through: without the guard a request that stays too large would
    // compact in a loop.
    let mut compacted_for_overflow = false;

    let refresh_anchor_in_system =
        |sys: &mut Vec<SystemPart>, root: &std::path::Path, session_id: &str| {
            let fresh = SystemPart::volatile(context::anchor(root, session_id));
            if let Some(slot) = sys
                .iter_mut()
                .find(|p| !p.cacheable && p.text.starts_with("ANCHOR "))
            {
                *slot = fresh;
            } else {
                sys.push(fresh);
            }
        };

    // The turn block is built once per session; only its clock line may not
    // freeze, or a run that crosses midnight keeps reporting yesterday.
    let refresh_turn_date = |ts: &mut [SystemPart]| {
        if let Some(part) = ts
            .iter_mut()
            .find(|p| p.text.starts_with("<turn_environment>"))
        {
            crate::prompts::env::refresh_date(&mut part.text);
        }
    };

    // The model's own runtime facts, refreshed on every request: a fallback
    // switch and a growing transcript both have to show up before the next
    // decision, not at the start of the turn.
    let refresh_agent_state =
        |ts: &mut Vec<SystemPart>, model: &str, used: u64, policy: &context::Policy| {
            let text = context::agent_state(model, used, policy);
            if let Some(slot) = ts
                .iter_mut()
                .find(|p| !p.cacheable && p.text.starts_with("<agent_state>"))
            {
                *slot = SystemPart::volatile(text);
            } else if !text.is_empty() {
                ts.push(SystemPart::volatile(text));
            }
        };

    let build_turn_system = |base: &[SystemPart], root: &std::path::Path, session_id: &str| {
        let mut ts = base.to_vec();
        if let Ok(Some(nudge)) = crate::agent::events::EventLog::nudge(
            root,
            Some(&events_session),
            plan_limits.nudge_after,
        ) {
            ts.push(crate::providers::SystemPart::volatile(nudge));
        }
        if let Some(plan) = crate::prompts::plan_block(root, Some(session_id)) {
            ts.push(crate::providers::SystemPart::volatile(plan));
        }
        if let Some(status) = crate::prompts::plan_status_block(root, Some(session_id)) {
            ts.push(crate::providers::SystemPart::volatile(status));
        }
        ts
    };
    let mut turn_system = build_turn_system(&system, &root, &session_id);
    let mut turn_tokens_used: u64 = 0;
    let mut tool_calls_used: usize = 0;

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
        // the plan nudge rides at most one mutating result per model
        // request: every result carrying it waters the next one down
        plan_nudged = false;
        refresh_agent_state(&mut turn_system, &current_model_key, prompt_size, &policy);
        refresh_turn_date(&mut turn_system);
        // Compaction gate. History only: the provider's full request size
        // is observed here, but the gate must use
        // what compaction can actually remove — otherwise a big fixed
        // prefix fires futile compactions every turn.
        let msgs_before = messages.len();
        let plan_hint = plan_hint_for_summary(&root, &session_id);
        let prefix = CompactionPrefix {
            system: &turn_system,
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
            None,
            Some(&prefix),
        )
        .await
        {
            record_compaction(
                &mut events,
                before,
                after,
                msgs_before,
                messages.len(),
                summarized,
                summary.as_deref().unwrap_or(""),
            );
            // The transcript the clear floor was counting against is gone:
            // keeping it would stub results by position, not by history.
            ctx.cleared_floor = 0;
            // Same rule as the overflow retry below: the transcript the host
            // owns has changed, so the provider's copy of it is no longer the
            // context this turn is about.
            previous_response_id = None;
            refresh_anchor_in_system(&mut system, &root, &events_session);
            turn_system = build_turn_system(&system, &root, &session_id);
            let _ = tx
                .send(AgentEvent::Compaction {
                    summarized,
                    before,
                    after,
                })
                .await;
        }
        // No active plan: the host says nothing about plans. The convention
        // "no plan block means no plan, do not reconstruct one from history" is
        // a system.md rule, stated once — a per-request note naming the closed
        // plan would inject plan vocabulary exactly where the plan is gone
        //.
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
        let mut wire = crate::agent::tool_output::Wire {
            context_limit,
            bytes_per_token: policy.bytes_per_token,
            cleared_floor: ctx.cleared_floor,
        };
        let request_messages = request_messages_for_wire(&messages, transport, &wire);
        // Counted here because the request owns the messages from the next line
        // on, and the record below has to say what actually went out.
        let wire_messages = request_messages.len();
        let wire_cleared = request_messages
            .iter()
            .filter(|m| m.role == Role::Tool && crate::agent::tool_output::is_stub(&m.content))
            .count();
        // Whatever this request hid stays hidden for the run: a boundary that
        // moves back rewrites the head of the transcript and forfeits the
        // provider's prefix cache behind it.
        if wire_cleared > ctx.cleared_floor {
            ctx.cleared_floor = wire_cleared;
            wire.cleared_floor = wire_cleared;
        }
        let chat_req = ChatRequest {
            model_id: model_id.clone(),
            system: turn_system.clone(),
            messages: request_messages,
            effort,
            effort_support,
            max_tokens,
            tools: tools.clone(),
            previous_response_id: previous_response_id.clone(),
            context_transport: transport,
        };
        let breakdown = RequestBreakdown::from_request(&chat_req);
        let _ = tx.send(AgentEvent::RequestBreakdown(breakdown)).await;

        // Refresh visible read windows before the turn streams so any eager
        // mid-stream `read` prefetch sees accurate window coverage.
        {
            let cleared = crate::agent::tool_output::cleared_indices(&messages, &wire);
            let visible_ids: std::collections::HashSet<String> = messages
                .iter()
                .enumerate()
                .filter(|(idx, m)| m.role == Role::Tool && !cleared.contains(idx))
                .filter_map(|(_, m)| m.tool_call_id.clone())
                .collect();
            ctx.retain_visible_read_windows(&visible_ids);
        }

        // one complete streaming turn (with eager mid-stream read-only prefetch)
        let mut turn = match run_turn_with_prefetch(
            &provider,
            &chat_req,
            &tx,
            &mut ctl,
            &mut previous_response_id,
            &mut prompt_size,
            !fallback_chain.is_empty(),
            Some((&ctx, &web_allow_hosts)),
        )
        .await
        {
            Ok(turn) => {
                if turn.retries > 0
                    && let Some(writer) = events.as_mut()
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
                if let Some(writer) = events.as_mut() {
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
                    if let Some(writer) = events.as_mut() {
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
                // try, which is the only thing that can make it fit.
                // Asking the same oversized request again cannot.
                if failure.class == Some(ErrorClass::ContextOverflow) && !compacted_for_overflow {
                    compacted_for_overflow = true;
                    let overflow_msgs_before = messages.len();
                    let overflow_plan_hint = plan_hint_for_summary(&root, &session_id);
                    let prefix = CompactionPrefix {
                        system: &turn_system,
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
                        None,
                        Some(&prefix),
                    )
                    .await
                    {
                        record_compaction(
                            &mut events,
                            before,
                            after,
                            overflow_msgs_before,
                            messages.len(),
                            summarized,
                            summary.as_deref().unwrap_or(""),
                        );
                        ctx.cleared_floor = 0;
                        // The transcript this turn resends is not the one the
                        // provider holds any more, so the reference cannot be
                        // reused for the retry either.
                        previous_response_id = None;
                        refresh_anchor_in_system(&mut system, &root, &events_session);
                        turn_system = build_turn_system(&system, &root, &session_id);
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
                // Stage T: Provider Fallback Chain on retry-exhausted network / 5xx error.
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
                    if let Some(writer) = events.as_mut() {
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

        if let Some(usage) = turn.usage {
            turn_tokens_used = turn_tokens_used
                .saturating_add(usage.prompt_tokens)
                .saturating_add(usage.completion_tokens);
            // One record per request: what the provider said it read, against
            // what the host actually put on the wire. Prefix caching breaks at
            // the first changed byte, and the only change the host makes to old
            // messages is clearing tool output — so `cached` falling where
            // `cleared` jumped is the whole question, and it cannot be answered
            // after the fact from a session that keeps one `last_usage`.
            // The window is counted in tokens and the host only measures bytes,
            // so the conversion is taken from the provider instead of assumed:
            // at the old flat 4, a 256k model was budgeted as a 1 MB request when
            // its own accounting put 1,189,643 bytes at 144,424 tokens — results
            // were thrown away at a third of the context.
            //
            // Learned once per run and then kept: the ratio depends on what the
            // request happens to carry, so re-reading it each turn moved the clear
            // threshold, and a threshold that moves backwards rewrites the head of
            // the transcript and kills the provider's prefix cache.
            if policy.bytes_per_token == crate::agent::tool_output::DEFAULT_BYTES_PER_TOKEN
                && let Some(ratio) = crate::agent::tool_output::observe_bytes_per_token(
                    breakdown.total_bytes,
                    usage.prompt_tokens,
                )
            {
                policy.bytes_per_token = ratio;
            }
            if let Some(writer) = events.as_mut() {
                let _ = writer.append(
                    "request",
                    serde_json::json!({
                        "prompt": usage.prompt_tokens,
                        "completion": usage.completion_tokens,
                        "cached": usage.cached_tokens,
                        "written": usage.cache_write_tokens,
                        "transport": match transport {
                            crate::providers::ContextTransport::Stateless => "stateless",
                            crate::providers::ContextTransport::PreviousResponse => {
                                "previous_response"
                            }
                            crate::providers::ContextTransport::ServerConversation => {
                                "server_conversation"
                            }
                        },
                        "cleared": wire_cleared,
                        "on_wire": wire_messages,
                        "system": breakdown.system_bytes,
                        "history": breakdown.history_bytes,
                        "tool_output": breakdown.tool_output_bytes,
                        "reasoning": breakdown.reasoning_bytes,
                        "total": breakdown.total_bytes,
                        "bpt": policy.bytes_per_token,
                    }),
                );
            }
        }
        let token_limit_hit =
            limits.max_tokens_per_turn > 0 && turn_tokens_used >= limits.max_tokens_per_turn;

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

        if token_limit_hit {
            for call in &turn.calls {
                messages.push(Message::tool_result(
                    &call.id,
                    format!(
                        "not run: turn token limit reached ({} tokens)",
                        limits.max_tokens_per_turn
                    ),
                    true,
                ));
            }
            break;
        }

        // once the user cancels one call, no further calls in
        // this batch run and no further model turns are requested — Esc means
        // stop, not "let the model decide what to do about it".
        let mut interrupted = false;
        let mut limit_hit = false;

        // A turn that only delegates to subagents fans the calls out
        // concurrently (each awaits its children inside run_subagent);
        // every other batch keeps the sequential loop below. Pre/post
        // bookkeeping is identical in both shapes: rows, events and
        // messages stay in call order, only the waits overlap.
        let pure_subagents = subagent_depth == 0
            && !turn.calls.is_empty()
            && turn.calls.iter().all(|call| call.name == "subagent");
        if pure_subagents {
            let remaining = limits
                .max_tool_calls_per_turn
                .saturating_sub(tool_calls_used);
            if limits.max_tool_calls_per_turn > 0 && turn.calls.len() > remaining {
                for call in &turn.calls {
                    messages.push(Message::tool_result(
                        &call.id,
                        format!(
                            "not run: tool-call limit reached ({} per turn)",
                            limits.max_tool_calls_per_turn
                        ),
                        true,
                    ));
                }
                limit_hit = true;
            } else {
                tool_calls_used += turn.calls.len();
                interrupted = run_subagent_batch(
                    &turn.calls,
                    &session_id,
                    &root,
                    &ctx.cancel,
                    &mut events,
                    &tx,
                    ctx.current_step.clone(),
                    &provider,
                    &model_id,
                    &blocked_patterns,
                    &web_allow_hosts,
                    plan_mode,
                    approval_mode,
                    context_limit,
                    vision,
                    pdf,
                    &tool_caps,
                    &limits,
                    &active_skills,
                    effort,
                    effort_support,
                    max_tokens,
                    &system,
                    &mcp,
                    &skills,
                    read_only,
                    shadow_store,
                    &mut messages,
                    compaction.clone(),
                    plan_limits,
                    fallback_chain.clone(),
                    std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
                )
                .await;
            }
        } else {
            refresh_visible_media(&mut ctx, &messages, &wire);
            let mut parallel_outcomes: std::collections::HashMap<
                usize,
                (u128, tools::Outcome, Option<ToolCtx>),
            > = std::collections::HashMap::new();
            // Collect any eager mid-stream prefetched read-only calls first.
            if !ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                for (idx, handle) in std::mem::take(&mut turn.prefetched) {
                    if let Ok((i, dur_ms, out, child_ctx)) = handle.await {
                        parallel_outcomes.insert(i, (dur_ms, out, child_ctx));
                    } else {
                        let _ = idx;
                    }
                }
            } else {
                for (_, handle) in std::mem::take(&mut turn.prefetched) {
                    handle.abort();
                }
            }
            for (call_index, call) in turn.calls.iter().enumerate() {
                if limits.max_tool_calls_per_turn > 0
                    && tool_calls_used >= limits.max_tool_calls_per_turn
                {
                    for rest in &turn.calls[call_index..] {
                        messages.push(Message::tool_result(
                            &rest.id,
                            format!(
                                "not run: tool-call limit reached ({} per turn)",
                                limits.max_tool_calls_per_turn
                            ),
                            true,
                        ));
                    }
                    limit_hit = true;
                    break;
                }
                tool_calls_used += 1;
                if !parallel_outcomes.contains_key(&call_index)
                    && !ctx.cancel.load(std::sync::atomic::Ordering::Relaxed)
                {
                    let batch_end = parallel_read_only_batch_end(
                        &turn.calls,
                        call_index,
                        limits.max_parallel_tools.clamp(1, 64),
                    );
                    // Outside reads earn the same boundary as writes: one
                    // approval opens the directory (run-only or session),
                    // full access never asks (`outside_all`). A denied read
                    // is precomputed into the batch outcomes so execution
                    // skips it cleanly.
                    if subagent_depth == 0 && approval_mode != ApprovalMode::Full {
                        for idx in call_index..batch_end.max(call_index + 1) {
                            let peer = &turn.calls[idx];
                            let Some(path) = loop_ask::read_call_path(&peer.name, &peer.args)
                            else {
                                continue;
                            };
                            if !ctx.outside_uncovered(&path) {
                                continue;
                            }
                            let grant_dir = ctx.outside_grant_dir(&path);
                            let preview = grant_dir
                                .as_ref()
                                .map(|d| format!("{}/**", d.display()))
                                .unwrap_or_default();
                            let decision = loop_ask::approval_dialog(
                                format!("{} {path}", peer.name),
                                loop_ask::OUTSIDE_READ_REASON,
                                preview,
                                &tx,
                                &mut ctl,
                                &mut next_id,
                                &mut events,
                            )
                            .await;
                            match decision {
                                Some(ApprovalDecision::Deny) => {
                                    parallel_outcomes.insert(
                                        idx,
                                        (
                                            0,
                                            tools::Outcome::err(format!(
                                                "{} denied by user ({})",
                                                peer.name,
                                                loop_ask::OUTSIDE_READ_REASON
                                            )),
                                            None,
                                        ),
                                    );
                                }
                                Some(ApprovalDecision::AlwaysSession) => {
                                    if let Some(d) = grant_dir {
                                        ctx.outside_roots.push(d.clone());
                                        ctx.outside_granted.push(d);
                                    }
                                }
                                Some(ApprovalDecision::RunOnce) => {
                                    if let Some(d) = grant_dir {
                                        ctx.outside_roots.push(d);
                                    }
                                }
                                None => {
                                    parallel_outcomes.insert(
                                        idx,
                                        (
                                            0,
                                            tools::Outcome::err("tui closed awaiting approval"),
                                            None,
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    if batch_end - call_index > 1 {
                        if turn.calls[call_index..batch_end]
                            .iter()
                            .any(|c| c.name == "read")
                        {
                            let cleared =
                                crate::agent::tool_output::cleared_indices(&messages, &wire);
                            let visible_ids: std::collections::HashSet<String> = messages
                                .iter()
                                .enumerate()
                                .filter(|(idx, m)| m.role == Role::Tool && !cleared.contains(idx))
                                .filter_map(|(_, m)| m.tool_call_id.clone())
                                .collect();
                            ctx.retain_visible_read_windows(&visible_ids);
                        }
                        use futures::{StreamExt, stream};
                        let tasks: Vec<(usize, ToolCallReq, ToolCtx, Vec<String>)> = (call_index
                            ..batch_end)
                            .filter(|idx| !parallel_outcomes.contains_key(idx))
                            .map(|idx| {
                                let c = turn.calls[idx].clone();
                                let mut child_ctx = ctx.clone();
                                child_ctx.current_call_id = Some(c.id.clone());
                                (idx, c, child_ctx, web_allow_hosts.clone())
                            })
                            .collect();
                        let results: Vec<(usize, u128, tools::Outcome, Option<ToolCtx>)> =
                            stream::iter(tasks)
                                .map(|(idx, c, child_ctx, allow_hosts)| {
                                    execute_parallel_read_only_call(idx, c, child_ctx, allow_hosts)
                                })
                                .buffer_unordered(limits.max_parallel_tools.clamp(1, 64))
                                .collect()
                                .await;
                        for (idx, dur_ms, out, child_ctx) in results {
                            parallel_outcomes.insert(idx, (dur_ms, out, child_ctx));
                        }
                    }
                }
                // A cancellation from the previous call must not leak into this
                // one: the flag is per-request, reset right before dispatch —
                // but the reset must not swallow an Esc that landed between
                // calls (#192): swap reports whether it was set, and if so this
                // call is recorded as cancelled without running. Downstream
                // (ToolNotice, events, the interrupted path) treats it exactly
                // like a mid-tool cancel.
                let pre_cancelled = ctx.cancel.swap(false, std::sync::atomic::Ordering::Relaxed);
                let tool_started = Instant::now();
                let mut precomputed_duration_ms: Option<u128> = None;
                if let Some(writer) = events.as_mut() {
                    let active = plan::open_active_for_session(&root, Some(&session_id))
                        .ok()
                        .flatten();
                    let plan_id = active.as_ref().map(|p| p.id.clone());
                    // Explicit session step first; the plan scan is only
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
                    writer.set_attribution(step, plan_id, &events_agent);
                }
                // live row first: the TUI shows the tool name and its arguments
                // with a spinner while it runs
                let _ = tx
                    .send(AgentEvent::ToolStart {
                        name: call.name.clone(),
                        summary: tools::call_summary(&call.name, &call.args),
                        call_id: call.id.clone(),
                    })
                    .await;

                let mut outcome = if pre_cancelled {
                    parallel_outcomes.clear();
                    tools::Outcome::cancelled()
                } else if let Some((dur_ms, pre_out, child_ctx_opt)) =
                    parallel_outcomes.remove(&call_index)
                {
                    if let Some(child_ctx) = child_ctx_opt {
                        ctx.merge_read_state(child_ctx);
                    }
                    precomputed_duration_ms = Some(dur_ms);
                    pre_out
                } else if read_only && tools::is_mutating_call(&call.name, &call.args) {
                    tools::Outcome::err(
                        "project is read-only because another sqwai instance owns the lock; use --force to enable writes",
                    )
                } else if plan_mode
                    && tools::is_mutating_call(&call.name, &call.args)
                    && call.name != "plan"
                {
                    tools::Outcome::err(format!(
                        "PLAN mode is read-only: '{}' is disabled. Switch to ACT to use it.",
                        call.name
                    ))
                } else if subagent_depth == 0
                    && call.name != "bash"
                    && call.name != "plan"
                    && tools::is_mutating_call(&call.name, &call.args)
                {
                    // One gate for mutating calls: the workspace boundary
                    // (an outside write asks in every mode but full) and
                    // ask mode's blanket confirm. `bash` asks inside
                    // `bash_call`, Safe commands included; subagents cannot
                    // prompt and keep the run's existing gates instead.
                    match mutating_tool_gate(
                        call,
                        &mut ctx,
                        &tx,
                        &mut ctl,
                        &mut next_id,
                        approval_mode,
                        &mut tool_grants,
                        &mut events,
                    )
                    .await
                    {
                        Some(outcome) => outcome,
                        None => {
                            ctx.current_call_id = Some(call.id.clone());
                            let out = run_tool_blocking(&mut ctx, &call.name, &call.args).await;
                            ctx.current_call_id = None;
                            out
                        }
                    }
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
                                approval_mode,
                                &mut events,
                                &mut None,
                            )
                            .await
                        }
                        "webfetch" | "websearch" => {
                            let mut outcome = if call.name == "webfetch" {
                                // The page ledger rides on the call id: a result
                                // that later leaves the wire stops counting as
                                // "already served".
                                ctx.current_call_id = Some(call.id.clone());
                                let out =
                                    tools::web::fetch(&call.args, &mut ctx, &web_allow_hosts).await;
                                ctx.current_call_id = None;
                                out
                            } else {
                                tools::web::search(&call.args, &tool_caps).await
                            };
                            // R banner: external bytes travel delimited
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
                                approval_mode,
                                context_limit,
                                vision,
                                pdf,
                                &tool_caps,
                                &limits,
                                &active_skills,
                                effort,
                                effort_support,
                                max_tokens,
                                system.clone(),
                                mcp.clone(),
                                skills.clone(),
                                read_only,
                                shadow_store,
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
                            // against the stale hold. Re-adopt.
                            let held_before = ctx.current_step.clone();
                            ctx.current_step = adopt_in_progress_step(&root, &ctx.session_id);
                            if ctx.current_step != held_before {
                                let plan_id = plan::open_active_for_session(&root, Some(&ctx.session_id))
                                    .ok()
                                    .flatten()
                                    .map(|p| p.id);
                                let _ = tx
                                    .send(AgentEvent::StepCurrent {
                                        step: ctx.current_step.clone(),
                                        plan: plan_id,
                                    })
                                    .await;
                            }
                            outcome
                        }
                        "subagent" => tools::Outcome::err("nested subagents are not allowed"),
                        "recall_result" => {
                            crate::agent::tool_output::recall_args(&messages, &call.args, &wire)
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
                                // R banner: server-authored bytes are
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
                                    crate::agent::tool_output::cleared_indices(&messages, &wire);
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

                // `bash` is the one tool whose targets are not known
                // in advance, so a cancelled run can have mutated files no
                // pre-checkpoint covered. `snapshot_session` already implements
                // "only if the tree changed since the previous snapshot" — that
                // is exactly the condition asks for, so it is read from
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
                    ctx.checkpoints.push((sha, "bash (cancelled)".to_string()));
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

                if let Some(writer) = events.as_mut() {
                    let duration_ms = precomputed_duration_ms
                        .unwrap_or_else(|| tool_started.elapsed().as_millis());
                    let result_seq = if call.name == "plan" {
                        writer
                        .append(
                            "observation",
                            serde_json::json!({
                                "tool": call.name,
                                "call_id": call.id,
                                "ok": outcome.ok,
                                "duration_ms": duration_ms,
                                "summary": outcome.output.chars().take(200).collect::<String>(),
                                "trust": "high",
                                "path": tools::call_path(&call.name, &call.args),
                                "paths": call.args.get("paths").cloned().unwrap_or(serde_json::Value::Null),
                                "args_digest": tools::call_summary(&call.name, &call.args),
                                "code": if outcome.cancelled { Some("cancelled") } else { None },
                            }),
                        )
                        .ok()
                    } else {
                        // R taint: every content-bearing result carries
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
                            "observation",
                            serde_json::json!({
                                "tool": call.name,
                                "call_id": call.id,
                                "ok": outcome.ok,
                                "duration_ms": duration_ms,
                                "summary": outcome.output.chars().take(200).collect::<String>(),
                                "trust": if taint.is_some() { "low" } else { "high" },
                                "taint": match taint {
                                    Some(crate::agent::trust::TaintClass::External) => "external",
                                    Some(crate::agent::trust::TaintClass::Local) => "local",
                                    None => "none",
                                },
                                "path": tools::call_path(&call.name, &call.args),
                                "paths": call.args.get("paths").cloned().unwrap_or(serde_json::Value::Null),
                                "args_digest": tools::call_summary(&call.name, &call.args),
                                // distinct from an ordinary failure, so the
                                // events can say the user stopped this rather
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
                                // links into the layer-1 blob store, so a
                                // single step can be reverted from the event log
                                // alone; absent when nothing was stored
                                "blob_before": metadata.blob_before,
                                "blob_after": metadata.blob_after,
                            }),
                        );
                    }
                    if call.name == "plan" && outcome.ok {
                        let active = plan::open_active_for_session(&root, Some(&session_id))
                            .ok()
                            .flatten();
                        let plan_id = active.as_ref().map(|p| p.id.clone());
                        let current = adopt_in_progress_step(&root, &session_id);
                        writer.set_attribution(current.clone(), plan_id.clone(), &events_agent);
                        ctx.current_step = current.clone();
                        if subagent_depth == 0 {
                            let _ = tx
                                .send(AgentEvent::StepCurrent {
                                    step: current,
                                    plan: plan_id,
                                })
                                .await;
                        }
                    }
                }
                if outcome.cancelled {
                    interrupted = true;
                }
                if outcome.ok
                    && matches!(
                        call.name.as_str(),
                        "edit" | "write"
                    )
                {
                    edited_since_green_bash = true;
                }
                if call.name == "bash" {
                    if outcome.ok {
                        consecutive_edit_bash_failures = 0;
                        edited_since_green_bash = false;
                    } else if !outcome.cancelled && edited_since_green_bash {
                        consecutive_edit_bash_failures += 1;
                        edited_since_green_bash = false;
                        if let Some(nudge) = failure_loop_nudge(consecutive_edit_bash_failures) {
                            outcome.output.push_str(nudge);
                        }
                    }
                }
                // Plan nudge: present tense, ready-made call, at most one
                // per model request. It rides any mutating result — work on
                // a step moves through bash just as much as through edits,
                // and the old edit-only, future-tense hint ("as soon as
                // this step's work is done") was read as "not yet" and
                // ignored through a whole session.
                if outcome.ok
                    && !plan_nudged
                    && subagent_depth == 0
                    && call.name != "plan"
                    && tools::is_mutating_call(&call.name, &call.args)
                    && let Ok(Some(active)) =
                        plan::open_active_for_session(&root, Some(&session_id))
                {
                    let open: Vec<&plan::Step> = active
                        .steps
                        .iter()
                        .filter(|s| s.status == plan::StepStatus::InProgress)
                        .collect();
                    match open.as_slice() {
                        [step] => {
                            plan_nudged = true;
                            let total = active.steps.len();
                            let title_short: String = step.title.chars().take(48).collect();
                            outcome.output.push_str(&format!(
                                "\n[plan: step {}/{total} \"{title_short}\" — if this call did \
                                 the step's work, close it now: plan(op=\"finish\", id=\"{}\", \
                                 summary=\"what this settled\") — before any other tool or \
                                 reply.]",
                                step.id, step.id
                            ));
                        }
                        [first, rest @ ..] => {
                            // file-shaped work often spans several content
                            // steps at once; naming only the first open step
                            // taught the model the others could wait
                            plan_nudged = true;
                            let mut ids = String::new();
                            for (i, step) in rest.iter().enumerate() {
                                if i > 0 {
                                    ids.push_str(", ");
                                }
                                ids.push_str(&step.id);
                            }
                            let title_short: String = first.title.chars().take(48).collect();
                            outcome.output.push_str(&format!(
                                "\n[plan: steps {} and {} of {} are open (\"{title_short}\" is \
                                 the first) — if this call did any of their work, close those \
                                 now, each with its own call: plan(op=\"finish\", \
                                 id=\"<step>\", summary=\"what it settled\") — do not save them \
                                 up for the end.]",
                                first.id,
                                ids,
                                active.steps.len()
                            ));
                        }
                        [] => {}
                    }
                }
                // The mirror case: an actual file change with NO plan at all.
                // Read-only bash, queries and failed calls are recon, not
                // "work without a plan" — nudging there was ignored 11/11 in a
                // live diagnostic session. Once per run, phrased as an option:
                // a one-step change can skip it.
                if outcome.ok
                    && !no_plan_nudged
                    && subagent_depth == 0
                    && call.name != "plan"
                    && outcome.changed_any_file()
                    && plan::open_active_for_session(&root, Some(&session_id))
                        .map(|p| p.is_none())
                        .unwrap_or(false)
                {
                    no_plan_nudged = true;
                    let mut changed: Vec<&tools::FileDiff> = outcome
                        .file_diffs
                        .iter()
                        .filter(|d| d.added > 0 || d.removed > 0)
                        .collect();
                    if changed.is_empty() {
                        changed.extend(
                            outcome
                                .file_diff
                                .iter()
                                .filter(|d| d.added > 0 || d.removed > 0),
                        );
                    }
                    let what = match changed.as_slice() {
                        [] => tools::call_summary(&call.name, &call.args),
                        [one] => one.path.clone(),
                        many => format!("{} files", many.len()),
                    };
                    let what = if what.chars().count() > 72 {
                        format!("{}…", what.chars().take(72).collect::<String>())
                    } else {
                        what
                    };
                    outcome.output.push_str(&format!(
                        "\n[host: this call changed {what}. If more state changes follow, \
                         plan them: 3–6 steps, each ending in a check — \
                         plan(op=\"create\", goal=\"...\", criteria=[\"...\"], \
                         steps=[{{\"title\": \"...\", \"check\": \"...\"}}]). \
                         Skip for a one-step change.]"
                    ));
                }
                if !outcome.ok && is_host_state_denial(&outcome.output) {
                    host_state_denials += 1;
                    if host_state_denials >= 2 {
                        outcome.output.push('\n');
                        outcome.output.push_str(&root_entries_note(&root));
                    }
                } else if outcome.ok {
                    host_state_denials = 0;
                }
                // Repeat handling: the first identical re-run is
                // reported with a note, the next one loses its bytes — the note
                // alone did not stop a live session from calling the same
                // `git_diff` five times over the same 18KB. Media results are
                // exempt: their attachment is the delivery, and a cleared
                // earlier copy makes the duplicate check meaningless.
                if outcome.ok && outcome.images.is_empty() && outcome.documents.is_empty() {
                    match repeat_verdict_with_limit(&messages, call, &outcome.output, &wire)
                    {
                        RepeatVerdict::Fresh => {}
                        RepeatVerdict::Note(note) => outcome.output.push_str(&note),
                        RepeatVerdict::Withheld(stub) => outcome.output = stub,
                    }
                }
                messages.push(
                    Message::tool_result(&call.id, outcome.output, !outcome.ok)
                        .with_images(outcome.images.clone())
                        .with_documents(outcome.documents.clone()),
                );
                for image in &outcome.images {
                    ctx.note_visible_image(&image.id);
                }
                for document in &outcome.documents {
                    ctx.note_visible_document(&document.id);
                }
                if interrupted {
                    // Esc means stop — the remaining calls in this batch
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
        if interrupted || limit_hit {
            // and no further model turn is requested this round; the turn
            // ends with what was accumulated, the same shape as a normal
            // final answer.
            break;
        }
    }

    let _ = tx
        .send(AgentEvent::Completed(Ok(AgentOutcome {
            messages,
            summary,
            todos,
            checkpoints: ctx.checkpoints,
            plan_todos,
            approval_grants: SessionGrants {
                bash: always_allow,
                tools: tool_grants,
                dirs: std::mem::take(&mut ctx.outside_granted),
            },
            read_ledger: ctx.read_ledger,
        })))
        .await;
}

#[cfg(test)]
mod subagent_tests;

#[cfg(test)]
mod transport_tests;

#[cfg(test)]
mod loop_tests;

/// The gates the simplification pass removed: what must now simply work.
#[cfg(test)]
mod freedom_tests;

#[cfg(test)]
mod trust_gate_tests;

#[cfg(test)]
mod checkpoint_event_tests;
