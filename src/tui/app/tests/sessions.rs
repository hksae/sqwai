use super::*;

fn press_enter(app: &mut App) {
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
}

#[test]
fn enter_confirms_provider_deletion() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // models menu -> delete provider -> confirmation prompt
    app.open_menu(Menu::Models {
        provider: "p".into(),
    });
    let del_idx = app
        .menu_rows
        .iter()
        .position(|(_, a)| matches!(a, MenuAction::DeleteProvider(_)))
        .expect("delete provider row");
    app.menu_sel = del_idx;
    app.menu_activate();
    assert!(
        matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })),
        "confirm prompt must be open"
    );
    assert_eq!(app.menu_sel, 1, "selection starts on the confirm row");

    // plain enter on the prompt deletes the provider
    press_enter(&mut app);
    assert!(!app.cfg.providers.contains_key("p"), "provider not deleted");
    assert!(
        !app.cfg.models.contains_key("m"),
        "models of provider left behind"
    );
}

#[test]
fn esc_cancels_provider_deletion() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::ConfirmDelete {
        label: "delete provider 'p'?".into(),
        action: MenuAction::DeleteProvider("p".into()),
    });
    // esc pops back without touching the config
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(crossterm::event::Event::Key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        ),
    ))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(
        app.cfg.providers.contains_key("p"),
        "esc deleted the provider"
    );
}

#[test]
fn apply_session_renders_history_and_switches_model() {
    use crate::providers::Role;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.insert(
        "m2".to_string(),
        crate::config::ModelConfig {
            provider: "p".into(),
            id: "test-model-2".into(),
            context: 2000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
        },
    );
    let mut s = Session::new("m2".into(), 2000);
    s.push(Role::User, "hello");
    s.push(Role::Assistant, "hi there");

    app.apply_session(s);

    assert_eq!(app.session.model_key, "m2");
    assert_eq!(app.model_cfg.id, "test-model-2");
    assert_eq!(app.model_cfg.context, 2000);
    let chat: Vec<&Segment> = app
        .segments
        .iter()
        .filter(|s| !matches!(s, Segment::Status { .. }))
        .collect();
    assert_eq!(chat.len(), 2, "history must be rendered");
    assert!(matches!(chat[0], Segment::User(t) if t == "hello"));
    assert!(matches!(chat[1], Segment::Assistant { text, live: false } if text == "hi there"));
    assert!(app.menu_stack.is_empty(), "menu must close after switching");
}

#[test]
fn load_history_restores_tool_calls_and_results() {
    use crate::providers::{Message, Role, ToolCallReq};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.messages = vec![
        Message::new(Role::User, "inspect"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "call-1",
            "read",
            serde_json::json!({"file_path": "src/main.rs"}),
        )]),
        Message::tool_result("call-1", "file contents", false),
        Message::new(Role::Assistant, "done"),
    ];
    app.clear_segments();
    app.load_history_segments();
    assert!(matches!(app.segments[0], Segment::User(ref text) if text == "inspect"));
    assert!(
        matches!(app.segments[1], Segment::Tool { ref name, ok: Some(true), ref output, .. } if name == "read" && output == "file contents")
    );
    assert!(matches!(app.segments[2], Segment::Assistant { ref text, .. } if text == "done"));
    assert_eq!(app.activity_groups.len(), 1, "tool turn is grouped");
    assert!(
        !app.activity_groups[0].expanded,
        "restored groups are folded by default"
    );

    app.rebuild_cache(80);
    let text = rendered(&app);
    assert!(
        text.contains("activity · 1 calls"),
        "header restored: {text}"
    );
    assert!(!text.contains("read"), "tool is folded: {text}");
    assert!(text.contains("done"), "answer remains visible: {text}");

    let header = app
        .cache_rowseg
        .iter()
        .position(|tag| *tag == Some(GROUP_BASE))
        .expect("restored activity header");
    app.click(header);
    app.rebuild_cache(80);
    assert!(
        rendered(&app).contains("read"),
        "restored group can be unfolded"
    );
}

