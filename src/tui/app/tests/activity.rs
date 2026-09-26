use super::*;

#[test]
fn thinking_segments_stay_in_event_order() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // Simulate an agent stream: think, two tools, think again.
    app.streaming = true;
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    let ev = |e: AgentEvent, app: &mut App| {
        let _ = app.agent.as_mut();
        match e {
            AgentEvent::ThinkingDelta(t) => app.handle_thinking_delta(t),
            AgentEvent::ToolStart {
                name,
                summary,
                call_id,
            } => app.handle_tool_start(name, summary, Some(call_id)),
            AgentEvent::ToolNotice {
                name,
                summary,
                ok,
                diff,
                call_id,
            } => app.handle_tool_notice(name, summary, ok, diff, Some(call_id)),
            AgentEvent::TextDelta(t) => app.handle_text_delta(t),
            _ => {}
        }
    };
    ev(AgentEvent::ThinkingDelta("first".into()), &mut app);
    ev(
        AgentEvent::ToolStart {
            name: "read".into(),
            summary: "a.rs".into(),
            call_id: "c-read".into(),
        },
        &mut app,
    );
    ev(
        AgentEvent::ToolNotice {
            name: "read".into(),
            summary: "lines".into(),
            ok: true,
            diff: None,
            call_id: "c-read".into(),
        },
        &mut app,
    );
    ev(AgentEvent::ThinkingDelta("second".into()), &mut app);
    ev(
        AgentEvent::ToolStart {
            name: "edit".into(),
            summary: "a.rs".into(),
            call_id: "c-edit".into(),
        },
        &mut app,
    );
    ev(
        AgentEvent::ToolNotice {
            name: "edit".into(),
            summary: "ok".into(),
            ok: true,
            diff: None,
            call_id: "c-edit".into(),
        },
        &mut app,
    );
    ev(AgentEvent::TextDelta("answer".into()), &mut app);

    let kinds: Vec<&str> = app
        .segments
        .iter()
        .map(|s| match s {
            Segment::Thinking { .. } => "thinking",
            Segment::Tool { .. } => "tool",
            Segment::Assistant { .. } => "answer",
            _ => "other",
        })
        .filter(|k| *k != "other")
        .collect();
    assert_eq!(
        kinds,
        vec!["thinking", "tool", "thinking", "tool", "answer"],
        "thinking and tools must interleave by event order"
    );
    // both thinking blocks kept their own text
    let thoughts: Vec<&String> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Thinking { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(thoughts.iter().any(|t| t.as_str() == "first"));
    assert!(thoughts.iter().any(|t| t.as_str() == "second"));
}

/// one finished turn: user, thinking, one tool, answer
fn finished_turn(app: &mut App, ok: bool) {
    app.push_segment(Segment::User("do it".into()));
    app.push_segment(Segment::Thinking {
        text: "hmm".into(),
        expanded: false,
        started: None,
        duration_ms: 0,
        live: false,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: Some(ok),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Assistant {
        text: "done".into(),
        live: false,
    });
}

#[test]
fn thinking_duration_freezes_when_block_closes() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Thinking {
        text: "work".into(),
        expanded: false,
        started: Some(std::time::Instant::now() - std::time::Duration::from_millis(1200)),
        duration_ms: 0,
        live: true,
    });
    app.thinking_idx = Some(0);
    app.thinking_open = true;
    app.handle_tool_start("read".into(), "a.rs".into(), None);
    match &app.segments[0] {
        Segment::Thinking {
            duration_ms, live, ..
        } => {
            assert!(*duration_ms >= 1000, "duration froze at {duration_ms}ms");
            assert!(!live);
        }
        _ => panic!("thinking row was removed unexpectedly"),
    }
}

