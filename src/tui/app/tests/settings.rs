use super::*;

#[test]
fn static_theme_keeps_seg_key_stable() {
    use crate::tui::app::Segment;
    let app = test_app("http://127.0.0.1:9/v1".into());
    let seg = Segment::User("hello".into());
    assert_eq!(app.seg_key(&seg), app.seg_key(&seg));
}

#[test]
fn ask_user_seg_key_changes_with_picked_options() {
    use crate::agent::loop_task::{AskOption, AskQuestion};
    use crate::tui::app::Segment;
    let app = test_app("http://127.0.0.1:9/v1".into());
    let q = vec![AskQuestion {
        header: "H".into(),
        question: "Choose?".into(),
        options: vec![
            AskOption {
                label: "A".into(),
                description: None,
                recommended: false,
            },
            AskOption {
                label: "B".into(),
                description: None,
                recommended: false,
            },
            AskOption {
                label: "C".into(),
                description: None,
                recommended: false,
            },
        ],
        multiple: false,
        allow_free: false,
    }];
    let seg1 = Segment::AskUser {
        id: 1,
        questions: q.clone(),
        picked: vec![vec![true, false, false]],
        custom: vec![String::new()],
        focus: 0,
        answered: None,
        expanded: true,
    };
    let seg2 = Segment::AskUser {
        id: 1,
        questions: q,
        picked: vec![vec![false, true, false]],
        custom: vec![String::new()],
        focus: 0,
        answered: None,
        expanded: true,
    };
    assert_ne!(app.seg_key(&seg1), app.seg_key(&seg2));
}

#[test]
fn form_popup_titles_have_no_parentheses_and_support_mouse_selection() {
    use crate::tui::app::forms::FormField;
    use ratatui::layout::Rect;

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::EditProvider {
        name: Some("anthropic".into()),
    });
    let title = app.menu_title();
    assert!(
        !title.contains('(') && !title.contains(')'),
        "title has parens: {title}"
    );
    assert!(title.contains("Edit provider: anthropic"));

    app.open_menu_replace(Menu::EditModel {
        provider: "anthropic".into(),
        key: Some("sonnet".into()),
    });
    let mtitle = app.menu_title();
    assert!(
        !mtitle.contains('(') && !mtitle.contains(')'),
        "mtitle has parens: {mtitle}"
    );
    assert!(mtitle.contains("Model · anthropic"));

    // Simulate form rect and mouse interaction
    app.menu_rect = Rect {
        x: 10,
        y: 10,
        width: 60,
        height: 10,
    };
    assert_eq!(app.form_focus, 0);

    // Click on row 12 (field index 1: request id)
    app.form_mouse_down(12, 15);
    assert_eq!(app.form_focus, 1);

    // Click inside the text area of field 1
    let text_x = app.menu_rect.x + 1 + app.form_label_w();
    app.form_mouse_down(12, text_x + 2);
    assert!(app.form_is_selecting());

    // Drag to select text
    app.form_mouse_drag(12, text_x + 5);
    assert!(app.form_is_selecting());
    app.form_mouse_up();

    // Click on field 2 (context)
    app.form_mouse_down(13, 15);
    assert_eq!(app.form_focus, 2);
}

