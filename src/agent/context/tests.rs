use super::*;

fn user(s: &str) -> Message {
    Message::new(Role::User, s)
}

fn temp_root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("sqwai-context-{label}-{}", std::process::id()))
}

#[test]
fn image_tokens_count_toward_estimated_and_wire_pressure() {
    let image = crate::media::test_image(1280, 720, "shot.png");
    let plain = Message::tool_result("c1", "view_image", false);
    let with_image = plain.clone().with_images(vec![image]);
    let expected = crate::media::pressure_tokens(1280, 720);
    let stored = estimated_tokens(std::slice::from_ref(&with_image))
        - estimated_tokens(std::slice::from_ref(&plain));
    assert_eq!(stored, expected);
    let wire = wire_estimated_tokens(&[with_image], &crate::agent::tool_output::Wire::default()) - wire_estimated_tokens(&[plain], &crate::agent::tool_output::Wire::default());
    assert_eq!(wire, expected);
}

#[test]
fn document_pages_count_toward_estimated_and_wire_pressure() {
    let document = crate::media::test_document(3, "report.pdf");
    let plain = Message::tool_result("c1", "view_document", false);
    let with_document = plain.clone().with_documents(vec![document]);
    let expected = crate::pdf::document_tokens(3);
    let stored = estimated_tokens(std::slice::from_ref(&with_document))
        - estimated_tokens(std::slice::from_ref(&plain));
    assert_eq!(stored, expected);
    let wire = wire_estimated_tokens(&[with_document], &crate::agent::tool_output::Wire::default()) - wire_estimated_tokens(&[plain], &crate::agent::tool_output::Wire::default());
    assert_eq!(wire, expected);
}

#[test]
fn anchor_keeps_event_facts_not_plan_state() {
    let root = temp_root("anchor");
    let mut plan = crate::plan::create(
        "ship the anchor".into(),
        vec!["keep the goal host-owned".into()],
        vec!["cmd: cargo test".into()],
        vec![crate::plan::NewStep {
            title: "implement anchor".into(),
            check: None,
        }],
        &crate::plan::Limits::default(),
    )
    .unwrap();
    // prod plans always carry their session (plan_op sets it on
    // create); without membership #171 strictness resolves nothing
    plan.sessions = vec!["anchor-session".to_string()];
    crate::plan::store(&root, &plan).unwrap();

    let mut events = crate::agent::events::EventLog::open(&root, "anchor-session").unwrap();
    events.set_attribution(Some("1".into()), Some(plan.id.clone()), "main");
    events
        .append("file_diff", serde_json::json!({"path": "src/main.rs"}))
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({"tool": "bash", "ok": true}),
        )
        .unwrap();

    let rendered = anchor(&root, "anchor-session");
    // plan state rides live per request, never in the anchor: a
    // turn-start snapshot would resurrect the two-plan problem
    assert!(!rendered.contains("goal: ship the anchor"), "{rendered}");
    assert!(!rendered.contains("keep the goal host-owned"), "{rendered}");
    assert!(!rendered.contains("implement anchor"), "{rendered}");
    // events facts stay
    assert!(rendered.contains("files changed this session: src/main.rs"));
    assert!(rendered.contains("last verification: successful exec j#2"));
    std::fs::remove_dir_all(root).ok();
}

