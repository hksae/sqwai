//! G0 benchmark harness (§8.2). Live, ignored-by-default tests driving full
//! agent runs against fixture copies.
//!
//! Run one shakedown first (single task, single arm), then the matrix:
//! `cargo test -- --ignored bench_ --test-threads=1` with `SQWAI_BENCH_MODEL`
//! set. Never in CI: these spend real money and wall time.
//!
//! What a run collects (no mid-run forks — end-state plus journal facts):
//! - journal records (compactions, tool calls, evidence, receipts),
//! - plan file (steps, acceptance validation) for the mechanism arm,
//! - fixture state (acceptance commands, trap-file hashes),
//! - provider usage events (cost), per-turn latencies,
//! - finish per the pre-registered symmetric definitions (§8.2).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::agent::loop_task::{
    AgentEvent, AgentInput, FallbackCandidate,
};
use crate::providers::{Message, Role, SharedProvider, Usage};

const WALL_CAP: Duration = Duration::from_secs(3600);
const COMPACTION_THRESHOLD: f64 = 0.04;

pub struct TaskSpec {
    pub id: &'static str,
    pub goal: &'static str,
    pub constraints: &'static [&'static str],
    pub acceptance_cmds: &'static [&'static str],
}

pub const T1: TaskSpec = TaskSpec {
    id: "T1",
    goal: "implement absolute unix-millis TTL expiry for keys end to end in minidb",
    constraints: &[
        "do not break the public API of `engine` (signatures stay)",
        "keep the on-disk log format backward compatible (old lines must load)",
        "`tmp:` scratch keys must never persist (existing invariant, keep it)",
    ],
    acceptance_cmds: &["cargo test --test engine"],
};

pub const T2: TaskSpec = TaskSpec {
    id: "T2",
    goal: "rename `get_unchecked` to `get_raw` in `engine`, `cli` and every caller, with no behavior change",
    constraints: &[
        "do not touch `storage/btree.rs` (deprecated)",
        "no behavior change: the suite passes identically before and after",
    ],
    acceptance_cmds: &["cargo test --test engine"],
};

pub const T3: TaskSpec = TaskSpec {
    id: "T3",
    goal: "make `cli_batch_persists_every_write` pass without breaking anything else",
    constraints: &[
        "do not change the log format",
        "do not touch `storage/btree.rs`",
    ],
    acceptance_cmds: &["cargo test"],
};

/// Prompt text per arm. Same goal/constraints/acceptance; only the plan
/// instruction differs (the baseline has no plan tools).
pub fn task_prompt(task: &TaskSpec, baseline: bool) -> String {
    let mut out = format!("Goal: {}.\n", task.goal);
    out.push_str("Constraints:\n");
    for (i, c) in task.constraints.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, c));
    }
    out.push_str("Acceptance will be checked with:\n");
    for cmd in task.acceptance_cmds {
        out.push_str(&format!("- `{cmd}`\n"));
    }
    if !baseline {
        out.push_str("Create a plan first, then work through it.\n");
    }
    out
}

pub struct BenchModel {
    pub provider: SharedProvider,
    pub model_id: String,
    pub effort_support: crate::config::EffortSupport,
    pub context_limit: u64,
}

/// Real provider from the user's own config. `SQWAI_BENCH_MODEL` picks the
/// model key, defaulting to the configured last model. `None` → skip the
/// test (no keys on this machine, e.g. CI).
pub fn bench_model() -> Option<BenchModel> {
    let cfg = crate::config::Config::load().ok()?;
    let key = std::env::var("SQWAI_BENCH_MODEL").unwrap_or_else(|_| cfg.last_model.clone());
    let mc = cfg.models.get(&key)?.clone();
    let resolved = cfg.resolve_provider(&mc).ok()?;
    let provider = crate::providers::create(&resolved).ok()?;
    Some(BenchModel {
        provider,
        model_id: mc.id.clone(),
        effort_support: mc.effort_support(resolved.format),
        context_limit: mc.context,
    })
}

pub fn fixture_source() -> PathBuf {
    std::env::var("SQWAI_BENCH_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Users\Asus\minidb"))
}

