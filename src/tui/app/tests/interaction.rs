use super::*;

#[test]
fn a_history_marker_resolves_to_the_image_target() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut image = crate::media::test_image(8, 8, "shot.png");
    image.marker = Some(1);
    app.session.messages = vec![
        crate::providers::Message::new(
            crate::providers::Role::User,
            "look [image 1]",
        )
        .with_images(vec![image.clone()]),
    ];
    app.push_segment(Segment::User("look [image 1]".into()));

    let line = "› look [image 1]";
    let marker_col = line.chars().position(|c| c == '[').unwrap();
    match app.image_target_at(0, line, marker_col) {
        Some(crate::tui::app::view::ClickTarget::Image {
            name,
            id,
            media_type,
        }) => {
            assert_eq!(name, "shot.png");
            assert_eq!(id, image.id);
            assert_eq!(media_type, "image/png");
        }
        _ => panic!("marker click must resolve to the image target"),
    }
    assert!(
        app.image_target_at(0, line, 0).is_none(),
        "clicking the message prefix does nothing"
    );

    app.cfg.ui.open_attachments = false;
    assert!(
        app.image_target_at(0, line, marker_col).is_none(),
        "open_attachments=false disables the marker target"
    );
}

#[test]
fn a_missing_blob_is_reported_not_opened() {
    let error = crate::media::open_blob(&"0".repeat(64), "image/png", "gone.png")
        .expect_err("a missing blob must not launch a viewer");
    assert!(error.contains("missing"), "{error}");
}

#[test]
fn markers_renumber_in_text_order() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut one = crate::media::test_image(8, 8, "one.png");
    one.marker = Some(1);
    let mut two = crate::media::test_image(9, 9, "two.png");
    two.marker = Some(2);
    let mut three = crate::media::test_image(10, 10, "three.png");
    three.marker = Some(3);
    app.pending_images = vec![one, two, three];
    app.input = App::fresh_input("[image 3] x [image 1] y [image 2]".to_string());

    app.renumber_markers();

    assert_eq!(app.input_text(), "[image 1] x [image 2] y [image 3]");
    let marker = |name: &str| {
        app.pending_images
            .iter()
            .find(|image| image.name == name)
            .unwrap()
            .marker
    };
    assert_eq!(marker("three.png"), Some(1));
    assert_eq!(marker("one.png"), Some(2));
    assert_eq!(marker("two.png"), Some(3));
    assert_eq!(app.image_marker, 3);

    // Every live marker binds an image block, in the same text order.
    let message = crate::providers::Message::new(
        crate::providers::Role::User,
        app.input_text(),
    )
    .with_images(app.pending_images.clone());
    let bound: Vec<String> = crate::providers::user_parts(&message)
        .into_iter()
        .filter_map(|part| match part {
            crate::providers::UserPart::Image(image) => Some(image.name.clone()),
            crate::providers::UserPart::Text(_) => None,
        })
        .collect();
    assert_eq!(bound, ["three.png", "one.png", "two.png"]);
}

#[test]
fn backspace_removes_a_marker_as_one_token_and_renumbers() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut one = crate::media::test_image(8, 8, "one.png");
    one.marker = Some(1);
    let mut two = crate::media::test_image(9, 9, "two.png");
    two.marker = Some(2);
    app.pending_images = vec![one, two];
    app.input = App::fresh_input("A [image 1] B [image 2] C".to_string());
    app.input
        .move_cursor(tui_textarea::CursorMove::Jump(0, 11));

    assert!(app.atomic_erase(true));

    assert_eq!(app.input_text(), "A  B [image 1] C");
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.pending_images[0].name, "two.png");
    assert_eq!(app.pending_images[0].marker, Some(1));
}

#[test]
fn a_selection_over_a_marker_deletes_the_whole_token() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut one = crate::media::test_image(8, 8, "one.png");
    one.marker = Some(1);
    app.pending_images = vec![one];
    app.input = App::fresh_input("[image 1]".to_string());
    app.input
        .move_cursor(tui_textarea::CursorMove::Jump(0, 2));
    app.input.start_selection();
    app.input
        .move_cursor(tui_textarea::CursorMove::Jump(0, 5));

    assert!(app.atomic_erase(true));

    assert_eq!(app.input_text(), "");
    assert!(app.pending_images.is_empty());
}

#[test]
fn a_manual_marker_without_an_image_stays_text() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.input = App::fresh_input("[image 9]".to_string());

    app.renumber_markers();
    app.repair_markers_after_edit();

    assert_eq!(app.input_text(), "[image 9]");
    assert!(app.pending_images.is_empty());
}

#[test]
fn drafted_images_follow_their_markers() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut one = crate::media::test_image(8, 8, "one.png");
    one.marker = Some(1);
    let mut two = crate::media::test_image(9, 9, "two.png");
    two.marker = Some(2);
    app.pending_images = vec![one, two];

    assert!(app.has_marked_images("keep [image 2] only"));
    let images = app.take_submitted_images("keep [image 2] only");
    assert_eq!(images.len(), 1, "the deleted marker drops its image");
    assert_eq!(images[0].marker, Some(2));
    assert!(!app.has_marked_images("[image 3]"));
}

#[test]
fn a_dropped_absolute_path_typed_as_text_becomes_a_marker() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.png");
    std::fs::write(&path, crate::media::test_png(12, 6)).unwrap();

    // Terminals that do not emit a paste event type the path character by
    // character, usually quoted.
    app.input = App::fresh_input(format!("\"{}\"", path.display()));
    app.maybe_attach_dropped_path();
    assert_eq!(app.input_text(), "[image 1]");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.pending_images.is_empty() && std::time::Instant::now() < deadline {
        app.poll_image_jobs();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.pending_images[0].name, "dropped.png");
}

#[test]
fn a_pasted_path_becomes_a_marker_and_then_an_attachment() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let dir = tempfile::tempdir().unwrap();
    app.project_root = dir.path().to_path_buf();
    std::fs::write(dir.path().join("shot.png"), crate::media::test_png(20, 10)).unwrap();

    app.paste_input_text("\"shot.png\"");
    assert_eq!(app.input_text(), "[image 1]");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.pending_images.is_empty() && std::time::Instant::now() < deadline {
        app.poll_image_jobs();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(app.pending_images.len(), 1);
    assert_eq!(app.pending_images[0].marker, Some(1));
    assert_eq!(app.pending_images[0].name, "shot.png");
}

