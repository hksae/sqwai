use super::loop_subagent::{
    MAX_PARALLEL_SUBAGENTS, SubagentTask, SubagentTrail, build_subagent_tail,
    build_subagent_user_prompt, next_subagent_session, subagent_tasks_from_args,
};

/// #190: every child session id is distinct, even back-to-back —
/// events files must never be shared between runs.
#[test]
fn subagent_session_ids_are_unique() {
    let a = next_subagent_session();
    let b = next_subagent_session();
    assert_ne!(a, b, "counter must disambiguate same-ms spawns");
    assert!(a.starts_with("sub-"), "{a}");
    assert!(
        !a.contains('/') && !a.contains('\\') && !a.contains(".."),
        "events-file safe: {a}"
    );
}

#[test]
fn accepts_one_or_many_subagent_tasks() {
    let labels = |tasks: Vec<SubagentTask>| tasks.into_iter().map(|t| t.label).collect::<Vec<_>>();
    assert_eq!(
        labels(subagent_tasks_from_args(&serde_json::json!({"tasks":["one","two"]})).unwrap()),
        vec!["one", "two"]
    );
    // the legacy singular form is refused with a hint, not guessed
    assert!(
        subagent_tasks_from_args(&serde_json::json!({"task": "inspect"}))
            .unwrap_err()
            .contains("tasks")
    );
    for task in subagent_tasks_from_args(&serde_json::json!({"tasks":["one"]})).unwrap() {
        assert!(task.paths.is_empty(), "no focus paths were given");
    }
}

/// Regression: the model sent three task OBJECTS and the whole batch
/// died — objects carry the text
/// under task|prompt|description, a lone string is one task.
#[test]
fn accepts_object_shaped_subagent_tasks() {
    let labels = |tasks: Vec<SubagentTask>| tasks.into_iter().map(|t| t.label).collect::<Vec<_>>();
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
        labels(subagent_tasks_from_args(&serde_json::json!({"tasks": "do it all"})).unwrap()),
        vec!["do it all"]
    );
    // empties still refuse with the tasks-only message intact
    let error = subagent_tasks_from_args(&serde_json::json!({"tasks": [{}, "  "]})).unwrap_err();
    assert!(error.contains("subagent tasks are required"), "{error}");
}

#[test]
fn paths_are_focus_not_a_write_scope() {
    // `write` is gone: a task with or without it parses the same, and paths
    // only name where the child should start.
    let tasks = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [
            {"task": "fix a", "write": true, "paths": ["src/a/", "./src/b"]},
            {"task": "read all"},
            {"task": "no paths", "write": true}
        ]
    }))
    .unwrap();
    assert_eq!(tasks[0].paths, vec!["src/a", "src/b"]);
    assert!(tasks[1].paths.is_empty());
    assert!(tasks[2].paths.is_empty(), "no scope is required anymore");
}

#[test]
fn a_task_timeout_is_parsed() {
    let tasks = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [
            {"task": "bounded", "timeout_secs": 5},
            {"task": "default"}
        ]
    }))
    .unwrap();
    assert_eq!(tasks[0].timeout_secs, Some(5));
    assert_eq!(tasks[1].timeout_secs, None);
}

#[test]
fn rejects_more_than_eight_subagents() {
    let tasks: Vec<String> = (0..9).map(|n| format!("task {n}")).collect();
    let error = subagent_tasks_from_args(&serde_json::json!({"tasks":tasks})).unwrap_err();
    assert!(error.contains("maximum is 8"));
    assert_eq!(MAX_PARALLEL_SUBAGENTS, 4);
}