/// Fresh writable copy of the fixture. The original is never mutated.
pub fn fresh_copy(task: &TaskSpec, arm: &str) -> Option<PathBuf> {
    let src = fixture_source();
    if !src.join("Cargo.toml").exists() {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dest = std::env::temp_dir().join(format!("sqwai-bench-{}-{arm}-{nanos}", task.id));
    copy_dir(&src, &dest).ok()?;
    Some(dest)
}

fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct RunReport {
    pub task: String,
    pub arm: String,
    /// fixture copy this run mutated (for acceptance + trap scoring)
    pub root: PathBuf,
    pub session_id: String,
    pub wall_secs: u64,
    pub tool_calls: usize,
    pub compactions: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// gaps (secs) between a tool result landing and the next assistant
    /// text: proxy for post-compaction slowdown
    pub latencies: Vec<u64>,
    /// mechanism finish: all steps done + acceptance verified/waived
    pub plan_finished: bool,
    /// baseline finish: model claimed done in final text
    pub claimed_done: bool,
    pub timed_out: bool,
    pub error: Option<String>,
    pub final_text: String,
}

/// Drive one full run to completion (or the wall cap). Returns the report;
/// the caller scores fixture state separately.
pub async fn run_arm(task: &TaskSpec, baseline: bool, session_tag: &str) -> Option<RunReport> {
    let model = bench_model()?;
    crate::bench::set_baseline_override(Some(baseline));
    let started = Instant::now();
    let mut report = RunReport {
        task: task.id.to_string(),
        arm: if baseline { "baseline".into() } else { "mechanism".into() },
        ..Default::default()
    };

    let session_id = format!("bench-{}-{session_tag}", task.id);
    report.session_id = session_id.clone();
    let root = fresh_copy(task, &report.arm)?;
    let mut compaction = crate::config::CompactionConfig::default();
    compaction.threshold = COMPACTION_THRESHOLD;
    let input = AgentInput {
        provider: model.provider,
        model_id: model.model_id,
        model_key: std::env::var("SQWAI_BENCH_MODEL").unwrap_or_default(),
        effort: None,
        effort_support: model.effort_support,
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, task_prompt(task, baseline))],
        root: root.clone(),
        session_id: session_id.clone(),
        blocked_patterns: vec![],
        plan_mode: false,
        context_limit: model.context_limit,
        enable_tools: true,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        compact_only: false,
        diary: Default::default(),
        memory: Default::default(),
        compaction,
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: Vec::<FallbackCandidate>::new(),
    };

    // system block: stable prefix like the app builds it (minus TUI state)
    let mut input = input;
    input.system = vec![crate::providers::SystemPart::cached(
        crate::prompts::stable_prefix(),
    )];

    let mut handle = crate::agent::loop_task::spawn_agent(input);
    let mut last_tool_at: Option<Instant> = None;
    let mut final_text = String::new();
    let deadline = tokio::time::Instant::now() + WALL_CAP;
    loop {
        let ev = match tokio::time::timeout_at(deadline, handle.rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                report.error = Some("agent channel disconnected".into());
                break;
            }
            Err(_) => {
                report.timed_out = true;
                handle.abort();
                break;
            }
        };
        match ev {
            AgentEvent::ToolNotice { .. } => {
                report.tool_calls += 1;
                last_tool_at = Some(Instant::now());
            }
            AgentEvent::TextDelta(t) => {
                if let Some(t0) = last_tool_at.take() {
                    report.latencies.push(t0.elapsed().as_secs());
                }
                final_text.push_str(&t);
            }
            AgentEvent::Usage(u) => {
                // mirror Session::add_usage: prompt replaces, completion
                // accumulates (output-only second events carry zero input)
                if u.prompt_tokens > 0 {
                    report.prompt_tokens += u.prompt_tokens;
                }
                report.completion_tokens += u.completion_tokens;
                report.cached_tokens += u.cached_tokens.unwrap_or(0);
            }
            AgentEvent::Compaction { .. } => {
                report.compactions += 1;
            }
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => {
                report.error = Some(e);
                break;
            }
            _ => {}
        }
    }
    report.wall_secs = started.elapsed().as_secs();
    report.final_text = final_text;
    let low = report.final_text.to_lowercase();
    report.claimed_done = low.contains("complete")
        || low.contains("done")
        || low.contains("finished")
        || low.contains("ready");
    crate::bench::set_baseline_override(None);

    // mechanism finish from the plan file (acceptance-aware, complete-call-free)
    if !baseline {
        report.plan_finished = plan_finished(&root);
    }
    report.root = root;
    Some(report)
}