#[test]
fn restore_keeps_stopped_turns_out_of_later_groups() {
    use crate::providers::{Message, Role, ToolCallReq};
    use crate::session::{ActivitySummary, SessionHeader};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // turn 1: stopped after a failed tool call (no assistant text);
    // turn 2: normal tool turn with an answer
    app.session.messages = vec![
        Message::new(Role::User, "one"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "call-1",
            "read",
            serde_json::json!({}),
        )]),
        Message::tool_result("call-1", "boom", true),
        Message::new(Role::User, "two"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![
            ToolCallReq::new("call-2", "read", serde_json::json!({})),
            ToolCallReq::new("call-3", "read", serde_json::json!({})),
        ]),
        Message::tool_result("call-2", "a", false),
        Message::tool_result("call-3", "b", false),
        Message::new(Role::Assistant, "two done"),
    ];
    app.session.activity = vec![
        ActivitySummary {
            calls: 1,
            thinking: 0,
            duration_ms: 100,
            errors: 1,
            rejected: 0,
            user_index: Some(0),
        },
        ActivitySummary {
            calls: 2,
            thinking: 0,
            duration_ms: 500,
            errors: 0,
            rejected: 0,
            user_index: Some(2),
        },
    ];
    app.clear_segments();
    app.load_history_segments();

    assert_eq!(app.activity_groups.len(), 2, "two turns, two groups");
    let (g1, g2) = (&app.activity_groups[0], &app.activity_groups[1]);
    assert_eq!(g1.turn_user, Some(0));
    assert_eq!((g1.calls, g1.errors), (1, 1));
    assert!(!g1.expanded, "a stopped turn's group restores folded");
    assert_eq!(g2.turn_user, Some(2));
    assert_eq!((g2.calls, g2.errors), (2, 0));
    assert!(!g2.expanded);

    // the second user message must sit between the groups, visible
    let user_two = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::User(t) if t == "two"))
        .expect("user two segment");
    assert!(
        !app.activity_groups
            .iter()
            .any(|g| g.seg_start <= user_two && user_two < g.seg_end),
        "the second user message was swallowed by the stopped turn's group"
    );

    // legacy save (no anchors): sequential fallback still groups correctly
    let mut legacy = test_app("http://127.0.0.1:9/v1".into());
    legacy.session.messages = app.session.messages.clone();
    legacy.session.activity = app
        .session
        .activity
        .iter()
        .map(|a| ActivitySummary {
            user_index: None,
            ..a.clone()
        })
        .collect();
    legacy.clear_segments();
    legacy.load_history_segments();
    assert_eq!(legacy.activity_groups.len(), 2);
    assert_eq!(legacy.activity_groups[0].calls, 1);
    assert_eq!(legacy.activity_groups[1].calls, 2);
}

#[test]
fn stopped_and_failed_turns_append_durable_notes() {
    let mut stopped = test_app("http://127.0.0.1:9/v1".into());
    stopped.session.push(Role::User, "first");
    stopped.turn_user_index = Some(0);
    stopped.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    stopped.streaming = true;
    stopped.finish_turn(Err("aborted".into()));
    assert!(matches!(
        stopped.session.turn_notes.as_slice(),
        [crate::session::TurnNote { text, is_error: false, .. }] if text == "stopped"
    ));

    let mut failed = test_app("http://127.0.0.1:9/v1".into());
    failed.session.push(Role::User, "second");
    failed.turn_user_index = Some(0);
    failed.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    failed.streaming = true;
    failed.finish_turn(Err("provider offline".into()));
    assert!(matches!(
        failed.session.turn_notes.as_slice(),
        [crate::session::TurnNote { text, is_error: true, .. }]
            if text == "error: provider offline"
    ));
    // durable notes render live in the chat AND toast — neither alone
    for (app, text, kind) in [
        (&stopped, "stopped", StatusKind::Info),
        (&failed, "error: provider offline", StatusKind::Err),
    ] {
        assert!(
            app.segments.iter().any(|s| matches!(
                s,
                Segment::Status { text: t, kind: k, .. } if t == text && *k == kind
            )),
            "live {text:?} segment missing"
        );
        assert!(
            app.toast.as_ref().is_some_and(|t| t.text == text),
            "live {text:?} toast missing"
        );
    }
}

#[tokio::test]
async fn failed_compact_does_not_attach_note_to_previous_turn() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(Role::User, "earlier user prompt");
    app.start_compaction();
    app.finish_turn(Err("compaction model error".into()));
    assert!(
        app.session.turn_notes.is_empty(),
        "must not attach error to earlier user message"
    );
}

