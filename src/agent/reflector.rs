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
    let request = crate::providers::ChatRequest {
        model_id: model_id.to_string(),
        system: vec![crate::providers::SystemPart::volatile(NEUTRALIZER_SYSTEM)],
        messages: vec![crate::providers::Message::new(
            crate::providers::Role::User,
            prompt,
        )],
        // Low, not None: the gateway returns an empty completion without an
        // effort budget on some models (observed, not theorized). Cheap call
        // either way — a handful of checks, no tools.
        effort: Some(crate::config::EffortLevel::Low),
        effort_support: Default::default(),
        max_tokens: Some(NEUTRALIZER_MAX_TOKENS),
        tools: Vec::new(),
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let text = tokio::time::timeout(
        std::time::Duration::from_secs(NEUTRALIZER_TIMEOUT_SECS),
        collect_text(provider, &request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("neutralizer timed out"))??;
    parse_checks(&text)
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

/// Journal `reflect` record fields for slice 1: context plus checks.
/// The full-verdict file (`journal/reflect/<seq>.json`) waits for slice 2.
pub fn reflect_fields(ctx: &ReflectContext, checks: &[Check]) -> serde_json::Value {
    serde_json::json!({
        "quote": ctx.quote,
        "artifacts": ctx.artifacts.iter().map(|a| &a.path).collect::<Vec<_>>(),
        "plan_id": ctx.plan.plan_id,
        "plan_refs": ctx.plan.refs,
        "checks": checks,
    })
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
    fn reflect_fields_carry_checks_for_the_record() {
        let checks = parse_checks(
            r#"[{"id": "c1", "kind": "plan_scope", "target": "p1", "method": "compare"}]"#,
        )
        .unwrap();
        let fields = reflect_fields(&ctx(), &checks);
        assert_eq!(fields["plan_id"], serde_json::json!("p1"));
        assert_eq!(fields["checks"][0]["kind"], serde_json::json!("plan_scope"));
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
}