/// An `/undo` takes the claim back. The anchor is a memo about the tree, not
/// a ledger of everything the session ever touched — and it is read exactly
/// when the model is re-grounded, so a stale "files changed" line is the
/// worst possible place to keep claiming reverted bytes.
#[test]
fn anchor_drops_files_that_a_later_undo_reverted() {
    let root = temp_root("anchor-undo");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let mut events = crate::agent::events::EventLog::open(&root, "s").unwrap();
    for path in ["src/a.rs", "src/b.rs"] {
        events
            .append("file_diff", serde_json::json!({"path": path}))
            .unwrap();
    }
    events
        .append_undo("deadbeef", &["src/a.rs".to_string()], &[])
        .unwrap();

    let rendered = anchor(&root, "s");
    assert!(
        rendered.contains("files changed this session: src/b.rs"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("src/a.rs"),
        "the reverted file must drop out: {rendered}"
    );

    // writing it again after the revert counts, exactly like the live tree
    events
        .append("file_diff", serde_json::json!({"path": "src/a.rs"}))
        .unwrap();
    let rendered = anchor(&root, "s");
    assert!(
        rendered.contains("files changed this session: src/a.rs · src/b.rs"),
        "{rendered}"
    );
    std::fs::remove_dir_all(root).ok();
}

/// Decisions, lessons and recent failures survive in the anchor: after a
/// compaction "why this way" and "what broke" must come from host state,
/// not from model memory. Cancelled runs are the user's doing, not
/// failures, and stay out.
#[test]
fn anchor_surfaces_decisions_and_recent_failures() {
    let root = temp_root("anchor-decisions");
    let mut events = crate::agent::events::EventLog::open(&root, "s").unwrap();
    events
        .append(
            "note",
            serde_json::json!({"note": "decision", "text": "chose btree over lsm"}),
        )
        .unwrap();
    events
        .append(
            "note",
            serde_json::json!({"note": "lesson", "text": "flush before read"}),
        )
        .unwrap();
    events
        .append(
            "note",
            serde_json::json!({"note": "assumption", "text": "disk is fast"}),
        )
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({"tool": "bash", "ok": false, "summary": "boom: segfault"}),
        )
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({"tool": "bash", "ok": false, "code": "cancelled"}),
        )
        .unwrap();

    let rendered = anchor(&root, "s");
    assert!(rendered.contains("chose btree over lsm"), "{rendered}");
    assert!(rendered.contains("flush before read"), "{rendered}");
    assert!(rendered.contains("disk is fast"), "{rendered}");
    assert!(rendered.contains("boom: segfault"), "{rendered}");
    assert!(
        !rendered.contains("cancelled"),
        "user-cancelled runs are not failures: {rendered}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn anchor_carries_no_plan_goals_for_any_session() {
    let root = temp_root("anchor-session-scope");
    let limits = crate::plan::Limits::default();
    let mut plan_a = crate::plan::create(
        "session A goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![crate::plan::NewStep {
            title: "step A".into(),
            check: None,
        }],
        &limits,
    )
    .unwrap();
    plan_a.created = "2026-01-01T00:00:00+00:00".to_string();
    plan_a.sessions = vec!["session-a".to_string()];
    crate::plan::store(&root, &plan_a).unwrap();

    let mut plan_b = crate::plan::create(
        "session B goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![crate::plan::NewStep {
            title: "step B".into(),
            check: None,
        }],
        &limits,
    )
    .unwrap();
    plan_b.created = "2026-09-09T00:00:00+00:00".to_string();
    plan_b.sessions = vec!["session-b".to_string()];
    crate::plan::store(&root, &plan_b).unwrap();

    // plans ride live per request now: no session's anchor renders
    // any goal, so scopes cannot leak or go stale mid-turn
    let rendered_a = anchor(&root, "session-a");
    assert!(!rendered_a.contains("goal:"), "{rendered_a}");
    assert!(!rendered_a.contains("session B goal"));

    let rendered_b = anchor(&root, "session-b");
    assert!(!rendered_b.contains("goal:"), "{rendered_b}");
    assert!(!rendered_b.contains("session A goal"));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn resume_notice_reports_unfinished_plan_step() {
    let root = temp_root("resume");
    let plan = crate::plan::create(
        "resume safely".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![crate::plan::NewStep {
            title: "unfinished change".into(),
            check: None,
        }],
        &crate::plan::Limits::default(),
    )
    .unwrap();
    crate::plan::store(&root, &plan).unwrap();
    let mut events = crate::agent::events::EventLog::open(&root, "resume-session").unwrap();
    events.set_attribution(Some("1".into()), Some(plan.id), "main");
    events
        .append("plan", serde_json::json!({"op": "create"}))
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({"tool": "read", "ok": true}),
        )
        .unwrap();
    let notice = resume_notice(&root, "resume-session").unwrap();
    assert!(notice.contains("Step 1 was in progress"));
    assert!(notice.contains("j#2 observation"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn completed_plan_step_has_no_resume_notice() {
    let root = temp_root("resume-closed");
    let mut events = crate::agent::events::EventLog::open(&root, "closed-session").unwrap();
    events
        .append("plan", serde_json::json!({"op": "start", "id": "1"}))
        .unwrap();
    events
        .append("plan", serde_json::json!({"op": "finish", "id": "1"}))
        .unwrap();
    assert!(resume_notice(&root, "closed-session").is_none());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn anchor_degrades_without_plan_or_events() {
    let root = temp_root("empty");
    let rendered = anchor(&root, "missing-session");
    assert!(rendered.contains("ANCHOR"), "{rendered}");
    assert!(rendered.contains("files changed this session: none"));
}

#[test]
fn anchor_ignores_completed_plans() {
    let root = temp_root("done-plan");
    std::fs::create_dir_all(root.join(".sqwai/plans")).unwrap();
    std::fs::write(
        root.join(".sqwai/plans/p.json"),
        r#"{"version":1,"id":"p","status":"completed","created":"t",
            "sessions":["s1"],
            "goal":{"text":"ship it","source":"user","created":"t"},
            "constraints":["keep it green"],
            "budget":{"tokens":0,"limit":0},"revision":3,
            "steps":[{"id":"1","title":"a","status":"done"}],
            "acceptance":[{"text":"cmd: true","status":"pending",
                           "validation":{"status":"passed"}}]}"#,
    )
    .unwrap();
    let rendered = anchor(&root, "s1");
    // completed plans surface nowhere in the anchor: done work stays
    // visible through the event log (file_diff records), not a plan echo
    assert!(!rendered.contains("ship it"), "{rendered}");
    assert!(!rendered.contains("keep it green"), "{rendered}");
    assert!(!rendered.contains("plan: none"), "{rendered}");
    // a foreign session still sees nothing of it
    assert!(!anchor(&root, "other").contains("ship it"));
    std::fs::remove_dir_all(root).ok();
}

/// The anchor describes what the host ledger proves and gives no orders.
#[test]
fn anchor_states_facts_and_issues_no_commands() {
    let root = temp_root("facts");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut events = crate::agent::events::EventLog::open(&root, "facts-session").unwrap();
    events
        .append("file_diff", serde_json::json!({"path": "src/x.rs"}))
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({"tool": "bash", "ok": true}),
        )
        .unwrap();
    let rendered = anchor(&root, "facts-session");
    assert!(rendered.contains("src/x.rs"), "{rendered}");
    for command in ["call ", "Do not", "must "] {
        assert!(
            !rendered.contains(command),
            "imperative {command:?} left in the anchor: {rendered}"
        );
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn keeps_recent_tail_and_summarizes_old_messages() {
    let messages: Vec<_> = (0..12).map(|i| user(&format!("message {i}"))).collect();
    let result = compact(&messages, None);
    assert!(result.compacted);
    assert_eq!(result.messages.len(), RECENT_MESSAGE_COUNT);
    assert!(result.summary.contains("message 0"));
    assert!(!result.summary.contains("message 11"));
}

#[test]
fn truncates_large_tool_results() {
    let messages = vec![Message::tool_result("call", "x".repeat(20_000), false)];
    let result = compact(&messages, None);
    assert!(result.messages[0].content.len() < 13_000);
    assert!(result.messages[0].content.contains("compacted"));
}

#[test]
fn carries_previous_summary_only_once() {
    let messages: Vec<_> = (0..12).map(|i| user(&format!("message {i}"))).collect();
    let result = compact(&messages, Some("old facts"));
    assert_eq!(result.summary.matches("old facts").count(), 1);
}

#[test]
fn reserve_is_clamped_and_budget_follows() {
    // small context: reserve floors at RESERVE_MIN
    let small = Policy::new(4_000);
    assert_eq!(small.reserve(), RESERVE_MIN.min(4_000));
    assert_eq!(small.budget(), 0);

    let huge = Policy::new(1_000_000);
    assert_eq!(huge.reserve(), RESERVE_MAX);
    assert_eq!(huge.budget(), 1_000_000 - RESERVE_MAX);

    let mid = Policy::new(100_000);
    assert_eq!(mid.reserve(), 20_000);
    assert_eq!(mid.pressure(80_000), Pressure::Ok);
    assert_eq!(mid.pressure(80_001), Pressure::Summarize);
    // unknown limit: never compact on a guess
    assert_eq!(Policy::new(0).pressure(10_000_000), Pressure::Ok);
}

#[test]
fn configured_policy_controls_reserve_and_tail() {
    let policy = Policy::with_compaction(100_000, 0.08, 4, 0.80);
    assert_eq!(policy.reserve(), 8_000);
    assert_eq!(policy.keep_turns(), 4);
    // threshold bites first here: 80% of 100k beats 100k − 8k reserve
    assert_eq!(policy.budget(), 80_000);

    let messages: Vec<_> = (0..8).map(|i| user(&format!("message {i}"))).collect();
    let (older, keep) = split_for_summary_with_keep(&messages, policy.keep_turns());
    assert_eq!(older.len(), 4);
    assert_eq!(keep.len(), 4);
}

#[test]
fn keep_turns_counts_user_turns_not_messages() {
    // one user turn with tool traffic is still one turn: all of it stays
    let messages = vec![
        user("old task"),
        Message::new(Role::Assistant, "working"),
        Message::tool_result("c1", "output", false),
        user("new task"),
        Message::new(Role::Assistant, "on it"),
    ];
    let (older, keep) = split_for_summary_with_keep(&messages, 1);
    assert_eq!(older.len(), 3);
    assert_eq!(keep.len(), 2);
    assert_eq!(keep[0].content, "new task");
}

#[test]
fn reserve_caps_the_threshold_on_small_contexts() {
    // 20k limit: 80% would be 16k, but the 8k minimum reserve wins
    let policy = Policy::with_compaction(20_000, 0.08, 4, 0.80);
    assert_eq!(policy.budget(), 12_000);
    assert_eq!(policy.pressure(12_000), Pressure::Ok);
    assert_eq!(policy.pressure(12_001), Pressure::Summarize);
}

#[test]
fn split_for_summary_adaptive_force_compacts_a_short_conversation() {
    // Two user turns with keep_turns=4 leave nothing to summarize, so a
    // manual `/compact` used to no-op. With force=true the tail shrinks to the
    // last turn and the earlier exchange is summarized.
    let messages = vec![
        user("first request"),
        Message::new(Role::Assistant, "did the first thing"),
        user("second request"),
    ];
    let (older, _keep) = split_for_summary_with_keep(&messages, 4);
    assert!(
        older.is_empty(),
        "configured keep_turns leaves nothing: {older:?}"
    );
    let (older, keep) = split_for_summary_adaptive(&messages, 4, true);
    assert_eq!(older.len(), 2, "the earlier exchange is summarized");
    assert_eq!(keep.len(), 1, "the last turn stays verbatim");
    assert_eq!(keep[0].content, "second request");
    // without force the configured keep_turns is respected
    let (older, _) = split_for_summary_adaptive(&messages, 4, false);
    assert!(older.is_empty());
}

/// The provider is the only true meter of its own window, and the byte guess is
/// not close: a live request of 736,839 bytes came back billed as 209,553
/// prompt tokens while bytes over four read it as 184,209 — under the 204,800
/// gate — so the session rode one more request into an interrupted stream.
#[test]
fn the_gate_reads_the_providers_meter_not_the_byte_guess() {
    let policy = Policy::new(100_000); // 80,000 tokens of history allowed
    assert_eq!(policy.budget(), 80_000);
    let messages = vec![user(&"y".repeat(200_000))];
    let wire = crate::agent::tool_output::Wire {
        context_limit: policy.context_limit,
        bytes_per_token: 4,
        ..Default::default()
    };
    assert_eq!(
        policy.pressure(wire_estimated_tokens(&messages, &wire)),
        Pressure::Ok,
        "200 KB over four is 50k tokens: the guess sees no pressure"
    );
    let metered = crate::agent::tool_output::Wire {
        measured: Some(crate::agent::tool_output::WireAnchor {
            tokens: 90_000,
            bytes: 200_000,
        }),
        ..wire
    };
    assert_eq!(
        policy.pressure(wire_estimated_tokens(&messages, &metered)),
        Pressure::Summarize,
        "the same transcript billed at 90k tokens is over the gate"
    );
}

/// The anchor is a starting point, not a freeze: what the transcript gains after
/// the billed request is priced at the observed ratio, so the gate keeps up with
/// a run that is still growing.
#[test]
fn the_anchored_gate_grows_with_the_transcript() {
    let policy = Policy::new(100_000);
    let anchor = crate::agent::tool_output::WireAnchor {
        tokens: 70_000,
        bytes: 200_000,
    };
    let metered = crate::agent::tool_output::Wire {
        context_limit: policy.context_limit,
        bytes_per_token: 4,
        measured: Some(anchor),
        ..Default::default()
    };
    let at_anchor = vec![user(&"y".repeat(200_000))];
    assert_eq!(wire_estimated_tokens(&at_anchor, &metered), 70_000);
    let mut grown = at_anchor.clone();
    grown.push(user(&"z".repeat(60_000)));
    assert_eq!(
        wire_estimated_tokens(&grown, &metered),
        85_000,
        "60 KB more at four bytes per token is 15k tokens"
    );
    assert_eq!(
        policy.pressure(wire_estimated_tokens(&grown, &metered)),
        Pressure::Summarize
    );
}

#[test]
fn wire_estimated_tokens_counts_cleared_results_as_stubs() {
    // Aged-out tool output is already a stub on the wire; counting raw stored
    // bytes would keep firing compaction long after the request stopped growing.
    let mut messages = Vec::new();
    for i in 0..40 {
        messages.push(Message::tool_result(
            format!("c{i}"),
            "x".repeat(8 * 1024),
            false,
        ));
    }
    let raw = estimated_tokens(&messages);
    let wire = wire_estimated_tokens(&messages, &crate::agent::tool_output::Wire::default());
    assert!(wire < raw, "wire {wire} must be below raw {raw}");
    assert!(
        wire < raw / 4,
        "most of the pool is stubbed on the wire: wire={wire} raw={raw}"
    );
}

#[test]
fn summary_prompt_names_the_kept_tail() {
    let prompt = summary_short_prompt(Some("earlier"), "", None, false, 3);
    assert!(prompt.contains("3 message(s) stay verbatim"), "{prompt}");
    assert!(prompt.contains("do not repeat the kept tail"), "{prompt}");
    // keep_len 0 keeps the old wording
    let plain = summary_short_prompt(Some("earlier"), "", None, false, 0);
    assert!(plain.contains("Do not repeat any of it"), "{plain}");
}

#[test]
fn summary_head_renders_the_user_compaction_focus() {
    let hint = "Goal: fix the bug";
    let input = summary_short_input(
        &[user("do it")],
        None,
        hint,
        Some("keep the parser details"),
        false,
    );
    assert!(input.contains("<user-compaction-focus>"), "{input}");
    assert!(input.contains("keep the parser details"), "{input}");
    assert!(input.contains("Goal: fix the bug"), "{input}");
    assert!(!input.contains("<user-compaction-focus>\nGoal"), "{input}");
}

/// Full fidelity by design (Codex shape): aged-out tool output is
/// never rewritten mid-session. History shrinks only on real
/// pressure, via summary or hard trim — never silently per turn.
/// This test pins the absence of the old prune tiers: even a long
/// history of large tool results passes through untouched.
#[test]
fn old_tool_output_is_never_rewritten() {
    let mut messages: Vec<Message> = Vec::new();
    for i in 0..15 {
        messages.push(user(&format!("turn {i}")));
        messages.push(Message::tool_result(
            format!("call{i}"),
            "y".repeat(6_000),
            false,
        ));
    }
    assert_eq!(messages.len(), 30);
    // no rewrite pass exists anymore: the oldest payload is intact
    assert!(messages[1].content.contains(&"y".repeat(6_000)));
    assert!(!messages[1].content.contains(PRUNE_NOTE));
    assert!(messages[29].content.contains(&"y".repeat(6_000)));
    // pairs stay paired
    for pair in messages.chunks(2) {
        assert_eq!(pair[0].role, Role::User);
        assert_eq!(pair[1].role, Role::Tool);
    }
}

#[test]
fn split_never_orphans_a_tool_call() {
    // a complete tool call (assistant request + its result) followed by
    // several text turns, so a safe cut can land after the result and
    // still leave the recent tail intact
    let messages = vec![
        user("first task"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![
            crate::providers::ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({"file_path": "a.rs"}),
            ),
        ]),
        Message::tool_result("c1", "fn main() {}", false),
        user("second task"),
        Message::new(Role::Assistant, "did second"),
        user("third task"),
        Message::new(Role::Assistant, "did third"),
        user("fourth task"),
        Message::new(Role::Assistant, "did fourth"),
        user("fifth task"),
    ];
    let (older, keep) = split_for_summary_with_keep(&messages, 2);
    assert!(!older.is_empty());
    assert!(keep.first().is_some_and(|m| m.role == Role::User));
    // kept tail is exactly the last two user turns and what follows them
    assert_eq!(keep.first().unwrap().content, "fourth task");
    // the split is a clean partition
    assert_eq!(older.len() + keep.len(), messages.len());
    // a tool call that was summarized keeps its result on the same side;
    // otherwise no assistant tool call may sit at the end of the part
    let call_idx = older
        .iter()
        .position(|m| m.role == Role::Assistant && !m.tool_calls.is_empty());
    if let Some(ci) = call_idx {
        assert!(
            older.get(ci + 1).is_some_and(|m| m.role == Role::Tool),
            "a summarized tool call must keep its result"
        );
    } else {
        assert!(!matches!(
            older.last(),
            Some(m) if m.role == Role::Assistant && !m.tool_calls.is_empty()
        ));
    }
}

#[test]
fn split_of_a_short_history_summarizes_nothing() {
    let messages: Vec<_> = (0..4).map(|i| user(&format!("m{i}"))).collect();
    let (older, keep) = split_for_summary(&messages);
    assert!(older.is_empty());
    assert_eq!(keep.len(), messages.len());
}

#[test]
fn summary_round_trip_starts_with_a_user_turn() {
    let keep = vec![user("latest question")];
    let out = apply_summary("did things", &keep);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].role, Role::User, "assistant-first breaks anthropic");
    assert!(out[0].content.contains("did things"));
    assert_eq!(out[1].content, "latest question");
}