#[test]
fn history_restores_stopped_and_failed_turn_notes() {
    use crate::session::TurnNote;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(Role::User, "first");
    app.session.push(Role::User, "second");
    app.session.turn_notes = vec![
        TurnNote {
            user_index: 0,
            text: "stopped".into(),
            is_error: false,
        },
        TurnNote {
            user_index: 1,
            text: "error: provider offline".into(),
            is_error: true,
        },
    ];
    app.clear_segments();
    app.load_history_segments();

    let rows: Vec<(&str, StatusKind)> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Status { text, kind, .. } => Some((text.as_str(), *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("stopped", StatusKind::Info),
            ("error: provider offline", StatusKind::Err)
        ]
    );
}

/// A delegated call has no generic tool row live, so a reload must not
/// invent one: the child row is rebuilt from the call plus its result,
/// keeping live and restored transcripts the same shape.
#[test]
fn history_restores_a_delegated_call_as_a_child_row() {
    use crate::providers::{Message, ToolCallReq};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.push(Role::User, "delegate");
    app.session
        .messages
        .push(Message::new(Role::Assistant, "").with_tool_calls(vec![
            ToolCallReq::new(
                "c1",
                "subagent",
                serde_json::json!({"task": "look around"}),
            ),
        ]));
    app.session
        .messages
        .push(Message::tool_result("c1", "found it", false));
    app.session
        .messages
        .push(Message::new(Role::Assistant, "done"));
    app.clear_segments();
    app.load_history_segments();

    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Tool { name, .. } if name == "subagent")),
        "no generic tool row on reload: {:?}",
        app.segments
    );
    let child = app
        .segments
        .iter()
        .find_map(|s| match s {
            Segment::Subagent {
                id,
                status,
                output,
                ..
            } => Some((*id, status.clone(), output.clone())),
            _ => None,
        })
        .expect("the child row is rebuilt from history");
    assert_eq!(child, (1, "completed".to_string(), "found it".to_string()));

    // and the restored group counts the delegation exactly once
    assert_eq!(app.activity_groups.len(), 1);
    assert_eq!(app.activity_groups[0].calls, 1);
}

#[test]
fn apply_session_from_startup_does_not_persist_empty_stub() {
    // on the startup screen the current session is empty; opening an
    // existing session from there must switch to it without saving that
    // empty startup stub to disk (see apply_session's session_has_messages guard)
    use crate::providers::Role;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(app.session.messages.is_empty(), "startup session is empty");
    let start_id = app.session.id;
    let mut s = Session::new("m".into(), 1000);
    s.push(Role::User, "existing conversation");
    app.apply_session(s);
    assert_ne!(app.session.id, start_id, "active session switched");
    assert!(
        app.session_has_messages(),
        "active session is the existing one with history"
    );
}

#[test]
fn pin_from_menu_does_not_pollute_chat() {
    use crate::providers::Role;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut s = Session::new("m".into(), 1000);
    s.push(Role::User, "x");
    let id = s.id.to_string();
    app.sessions = vec![SessionHeader::from_session(&s)];
    app.open_menu(Menu::Sessions);
    // select the session row and pin it
    let row = app
        .menu_rows
        .iter()
        .position(|(_, a)| matches!(a, MenuAction::OpenSession(i) if *i == id))
        .expect("session row");
    app.menu_sel = row;
    app.run_action(MenuAction::PinSession(id));

    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Status { .. })),
        "menu actions must not write into the chat"
    );
    assert!(
        app.toast
            .as_ref()
            .is_some_and(|t| t.text.contains("pinned")),
        "pin notice goes to the toast"
    );
    assert!(app.sessions[0].pinned);
}

#[test]
fn menu_wheel_scrolls_view_without_wrap_or_trap() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Controls);
    let n = app.menu_rows.len();
    assert!(n > 6, "need room to scroll");
    app.menu_visible_rows = 5;
    // from the top, wheeling up stays (no wrap to the bottom)
    app.menu_wheel(-3);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 0));
    // wheel moves the view, never the selection
    app.menu_wheel(3);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 3));
    app.menu_wheel(3);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 6));
    // bottom edge clamps without wrapping to the top
    app.menu_wheel(10000);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, n - 5));
    app.menu_wheel(-3);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, n - 8));
}