#[test]
fn a_paste_attaches_only_when_every_word_is_an_image_path() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let dir = tempfile::tempdir().unwrap();
    app.project_root = dir.path().to_path_buf();
    std::fs::write(dir.path().join("shot.png"), crate::media::test_png(8, 8)).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "x").unwrap();

    assert_eq!(app.pasted_image_paths("shot.png").unwrap().len(), 1);
    assert!(
        app.pasted_image_paths("look at shot.png").is_none(),
        "prose with a path is ordinary text"
    );
    assert!(
        app.pasted_image_paths("notes.txt").is_none(),
        "only image files qualify"
    );

    let spaced = dir.path().join("a b");
    std::fs::create_dir_all(&spaced).unwrap();
    let spaced_file = spaced.join("with space.png");
    std::fs::write(&spaced_file, crate::media::test_png(8, 8)).unwrap();
    assert_eq!(
        app.pasted_image_paths(&spaced_file.display().to_string())
            .unwrap()
            .len(),
        1,
        "a single unquoted path with spaces is still one path"
    );
    assert_eq!(
        app.pasted_image_paths(&format!("\"{}\"", spaced_file.display()))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn debug_enter_updates_visible_value() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
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
    app.open_menu(Menu::Settings);
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
            cached: 2,
            why: 0x81,
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
            parts.len() >= 22
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
                // the two probe columns: cache size, and why the frame drew
                && parts[20] == "2"
                && parts[21] == "0x81"
        }),
        "frame line:\n{content}"
    );
    assert!(content.contains("EVT"), "event line:\n{content}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn error_status_shows_in_notice_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.status("provider boom", StatusKind::Err);
    let text: String = app
        .notice_line(120)
        .expect("notice row")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("provider boom"), "notice: {text}");
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
    // expired toasts vanish from the notice row
    app.toast.as_mut().expect("toast").until =
        std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(app.notice_line(120).is_none(), "expired toast hidden");
    assert!(app.toast.is_none(), "expired toast dropped");
}

#[test]
fn status_bar_ignores_subagents_and_abort_cancels_running_children() {
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
    // children surface in the transcript rows now, never in the bar
    assert!(!text.contains("agents"), "bar: {text}");
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
        ask: None,
    });
    app.clear_subagent_ui_on_stop();
    assert!(app.subagents.is_empty());
    assert!(!app.segments.iter().any(|segment| {
        matches!(segment, Segment::Subagent { .. })
            || matches!(segment, Segment::Tool { name, .. } if name == "subagent")
    }));
}

#[test]
fn status_bar_never_mentions_subagents() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.subagents
        .push((1, "one".into(), "completed".into(), "done".into(), false));
    app.subagents
        .push((2, "two".into(), "completed".into(), "done".into(), false));
    // failures surface through the transcript rows, never the bar
    app.subagents
        .push((3, "three".into(), "failed".into(), "err".into(), false));
    let text: String = app
        .status_bar_spans(160)
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(!text.contains("agents"), "stale label must go: {text}");
    assert!(!text.contains("failed"), "bar: {text}");
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

#[test]
fn reveal_flush_of_one_char_does_not_panic() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    // one queued char with the flush window long due: the step must come out
    // usable instead of tripping clamp's min<=max assert (queued == 1)
    app.pending_reveal = "x".into();
    let interval = crate::tui::app::REVEAL_FLUSH_INTERVAL.as_millis();
    let step = crate::tui::app::reveal_step(interval, 1);
    assert_eq!(step, 2);
    assert!(app.reveal_chars(step));
    assert_eq!(app.assistant_buf, "x");
    assert_eq!(live_assistant_text(&app), "x");
    // a stale window (no reveal for a long time) is capped by the queue
    assert_eq!(crate::tui::app::reveal_step(3_600_000, 2), 2);
    assert_eq!(crate::tui::app::reveal_step(3_600_000, 500), 500);
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
        approval_grants: Default::default(),
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
        approval_grants: Default::default(),
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
        approval_grants: Default::default(),
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
fn transcript_row_toggles_subagent_inline_like_standard_tool() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::Subagent {
        id: 7,
        task: "inspect rendering".into(),
        status: "completed".into(),
        output: "found 3 callers in src/view.rs\n[Subagent receipt: ok | 2 tool call(s): grep(render)]".into(),
        expanded: false,
    });
    let collapsed = render_to_string(&mut app, 100, 30);
    assert!(!collapsed.contains("found 3 callers"), "collapsed by default:\n{collapsed}");

    let row = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(0))
        .expect("child row");
    app.click(row);
    assert_eq!(app.active_subagent, None, "stays in main chat instead of switching screens");
    let unfolded = render_to_string(&mut app, 100, 30);
    assert!(unfolded.contains("found 3 callers in src/view.rs"), "unfolds inline:\n{unfolded}");
    assert!(unfolded.contains("[Subagent receipt: ok"), "receipt visible inline:\n{unfolded}");
}

#[test]
fn subagent_multiline_task_and_blank_deltas_render_without_empty_gaps() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.handle_subagent_start(
        3,
        "Исследуй файл src/tui/presenter.rs. Найди и опиши:\n1. Как работает дифф\n2. Как работает coalesce"
            .into(),
    );
    // Expand while running before any tools arrive -> shows "thinking…" without blank gaps.
    if let Some(Segment::Subagent { expanded, .. }) = app.segments.first_mut() {
        *expanded = true;
    }
    app.touch_segment(0);
    let initial = render_to_string(&mut app, 100, 20);
    assert!(
        initial.contains("thinking…"),
        "running subagent with no output yet shows thinking indicator:\n{initial}"
    );
    assert!(
        !initial.contains("\n1. Как"),
        "multiline task must be flattened onto the header row:\n{initial}"
    );

    // Provider emits leading/interleaved "\n\n" text deltas around tool calls.
    if let Some(Segment::Subagent { output, .. }) = app.segments.first_mut() {
        output.push_str("\n\n• read (src/tui/presenter.rs)\n\n\n• grep (coalesce)\n\n");
    }
    app.touch_segment(0);
    let mid = render_to_string(&mut app, 100, 20);
    let lines: Vec<&str> = mid.lines().map(str::trim_end).collect();
    let head_idx = lines.iter().position(|l| l.contains("subagent-3")).unwrap();
    assert!(
        lines[head_idx + 1].contains("• read (src/tui/presenter.rs)"),
        "first tool row must sit immediately under subagent header with no blank gap:\n{mid}"
    );
    assert!(
        lines[head_idx + 2].contains("• grep (coalesce)"),
        "second tool row must sit immediately under first tool row with no blank gap:\n{mid}"
    );
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
                ask: None,
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
fn sub_chat_renders_every_row_flat() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("found it"), "answer stays visible:\n{s}");
    assert!(s.contains("a.rs"), "tool row stays visible:\n{s}");
    assert!(!s.contains("calls"), "no aggregate:\n{s}");
    assert!(!s.contains("hmm"), "finished thought hidden:\n{s}");
}

