use super::*;

#[test]

fn debug_enter_updates_visible_value() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Debug);
    let idx = app
        .menu_rows
        .iter()
        .position(|(_, action)| matches!(action, MenuAction::ToggleHttpLog))
        .expect("http debug row");
    app.menu_sel = idx;
    let before = app.cfg.ui.http_log;
    app.menu_activate();
    assert_eq!(app.cfg.ui.http_log, !before);
    assert!(matches!(app.menu_rows[idx].1, MenuAction::ToggleHttpLog));
    let rendered: String = app.menu_rows[idx]
        .0
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(rendered.contains(if !before { "on" } else { "off" }));
}

#[test]
fn debug_perf_log_toggle_writes_frame_lines_to_temp_file() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Debug);
    let find_toggle = |app: &App| {
        app.menu_rows
            .iter()
            .position(|(_, action)| matches!(action, MenuAction::TogglePerfLog))
            .expect("perf log row")
    };
    app.menu_sel = find_toggle(&app);
    assert!(!app.perf.enabled());
    app.menu_activate();
    assert!(app.perf.enabled());
    let path = app.perf.path().to_string();
    assert!(path.contains("sqwai-perf"), "temp file path: {path}");
    app.perf.frame(
        super::super::perf::FrameStat {
            draw_us: 10,
            rebuild_us: 5,
            bytes: 2048,
            flush_us: 7,
            pace_us: 8333,
            render_us: 900,
            latency_us: 1200,
            dropped: 2,
            merge: "splice",
            fresh: 1,
            segs: 2,
            rows: 3,
            tick: 7,
            streaming: true,
            running: false,
            view: 0,
        },
        4,
        9,
    );
    app.perf.event("tool_start read");
    app.menu_sel = find_toggle(&app);
    app.menu_activate();
    assert!(!app.perf.enabled());
    let content = std::fs::read_to_string(&path).expect("log file written");
    assert!(
        content.contains("# frame t_ms draw_us"),
        "header:\n{content}"
    );
    assert!(
        content.lines().any(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            parts.len() > 6
                && parts[0] == "1"
                && parts[2] == "10"
                && parts[3] == "5"
                && parts[4] == "2048"
                && parts[5] == "7"
                && parts[6] == "8333"
                && parts[7] == "900"
                && parts[8] == "1200"
                && parts[9] == "2"
                && parts[10] == "splice"
        }),
        "frame line:\n{content}"
    );
    assert!(content.contains("EVT"), "event line:\n{content}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn error_status_shows_in_bar() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.status("provider boom", StatusKind::Err);
    let spans = app.status_bar_spans(120);
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("provider boom"), "bar: {text}");
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Status { .. })),
        "toast never lands in the chat"
    );
}

#[test]
fn toast_replaces_previous_notice() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.status("first", StatusKind::Info);
    app.status("second", StatusKind::Warn);
    assert_eq!(toast_text(&app), "second", "new notice wins outright");
    assert!(
        app.toast
            .as_ref()
            .is_some_and(|t| t.kind == StatusKind::Warn)
    );
    // expired toasts vanish from the bar
    app.toast.as_mut().expect("toast").until =
        std::time::Instant::now() - std::time::Duration::from_secs(1);
    let spans = app.status_bar_spans(120);
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!text.contains("second"), "expired toast hidden: {text}");
    assert!(app.toast.is_none(), "expired toast dropped");
}

#[test]
fn status_bar_summarizes_subagents_and_abort_cancels_running_children() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.subagents
        .push((1, "one".into(), "running".into(), String::new(), false));
    app.subagents
        .push((2, "two".into(), "completed".into(), "done".into(), false));
    let text: String = app
        .status_bar_spans(160)
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("agents:1/2"), "bar: {text}");
    app.push_segment(Segment::Subagent {
        id: 1,
        task: "one".into(),
        status: "running".into(),
        output: String::new(),
        expanded: false,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "subagent".into(),
        args: "2 tasks".into(),
        ok: None,
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.clear_subagent_ui_on_stop();
    assert!(app.subagents.is_empty());
    assert!(!app.segments.iter().any(|segment| {
        matches!(segment, Segment::Subagent { .. })
            || matches!(segment, Segment::Tool { name, .. } if name == "subagent")
    }));
}

#[test]
fn status_bar_hides_completed_subagents() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.subagents
        .push((1, "one".into(), "completed".into(), "done".into(), false));
    app.subagents
        .push((2, "two".into(), "completed".into(), "done".into(), false));
    let text: String = app
        .status_bar_spans(160)
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(!text.contains("agents:"), "stale label must go: {text}");
    // failures still show — a collapsed block must not hide breakage
    app.subagents
        .push((3, "three".into(), "failed".into(), "err".into(), false));
    let text: String = app
        .status_bar_spans(160)
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("1 failed"), "bar: {text}");
}