#[test]
fn menu_scrollbar_yields_to_drawn_frames() {
    use crate::providers::Role;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // pinned frame + enough rows to overflow a short card
    let mut pinned = Session::new("m".into(), 1000);
    pinned.push(Role::User, "pinned one");
    pinned.pinned = true;
    app.sessions = vec![crate::session::SessionHeader::from_session(&pinned)];
    for i in 0..12 {
        let mut s = Session::new("m".into(), 1000);
        s.push(Role::User, format!("filler session number {i}"));
        app.sessions
            .push(crate::session::SessionHeader::from_session(&s));
    }
    app.open_menu(Menu::Sessions);
    let area = Rect::new(0, 0, 100, 14);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let row_text =
        |y: u16| -> String { (0..area.width).map(|x| buf[(x, y)].symbol()).collect() };
    // pinned frame survived the scrollbar: corners still corners
    let hy = (0..area.height)
        .find(|y| row_text(*y).contains("pinned"))
        .expect("pinned header row");
    let row = row_text(hy);
    assert!(
        row.contains("┌") && row.contains("┐"),
        "scrollbar must not eat the frame: {row:?}"
    );
}

#[test]
fn menu_list_shows_popup_style_scrollbar_on_overflow() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Controls);
    assert!(app.menu_rows.len() > 6, "need overflow for a scrollbar");
    // short card: only a window of rows fits, the rest scrolls
    let area = Rect::new(0, 0, 80, 12);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let symbols: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(
        symbols.contains('▐'),
        "overflowed menu must draw a thumb like the popup"
    );
}

#[test]
fn pinned_frame_resolves_against_real_card_not_stale_rect() {
    use crate::providers::Role;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut s = Session::new("m".into(), 1000);
    s.push(Role::User, "hi");
    s.pinned = true;
    app.sessions = vec![SessionHeader::from_session(&s)];
    // stale narrow rect, e.g. left over from the effort slider card
    app.menu_rect = Rect::new(0, 0, 52, 5);
    app.open_menu(Menu::Sessions);
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    // the draw-time check rebuilt rows for the real card
    let row_text =
        |y: u16| -> String { (0..area.width).map(|x| buf[(x, y)].symbol()).collect() };
    let hy = (0..area.height)
        .find(|y| row_text(*y).contains("pinned"))
        .expect("pinned header row");
    let row = row_text(hy);
    assert!(
        row.contains("┌") && row.contains("┐"),
        "header must carry the full frame: {row:?}"
    );
    // real card is 78 wide → 72-col frame + corners; a stale 52-wide
    // estimate would leave a 50-col header behind
    let start = row.find('┌').expect("frame start");
    let end = row.find('┐').expect("frame end") + '┐'.len_utf8();
    let frame = &row[start..end];
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(frame),
        74,
        "header must span the real card: {row:?}"
    );
}