#[test]
fn truncate_summary_caps_with_marker() {
    let short = "did things";
    assert_eq!(truncate_summary(short), short);
    let exact = "x".repeat(SUMMARY_SHORT_MAX_CHARS);
    assert_eq!(truncate_summary(&exact), exact);
    let long = "y".repeat(SUMMARY_SHORT_MAX_CHARS + 500);
    let cut = truncate_summary(&long);
    assert!(cut.chars().count() <= SUMMARY_SHORT_MAX_CHARS);
    assert!(cut.contains("truncated to fit"));
    assert!(cut.starts_with(&"y".repeat(100)));
    // multibyte-safe: no split codepoints, marker included in budget
    let emoji = "🔥".repeat(SUMMARY_SHORT_MAX_CHARS + 10);
    let cut_emoji = truncate_summary(&emoji);
    assert!(cut_emoji.chars().count() <= SUMMARY_SHORT_MAX_CHARS);
    assert!(cut_emoji.contains("truncated to fit"));
}

/// The cache-aware prompt carries no transcript: the conversation
/// travels as the request's own messages above it. Same head, same
/// rules — and an explicit no-tools line, because the parent's schemas
/// travel with that request to hold the prefix.
#[test]
fn summary_short_prompt_points_at_messages_above() {
    let plan = "Goal: fix the bug\nConstraints:\n- keep API stable";
    let prompt = summary_short_prompt(Some("earlier facts"), plan, None, false, 0);
    assert!(prompt.contains("earlier facts"));
    assert!(prompt.contains("keep API stable"));
    assert!(prompt.contains("no tool calls"));
    assert!(prompt.contains("above"));
    assert!(!prompt.contains("<conversation>"), "{prompt}");
    assert!(!prompt.contains("HARD LIMIT"));
    let retry = summary_short_prompt(None, "", None, true, 0);
    assert!(retry.contains("HARD LIMIT"));
    assert!(retry.contains("no durable plan"));
}