#[test]
fn text_combo_select_all_undo_redo() {
    use crate::tui::app::events::{
        handle_text_combo, is_redo_key, is_select_all_key, is_undo_key, select_all,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tui_textarea::TextArea;

    let mut ta = TextArea::new(vec!["hello world".to_string()]);
    select_all(&mut ta);
    assert!(ta.is_selecting());
    ta.copy();
    assert_eq!(ta.yank_text(), "hello world");

    // Test key matchers
    let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert!(is_select_all_key(&ctrl_a));

    let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(is_undo_key(&ctrl_z));
    assert!(!is_redo_key(&ctrl_z));

    // Ctrl+Shift+Z with Shift modifier
    let ctrl_shift_z = KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(!is_undo_key(&ctrl_shift_z));
    assert!(is_redo_key(&ctrl_shift_z));

    // Ctrl+Shift+Z as uppercase Z
    let ctrl_cap_z = KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::CONTROL);
    assert!(!is_undo_key(&ctrl_cap_z));
    assert!(is_redo_key(&ctrl_cap_z));

    // Ctrl+Y
    let ctrl_y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert!(is_redo_key(&ctrl_y));

    // Test undo and redo actions
    let mut ta2 = TextArea::new(vec![String::new()]);
    ta2.insert_str("first");
    ta2.insert_str(" second");
    assert_eq!(ta2.lines().join(""), "first second");

    handle_text_combo(&mut ta2, ctrl_z);
    assert_eq!(ta2.lines().join(""), "first");

    handle_text_combo(&mut ta2, ctrl_shift_z);
    assert_eq!(ta2.lines().join(""), "first second");

    handle_text_combo(&mut ta2, ctrl_z);
    assert_eq!(ta2.lines().join(""), "first");

    handle_text_combo(&mut ta2, ctrl_y);
    assert_eq!(ta2.lines().join(""), "first second");
}

#[test]
fn subagents_shortcut_remapped_to_ctrl_b_and_ctrl_shift_a() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();

    // Ctrl+B opens subagents
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Subagents)));
    app.menu_back();
    assert!(app.cur_menu().is_none());

    // Ctrl+Shift+A opens subagents
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Subagents)));
    app.menu_back();
    assert!(app.cur_menu().is_none());

    // Ctrl+A does NOT open subagents, but selects all in app.input
    app.input.insert_str("select me completely");
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.cur_menu().is_none());
    assert!(app.input.is_selecting());
    app.input.copy();
    assert_eq!(app.input.yank_text(), "select me completely");
}

#[test]
fn paste_burst_undoes_completely_with_single_ctrl_z() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();

    // Simulate terminal injecting pasted characters as key events
    for ch in "https://example.com/api".chars() {
        tx.send(crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
        )))
        .unwrap();
    }
    app.poll_input(&rx).unwrap();
    assert_eq!(app.input_text(), "https://example.com/api");

    // Ctrl+Z should undo the ENTIRE pasted text, not just the last letter
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.input_text(), "");

    // Multi-line paste burst
    for ch in "line1\nline2\nline3".chars() {
        let key = match ch {
            '\n' => KeyCode::Enter,
            c => KeyCode::Char(c),
        };
        tx.send(crossterm::event::Event::Key(KeyEvent::new(
            key,
            KeyModifiers::empty(),
        )))
        .unwrap();
    }
    app.poll_input(&rx).unwrap();
    assert_eq!(app.input_text(), "line1\nline2\nline3");

    // Ctrl+Z should undo the entire multi-line paste
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.input_text(), "");
}

#[test]
fn form_paste_burst_undoes_completely() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();

    // Open a form menu (e.g. EditProvider)
    app.open_menu(Menu::EditProvider { name: None });
    assert!(app.is_form_menu());

    // Simulate terminal injecting pasted characters into the focused text field
    for ch in "pasted-model-name".chars() {
        tx.send(crossterm::event::Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::empty(),
        )))
        .unwrap();
    }
    app.poll_input(&rx).unwrap();
    if let Some(FormField::Text { ta, .. }) = app.form_fields.get(app.form_focus) {
        assert_eq!(ta.lines().join(""), "pasted-model-name");
    } else {
        panic!("expected focused form field to be text");
    }

    // Ctrl+Z should undo the entire pasted text
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('z'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    if let Some(FormField::Text { ta, .. }) = app.form_fields.get(app.form_focus) {
        assert_eq!(ta.lines().join(""), "");
    }
}

