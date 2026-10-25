use super::effort_tests::MockTestProvider;
use super::*;

/// The campaign's central behavioral claim: the plan-first gate is gone,
/// so a file mutation in a session with no plan at all just runs. Nothing
/// asks for a plan, nothing nudges about one, and the refusal codes that
/// used to guard this (`plan_required`, missing acceptance) have no path
/// left to fire from.
#[tokio::test]
async fn a_mutation_runs_without_any_plan() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::ToolCall(
                crate::providers::ToolCallReq::new(
                    "c1",
                    "write",
                    serde_json::json!({
                        "file_path": "feature.rs",
                        "content": "pub fn hello() {}"
                    }),
                ),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });

    let temp_dir = std::env::temp_dir().join(format!("sqwai-noplan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "add a small helper module")],
        root: temp_dir.clone(),
        session_id: "no-plan-sess".into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: crate::config::PlanConfig::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    let mut saw_write = false;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolNotice { name, ok, .. } => {
                assert_eq!(name, "write");
                assert!(ok, "no plan is required to write");
                saw_write = true;
            }
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    assert!(saw_write, "the write must have run");
    assert!(
        temp_dir.join("feature.rs").exists(),
        "the mutation must land on disk"
    );
    // and the journal holds no gate refusal behind it
    let refused: Vec<String> =
        crate::agent::journal::Journal::records_for(&temp_dir, "no-plan-sess")
            .unwrap_or_default()
            .iter()
            .filter(|r| r.kind == "tool_result")
            .filter(|r| r.fields.get("ok") == Some(&serde_json::Value::Bool(false)))
            .map(|r| {
                r.fields
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
    assert!(refused.is_empty(), "nothing refused the write: {refused:?}");
    assert!(
        crate::plan::open_active(&temp_dir).unwrap().is_none(),
        "no plan was invented to justify the write"
    );
    std::fs::remove_dir_all(&temp_dir).ok();
}

fn limited_input(
    provider: SharedProvider,
    root: std::path::PathBuf,
    limits: crate::config::LimitsConfig,
) -> AgentInput {
    AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "do the work")],
        root,
        session_id: "limit-sess".into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        limits,
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: crate::config::PlanConfig::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    }
}

#[tokio::test]
async fn tool_call_limit_refuses_the_rest_of_the_batch() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "ls",
                        serde_json::json!({"path": "."}),
                    ),
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c2",
                        "ls",
                        serde_json::json!({"path": "."}),
                    ),
                )),
            ],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });
    let temp_dir = std::env::temp_dir().join(format!("sqwai-call-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let limits = crate::config::LimitsConfig {
        max_tool_calls_per_turn: 1,
        ..Default::default()
    };
    let mut handle = spawn_agent(limited_input(provider, temp_dir.clone(), limits));
    let mut notices = 0usize;
    let mut outcome = None;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolNotice { .. } => notices += 1,
            AgentEvent::Completed(Ok(done)) => {
                outcome = Some(done);
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    assert_eq!(notices, 1, "only the allowed call may run");
    let messages = outcome.expect("turn must complete").messages;
    assert!(
        messages
            .iter()
            .any(|m| { m.role == Role::Tool && m.content.contains("tool-call limit reached") }),
        "the refused call must carry a result: {messages:?}"
    );
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn active_skills_make_a_reload_return_a_pointer() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::ToolCall(
                crate::providers::ToolCallReq::new(
                    "c1",
                    "skill",
                    serde_json::json!({"name": "tui-rules"}),
                ),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });
    let temp_dir = std::env::temp_dir().join(format!("sqwai-skill-seed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(temp_dir.join(".sqwai/skills/tui-rules")).unwrap();
    std::fs::write(
        temp_dir.join(".sqwai/skills/tui-rules/SKILL.md"),
        "---\nname: tui-rules\ndescription: TUI layout\n---\nKeep rows within width.\n",
    )
    .unwrap();
    let mut input = limited_input(provider, temp_dir.clone(), Default::default());
    input.active_skills = vec!["tui-rules".into()];
    let mut handle = spawn_agent(input);
    let mut summary = None;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolNotice {
                name, summary: s, ..
            } if name == "skill" => {
                summary = Some(s);
            }
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    let summary = summary.expect("skill tool row");
    assert!(
        summary.contains("already loaded"),
        "an active skill must answer with a pointer, got: {summary}"
    );
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[tokio::test]
async fn token_limit_stops_before_dispatching_the_tool_batch() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![
                Ok(crate::providers::StreamEvent::Usage(
                    crate::providers::Usage {
                        prompt_tokens: 200,
                        completion_tokens: 10,
                        ..Default::default()
                    },
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "ls",
                        serde_json::json!({"path": "."}),
                    ),
                )),
            ],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });
    let temp_dir = std::env::temp_dir().join(format!("sqwai-token-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let limits = crate::config::LimitsConfig {
        max_tokens_per_turn: 100,
        ..Default::default()
    };
    let mut handle = spawn_agent(limited_input(provider, temp_dir.clone(), limits));
    let mut notices = 0usize;
    let mut outcome = None;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolNotice { .. } => notices += 1,
            AgentEvent::Completed(Ok(done)) => {
                outcome = Some(done);
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    assert_eq!(
        notices, 0,
        "no tool may run after the token budget is spent"
    );
    let messages = outcome.expect("turn must complete").messages;
    assert!(
        messages
            .iter()
            .any(|m| { m.role == Role::Tool && m.content.contains("turn token limit reached") }),
        "the skipped call must carry a result: {messages:?}"
    );
    std::fs::remove_dir_all(&temp_dir).ok();
}