/// The summary is all that survives of the tool output a compaction clears from
/// the request, so it opens with where the work stands, names the paths that
/// carry it, and keeps the identifiers verbatim. The tail line used to send the
/// model to re-read files whose bytes `recall_result` still holds — that is the
/// promise the cleared wire now depends on.
#[test]
fn the_summary_leads_with_the_state_and_keeps_the_numbers() {
    let input = summary_short_input(&[user("do it")], None, "", None, false);
    let now = input.find("## Now").expect("no Now section");
    let files = input.find("## Files").expect("no Files section");
    let findings = input.find("## Findings").expect("no Findings section");
    assert!(
        now < files && files < findings,
        "what to do now, then where it lives, then what was learned: {input}"
    );
    assert!(input.contains("in their own words"), "{input}");
    assert!(input.contains("at most 12"), "{input}");
    assert!(
        input.contains("name a step by its id instead of restating it"),
        "{input}"
    );
    assert!(input.contains("verbatim"), "{input}");
    // the anti-growth rule for chained summaries: newer history wins
    assert!(input.contains("newer history wins"), "{input}");

    let applied = apply_summary("facts", &[user("next")]);
    let text = &applied[0].content;
    assert!(!text.contains("re-read a file"), "{text}");
    assert!(text.contains("recall_result"), "{text}");
}