#[test]
fn typewriter_reveals_gradually_and_drains() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.pending_reveal = "Hello".into();
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    assert!(app.reveal_chars(2));
    assert_eq!(app.assistant_buf, "He");
    assert_eq!(live_assistant_text(&app), "He");
    assert!(app.reveal_chars(2));
    assert_eq!(app.assistant_buf, "Hell");
    assert_eq!(live_assistant_text(&app), "Hell");
    assert!(app.reveal_chars(usize::MAX));
    assert_eq!(app.assistant_buf, "Hello");
    assert_eq!(live_assistant_text(&app), "Hello");
    assert!(!app.reveal_chars(10), "empty queue must report no progress");
}

fn live_assistant_text(app: &App) -> String {
    app.segments
        .iter()
        .rev()
        .find_map(|s| match s {
            Segment::Assistant { text, live: true } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn aborted_answer_is_kept_in_history() {
    use crate::providers::Role;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(Role::User, "write a poem");
    app.streaming = true;
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.assistant_buf = "partial answer".into();

    app.finish_turn(Err("aborted".into()));

    assert!(!app.streaming);
    let last = app.session.messages.last().expect("assistant kept");
    assert_eq!(
        last.content, "partial answer",
        "aborted partial must be saved"
    );
}

#[test]
fn aborted_does_not_duplicate_prior_answer() {
    use crate::providers::Role;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // a previously completed turn: user + assistant answer (both in the
    // session and in the visible transcript)
    app.session.push(Role::User, "hello");
    app.session.push(Role::Assistant, "hello!");
    app.push_segment(Segment::User("hello".into()));
    app.push_segment(Segment::Assistant {
        text: "hello!".into(),
        live: false,
    });
    app.streaming = true;
    app.push_segment(Segment::User("how are you".into()));
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    // nothing streamed yet for the new turn
    app.assistant_buf = String::new();

    app.finish_turn(Err("aborted".into()));

    assert!(!app.streaming);
    // the prior answer must not be backfilled into the stopped turn
    let answers: Vec<&str> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let dupes = answers.iter().filter(|a| *a == &"hello!").count();
    assert_eq!(
        dupes, 1,
        "abort must not duplicate the prior answer: {answers:?}"
    );
    // and the empty live slot for the new turn must be dropped, not kept
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Assistant { live: true, .. })),
        "empty live slot should be removed on abort"
    );
}

#[test]
fn cancelled_tool_turn_does_not_backfill_prior_answer() {
    use crate::agent::loop_task::AgentOutcome;
    use crate::providers::{Message, Role, ToolCallReq};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // prior completed turn, on screen and in history
    app.session.push(Role::User, "hello");
    app.session.push(Role::Assistant, "hello!");
    app.push_segment(Segment::User("hello".into()));
    app.push_segment(Segment::Assistant {
        text: "hello!".into(),
        live: false,
    });
    // new turn: tool started, Esc cancelled it before any answer text
    // streamed — the loop ends cooperatively with Ok and no new
    // assistant message, only tool traffic
    app.streaming = true;
    app.session.push(Role::User, "run it");
    app.push_segment(Segment::User("run it".into()));
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.finish_turn_ok(AgentOutcome {
        messages: vec![
            Message::new(Role::User, "hello"),
            Message::new(Role::Assistant, "hello!"),
            Message::new(Role::User, "run it"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({}),
            )]),
            Message::tool_result("c1", "cancelled", true),
        ],
        summary: None,
        todos: Vec::new(),
        plan_todos: Vec::new(),
        journal: Vec::new(),
    });

    assert!(!app.streaming);
    let answers: Vec<&str> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        answers.iter().filter(|a| *a == &"hello!").count(),
        1,
        "cancel must not stamp the prior answer into the new turn: {answers:?}"
    );
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Assistant { live: true, .. })),
        "empty live slot should be removed on cancel"
    );
}