#[test]
fn sub_chat_tool_click_expands_output() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    render_to_string(&mut app, 100, 30);
    // click the tool row itself: output unfolds inline, nothing folds
    let abs = app
        .cache_lines
        .iter()
        .position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains("a.rs")
        })
        .expect("tool row");
    app.click(abs);
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("contents"), "output unfolded:\n{s}");
    assert!(s.contains("found it"), "answer stays:\n{s}");
}

#[test]
fn chat_link_click_resolves_and_fires_gracefully() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::Assistant {
        text: "see [docs](https://example.com/x) now".into(),
        live: false,
    });
    render_to_string(&mut app, 100, 30);
    let abs = app
        .cache_lines
        .iter()
        .position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains("docs")
        })
        .expect("link row");
    let row_text: String = app.cache_lines[abs]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let label_col = row_text.find("docs").unwrap();
    // plain text: no target, no toast
    app.click_at(abs, 0);
    assert!(app.toast.is_none(), "click off the link stays silent");
    // the label: resolves the pinned URL; the test opener refuses to
    // spawn a browser, so firing lands on the graceful error toast
    app.click_at(abs, label_col);
    assert_eq!(
        toast_text(&app),
        "open failed: no browser in tests",
        "link click fires the opener"
    );
}

#[test]
fn close_subagent_view_returns_to_main() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "completed");
    render_to_string(&mut app, 100, 30);
    app.close_subagent_view();
    assert_eq!(app.active_subagent, None);
    assert!(app.follow, "closing restores follow");
}

#[test]
fn sub_chat_live_while_running() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    open_sub_chat(&mut app, 7, "running");
    // reopen the tool row: the running turn is still open
    if let Some(Segment::Tool { ok, .. }) = app
        .subagent_chats
        .get_mut(&7)
        .and_then(|chat| chat.get_mut(2))
    {
        *ok = None;
    }
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("a.rs"), "running rows stay visible:\n{s}");
    assert!(!s.contains("calls"), "no aggregate:\n{s}");
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
            ask: None,
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
fn subagent_rows_read_as_tool_rows_without_accent() {
    use crate::tui::theme::Theme;
    let app = test_app("http://127.0.0.1:9/v1".into());
    let chat = vec![
        Segment::Subagent {
            id: 1,
            task: "look".into(),
            status: "running".into(),
            output: String::new(),
            expanded: false,
        },
        Segment::Subagent {
            id: 2,
            task: "look".into(),
            status: "completed".into(),
            output: String::new(),
            expanded: false,
        },
        Segment::Subagent {
            id: 3,
            task: "look".into(),
            status: "failed".into(),
            output: String::new(),
            expanded: false,
        },
    ];
    let line = |i: usize| app.render_segment(&chat, i, 80, false)[0].0.clone();
    let name_style = |line: &ratatui::text::Line| {
        line.spans
            .iter()
            .find(|s| s.content.contains("subagent-"))
            .map(|s| s.style)
    };
    // running: the quiet-white spinner marker, gray name, no arrow anywhere
    let running = line(0);
    let text: String = running.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(!text.contains('→'), "arrow gone: {text:?}");
    assert_eq!(running.spans[0].style, Theme::spinner());
    assert_ne!(
        running.spans[0].style,
        Theme::tool_head(),
        "the marker must not outshine the name"
    );
    assert_eq!(name_style(&running), Some(Theme::meta()));
    assert!(text.contains("look"), "task rides along: {text:?}");
    // done: the house green dot, gray name, no bold on either
    let done = line(1);
    assert_eq!(done.spans[0].content.as_ref(), "• ");
    assert!(
        !done.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the dot carries no bold: {:?}",
        done.spans[0].style
    );
    assert_eq!(name_style(&done), Some(Theme::meta()));
    // failed: the house red dot and a red name — colored, not bolded; the
    // marker shape and the hue carry the verdict
    let failed = line(2);
    assert_eq!(failed.spans[0].content.as_ref(), "• ");
    assert!(
        !failed.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the dot carries no bold: {:?}",
        failed.spans[0].style
    );
    assert_eq!(name_style(&failed), Some(Theme::err()));
}

/// Expanding a tool inside a subagent chat must keep the viewport: the
/// anchor resolves the row's tag, never its position. Before the fix the
/// position leaked in as the tag, re-anchored to a row further down and
/// yanked the view to the bottom.
#[test]
fn expanding_a_tool_in_subagent_view_keeps_the_viewport() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.active_subagent = Some(7);
    let tool = |output: &str| Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: Some(true),
        output: output.into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
        ask: None,
    };
    app.subagent_chats.insert(
        7,
        vec![
            Segment::Assistant {
                text: "word ".repeat(60),
                live: false,
            },
            tool(""),
            tool("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10"),
            Segment::Commentary("filler one".into()),
            Segment::Commentary("filler two".into()),
            Segment::Commentary("filler three".into()),
            Segment::Commentary("filler four".into()),
        ],
    );
    app.subagent_meta.insert(
        7,
        (0..7)
            .map(|i| crate::tui::app::view::SegMeta {
                id: 100 + i,
                rev: 0,
            })
            .collect(),
    );
    // short viewport: the expanded tool alone overflows it, so a wrong
    // anchor visibly yanks the view (a tall viewport would clamp the
    // jump back to zero and hide the bug)
    render_to_string(&mut app, 100, 16);
    app.follow = false;
    app.view_top = 0;
    // head row of the tool with the long output (segment 2)
    let head = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(2))
        .expect("tool head row");
    app.click(head);
    assert!(
        matches!(
            app.subagent_chats.get(&7).and_then(|c| c.get(2)),
            Some(Segment::Tool { expanded: true, .. })
        ),
        "tool unfolded"
    );
    render_to_string(&mut app, 100, 16);
    assert_eq!(app.view_top, 0, "viewport must not jump");
    assert!(!app.follow, "inspection keeps follow off");
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
    // settled ACT: bright-green go-chip
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[0].content.as_ref(), " ACT ");
    assert_eq!(spans[0].style, crate::tui::theme::Theme::mode_chip_act());
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

#[test]
fn approval_chip_matches_mode_when_settled() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // default AUTO: the quiet gray chip sits second, the mode chip keeps
    // its place and its color
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[0].content.as_ref(), " ACT ");
    assert_eq!(spans[0].style, crate::tui::theme::Theme::mode_chip_act());
    assert_eq!(spans[1].content.as_ref(), "AUTO ");
    assert_eq!(spans[1].style, crate::tui::theme::Theme::mode_chip_auto());
    // ASK earns the amber, FULL the red (blend force-expired, no timing)
    let expire = |app: &mut App| {
        app.approval_blend = Some((
            super::super::MODE_PLAN_RGB,
            std::time::Instant::now() - std::time::Duration::from_secs(5),
        ));
    };
    app.set_approval_mode(ApprovalMode::Ask);
    expire(&mut app);
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[1].content.as_ref(), "ASK ");
    assert_eq!(spans[1].style, crate::tui::theme::Theme::mode_chip_ask());
    app.set_approval_mode(ApprovalMode::Full);
    expire(&mut app);
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[1].content.as_ref(), "FULL ");
    assert_eq!(spans[1].style, crate::tui::theme::Theme::mode_chip_full());
}