#[test]
fn activity_group_folds_the_turn_work_and_keeps_the_answer() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    finished_turn(&mut app, true);
    app.finalize_activity_group();

    assert_eq!(app.activity_groups.len(), 1);
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 3));
    assert_eq!((g.calls, g.thinking, g.errors), (1, 1, 0));

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(
        text.contains("activity · 1 calls · 1 thinking"),
        "header missing: {text}"
    );
    assert!(!text.contains("read"), "tool row must be folded: {text}");
    assert!(!text.contains("hmm"), "thinking must be folded: {text}");
    assert!(text.contains("done"), "the answer stays visible: {text}");

    // clicking the header unfolds the whole block
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("one header row tagged as group 0");
    app.click(header);
    assert!(app.activity_groups[0].expanded);
    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(text.contains("read"), "unfolded block shows tools: {text}");
    assert!(text.contains("done"), "answer still visible: {text}");
}

#[test]
fn failed_turn_folds_its_activity_group_shut() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    finished_turn(&mut app, false);
    app.finalize_activity_group();

    let g = &app.activity_groups[0];
    assert_eq!(g.errors, 1);
    assert!(!g.expanded, "even a failed turn folds shut at finish");

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(text.contains("1 error"), "error marker missing: {text}");
    assert!(!text.contains("read"), "the failed call folds away: {text}");
    // clicking the header unfolds the block on demand
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("one header row tagged as group 0");
    app.click(header);
    assert!(app.activity_groups[0].expanded);
    app.rebuild_cache(80);
    assert!(
        rendered(&app).contains("read"),
        "unfolded block shows the failed call"
    );
}

#[test]
fn abort_remaps_activity_group_ranges_past_dropped_rows() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::User("go".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "subagent".into(),
        args: "task".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Thinking {
        text: "hmm".into(),
        expanded: false,
        started: None,
        duration_ms: 0,
        live: false,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Assistant {
        text: "done".into(),
        live: false,
    });
    app.finalize_activity_group();
    // The subagent row is itself a tool call, so it belongs to the run.
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 4));

    // Stopping drops the subagent row, pulling only the range end inward.
    app.clear_subagent_ui_on_stop();
    assert_eq!(app.segments.len(), 4, "the subagent row is gone");
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 3), "end pulled in by one");

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(
        text.contains("activity · 1 calls · 1 thinking"),
        "header intact: {text}"
    );
    assert!(!text.contains("read"), "group stays folded: {text}");
    assert!(!text.contains("subagent"), "subagent row removed: {text}");
    assert!(text.contains("done"), "answer stays visible: {text}");
}

/// A compact `Subagent` row is a tool call like any other: it must join
/// the turn's activity group instead of breaking the run. Before the fix
/// the backward scan stopped dead on it, so the group was never built and
/// every row above the child stayed visible as an orphan.
#[test]
fn subagent_row_joins_the_turn_activity_group() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::User("delegate".into()));
    for name in ["grep", "read"] {
        app.push_segment(Segment::Tool {
            call_id: None,
            name: name.into(),
            args: "a.rs".into(),
            ok: Some(true),
            output: String::new(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
    }
    app.push_segment(Segment::Subagent {
        id: 1,
        task: "look".into(),
        status: "completed".into(),
        output: "found".into(),
        expanded: false,
    });
    app.push_segment(Segment::Assistant {
        text: "done".into(),
        live: false,
    });
    app.finalize_activity_group();

    assert_eq!(app.activity_groups.len(), 1, "the turn folds into one group");
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 4));
    assert_eq!(g.calls, 3, "grep + read + subagent are three calls");

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(text.contains("activity · 3 calls"), "header missing: {text}");
    assert!(
        !text.contains("subagent-1"),
        "the child row folds away with the rest: {text}"
    );
    assert!(!text.contains("grep"), "tool rows fold away: {text}");
    assert!(text.contains("done"), "the answer stays visible: {text}");
}