#[test]
fn cancelled_tool_turn_keeps_streamed_partial() {
    use crate::agent::loop_task::AgentOutcome;
    use crate::providers::{Message, Role, ToolCallReq};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(Role::User, "hello");
    app.session.push(Role::Assistant, "hello!");
    app.push_segment(Segment::User("hello".into()));
    app.push_segment(Segment::Assistant {
        text: "hello!".into(),
        live: false,
    });
    // cancel landed after some post-tool text streamed: the revealed
    // partial is kept and preserved, the prior answer stays single
    app.streaming = true;
    app.session.push(Role::User, "run it");
    app.push_segment(Segment::User("run it".into()));
    app.push_segment(Segment::Assistant {
        text: "partial…".into(),
        live: true,
    });
    app.assistant_buf = "partial…".into();
    app.finish_turn_ok(AgentOutcome {
        messages: vec![
            Message::new(Role::User, "hello"),
            Message::new(Role::Assistant, "hello!"),
            Message::new(Role::User, "run it"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({}),
            )]),
            Message::tool_result("c1", "cancelled", true),
        ],
        summary: None,
        todos: Vec::new(),
        plan_todos: Vec::new(),
        journal: Vec::new(),
    });

    let answers: Vec<&str> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, vec!["hello!", "partial…"], "{answers:?}");
    let last = app.session.messages.last().expect("partial kept");
    assert_eq!(last.content, "partial…");
}

#[test]
fn finish_turn_ok_rebases_notes_after_compaction() {
    use crate::agent::loop_task::AgentOutcome;
    use crate::providers::{Message, Role};
    use crate::session::TurnNote;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    for (user, answer) in [("u1", "a1"), ("u2", "a2"), ("u3", "a3")] {
        app.session.push(Role::User, user);
        app.session.push(Role::Assistant, answer);
    }
    for (idx, text) in [(0, "n0"), (2, "n2"), (4, "n4")] {
        app.session.turn_notes.push(TurnNote {
            user_index: idx,
            text: text.into(),
            is_error: false,
        });
    }
    // compaction dropped the first two turns, kept the third
    app.finish_turn_ok(AgentOutcome {
        messages: vec![
            Message::new(Role::User, "summary"),
            Message::new(Role::User, "u3"),
            Message::new(Role::Assistant, "a3"),
        ],
        summary: Some("summary".into()),
        todos: Vec::new(),
        plan_todos: Vec::new(),
        journal: Vec::new(),
    });
    let notes: Vec<usize> = app
        .session
        .turn_notes
        .iter()
        .map(|n| n.user_index)
        .collect();
    assert_eq!(notes, vec![0, 0, 1], "{notes:?}");
}

#[test]
fn subagents_menu_opens_read_only_chat() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.subagents.push((
        7,
        "inspect rendering".into(),
        "running".into(),
        "read view.rs".into(),
        false,
    ));
    app.open_menu(Menu::Subagents);
    assert!(
        app.menu_rows
            .iter()
            .any(|(_, action)| matches!(action, MenuAction::OpenSubagent(7)))
    );
    app.run_action(MenuAction::OpenSubagent(7));
    assert!(app.cur_menu().is_none());
    assert_eq!(app.active_subagent, Some(7));
}

fn open_sub_chat(app: &mut App, id: u64, status: &str) {
    use crate::tui::app::view::SegMeta;
    app.subagent_chats.insert(
        id,
        vec![
            Segment::User("do research".into()),
            Segment::Thinking {
                text: "hmm".into(),
                expanded: false,
                started: None,
                duration_ms: 0,
                live: false,
            },
            Segment::Tool {
                name: "read".into(),
                args: "a.rs".into(),
                call_id: None,
                ok: Some(true),
                output: "contents".into(),
                diff: None,
                preview: vec!["contents".into()],
                preview_total: 1,
                expanded: false,
                flash: None,
            },
            Segment::Assistant {
                text: "found it".into(),
                live: false,
            },
        ],
    );
    app.subagent_meta
        .insert(id, (771..775).map(|n| SegMeta { id: n, rev: 0 }).collect());
    app.subagents.push((
        id,
        "do research".into(),
        status.into(),
        String::new(),
        false,
    ));
    app.active_subagent = Some(id);
}