#[test]
fn approval_chip_can_be_hidden_by_config() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.approval_chip = false;
    let spans = app.status_bar_spans(120);
    assert_eq!(spans[0].content.as_ref(), " ACT ");
    assert!(
        spans.iter().all(|s| !s.content.contains("AUTO")),
        "the approval chip is gone from the bar"
    );
    // the axis itself is untouched: cycling still works
    app.cycle_approval_mode();
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmFullAccess)));
}

#[test]
fn approval_cycle_gates_full_behind_the_confirm_dialog() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // auto -> full opens the red dialog; the mode itself must not move
    app.cycle_approval_mode();
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmFullAccess)));
    assert_eq!(
        app.approval_mode,
        ApprovalMode::Auto,
        "the dialog, not the key, grants full access"
    );
    // cancel is the preselected answer and leaves everything as it was
    assert!(matches!(
        app.menu_row_at(app.menu_sel).map(|(_, a)| a.clone()),
        Some(MenuAction::Back)
    ));
    app.menu_back();
    assert!(app.menu_stack.is_empty());
    assert_eq!(app.approval_mode, ApprovalMode::Auto);
    // enabling through the dialog's row is the only road in
    app.cycle_approval_mode();
    app.run_action(MenuAction::EnableFullAccess);
    assert_eq!(app.approval_mode, ApprovalMode::Full);
    assert!(app.menu_stack.is_empty());
    // leaving full is immediate: full -> ask, no dialog on the way out
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Ask);
    assert!(app.menu_stack.is_empty());
    // ask -> auto lands directly
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Auto);
    assert!(app.menu_stack.is_empty());
    // one enable per run: the second entry cycles straight in, no dialog
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Full);
    assert!(
        app.menu_stack.is_empty(),
        "the dialog must not interrupt cycling after the first enable"
    );
}

#[test]
fn launching_in_full_mode_counts_as_already_enabled() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // a run started with --approval full (or the config) already answered
    // the question once: cycling back must not re-ask
    app.approval_mode = ApprovalMode::Full;
    app.full_access_unlocked = true;
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Ask);
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Auto);
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Full);
    assert!(app.menu_stack.is_empty(), "no dialog for a pre-enabled run");
}

#[test]
fn approval_cycle_is_deliberately_inert_in_plan() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.set_mode(Mode::Plan);
    app.cycle_approval_mode();
    assert_eq!(app.approval_mode, ApprovalMode::Auto);
    assert!(
        app.menu_stack.is_empty(),
        "PLAN must not arm a hidden approval mode"
    );
}

#[test]
fn approval_mode_change_is_a_notice_not_a_history_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let before = app.segments.len();
    app.set_approval_mode(ApprovalMode::Full);
    // the chat stays clean; the 3s notice carries the change
    assert_eq!(
        app.segments.len(),
        before,
        "a mode switch must not write into the history"
    );
    assert!(app
        .toast
        .as_ref()
        .is_some_and(|t| t.text == "approval: auto → full"));
}

/// the status bar shows the *effective* mapping. Selecting `max` on
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
        text.contains(" max ") && !text.contains("max→"),
        "status bar: {text:?}"
    );

    // and a level the model will not act on is marked, not implied
    app.model_cfg.effort_control = Some(crate::config::EffortControl::None);
    let text: String = app
        .status_bar_spans(120)
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("max (ignored)"), "status bar: {text:?}");
}

/// The effort menu is the one place with room for the reason, so it must
/// carry it for every level rather than only for the selected one.
#[test]
fn the_effort_menu_annotates_every_level_it_offers() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort_control = Some(crate::config::EffortControl::Named);
    app.open_menu(crate::tui::app::menus::Menu::Effort { model: None });
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
    app.open_menu(crate::tui::app::menus::Menu::Effort { model: None });
    // slider opens on the current level, not on `off`
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::High)
        .unwrap();
    assert_eq!(app.menu_sel, want);
    // render the surface into the composer slot: 6 click targets, one per level
    let area = Rect::new(0, 0, 100, 30);
    let surface = Rect::new(0, 23, 100, 7);
    let mut buf = Buffer::empty(area);
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
    // first draw only tracks, never sweeps from nothing
    assert_eq!(app.effort_blend_sel, Some(want));
    assert!(app.effort_blend.is_none(), "no sweep on open");
    // moving the selection arms the color sweep for the new level
    app.menu_sel = 0;
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert_eq!(app.effort_blend_sel, Some(0));
    // the surface owns the bottom slot: title left, the committed level
    // right, one dim hint line at the very bottom
    let title: String = (0..100)
        .map(|x| buf[(x, surface.y + 1)].symbol().to_string())
        .collect();
    assert!(
        title.trim_start().starts_with("Effort"),
        "title sits at the left of the surface: {title:?}"
    );
    assert!(
        title.contains("high"),
        "the committed level rides the title row: {title:?}"
    );
    assert!(
        !title.contains("ctrl+e"),
        "the surface says nothing about the key that opened it: {title:?}"
    );
    let hint: String = (0..100)
        .map(|x| buf[(x, surface.bottom() - 1)].symbol().to_string())
        .collect();
    assert!(
        hint.contains("enter: confirm") && hint.contains("esc: cancel"),
        "one hint line at the bottom: {hint:?}"
    );
    let air: String = (0..100)
        .map(|x| buf[(x, surface.y + 2)].symbol().to_string())
        .collect();
    assert!(air.trim().is_empty(), "air row under title: {air:?}");
    assert!(
        app.effort_blend.is_some(),
        "level change must arm the sweep"
    );
    // restore High for the dots/hover checks below (they assume it selected)
    app.menu_sel = want;
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    // expired sweep settles back to the palette color: the selected dot reads
    // the theme color, not a stale blend frame
    app.effort_blend = Some((
        (0, 0, 0),
        std::time::Instant::now() - std::time::Duration::from_secs(5),
    ));
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    let (hit, _) = app.effort_hits[want];
    let dot = buf[(hit.x + 4, hit.y + 1)].style().fg;
    assert_eq!(
        dot,
        Some(ratatui::style::Color::Rgb(110, 165, 255)),
        "{dot:?}"
    );
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
fn effort_narrow_is_the_same_surface() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort = EffortLevel::Low;
    app.open_menu(crate::tui::app::menus::Menu::Effort { model: None });
    // narrow gets the same picker: the columns pack, there is no list form
    let mut buf = Buffer::empty(Rect::new(0, 0, 40, 30));
    let surface = Rect::new(0, 23, 40, 7);
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
    assert!(
        app.effort_hits
            .iter()
            .all(|(r, _)| r.right() <= surface.right()),
        "packed columns must stay inside the surface"
    );
    // and the first column is still the level it names
    let (first, idx) = app.effort_hits[0];
    assert_eq!(idx, 0);
    assert_eq!(app.effort_index_at(first.y, first.x + 1), Some(0));
}

