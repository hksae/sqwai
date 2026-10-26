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
    // strings are read-only writers of nothing
    for task in subagent_tasks_from_args(&serde_json::json!({"tasks":["one"]})).unwrap() {
        assert!(!task.write);
        assert!(task.paths.is_empty());
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
fn subagent_writer_scope_rules() {
    // write without paths refuses: a scope must be declared
    let error = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [{"task": "fix it", "write": true}]
    }))
    .unwrap_err();
    assert!(error.contains("needs paths"), "{error}");

    // overlapping writer scopes refuse, naming the conflict
    let error = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [
            {"task": "fix a", "write": true, "paths": ["src/a"]},
            {"task": "fix b", "write": true, "paths": ["src/a/b.rs"]},
        ]
    }))
    .unwrap_err();
    assert!(error.contains("overlap"), "{error}");

    // disjoint writers pass with normalized scopes
    let tasks = subagent_tasks_from_args(&serde_json::json!({
        "tasks": [
            {"task": "fix a", "write": true, "paths": ["src/a/", "./src/b"]},
            {"task": "read all"},
        ]
    }))
    .unwrap();
    assert!(tasks[0].write);
    assert_eq!(tasks[0].paths, vec!["src/a", "src/b"]);
    assert!(!tasks[1].write);
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
        "tool_call",
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

    let explorer_tail = build_subagent_tail(root, "parent-s1", &tasks[0], true);
    assert!(explorer_tail.contains("Role: READ-ONLY EXPLORER SUBAGENT"));
    assert!(explorer_tail.contains("Plan goal: Refactor parser module"));
    assert!(explorer_tail.contains("Active step [1]: Extract lexer tokens"));
    assert!(explorer_tail.contains("Parent modified files: src/lexer.rs"));
    assert!(explorer_tail.contains("Parent recently read files: src/parser.rs"));

    let writer_tail = build_subagent_tail(root, "parent-s1", &tasks[1], false);
    assert!(writer_tail.contains("Role: SCOPED WRITER SUBAGENT"));
    assert!(writer_tail.contains("Allowed write scope: [src/lexer.rs]"));

    // Clean-context reviewer mode (`clean: true`) omits `<parent_context>`
    let clean_tasks = subagent_tasks_from_args(&serde_json::json!({
        "clean": true,
        "tasks": ["Review the current git diff for logic bugs"]
    }))
    .unwrap();
    assert!(clean_tasks[0].clean);
    let clean_tail = build_subagent_tail(root, "parent-s1", &clean_tasks[0], true);
    assert!(clean_tail.contains("Role: READ-ONLY EXPLORER SUBAGENT"));
    assert!(
        !clean_tail.contains("<parent_context>"),
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

    let attached = trail.attach_receipt("All callers updated.", true);
    assert!(attached.contains("[Subagent receipt: ok | 2 tool call(s): grep(parse_expr), read(src/parser.rs) | modified: src/lexer.rs]"));

    let salvaged = trail.salvage_on_error(
        "subagent timed out after 600s",
        "Found 3 callers in src/ast.rs:42",
    );
    assert!(salvaged.contains("subagent timed out after 600s"));
    assert!(salvaged.contains("[Subagent receipt: stopped | 2 tool call(s)"));
    assert!(salvaged.contains("Partial findings before stop:\nFound 3 callers in src/ast.rs:42"));
}