#[test]
fn stale_acceptance_announces_one_durable_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    let plan: crate::plan::Plan = serde_json::from_str(
        r#"{"version":1,"id":"p","status":"active","created":"t",
            "goal":{"text":"g","source":"user","created":"t"},
            "budget":{"tokens":0,"limit":0},"revision":1,"steps":[],
            "acceptance":[
              {"text":"cmd: a","status":"pending","validation":{"status":"passed"}},
              {"text":"cmd: b","status":"pending","validation":{"status":"stale"}}
            ]}"#,
    )
    .expect("test plan must parse");
    app.announce_stale_rows(Some(plan.clone()));
    let rows: Vec<String> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Status { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec!["acceptance 1 went stale — re-verify".to_string()]
    );
    // second call: silence (already announced)
    let n = app.segments.len();
    app.announce_stale_rows(Some(plan));
    assert_eq!(app.segments.len(), n);
    // no plan: set clears, nothing pushed
    app.announce_stale_rows(None);
    assert!(app.announced_stale.is_empty());
}

#[test]
fn sub_group_folds_finished_chat_like_main() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    app.finalize_sub_group(7);
    let groups = app.sub_groups.get(&7).expect("folded group");
    assert_eq!(groups.len(), 1);
    assert_eq!((groups[0].seg_start, groups[0].seg_end), (1, 3));
    assert!(!groups[0].expanded, "ok child folds shut");
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("activity"), "header shows:\n{s}");
    assert!(s.contains("found it"), "answer stays visible:\n{s}");
    assert!(!s.contains("a.rs"), "tool row folds away:\n{s}");
}

#[test]
fn sub_group_header_click_toggles() {
    use crate::tui::app::view::GROUP_BASE;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    app.finalize_sub_group(7);
    render_to_string(&mut app, 100, 30);
    let abs = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("header row");
    app.click(abs);
    assert!(
        app.sub_groups[&7][0].expanded,
        "header click unfolds the child turn"
    );
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("a.rs"), "tool row is back:\n{s}");
}

#[test]
fn close_subagent_view_folds_expanded_child_groups() {
    use crate::tui::app::view::GROUP_BASE;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    app.finalize_sub_group(7);
    render_to_string(&mut app, 100, 30);
    // unfold while viewing, then close the chat
    let abs = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("header row");
    app.click(abs);
    assert!(app.sub_groups[&7][0].expanded);
    app.close_subagent_view();
    assert!(
        !app.sub_groups[&7][0].expanded,
        "closing the chat folds its groups shut"
    );
    assert_eq!(app.active_subagent, None);
}

#[test]
fn sub_group_live_while_running() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "running");
    app.sub_started.insert(7, std::time::Instant::now());
    // reopen the tool row: the running turn is still open
    if let Some(Segment::Tool { ok, .. }) = app
        .subagent_chats
        .get_mut(&7)
        .and_then(|chat| chat.get_mut(2))
    {
        *ok = None;
    }
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("activity"), "live header shows:\n{s}");
    assert!(s.contains("a.rs"), "running rows stay visible:\n{s}");
}

#[test]
fn subagent_chat_click_expands_its_tool_without_switching_chat() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.active_subagent = Some(7);
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
    app.cache_rowseg = vec![Some(0)];
    app.click(0);
    assert_eq!(app.active_subagent, Some(7));
    assert!(matches!(
        app.subagent_chats.get(&7).and_then(|chat| chat.first()),
        Some(Segment::Tool { expanded: true, .. })
    ));
}

#[test]
fn new_session_clears_active_subagent_and_subagent_chats() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.active_subagent = Some(7);
    app.subagents
        .push((7, "child".into(), String::new(), String::new(), false));
    app.start_new_session();
    assert_eq!(app.active_subagent, None);
    assert!(app.subagents.is_empty());
    assert!(app.subagent_chats.is_empty());
}

#[test]
fn effort_levels_include_off_for_status_bar_and_model_settings() {
    assert!(EffortLevel::SELECTABLE.contains(&EffortLevel::Off));
    assert_eq!(EffortLevel::SELECTABLE, EffortLevel::ALL);
}