/// A press that opens a submenu owns the whole gesture: a 1px jitter
/// between Down and Up must not hover-yank the fresh card's cursor off
/// the selected level — and it must work even before the first paint
/// built the slider hit map (no surface drawn yet, so the hit map is empty).
#[test]
fn submenu_open_jitter_does_not_move_effort_cursor() {
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // session model "m" sits on Off: the fresh card must open there
    app.open_menu(crate::tui::app::menus::Menu::EditModel {
        provider: "p".into(),
        key: Some("m".into()),
    });
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    // click the effort row on the EditModel surface
    let effort_row = app
        .surf_hits
        .iter()
        .find(|(_, idx)| *idx == 3)
        .map(|(y, _)| *y)
        .expect("effort row in surface hits");
    let effort_col = app.menu_rect.x + 4;
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::Off)
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let me = |kind: MouseEventKind, row: u16, col: u16| {
        Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    tx.send(me(
        MouseEventKind::Down(MouseButton::Left),
        effort_row,
        effort_col,
    ))
    .unwrap();
    tx.send(me(
        MouseEventKind::Up(MouseButton::Left),
        effort_row,
        effort_col,
    ))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(
            app.cur_menu(),
            Some(crate::tui::app::menus::Menu::Effort { .. })
        ),
        "click on the effort row must open the submenu"
    );
    assert_eq!(app.menu_sel, want, "fresh card opens on the target level");
    // before the effort surface is drawn, hover jitter must not yank the cursor
    assert!(app.effort_hits.is_empty());
    let max_row = app.menu_rect.y + 2 + 5;
    tx.send(me(MouseEventKind::Moved, max_row, effort_col))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(
        app.menu_sel, want,
        "hover jitter must not yank the cursor"
    );
}

/// Keyboard-opened pickers never had a press: a resting mouse's ambient
/// jitter must not yank the cursor off the session level. The surface hovers
/// nothing at all — only a click moves the preview, Enter commits.
#[test]
fn keyboard_open_effort_ignores_resting_mouse_jitter() {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::High;
    app.model_cfg.effort = EffortLevel::High;
    let (tx, rx) = std::sync::mpsc::channel();
    let me = |kind: MouseEventKind, row: u16, col: u16| {
        Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    let ctrl_e = Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
    let area = Rect::new(0, 0, 100, 30);
    let surface = Rect::new(0, 23, 100, 7);
    // phase 1 — geometry discovery: open, paint the surface, take a column
    tx.send(ctrl_e.clone()).unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(
            app.cur_menu(),
            Some(crate::tui::app::menus::Menu::Effort { .. })
        ),
        "Ctrl+E must open the Effort menu"
    );
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::High)
        .unwrap();
    assert_eq!(
        app.menu_sel, want,
        "fresh picker opens on the session level"
    );
    let mut buf = Buffer::empty(area);
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
    let (off_col, _) = app.effort_hits[0];
    let (max_col, max_idx) = app.effort_hits.last().copied().expect("hit map");
    let row = off_col.y;
    let col = off_col.x + 1;
    // close, park the mouse exactly on the Off column (no menu: chat hover)
    tx.send(esc).unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.cur_menu().is_none());
    tx.send(me(MouseEventKind::Moved, row, col)).unwrap();
    app.poll_input(&rx).unwrap();
    // phase 2 — reopen under the resting mouse, paint, jitter ±1 row
    tx.send(ctrl_e).unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        matches!(
            app.cur_menu(),
            Some(crate::tui::app::menus::Menu::Effort { .. })
        ),
        "Ctrl+E must reopen the Effort menu"
    );
    assert_eq!(
        app.menu_sel, want,
        "reopened picker opens on the session level"
    );
    let mut buf2 = Buffer::empty(area);
    app.draw_effort_slider(&mut buf2, surface, 0, 7);
    tx.send(me(MouseEventKind::Moved, row + 1, col)).unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(
        app.menu_sel, want,
        "resting-mouse jitter must not move the cursor"
    );
    // hovering the far column previews nothing either
    tx.send(me(MouseEventKind::Moved, max_col.y, max_col.x + 1))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.menu_sel, want, "hover must not preview a level");
    // a click on it does, and keeps the picker open for Enter
    tx.send(me(
        MouseEventKind::Down(MouseButton::Left),
        max_col.y,
        max_col.x + 1,
    ))
    .unwrap();
    app.poll_input(&rx).unwrap();
    tx.send(me(
        MouseEventKind::Up(MouseButton::Left),
        max_col.y,
        max_col.x + 1,
    ))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(
        app.menu_sel, max_idx,
        "a click previews the column it landed on"
    );
    assert!(
        matches!(
            app.cur_menu(),
            Some(crate::tui::app::menus::Menu::Effort { .. })
        ),
        "a click must not close the picker"
    );
}

/// Scoped Effort menu follows the target model, never the session one:
/// title names the target, cursor and star sit on its level, SetEffort
/// writes the target and leaves the session model alone.
#[test]
fn effort_scoped_menu_follows_target_model_not_session() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::High;
    app.model_cfg.effort = EffortLevel::High;
    let mut other = app.cfg.models["m"].clone();
    other.effort = EffortLevel::Off;
    app.cfg.models.insert("other".to_string(), other);
    app.open_menu(crate::tui::app::menus::Menu::Effort {
        model: Some("other".to_string()),
    });
    assert!(
        app.menu_title().contains("other"),
        "title names the target: {}",
        app.menu_title()
    );
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::Off)
        .unwrap();
    assert_eq!(app.menu_sel, want, "cursor on the target level");
    let star = app.menu_rows.iter().position(|(line, _)| {
        line.spans
            .first()
            .is_some_and(|s| s.content.as_ref() == "*")
    });
    assert_eq!(star, Some(want), "star on the target level");
    app.run_action(crate::tui::app::menus::MenuAction::SetEffort(
        EffortLevel::Max,
    ));
    assert_eq!(
        app.cfg.models.get("other").unwrap().effort,
        EffortLevel::Max,
        "write lands on the target"
    );
    assert_eq!(
        app.cfg.models.get("m").unwrap().effort,
        EffortLevel::High,
        "session model untouched"
    );
    assert_eq!(app.model_cfg.effort, EffortLevel::High);
}

