use super::*;

/// The cache split, nailed down: goal + constraints never mention step
/// state, and step transitions never touch the goal block. A step that
/// finishes must leave `render_goal` byte-identical.
#[test]
fn render_splits_stable_goal_from_moving_status() {
    let mut plan = new_plan();
    let goal_before = render_goal(&plan);
    assert!(
        goal_before.contains("persist the plan on disk"),
        "{goal_before}"
    );
    assert!(goal_before.contains("no new dependencies"), "{goal_before}");
    assert!(!goal_before.contains("add the model"), "{goal_before}");
    assert!(!goal_before.contains("active"), "{goal_before}");

    let status_before = render_status(&plan);
    assert!(status_before.contains("active"), "{status_before}");
    assert!(status_before.contains("add the model"), "{status_before}");

    plan.steps[0].status = StepStatus::Done;
    plan.steps[1].status = StepStatus::InProgress;
    assert_eq!(
        render_goal(&plan),
        goal_before,
        "step moves re-key nothing cached"
    );
    assert_ne!(render_status(&plan), status_before, "step moves surface");

    // the full render still carries both halves
    let full = render(&plan);
    assert!(full.contains("persist the plan on disk"), "{full}");
    assert!(full.contains("[x] 1 add the model"), "{full}");
}

fn new_plan() -> Plan {
    create(
        "persist the plan on disk".to_string(),
        vec!["no new dependencies".to_string()],
        vec!["cargo test must pass".to_string()],
        vec![
            NewStep {
                title: "add the model".to_string(),
            },
            NewStep {
                title: "add the validator".to_string(),
            },
        ],
        &Limits::default(),
    )
    .expect("plan creates")
}

#[test]
fn ulid_shape() {
    let id = new_id();
    assert_eq!(id.len(), 26, "ULID is 26 chars: {id}");
    assert!(
        id.chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)),
        "Crockford base32 only: {id}"
    );
}

#[test]
fn undo_reopens_done_step_and_clears_completion_metadata() {
    let mut plan = new_plan();
    plan.step_mut("1").unwrap().evidence.push(EvidenceRef {
        session: "test".into(),
        seq: 42,
    });
    apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "model added".into(),
        },
        &Limits::default(),
    )
    .unwrap();
    let revision = plan.revision;

    reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();

    let step = plan.step("1").unwrap();
    assert_eq!(step.status, StepStatus::Reopened);
    assert_eq!(step.finished, None);
    assert_eq!(step.summary, None);
    assert_eq!(step.reason.as_deref(), Some("reopened by undo"));
    assert!(step.evidence.is_empty());
    assert_eq!(plan.revision, revision + 1);
    assert!(
        apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                ids: Vec::new(),
                summary: "re-did model".into(),
            },
            &Limits::default(),
        )
        .is_ok()
    );
}

#[test]
fn undo_leaves_unrelated_steps_and_non_done_steps_unchanged() {
    let mut plan = new_plan();
    plan.steps[0].status = StepStatus::Done;
    plan.steps[0].finished = Some("finished".into());
    plan.steps[0].summary = Some("kept for undo test".into());
    plan.steps[1].status = StepStatus::InProgress;
    plan.steps[1].reason = Some("still working".into());
    let revision = plan.revision;

    reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();
    reopen_for_undo(&mut plan, "2", "must remain in progress").unwrap();

    assert_eq!(plan.step("1").unwrap().status, StepStatus::Reopened);
    assert_eq!(plan.step("2").unwrap().status, StepStatus::InProgress);
    assert_eq!(
        plan.step("2").unwrap().reason.as_deref(),
        Some("still working")
    );
    assert_eq!(plan.revision, revision + 1);
}

#[test]
fn undo_rejects_unknown_step() {
    let mut plan = new_plan();
    let error = reopen_for_undo(&mut plan, "missing", "reopened by undo").unwrap_err();
    assert!(error.to_string().contains("unknown plan step missing"));
}

