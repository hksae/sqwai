use super::loop_subagent::{
    MAX_PARALLEL_SUBAGENTS, SubagentTask, next_subagent_session, subagent_tasks_from_args,
};

/// #190: every child session id is distinct, even back-to-back —
/// journal files must never be shared between runs.
#[test]
fn subagent_session_ids_are_unique() {
    let a = next_subagent_session();
    let b = next_subagent_session();
    assert_ne!(a, b, "counter must disambiguate same-ms spawns");
    assert!(a.starts_with("sub-"), "{a}");
    assert!(
        !a.contains('/') && !a.contains('\\') && !a.contains(".."),
        "journal-file safe: {a}"
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