/// Missing scoped key falls back to the session level — cursor and star
/// agree with each other either way.
#[test]
fn effort_scoped_menu_missing_key_falls_back_to_session() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::High;
    app.model_cfg.effort = EffortLevel::High;
    app.open_menu(crate::tui::app::menus::Menu::Effort {
        model: Some("ghost".to_string()),
    });
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::High)
        .unwrap();
    assert_eq!(app.menu_sel, want, "cursor falls back to session");
    let star = app.menu_rows.iter().position(|(line, _)| {
        line.spans
            .first()
            .is_some_and(|s| s.content.as_ref() == "*")
    });
    assert_eq!(star, Some(want), "star agrees with cursor on fallback");
}

/// Full path through the edit form: clicking the effort row opens the
/// menu scoped to the edited model, cursor and star on its level in both
/// wide (slider) and narrow (list) paint.
#[test]
fn effort_scoped_menu_via_form_click() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::High;
    app.model_cfg.effort = EffortLevel::High;
    let mut other = app.cfg.models["m"].clone();
    other.effort = EffortLevel::Off;
    app.cfg.models.insert("other".to_string(), other);
    app.open_menu(crate::tui::app::menus::Menu::EditModel {
        provider: "p".into(),
        key: Some("other".into()),
    });
    assert_eq!(app.form_fields.len(), 4);
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    // click the effort row (title + air row precede the fields)
    let row = app.menu_rect.y + 2 + 3;
    let col = app.menu_rect.x + app.menu_rect.width.saturating_sub(2);
    app.form_mouse_down(row, col);
    assert!(
        matches!(
            app.cur_menu(),
            Some(crate::tui::app::menus::Menu::Effort { model: Some(k) })
            if k.as_str() == "other"
        ),
        "must open scoped, got title {:?}",
        app.menu_title()
    );
    let want = EffortLevel::SELECTABLE
        .iter()
        .position(|l| *l == EffortLevel::Off)
        .unwrap();
    assert_eq!(app.menu_sel, want, "cursor after form click");
    // wide draw keeps the cursor on the target level
    app.draw_effort_slider(&mut buf, Rect::new(0, 23, 100, 7), 0, 7);
    assert_eq!(app.menu_sel, want, "cursor after wide draw");
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
    // narrow is the same surface: the columns pack, the cursor stays put
    let mut nbuf = Buffer::empty(Rect::new(0, 0, 40, 30));
    app.draw_effort_slider(&mut nbuf, Rect::new(0, 23, 40, 7), 0, 7);
    assert_eq!(app.menu_sel, want, "cursor after a narrow draw");
    assert_eq!(app.effort_hits.len(), EffortLevel::SELECTABLE.len());
}

#[test]
fn effort_slider_colors_span_gray_to_magenta() {
    use crate::tui::theme::Theme;
    use ratatui::style::Color;
    assert_eq!(
        Theme::effort_color(EffortLevel::Off),
        Color::Rgb(128, 128, 128)
    );
    assert_eq!(
        Theme::effort_color(EffortLevel::Max),
        Color::Rgb(220, 130, 220)
    );
    // every level gets a distinct color
    let mut seen = std::collections::HashSet::new();
    for lvl in EffortLevel::SELECTABLE {
        assert!(seen.insert(Theme::effort_color(lvl)), "{lvl:?}");
    }
}

/// The sweep blends between the same numbers the palette rests on, so the
/// frame it ends on cannot be brighter than the settled one. This identity is
/// the whole anti-pop invariant: break it and a level change leaves a stuck,
/// lighter card behind.
#[test]
fn effort_sweep_lands_exactly_on_the_settled_color() {
    use crate::tui::theme::Theme;
    use ratatui::style::Color;
    for lvl in EffortLevel::SELECTABLE {
        let (r, g, b) = Theme::effort_rgb(lvl);
        assert_eq!(Theme::effort_color(lvl), Color::Rgb(r, g, b), "{lvl:?}");
        // a fully-progressed blend is the endpoint, not something past it
        let from = Theme::effort_rgb(EffortLevel::Off);
        assert_eq!(
            crate::tui::shimmer::blend(from, (r, g, b), 1.0),
            (r, g, b),
            "{lvl:?}"
        );
        assert!(
            !Theme::effort(lvl)
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "{lvl:?} must not be bold: a brightening terminal would show one level as two hues"
        );
    }
}

/// Reopening the Effort popup lands on the level's own color. The previous
/// blend belonged to a popup that is no longer on screen, and sweeping from it
/// made a fresh open look like a level change that never happened.
#[test]
fn effort_menu_reopens_without_a_stale_sweep() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let area = Rect::new(0, 0, 100, 30);
    let surface = Rect::new(0, 23, 100, 7);
    let mut buf = Buffer::empty(area);
    app.model_cfg.effort = EffortLevel::High;
    app.open_menu(Menu::Effort { model: None });
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    app.menu_sel = EffortLevel::SELECTABLE.len() - 1;
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert!(app.effort_blend.is_some(), "a level change sweeps");
    app.menu_home();
    app.model_cfg.effort = EffortLevel::Low;
    app.open_menu(Menu::Effort { model: None });
    assert!(
        app.effort_blend.is_none() && app.effort_blend_sel.is_none(),
        "opening must clear the old surface's blend state"
    );
    app.draw_effort_slider(&mut buf, surface, 0, 7);
    assert!(app.effort_blend.is_none(), "first frame must not sweep");
}

/// The effort card dims the screen behind it like every other menu. DIM alone
/// only dulls the foreground, so painted backgrounds (user band, selection
/// fills) would keep glowing behind this one popup.
/// The effort surface owns the composer slot and nothing above it: the old
/// floating card dimmed the whole screen behind it, which is exactly the
/// "menu pops over the chat" behavior the redesign removes.
#[test]
fn effort_surface_leaves_the_transcript_untouched() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.model_cfg.effort = EffortLevel::Max;
    app.open_menu(Menu::Effort { model: None });
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    // a painted transcript cell well above the surface slot
    buf[(0, 0)].set_style(Style::new().bg(Color::Indexed(235)));
    app.draw_effort_slider(&mut buf, Rect::new(0, 23, 100, 7), 0, 7);
    assert_eq!(
        buf[(0, 0)].style().bg,
        Some(Color::Indexed(235)),
        "nothing above the surface may be dimmed or cleared"
    );
    assert!(
        !buf[(0, 0)]
            .style()
            .add_modifier
            .contains(ratatui::style::Modifier::DIM),
        "no modal dim above the surface"
    );
}