#[test]
fn create_starts_first_step_and_finish_advances_then_completes() {
    let mut plan = new_plan();
    assert_eq!(plan.step("1").unwrap().status, StepStatus::InProgress);
    assert_eq!(plan.step("2").unwrap().status, StepStatus::Pending);

    assert!(matches!(
        apply(
            &mut plan,
            Op::Finish {
                id: "1".into(),
                ids: Vec::new(),
                summary: "model added".into()
            },
            &Limits::default(),
        ),
        Ok(Applied::Updated { .. })
    ));
    assert_eq!(plan.step("1").unwrap().status, StepStatus::Done);
    assert_eq!(plan.step("2").unwrap().status, StepStatus::InProgress);

    assert!(matches!(
        apply(
            &mut plan,
            Op::Finish {
                id: "2".into(),
                ids: Vec::new(),
                summary: "validator added".into()
            },
            &Limits::default(),
        ),
        Ok(Applied::Completed { .. })
    ));
    assert_eq!(plan.step("2").unwrap().status, StepStatus::Done);
    assert_eq!(plan.status, PlanStatus::Completed);
}

#[test]
fn finish_requires_in_progress() {
    let mut plan = new_plan();
    let err = apply(
        &mut plan,
        Op::Finish {
            id: "2".into(),
            ids: Vec::new(),
            summary: "x".into(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "step_not_in_progress");
}

#[test]
fn closed_plans_are_read_only() {
    let mut plan = new_plan();
    plan.status = PlanStatus::Completed;
    let err = apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "x".into(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "plan_closed");

    plan.status = PlanStatus::Abandoned;
    let err = apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "x".into(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "plan_closed");
}

#[test]
fn cancel_needs_a_step_id_and_never_abandons() {
    let mut plan = new_plan();
    let err = apply(
        &mut plan,
        Op::Cancel {
            id: None,
            reason: String::new(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "need_step_id");
    assert_eq!(plan.status, PlanStatus::Active);

    let id = plan.id.clone();
    let err = apply(
        &mut plan,
        Op::Cancel {
            id: Some(id),
            reason: String::new(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "abandon_user_only");
    assert_eq!(plan.status, PlanStatus::Active);

    // Cancelling step 1 automatically starts step 2
    assert!(matches!(
        apply(
            &mut plan,
            Op::Cancel {
                id: Some("1".into()),
                reason: "not needed".into(),
            },
            &Limits::default(),
        ),
        Ok(Applied::Updated { .. })
    ));
    assert_eq!(plan.step("1").unwrap().status, StepStatus::Cancelled);
    assert_eq!(plan.step("2").unwrap().status, StepStatus::InProgress);
}

#[test]
fn abandon_validates_reason_and_closes_plan() {
    let mut plan = new_plan();
    let err = apply(
        &mut plan,
        Op::Abandon {
            reason: "   ".to_string(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "empty_reason");
    let err = apply(
        &mut plan,
        Op::Abandon {
            reason: "nope".to_string(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "thin_reason");
    assert_eq!(plan.status, PlanStatus::Active, "refusals change nothing");

    assert!(matches!(
        apply(
            &mut plan,
            Op::Abandon {
                reason: "goal targets removed feature X, steps assume the deleted API".to_string(),
            },
            &Limits::default(),
        ),
        Ok(Applied::Updated { .. })
    ));
    assert_eq!(plan.status, PlanStatus::Abandoned);

    let err = apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "x".into(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "plan_closed");
}

#[test]
fn add_respects_the_limit() {
    let mut plan = new_plan();
    let limits = Limits { max_steps: 3 };
    apply(
        &mut plan,
        Op::Add {
            after: Some("1".into()),
            title: "extra".into(),
        },
        &limits,
    )
    .unwrap();
    assert_eq!(plan.steps.len(), 3);
    let err = apply(
        &mut plan,
        Op::Add {
            after: None,
            title: "one too many".into(),
        },
        &limits,
    )
    .unwrap_err();
    assert_eq!(err.code, "too_many_steps");
}

#[test]
fn store_and_reopen_round_trip() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-{}", new_id()));
    let plan = new_plan();
    let id = plan.id.clone();
    store(&dir, &plan).unwrap();
    let loaded = open(&dir, &id).unwrap();
    assert_eq!(loaded.goal.text, plan.goal.text);
    assert_eq!(loaded.steps.len(), 2);
    assert_eq!(open_active(&dir).unwrap().map(|p| p.id), Some(id.clone()));
    assert_eq!(list(&dir).len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn corrupt_plan_is_moved_aside() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-{}", new_id()));
    let plans = plans_dir(&dir);
    std::fs::create_dir_all(&plans).unwrap();
    let id = "01JNOTAPLAN";
    std::fs::write(plans.join(format!("{id}.json")), "{ not json").unwrap();
    assert!(open(&dir, id).is_err());
    assert!(plans.join("corrupt").join(format!("{id}.json")).exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replay_heals_other_session_crash_leaving_mine_alone() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-multisess-{}", new_id()));
    let mut plan = new_plan();
    plan.applied_events.insert("aaa".to_string(), 1);
    plan.applied_events.insert("bbb".to_string(), 0);
    store(&dir, &plan).unwrap();

    let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
    ja.append(
        "plan",
        serde_json::json!({
            "op": "add", "title": "already reflected",
            "plan_id": plan.id, "by": "model", "ok": true,
        }),
    )
    .unwrap();
    let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
    jb.append(
        "plan",
        serde_json::json!({
            "op": "cancel", "id": "1", "reason": "not needed",
            "plan_id": plan.id, "by": "model", "ok": true,
        }),
    )
    .unwrap();

    let report = replay(&dir).unwrap();
    assert_eq!(report.ops_applied, 1);
    let healed = open(&dir, &plan.id).unwrap();
    assert_eq!(healed.step("1").unwrap().status, StepStatus::Cancelled);
    assert_eq!(healed.step("2").unwrap().status, StepStatus::InProgress);
    assert_eq!(healed.applied_events.get("aaa"), Some(&1));
    assert_eq!(healed.applied_events.get("bbb"), Some(&1));

    let again = replay(&dir).unwrap();
    assert_eq!(again.ops_applied, 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replay_stall_in_one_session_does_not_block_the_other() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-stall-{}", new_id()));
    let mut plan = new_plan();
    plan.applied_events.insert("aaa".to_string(), 0);
    plan.applied_events.insert("bbb".to_string(), 0);
    store(&dir, &plan).unwrap();

    // aaa journaled a finish on a missing step: diverges
    let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
    ja.append(
        "plan",
        serde_json::json!({
            "op": "finish", "id": "99", "summary": "missing",
            "plan_id": plan.id, "by": "model", "ok": true,
        }),
    )
    .unwrap();
    // bbb journaled a valid finish on step 1
    let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
    jb.append(
        "plan",
        serde_json::json!({
            "op": "finish", "id": "1", "summary": "done 1",
            "plan_id": plan.id, "by": "model", "ok": true,
        }),
    )
    .unwrap();

    let report = replay(&dir).unwrap();
    assert_eq!(report.stalled, vec![plan.id.clone()]);
    let healed = open(&dir, &plan.id).unwrap();
    assert_eq!(healed.step("1").unwrap().status, StepStatus::Done);
    assert_eq!(healed.step("2").unwrap().status, StepStatus::InProgress);
    assert_eq!(healed.applied_events.get("aaa"), Some(&0));
    assert_eq!(healed.applied_events.get("bbb"), Some(&1));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_rebuilds_torn_plan_file_from_journal() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-rebuild-{}", new_id()));
    let id = "rebuild-me";
    let mut ja = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
    ja.append(
        "plan",
        serde_json::json!({
            "op": "create", "result_id": id, "result_created": "t",
            "result_sessions": ["aaa", "bbb"],
            "goal": "ship it", "constraints": ["c1"],
            "acceptance": ["check x"], "budget_limit": 1000,
            "steps": [{"title": "s1"}, {"title": "s2"}],
            "by": "model", "ok": true,
        }),
    )
    .unwrap();
    let mut jb = crate::agent::journal::Journal::open(&dir, "bbb").unwrap();
    jb.append(
        "plan",
        serde_json::json!({
            "op": "finish", "id": "1", "summary": "s1 done",
            "plan_id": id, "by": "model", "ok": true,
        }),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".sqwai/plans")).unwrap();
    std::fs::write(
        dir.join(".sqwai/plans").join(format!("{id}.json")),
        r#"{"version":1,"id":"rebuild-me","status":"act"#,
    )
    .unwrap();

    let plan = open(&dir, id).expect("torn file rebuilds");
    assert_eq!(plan.goal.text, "ship it");
    assert_eq!(plan.step("1").unwrap().status, StepStatus::Done);
    assert_eq!(plan.step("2").unwrap().status, StepStatus::InProgress);
    assert_eq!(plan.applied_events.get("aaa"), Some(&1));
    assert_eq!(plan.applied_events.get("bbb"), Some(&1));
    assert!(
        !dir.join(".sqwai/plans/corrupt")
            .join(format!("{id}.json"))
            .exists()
    );
    let again = open(&dir, id).expect("rebuilt file parses");
    assert_eq!(again.step("1").unwrap().status, StepStatus::Done);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_quarantines_when_rebuild_diverges_or_lacks_intent() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-diverge-{}", new_id()));
    let id = "diverged";
    let mut j = crate::agent::journal::Journal::open(&dir, "aaa").unwrap();
    j.append(
        "plan",
        serde_json::json!({
            "op": "create", "result_id": id, "result_created": "t",
            "result_sessions": ["aaa"],
            "goal": "g", "constraints": [], "criteria": ["note"],
            "budget_limit": 1000, "steps": [{"title": "s1"}],
            "by": "model", "ok": true,
        }),
    )
    .unwrap();
    j.append(
        "plan",
        serde_json::json!({
            "op": "finish", "id": "99", "summary": "missing",
            "plan_id": id, "by": "model", "ok": true,
        }),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join(".sqwai/plans")).unwrap();
    std::fs::write(
        dir.join(".sqwai/plans").join(format!("{id}.json")),
        "garbage{",
    )
    .unwrap();
    assert!(open(&dir, id).is_err());
    assert!(
        dir.join(".sqwai/plans/corrupt")
            .join(format!("{id}.json"))
            .exists()
    );

    let dir2 = std::env::temp_dir().join(format!("sqwai-plan-del-{}", new_id()));
    let mut j2 = crate::agent::journal::Journal::open(&dir2, "aaa").unwrap();
    j2.append(
        "plan",
        serde_json::json!({
            "op": "create", "result_id": "gone", "result_created": "t",
            "result_sessions": ["aaa"],
            "goal": "g", "constraints": [], "criteria": ["note"],
            "budget_limit": 1000, "steps": [{"title": "s1"}],
            "by": "model", "ok": true,
        }),
    )
    .unwrap();
    j2.append(
        "plan",
        serde_json::json!({"op": "plan_deleted", "plan_id": "gone", "by": "host", "ok": true}),
    )
    .unwrap();
    std::fs::create_dir_all(dir2.join(".sqwai/plans")).unwrap();
    std::fs::write(dir2.join(".sqwai/plans/gone.json"), "garbage{").unwrap();
    assert!(open(&dir2, "gone").is_err());
    assert!(dir2.join(".sqwai/plans/corrupt/gone.json").exists());
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

#[test]
fn replay_heals_crash_between_journal_and_store() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-replay-{}", new_id()));
    let mut plan = new_plan();
    plan.applied_events.insert("crash".to_string(), 0);
    store(&dir, &plan).unwrap();

    let mut journal = crate::agent::journal::Journal::open(&dir, "crash").unwrap();
    let seq = journal
        .append(
            "plan",
            serde_json::json!({
                "op": "finish", "id": "1", "summary": "finished step 1",
                "plan_id": plan.id, "by": "model", "ok": true,
            }),
        )
        .unwrap();
    assert_eq!(seq, 1);

    let report = replay(&dir).unwrap();
    assert_eq!(report.ops_applied, 1);
    assert_eq!(report.plans_healed, vec![plan.id.clone()]);
    let healed = open(&dir, &plan.id).unwrap();
    assert_eq!(healed.step("1").unwrap().status, StepStatus::Done);
    assert_eq!(healed.step("2").unwrap().status, StepStatus::InProgress);
    assert_eq!(healed.applied_events.get("crash"), Some(&1));

    let again = replay(&dir).unwrap();
    assert_eq!(again.ops_applied, 0);
    assert!(again.plans_healed.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn join_is_journal_first_idempotent_and_replayable() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-join-{}", new_id()));
    let mut plan = new_plan();
    plan.sessions = vec!["parent".to_string()];
    plan.applied_events.insert("parent".to_string(), 0);
    store(&dir, &plan).unwrap();

    let mut journal = crate::agent::journal::Journal::open(&dir, "parent").unwrap();
    journal
        .append(
            "plan",
            serde_json::json!({
                "op": "join", "session": "sub-9",
                "plan_id": plan.id, "by": "host", "ok": true,
            }),
        )
        .unwrap();

    let report = replay(&dir).unwrap();
    assert_eq!(report.ops_applied, 1);
    let healed = open(&dir, &plan.id).unwrap();
    assert!(healed.sessions.contains(&"sub-9".to_string()));
    assert_eq!(healed.applied_events.get("parent"), Some(&1));

    let again = replay(&dir).unwrap();
    assert_eq!(again.ops_applied, 0);
    let mut healed = healed;
    apply(
        &mut healed,
        Op::Join {
            session: "sub-9".to_string(),
        },
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(healed.sessions.iter().filter(|s| *s == "sub-9").count(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replay_rebuilds_orphan_create_and_respects_delete() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-orphan-{}", new_id()));
    let mut journal = crate::agent::journal::Journal::open(&dir, "orphan").unwrap();
    journal
        .append(
            "plan",
            serde_json::json!({
                "op": "create", "by": "model", "ok": true,
                "plan_id": "01J000ORPHAN00000000000001",
                "goal": "orphaned goal",
                "constraints": [],
                "acceptance": ["check true"],
                "steps": [{"title": "s1"}],
                "budget_limit": 20000,
                "result_id": "01J000ORPHAN00000000000001",
                "result_created": "2026-01-01T00:00:00+00:00",
                "result_sessions": ["orphan"],
            }),
        )
        .unwrap();

    let report = replay(&dir).unwrap();
    assert_eq!(report.orphans_rebuilt, vec!["01J000ORPHAN00000000000001"]);
    let rebuilt = open(&dir, "01J000ORPHAN00000000000001").unwrap();
    assert_eq!(rebuilt.goal.text, "orphaned goal");
    assert_eq!(rebuilt.applied_events.get("orphan"), Some(&1));

    journal
        .append(
            "plan_deleted",
            serde_json::json!({"plan_id": "01J000ORPHAN00000000000001"}),
        )
        .unwrap();
    std::fs::remove_file(plans_dir(&dir).join("01J000ORPHAN00000000000001.json")).unwrap();
    let report = replay(&dir).unwrap();
    assert!(report.orphans_rebuilt.is_empty());
    assert!(open(&dir, "01J000ORPHAN00000000000001").is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replay_skips_rejected_intents() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-repskip-{}", new_id()));
    let mut plan = new_plan();
    plan.applied_events.insert("rs".to_string(), 0);
    store(&dir, &plan).unwrap();
    let mut journal = crate::agent::journal::Journal::open(&dir, "rs").unwrap();
    journal
        .append(
            "plan",
            serde_json::json!({
                "op": "finish", "id": "1", "summary": "",
                "plan_id": plan.id, "by": "model", "ok": false,
            }),
        )
        .unwrap();

    let report = replay(&dir).unwrap();
    assert_eq!(report.ops_applied, 0);
    assert_eq!(
        open(&dir, &plan.id).unwrap().step("1").unwrap().status,
        StepStatus::InProgress
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn legacy_plan_file_loads_with_phase0_defaults() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-legacy-{}", new_id()));
    let plans = plans_dir(&dir);
    std::fs::create_dir_all(&plans).unwrap();
    let id = "01JLEGACYPLAN00000000000001";
    std::fs::write(
        plans.join(format!("{id}.json")),
        serde_json::json!({
            "version": 1,
            "id": id,
            "status": "active",
            "created": "2026-01-01T00:00:00+00:00",
            "forked_from": "01JOLDPARENT00000000000000",
            "sessions": ["s1"],
            "goal": {"text": "old goal", "source": "user", "created": "..."},
            "acceptance": [
                {"text": "cargo test", "status": "verified", "evidence": [3]},
            ],
            "steps": [
                {"id": "1", "title": "old step", "status": "done",
                 "evidence": [1, 2],
                 "refs": ["src/session/mod.rs::fn::save", "src/plain.rs"]},
            ],
            "budget": {"tokens": 10, "limit": 20000},
            "revision": 2,
        })
        .to_string(),
    )
    .unwrap();
    let plan = open(&dir, id).unwrap();
    assert!(plan.criteria.is_empty());
    assert!(plan.applied_events.is_empty());
    let step = &plan.steps[0];
    assert_eq!(step.step_epoch, 0);
    assert_eq!(step.evidence.len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reopen_bumps_epoch_and_clears_evidence() {
    let mut plan = new_plan();
    plan.step_mut("1").unwrap().evidence.push(EvidenceRef {
        session: "test".into(),
        seq: 42,
    });
    apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "model added".into(),
        },
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(plan.step("1").unwrap().step_epoch, 0);

    reopen_for_undo(&mut plan, "1", "reopened by undo").unwrap();

    let step = plan.step("1").unwrap();
    assert_eq!(step.status, StepStatus::Reopened);
    assert_eq!(step.step_epoch, 1);
    assert!(step.evidence.is_empty());
}

#[test]
fn reopen_for_undo_reactivates_completed_plan() {
    let mut plan = new_plan();
    plan.status = PlanStatus::Completed;
    plan.steps[0].status = StepStatus::Done;
    reopen_for_undo(&mut plan, "1", "undo step 1").unwrap();
    assert_eq!(plan.steps[0].status, StepStatus::Reopened);
    assert_eq!(plan.status, PlanStatus::Active);
}

#[test]
fn open_active_resolves_per_session_and_deterministically() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-resolve-{}", new_id()));
    let mut parent = new_plan();
    parent.sessions = vec!["sess-parent".into()];
    parent.created = "2026-01-01T00:00:00+00:00".to_string();
    let parent_id = parent.id.clone();
    store(&dir, &parent).unwrap();

    let mut second = new_plan();
    second.sessions = vec!["sess-child".into()];
    second.created = "2026-09-09T00:00:00+00:00".to_string();
    let child_id = second.id.clone();
    store(&dir, &second).unwrap();

    let resolved_parent = open_active_for_session(&dir, Some("sess-parent"))
        .unwrap()
        .unwrap();
    assert_eq!(resolved_parent.id, parent_id);

    let resolved_child = open_active_for_session(&dir, Some("sess-child"))
        .unwrap()
        .unwrap();
    assert_eq!(resolved_child.id, child_id);

    let resolved_default = open_active(&dir).unwrap().unwrap();
    assert_eq!(resolved_default.id, child_id);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_active_for_session_never_returns_a_foreign_plan() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-strict-{}", new_id()));
    let mut plan = new_plan();
    plan.sessions = vec!["owner".into()];
    store(&dir, &plan).unwrap();

    assert!(
        open_active_for_session(&dir, Some("stranger"))
            .unwrap()
            .is_none(),
        "a plan-less session must not adopt the foreign plan"
    );
    assert!(
        open_active_for_session(&dir, Some("owner"))
            .unwrap()
            .is_some()
    );
    assert!(open_active(&dir).unwrap().is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_active_for_session_falls_back_to_the_later_instant() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-order-{}", new_id()));

    let mut older = new_plan();
    older.created = "2026-10-25T02:00:00+03:00".into();
    older.sessions = vec!["older".into()];
    store(&dir, &older).unwrap();

    let mut newer = new_plan();
    newer.created = "2026-10-25T01:30:00+02:00".into();
    newer.sessions = vec!["newer".into()];
    store(&dir, &newer).unwrap();

    let picked = open_active_for_session(&dir, None).unwrap();
    assert_eq!(
        picked.as_ref().map(|p| p.id.as_str()),
        Some(newer.id.as_str()),
        "the fallback must rank by instant, not by string"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn future_plan_version_is_refused_not_adopted() {
    let dir = std::env::temp_dir().join(format!("sqwai-plan-future-{}", new_id()));
    let mut plan = new_plan();
    plan.version = super::PLAN_VERSION + 1;
    store(&dir, &plan).unwrap();

    let err = open(&dir, &plan.id).unwrap_err();
    assert!(
        err.to_string().contains("reads up to version"),
        "must name the cause: {err:#}"
    );
    assert!(
        plans_dir(&dir).join(format!("{}.json", plan.id)).exists(),
        "a future-version file must not be quarantined"
    );
    assert!(
        !plans_dir(&dir).join("corrupt").exists(),
        "no quarantine dir for a well-formed future file"
    );
    assert!(
        read_plan_file(&dir, &plan.id).is_none(),
        "the linked-plan resolver must not adopt it"
    );
    assert!(
        list(&dir).iter().all(|p| p.id != plan.id),
        "replay/list must not heal a file this binary cannot read"
    );
    assert!(
        open_active_for_session(&dir, None).unwrap().is_none(),
        "the newest-plan fallback must not pick it"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn add_refuses_non_numeric_step_ids_instead_of_colliding() {
    let mut plan = new_plan();
    plan.steps[0].id = "1a".to_string();
    let err = apply(
        &mut plan,
        Op::Add {
            after: None,
            title: "another".to_string(),
        },
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(err.code, "bad_step_id", "{err:?}");
    assert_eq!(plan.steps.len(), 2, "a refused add stores nothing");
}

#[test]
fn new_step_deserializes_strings_and_aliases_and_finish_names_next_step() {
    let steps: Vec<NewStep> = serde_json::from_value(serde_json::json!([
        "plain string step",
        {"step": "step alias"},
        {"description": "desc alias"}
    ]))
    .unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].title, "plain string step");
    assert_eq!(steps[1].title, "step alias");
    assert_eq!(steps[2].title, "desc alias");

    let mut plan = new_plan();
    let res = apply(
        &mut plan,
        Op::Finish {
            id: "1".into(),
            ids: Vec::new(),
            summary: "done 1".into(),
        },
        &Limits::default(),
    )
    .unwrap();
    match res {
        Applied::Updated { message } => {
            assert!(
                message.contains("next step in progress: 2"),
                "finish should point to next in-progress step: {message}"
            );
        }
        _ => panic!("expected Updated"),
    }
}