#[test]
fn mode_chip_matches_mode_when_settled() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // settled ACT: the classic yellow chip
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[0].content.as_ref(), " ACT ");
    assert_eq!(spans[0].style, crate::tui::theme::Theme::status_chip());
    // settled PLAN: blue chip (blend force-expired, no timing involved)
    app.set_mode(Mode::Plan);
    app.mode_blend = Some((
        super::super::MODE_PLAN_RGB,
        std::time::Instant::now() - std::time::Duration::from_secs(5),
    ));
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[0].content.as_ref(), " PLAN ");
    assert_eq!(spans[0].style, crate::tui::theme::Theme::mode_chip_plan());
}

#[test]
fn mode_toggle_rearms_blend_instead_of_breaking() {
    // rapid toggles redirect the sweep from the displayed color: the
    // blend stays armed and the mode always wins
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.set_mode(Mode::Plan);
    assert_eq!(app.mode, Mode::Plan);
    assert!(app.mode_blend.is_some(), "toggle arms the sweep");
    app.set_mode(Mode::Act);
    assert_eq!(app.mode, Mode::Act);
    assert!(
        app.mode_blend.is_some(),
        "second toggle re-arms, never drops"
    );
}

/// §5.1: the status bar shows the *effective* mapping. Selecting `max` on
/// a model whose API stops at `high` used to read `th:max`, which claimed
/// work that was never requested.
#[test]
fn the_status_bar_reports_the_sent_level_verbatim() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.model_cfg.effort = EffortLevel::Max;

    // transparent slider: max goes on the wire as max, no clamp note
    app.model_cfg.effort_control = Some(crate::config::EffortControl::Named);
    let text: String = app
        .status_bar_spans(120)
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("ef:max") && !text.contains("ef:max→"),
        "status bar: {text:?}"
    );

    // and a level the model will not act on is marked, not implied
    app.model_cfg.effort_control = Some(crate::config::EffortControl::None);
    let text: String = app
        .status_bar_spans(120)
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("ef:max (ignored)"), "status bar: {text:?}");
}

/// The effort menu is the one place with room for the reason, so it must
/// carry it for every level rather than only for the selected one.
#[test]
fn the_effort_menu_annotates_every_level_it_offers() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort_control = Some(crate::config::EffortControl::Named);
    app.open_menu(crate::tui::app::menus::Menu::Effort);
    let rows: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect();
    let all = rows.join("\n");
    assert_eq!(rows.len(), EffortLevel::SELECTABLE.len(), "{all}");
    // transparent slider: every named level lands, so no clamp notes
    assert!(
        all.contains("max") && !all.contains("sent as"),
        "no level must be marked as clamped: {all}"
    );
    assert!(all.contains("xhigh"), "xhigh must be offered: {all}");
    assert!(
        !all.contains("low  →"),
        "levels that land must carry no note: {all}"
    );
}

#[test]
fn effort_slider_opens_on_current_level_with_hits() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort = EffortLevel::High;
    app.open_menu(crate::tui::app::menus::Menu::Effort);
    // slider opens on the current level, not on `off`
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::High)
        .unwrap();
    assert_eq!(app.menu_sel, want);
    // render the slider card: 6 click targets, one per level
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
    assert!(app.menu_rect.width > 0 && app.menu_rect.height > 0);
    // progress dots: High is index 3, so 4 filled dots, 2 hollow
    let dots: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert_eq!(dots.matches('●').count(), want + 1, "{dots:?}");
    assert_eq!(
        dots.matches('○').count(),
        EffortLevel::SELECTABLE.len() - want - 1
    );
    // hover the max dot previews nothing (no mutation without click)
    let (_, max_idx) = app.effort_hits.last().copied().expect("max hit");
    let (rect, _) = app.effort_hits[max_idx];
    assert_eq!(app.effort_index_at(rect.y, rect.x + 1), Some(max_idx));
    assert_eq!(app.menu_sel, want, "hover must not move the slider");
    // click commits it
    app.menu_sel = max_idx;
    app.menu_activate();
    assert_eq!(app.model_cfg.effort, EffortLevel::Max);
}

#[test]
fn effort_narrow_fallback_list_hovers_and_clicks_by_row() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort = EffortLevel::Low;
    app.open_menu(crate::tui::app::menus::Menu::Effort);
    // narrow terminal: slider card stays off, plain row list instead
    let area = Rect::new(0, 0, 40, 20);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    assert!(app.effort_hits.is_empty(), "slider must not render narrow");
    // hover the first row moves the selection (unlike the slider card)
    let first_row = app.menu_rect.y + 1;
    assert_eq!(app.menu_hover(first_row), Some(0));
    assert_eq!(app.menu_sel, 0);
    // click commits the row's level and closes the menu
    app.menu_click(first_row);
    assert_eq!(app.model_cfg.effort, EffortLevel::Off);
    assert!(app.menu_stack.is_empty(), "menu must close on commit");
}