#[test]
fn command_popup_contains_only_command_names() {
    assert!(COMMANDS.iter().all(|command| !command.contains(' ')));
    assert!(COMMANDS.contains(&"/undo"));
    assert!(COMMANDS.contains(&"/skill"));
}
#[test]
fn help_menu_is_empty() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(COMMANDS.contains(&"/help"));
    app.command("help");
    assert!(matches!(app.cur_menu(), Some(Menu::Help)));
    assert!(!app.menu_rows.is_empty());
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    assert!(app.surface_open);
}

#[test]
fn test_subcommands_gated_behind_experimental_flag() {
    // off by default: unknown command, hidden from both popup levels
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(!app.cfg.ui.experimental_test, "flag defaults to off");
    app.command("test art");
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
    assert_eq!(
        app.popup_items(),
        vec![
            "/test anim".to_string(),
            "/test approval".to_string(),
            "/test art".to_string(),
            "/test ask".to_string(),
            "/test churn".to_string(),
            "/test colors".to_string(),
            "/test md".to_string(),
            "/test plan".to_string()
        ]
    );
    app.command("test art");
    assert!(matches!(app.cur_menu(), Some(Menu::TestArt)));
    // the animation gallery opens (its content is checked in
    // `test_galleries_open_with_content`)
    app.command("test anim");
    assert!(matches!(app.cur_menu(), Some(Menu::TestAnim)));
    // bare /test with the flag on hints at the subcommand
    let mut app2 = test_app("http://127.0.0.1:9/v1".into());
    app2.cfg.ui.experimental_test = true;
    app2.command("test");
    assert!(app2.menu_stack.is_empty(), "bare /test opens nothing");
}

#[test]
fn test_art_opens_numbered_gallery() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.experimental_test = true;
    app.command("test art");
    assert!(matches!(app.cur_menu(), Some(Menu::TestArt)));
    assert_eq!(
        crate::tui::art::variants().len(),
        10,
        "gallery holds ten picks"
    );
    let labels: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .filter(|t: &String| t.contains(" · "))
        .collect();
    assert_eq!(labels.len(), 10, "{labels:?}");
    for (i, label) in labels.iter().enumerate() {
        assert!(
            label.starts_with(&format!(" {:02} · ", i + 1)),
            "variant {i} must carry its number: {label:?}"
        );
    }
}

#[test]
fn test_galleries_open_with_content() {
    use ratatui::{buffer::Buffer, layout::Rect};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.experimental_test = true;
    // colors: one row per palette entry
    app.command("test colors");
    assert!(matches!(
        app.cur_menu(),
        Some(crate::tui::app::menus::Menu::TestColors)
    ));
    assert_eq!(
        app.menu_rows.len(),
        crate::tui::theme::Theme::palette().len()
    );
    assert!(!app.menu_rows.is_empty());
    // md: the showcase renders to real rows
    app.command("test md");
    assert!(matches!(
        app.cur_menu(),
        Some(crate::tui::app::menus::Menu::TestMd)
    ));
    assert!(
        app.menu_rows.len() > 15,
        "showcase must span: {}",
        app.menu_rows.len()
    );
    let text: String = app
        .menu_rows
        .iter()
        .flat_map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    for needle in ["Title", "bold", "quoted line", "first", "fn main", "x + y"] {
        assert!(text.contains(needle), "showcase shows {needle}: {text:?}");
    }
    // anim: exactly one row per frame set
    app.command("test anim");
    assert!(matches!(
        app.cur_menu(),
        Some(crate::tui::app::menus::Menu::TestAnim)
    ));
    assert_eq!(
        app.menu_rows.len(),
        crate::tui::anim::sets().len(),
        "one row per set"
    );
    // draws without panic at both widths
    for (w, h) in [(100u16, 30u16), (70, 24)] {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        app.render_into(&mut buf, Rect::new(0, 0, w, h));
        assert!(app.surface_open);
    }
}

#[test]
fn held_tab_toggles_mode_only_once() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let send = |app: &mut App, kind: KeyEventKind| {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Tab,
            KeyModifiers::empty(),
            kind,
        )))
        .unwrap();
        app.poll_input(&rx).unwrap();
    };
    let before = app.mode;
    send(&mut app, KeyEventKind::Press);
    assert_ne!(app.mode, before, "press must toggle");
    // auto-repeat while held must not flap back and forth
    send(&mut app, KeyEventKind::Repeat);
    send(&mut app, KeyEventKind::Repeat);
    assert_ne!(app.mode, before, "repeat must not toggle");
    // terminals without repeat events send held Tab as rapid Press:
    // without an intervening Release that is a hold, not presses
    send(&mut app, KeyEventKind::Press);
    send(&mut app, KeyEventKind::Press);
    assert_ne!(app.mode, before, "held presses must not toggle back");
    // Release ends the hold: the next press toggles instantly, no window
    send(&mut app, KeyEventKind::Release);
    send(&mut app, KeyEventKind::Press);
    assert_eq!(app.mode, before, "fresh press must toggle at once");
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

/// Dragging inside the composer has to select, not just park the cursor. It
/// used to fall through to the chat selection: the routing guard asked whether
/// anything was selected, and at the first Drag event nothing was yet.
#[test]
fn dragging_in_the_composer_selects_text() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::{buffer::Buffer, layout::Rect};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.input = App::fresh_input("hello world".into());
    let area = Rect::new(0, 0, 60, 16);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    let row = app.last_input.y;
    let col0 = app.last_input.x + app.input_marker_w() + 1;

    let (tx, rx) = std::sync::mpsc::channel();
    let me = |kind: MouseEventKind, col: u16| {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    tx.send(me(MouseEventKind::Down(MouseButton::Left), col0))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        !app.input.is_selecting(),
        "a press alone only moves the cursor"
    );

    tx.send(me(MouseEventKind::Drag(MouseButton::Left), col0 + 4))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.input.is_selecting(), "the drag must select");
    assert_eq!(
        app.input.cursor(),
        (0, 4),
        "the cursor lands where the mouse is, in characters"
    );
    assert!(
        app.sel.is_none(),
        "the chat must not steal the gesture from the composer"
    );

    // Releasing copies the span; it must not take the highlight away with it —
    // `TextArea::copy()` cancels the selection it reads, which used to make the
    // selection vanish the moment the button came up.
    tx.send(me(MouseEventKind::Up(MouseButton::Left), col0 + 4))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        app.input.is_selecting(),
        "release keeps the selection visible"
    );
    assert_eq!(app.input.cursor(), (0, 4), "and the cursor stays put");

    // Ctrl+C is a copy, not a cut: the text goes to the clipboard and the
    // selection survives it.
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        app.input.is_selecting(),
        "copying must not cancel the selection"
    );
    assert_eq!(
        app.input.lines().join(""),
        "hello world",
        "and must not delete anything"
    );
}