/// Mechanism finish (§8.2, pre-registered): every step closed AND every
/// acceptance item verified or waived — the `complete` call itself is not
/// required.
fn plan_finished(root: &Path) -> bool {    let Ok(Some(plan)) = crate::plan::open_active(root) else {
        return false;
    };
    let steps_closed = plan.steps.iter().all(|s| {
        matches!(
            s.status,
            crate::plan::StepStatus::Done | crate::plan::StepStatus::Cancelled
        )
    });
    let acceptance_ok = plan.acceptance.iter().all(|a| {
        matches!(
            a.validation.status,
            crate::plan::ValidationStatus::Passed | crate::plan::ValidationStatus::Waived
        ) || crate::plan::legacy_passed(a)
    });
    steps_closed && acceptance_ok
}

#[derive(Debug, Default)]
pub struct Score {
    /// acceptance commands all exited 0 in the fixture copy
    pub acceptance_green: bool,
    /// trap files untouched (btree.rs hash) and no `tmp:` lines in the log
    pub traps_ok: bool,
    /// file_diff records per path (churn signal for redundant work)
    pub diffs_per_path: Vec<(String, usize)>,
    /// goal fidelity + constraint retention stay HUMAN-scored (0/0.5/1):
    /// the report carries the final anchor and answer for the judge
    pub anchor: String,
    pub final_answer_tail: String,
}

/// Score a finished run. Acceptance + traps are automatic; goal fidelity
/// and constraint retention are judged by a human from the report.
pub fn score_run(report: &RunReport, task: &TaskSpec, fixture_src: &Path) -> Score {
    let mut score = Score::default();
    score.acceptance_green = task.acceptance_cmds.iter().all(|cmd| {
        std::process::Command::new("cmd")
            .args(["/C", cmd])
            .current_dir(&report.root)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    });
    let trap_same = |rel: &str| -> bool {
        let a = std::fs::read(fixture_src.join(rel)).unwrap_or_default();
        let b = std::fs::read(report.root.join(rel)).unwrap_or_default();
        a == b
    };
    let log = std::fs::read_to_string(report.root.join("minidb.log")).unwrap_or_default();
    let tmp_leaked = log.lines().any(|line| line.starts_with("tmp:"));
    score.traps_ok = trap_same("src/storage/btree.rs") && !tmp_leaked;

    let journal_dir = report.root.join(".sqwai").join("journal");
    let mut per_path: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(&journal_dir) {
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines() {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if record.get("kind").and_then(|k| k.as_str()) == Some("file_diff") {
                    if let Some(path) = record
                        .get("fields")
                        .and_then(|f| f.get("path"))
                        .and_then(|p| p.as_str())
                    {
                        *per_path.entry(path.to_string()).or_default() += 1;
                    }
                }
            }
        }
    }
    score.diffs_per_path = per_path.into_iter().collect();

    score.anchor = crate::agent::context::anchor(&report.root, &report.session_id);
    score
}

