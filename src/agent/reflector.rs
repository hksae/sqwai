//! H1 reflector, slice 1: Scope + Neutralizer (§12.7).
//!
//! The neutralizer turns user criticism into neutral, verifiable checks.
//! It sees the criticism (de-framing is its job); the executor (slice 2)
//! will be blinded — checks with `expects` stripped, never the complaint.
//! Slice 1 runs the neutralizer synchronously on Fire+artifact turns and
//! records the checks in a journal `reflect` record. Nothing is shown yet:
//! the visible `[verified]` block waits for Executor+Verdict (slice 2).
//! Running now still pays off: real checks in production validate the
//! schema and the prompt before anything depends on them.

use serde::{Deserialize, Serialize};

use crate::agent::criticism;

/// What kind of inspection a check needs. The executor (slice 2) maps
/// each kind to a read-only tool subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// a file holds (or lacks) stated content
    File,
    /// a shell command exits as stated — executor-only, sandboxed
    Command,
    /// a symbol resolves where stated
    Symbol,
    /// the criticized work belongs to the current plan scope (mandatory)
    PlanScope,
}

/// One neutral, verifiable check. `expects` is the state that should hold;
/// slice 2 strips it before the blinded executor sees the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// auto-numbered (`c1`…) when the model omits it
    #[serde(default)]
    pub id: String,
    pub kind: CheckKind,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expects: Option<String>,
    #[serde(default)]
    pub method: String,
}

/// Everything the neutralizer may look at: criticism plus host facts.
/// The criticism quote is present (de-framing needs the source); model
/// prose and blame travel no further than this prompt.
#[derive(Debug, Clone)]
pub struct ReflectContext {
    pub quote: String,
    pub artifacts: Vec<criticism::ArtifactFact>,
    pub touched: Vec<criticism::ArtifactFact>,
    pub failures: Vec<String>,
    pub plan: PlanScope,
}

/// The scope under test: the active plan, or its honest absence.
#[derive(Debug, Clone)]
pub struct PlanScope {
    pub plan_id: Option<String>,
    pub goal: Option<String>,
    pub step: Option<String>,
    pub refs: Vec<String>,
}