#[test]
fn pinned_sessions_frame_stays_dim() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut s = Session::new("m".into(), 1000);
    s.pinned = true;
    app.sessions = vec![crate::session::SessionHeader::from_session(&s)];
    app.open_menu(Menu::Sessions);
    let area = Rect::new(0, 0, 100, 30);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let row_text =
        |y: u16| -> String { (0..area.width).map(|x| buf[(x, y)].symbol()).collect() };
    // the pinned header row (not the outer menu frame corner)
    let hy = (0..area.height)
        .find(|y| row_text(*y).contains("pinned"))
        .expect("pinned header row");
    let corner_x = (0..area.width)
        .find(|x| buf[(*x, hy)].symbol() == "┌")
        .expect("pinned frame corner");
    assert_eq!(buf[(corner_x, hy)].style().fg, Some(Color::DarkGray));
    // pinned row rails live strictly inside the outer menu frame
    let r = app.menu_rect;
    let rail = buf
        .content()
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u16 % area.width, c))
        .find(|(x, c)| c.symbol() == "│" && *x != r.x && *x != r.right().saturating_sub(1))
        .expect("pinned row rail");
    assert_eq!(rail.1.style().fg, Some(Color::DarkGray));
}
#[test]
fn pinned_session_frame_aligns_columns_and_respects_narrow_terminal() {
    use crate::providers::Role;
    use unicode_width::UnicodeWidthStr;

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut s = Session::new("m".into(), 1000);
    s.push(Role::User, "тестовая сессия (wide / cyrillic) 🚀");
    s.pinned = true;
    app.sessions = vec![SessionHeader::from_session(&s)];
    app.menu_rect.width = 40;
    app.cache_w = 40;
    app.open_menu(Menu::Sessions);

    let render_line = |l: &ratatui::text::Line| -> String {
        l.spans.iter().map(|sp| sp.content.as_ref()).collect()
    };

    let top_row = app
        .menu_rows
        .iter()
        .find(|(l, _)| render_line(l).contains("pinned"))
        .map(|(l, _)| render_line(l))
        .expect("top border");
    let bot_row = app
        .menu_rows
        .iter()
        .find(|(l, _)| render_line(l).starts_with('└'))
        .map(|(l, _)| render_line(l))
        .expect("bottom border");
    let content_row = app
        .menu_rows
        .iter()
        .find(|(l, _)| render_line(l).starts_with('│'))
        .map(|(l, _)| render_line(l))
        .expect("content row");

    let w_top = UnicodeWidthStr::width(top_row.as_str());
    let w_bot = UnicodeWidthStr::width(bot_row.as_str());
    let w_row = UnicodeWidthStr::width(content_row.as_str());

    assert_eq!(w_top, w_bot, "top and bottom borders must match");
    assert_eq!(w_top, w_row, "row and borders must match in display width");
    assert!(w_top <= 40, "must fit within menu rect width: {w_top} > 40");
    // the closing rail seals the row: padding lives inside the frame,
    // never parked after the rail (right rail must not drift left)
    assert!(
        content_row.ends_with('│'),
        "trailing pad leaked past the rail: {content_row:?}"
    );
}

#[test]
fn sessions_rows_share_date_model_token_columns() {
    use crate::providers::Role;
    use unicode_width::UnicodeWidthStr;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut a = Session::new("m".into(), 1000);
    a.push(Role::User, "hi");
    let mut b = Session::new("m".into(), 1000);
    b.push(Role::User, "a medium length title!");
    app.sessions = vec![
        SessionHeader::from_session(&a),
        SessionHeader::from_session(&b),
    ];
    app.open_menu(Menu::Sessions);
    let rows: Vec<String> = app
        .menu_rows
        .iter()
        .filter(|(_, act)| matches!(act, MenuAction::OpenSession(_)))
        .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // `{title:28}  {date:11}  {model:16}  {tok:>7}` — every row 68 cols
    // (the leading space lives inside the title cell)
    for r in &rows {
        assert_eq!(UnicodeWidthStr::width(r.as_str()), 68, "{r:?}");
        let date = &r[30..41];
        assert!(
            date.as_bytes()[2] == b'.' && date.as_bytes()[8] == b':',
            "date must start at column 30: {r:?}"
        );
        assert!(!r.contains("tok"), "token unit must be gone: {r:?}");
    }
    assert!(rows[0].contains("hi"));
    assert!(rows[1].contains("a medium length title!"));
}

fn project_session(title: &str, project: Option<std::path::PathBuf>) -> Session {
    use crate::providers::Role;
    let mut s = Session::new("m".into(), 1000);
    s.push(Role::User, title);
    s.project = project;
    s
}

#[test]
fn sessions_menu_splits_foreign_projects() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let root = app.project_root.clone();
    let foreign_root = root.join("definitely-not-this-project");
    let mine = project_session("mine task", Some(root));
    let legacy = project_session("legacy task", None);
    let away = project_session("away task", Some(foreign_root));
    app.sessions = vec![
        SessionHeader::from_session(&mine),
        SessionHeader::from_session(&away),
        SessionHeader::from_session(&legacy),
    ];
    app.open_menu(Menu::Sessions);
    let all: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let joined = all.join("\n");
    assert!(joined.contains("other projects"), "{joined:?}");
    let div = all
        .iter()
        .position(|r| r.contains("other projects"))
        .unwrap();
    let pos = |needle: &str| all.iter().position(|r| r.contains(needle)).unwrap();
    // current + legacy above the divider, foreign below it
    assert!(pos("mine task") < div, "{joined:?}");
    assert!(pos("legacy task") < div, "{joined:?}");
    assert!(pos("away task") > div, "{joined:?}");
}