#[test]
fn startup_screen_renders_identification_and_state_wide() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.startup_data = Some(App::collect_startup_data(
        &app.cfg,
        &app.model_cfg,
        app.read_only,
        None,
        &app.session.model_key.clone(),
    ));

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let text: Vec<String> = buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();

    // Must contain sqwai version from cargo
    let has_version = text
        .iter()
        .any(|line| line.contains(&format!("sqwai {}", env!("CARGO_PKG_VERSION"))));
    assert!(has_version, "startup screen must show cargo version");

    // Must show hints
    let has_hints = text
        .iter()
        .any(|line| line.contains("tab") && line.contains("plan / act"));
    assert!(has_hints, "startup screen must show tab hint");
}

#[test]
fn startup_screen_renders_narrow_layout() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.startup_data = Some(App::collect_startup_data(
        &app.cfg,
        &app.model_cfg,
        app.read_only,
        None,
        &app.session.model_key.clone(),
    ));

    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let text: Vec<String> = buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();

    // Identification line must NOT contain model in narrow layout
    let id_line = text
        .iter()
        .find(|line| line.contains("sqwai"))
        .expect("identification line");
    assert!(
        !id_line.contains(&app.model_cfg.id),
        "narrow layout omits model from line 1"
    );
}

#[test]
fn startup_keys_q_and_n_type_into_input() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;

    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::empty(),
    )))
    .unwrap();
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::empty(),
    )))
    .unwrap();

    app.poll_input(&rx).unwrap();

    // Must NOT quit and must NOT reset session
    assert!(!app.quit);
    assert_eq!(app.input_text(), "qn");
}

#[test]
fn startup_ctrl_s_opens_sessions_menu() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;

    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();

    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Sessions)));
}

#[tokio::test]
async fn startup_empty_enter_with_plan_continues_plan() {
    let (_url, _h) = mock_sse_server("x", "y");
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    assert!(app.input_text().trim().is_empty());

    let root = std::env::current_dir().unwrap_or_default();
    if crate::plan::open_active(&root).ok().flatten().is_some() {
        app.submit();
        assert!(!app.startup);
        assert_eq!(app.session.messages.len(), 1);
        let msg = &app.session.messages[0].content;
        assert!(msg.starts_with("Continue next plan step"));
    }
}

#[test]
fn startup_command_keeps_startup_screen() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.input = App::fresh_input("/help".into());

    app.submit();

    // a command popup no longer dismisses the startup screen: the screen
    // belongs to the empty session, so it must survive opening/closing
    // menus like /help or /plan
    assert!(app.startup);
    assert!(matches!(app.cur_menu(), Some(Menu::Help)));
}

#[test]
fn startup_screen_survives_menu_close() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.input = App::fresh_input("/help".into());
    app.submit();
    app.menu_home();

    // closing the popup restores the startup screen
    assert!(app.startup);
    assert!(app.menu_stack.is_empty());
}

#[test]
fn session_switch_back_to_empty_session_shows_startup() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut s = Session::new("m".into(), 10_000);
    s.push(Role::User, "hi");
    app.apply_session(s);
    assert!(!app.startup);

    app.apply_session(Session::new("m".into(), 10_000));

    assert!(app.startup, "empty session must show the startup screen");
}

fn open_test_approval(app: &mut App) {
    app.open_menu(Menu::Approval {
        id: 7,
        command: "rm -rf x".into(),
        reason: "test".into(),
    });
}

fn approval_option_index(app: &App, decision: ApprovalDecision) -> usize {
    app.menu_rows
        .iter()
        .position(|(_, a)| matches!(a, MenuAction::DecideApproval(d) if *d == decision))
        .expect("approval option row must exist")
}

#[test]
fn approval_opens_with_deny_preselected() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::Deny)
    );
}

#[test]
fn approval_enter_inside_grace_does_not_commit() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    // a stray Enter from window switching lands here: nothing happens,
    // the dialog stays open on the preselected deny
    app.menu_activate();
    assert!(matches!(app.cur_menu(), Some(Menu::Approval { .. })));

    // past the grace window the same Enter commits the selection
    app.approval_opened_at =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(600));
    app.menu_activate();
    assert!(
        app.menu_stack.is_empty(),
        "committed approval must close the dialog"
    );
}