/// End to end over the real handlers: the event sequence a delegated call
/// produces (ToolStart, SubagentStart, ToolNotice) must paint exactly one
/// row and fold into the turn's activity group like any other call.
#[test]
fn delegated_call_paints_one_row_and_folds_with_the_turn() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("go".into()));
    app.handle_tool_start("subagent".into(), "task: look".into(), Some("c1".into()));
    app.handle_subagent_start(1, "look".into());
    app.handle_tool_notice(
        "subagent".into(),
        "child done".into(),
        true,
        None,
        Some("c1".into()),
    );
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Tool { name, .. } if name == "subagent")),
        "no generic tool row next to the child row: {:?}",
        app.segments
    );
    assert_eq!(app.segments.len(), 2, "user + one child row");
    app.push_segment(Segment::Assistant {
        text: "done".into(),
        live: false,
    });
    app.finalize_activity_group();

    assert_eq!(app.activity_groups.len(), 1, "one group for the turn");
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 2), "the run, answer excluded");
    assert_eq!(g.calls, 1, "one delegated call is one call");

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(text.contains("activity · 1 calls"), "header: {text}");
    assert!(!text.contains("subagent-1"), "child row folds away: {text}");
    assert!(text.contains("done"), "the answer stays visible: {text}");
}

/// Stopping mid-turn drops the subagent rows from *inside* a group: the
/// range must pull in by exactly the rows removed, and the recount must
/// agree with the builder that produced the header.
#[test]
fn abort_drops_subagent_rows_inside_a_group() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::User("go".into()));
    app.push_segment(Segment::Thinking {
        text: "hmm".into(),
        expanded: false,
        started: None,
        duration_ms: 0,
        live: false,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Subagent {
        id: 1,
        task: "look".into(),
        status: "running".into(),
        output: String::new(),
        expanded: false,
    });
    app.push_segment(Segment::Assistant {
        text: "done".into(),
        live: false,
    });
    app.finalize_activity_group();
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 4));
    assert_eq!(g.calls, 2, "read + the delegated call");

    app.clear_subagent_ui_on_stop();
    assert_eq!(app.segments.len(), 4, "the child row is gone");
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 3), "end pulled in by it");
    assert_eq!((g.calls, g.thinking), (1, 1), "recount agrees with builder");
    // the recount must match a freshly built group over the same slice
    let rebuilt = App::build_activity_group_in(&app.segments, (g.seg_start, g.seg_end), 0, None);
    assert_eq!(
        (g.calls, g.thinking, g.errors),
        (rebuilt.calls, rebuilt.thinking, rebuilt.errors),
        "abort recount and group builder must never disagree"
    );

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(
        text.contains("activity · 1 calls · 1 thinking"),
        "header stays coherent after the abort: {text}"
    );
    assert!(!text.contains("subagent"), "no subagent row survives: {text}");
}

/// A provider dump must not flood the chat: multi-line errors arrive
/// collapsed to one width-capped row and unfold on click.
#[test]
fn error_status_arrives_collapsed_and_unfolds_on_click() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // durable error segments (turn notes) still live in the chat;
    // transient notices toast and never become segments
    app.push_segment(Segment::Status {
        text: "first\nsecond\nthird".into(),
        kind: StatusKind::Err,
        expanded: false,
        transient: false,
    });
    let idx = app.segments.len() - 1;

    let rows = app.render_segment(&app.segments, idx, 100, true);
    assert_eq!(rows.len(), 1, "collapsed to a single row");
    let line: String = rows[0]
        .0
        .spans
        .iter()
        .map(|span| span.content.to_string())
        .collect();
    assert!(
        line.contains("first") && line.contains("… (2 more)"),
        "first line plus count: {line:?}"
    );
    assert!(
        !line.contains("second") && !line.contains("third"),
        "rest hidden until unfolded: {line:?}"
    );

    // a single line wider than the row collapses too
    app.push_segment(Segment::Status {
        text: "x".repeat(200),
        kind: StatusKind::Err,
        expanded: false,
        transient: false,
    });
    let wide = app.render_segment(&app.segments, app.segments.len() - 1, 100, true);
    assert_eq!(wide.len(), 1, "wide single line capped: {wide:?}");

    // click unfolds the first one back to all three lines
    app.rebuild_cache(100);
    let row = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(idx))
        .expect("collapsed error row tagged");
    app.click(row);
    app.rebuild_cache(100);
    let text = rendered(&app);
    assert!(
        text.contains("first") && text.contains("second") && text.contains("third"),
        "unfolded error shows everything: {text}"
    );
    assert!(!text.contains("more)"), "count hint gone once open: {text}");
}