pub fn print_report(report: &RunReport, score: &Score) {
    let tail: String =
        report.final_text.chars().rev().take(2000).collect::<String>().chars().rev().collect();
    println!("=== bench {} {} ===", report.task, report.arm);
    println!("wall: {}s  tools: {}  compactions: {}", report.wall_secs, report.tool_calls, report.compactions);
    println!(
        "tokens: in={} out={} cached={}",
        report.prompt_tokens, report.completion_tokens, report.cached_tokens
    );
    println!(
        "finish: plan={} claimed={} timeout={} error={:?}",
        report.plan_finished, report.claimed_done, report.timed_out, report.error
    );
    println!("acceptance_green: {}  traps_ok: {}", score.acceptance_green, score.traps_ok);
    println!("diffs_per_path: {:?}", score.diffs_per_path);
    if !report.latencies.is_empty() {
        let max = report.latencies.iter().max().unwrap_or(&0);
        let sum: u64 = report.latencies.iter().sum();
        println!(
            "latency tool->text: n={} avg={}s max={}s",
            report.latencies.len(),
            sum / report.latencies.len() as u64,
            max
        );
    }
    println!("--- anchor ---\n{}", score.anchor);
    println!("--- final answer tail ---\n{tail}");
    println!("HUMAN: goal fidelity (0/0.5/1) ___  constraint retention (0..1) ___");
}

/// Shakedown (§8.2): one task, mechanism arm, threshold 0.04. Go/no-go:
/// compactions ≥ 3, finish detected, latency not strangled. Live: needs
/// keys (skipped without them) and costs a real run.
#[tokio::test]
#[ignore]
async fn bench_t1_shakedown_mechanism() {
    let Some(report) = run_arm(&T1, false, "shakedown").await else {
        eprintln!("SKIP: no bench provider (config/model)");
        return;
    };
    let fixture_src = fixture_source();
    let score = score_run(&report, &T1, &fixture_src);
    print_report(&report, &score);
    assert!(
        report.compactions >= 3,
        "shakedown needs ≥3 compactions, got {}",
        report.compactions
    );
    assert!(report.plan_finished, "mechanism must finish T1: {report:?}");
    assert!(score.acceptance_green, "acceptance must be green");
}

/// Same, baseline arm: must compact just as often (methodology symmetry),
/// finish by claim + independent check.
#[tokio::test]
#[ignore]
async fn bench_t1_shakedown_baseline() {
    let Some(report) = run_arm(&T1, true, "shakedown").await else {
        eprintln!("SKIP: no bench provider (config/model)");
        return;
    };
    let fixture_src = fixture_source();
    let score = score_run(&report, &T1, &fixture_src);
    print_report(&report, &score);
    assert!(
        report.compactions >= 3,
        "baseline must compact just as often, got {}",
        report.compactions
    );
    assert!(score.acceptance_green, "acceptance must be green");
}

#[test]
fn score_run_reads_traps_and_diff_chains() {
    // synthetic run root: trap file changed, tmp: leaked, two file_diffs
    let root = std::env::temp_dir().join(format!(
        "sqwai-bench-score-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src/storage")).unwrap();
    std::fs::create_dir_all(root.join(".sqwai/journal")).unwrap();
    std::fs::write(root.join("src/storage/btree.rs"), "touched").unwrap();
    std::fs::write(root.join("minidb.log"), "tmp:sneaky\t1\n").unwrap();
    std::fs::write(
        root.join(".sqwai/journal/sess.jsonl"),
        "{\"seq\":1,\"kind\":\"file_diff\",\"fields\":{\"path\":\"src/a.rs\"}}\n\
         {\"seq\":2,\"kind\":\"file_diff\",\"fields\":{\"path\":\"src/a.rs\"}}\n\
         {\"seq\":3,\"kind\":\"file_diff\",\"fields\":{\"path\":\"src/b.rs\"}}\n",
    )
    .unwrap();

    let src = std::env::temp_dir().join(format!(
        "sqwai-bench-src-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(src.join("src/storage")).unwrap();
    std::fs::write(src.join("src/storage/btree.rs"), "pristine").unwrap();

    let task = TaskSpec {
        id: "TX",
        goal: "x",
        constraints: &[],
        acceptance_cmds: &["cmd /C exit 0"],
    };
    let mut report = RunReport::default();
    report.root = root.clone();
    let score = score_run(&report, &task, &src);
    assert!(score.acceptance_green);
    assert!(!score.traps_ok, "touched trap + tmp leak must fail");
    assert_eq!(
        score.diffs_per_path,
        vec![
            ("src/a.rs".to_string(), 2),
            ("src/b.rs".to_string(), 1)
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&src);
}