/// Build the neutralizer's context from a fired criticism check.
/// `None` when there is nothing to verify (no artifacts, no touches —
/// the strict trigger should have held back already; this is the belt).
pub fn scope(
    root: &std::path::Path,
    session: &str,
    check: &criticism::Check,
    quote: &str,
) -> Option<ReflectContext> {
    if !check.fire || (check.artifacts.is_empty() && check.touched.is_empty()) {
        return None;
    }
    let plan = crate::plan::open_active_for_session(root, Some(session))
        .ok()
        .flatten()
        .map(|active| {
            let in_progress = active
                .steps
                .iter()
                .find(|s| s.status == crate::plan::StepStatus::InProgress);
            let step = in_progress.map(|s| {
                let mut line = format!("{}: {}", s.id, s.title);
                if !s.refs.is_empty() {
                    line.push_str(&format!(
                        " [refs: {}]",
                        s.refs
                            .iter()
                            .map(|r| r.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                line
            });
            PlanScope {
                plan_id: Some(active.id),
                goal: Some(active.goal.text.clone()),
                step,
                refs: in_progress
                    .map(|s| s.refs.iter().map(|r| r.path.clone()).collect())
                    .unwrap_or_default(),
            }
        })
        .unwrap_or(PlanScope {
            plan_id: None,
            goal: None,
            step: None,
            refs: Vec::new(),
        });
    Some(ReflectContext {
        quote: quote.trim().to_string(),
        artifacts: check.artifacts.clone(),
        touched: check.touched.clone(),
        failures: check.failures.clone(),
        plan,
    })
}

const NEUTRALIZER_SYSTEM: &str = "You turn user criticism of prior coding work into neutral, verifiable checks.\nOutput a JSON array only, no prose, no fences. Each item: {\"id\": \"c1\", \"kind\": \"file|command|symbol|plan_scope\", \"target\": \"...\", \"expects\": \"...\", \"method\": \"...\"}.\nRules: no blame or accusation language anywhere, not even quoted; every check states one fact a read-only inspection can confirm or refute; the FIRST item is always kind plan_scope, verifying the criticized work belongs to the current plan scope (or that the touched files form one coherent scope when no plan is active); the plan_scope target is the plan id verbatim as given above (no prefixes); at most 6 checks; expects is the state that should hold; method is one short imperative phrase.";
const NEUTRALIZER_MAX_TOKENS: u32 = 1200;
const NEUTRALIZER_TIMEOUT_SECS: u64 = 90;
const MAX_CHECKS: usize = 6;

/// Run the neutralizer: one tool-free model call, one schema-bound parse,
/// one retry on schema failure. `Err` means "no checks" — the caller keeps
/// the L0 fact block and moves on (degrade, don't refuse).
pub async fn neutralize(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    ctx: &ReflectContext,
) -> anyhow::Result<Vec<Check>> {
    let prompt = render_prompt(ctx);
    match neutralize_once(provider, model_id, &prompt).await {
        Ok(checks) => Ok(checks),
        Err(first) => {
            crate::providers::log_http(&format!("reflector: neutralizer retry after: {first:#}"));
            neutralize_once(
                provider,
                model_id,
                &format!("{prompt}\n\nOutput JSON array only."),
            )
            .await
        }
    }
}

async fn neutralize_once(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    prompt: &str,
) -> anyhow::Result<Vec<Check>> {
    let text = micro_call(
        provider,
        model_id,
        NEUTRALIZER_SYSTEM,
        prompt,
        NEUTRALIZER_MAX_TOKENS,
        NEUTRALIZER_TIMEOUT_SECS,
    )
    .await
    .map_err(|e| anyhow::anyhow!("neutralizer call: {e:#}"))?;
    parse_checks(&text)
}

/// One tool-free model call with a timeout. Shared by the neutralizer,
/// the H0-maybe confirm and the `/why` narrator: all are schema-bound
/// (or stop-ruled) micro-calls, not turns.
pub(crate) async fn micro_call(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    timeout_secs: u64,
) -> anyhow::Result<String> {
    let request = crate::providers::ChatRequest {
        model_id: model_id.to_string(),
        system: vec![crate::providers::SystemPart::volatile(system)],
        messages: vec![crate::providers::Message::new(
            crate::providers::Role::User,
            prompt,
        )],
        // Low, not None: the gateway returns an empty completion without an
        // effort budget on some models (observed, not theorized). Cheap call
        // either way — schema-bound, no tools.
        effort: Some(crate::config::EffortLevel::Low),
        effort_support: Default::default(),
        max_tokens: Some(max_tokens),
        tools: Vec::new(),
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        collect_text(provider, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("micro call timed out"))?
}

// --- H0-maybe confirm (reserved interface, now built) ------------------

/// The confirm's answer: is it criticism, and of what. `target` is a
/// quoted span from the message (path, symbol, or work description) or
/// null when the complaint names nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub is_criticism: bool,
    pub target: Option<String>,
}

const CONFIRM_SYSTEM: &str = "You classify one user message from a coding session. Output JSON only, no prose: {\"is_criticism\": true|false, \"target\": \"...\"|null}.\nCriticism = a complaint that prior work is broken, wrong, missing, or misattributed. Quote in target the complained-about work (path, symbol, or short description), or null when it names nothing.\nExasperation or impatience expressed right after the listed files changed counts as criticism of that work — name the touched files as target.\nBare interjections with no verb, pronoun, or reference are never criticism by themselves — false.\nNOT criticism: requests and questions about future work (even irritated ones), praise (even profane), redirects to new work, self-blame (\"my bad\", \"I broke it\"), and error discussion without blame. When in doubt, false.";
const CONFIRM_MAX_TOKENS: u32 = 1200;
const CONFIRM_TIMEOUT_SECS: u64 = 60;

/// Ask the model about a Maybe message the strict trigger held back:
/// Maybe + prior mutations + no resolved artifact. Runs at most once per
/// turn (the hook calls it, nothing else). Failure, timeout or schema
/// miss all mean "not confirmed" — silence, never a refusal.
pub async fn confirm(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    text: &str,
    touched: &[criticism::ArtifactFact],
) -> Option<Confirmation> {
    let mut prompt = format!("Message:\n\"{text}\"\n\nFiles the last turn touched:\n");
    if touched.is_empty() {
        prompt.push_str("(none)\n");
    } else {
        for fact in touched.iter().take(8) {
            prompt.push_str(&format!("- {}\n", fact.path));
        }
    }
    let text = micro_call(
        provider,
        model_id,
        CONFIRM_SYSTEM,
        &prompt,
        CONFIRM_MAX_TOKENS,
        CONFIRM_TIMEOUT_SECS,
    )
    .await
    .map_err(|e| crate::providers::log_http(&format!("reflector: confirm call failed: {e:#}")))
    .ok()?;
    parse_confirmation(&text).or_else(|| {
        crate::providers::log_http("reflector: confirm parse missed");
        None
    })
}

/// Strict parse: one object, boolean verdict, optional string target.
pub fn parse_confirmation(text: &str) -> Option<Confirmation> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let is_criticism = value.get("is_criticism")?.as_bool()?;
    let target = value
        .get("target")
        .and_then(|t| t.as_str())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(Confirmation {
        is_criticism,
        target,
    })
}

/// Upgrade a held-back Maybe: confirm, resolve the confirmed target
/// against the touched files, fire when it grounds. Returns the fired
/// check, or `None` when the gray stays gray. The marker keeps
/// verdict=maybe (the classifier's word); `fired` tells what happened.
pub async fn confirm_maybe(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    root: &std::path::Path,
    check: &criticism::Check,
    text: &str,
) -> Option<criticism::Check> {
    if check.verdict != criticism::Verdict::Maybe
        || check.fire
        || check.touched.is_empty()
        || !check.artifacts.is_empty()
    {
        return None;
    }
    let confirmation = confirm(provider, model_id, text, &check.touched).await?;
    if !confirmation.is_criticism {
        return None;
    }
    let source = match &confirmation.target {
        Some(target) => format!("{text} {target}"),
        None => text.to_string(),
    };
    let artifacts = criticism::match_artifacts(root, &source, &check.touched);
    if artifacts.is_empty() {
        return None;
    }
    Some(criticism::Check {
        verdict: check.verdict,
        score: check.score,
        artifacts,
        touched: check.touched.clone(),
        failures: check.failures.clone(),
        fire: true,
    })
}

async fn collect_text(
    provider: &crate::providers::SharedProvider,
    request: &crate::providers::ChatRequest,
) -> anyhow::Result<String> {
    use futures::StreamExt;
    let mut stream = provider.stream_chat(request.clone());
    let mut text = String::new();
    let (mut reasoning, mut tools, mut other) = (0u32, 0u32, 0u32);
    while let Some(event) = stream.next().await {
        match event? {
            crate::providers::StreamEvent::Text(chunk) => text.push_str(&chunk),
            crate::providers::StreamEvent::Reasoning(_) => reasoning += 1,
            crate::providers::StreamEvent::ToolCall(_) => tools += 1,
            _ => other += 1,
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        anyhow::bail!(
            "neutralizer returned empty text (reasoning={reasoning} toolcalls={tools} other={other})"
        );
    }
    Ok(text)
}

/// Strict parse: outermost JSON array, known kinds, non-empty targets,
/// auto-numbered ids, exactly the cap, and the mandatory plan_scope lead.
pub fn parse_checks(text: &str) -> anyhow::Result<Vec<Check>> {
    let start = text
        .find('[')
        .ok_or_else(|| anyhow::anyhow!("no JSON array found"))?;
    let end = text
        .rfind(']')
        .ok_or_else(|| anyhow::anyhow!("no JSON array found"))?;
    if end <= start {
        anyhow::bail!("no JSON array found");
    }
    let mut checks: Vec<Check> = serde_json::from_str(&text[start..=end])
        .map_err(|e| anyhow::anyhow!("checks do not parse: {e}"))?;
    if checks.is_empty() || checks.len() > MAX_CHECKS {
        anyhow::bail!("need 1-{} checks, got {}", MAX_CHECKS, checks.len());
    }
    for (i, check) in checks.iter_mut().enumerate() {
        if check.target.trim().is_empty() {
            anyhow::bail!("check {} has an empty target", i + 1);
        }
        if check.id.trim().is_empty() {
            check.id = format!("c{}", i + 1);
        }
        if check.method.trim().is_empty() {
            check.method = "inspect".to_string();
        }
    }
    if checks[0].kind != CheckKind::PlanScope {
        anyhow::bail!("first check must be kind plan_scope");
    }
    Ok(checks)
}

fn render_prompt(ctx: &ReflectContext) -> String {
    let mut out = String::from("User criticism:\n\"");
    out.push_str(&ctx.quote);
    out.push_str("\"\n\nHost facts (ground every check in these, invent nothing):\n");
    if ctx.artifacts.is_empty() {
        out.push_str("touched last turn:\n");
        for fact in &ctx.touched {
            out.push_str(&format!("- {}\n", fact.path));
        }
    } else {
        out.push_str("named artifacts:\n");
        for fact in &ctx.artifacts {
            out.push_str(&format!("- {}\n", fact.path));
        }
    }
    if !ctx.failures.is_empty() {
        out.push_str("recent tool failures:\n");
        for failure in &ctx.failures {
            out.push_str(&format!("- {failure}\n"));
        }
    }
    out.push_str("plan scope:\n");
    match (&ctx.plan.plan_id, &ctx.plan.goal) {
        (Some(id), Some(goal)) => {
            out.push_str(&format!("- plan {id}: {goal}\n"));
            if let Some(step) = &ctx.plan.step {
                out.push_str(&format!("- in-progress step: {step}\n"));
            }
            if !ctx.plan.refs.is_empty() {
                out.push_str(&format!("- step refs: {}\n", ctx.plan.refs.join(", ")));
            }
        }
        _ => out.push_str("- no active plan: scope is the touched files above\n"),
    }
    out
}

/// Journal `reflect` record fields: context, checks, outcomes, verdict.
/// Written once per executed reflection; the verdict file
/// (`journal/reflect/<seq>.json`) mirrors it for auditors.
pub fn verdict_fields(
    ctx: &ReflectContext,
    checks: &[Check],
    execution: &Execution,
) -> serde_json::Value {
    serde_json::json!({
        "quote": ctx.quote,
        "artifacts": ctx.artifacts.iter().map(|a| &a.path).collect::<Vec<_>>(),
        "plan_id": ctx.plan.plan_id,
        "plan_refs": ctx.plan.refs,
        "checks": checks,
        "outcomes": execution.outcomes.iter().map(|(c, o)| serde_json::json!({
            "id": c.id,
            "kind": c.kind,
            "target": c.target,
            "status": o.status,
            "evidence": o.evidence,
        })).collect::<Vec<_>>(),
        "verdict": execution.verdict,
    })
}

// --- Slice 2: Executor + Verdict -------------------------------------

/// Per-check outcome, reported by the executor model and parsed strictly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Confirmed,
    Refuted,
    Undetermined,
}

/// Host verdict over a finished execution. Computed from per-check
/// outcomes (§12.7), never by the model — the model reports facts,
/// the host judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// the criticized claims hold: we broke it
    AgentError,
    /// the criticized claims do not hold: work is fine
    ClaimNotConfirmed,
    /// some hold, some do not (or some unknown)
    Partial,
    /// the criticized work is outside the current plan scope
    ScopeMismatch,
    /// nothing could be verified
    Undetermined,
}

impl Verdict {
    /// snake_case label, shared by the block, records and statuses.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::AgentError => "agent_error",
            Verdict::ClaimNotConfirmed => "claim_not_confirmed",
            Verdict::Partial => "partial",
            Verdict::ScopeMismatch => "scope_mismatch",
            Verdict::Undetermined => "undetermined",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub status: OutcomeStatus,
    pub evidence: String,
}

#[derive(Debug)]
pub struct Execution {
    pub outcomes: Vec<(Check, CheckOutcome)>,
    pub verdict: Verdict,
}

/// Executor budget, mirroring subagents (decided): a bounded wall clock
/// plus a tool-call cap. Overrun leaves the remaining checks
/// `Undetermined` — a partial verification, never a hang.
pub const EXECUTOR_CALLS_PER_CHECK: usize = 6;
const EXECUTOR_MAX_TOKENS: u32 = 2000;

const EXECUTOR_SYSTEM: &str = "You verify ONE stated check against the worktree. Rules: read-only — inspection tools only, mutations are refused, and you cannot change anything; the complaint that motivated this check is deliberately withheld, verify the fact not a story; use as few calls as needed; finish with exactly one line: FINAL: {\"status\": \"confirmed|refuted|undetermined\", \"evidence\": \"<one sentence>\"}.";

/// Run one check to an outcome: a bounded tool-calling loop in a
/// reflector ToolCtx (dispatch refuses writers, gated bash, no plan).
/// The prompt carries target+method only — never `expects`, never the
/// criticism quote (blinding by construction: this function does not
/// even receive them).
pub async fn execute_check(
    provider: &crate::providers::SharedProvider,
    model_id: &str,
    root: &std::path::Path,
    check: &Check,
    goal: Option<&str>,
    deadline: tokio::time::Instant,
    calls_left: &mut usize,
) -> CheckOutcome {
    let specs = crate::agent::tools::reflector_specs();
    let mut ctx = crate::agent::tools::ToolCtx::new(root).in_session("reflect".to_string());
    ctx.reflector = true;
    let mut messages = vec![crate::providers::Message::new(
        crate::providers::Role::User,
        render_executor_prompt(check, goal),
    )];
    let mut transcript = String::new();
    let undetermined = |why: &str| CheckOutcome {
        status: OutcomeStatus::Undetermined,
        evidence: why.to_string(),
    };
    loop {
        if *calls_left == 0 || tokio::time::Instant::now() >= deadline {
            return undetermined("executor budget exhausted");
        }
        let request = crate::providers::ChatRequest {
            model_id: model_id.to_string(),
            system: vec![crate::providers::SystemPart::volatile(EXECUTOR_SYSTEM)],
            messages: messages.clone(),
            effort: Some(crate::config::EffortLevel::Low),
            effort_support: Default::default(),
            max_tokens: Some(EXECUTOR_MAX_TOKENS),
            tools: specs.clone(),
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let events = match tokio::time::timeout_at(deadline, collect_events(provider, &request))
            .await
        {
            Ok(Ok(events)) => events,
            Ok(Err(error)) => {
                crate::providers::log_http(&format!("reflector: executor call failed: {error:#}"));
                return undetermined("executor call failed");
            }
            Err(_) => return undetermined("executor wall clock exhausted"),
        };
        let mut calls: Vec<crate::providers::ToolCallReq> = Vec::new();
        for event in events {
            match event {
                crate::providers::StreamEvent::Text(chunk) => transcript.push_str(&chunk),
                crate::providers::StreamEvent::ToolCall(req) => calls.push(req),
                _ => {}
            }
        }
        if calls.is_empty() {
            return parse_final(&transcript);
        }
        let mut assistant =
            crate::providers::Message::new(crate::providers::Role::Assistant, transcript.clone());
        assistant.tool_calls = calls.clone();
        messages.push(assistant);
        for call in calls {
            if *calls_left == 0 {
                break;
            }
            *calls_left -= 1;
            let outcome = run_tool_blocking(&mut ctx, &call.name, &call.args).await;
            messages.push(crate::providers::Message::tool_result(
                call.id,
                outcome.output,
                !outcome.ok,
            ));
        }
        transcript.clear();
    }
}

async fn collect_events(
    provider: &crate::providers::SharedProvider,
    request: &crate::providers::ChatRequest,
) -> anyhow::Result<Vec<crate::providers::StreamEvent>> {
    use futures::StreamExt;
    let mut stream = provider.stream_chat(request.clone());
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event?);
    }
    Ok(events)
}

/// A blocking tool call must not stall the async runtime (same reason as
/// the main loop's `run_tool_blocking`): the reflector runs synchronously
/// inside the turn, on the driver's thread.
async fn run_tool_blocking(
    ctx: &mut crate::agent::tools::ToolCtx,
    name: &str,
    args: &serde_json::Value,
) -> crate::agent::tools::Outcome {
    let mut exec_ctx = ctx.clone();
    let name = name.to_string();
    let args = args.clone();
    let (outcome, exec_ctx) = tokio::task::spawn_blocking(move || {
        let o = crate::agent::tools::execute(&mut exec_ctx, &name, &args);
        (o, exec_ctx)
    })
    .await
    .unwrap_or_else(|e| {
        (
            crate::agent::tools::Outcome::err(format!("reflector tool thread failed: {e}")),
            ctx.clone(),
        )
    });
    ctx.files_read = exec_ctx.files_read;
    outcome
}

/// Strict FINAL parse: one line, one object, known status. Anything else
/// is `Undetermined` — a model that will not commit to a shape has not
/// verified anything either.
pub fn parse_final(transcript: &str) -> CheckOutcome {
    let blank = CheckOutcome {
        status: OutcomeStatus::Undetermined,
        evidence: String::new(),
    };
    let Some(at) = transcript.rfind("FINAL:") else {
        return blank;
    };
    let body = transcript[at + "FINAL:".len()..].trim();
    let end = body.find('\n').map(|i| &body[..i]).unwrap_or(body).trim();
    let value: serde_json::Value = match serde_json::from_str(end) {
        Ok(v) => v,
        Err(_) => return blank,
    };
    let status = match value.get("status").and_then(|s| s.as_str()) {
        Some("confirmed") => OutcomeStatus::Confirmed,
        Some("refuted") => OutcomeStatus::Refuted,
        Some("undetermined") => OutcomeStatus::Undetermined,
        _ => return blank,
    };
    let evidence = value
        .get("evidence")
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let evidence = if evidence.chars().count() > 300 {
        let taken: String = evidence.chars().take(299).collect();
        format!("{taken}…")
    } else {
        evidence
    };
    CheckOutcome { status, evidence }
}

fn render_executor_prompt(check: &Check, goal: Option<&str>) -> String {
    let mut out = format!(
        "Check {} [{:?}] target: {}\nMethod: {}\n",
        check.id, check.kind, check.target, check.method
    );
    if let Some(goal) = goal {
        out.push_str(&format!("Plan goal (scope context only): {goal}\n"));
    }
    out
}

/// Host verdict from per-check outcomes. The plan_scope check decides
/// scope first; the rest decide fault. Anything unverified makes a
/// one-sided result `Partial` rather than certain — except all-unknown,
/// which is `Undetermined`.
pub fn verdict(checks: &[(Check, CheckOutcome)]) -> Verdict {
    if let Some((_, scope)) = checks.iter().find(|(c, _)| c.kind == CheckKind::PlanScope)
        && scope.status == OutcomeStatus::Refuted
    {
        return Verdict::ScopeMismatch;
    }
    let rest: Vec<&CheckOutcome> = checks
        .iter()
        .filter(|(c, _)| c.kind != CheckKind::PlanScope)
        .map(|(_, o)| o)
        .collect();
    if rest.is_empty() {
        return Verdict::Undetermined;
    }
    let confirmed = rest
        .iter()
        .filter(|o| o.status == OutcomeStatus::Confirmed)
        .count();
    let refuted = rest
        .iter()
        .filter(|o| o.status == OutcomeStatus::Refuted)
        .count();
    let unknown = rest.len() - confirmed - refuted;
    if unknown == rest.len() {
        Verdict::Undetermined
    } else if refuted == 0 && unknown == 0 {
        Verdict::AgentError
    } else if confirmed == 0 && unknown == 0 {
        Verdict::ClaimNotConfirmed
    } else {
        Verdict::Partial
    }
}

/// What a verification run produced: the host block plus its verdict.
/// The caller decides where the block travels — prepended to the turn's
/// assistant messages (auto flow) or a durable chat row (`/verify`).
#[derive(Debug)]
pub struct VerifyReport {
    pub block: String,
    pub verdict: Verdict,
    /// journal `reflect` seq, when the record landed
    pub seq: Option<u64>,
}

/// Full verification flow for one reflection: execute every check under
/// the budget, judge, record, render. Synchronous from the turn hook or
/// the `/verify` task; every failure degrades to whatever carried the
/// turn before (never an error turn).
#[allow(clippy::too_many_arguments)]
pub async fn verify(
    root: &std::path::Path,
    session: &str,
    model_id: &str,
    provider: &crate::providers::SharedProvider,
    writer: &mut crate::agent::journal::Journal,
    rctx: &ReflectContext,
    checks: &[Check],
    budget: &criticism::Budget,
) -> VerifyReport {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(budget.wall_secs);
    let mut calls_left = budget.calls;
    let mut outcomes: Vec<(Check, CheckOutcome)> = Vec::new();
    for check in checks {
        if calls_left == 0 || tokio::time::Instant::now() >= deadline {
            outcomes.push((
                check.clone(),
                CheckOutcome {
                    status: OutcomeStatus::Undetermined,
                    evidence: "executor budget exhausted".into(),
                },
            ));
            continue;
        }
        let mut per_check = EXECUTOR_CALLS_PER_CHECK.min(calls_left);
        let before = calls_left;
        let outcome = execute_check(
            provider,
            model_id,
            root,
            check,
            rctx.plan.goal.as_deref(),
            deadline,
            &mut per_check,
        )
        .await;
        calls_left -= before - per_check;
        outcomes.push((check.clone(), outcome));
    }
    let execution = Execution {
        outcomes,
        verdict: Verdict::Undetermined,
    };
    let verdict = verdict(&execution.outcomes);
    let execution = Execution {
        outcomes: execution.outcomes,
        verdict,
    };
    let artifacts: Vec<String> = rctx.artifacts.iter().map(|a| a.path.clone()).collect();
    let recurrence = recurrence(root, session, rctx.plan.plan_id.as_deref(), &artifacts);
    let has_recurrence = recurrence.0 > 0;
    let seq = writer
        .append("reflect", verdict_fields(rctx, checks, &execution))
        .ok();
    if let Some(seq) = seq {
        let dir = root.join(".sqwai").join("journal").join("reflect");
        std::fs::create_dir_all(&dir).ok();
        let mut file = verdict_fields(rctx, checks, &execution);
        file["seq"] = serde_json::json!(seq);
        file["model"] = serde_json::json!(model_id);
        let _ = std::fs::write(
            dir.join(format!("{seq}.json")),
            serde_json::to_string_pretty(&file).unwrap_or_default(),
        );
    }
    if verdict == Verdict::AgentError {
        let _ = writer.append(
            "note",
            serde_json::json!({
                "by": "host",
                "note": "lesson",
                "text": format!(
                    "reflector verified agent_error on {}: {}",
                    artifacts.join(", "),
                    rctx.quote,
                ),
            }),
        );
    }
    let block = render_block(&execution, has_recurrence.then_some(recurrence));
    VerifyReport {
        block,
        verdict,
        seq,
    }
}

/// Manual `/verify` target: no classifier (the command IS the assertion),
/// just grounding against the window. `None` when there is nothing to
/// verify — the caller says so instead of running an empty reflection.
pub fn manual(
    root: &std::path::Path,
    session: &str,
    text: &str,
    window: usize,
) -> Option<(ReflectContext, criticism::Check)> {
    let (touched, failures) = criticism::gather(root, session, window);
    if touched.is_empty() {
        return None;
    }
    let artifacts = criticism::match_artifacts(root, text, &touched);
    let check = criticism::Check {
        verdict: criticism::Verdict::Fire,
        score: 1.0,
        artifacts,
        touched,
        failures,
        fire: true,
    };
    let rctx = scope(root, session, &check, text)?;
    Some((rctx, check))
}

/// Fired `criticism` markers since the last `reflect` record: the
/// objection count for self-protection (slice 3). The current turn's
/// marker is not written yet when the hook counts, so this is prior
/// objections only.
pub fn objections_after_last_verify(root: &std::path::Path, session: &str) -> usize {
    let records = crate::agent::journal::Journal::records_for(root, session).unwrap_or_default();
    let mut objections = 0;
    for record in records.iter().rev() {
        if record.kind == "reflect" {
            break;
        }
        if record.kind == "criticism"
            && record.fields.get("fired").and_then(|v| v.as_bool()) == Some(true)
        {
            objections += 1;
        }
    }
    objections
}

/// Third objection after `[verified]` disables the reflector for the
/// session (§12.7). Journal-first like everything else: durable, visible,
/// replayable — no hidden in-memory switch.
pub fn reflector_disabled(root: &std::path::Path, session: &str) -> bool {
    crate::agent::journal::Journal::records_for(root, session)
        .map(|records| records.iter().any(|r| r.kind == "reflector_disabled"))
        .unwrap_or(false)
}

/// Most recent fired criticism text, for `/verify` without a fresh
/// complaint: verify what was last objected to.
pub fn last_criticism_text(root: &std::path::Path, session: &str) -> Option<String> {
    crate::agent::journal::Journal::records_for(root, session)
        .ok()?
        .iter()
        .rev()
        .find(|r| {
            r.kind == "criticism" && r.fields.get("fired").and_then(|v| v.as_bool()) == Some(true)
        })
        .and_then(|r| {
            r.fields
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
}

/// Host-rendered `[verified]` block, prepended to the turn's assistant
/// messages. Deterministic format — apology theater is impossible by
/// construction, and the label marks host verification, not model prose.
pub fn render_block(execution: &Execution, recurrence: Option<(usize, Vec<String>)>) -> String {
    let mut out = format!(
        "[verified: {} — host check, not model prose]\n",
        execution.verdict.label()
    );
    for (check, outcome) in &execution.outcomes {
        let status = match outcome.status {
            OutcomeStatus::Confirmed => "confirmed",
            OutcomeStatus::Refuted => "refuted",
            OutcomeStatus::Undetermined => "undetermined",
        };
        if outcome.evidence.is_empty() {
            out.push_str(&format!(
                "{} {:?} {}: {status}\n",
                check.id, check.kind, check.target
            ));
        } else {
            out.push_str(&format!(
                "{} {:?} {}: {status} — {}\n",
                check.id, check.kind, check.target, outcome.evidence
            ));
        }
    }
    if let Some((times, paths)) = recurrence
        && times > 0
    {
        out.push_str(&format!(
            "recurring unconfirmed criticism about {} ({times}×) — consider a memory_propose\n",
            paths.join(", ")
        ));
    }
    out
}

/// Prior `claim_not_confirmed` reflects on the same plan with overlapping
/// artifacts: how many, and which paths recur. Slice 2 keeps the count;
/// slice 3 (`/verify`, self-protection) will act on it.
pub fn recurrence(
    root: &std::path::Path,
    session: &str,
    plan_id: Option<&str>,
    artifacts: &[String],
) -> (usize, Vec<String>) {
    if artifacts.is_empty() {
        return (0, Vec::new());
    }
    let mut times = 0usize;
    let mut paths: Vec<String> = Vec::new();
    let records = crate::agent::journal::Journal::records_for(root, session).unwrap_or_default();
    for record in records.iter().rev().take(200) {
        if record.kind != "reflect" {
            continue;
        }
        if record.fields.get("verdict").and_then(|v| v.as_str()) != Some("claim_not_confirmed") {
            continue;
        }
        if plan_id
            .is_some_and(|id| record.fields.get("plan_id").and_then(|v| v.as_str()) != Some(id))
        {
            continue;
        }
        let prior: Vec<String> = record
            .fields
            .get("artifacts")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let overlap: Vec<String> = artifacts
            .iter()
            .filter(|a| prior.iter().any(|p| p == *a))
            .cloned()
            .collect();
        if !overlap.is_empty() {
            times += 1;
            for path in overlap {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    (times, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(path: &str) -> criticism::ArtifactFact {
        criticism::ArtifactFact {
            path: path.to_string(),
            added: Some(3),
            removed: Some(1),
            step: Some("2".into()),
        }
    }

    fn ctx() -> ReflectContext {
        ReflectContext {
            quote: "ты сломал src/a.rs".into(),
            artifacts: vec![fact("src/a.rs")],
            touched: vec![fact("src/a.rs"), fact("src/b.rs")],
            failures: vec!["bash: exit 1".into()],
            plan: PlanScope {
                plan_id: Some("p1".into()),
                goal: Some("ship it".into()),
                step: Some("2: touch a".into()),
                refs: Vec::new(),
            },
        }
    }

    #[test]
    fn parse_accepts_valid_checks_with_plan_scope_lead() {
        let checks = parse_checks(
            r#"[{"id": "c1", "kind": "plan_scope", "target": "p1", "expects": "auth work is in scope", "method": "compare refs"}, {"kind": "file", "target": "src/a.rs", "expects": "parses", "method": "read"}]"#,
        )
        .expect("parses");
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[1].id, "c2", "ids auto-number when absent");
        assert_eq!(checks[1].method, "read");
    }

    #[test]
    fn parse_rejects_missing_plan_scope_lead() {
        let err = parse_checks(
            r#"[{"id": "c1", "kind": "file", "target": "src/a.rs", "method": "read"}]"#,
        )
        .expect_err("plan_scope lead is mandatory");
        assert!(err.to_string().contains("plan_scope"), "{err}");
    }

    #[test]
    fn parse_rejects_prose_empty_and_oversized() {
        assert!(parse_checks("no json here").is_err());
        assert!(parse_checks(r#"[]"#).is_err());
        let many: Vec<String> = (0..8)
            .map(|i| {
                format!(r#"{{"id": "c{i}", "kind": "file", "target": "f{i}", "method": "read"}}"#)
            })
            .collect();
        let mut with_lead = vec![
            r#"{"id": "c0", "kind": "plan_scope", "target": "p", "method": "compare"}"#.to_string(),
        ];
        with_lead.extend(many);
        assert!(parse_checks(&format!("[{}]", with_lead.join(","))).is_err());
    }

    #[test]
    fn parse_tolerates_fences_and_surrounding_prose() {
        let checks = parse_checks(
            "Here are the checks:\n```json\n[{\"id\": \"c1\", \"kind\": \"plan_scope\", \"target\": \"p1\", \"method\": \"compare\"}]\n```",
        )
        .expect("fences tolerated");
        assert_eq!(checks.len(), 1);
    }

    #[test]
    fn prompt_grounds_in_facts_and_plan() {
        let prompt = render_prompt(&ctx());
        for needle in [
            "ты сломал src/a.rs",
            "src/a.rs",
            "bash: exit 1",
            "plan p1: ship it",
            "2: touch a",
        ] {
            assert!(prompt.contains(needle), "missing {needle}:\n{prompt}");
        }
    }

    #[test]
    fn scope_holds_back_without_fire() {
        let dir = std::env::temp_dir().join(format!("sqwai-refl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let check = criticism::Check {
            verdict: criticism::Verdict::Silent,
            score: 0.0,
            artifacts: Vec::new(),
            touched: Vec::new(),
            failures: Vec::new(),
            fire: false,
        };
        assert!(scope(&dir, "sess", &check, "сделай вот так").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verdict_fields_carry_checks_outcomes_and_verdict() {
        let checks = parse_checks(
            r#"[{"id": "c1", "kind": "plan_scope", "target": "p1", "method": "compare"}]"#,
        )
        .unwrap();
        let execution = Execution {
            outcomes: vec![(
                checks[0].clone(),
                CheckOutcome {
                    status: OutcomeStatus::Confirmed,
                    evidence: "in scope".into(),
                },
            )],
            verdict: Verdict::Partial,
        };
        let fields = verdict_fields(&ctx(), &checks, &execution);
        assert_eq!(fields["plan_id"], serde_json::json!("p1"));
        assert_eq!(fields["checks"][0]["kind"], serde_json::json!("plan_scope"));
        assert_eq!(
            fields["outcomes"][0]["status"],
            serde_json::json!("confirmed")
        );
        assert_eq!(fields["verdict"], serde_json::json!("partial"));
    }

    fn outcome(status: OutcomeStatus) -> CheckOutcome {
        CheckOutcome {
            status,
            evidence: "e".into(),
        }
    }

    fn check(id: &str, kind: CheckKind) -> Check {
        Check {
            id: id.into(),
            kind,
            target: "t".into(),
            expects: None,
            method: "inspect".into(),
        }
    }

    #[test]
    fn verdict_mapping_covers_the_matrix() {
        use OutcomeStatus::{Confirmed as C, Refuted as R, Undetermined as U};
        // scope decides first
        assert_eq!(
            verdict(&[(check("c1", CheckKind::PlanScope), outcome(R))]),
            Verdict::ScopeMismatch
        );
        // all confirmed → we broke it
        assert_eq!(
            verdict(&[
                (check("c1", CheckKind::PlanScope), outcome(C)),
                (check("c2", CheckKind::File), outcome(C)),
            ]),
            Verdict::AgentError
        );
        // all refuted → work is fine
        assert_eq!(
            verdict(&[
                (check("c1", CheckKind::PlanScope), outcome(C)),
                (check("c2", CheckKind::File), outcome(R)),
            ]),
            Verdict::ClaimNotConfirmed
        );
        // mixed → partial
        assert_eq!(
            verdict(&[
                (check("c2", CheckKind::File), outcome(C)),
                (check("c3", CheckKind::File), outcome(R)),
            ]),
            Verdict::Partial
        );
        // one-sided plus unknown → partial, not certain
        assert_eq!(
            verdict(&[
                (check("c2", CheckKind::File), outcome(C)),
                (check("c3", CheckKind::File), outcome(U)),
            ]),
            Verdict::Partial
        );
        // all unknown → undetermined
        assert_eq!(
            verdict(&[(check("c2", CheckKind::File), outcome(U))]),
            Verdict::Undetermined
        );
        // nothing but scope → nothing verified
        assert_eq!(
            verdict(&[(check("c1", CheckKind::PlanScope), outcome(C))]),
            Verdict::Undetermined
        );
        assert_eq!(verdict(&[]), Verdict::Undetermined);
    }

    #[test]
    fn final_parse_is_strict() {
        let ok = parse_final(
            "some reasoning\nFINAL: {\"status\": \"confirmed\", \"evidence\": \"parses clean\"}",
        );
        assert_eq!(ok.status, OutcomeStatus::Confirmed);
        assert!(ok.evidence.contains("parses clean"));
        assert_eq!(
            parse_final("no verdict here").status,
            OutcomeStatus::Undetermined
        );
        assert_eq!(
            parse_final("FINAL: {\"status\": \"maybe\", \"evidence\": \"x\"}").status,
            OutcomeStatus::Undetermined
        );
        assert_eq!(
            parse_final("FINAL: not json").status,
            OutcomeStatus::Undetermined
        );
    }

    #[test]
    fn executor_prompt_blinds_expects_and_quote() {
        let check = Check {
            id: "c1".into(),
            kind: CheckKind::File,
            target: "src/a.rs".into(),
            expects: Some("you broke auth, idiot".into()),
            method: "read".into(),
        };
        let prompt = render_executor_prompt(&check, Some("ship it"));
        assert!(prompt.contains("src/a.rs"), "{prompt}");
        assert!(!prompt.contains("you broke auth"), "{prompt}");
        assert!(!prompt.contains("expects"), "{prompt}");
    }

    #[test]
    fn block_renders_deterministically_with_recurrence() {
        let execution = Execution {
            outcomes: vec![(
                check("c1", CheckKind::File),
                CheckOutcome {
                    status: OutcomeStatus::Confirmed,
                    evidence: "red".into(),
                },
            )],
            verdict: Verdict::AgentError,
        };
        let block = render_block(&execution, Some((2, vec!["src/a.rs".into()])));
        assert!(block.starts_with("[verified: agent_error"), "{block}");
        assert!(block.contains("c1"), "{block}");
        assert!(block.contains("memory_propose"), "{block}");
        let plain = render_block(&execution, None);
        assert!(!plain.contains("memory_propose"), "{plain}");
    }

    #[test]
    fn recurrence_counts_same_plan_overlapping_unconfirmed() {
        let dir = std::env::temp_dir().join(format!("sqwai-refl-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        for seq_note in ["first", "second"] {
            journal
                .append(
                    "reflect",
                    serde_json::json!({
                        "verdict": "claim_not_confirmed",
                        "plan_id": "p1",
                        "artifacts": ["src/a.rs"],
                        "note": seq_note,
                    }),
                )
                .unwrap();
        }
        journal
            .append(
                "reflect",
                serde_json::json!({
                    "verdict": "agent_error",
                    "plan_id": "p1",
                    "artifacts": ["src/a.rs"],
                }),
            )
            .unwrap();
        let (times, paths) = recurrence(&dir, "sess", Some("p1"), &["src/a.rs".to_string()]);
        assert_eq!(times, 2, "only the unconfirmed pair counts");
        assert_eq!(paths, vec!["src/a.rs".to_string()]);
        let (other_plan, _) = recurrence(&dir, "sess", Some("p9"), &["src/a.rs".to_string()]);
        assert_eq!(other_plan, 0);
        let (other_path, _) = recurrence(&dir, "sess", Some("p1"), &["src/z.rs".to_string()]);
        assert_eq!(other_path, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Tone invariance (§12.7): same facts, hostile vs flat phrasing, must
    /// yield the same check targets. Live model — run explicitly:
    /// `SQWAI_BENCH_MODEL=<key> cargo test -- --ignored reflector_tone --test-threads=1`
    #[test]
    #[ignore]
    fn reflector_tone_invariance() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let Some(model) = crate::agent::bench_harness::bench_model() else {
                eprintln!("SKIP: no bench model (set SQWAI_BENCH_MODEL)");
                return;
            };
            // session-aware gateways (OpenCode Go) 400 without this header;
            // mirrors what run_arm does per cell. Scoped by host, so other
            // providers never see it.
            crate::providers::set_conversation_id("reflect-tone-test");
            let base = ctx();
            let hostile = ReflectContext {
                quote: "ты всё сломал, долбаёб, src/a.rs лежит".into(),
                ..base.clone()
            };
            let flat = ReflectContext {
                quote: "src/a.rs seems broken".into(),
                ..base
            };
            let b = neutralize(&model.provider, &model.model_id, &flat)
                .await
                .expect("flat neutralizes");
            let a = neutralize(&model.provider, &model.model_id, &hostile)
                .await
                .expect("hostile neutralizes");
            let targets = |checks: &[Check]| {
                let mut t: Vec<String> = checks
                    .iter()
                    .map(|c| {
                        // tone leaks into surface wording ("plan p1" vs "p1");
                        // the property is same checks, not same bytes
                        let bare = c.target.trim();
                        let bare = bare
                            .strip_prefix("plan ")
                            .or_else(|| bare.strip_prefix("Plan "))
                            .unwrap_or(bare);
                        format!("{:?}:{}", c.kind, bare.to_lowercase())
                    })
                    .collect();
                t.sort();
                t
            };
            assert_eq!(targets(&a), targets(&b), "tone must not move targets");
        });
    }

    /// Executor end to end (§12.7): true checks confirm, a missing symbol
    /// refutes, verdict comes out Partial. Live model, real tools, temp
    /// project — run explicitly:
    /// `SQWAI_BENCH_MODEL=<key> cargo test -- --ignored reflector_executor_live --test-threads=1`
    #[test]
    #[ignore]
    fn reflector_executor_live() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let Some(model) = crate::agent::bench_harness::bench_model() else {
                eprintln!("SKIP: no bench model (set SQWAI_BENCH_MODEL)");
                return;
            };
            crate::providers::set_conversation_id("reflect-executor-test");
            let dir = std::env::temp_dir().join(format!("sqwai-refl-live-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("src/lib.rs"),
                "pub fn calculate(x: i32) -> i32 {\n    x + 1\n}\n",
            )
            .unwrap();
            let mut store = crate::agent::graph::SqliteGraphStore::open(&dir).expect("graph opens");
            crate::agent::graph_index::index_project(&mut store, &dir).expect("indexed");

            let checks = vec![
                Check {
                    id: "c1".into(),
                    kind: CheckKind::File,
                    target: "src/lib.rs".into(),
                    expects: None,
                    method: "read the file and confirm it defines calculate".into(),
                },
                Check {
                    id: "c2".into(),
                    kind: CheckKind::Symbol,
                    target: "calculate".into(),
                    expects: None,
                    method: "resolve the symbol and confirm where it is defined".into(),
                },
                Check {
                    id: "c3".into(),
                    kind: CheckKind::Symbol,
                    target: "nope_xyz_missing".into(),
                    expects: None,
                    method: "resolve the symbol; refute if nothing defines it".into(),
                },
            ];
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
            let mut calls_left = 24;
            let mut outcomes = Vec::new();
            for check in &checks {
                outcomes.push((
                    check.clone(),
                    execute_check(
                        &model.provider,
                        &model.model_id,
                        &dir,
                        check,
                        Some("test"),
                        deadline,
                        &mut calls_left,
                    )
                    .await,
                ));
            }
            let by_id = |id: &str| {
                outcomes
                    .iter()
                    .find(|(c, _)| c.id == id)
                    .map(|(_, o)| o.status)
                    .expect("outcome present")
            };
            assert_eq!(by_id("c1"), OutcomeStatus::Confirmed, "{outcomes:?}");
            assert_eq!(by_id("c2"), OutcomeStatus::Confirmed, "{outcomes:?}");
            assert_eq!(by_id("c3"), OutcomeStatus::Refuted, "{outcomes:?}");
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn objections_count_fired_markers_since_last_reflect() {
        let dir = std::env::temp_dir().join(format!("sqwai-refl-obj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        let fire = |journal: &mut crate::agent::journal::Journal| {
            journal
                .append("criticism", serde_json::json!({"fired": true, "text": "x"}))
                .unwrap()
        };
        fire(&mut journal);
        fire(&mut journal);
        assert_eq!(objections_after_last_verify(&dir, "sess"), 2);
        journal
            .append("reflect", serde_json::json!({"verdict": "partial"}))
            .unwrap();
        assert_eq!(objections_after_last_verify(&dir, "sess"), 0);
        fire(&mut journal);
        assert_eq!(objections_after_last_verify(&dir, "sess"), 1);
        // unfired markers never count, even after the verify
        journal
            .append(
                "criticism",
                serde_json::json!({"fired": false, "text": "y"}),
            )
            .unwrap();
        assert_eq!(objections_after_last_verify(&dir, "sess"), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_flag_reads_the_record() {
        let dir = std::env::temp_dir().join(format!("sqwai-refl-dis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!reflector_disabled(&dir, "sess"));
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        journal
            .append("reflector_disabled", serde_json::json!({}))
            .unwrap();
        assert!(reflector_disabled(&dir, "sess"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manual_grounds_without_classifier() {
        let dir = std::env::temp_dir().join(format!("sqwai-refl-man-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // empty window: nothing to verify
        assert!(manual(&dir, "sess", "anything", 80).is_none());
        let mut journal =
            crate::agent::journal::Journal::open(&dir, "sess").expect("journal opens");
        journal
            .append("file_diff", serde_json::json!({"path": "src/a.rs"}))
            .unwrap();
        // a flat request the classifier would never fire on still verifies:
        // the command IS the assertion
        let (rctx, check) = manual(&dir, "sess", "show me the diff", 80).expect("manual");
        assert!(check.fire);
        assert_eq!(check.verdict, crate::agent::criticism::Verdict::Fire);
        assert!(!rctx.artifacts.is_empty() || !rctx.touched.is_empty());
        // last_criticism_text prefers the latest fired marker
        journal
            .append(
                "criticism",
                serde_json::json!({"fired": true, "text": "old complaint"}),
            )
            .unwrap();
        journal
            .append(
                "criticism",
                serde_json::json!({"fired": true, "text": "new complaint"}),
            )
            .unwrap();
        assert_eq!(
            last_criticism_text(&dir, "sess").as_deref(),
            Some("new complaint")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn budgets_widen_by_level() {
        use crate::agent::criticism::Budget;
        assert!(Budget::auto().window < Budget::verify().window);
        assert!(Budget::verify().window < Budget::full().window);
        assert!(Budget::auto().calls < Budget::verify().calls);
        assert!(Budget::auto().wall_secs <= 600);
    }

    #[test]
    fn confirmation_parse_is_strict() {
        let yes = parse_confirmation("{\"is_criticism\": true, \"target\": \"src/auth\"}")
            .expect("parses");
        assert!(yes.is_criticism);
        assert_eq!(yes.target.as_deref(), Some("src/auth"));
        let no = parse_confirmation(" рад: {\"is_criticism\": false, \"target\": null} ")
            .expect("parses");
        assert!(!no.is_criticism);
        assert_eq!(no.target, None);
        assert!(parse_confirmation("no json here").is_none());
        assert!(parse_confirmation("{\"is_criticism\": \"yes\"}").is_none());
        assert!(parse_confirmation("{\"target\": \"x\"}").is_none());
    }

    /// Confirm on the live model (§12.7 gray zone): bare ambiguity resolves
    /// without the strict trigger's artifact. Run explicitly:
    /// `SQWAI_BENCH_MODEL=<key> cargo test -- --ignored reflector_confirm_live --test-threads=1`
    #[test]
    #[ignore]
    fn reflector_confirm_live() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let Some(model) = crate::agent::bench_harness::bench_model() else {
                eprintln!("SKIP: no bench model (set SQWAI_BENCH_MODEL)");
                return;
            };
            crate::providers::set_conversation_id("reflect-confirm-test");
            let touched = vec![criticism::ArtifactFact {
                path: "src/a.rs".into(),
                added: Some(1),
                removed: Some(0),
                step: None,
            }];
            let yes = confirm(
                &model.provider,
                &model.model_id,
                "ну сколько можно",
                &touched,
            )
            .await
            .expect("confirm answers");
            assert!(yes.is_criticism, "{yes:?}");
            let bare = confirm(&model.provider, &model.model_id, "сука", &touched)
                .await
                .expect("confirm answers");
            assert!(!bare.is_criticism, "{bare:?}");
            let self_blame = confirm(
                &model.provider,
                &model.model_id,
                "my bad, I broke it",
                &touched,
            )
            .await
            .expect("confirm answers");
            assert!(!self_blame.is_criticism, "{self_blame:?}");
        });
    }
}
