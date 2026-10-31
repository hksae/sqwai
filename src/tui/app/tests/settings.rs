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
    let ask = |picked: Vec<Vec<bool>>| {
        Box::new(crate::tui::app::view::AskState {
            id: 1,
            questions: q.clone(),
            picked,
            custom: vec![String::new()],
            focus: 0,
            cursor: vec![0],
        })
    };
    let tool = |ask| Segment::Tool {
        name: "ask_user".into(),
        args: String::new(),
        call_id: None,
        ok: None,
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: true,
        ask,
    };
    let seg1 = tool(Some(ask(vec![vec![true, false, false]])));
    let seg2 = tool(Some(ask(vec![vec![false, true, false]])));
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

    app.open_menu(Menu::EditModel {
        provider: "anthropic".into(),
        key: Some("sonnet".into()),
    });
    let mtitle = app.menu_title();
    assert!(
        !mtitle.contains('(') && !mtitle.contains(')'),
        "mtitle has parens: {mtitle}"
    );
    assert!(mtitle.contains("Model · anthropic"));

    // The one live form is the single-field editor; drive it the way a click
    // does and check the selection gestures, not a focus walk across fields
    app.open_menu(crate::tui::app::menus::Menu::EditModelField {
        provider: "anthropic".into(),
        key: Some("sonnet".into()),
        field: super::super::menus::ModelField::RequestId,
    });
    assert_eq!(app.form_fields.len(), 1);
    app.menu_rect = Rect {
        x: 10,
        y: 10,
        width: 60,
        height: 10,
    };
    assert_eq!(app.form_focus, 0);

    // surface fields sit past the top rule and the title row
    let text_x = app.menu_rect.x + 2;
    let field_row = app.menu_rect.y + 3;
    app.form_mouse_down(field_row, text_x + 2);
    assert!(app.form_is_selecting());

    // Drag to extend the selection; release copies and leaves it standing
    app.form_mouse_drag(field_row, text_x + 5);
    assert!(app.form_is_selecting());
    app.form_mouse_up();

    // A click left of the text parks the cursor and drops the selection
    app.form_mouse_down(field_row, app.menu_rect.x);
    assert!(!app.form_is_selecting(), "label side must not select");
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
fn subagents_shortcuts_are_freed_ctrl_a_still_selects_all() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();

    // Ctrl+B opens nothing (the subagents list menu is gone)
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.cur_menu().is_none());
    app.menu_back();
    assert!(app.cur_menu().is_none());

    // Ctrl+Shift+A opens nothing either
    tx.send(crossterm::event::Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(app.cur_menu().is_none());
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

    // Open a form menu (e.g. EditProviderField)
    app.open_menu(Menu::EditProviderField {
        name: None,
        field: crate::tui::app::menus::ProviderField::Name,
    });
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
async fn startup_empty_enter_ignores_foreign_global_plan() {
    use crate::plan::{Limits, NewStep};
    let (_url, _h) = mock_sse_server("x", "y");
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    // temp project with an active plan that belongs to nobody here:
    // Enter must not silently adopt it (#171) — the Plan menu agrees
    // (session_plan() is None too)
    let temp_dir =
        std::env::temp_dir().join(format!("sqwai-test-enter-foreign-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    app.project_root = temp_dir.clone();
    let limits = Limits { max_steps: 10 };
    let foreign = crate::plan::create(
        "someone else's goal".into(),
        vec![],
        vec!["note".to_string()],
        vec![NewStep {
            title: "foreign step".into(),
            check: None,
        }],
        &limits,
    )
    .unwrap();
    crate::plan::store(&temp_dir, &foreign).unwrap();
    assert!(app.input_text().trim().is_empty());
    app.submit();
    assert!(app.startup, "no silent adoption: stays fresh");
    assert!(
        app.session.messages.is_empty(),
        "no turn started from a foreign plan"
    );
    // linked plan: Enter continues it as before
    app.session.plan_id = Some(foreign.id.clone());
    app.submit();
    assert!(!app.startup);
    assert_eq!(app.session.messages.len(), 1);
    let msg = &app.session.messages[0].content;
    assert!(msg.starts_with("Continue next plan step"), "{msg:?}");
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[test]
fn empty_session_survives_command_popup() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.input = App::fresh_input("/help".into());

    app.submit();

    // a command popup no longer dismisses the fresh session: emptiness
    // belongs to the session, so it must survive opening/closing
    // menus like /help or /plan
    assert!(app.startup);
    assert!(matches!(app.cur_menu(), Some(Menu::Help)));
}

#[test]
fn empty_session_survives_menu_close() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    app.input = App::fresh_input("/help".into());
    app.submit();
    app.menu_home();

    // closing the popup restores the fresh session
    assert!(app.startup);
    assert!(app.menu_stack.is_empty());
}

#[test]
fn session_switch_back_to_empty_session_marks_fresh() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;
    let mut s = Session::new("p/m".into(), 10_000);
    s.push(Role::User, "hi");
    app.apply_session(s);
    assert!(!app.startup);

    app.apply_session(Session::new("p/m".into(), 10_000));

    assert!(app.startup, "empty session must stay fresh");
}

fn open_test_approval(app: &mut App) {
    app.open_menu(Menu::Approval {
        id: 7,
        subject: "rm -rf x".into(),
        reason: "test".into(),
        grant: "rm -rf x".into(),
        allow_always: true,
        expanded: false,
    });
}

fn approval_option_index(app: &App, decision: ApprovalDecision) -> usize {
    app.menu_rows
        .iter()
        .position(|(_, a)| matches!(a, MenuAction::DecideApproval(d) if *d == decision))
        .expect("approval option row must exist")
}

/// Red belongs to the refusal. The two granting rows are plain text — with all
/// three painted pink, the safe action and the stopping action read as one
/// kind of thing. The hint follows the shape every other menu uses.
#[test]
fn approval_paints_only_deny_in_red() {
    use crate::tui::theme::Theme;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    let style = |d: ApprovalDecision| -> ratatui::style::Style {
        app.menu_rows[approval_option_index(&app, d)].0.spans[0].style
    };
    assert_eq!(style(ApprovalDecision::RunOnce), Theme::base());
    assert_eq!(style(ApprovalDecision::AlwaysSession), Theme::base());
    assert_eq!(style(ApprovalDecision::Deny), Theme::err());
    assert_eq!(
        app.menu_footer_text.as_deref(),
        Some("enter: confirm · esc: deny")
    );
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
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    let area = Rect::new(0, 0, 80, 20);
    let mut buf = Buffer::empty(area);
    app.render_into(&mut buf, area);
    let want_idx = approval_option_index(&app, ApprovalDecision::RunOnce);
    let screen_row = app
        .surf_hits
        .iter()
        .find(|(_, idx)| *idx == want_idx)
        .map(|(y, _)| *y)
        .expect("run once row in surf_hits");
    app.menu_click(screen_row);
    assert_eq!(app.menu_sel, want_idx);
    assert!(
        matches!(app.cur_menu(), Some(Menu::Approval { .. })),
        "a click must never approve"
    );
}

#[test]
fn approval_letter_selects_and_digits_are_ignored() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    open_test_approval(&mut app);
    app.approval_select(ApprovalDecision::AlwaysSession);
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::AlwaysSession)
    );
    assert!(matches!(app.cur_menu(), Some(Menu::Approval { .. })));

    // digits used to be shortcuts here; on the one dialog that runs a shell
    // command they must be inert, whatever option they once named
    let (tx, rx) = std::sync::mpsc::channel();
    for digit in ['1', '2', '3'] {
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char(digit),
            KeyModifiers::empty(),
        )))
        .unwrap();
        app.poll_input(&rx).unwrap();
    }
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::AlwaysSession),
        "a digit must not move the approval selection"
    );
    assert!(
        matches!(app.cur_menu(), Some(Menu::Approval { .. })),
        "a digit must not decide the approval"
    );
}