#[test]
fn summary_short_input_restricts_to_uncovered_asks() {
    let older = vec![user("do not touch btree"), user("fix the bug")];
    let plan = "Goal: fix the bug\nConstraints:\n- keep API stable";
    let input = summary_short_input(&older, Some("earlier facts"), plan, None, false);
    // chains, renders the asks, carries the plan for exclusion
    assert!(input.contains("earlier facts"));
    assert!(input.contains("do not touch btree"));
    assert!(input.contains("keep API stable"));
    assert!(input.contains("not already carry"));
    assert!(!input.contains("HARD LIMIT"));
    // no plan: everything counts as uncovered, said explicitly
    let noplan = summary_short_input(&older, None, "", None, false);
    assert!(noplan.contains("no durable plan"));
    assert!(!noplan.contains("earlier facts"));
    // retry appends the hard limit and nothing else changes
    let retry = summary_short_input(&older, Some("earlier facts"), plan, None, true);
    assert!(retry.contains("HARD LIMIT"));
    assert!(retry.contains("do not touch btree"));
}

#[test]
fn hard_trim_fits_the_budget_and_stays_safe() {
    let mut messages: Vec<Message> = Vec::new();
    for _ in 0..40 {
        messages.push(user(&"x".repeat(400))); // ~100 tokens each
    }
    let trimmed = hard_trim(&messages, 1_000); // ~10 messages
    assert!(estimated_tokens(&trimmed) <= 1_000);
    assert!(!trimmed.is_empty());
    assert!(
        trimmed.first().is_some_and(|m| m.role == Role::User),
        "a trimmed history must open on a user turn"
    );
    // the newest turn always survives
    assert_eq!(
        trimmed.last().unwrap().content,
        messages.last().unwrap().content
    );
}