/// A turn that fails before streaming anything has tool rows but no
/// answer slot (the empty live slot is dropped at finish): the activity
/// group must still cover the tools instead of vanishing.
#[test]
fn failed_turn_without_an_answer_still_groups_its_tools() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::User("go".into()));
    for name in ["read", "write"] {
        app.push_segment(Segment::Tool {
            call_id: None,
            name: name.into(),
            args: "a.rs".into(),
            ok: Some(true),
            output: String::new(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
    }
    app.finalize_activity_group();

    assert_eq!(app.activity_groups.len(), 1);
    let g = &app.activity_groups[0];
    assert_eq!((g.seg_start, g.seg_end), (1, 3));
    assert!(!g.expanded, "a failed turn folds shut");

    // and the header unfolds the block on click
    app.rebuild_cache(80);
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("header row tagged");
    app.click(header);
    assert!(app.activity_groups[0].expanded);
    app.rebuild_cache(80);
    assert!(
        rendered(&app).contains("a.rs"),
        "unfolded group shows tool rows"
    );
}

/// A group must never overlap the previous one: before the fix, an error
/// with no answer slot anchored on an older turn's answer and the new
/// group landed on top of the old range.
#[test]
fn activity_groups_never_overlap_previous_ranges() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    finished_turn(&mut app, true);
    app.finalize_activity_group();
    // second turn: tools ran, then a provider error with no streamed text
    app.push_segment(Segment::User("again".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "b.rs".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.finalize_activity_group();

    assert_eq!(app.activity_groups.len(), 2);
    let (first, second) = (&app.activity_groups[0], &app.activity_groups[1]);
    assert!(
        second.seg_start >= first.seg_end,
        "groups overlap: ({},{}) vs ({},{})",
        first.seg_start,
        first.seg_end,
        second.seg_start,
        second.seg_end
    );
    assert_eq!((second.seg_start, second.seg_end), (5, 6));
}

#[test]
fn activity_group_renders_live_while_the_turn_streams() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.streaming = true;
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.handle_thinking_delta("thinking".into());
    app.handle_tool_start("read".into(), "a.rs".into(), None);

    // the running turn is not frozen yet: no group stored, but rendered
    assert!(app.activity_groups.is_empty());
    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(
        text.contains("activity · 1 calls · 1 thinking"),
        "live header missing: {text}"
    );
    assert!(text.contains("read"), "live work is expanded: {text}");

    // clicking the live header folds it; the turn stays running
    let header = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(GROUP_BASE))
        .expect("live header row");
    app.click(header);
    assert!(app.streaming);
    app.rebuild_cache(80);
    assert!(
        !rendered(&app).contains("read"),
        "live group folds on click"
    );
}

#[test]
fn overscroll_leaves_no_dead_distance() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // 50 content lines, 10-row viewport -> deepest top = 40
    app.cache_lines = (0..50).map(|_| blank()).collect();
    app.last_chat = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
    };
    assert!(app.follow, "starts pinned to the bottom");
    // wheel up far past the very first line
    for _ in 0..30 {
        app.scroll(4);
    }
    assert!(!app.follow);
    assert_eq!(app.chat_top(10), 0, "parked at the top");
    // a single wheel down must move the viewport right away
    app.scroll(-4);
    assert_eq!(app.chat_top(10), 4);
    // scrolling back to the bottom re-enables follow
    for _ in 0..20 {
        app.scroll(-4);
    }
    assert!(app.follow);
}