#[test]
fn effort_slider_colors_span_gray_to_magenta() {
    use crate::tui::theme::Theme;
    use ratatui::style::Color;
    assert_eq!(Theme::effort_color(EffortLevel::Off), Color::DarkGray);
    assert_eq!(Theme::effort_color(EffortLevel::Max), Color::Magenta);
    // every level gets a distinct color
    let mut seen = std::collections::HashSet::new();
    for lvl in EffortLevel::SELECTABLE {
        assert!(seen.insert(Theme::effort_color(lvl)), "{lvl:?}");
    }
}

#[test]
fn command_popup_contains_only_command_names() {
    assert!(COMMANDS.iter().all(|command| !command.contains(' ')));
    assert!(COMMANDS.contains(&"/undo"));
    assert!(COMMANDS.contains(&"/skill"));
    assert!(COMMANDS.contains(&"/graph-rebuild"));
}
#[test]
fn help_menu_is_empty() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(COMMANDS.contains(&"/help"));
    app.command("help");
    assert!(matches!(app.cur_menu(), Some(Menu::Help)));
    assert!(app.menu_rows.is_empty());
}

#[test]
fn test_command_opens_animation_gallery_with_live_rows() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(COMMANDS.contains(&"/test"));
    app.cfg.ui.experimental_test = true;
    app.command("test animations");
    assert!(
        matches!(app.cur_menu(), Some(Menu::TestAnims)),
        "/test animations must open the gallery"
    );
    assert_eq!(
        app.menu_rows.len(),
        crate::tui::spinners::ALL.len(),
        "one row per catalog entry"
    );
    // smoke-render: live frames paint without panic, names visible
    app.spinner_tick = 7;
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("braille-classic"), "{text:?}");
    assert!(text.contains(" Test "), "{text:?}");
    // scroll to the shimmer row: keyboard nav pulls the window after it
    let shim = crate::tui::spinners::ALL
        .iter()
        .position(|e| e.name == "shimmer-live")
        .expect("shimmer row");
    while app.menu_sel < shim {
        app.menu_nav(1);
    }
    assert_eq!(app.menu_sel, shim);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let text: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("shimmer-live"), "{text:?}");
    assert!(text.contains("Working"), "{text:?}");
    // new rows paint too: a tint shimmer, a flux preset, the wave
    for probe in ["shimmer-ocean", "flux-dice", "flux-wave-wide"] {
        let pos = crate::tui::spinners::ALL
            .iter()
            .position(|e| e.name == probe)
            .expect("gallery row");
        while app.menu_sel < pos {
            app.menu_nav(1);
        }
        assert_eq!(app.menu_sel, pos);
        let mut buf = Buffer::empty(area);
        app.draw_menu(&mut buf, area);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains(probe), "{probe} not visible: {text:?}");
        if probe.starts_with("shimmer-") {
            assert!(text.contains("Working"), "{probe} lost its demo text");
        }
    }
    // Enter closes the gallery again
    app.menu_activate();
    assert!(app.menu_stack.is_empty(), "gallery must close on commit");
}

#[test]
fn test_animations_gated_behind_experimental_flag() {
    // off by default: unknown command, hidden from both popup levels
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(!app.cfg.ui.experimental_test, "flag defaults to off");
    app.command("test animations");
    assert!(
        app.menu_stack.is_empty(),
        "gated command must not open anything"
    );
    app.input = App::fresh_input("/test".into());
    assert!(
        !app.popup_items().iter().any(|i| i.starts_with("/test")),
        "level-1 popup must hide /test"
    );
    app.input = App::fresh_input("/test ".into());
    assert!(
        app.popup_items().is_empty(),
        "level-2 popup must offer nothing, got {:?}",
        app.popup_items()
    );
    // on: full command works and both popup levels list it
    app.cfg.ui.experimental_test = true;
    app.input = App::fresh_input("/test".into());
    assert!(app.popup_items().contains(&"/test".to_string()));
    app.input = App::fresh_input("/test ".into());
    assert_eq!(app.popup_items(), vec!["/test animations".to_string()]);
    app.command("test animations");
    assert!(matches!(app.cur_menu(), Some(Menu::TestAnims)));
    // bare /test with the flag on hints at the subcommand
    let mut app2 = test_app("http://127.0.0.1:9/v1".into());
    app2.cfg.ui.experimental_test = true;
    app2.command("test");
    assert!(app2.menu_stack.is_empty(), "bare /test opens nothing");
}