#[test]
fn hard_trim_never_starts_with_tool_role() {
    let messages = vec![
        user("run command"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![
            crate::providers::ToolCallReq::new("c1", "bash", serde_json::json!({"command": "ls"})),
        ]),
        Message::tool_result("c1", "file.txt", false),
    ];
    // Budget is 0, forcing fallback
    let trimmed = hard_trim(&messages, 0);
    assert!(!trimmed.is_empty());
    assert_eq!(trimmed[0].role, Role::User);
}

#[test]
fn hard_trim_never_eats_a_leading_summary() {
    // tail alone exceeds the budget: without protection the trim would
    // drop the summary it has just written, breaking the chain
    let mut messages = vec![Message::new(
        Role::User,
        "<conversation-summary>\nold facts\n</conversation-summary>",
    )];
    for i in 0..10 {
        messages.push(user(&format!("turn {i} {}", "x".repeat(400))));
    }
    let trimmed = hard_trim(&messages, 500);
    assert!(
        trimmed[0].content.contains("<conversation-summary>"),
        "summary must survive: {:?}",
        trimmed[0].content.chars().take(60).collect::<String>()
    );
    // the tail behind it still got trimmed to roughly the budget
    assert!(trimmed.len() < messages.len());
}

#[test]
fn hard_trim_cuts_a_single_prompt_tool_loop() {
    // Autonomous run: one user prompt, then a long assistant/tool chain
    // with no further user turns. The old user-boundary-only cut never
    // fired here: compactions stayed 0 while the transcript grew forever.
    let mut messages = vec![user("do the thing")];
    for i in 0..10 {
        messages.push(Message::new(Role::Assistant, "").with_tool_calls(vec![
            crate::providers::ToolCallReq::new(
                format!("c{i}"),
                "bash",
                serde_json::json!({"command": "ls"}),
            ),
        ]));
        messages.push(Message::tool_result(
            format!("c{i}"),
            "x".repeat(400),
            false,
        ));
    }
    let budget = estimated_tokens(&messages) / 2;
    let trimmed = hard_trim(&messages, budget);
    assert!(
        trimmed.len() < messages.len(),
        "must drop old cycles, kept {} of {}",
        trimmed.len(),
        messages.len()
    );
    // the host trim marker absorbs the small overshoot (audit H6):
    // an assistant-first transcript is a 400 on Anthropic, so the cut
    // gets a user-voiced head instead
    assert!(estimated_tokens(&trimmed) <= budget + 32);
    assert_eq!(trimmed[0].role, Role::User);
    assert_eq!(trimmed[0].content, TRIM_MARKER);
    assert_eq!(trimmed[1].role, Role::Assistant);
    // no orphan: every kept tool result's call is kept as well
    let kept: std::collections::HashSet<&str> = trimmed
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|c| c.id.as_str()))
        .collect();
    for m in &trimmed {
        if m.role == Role::Tool
            && let Some(id) = &m.tool_call_id
        {
            assert!(kept.contains(id.as_str()), "orphaned tool result {id}");
        }
    }
    // the newest cycle survives
    assert_eq!(
        trimmed.last().unwrap().content,
        messages.last().unwrap().content
    );
}