#[test]
fn approval_without_always_hides_the_option() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Approval {
        id: 8,
        subject: "webfetch https://example.com".into(),
        reason: "network read in ask mode".into(),
        grant: String::new(),
        allow_always: false,
        expanded: false,
    });
    assert!(
        !app.menu_rows.iter().any(|(_, a)| matches!(
            a,
            MenuAction::DecideApproval(ApprovalDecision::AlwaysSession)
        )),
        "a per-call approval must not offer always"
    );
    assert!(
        app.menu_rows.iter().all(|(line, _)| {
            !line
                .spans
                .iter()
                .any(|span| span.content.contains("always allow"))
        }),
        "the always row must be absent, not just disabled"
    );
    // a is inert when there is no row to select
    app.approval_select(ApprovalDecision::AlwaysSession);
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::Deny)
    );
}

#[test]
fn approval_long_subject_expands_on_request() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Approval {
        id: 9,
        subject: "line1\nline2\nline3\nline4\nline5\nline6".into(),
        reason: "test".into(),
        grant: "edit *".into(),
        allow_always: true,
        expanded: false,
    });
    let text = |app: &App| {
        app.menu_rows
            .iter()
            .map(|(line, _)| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let compact = text(&app);
    assert!(compact.contains("line1"), "{compact}");
    assert!(
        !compact.contains("line6"),
        "preview must be capped: {compact}"
    );
    assert!(compact.contains("show all"), "{compact}");
    let expand = app
        .menu_rows
        .iter()
        .position(|(_, a)| matches!(a, MenuAction::ApprovalExpand))
        .expect("expand row");
    app.menu_sel = expand;
    app.approval_opened_at =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(600));
    app.menu_activate();
    let expanded = text(&app);
    assert!(
        expanded.contains("line6"),
        "the tail must be reachable: {expanded}"
    );
    assert!(
        !app.menu_rows
            .iter()
            .any(|(_, a)| matches!(a, MenuAction::ApprovalExpand)),
        "the expand row disappears once expanded"
    );
    assert_eq!(
        app.menu_sel,
        approval_option_index(&app, ApprovalDecision::Deny),
        "expansion must land on the safe answer"
    );
}