/// `/context` (and the click on the context readout) shows the composition of
/// the window: parts with their colors, free space, and the honest gaps —
/// "no request yet" before one happens, "unaccounted" for whatever the parts
/// do not cover of the provider's real prompt count.
#[test]
fn context_menu_shows_parts_legend_and_honest_gaps() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.command("context");
    assert!(matches!(app.cur_menu(), Some(Menu::Context)));
    assert!(!app.menu_rows.is_empty());
    let text = |app: &App| {
        app.menu_rows
            .iter()
            .map(|(l, _)| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    app.session.last_breakdown = Some(crate::providers::RequestBreakdown {
        system_bytes: 4_000,
        memory_bytes: 800,
        skills_bytes: 1_200,
        history_bytes: 8_000,
        tool_output_bytes: 16_000,
        host_bytes: 800,
        reasoning_bytes: 400,
        total_bytes: 32_000,
        system_tokens: 1_000,
        memory_tokens: 200,
        skills_tokens: 300,
        history_tokens: 2_000,
        tool_output_tokens: 4_000,
        host_tokens: 200,
        reasoning_tokens: 100,
    });
    app.session.last_usage = Some(crate::providers::Usage {
        prompt_tokens: 8_000,
        completion_tokens: 500,
        cached_tokens: Some(6_000),
        cache_write_tokens: None,
        reasoning_tokens: Some(300),
    });
    app.session.context_peak_tokens = 9_500;
    app.build_menu_rows();
    let t = text(&app);
    assert!(t.contains("used 8k"), "the header keeps only used: {t}");
    assert!(t.contains("unaccounted"), "the gap is its own row: {t}");
    assert!(t.contains("free"), "free space is a legend row: {t}");
    // #209 L14 stays out of this panel: the fixed, uncompactable share is the
    // host's own accounting, and the owner asked for the user's content only
    assert!(
        !t.contains("fixed by the host"),
        "the panel carries no host-side footnote: {t}"
    );
    // the legend is a partition of the window: every row carries a percent and
    // together they may not claim more than the window has
    let sum: f64 = t
        .lines()
        .filter(|l| l.starts_with("● ") || l.starts_with("○ ") || l.starts_with("◌ "))
        .filter_map(|l| l.split_whitespace().last())
        .filter_map(|p| p.strip_suffix('%'))
        .filter_map(|p| p.parse::<f64>().ok())
        .sum();
    assert!(
        (99.5..=100.5).contains(&sum),
        "legend percents must sum to 100: {sum:.1}\n{t}"
    );
    // the scale is solid color: spaces painted with the part's background
    let meter = &app.menu_rows[2].0;
    assert!(
        meter
            .spans
            .iter()
            .all(|s| s.content.chars().all(|c| c == ' ')),
        "no glyphs on the scale: {:?}",
        meter
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
    );
    // the bar and the legend must agree with each other, whatever hues get
    // picked: the leading block wears the color the legend gives its part
    let dot = |name: &str| -> Option<ratatui::style::Color> {
        app.menu_rows.iter().find_map(|(line, _)| {
            let mark = line.spans.first()?;
            let label = line.spans.get(1)?;
            if mark.content.starts_with('●') && label.content.starts_with(name) {
                mark.style.fg
            } else {
                None
            }
        })
    };
    assert_eq!(
        meter.spans[0].style.bg,
        dot("tools"),
        "the fattest part (tools, i.e. their output) leads the bar"
    );
    // memory and skills are their own parts: burying them in "system" made the
    // biggest block on the scale unreadable
    assert!(t.contains("● memory"), "memory is a legend row: {t}");
    assert!(t.contains("● skills"), "skills is a legend row: {t}");
    // and the replayed chain of thought is not allowed to hide inside a row
    // named after the mode line
    assert!(
        t.contains("● reasoning"),
        "replayed provider state is a legend row: {t}"
    );
    assert!(
        t.contains("● host block"),
        "the volatile host block is a legend row: {t}"
    );
}

/// The context menu is a read-only card: no cursor, no selection band — arrows
/// and page keys do nothing, and the rows never paint the selection fill.
#[test]
fn context_menu_is_read_only() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.command("context");
    assert!(app.menu_is_dead(), "context is a dead menu");
    let sel = app.menu_sel;
    app.menu_nav(1);
    app.menu_nav(-1);
    app.menu_nav(5); // page jump
    assert_eq!(app.menu_sel, sel, "a dead menu has no cursor");
    // and a live menu still moves
    app.command("settings");
    assert!(!app.menu_is_dead());
    let sel = app.menu_sel;
    app.menu_nav(1);
    assert_ne!(app.menu_sel, sel);
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
        lines[input_y - 1].contains("queued 2: fix the typo"),
        "with no notice to show, the queue takes the row above the composer: {:?}",
        lines[input_y - 1]
    );
    assert!(
        lines[input_y - 1].contains("(+1 more)"),
        "the whole preview travels with it: {:?}",
        lines[input_y - 1]
    );
}

#[test]
fn context_view_renders_sections_and_scrolls_with_arrows() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(crate::providers::Role::User, "check the context");
    app.command("context-view");
    assert!(matches!(app.cur_menu(), Some(Menu::ContextView)));

    let md = app.context_view_markdown();
    assert!(md.contains("# System"));
    assert!(md.contains("# Host Block"));
    assert!(md.contains("# Messages (1)"));
    assert!(md.contains("check the context"));
    assert!(md.contains("# Tools ("));

    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    assert!(app.surface_open);
    assert_eq!(app.surf_top, 0);

    app.menu_nav(1);
    app.render_into(&mut buf, area);
    assert_eq!(app.surf_top, 1, "down arrow scrolls /context-view by one line");
}

#[test]
fn test_ask_settles_tool_row_on_confirm_and_on_skip() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.experimental_test = true;
    app.command("test ask");
    assert!(app.tool_running(), "/test ask starts an open tool row");

    // Answer all 3 questions and confirm each step
    app.inline_ask_select(0, 0);
    app.inline_ask_confirm();
    assert!(app.tool_running(), "still open after Q1");

    app.inline_ask_select(1, 0);
    app.inline_ask_confirm();
    assert!(app.tool_running(), "still open after Q2");

    app.inline_ask_select(2, 0);
    app.inline_ask_confirm();
    assert!(
        !app.tool_running(),
        "/test ask must settle the tool row once all questions are confirmed"
    );
    assert!(matches!(
        app.segments.last(),
        Some(Segment::Tool {
            ok: Some(true),
            ask: None,
            ..
        })
    ));

    // Skipping via Esc also settles the tool row with ok: Some(false)
    app.command("test ask");
    assert!(app.tool_running());
    app.inline_ask_skip();
    assert!(!app.tool_running(), "skipping /test ask must settle the tool row");
    assert!(matches!(
        app.segments.last(),
        Some(Segment::Tool {
            ok: Some(false),
            ask: None,
            ..
        })
    ));
}