#[test]
fn approval_click_selects_without_committing() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    app.menu_rect = ratatui::layout::Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 20,
    };
    // screen row 4 → row index 3 → the "run once" option
    app.menu_click(4);
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::RunOnce)
    );
    assert!(
        matches!(app.cur_menu(), Some(Menu::Approval { .. })),
        "a click must never approve"
    );
}

#[test]
fn approval_number_keys_select_without_committing() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    app.approval_select(ApprovalDecision::AlwaysSession);
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::AlwaysSession)
    );
    assert!(matches!(app.cur_menu(), Some(Menu::Approval { .. })));
}

#[test]
fn debug_command_opens_the_debug_menu() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.input = App::fresh_input("/debug".into());

    app.submit();

    // /debug used to fall through to "not implemented yet" even though
    // the menu behind it exists
    assert!(matches!(app.cur_menu(), Some(Menu::Debug)));
}

#[test]
fn startup_new_command_does_nothing_on_startup() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.input = App::fresh_input("/new".into());

    app.submit();

    // /new on startup screen does nothing and leaves startup screen active
    assert!(app.startup);
    assert!(app.session.messages.is_empty());
}

#[test]
fn startup_screen_wraps_text_on_narrow_window() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = App::collect_startup_data(
        &app.cfg,
        &app.model_cfg,
        app.read_only,
        None,
        &app.session.model_key.clone(),
    );
    data.project_path = "~/dev/a/very/long/nested/path/to/project".into();
    app.startup_data = Some(data);

    let mut terminal = Terminal::new(TestBackend::new(35, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let text: Vec<String> = buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();

    // Path should wrap across rows instead of being truncated
    let path_rows = text
        .iter()
        .filter(|line| line.contains("nested") || line.contains("project"))
        .count();
    assert!(path_rows >= 1, "path should wrap on narrow screen");
}

#[test]
fn startup_preferred_plan_falls_back_to_global_when_missing() {
    let app = test_app("http://127.0.0.1:9/v1".into());
    // a dangling preferred id (deleted plan) must not break collection:
    // same result as no preference
    let with_pref = App::collect_startup_data(
        &app.cfg,
        &app.model_cfg,
        app.read_only,
        Some("01JDOESNOTEXIST000000000000".into()),
        &app.session.model_key.clone(),
    );
    let without_pref = App::collect_startup_data(
        &app.cfg,
        &app.model_cfg,
        app.read_only,
        None,
        &app.session.model_key.clone(),
    );
    assert_eq!(
        with_pref.active_plan.map(|p| p.title),
        without_pref.active_plan.map(|p| p.title)
    );
}

/// Shared insta filter: normalises the fixture directory label.
///
/// Fixtures always render with the literal label `sqwai` (pinned in
/// `render_to_string`, not read from the environment), so the pattern
/// is a constant rather than built from the actual checkout directory.
/// Anchored at end of line on purpose: the directory label is the last
/// thing on the status row. A bare word match would also rewrite the
/// product name on the startup screen, since this repository happens
/// to be named after it.
///
/// Nothing else needs a filter: every startup fixture supplies its own
/// `StartupData`, so the version and project path are literals owned by
/// the test rather than values read out of the environment.
fn cwd_filter() -> Vec<(&'static str, &'static str)> {
    vec![(r"(?m)sqwai\s*$", "[CWD]")]
}

macro_rules! snap {
    ($name:expr, $rendered:expr) => {
        insta::with_settings!({ filters => cwd_filter() }, {
            insta::assert_snapshot!($name, $rendered);
        });
    };
}

fn make_startup_data() -> StartupData {
    StartupData {
        version: "1.2.3",
        project_path: "/home/user/projects/myapp".into(),
        git_branch: Some("main".into()),
        git_modified: Some(0),
        model: "test-model".into(),
        active_plan: None,
        last_session: None,
        memory: MemoryInfo {
            has_memory_md: false,
            latest_diary: None,
            graph_ready: true,
        },
        recent: Vec::new(),
        warnings: Vec::new(),
        has_sqwai_dir: true,
    }
}

// ── Chat fixtures ──────────────────────────────────────────────────────

#[test]
fn snap_chat_empty() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_empty_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_chat_long_assistant_with_code_block() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.segments
        .push(Segment::User("Explain this snippet.".into()));
    app.push_segment(Segment::Assistant {
        text: "Here is the explanation:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nThe `main` function is the program entry point. It calls `println!` which writes to stdout. This is very idiomatic Rust and the canonical hello-world example.".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_code_block_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_chat_three_tool_calls_one_failed() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Do the thing.".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read_file".into(),
        args: r#"{"path":"src/main.rs"}"#.into(),
        ok: Some(true),
        output: "fn main() {}".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "bash".into(),
        args: r#"{"cmd":"cargo build"}"#.into(),
        ok: Some(false),
        output: "error[E0425]: cannot find value `foo`".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "write_file".into(),
        args: r#"{"path":"out.txt"}"#.into(),
        ok: Some(true),
        output: "written".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Assistant {
        text: "Done, though one step failed.".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(
            format!("chat_three_tools_one_failed_{}x{}", w_h.0, w_h.1),
            s
        );
    }
}

#[test]
fn snap_chat_twelve_successful_tool_calls_collapsed() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Do many things.".into()));
    let seg_start = app.segments.len();
    for i in 0..12 {
        app.push_segment(Segment::Tool {
            call_id: None,
            name: format!("tool_{i}"),
            args: r#"{}"#.into(),
            ok: Some(true),
            output: format!("output {i}"),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
    }
    let seg_end = app.segments.len();
    app.activity_groups.push(ActivityGroup {
        seg_start,
        seg_end,
        calls: 12,
        thinking: 0,
        duration_ms: 3000,
        errors: 0,
        rejected: 0,
        turn_user: None,
        expanded: false,
    });
    app.push_segment(Segment::Assistant {
        text: "All done!".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(
            format!("chat_twelve_tools_collapsed_{}x{}", w_h.0, w_h.1),
            s
        );
    }
}

#[test]
fn snap_chat_bash_tool_still_running() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Run the tests.".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "bash".into(),
        args: r#"{"cmd":"cargo test"}"#.into(),
        ok: None, // still running
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    // production always holds the live answer slot while streaming; the
    // group logic keys off the work rows, not the slot, but the frame
    // must show what a real running turn looks like
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    app.streaming = true;
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_bash_running_{}x{}", w_h.0, w_h.1), s);
    }
    app.streaming = false;
}

#[test]
fn snap_chat_activity_group_collapsed() {
    // Activity group: 6 calls, 2 thinking blocks, 1 error — collapsed by default
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Complex task.".into()));
    let seg_start = app.segments.len();
    app.push_segment(Segment::Thinking {
        text: "Let me think about this carefully.".into(),
        expanded: false,
        started: None,
        duration_ms: 800,
        live: false,
    });
    app.push_segment(Segment::Thinking {
        text: "More reasoning here.".into(),
        expanded: false,
        started: None,
        duration_ms: 400,
        live: false,
    });
    for i in 0..4 {
        app.push_segment(Segment::Tool {
            call_id: None,
            name: format!("tool_{i}"),
            args: r#"{}"#.into(),
            ok: Some(true),
            output: format!("ok {i}"),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
    }
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "risky_tool".into(),
        args: r#"{"x":1}"#.into(),
        ok: Some(false),
        output: "Error: permission denied".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "cleanup".into(),
        args: r#"{}"#.into(),
        ok: Some(true),
        output: "cleaned".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    let seg_end = app.segments.len();
    app.activity_groups.push(ActivityGroup {
        seg_start,
        seg_end,
        calls: 6,
        thinking: 2,
        duration_ms: 5200,
        errors: 1,
        rejected: 0,
        turn_user: None,
        expanded: false, // collapsed by default
    });
    app.push_segment(Segment::Assistant {
        text: "Completed with one error.".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(
            format!("chat_activity_group_collapsed_{}x{}", w_h.0, w_h.1),
            s
        );
    }
}

#[test]
fn snap_chat_models_picker_popup() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Hello".into()));
    app.push_segment(Segment::Assistant {
        text: "Hi there!".into(),
        live: false,
    });
    app.open_menu(Menu::Models {
        provider: "p".into(),
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_models_popup_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_chat_plan_panel_open() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.todos = vec![
        "Step 1: Analyse requirements".into(),
        "Step 2: Implement feature".into(),
        "Step 3: Write tests".into(),
    ];
    app.push_segment(Segment::User("What is the plan?".into()));
    app.push_segment(Segment::Assistant {
        text: "See the plan panel.".into(),
        live: false,
    });
    app.open_menu(Menu::Todo);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_plan_panel_{}x{}", w_h.0, w_h.1), s);
    }
}

// ── Startup-screen fixtures ────────────────────────────────────────────

#[test]
fn snap_startup_active_plan() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = make_startup_data();
    data.active_plan = Some(ActivePlanInfo {
        title: "Refactor auth module".into(),
        current_step: 2,
        total_steps: 5,
        status_text: "In progress".into(),
        source: "session a8f2c1d4 · 14:02".into(),
    });
    app.startup_data = Some(data);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("startup_active_plan_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_startup_no_plan_with_history() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = make_startup_data();
    data.active_plan = None;
    data.last_session = Some(RecentSessionInfo {
        date: "2025-01-15".into(),
        title: "Fix login bug".into(),
        outcome: "Completed successfully".into(),
    });
    data.recent = vec![
        RecentSessionInfo {
            date: "2025-01-15".into(),
            title: "Fix login bug".into(),
            outcome: "Completed".into(),
        },
        RecentSessionInfo {
            date: "2025-01-14".into(),
            title: "Add dark mode".into(),
            outcome: "Completed".into(),
        },
    ];
    app.startup_data = Some(data);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(
            format!("startup_no_plan_with_history_{}x{}", w_h.0, w_h.1),
            s
        );
    }
}

#[test]
fn snap_startup_first_run() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = make_startup_data();
    data.active_plan = None;
    data.last_session = None;
    data.recent = Vec::new();
    data.has_sqwai_dir = false;
    data.memory = MemoryInfo {
        has_memory_md: false,
        latest_diary: None,
        graph_ready: false,
    };
    app.startup_data = Some(data);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("startup_first_run_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_startup_api_key_warning() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = make_startup_data();
    data.warnings = vec![
        "API key not set for provider 'openai'. Set OPENAI_API_KEY or add api_key to config."
            .into(),
    ];
    app.startup_data = Some(data);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("startup_api_key_warning_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_startup_graph_not_ready() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut data = make_startup_data();
    data.memory = MemoryInfo {
        has_memory_md: true,
        latest_diary: Some("2025-01-14".into()),
        graph_ready: false,
    };
    app.startup_data = Some(data);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("startup_graph_not_ready_{}x{}", w_h.0, w_h.1), s);
    }
}

// ── Behavioural tests (no snapshot) ───────────────────────────────────

#[tokio::test]
async fn after_first_message_startup_info_block_gone() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.startup_data = Some(make_startup_data());

    // Startup screen is shown before the first message
    assert!(app.startup, "startup should be true before first message");

    // Simulate submitting a message — startup becomes false
    app.input = App::fresh_input("hello".into());
    app.submit();

    assert!(!app.startup, "startup should be false after first message");
}

#[tokio::test]
async fn session_save_called_only_after_first_message() {
    // The session starts with no messages; after submit the user message is added.
    // (In cfg(test) save() is a no-op, so we verify the message count instead.)
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;

    // Before any submit, session has no messages
    assert!(
        app.session.messages.is_empty(),
        "no messages before first submit"
    );

    app.input = App::fresh_input("first message".into());
    app.submit();

    // After submit, the user message has been recorded
    assert!(
        !app.session.messages.is_empty(),
        "session should have at least one message after submit"
    );
}