#[test]
fn ask_mode_test_cycles_the_whole_gallery_and_sends_nothing() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.experimental_test = true;
    app.command("test ask-mode");
    assert!(app.test_approval_active);
    let total = app.test_approval_total;
    assert_eq!(total, 12, "the gallery is twelve dialogs deep");
    let mut subjects = Vec::new();
    for _ in 0..total {
        match app.cur_menu() {
            Some(Menu::Approval { subject, .. }) => subjects.push(subject.clone()),
            _ => panic!("expected an approval dialog"),
        }
        app.approval_decide(ApprovalDecision::RunOnce);
    }
    assert_eq!(subjects.len(), total);
    assert!(app.cur_menu().is_none(), "the sequence must end");
    assert!(!app.test_approval_active);
    assert!(app.agent.is_none(), "the test must not reach an agent");
    assert!(
        subjects.iter().any(|s| s.contains("webfetch")),
        "the network layout is part of the run: {subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s.contains("write src/parser.rs")),
        "the write summary is part of the run: {subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s.contains("more outside")),
        "the fused outside read is part of the run: {subjects:?}"
    );
    assert!(
        subjects.iter().any(|s| s.lines().count() >= 50),
        "the long script is part of the run: {subjects:?}"
    );
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

/// Shared insta filter: normalises the fixture directory label.
///
/// Fixtures always render with the literal label `sqwai` (pinned in
/// `render_to_string`, not read from the environment), so the pattern
/// is a constant rather than built from the actual checkout directory.
/// Anchored at end of line on purpose: the directory label is the last
/// thing on the status row.
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
        ask: None,
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
        ask: None,
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
        ask: None,
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
fn snap_chat_twelve_successful_tool_calls() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Do many things.".into()));
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
            ask: None,
        });
    }
    app.push_segment(Segment::Assistant {
        text: "All done!".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_twelve_tools_{}x{}", w_h.0, w_h.1), s);
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
        ask: None,
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
fn snap_chat_many_tools_flat() {
    // Flat transcript: 6 calls (one failed), finished thoughts hidden,
    // every tool row visible, no footer
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("Complex task.".into()));
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
            ask: None,
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
        ask: None,
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
        ask: None,
    });
    app.push_segment(Segment::Assistant {
        text: "Completed with one error.".into(),
        live: false,
    });
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_many_tools_flat_{}x{}", w_h.0, w_h.1), s);
    }
}

#[test]
fn snap_chat_models_surface() {
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
        snap!(format!("chat_models_surface_{}x{}", w_h.0, w_h.1), s);
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
    app.open_menu(Menu::Plan);
    for w_h in [(100u16, 30u16), (70, 24)] {
        let s = render_to_string(&mut app, w_h.0, w_h.1);
        snap!(format!("chat_plan_panel_{}x{}", w_h.0, w_h.1), s);
    }
}

// ── Behavioural tests (no snapshot) ───────────────────────────────────

#[tokio::test]
async fn after_first_message_startup_info_block_gone() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = true;

    // Fresh session before the first message
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