#[test]
fn ctrl_c_copy_keeps_scroll_position() {
    // selecting text while scrolled up, then Ctrl+C must copy the
    // selection without jumping to the bottom of the session
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cache_lines = (0..50).map(|_| blank()).collect();
    app.last_chat = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
    };
    for _ in 0..10 {
        app.scroll(4);
    }
    assert!(!app.follow);
    let top = app.view_top;

    app.sel = Some(Selection {
        a: CellPos { row: 5, col: 0 },
        b: CellPos { row: 8, col: 3 },
    });

    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();

    assert!(!app.follow, "Ctrl+C must not re-enable follow");
    assert_eq!(app.view_top, top, "viewport must not move");
}

#[test]
fn drag_copy_then_ctrl_c_keeps_scroll_position() {
    // full mouse flow: press, drag, release (auto-copy), then Ctrl+C —
    // the viewport must stay parked where the user scrolled it
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
    };

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cache_lines = (0..50).map(|_| blank()).collect();
    app.last_chat = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
    };
    for _ in 0..10 {
        app.scroll(4);
    }
    assert!(!app.follow);
    let top = app.view_top;

    let (tx, rx) = std::sync::mpsc::channel();
    let me = |kind: MouseEventKind, row: u16| {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: 10,
            row,
            modifiers: KeyModifiers::empty(),
        })
    };
    tx.send(me(MouseEventKind::Down(MouseButton::Left), 4))
        .unwrap();
    app.poll_input(&rx).unwrap();
    tx.send(me(MouseEventKind::Drag(MouseButton::Left), 6))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.sel.is_some(), "drag creates a selection");
    tx.send(me(MouseEventKind::Up(MouseButton::Left), 6))
        .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.sel.is_some(), "mouse_up keeps the selection visible");
    assert_eq!(app.view_top, top, "auto-copy must not move the viewport");
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();

    assert!(!app.follow, "copy must not re-enable follow");
    assert_eq!(app.view_top, top, "viewport must not move");
}

#[test]
fn ctrl_copy_works_with_cyrillic_layout() {
    // with a Russian (ЙЦУКЕН) layout the terminal reports Ctrl+C as the
    // Cyrillic 'с': the app must treat it as the copy combo, not as
    // typing (which jumps the viewport to the bottom)
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cache_lines = (0..50).map(|_| blank()).collect();
    app.last_chat = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
    };
    for _ in 0..10 {
        app.scroll(4);
    }
    let top = app.view_top;
    app.sel = Some(Selection {
        a: CellPos { row: 5, col: 0 },
        b: CellPos { row: 8, col: 3 },
    });

    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('с'), // U+0441 CYRILLIC SMALL LETTER ES
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();

    assert!(
        app.input_text().is_empty(),
        "Ctrl+С with a Russian layout must not type into the input"
    );
    assert!(!app.follow, "Ctrl+С must not jump to the bottom");
    assert_eq!(app.view_top, top, "viewport must not move");
}

#[test]
fn chat_growth_keeps_viewport_stable() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cache_lines = (0..50).map(|_| blank()).collect();
    app.last_chat = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 10,
    };
    app.scroll(4); // leave the bottom
    let top = app.chat_top(10);
    // new messages arrive while scrolled up
    for _ in 0..10 {
        app.cache_lines.push(blank());
    }
    assert_eq!(app.chat_top(10), top, "viewport must not drift");
}

#[tokio::test]
async fn submit_without_api_key_is_rejected() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let pc = app.cfg.providers.get_mut("p").unwrap();
    pc.api_key = None;
    pc.api_key_env = None;
    unsafe { std::env::remove_var("P_API_KEY") };

    app.input = App::fresh_input("hi".into());
    app.submit();

    assert!(!app.streaming, "turn started without a key");
    assert!(
        !app.segments.iter().any(|s| matches!(s, Segment::User(_))),
        "user message must not be added"
    );
    assert!(
        matches!(&app.toast, Some(t) if t.kind == StatusKind::Err),
        "rejection toasts an error"
    );
    assert_eq!(
        app.input.lines().join("\n"),
        "hi",
        "typed input must be preserved"
    );
}