#[test]
fn split_summarizes_a_tool_loop_with_one_user_turn() {
    let mut messages = vec![user("do the thing")];
    for i in 0..10 {
        messages.push(
            Message::new(Role::Assistant, format!("step {i}")).with_tool_calls(vec![
                crate::providers::ToolCallReq::new(
                    format!("c{i}"),
                    "bash",
                    serde_json::json!({"command": "ls"}),
                ),
            ]),
        );
        messages.push(Message::tool_result(format!("c{i}"), "output", false));
    }
    let (older, keep) = split_for_summary_with_keep(&messages, 4);
    assert!(!older.is_empty(), "old cycles must be summarizable");
    assert_eq!(older.len() + keep.len(), messages.len());
    assert!(
        matches!(keep[0].role, Role::Assistant | Role::User),
        "keep must open on a step boundary, got {:?}",
        keep[0].role
    );
}

#[test]
fn anchor_restores_files_on_redo_and_lists_recent_reads_and_blockers() {
    let root = temp_root("anchor-redo-reads");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let mut events = crate::agent::events::EventLog::open(&root, "s").unwrap();
    events
        .append("file_diff", serde_json::json!({"path": "src/a.rs"}))
        .unwrap();
    events
        .append_undo("cp1", &["src/a.rs".to_string()], &[])
        .unwrap();
    events
        .append_redo(&["cp1".to_string()], &["src/a.rs".to_string()], &[])
        .unwrap();
    events
        .append(
            "observation",
            serde_json::json!({
                "tool": "read",
                "path": "src/lib.rs",
                "args_digest": "src/lib.rs"
            }),
        )
        .unwrap();
    events
        .append(
            "note",
            serde_json::json!({"note": "blocker", "text": "waiting on schema migration"}),
        )
        .unwrap();

    let rendered = anchor(&root, "s");
    assert!(
        rendered.contains("files changed this session: src/a.rs"),
        "redo must restore reverted file to anchor: {rendered}"
    );
    assert!(
        rendered.contains("recent files read: src/lib.rs"),
        "recent unedited reads must appear in anchor: {rendered}"
    );
    assert!(
        rendered.contains("waiting on schema migration"),
        "blocker notes must survive in anchor decisions: {rendered}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn anchor_tracks_fused_paths_hard_trim_deducts_summary_head_and_local_summary_skips_stubs() {
    let root = temp_root("anchor-fused-reads");
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).unwrap();
    let mut events = crate::agent::events::EventLog::open(&root, "s").unwrap();
    events
        .append(
            "observation",
            serde_json::json!({
                "tool": "read",
                "paths": ["src/one.rs", "src/two.rs"],
                "args_digest": "2 files"
            }),
        )
        .unwrap();

    let rendered = anchor(&root, "s");
    assert!(
        rendered.contains("src/one.rs · src/two.rs"),
        "fused read paths must appear in recent files read: {rendered}"
    );
    std::fs::remove_dir_all(root).ok();

    // `hard_trim` deducts the leading `<conversation-summary>` token weight from `budget`
    let summary_msg = Message::new(
        Role::User,
        format!(
            "<conversation-summary>\n{}\n</conversation-summary>",
            "s".repeat(400)
        ),
    );
    let msgs = vec![
        summary_msg.clone(),
        Message::new(Role::User, "u".repeat(400)),
        Message::new(Role::Assistant, "a".repeat(400)),
        Message::new(Role::User, "tail"),
    ];
    let head_toks = estimated_tokens(std::slice::from_ref(&summary_msg));
    let trimmed = hard_trim(&msgs, head_toks + 20);
    assert!(
        estimated_tokens(&trimmed) <= head_toks + 20,
        "hard_trim must deduct summary head tokens so total stays within budget: {} > {}",
        estimated_tokens(&trimmed),
        head_toks + 20
    );

    // `local_summary` records tool_calls on silent assistant turns and skips cleared tool stubs
    let silent_assistant = Message::new(Role::Assistant, "").with_tool_calls(vec![
        crate::providers::ToolCallReq::new(
            "c1",
            "read",
            serde_json::json!({"file_path": "src/main.rs"}),
        ),
    ]);
    let cleared_stub = Message::tool_result(
        "c1",
        "[output cleared] read src/main.rs · 20 KB of output removed",
        false,
    );
    let summary = local_summary(&[silent_assistant, cleared_stub], None);
    assert!(
        summary.contains("Agent called read"),
        "silent assistant tool calls must be preserved in local_summary: {summary}"
    );
    assert!(
        !summary.contains("[output cleared]"),
        "cleared stubs must be skipped in local_summary: {summary}"
    );
}