/// The search provider is a pick-one menu, not a text field: the list names
/// the valid values and carries each provider's key state.
#[test]
fn search_provider_is_chosen_from_a_list() {
    use crate::config::SearchProvider;
    use crate::tui::app::menus::{Menu, MenuAction};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.open_menu(Menu::SearchProvider);
    let labels: Vec<&str> = app
        .menu_rows
        .iter()
        .filter_map(|(_, action)| match action {
            MenuAction::SetSearchProvider(provider) => Some(provider.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        vec!["random", "hosted", "exa", "parallel", "deepseek", "tavily", "firecrawl", "off"]
    );

    app.run_action(MenuAction::SetSearchProvider(SearchProvider::Tavily));
    assert_eq!(app.cfg.web.search_provider, SearchProvider::Tavily);
}

/// The Search row on the provider surface filters as it types, like the
/// Providers screen it copies.
#[test]
fn search_provider_surface_filters_while_typing() {
    use crate::config::SearchProvider;
    use crate::tui::app::menus::{Menu, MenuAction};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.open_menu(Menu::SearchProvider);
    app.run_action(MenuAction::FocusSearch);
    let send = |app: &mut App, code: KeyCode| {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Event::Key(KeyEvent::new(code, KeyModifiers::empty())))
            .unwrap();
        app.poll_input(&rx).unwrap();
    };
    send(&mut app, KeyCode::Char('v'));

    let labels: Vec<&str> = app
        .menu_rows
        .iter()
        .filter_map(|(_, action)| match action {
            MenuAction::SetSearchProvider(provider) => Some(provider.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels, vec![SearchProvider::Tavily.as_str()], "{labels:?}");
}

/// Search configuration lives behind /providers-search, not in /config:
/// the command opens the list, ctrl+e opens the selected provider's settings.
#[test]
fn search_provider_surface_opens_from_its_command() {
    use crate::config::SearchProvider;
    use crate::tui::app::menus::{Menu, MenuAction};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.command("providers-search");
    assert!(matches!(app.cur_menu(), Some(Menu::SearchProvider)));

    // /config no longer carries search rows
    app.open_menu(Menu::Settings);
    let settings_text: String = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!settings_text.contains("search provider"), "{settings_text}");
    app.menu_back();

    app.menu_sel = app
        .menu_rows
        .iter()
        .position(|(_, action)| {
            matches!(action, MenuAction::SetSearchProvider(SearchProvider::Tavily))
        })
        .expect("tavily row");
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();

    assert!(
        matches!(
            app.cur_menu(),
            Some(Menu::EditSearchProvider {
                provider: SearchProvider::Tavily
            })
        ),
        "ctrl+e must open the selected provider's editor"
    );
    // ctrl+e edits only; the active choice moves on Enter alone
    assert_eq!(app.cfg.web.search_provider, SearchProvider::Off);
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
    assert!(rows.iter().any(|row| row.starts_with("key")), "{rows:?}");
    assert!(
        !rows.iter().any(|row| row.starts_with("base url")),
        "the global base url row belongs to the active provider: {rows:?}"
    );

    // a stored key shows as bullets on the row, never as its value
    app.cfg.keys.insert("tavily".into(), "tvly-secret".into());
    app.build_menu_rows();
    let rows: String = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("
");
    assert!(rows.contains("key ••••••••••"), "{rows}");
    assert!(!rows.contains("tvly-secret"), "{rows}");

    // making it active brings the base url row in, still on ctrl+e
    app.menu_back();
    app.run_action(MenuAction::SetSearchProvider(SearchProvider::Tavily));
    app.command("providers-search");
    app.menu_sel = app
        .menu_rows
        .iter()
        .position(|(_, action)| {
            matches!(action, MenuAction::SetSearchProvider(SearchProvider::Tavily))
        })
        .expect("tavily row");
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
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
    assert!(rows.iter().any(|row| row.starts_with("base url")), "{rows:?}");
}

/// ctrl+t probes the selected search backend without selecting it. Off and
/// hosted answer immediately: there is nothing to call.
#[test]
fn search_provider_test_handles_off_and_hosted_without_network() {
    use crate::config::SearchProvider;
    use crate::tui::app::menus::Menu;

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.open_menu(Menu::SearchProvider);
    let hints = app.list_hints();
    assert!(
        hints.iter().any(|(key, what)| *key == "ctrl+t" && what == "test"),
        "{hints:?}"
    );

    app.check_search_provider(SearchProvider::Off);
    assert!(app.search_test_rx.is_none(), "off starts no probe");
    app.check_search_provider(SearchProvider::Hosted);
    assert!(app.search_test_rx.is_none(), "hosted has nothing to call");
}