fn queue_test_app() -> crate::tui::app::App {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app
}

#[test]
fn submit_mid_turn_queues_instead_of_sending() {
    let mut app = queue_test_app();
    app.streaming = true;
    app.input = App::fresh_input("do it after".into());
    app.submit();
    assert_eq!(app.pending_queue, vec!["do it after".to_string()]);
    assert!(
        !app.segments.iter().any(|s| matches!(s, Segment::User(_))),
        "nothing sent while streaming"
    );
    assert_eq!(app.input_text(), "", "input cleared like a submit");
    assert!(toast_text(&app).contains("queued"), "queued toast shown");
    // a second message keeps FIFO order
    app.input = App::fresh_input("and this too".into());
    app.submit();
    assert_eq!(app.pending_queue.len(), 2);
}

#[test]
fn submit_mid_turn_commands_are_not_queued() {
    let mut app = queue_test_app();
    app.streaming = true;
    // non-whitelisted command: busy notice, nothing queued
    app.input = App::fresh_input("/undo".into());
    app.submit();
    assert!(app.pending_queue.is_empty());
    assert_eq!(toast_text(&app), App::BUSY_STATUS);
    // whitelisted command: runs immediately, nothing queued
    app.input = App::fresh_input("/help".into());
    app.submit();
    assert!(app.pending_queue.is_empty());
    assert!(matches!(app.cur_menu(), Some(Menu::Help)));
}

#[tokio::test]
async fn natural_finish_sends_first_queued_message() {
    use crate::providers::Role;
    let mut app = queue_test_app();
    app.session.push(Role::User, "first");
    app.turn_user_index = Some(0);
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.streaming = true;
    app.pending_queue = vec!["queued one".into(), "queued two".into()];
    app.finish_turn(Ok(()));
    assert_eq!(app.pending_queue, vec!["queued two".to_string()]);
    assert!(
        app.segments.iter().any(|s| matches!(
            s,
            Segment::User(t) if t == "queued one"
        )),
        "front message sent as a new turn"
    );
    assert!(app.streaming, "new turn started");
}

#[tokio::test]
async fn failed_turn_keeps_queue_for_manual_resend() {
    use crate::providers::Role;
    let mut app = queue_test_app();
    app.session.push(Role::User, "first");
    app.turn_user_index = Some(0);
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.streaming = true;
    app.pending_queue = vec!["stays".into()];
    app.finish_turn(Err("provider offline".into()));
    assert_eq!(app.pending_queue, vec!["stays".to_string()]);
    assert!(!app.streaming);
    // empty Enter resends it once idle
    app.input = App::fresh_input(String::new());
    app.submit();
    assert!(app.pending_queue.is_empty(), "resent");
    assert!(
        app.segments.iter().any(|s| matches!(
            s,
            Segment::User(t) if t == "stays"
        )),
        "manual resend works"
    );
}

#[test]
fn session_switch_drops_the_queue() {
    use crate::session::Session;
    let mut app = queue_test_app();
    app.pending_queue = vec!["stale".into()];
    app.apply_session(Session::new("m".into(), 1000));
    assert!(app.pending_queue.is_empty());
    app.pending_queue = vec!["stale".into()];
    app.startup = false; // apply_session raised the startup screen
    app.start_new_session();
    assert!(app.pending_queue.is_empty());
}

#[test]
fn queue_preview_renders_above_input() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = queue_test_app();
    app.pending_queue = vec!["fix the typo".into(), "and docs".into()];
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    let lines: Vec<String> = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    let input_y = app.last_input.y as usize;
    assert!(
        lines[input_y - 1].contains("queued (2): fix the typo"),
        "preview sits right above input: {:?}",
        lines[input_y - 1]
    );
    assert!(lines[input_y - 1].contains("[+1 more]"));
}

