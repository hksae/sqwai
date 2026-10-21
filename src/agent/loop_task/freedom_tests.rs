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
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        vision: true,
        pdf: true,
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        skills: Default::default(),
        compact_only: false, compact_focus: String::new(),
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