/// The runtime block is what lets the model plan a batch: it must name the
/// answering model and the point where history gets compacted, and stay silent
/// while the numbers are still guesses.
#[test]
fn agent_state_names_the_model_and_the_compaction_point() {
    let p = Policy::with_compaction(256_000, 0.08, 4, 0.80);
    let s = agent_state("gpt-6-luna2", 61_000, &p);
    assert!(s.contains("Model: gpt-6-luna2"), "{s}");
    assert!(s.contains("61k used of 256k"), "{s}");
    assert!(s.contains("compacts at 205k"), "{s}");
    assert!(agent_state("m", 0, &p).is_empty(), "no size yet");
    assert!(
        agent_state("m", 10, &Policy::new(0)).is_empty(),
        "no window known"
    );
}

/// A run that starts hours after the last recorded action says so. Staleness
/// of the tree is caught at edit time by hashes; staleness of *reading* is
/// something only the model can undo, and it can only do that knowingly.
#[test]
fn anchor_says_when_the_session_was_idle() {
    let root = temp_root("idle");
    let stamp_session = |name: &str, offset_hours: i64| {
        let mut events = crate::agent::events::EventLog::open(&root, name).unwrap();
        events
            .append("user_msg", serde_json::json!({"chars": 2}))
            .unwrap();
        let path = root
            .join(".sqwai")
            .join("events")
            .join(format!("{name}.jsonl"));
        let ts = (chrono::Local::now() - chrono::Duration::hours(offset_hours)).to_rfc3339();
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                record["ts"] = serde_json::Value::String(ts.clone());
                record.to_string()
            })
            .collect();
        std::fs::write(&path, lines.join("
") + "
").unwrap();
    };

    stamp_session("idle-session", 30);
    let rendered = anchor(&root, "idle-session");
    assert!(
        rendered.contains("resumed after 30h of silence"),
        "{rendered}"
    );

    stamp_session("fresh-session", 0);
    assert!(
        !anchor(&root, "fresh-session").contains("resumed after"),
        "a session with no pause claims nothing about time"
    );
    std::fs::remove_dir_all(root).ok();
}