#[test]
fn subagent_contract_parent_context_and_receipt_salvage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Create an active plan and parent events entries so `<parent_context>` is populated.
    let mut plan = crate::plan::create(
        "Refactor parser module".into(),
        vec!["Preserve public AST API".into()],
        vec!["cargo test passes".into()],
        vec![crate::plan::NewStep {
            title: "Extract lexer tokens".into(),
            check: None,
        }],
        &crate::plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["parent-s1".into()];
    crate::plan::store(root, &plan).unwrap();

    let mut j = crate::agent::events::EventLog::open(root, "parent-s1").unwrap();
    j.append(
        "observation",
        serde_json::json!({"tool": "read", "path": "src/parser.rs"}),
    )
    .unwrap();
    j.append(
        "file_diff",
        serde_json::json!({"path": "src/lexer.rs", "diff": "+token"}),
    )
    .unwrap();
    let tasks = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [
            {
                "task": "Find all callers of parse_expr",
                "files": ["./src/parser.rs"],
                "context": "Checking impact before signature change"
            },
            {
                "task": "Implement Token::is_keyword",
                "write": true,
                "paths": ["src/lexer.rs"]
            }
        ]
    }))
    .unwrap();

    let explorer_prompt = build_subagent_user_prompt(&tasks[0]);
    assert!(explorer_prompt.contains("Target files/paths: src/parser.rs"));
    assert!(explorer_prompt.contains("Checking impact before signature change"));

    let explorer_tail = build_subagent_tail(root, "parent-s1", &tasks[0]);
    assert!(explorer_tail.contains("<subagent_role>"));
    assert!(explorer_tail.contains("Your plan is yours"));
    assert!(explorer_tail.contains("never modify the parent's plan"));
    assert!(explorer_tail.contains("Plan goal: Refactor parser module"));
    assert!(explorer_tail.contains("Active step [1]: Extract lexer tokens"));
    assert!(explorer_tail.contains("Parent modified files: src/lexer.rs"));
    assert!(explorer_tail.contains("Parent recently read files: src/parser.rs"));

    let writer_tail = build_subagent_tail(root, "parent-s1", &tasks[1]);
    assert!(writer_tail.contains("<subagent_role>"));
    assert!(
        !writer_tail.contains("Allowed write scope"),
        "the child role no longer carries a host write scope"
    );

    // Clean-context reviewer mode (`clean: true`) omits `<parent_context>`
    let clean_tasks = subagent_tasks_from_args(&serde_json::json!({
        "clean": true,
        "tasks": ["Review the current git diff for logic bugs"]
    }))
    .unwrap();
    assert!(clean_tasks[0].clean);
    let clean_tail = build_subagent_tail(root, "parent-s1", &clean_tasks[0]);
    assert!(clean_tail.contains("<subagent_role>"));
    assert!(
        !clean_tail.contains("Parent context (read-only"),
        "clean:true subagent must not receive parent history: {clean_tail}"
    );

    // Failure-loop intervention fires every 3 consecutive edit -> bash failure cycles
    assert!(super::failure_loop_nudge(1).is_none());
    assert!(super::failure_loop_nudge(2).is_none());
    assert!(super::failure_loop_nudge(3).is_some());
    assert!(super::failure_loop_nudge(4).is_none());
    assert!(super::failure_loop_nudge(6).is_some());

    // Verify SubagentTrail receipt and partial-work salvage on timeout/error.
    let mut trail = SubagentTrail::default();
    trail.record_start("grep", "parse_expr");
    trail.record_done("grep", true);
    trail.record_start("read", "src/parser.rs");
    trail.record_done("read", true);
    trail.modified_files.push("src/lexer.rs".into());

    let envelope = trail.envelope(
        3,
        "completed",
        std::time::Duration::from_secs(84),
        true,
        "All callers updated.",
    );
    assert!(
        envelope.starts_with("[subagent 3 | completed | 1m24s | 2 tool(s) | files: src/lexer.rs | plan: abandoned]"),
        "{envelope}"
    );
    assert!(envelope.ends_with("All callers updated."), "{envelope}");

    let salvaged = trail.salvage_on_error(
        "subagent timed out after 600s",
        "Found 3 callers in src/ast.rs:42",
    );
    assert!(salvaged.contains("subagent timed out after 600s"));
    assert!(!salvaged.contains("receipt"), "{salvaged}");
    assert!(salvaged.contains("Partial findings before stop:\nFound 3 callers in src/ast.rs:42"));
}
