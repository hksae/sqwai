use super::*;

#[test]
fn plan_delete_user_command_flow() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir =
        std::env::temp_dir().join(format!("sqwai-test-plan-delete-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    app.session.plan_id = None;

    // 1. When no active plan exists:
    app.plan_command("/plan delete");
    assert!(
        app.menu_stack.is_empty(),
        "menu should not open when no active plan"
    );
    assert!(
        toast_text(&app) == "no active plan",
        "refusal toasts: {:?}",
        toast_text(&app)
    );

    // 2. Create an active plan in temp_dir
    let limits = plan::Limits { max_steps: 10 };
    let created = plan::create(
        "test delete goal".into(),
        vec![],
        vec![],
        vec![plan::NewStep {
            title: "step 1".into(),
            refs: vec![],
        }],
        1000,
        &limits,
    )
    .unwrap();
    plan::store(&temp_dir, &created).unwrap();
    app.session.plan_id = Some(created.id.clone());
    let plan_file = plan::plans_dir(&temp_dir).join(format!("{}.json", created.id));
    assert!(plan_file.exists());

    // 3. /plan delete should open confirmation dialog
    app.plan_command("/plan delete");
    assert!(matches!(
        app.cur_menu(),
        Some(Menu::ConfirmDelete { label, .. }) if label == "Are you sure? (y/N)"
    ));

    // 4. Confirming delete removes file, clears session.plan_id, and records journal event
    app.run_confirm_action();
    assert!(!plan_file.exists(), "plan file must be deleted");
    assert_eq!(app.session.plan_id, None, "session plan_id must be cleared");
    assert!(
        toast_text(&app) == "plan deleted; no active plan",
        "success toasts, it must not vanish with the menu: {:?}",
        toast_text(&app)
    );

    // Verify journal has plan_deleted event
    let records =
        crate::agent::journal::Journal::records_for(&temp_dir, &app.session.id.to_string())
            .unwrap();
    assert!(
        records.iter().any(|r| r.kind == "plan_deleted"
            && r.fields.get("plan_id").and_then(|v| v.as_str()) == Some(&created.id)),
        "journal must record plan_deleted event"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn plan_confirm_user_command_flow() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir =
        std::env::temp_dir().join(format!("sqwai-test-plan-confirm-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    app.session.plan_id = None;

    // usage without a reason
    app.plan_command("/plan confirm 0");
    assert!(
        toast_text(&app).starts_with("usage: /plan confirm"),
        "usage toasts: {:?}",
        toast_text(&app)
    );

    let limits = plan::Limits { max_steps: 10 };
    let created = plan::create(
        "test confirm goal".into(),
        vec![],
        vec!["manual: eyeball it".into()],
        vec![plan::NewStep {
            title: "step 1".into(),
            refs: vec![],
        }],
        1000,
        &limits,
    )
    .unwrap();
    plan::store(&temp_dir, &created).unwrap();
    app.session.plan_id = Some(created.id.clone());

    app.plan_command("/plan confirm 0 looks good");
    assert_eq!(toast_text(&app), "acceptance 0 confirmed");
    let reloaded = plan::read_plan_file(&temp_dir, &created.id).unwrap();
    assert_eq!(
        reloaded.acceptance[0].validation.status,
        plan::ValidationStatus::Passed
    );
    assert_eq!(
        reloaded.acceptance[0]
            .validation
            .receipts
            .last()
            .and_then(|r| r.runner.as_deref()),
        Some("manual")
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn plan_delete_prefers_session_plan_over_most_recent() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-plan-delete-multi-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();

    let limits = plan::Limits { max_steps: 10 };
    let mk_plan = |goal: &str| {
        plan::create(
            goal.into(),
            vec![],
            vec![],
            vec![plan::NewStep {
                title: "step 1".into(),
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap()
    };
    // session's own (older) plan and a newer unrelated active plan
    let mut plan_x = mk_plan("session plan");
    plan_x.created = "2026-01-01T00:00:00+00:00".to_string();
    plan::store(&temp_dir, &plan_x).unwrap();
    let mut plan_y = mk_plan("other plan");
    plan_y.created = "2026-09-09T00:00:00+00:00".to_string();
    plan::store(&temp_dir, &plan_y).unwrap();
    app.session.plan_id = Some(plan_x.id.clone());

    let file_x = plan::plans_dir(&temp_dir).join(format!("{}.json", plan_x.id));
    let file_y = plan::plans_dir(&temp_dir).join(format!("{}.json", plan_y.id));

    app.plan_command("/plan delete");
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
    app.run_confirm_action();

    assert!(!file_x.exists(), "session plan file must be deleted");
    assert!(file_y.exists(), "unrelated active plan must survive");
    assert_eq!(app.session.plan_id, None);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn completed_linked_plan_refuses_tui_mutations() {
    // Defect A: a linked completed plan is read-only history. TUI
    // mutations must refuse it explicitly instead of silently rewriting
    // a finished plan the agent itself can no longer touch.
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-plan-completed-guard-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();

    let limits = plan::Limits { max_steps: 10 };
    let mut finished = plan::create(
        "done goal".into(),
        vec![],
        vec!["manual: eyeball it".into()],
        vec![plan::NewStep {
            title: "step 1".into(),
            refs: vec![],
        }],
        1000,
        &limits,
    )
    .unwrap();
    finished.status = plan::PlanStatus::Completed;
    plan::store(&temp_dir, &finished).unwrap();
    app.session.plan_id = Some(finished.id.clone());
    let revision = finished.revision;

    let last_status = |app: &App| toast_text(app);

    app.plan_command("/plan complete");
    assert!(
        last_status(&app).contains("completed") && last_status(&app).contains("read-only"),
        "complete must refuse: {}",
        last_status(&app)
    );
    app.plan_command("/plan waive 0 looks fine");
    assert!(
        last_status(&app).contains("read-only"),
        "waive must refuse: {}",
        last_status(&app)
    );
    app.plan_command("/plan abandon");
    assert!(
        last_status(&app).contains("read-only"),
        "abandon must refuse: {}",
        last_status(&app)
    );
    app.goal_command("/goal a new direction");
    assert!(
        last_status(&app).contains("read-only"),
        "goal must refuse: {}",
        last_status(&app)
    );
    app.constraints_command("/constraints add something");
    assert!(
        last_status(&app).contains("read-only"),
        "constraints must refuse: {}",
        last_status(&app)
    );

    let after: plan::Plan = serde_json::from_str(
        &std::fs::read_to_string(
            plan::plans_dir(&temp_dir).join(format!("{}.json", finished.id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(after.status, plan::PlanStatus::Completed);
    assert_eq!(after.revision, revision, "refused mutations must not write");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn plan_delete_then_session_has_no_plan() {
    // #171: after delete, another session's active plan must NOT
    // surface as this session's — the session simply has no plan.
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-plan-delete-foreign-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    let sid = app.session.id.to_string();

    let limits = plan::Limits { max_steps: 10 };
    let mk_plan = |goal: &str| {
        plan::create(
            goal.into(),
            vec![],
            vec![],
            vec![plan::NewStep {
                title: "step 1".into(),
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap()
    };
    let mut own = mk_plan("own finished work");
    own.created = "2026-01-01T00:00:00+00:00".to_string();
    own.sessions = vec![sid.clone()];
    plan::store(&temp_dir, &own).unwrap();
    let mut foreign = mk_plan("foreign active work");
    foreign.created = "2026-09-09T00:00:00+00:00".to_string();
    foreign.sessions = vec!["someone-else".into()];
    plan::store(&temp_dir, &foreign).unwrap();
    // `foreign` stays stored but unreferenced below: its presence is
    // the point — it must not surface as this session's plan.
    let _ = &foreign;
    app.session.plan_id = Some(own.id.clone());

    app.plan_command("/plan delete");
    app.run_confirm_action();

    assert_eq!(app.session.plan_id, None);
    let text = toast_text(&app);
    assert!(
        text.contains("no active plan"),
        "delete leaves the session plan-less: {text}"
    );
    assert!(
        app.session_plan().is_none(),
        "no silent fallback onto the foreign plan"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn plan_delete_reports_failure_instead_of_lying() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-plan-delete-locked-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();

    // an unremovable entry where the plan file should be: remove_file
    // fails on it on every platform (as with a locked file on Windows)
    let ghost_id = "ghostplan";
    let ghost_path = plan::plans_dir(&temp_dir).join(format!("{ghost_id}.json"));
    std::fs::create_dir_all(&ghost_path).unwrap();
    app.session.plan_id = Some(ghost_id.into());

    app.plan_command("/plan delete");
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
    app.run_confirm_action();

    assert!(ghost_path.exists(), "unremovable entry must still be there");
    assert_eq!(
        app.session.plan_id.as_deref(),
        Some(ghost_id),
        "plan_id must be kept while the plan still exists"
    );
    assert!(
        toast_text(&app).contains("plan delete failed"),
        "toast must report the failure, not a fake success: {:?}",
        toast_text(&app)
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn tui_commands_operate_on_session_plan_not_newest_global() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-plan-session-scope-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    let sid = app.session.id.to_string();

    let limits = plan::Limits { max_steps: 10 };
    let mk_plan = |goal: &str| {
        plan::create(
            goal.into(),
            vec![],
            vec!["manual: eyeball it".into()],
            vec![plan::NewStep {
                title: "step 1".into(),
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap()
    };
    // session's own older plan vs a newer unrelated active plan
    let mut plan_a = mk_plan("session goal");
    plan_a.created = "2026-01-01T00:00:00+00:00".to_string();
    plan_a.sessions = vec![sid.clone()];
    plan::store(&temp_dir, &plan_a).unwrap();
    let mut plan_b = mk_plan("other goal");
    plan_b.created = "2026-09-09T00:00:00+00:00".to_string();
    plan_b.sessions = vec!["someone-else".into()];
    plan::store(&temp_dir, &plan_b).unwrap();
    app.session.plan_id = Some(plan_a.id.clone());

    // resolver prefers the linked plan over the newest global one
    assert_eq!(app.session_plan().map(|p| p.id), Some(plan_a.id.clone()));

    // /plan waive mutates A, leaves B alone
    app.plan_command("/plan waive 0 looks good");
    let a_after = plan::open(&temp_dir, &plan_a.id).unwrap();
    assert!(matches!(
        a_after.acceptance[0].status,
        plan::AcceptanceStatus::Waived
    ));
    let b_after = plan::open(&temp_dir, &plan_b.id).unwrap();
    assert!(matches!(
        b_after.acceptance[0].status,
        plan::AcceptanceStatus::Pending
    ));

    // /plan complete mutates A, leaves B alone
    let mut a_ready = a_after;
    a_ready.steps[0].status = plan::StepStatus::Done;
    plan::store(&temp_dir, &a_ready).unwrap();
    app.plan_command("/plan complete");
    let a_completed = plan::open(&temp_dir, &plan_a.id).unwrap();
    assert_eq!(a_completed.status, plan::PlanStatus::Completed);
    let b_after_complete = plan::open(&temp_dir, &plan_b.id).unwrap();
    assert_eq!(b_after_complete.status, plan::PlanStatus::Active);

    // /plan abandon mutates linked plan, leaves B alone
    let mut plan_c = mk_plan("session C goal");
    plan_c.sessions = vec![sid.clone()];
    plan::store(&temp_dir, &plan_c).unwrap();
    app.session.plan_id = Some(plan_c.id.clone());
    app.plan_command("/plan abandon");
    let c_abandoned = plan::open(&temp_dir, &plan_c.id).unwrap();
    assert_eq!(c_abandoned.status, plan::PlanStatus::Abandoned);
    let b_still_active = plan::open(&temp_dir, &plan_b.id).unwrap();
    assert_eq!(b_still_active.status, plan::PlanStatus::Active);

    app.session.plan_id = Some(plan_a.id.clone());
    // Menu::Plan renders the session's plan, not the newest global one
    app.open_menu(Menu::Plan);
    let rendered: String = app
        .menu_rows
        .iter()
        .flat_map(|(line, _)| line.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect();
    assert!(rendered.contains(&plan_a.id), "menu must show session plan");
    assert!(
        !rendered.contains(&plan_b.id),
        "menu must not show another session's plan"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn plan_waive_constraint_marks_and_renders() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp_dir = std::env::temp_dir().join(format!(
        "sqwai-test-waive-constraint-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    let sid = app.session.id.to_string();

    let limits = plan::Limits { max_steps: 10 };
    let mut plan = plan::create(
        "constrained".into(),
        vec!["forbid-import: btree".into(), "keep the format".into()],
        vec!["manual: eyeball it".into()],
        vec![plan::NewStep {
            title: "step 1".into(),
            refs: vec![],
        }],
        1000,
        &limits,
    )
    .unwrap();
    plan.sessions = vec![sid.clone()];
    plan::store(&temp_dir, &plan).unwrap();
    app.session.plan_id = Some(plan.id.clone());

    // usage without reason
    app.plan_command("/plan waive-constraint 0");
    assert!(
        toast_text(&app).contains("usage"),
        "needs index and reason: {}",
        toast_text(&app)
    );
    // unknown index
    app.plan_command("/plan waive-constraint 9 legacy use");
    assert!(
        toast_text(&app).contains("unknown_constraint"),
        "{}",
        toast_text(&app)
    );
    // happy path: marked, rendered, idempotent
    app.plan_command("/plan waive-constraint 0 legacy use, tracked");
    assert!(
        toast_text(&app).contains("constraint 0 waived"),
        "{}",
        toast_text(&app)
    );
    let after = plan::open(&temp_dir, &plan.id).unwrap();
    assert_eq!(plan::waived_constraint_indices(&after), vec![0]);
    assert!(plan::render(&after).contains("[waived]"));
    app.plan_command("/plan waive-constraint 0 again");
    let again = plan::open(&temp_dir, &plan.id).unwrap();
    assert_eq!(again.waived_constraints.len(), 1);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn builtin_providers_and_models_are_immutable_and_updateable() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());

    // Built-in provider and model checks
    assert!(app.cfg.is_builtin_provider("gemini"));
    assert!(app.cfg.is_builtin_model("gemini-3.8-flash"));

    // 1. Trying to delete built-in provider is refused
    app.run_action(MenuAction::DeleteProvider("gemini".into()));
    assert!(toast_text(&app).contains("cannot be deleted"));

    // 2. Trying to delete built-in model is refused
    app.run_action(MenuAction::DeleteModel(
        "gemini".into(),
        "gemini-3.8-flash".into(),
    ));
    assert!(toast_text(&app).contains("cannot be deleted"));

    // 3. Trying to edit built-in model is refused
    app.run_action(MenuAction::EditModel(
        "gemini".into(),
        "gemini-3.8-flash".into(),
    ));
    assert!(toast_text(&app).contains("cannot be modified"));

    // 4. /providers update triggers update status
    app.builtin_update_rx = None; // clear in-flight check from test_app startup
    app.command("providers update");
    assert!(toast_text(&app).contains("checking for built-in provider updates"));
}

#[test]
fn model_menu_row_formats_ctx() {
    use super::super::menus::fmt_ctx;

    assert_eq!(fmt_ctx(1048576), "1m");
    assert_eq!(fmt_ctx(1000000), "1m");
    assert_eq!(fmt_ctx(1050000), "1.05m");
    assert_eq!(fmt_ctx(262144), "256k");
    assert_eq!(fmt_ctx(200000), "200k");
    assert_eq!(fmt_ctx(131072), "128k");
    assert_eq!(fmt_ctx(128000), "128k");
    assert_eq!(fmt_ctx(32768), "32k");
    assert_eq!(fmt_ctx(8192), "8k");
    assert_eq!(fmt_ctx(512), "512");

    // row shows compact ctx
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.insert(
        "gpt-5.5".into(),
        ModelConfig {
            provider: "openai".into(),
            id: "gpt-5.5".into(),
            context: 1050000,
            effort: EffortLevel::High,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
        },
    );
    app.open_menu(Menu::Models {
        provider: "openai".into(),
    });
    app.build_menu_rows();
    let texts: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("1.05m")), "rows: {texts:?}");
}

/// Review checklist 1–2: composer input, scrolling and selection are
/// paint-only — an idle frame must not re-render any segment nor wrap
/// any row.
#[test]
fn idle_draw_skips_transcript_pipeline() {
    use std::sync::atomic::Ordering;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("hello".into()));
    app.push_segment(Segment::Assistant {
        text: "world".into(),
        live: false,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.test_renders = 0;
    crate::tui::markdown::WRAP_TAGGED_CALLS.swap(0, Ordering::SeqCst);
    app.input.insert_str("typing");
    app.scroll(4);
    app.sel = Some(Selection {
        a: CellPos { row: 0, col: 0 },
        b: CellPos { row: 1, col: 2 },
    });
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.test_renders, 0, "idle frame re-rendered segments");
    assert_eq!(
        crate::tui::markdown::WRAP_TAGGED_CALLS.load(Ordering::SeqCst),
        0,
        "idle frame wrapped rows"
    );
}

/// Review checklist 3: appending one segment renders exactly that
/// segment; every previously assembled row stays byte-identical.
#[test]
fn append_renders_only_the_new_segment() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    for i in 0..5 {
        app.push_segment(Segment::User(format!("q{i}")));
        app.push_segment(Segment::Assistant {
            text: format!("a{i}"),
            live: false,
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let before: Vec<String> = app
        .cache_lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    app.test_renders = 0;
    app.push_segment(Segment::Status {
        text: "done".into(),
        kind: StatusKind::Info,
        expanded: false,
        transient: false,
    });
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.test_renders, 1, "append re-rendered old segments");
    let after: Vec<String> = app
        .cache_lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "append rewrote previously assembled rows"
    );
}

/// Review checklist 4 (patch 1): expanding a tool keeps its header on
/// the same screen row instead of jumping with `follow`.
#[test]
fn toggle_keeps_header_on_screen_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    for i in 0..3 {
        app.push_segment(Segment::User(format!("q{i}")));
    }
    let tool_idx = app.segments.len();
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "f".into(),
        ok: Some(true),
        output: "l1\nl2\nl3".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    for i in 0..25 {
        app.push_segment(Segment::Assistant {
            text: format!("filler {i}"),
            live: false,
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(tool_idx))
        .expect("tool header assembled");
    app.follow = false;
    app.view_top = header - 5;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let h = app.last_chat.height;
    let screen_before = header - app.chat_top(h);
    assert_eq!(screen_before, 5);
    app.click(header);
    assert!(!app.follow, "toggle must leave follow disengaged");
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let header_after = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(tool_idx))
        .expect("tool header survives expand");
    assert_eq!(
        header_after - app.chat_top(h),
        5,
        "header moved on screen after expand"
    );
    assert!(
        matches!(
            app.segments.get(tool_idx),
            Some(Segment::Tool { expanded: true, .. })
        ),
        "tool did not expand"
    );
}

/// Review checklist 5: a repeat draw of an unchanged subagent view
/// clones no transcript and renders nothing; closing restores the main
/// rows whole, again with zero renders.
#[test]
fn subagent_redraw_reuses_cache_and_switch_restores_whole() {
    use std::sync::atomic::Ordering;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("main q".into()));
    app.push_segment(Segment::Assistant {
        text: "main a".into(),
        live: false,
    });
    app.subagent_chats.insert(
        7,
        vec![Segment::Tool {
            call_id: None,
            name: "read".into(),
            args: "src/main.rs".into(),
            ok: Some(true),
            output: "contents".into(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        }],
    );
    app.subagent_meta
        .insert(7, vec![crate::tui::app::view::SegMeta { id: 77, rev: 0 }]);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.open_subagent_view(7);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    app.test_renders = 0;
    crate::tui::markdown::WRAP_TAGGED_CALLS.swap(0, Ordering::SeqCst);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.test_renders, 0, "subagent redraw re-rendered rows");
    assert_eq!(
        crate::tui::markdown::WRAP_TAGGED_CALLS.load(Ordering::SeqCst),
        0,
        "subagent redraw wrapped rows"
    );
    app.close_subagent_view();
    app.test_renders = 0;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.test_renders, 0,
        "returning to main reassembled instead of restoring"
    );
    assert_eq!(app.active_subagent, None);
}

/// A press resolved against one layout must not fire at whatever slid
/// under the old row number: the pressed tool is removed before release,
/// so the surviving tool stays collapsed.
#[test]
fn stale_click_target_ignored() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    for name in ["first", "second"] {
        app.push_segment(Segment::Tool {
            call_id: None,
            name: name.into(),
            args: String::new(),
            ok: Some(true),
            output: "out".into(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let abs0 = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(0))
        .expect("first tool header assembled");
    let row = app.last_chat.y + (abs0 - app.chat_top(app.last_chat.height)) as u16;
    let col = app.last_chat.x + 2;
    app.mouse_down(row, col);
    app.remove_segment(0);
    app.mouse_up(row, col);
    assert_eq!(app.segments.len(), 1);
    assert!(
        matches!(
            app.segments.first(),
            Some(Segment::Tool {
                expanded: false,
                flash: None,
                ..
            })
        ),
        "stale press toggled the wrong tool"
    );
}

/// Multi-line selections order whole points: a bottom-up drag keeps the
/// start's column at the start and the end's column at the end.
#[test]
fn selection_orders_whole_points_before_columns() {
    let rev = Selection {
        a: CellPos { row: 12, col: 3 },
        b: CellPos { row: 10, col: 20 },
    };
    let (first, second) = rev.ordered();
    assert!(first.row == 10 && first.col == 20);
    assert!(second.row == 12 && second.col == 3);
    let same_row = Selection {
        a: CellPos { row: 5, col: 9 },
        b: CellPos { row: 5, col: 2 },
    };
    let (first, second) = same_row.ordered();
    assert!(first.row == 5 && first.col == 2);
    assert!(second.row == 5 && second.col == 9);
}

/// Copying strips UI chrome but never real indentation: only exact known
/// decorations go, code indent survives byte-for-byte.
#[test]
fn strip_row_chrome_preserves_code_indent() {
    use super::super::view::strip_row_chrome;
    assert_eq!(strip_row_chrome("    │ body"), "body");
    assert_eq!(strip_row_chrome("│ code"), "code");
    assert_eq!(strip_row_chrome("› hi"), "hi");
    assert_eq!(strip_row_chrome("      indented"), "      indented");
    assert_eq!(strip_row_chrome("  two spaces"), "  two spaces");
    assert_eq!(strip_row_chrome("text │"), "text");
}

/// Hovering an ask option highlights it; moving the mouse out of the
/// chat rectangle clears the highlight instead of saturating onto row 0
/// and lighting up whatever sits on top.
#[test]
fn hover_outside_chat_clears_ask_highlight() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    push_inline_ask(&mut app, ask_fixture());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let ask_idx = app.active_ask_seg().expect("ask must be active");
    let start = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(ask_idx))
        .expect("ask block assembled");
    // header row + question row, then option 0 (lockstep with ask_decode)
    let opt_abs = start + 2;
    let y = app.last_chat.y;
    let h = app.last_chat.height;
    app.mouse_move(y + (opt_abs - app.chat_top(h)) as u16);
    assert!(app.ask_hover.is_some(), "option hover must highlight");
    app.mouse_move(y + h + 1); // input area: below the chat rectangle
    assert!(
        app.ask_hover.is_none(),
        "leaving the chat must clear the highlight"
    );
}

/// Drag selection tracks pressed CONTENT, not a stale row number: rows
/// inserted above the press point (a stream event racing the drag) shift
/// the layout, and the selection start must follow the pressed segment.
#[test]
fn drag_anchor_survives_rows_inserted_above() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("question".into()));
    app.push_segment(Segment::Assistant {
        text: "first answer line with enough text to map columns".into(),
        live: false,
    });
    app.push_segment(Segment::Assistant {
        text: "second answer line also fairly long for column mapping".into(),
        live: false,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let abs_a2 = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(2))
        .expect("second answer assembled");
    let y = app.last_chat.y;
    let x = app.last_chat.x;
    app.mouse_down(
        y + (abs_a2 - app.chat_top(app.last_chat.height)) as u16,
        x + 2,
    );
    // a stream event lands above the press point before the drag continues
    app.insert_segment(0, Segment::User("new first".into()));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let new_a2 = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(3))
        .expect("second answer shifted");
    assert!(new_a2 > abs_a2, "insert must shift rows down");
    // drag within the same (shifted) row, wide column so it counts as a drag
    let h = app.last_chat.height;
    app.mouse_drag(y + (new_a2 - app.chat_top(h)) as u16, x + 40);
    let sel = app.sel.expect("drag must select");
    let (first, _) = sel.ordered();
    assert_eq!(
        first.row, new_a2,
        "selection start must track the pressed content"
    );
}

/// Toggling a tool whose output carries terminal controls (tabs from
/// `read`'s `{:>6}\tline` rows, `\r` progress bars) must never leak
/// them into terminal cells: raw controls desync ratatui's cursor model
/// and spray neighbor-row fragments in both toggle directions.
#[test]
fn toggled_tool_with_tabbed_output_leaves_no_control_cells() {
    fn assert_clean_buffer(terminal: &Terminal<TestBackend>, when: &str) {
        let buffer = terminal.backend().buffer();
        for cell in buffer.content.iter() {
            assert!(
                !cell.symbol().contains(['\t', '\r']),
                "{when}: control cell {:?}",
                cell.symbol()
            );
        }
    }
    fn screen_text(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol())
                            .unwrap_or_default()
                            .to_string()
                    })
                    .collect()
            })
            .collect()
    }
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::Assistant {
        text: "Проверь файл и кратко ответь.".into(),
        live: false,
    });
    let tool_idx = app.segments.len();
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "Cargo.toml".into(),
        ok: Some(true),
        output: "     1\t[package]\n     2\tname = \"sqwai\"\nprogress 50%\rprogress 100%"
            .into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_clean_buffer(&terminal, "collapsed");
    // expand: numbered rows keep number, tab stop, and text, in order
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(tool_idx))
        .expect("tool header assembled");
    app.click(header);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_clean_buffer(&terminal, "expanded");
    let rows = screen_text(&terminal);
    // tab stops are absolute to the row start: the 6-cell tool rail
    // pushes the content tab to column 16, i.e. four spaces here
    assert!(
        rows.iter().any(|r| r.contains("1    [package]")),
        "tab stop lost: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("progress 100%")),
        "carriage overwrite lost: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("50%")),
        "overwritten progress survived: {rows:?}"
    );
    // collapse again: the dialog keeps no tool fragments either
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(tool_idx))
        .expect("tool header survives expand");
    app.click(header);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_clean_buffer(&terminal, "re-collapsed");
    let rows = screen_text(&terminal);
    assert!(
        !rows.iter().any(|r| r.contains("[package]")),
        "collapsed tool body visible: {rows:?}"
    );
}

/// A resize drag repaints but never re-renders: the width rebuild waits
/// until no resize event arrived for the settle window, then runs once.
#[test]
fn resize_defers_full_rebuild_until_size_settles() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("q".into()));
    app.push_segment(Segment::Assistant {
        text: "a".into(),
        live: false,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.cache_w, 78);
    // drag in flight: repaint only, stale width kept for the retry
    app.last_resize = Some(std::time::Instant::now());
    app.test_renders = 0;
    terminal.backend_mut().resize(100, 24);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.test_renders, 0, "resize drag must not re-render");
    assert_eq!(app.cache_w, 78, "width rebuild must wait for settle");
    assert!(app.defer_rebuild);
    // settled: the same draw rebuilds everything once
    app.last_resize = None;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(app.test_renders > 0, "settled resize must rebuild");
    assert_eq!(app.cache_w, 98);
    assert!(!app.defer_rebuild);
}

/// Subagent summary rows belong to the call site: with a live answer
/// slot open, two spawns must land before it (in spawn order) — never
/// appended after the finished answer as stray `✓ subagent-N` lines.
#[test]
fn subagent_rows_land_at_call_site_not_after_answer() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("q".into()));
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.handle_subagent_start(1, "t1".into());
    app.handle_subagent_start(2, "t2".into());
    // the answer streams into the live slot afterwards
    let live = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Assistant { live: true, .. }))
        .expect("live slot survives");
    if let Some(Segment::Assistant { text, live }) = app.segments.get_mut(live) {
        *text = "done".to_string();
        *live = false;
    }
    app.touch_segment(live);
    let kinds: Vec<&str> = app
        .segments
        .iter()
        .map(|s| match s {
            Segment::User(_) => "user",
            Segment::Assistant { .. } => "answer",
            Segment::Subagent { .. } => "sub",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["user", "sub", "sub", "answer"]);
}

#[test]
fn test_node_badge_method_and_memory() {
    use crate::agent::graph::NodeKind;
    use crate::tui::app::menus::node_badge;

    assert_eq!(node_badge(&NodeKind::Method), "[meth]");
    assert_eq!(node_badge(&NodeKind::Memory), "[mem]");
    assert_eq!(node_badge(&NodeKind::Function), "[fn]");
    assert_eq!(node_badge(&NodeKind::Struct), "[st]");
    assert_eq!(node_badge(&NodeKind::Enum), "[e]");
}