#[test]
fn apply_session_warns_on_foreign_project() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let foreign_root = app.project_root.join("definitely-not-this-project");
    let mut away = project_session("away", Some(foreign_root));
    away.push(crate::providers::Role::User, "more");
    app.apply_session(away);
    let toast = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(toast.contains("another project"), "{toast:?}");

    // same project (and legacy) open quietly
    let root = app.project_root.clone();
    let home = project_session("home", Some(root));
    app.apply_session(home);
    let toast = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(!toast.contains("another project"), "{toast:?}");
    let legacy = project_session("old", None);
    app.apply_session(legacy);
    let toast = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(!toast.contains("another project"), "{toast:?}");
}

#[test]
fn session_switch_drops_stale_notices() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.effort_observed_ignored = Some(("m".into(), EffortLevel::High, "nope".into()));
    app.retry_line = Some("retry #1 in 5s — boom".into());
    app.last_checkpoint = Some("cp".into());
    app.prev_turn_ok = true;
    app.retry_notified = false;
    app.apply_session(Session::new("m".into(), 1000));
    assert!(app.effort_observed_ignored.is_none());
    assert!(app.retry_line.is_none());
    assert!(app.last_checkpoint.is_none());
    assert!(!app.prev_turn_ok);
    assert!(app.retry_notified);
}

#[test]
fn start_new_session_stamps_project() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(app.start_new_session());
    assert_eq!(app.session.project, Some(app.project_root.clone()));
}

/// The resume notice is one-shot: armed by a genuine restore (session
/// with history, real compaction), consumed by the first request. An
/// always-on notice taught the model that context is restored every
/// turn, and it re-verified the plan before each step.
#[test]
fn resume_notice_arms_once_and_disarms_on_use() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(!app.resume_notice_armed);
    // empty session: nothing to resume from
    let empty = Session::new("m".into(), 1000);
    app.apply_session(empty);
    assert!(!app.resume_notice_armed);
    // session with history: genuine restore, arm once
    let mut loaded = Session::new("m".into(), 1000);
    loaded.push(crate::providers::Role::User, "earlier work");
    app.apply_session(loaded);
    assert!(app.resume_notice_armed);
    // no open step on disk: nothing to say, but the flag still consumes
    app.system_block();
    assert!(!app.resume_notice_armed, "one-shot means one-shot");
    app.system_block();
    assert!(!app.resume_notice_armed);
}

#[test]
fn compaction_arms_resume_notice_only_on_real_change() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.note_compaction(false, 500, 500);
    assert!(!app.resume_notice_armed, "a no-op compact restores nothing");
    app.note_compaction(true, 9000, 1000);
    assert!(app.resume_notice_armed, "a real compaction is a restore");
}

/// A retry that recovered leaves no scar: the transient notice is
/// retracted on success. A terminal failure keeps its explanation.
#[test]
fn successful_finish_retracts_transient_retry_notice() {
    fn statuses(app: &App) -> Vec<String> {
        app.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Status { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Status {
        text: "request failed — retrying with backoff: boom".into(),
        kind: StatusKind::Err,
        expanded: false,
        transient: true,
    });
    app.push_segment(Segment::Status {
        text: "error: earlier".into(),
        kind: StatusKind::Err,
        expanded: false,
        transient: false,
    });
    app.retry_notified = true;
    app.finish_turn_inner(Ok(()), false);
    let texts = statuses(&app);
    assert!(
        !texts.iter().any(|t| t.contains("retrying")),
        "transient notice must go: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("earlier")),
        "durable notes stay: {texts:?}"
    );
    assert!(!app.retry_notified, "next turn must notify again");

    // terminal failure: the explanation stays
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Status {
        text: "request failed — retrying with backoff: boom".into(),
        kind: StatusKind::Err,
        expanded: false,
        transient: true,
    });
    app.finish_turn_inner(Err("boom".into()), false);
    let texts = statuses(&app);
    assert!(
        texts.iter().any(|t| t.contains("retrying")),
        "unfinished answer stays explained: {texts:?}"
    );
}

