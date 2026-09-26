use super::*;

#[test]
fn experimental_section_sits_above_debug_and_toggles() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let pos = |a: &MenuAction| {
        app.menu_rows
            .iter()
            .position(|(_, act)| std::mem::discriminant(act) == std::mem::discriminant(a))
    };
    let exp = pos(&MenuAction::OpenExperimental).expect("experimental row");
    let dbg = pos(&MenuAction::OpenDebug).expect("debug row");
    assert!(exp + 1 == dbg, "experimental must sit right above debug");
    // toggle flips the persisted flag and rebuilds the row
    app.run_action(MenuAction::ToggleExperimentalTest);
    assert!(app.cfg.ui.experimental_test);
    app.open_menu(Menu::Experimental);
    assert_eq!(app.menu_rows.len(), 1, "one toggle row for now");
    app.run_action(MenuAction::ToggleExperimentalTest);
    assert!(!app.cfg.ui.experimental_test);
}

#[test]
fn tool_notice_arms_the_finish_wave() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.handle_tool_start("read".into(), "a.rs".into(), None);
    app.handle_tool_notice("read".into(), "done".into(), true, None, None);
    let seg = app
        .segments
        .iter()
        .find(|s| matches!(s, Segment::Tool { .. }))
        .expect("tool row");
    assert!(
        matches!(
            seg,
            Segment::Tool {
                ok: Some(true),
                flash: Some(_),
                ..
            }
        ),
        "notice resolves the row and arms the wave"
    );
}

#[test]
fn tool_notice_matches_call_id_not_position() {
    // two same-name rows open (parallel calls): the notice must
    // close its own row, not the last open one (rposition would)
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.handle_tool_start("bash".into(), "first".into(), Some("c1".into()));
    app.handle_tool_start("bash".into(), "second".into(), Some("c2".into()));
    app.handle_tool_notice(
        "bash".into(),
        "one done".into(),
        true,
        None,
        Some("c1".into()),
    );
    assert!(
        matches!(
            &app.segments[0],
            Segment::Tool {
                ok: Some(true),
                output,
                ..
            } if output.as_str() == "one done"
        ),
        "first notice closes the first row: {:?}",
        app.segments[0]
    );
    assert!(
        matches!(&app.segments[1], Segment::Tool { ok: None, .. }),
        "second row stays open: {:?}",
        app.segments[1]
    );
    app.handle_tool_notice(
        "bash".into(),
        "two done".into(),
        true,
        None,
        Some("c2".into()),
    );
    assert!(
        matches!(
            &app.segments[1],
            Segment::Tool {
                ok: Some(true),
                output,
                ..
            } if output.as_str() == "two done"
        ),
        "second notice closes the second row: {:?}",
        app.segments[1]
    );
}

#[test]
fn finish_wave_keeps_geometry_and_settles_static() {
    // env-independent: geometry holds on both paths, and an expired
    // wave renders exactly the static row
    let mut app = test_app("http://127.0.0.1:9/v1".into());
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
        flash: Some(std::time::Instant::now()),
    });
    let rows = app.render_segment(&app.segments, 0, 80, true);
    assert_eq!(rows.len(), 1, "head row only, like static");
    let text: String = rows[0].0.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.starts_with("  "), "{text:?}");
    assert!(text.contains("read"), "{text:?}");

    // expired flash: byte-identical static row
    if let Some(Segment::Tool { flash, .. }) = app.segments.get_mut(0) {
        *flash = Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
    }
    let rows = app.render_segment(&app.segments, 0, 80, true);
    assert_eq!(rows[0].0.spans[0].content.as_ref(), "  ✓ ");
    assert_eq!(
        rows[0].0.spans[0].style,
        crate::tui::theme::Theme::tool_head_bold()
    );
    assert_eq!(rows[0].0.spans[1].content.as_ref(), "read");
}

#[test]
fn expired_flash_key_matches_static_key() {
    // the live wave must not pin the cache: expiry returns the key to
    // the static one, so the row repaints settled (no stuck wave frame)
    let app = test_app("http://127.0.0.1:9/v1".into());
    let mk = |flash| Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash,
    };
    let expired = mk(Some(
        std::time::Instant::now() - std::time::Duration::from_secs(5),
    ));
    assert_eq!(app.seg_key(&mk(None)), app.seg_key(&expired));
}

#[test]
fn edit_model_form_keeps_xhigh_effort() {
    // the providers-menu form once offered only five levels: an xhigh
    // model showed `off`, and saving the untouched form clobbered it
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.models.get_mut("m").unwrap().effort = EffortLevel::Xhigh;
    app.open_menu_replace(Menu::EditModel {
        provider: "p".into(),
        key: Some("m".into()),
    });
    assert_eq!(app.form_fields[3].trimmed(), "xhigh");
    app.form_save();
    assert_eq!(app.cfg.models.get("m").unwrap().effort, EffortLevel::Xhigh);
}

#[test]
fn edit_model_form_options_match_control_cycle() {
    // the form must offer auto-then-every-control, like CycleEffortControl
    use super::super::forms::{ALWAYS_OPTS, EFFORT_CONTROL_OPTS};
    use crate::config::EffortControl;
    assert_eq!(EFFORT_CONTROL_OPTS[0], "auto");
    assert_eq!(&EFFORT_CONTROL_OPTS[1..], &EffortControl::STRS[..]);
    assert_eq!(ALWAYS_OPTS, &["off", "on"]);
}

#[test]
fn edit_model_form_round_trips_effort_declarations() {
    use crate::config::EffortControl;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let m = app.cfg.models.get_mut("m").unwrap();
    m.effort_control = Some(EffortControl::Budget);
    m.effort_always_on = true;
    app.open_menu_replace(Menu::EditModel {
        provider: "p".into(),
        key: Some("m".into()),
    });
    assert_eq!(app.form_fields[4].trimmed(), "budget");
    assert_eq!(app.form_fields[5].trimmed(), "on");
    app.form_save();
    let m = app.cfg.models.get("m").unwrap();
    assert_eq!(m.effort_control, Some(EffortControl::Budget));
    assert!(m.effort_always_on);
}

fn wheel_test_app() -> (crate::tui::app::App, usize) {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.cfg.ui.experimental_test = true;
    app.command("test animations");
    assert!(
        crate::tui::spinners::ALL.len() > 20,
        "gallery must be longer than one window"
    );
    // one draw so the wheel knows the window height
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    let vis = app.menu_visible_rows;
    assert!(vis > 0 && vis < app.menu_rows.len(), "vis={vis}");
    (app, vis)
}

#[test]
fn menu_wheel_scrolls_view_selection_stays() {
    let (mut app, vis) = wheel_test_app();
    let n = app.menu_rows.len();
    app.menu_wheel(3);
    assert_eq!(app.menu_sel, 0, "wheel must not move the selection");
    assert_eq!(app.menu_scroll, 3, "wheel must scroll the view");
    app.menu_wheel(-2);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 1));
    // top clamp
    app.menu_wheel(-100);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 0));
    // bottom clamp: no trailing empty space
    app.menu_wheel(10000);
    assert_eq!(app.menu_sel, 0);
    assert_eq!(
        app.menu_scroll,
        n - vis,
        "scroll={} n={n} vis={vis}",
        app.menu_scroll
    );
}

#[test]
fn menu_wheel_may_scroll_selection_out_of_view_and_draw_keeps_it() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let (mut app, vis) = wheel_test_app();
    app.menu_wheel(10000);
    assert!(
        app.menu_sel < app.menu_scroll || app.menu_sel >= app.menu_scroll + vis,
        "selection must be allowed out of the window"
    );
    // redraw must not snap the view back to the selection
    let scroll = app.menu_scroll;
    let area = Rect::new(0, 0, 80, 24);
    let mut buf = Buffer::empty(area);
    app.draw_menu(&mut buf, area);
    assert_eq!(app.menu_scroll, scroll, "draw must keep free scroll");
    assert_eq!(app.menu_sel, 0);
}

#[test]
fn menu_nav_and_jump_pull_view_after_selection() {
    let (mut app, vis) = wheel_test_app();
    let n = app.menu_rows.len();
    // scroll the selection out of view, then step: view follows sel
    app.menu_wheel(10000);
    app.menu_nav(1);
    assert_eq!(app.menu_sel, 1);
    assert_eq!(app.menu_scroll, 1, "view must follow the selection");
    // jump to end / start pulls the window along
    app.menu_jump(true);
    assert_eq!(app.menu_sel, n - 1);
    assert_eq!(app.menu_scroll, n - vis);
    app.menu_jump(false);
    assert_eq!((app.menu_sel, app.menu_scroll), (0, 0));
    // page jump also follows
    app.menu_nav(10);
    assert!(
        app.menu_sel < app.menu_scroll + vis,
        "page sel must be visible"
    );
    assert!(app.menu_sel >= app.menu_scroll, "page sel must be visible");
}

#[test]
fn question_mark_key_types_into_input() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('?'),
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.input_text(), "?");
    assert!(!app.segments.iter().any(|s| matches!(
        s,
        Segment::Status { text, .. } if text.contains("commands:")
    )));
}

#[test]
fn plan_show_alias_is_gone() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.plan_command("/plan show");
    assert!(
        matches!(&app.toast, Some(t) if t.text.contains("unknown /plan action")),
        "unknown action toasts"
    );
    // bare `/plan` still opens the overview
    app.plan_command("/plan");
    assert!(matches!(app.cur_menu(), Some(Menu::Plan)));
}

#[test]
fn form_label_column_fits_long_setting_names() {
    use super::super::menus::ScalarSetting;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // short-label form keeps the legacy 16-column layout
    app.open_menu(Menu::EditProvider { name: None });
    assert_eq!(app.form_label_w(), 16);
    // long scalar labels widen the column so the value never overlaps
    app.open_menu(Menu::EditScalar(ScalarSetting::MemoryLoadBudgetRatio));
    assert_eq!(app.form_label_w(), 21);
}

#[test]
fn plan_subcommand_popup_filters_and_inserts() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // level 2 visible after `/plan `, lists every subcommand
    app.input = App::fresh_input("/plan ".into());
    assert!(app.popup_visible());
    let items = app.popup_items();
    assert_eq!(items.len(), menus::subcommands_of("/plan").unwrap().len());
    assert!(items.contains(&"/plan waive".to_string()));
    // filtering by the typed prefix
    app.input = App::fresh_input("/plan w".into());
    assert!(app.popup_visible());
    assert_eq!(
        app.popup_items(),
        vec![
            "/plan waive".to_string(),
            "/plan waive-constraint".to_string()
        ]
    );
    // insert replaces only the subcommand word, keeps typed args
    app.input = App::fresh_input("/plan wai".into());
    app.apply_subcommand_insert("/plan", "waive");
    assert_eq!(app.input_text(), "/plan waive ");
    app.input = App::fresh_input("/plan waive 2".into());
    app.apply_subcommand_insert("/plan", "waive");
    assert_eq!(app.input_text(), "/plan waive 2");
    // other level-2 commands behave the same way
    app.input = App::fresh_input("/mode ".into());
    assert!(app.popup_visible());
    assert_eq!(
        app.popup_items(),
        vec!["/mode plan".to_string(), "/mode act".to_string()]
    );
    app.input = App::fresh_input("/undo s".into());
    assert_eq!(app.popup_items(), vec!["/undo step".to_string()]);
    app.input = App::fresh_input("/constraints a".into());
    assert_eq!(app.popup_items(), vec!["/constraints add".to_string()]);
    app.input = App::fresh_input("/providers ".into());
    assert_eq!(app.popup_items(), vec!["/providers update".to_string()]);
    // commands without subcommands keep level-1-only behavior
    app.input = App::fresh_input("/goal ".into());
    assert!(!app.popup_visible());
    app.input = App::fresh_input("/skill ".into());
    assert!(!app.popup_visible());
    // level 1 untouched
    app.input = App::fresh_input("/pl".into());
    assert!(app.popup_visible());
    assert_eq!(app.popup_items(), vec!["/plan".to_string()]);
}

/// @ completion: fragment detection, unified file candidates, insert
/// over the fragment with the cursor after it, hover navigation.
/// No graph index in the temp project, so only files complete here;
/// symbols ride the same rows through recall (covered in agent tests).
#[test]
fn mention_popup_completes_files_and_inserts() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp = tempfile::tempdir().unwrap();
    app.project_root = temp.path().to_path_buf();
    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(temp.path().join("src/lib.rs"), "lib\n").unwrap();
    app.refresh_mention_files();

    // no @, no mention popup (slash rules unchanged)
    app.input = App::fresh_input("hello".into());
    assert!(app.mention_fragment().is_none());
    app.input = App::fresh_input("/pl".into());
    assert!(app.mention_fragment().is_none());
    assert_eq!(app.popup_items(), vec!["/plan".to_string()]);

    // fragment under the cursor drives file candidates
    app.input = App::fresh_input("see @src/ma".into());
    let (start, end, frag) = app.mention_fragment().expect("fragment");
    assert_eq!(frag, "src/ma");
    assert_eq!(&app.input_text()[start..end], "@src/ma");
    let items = app.popup_items();
    assert!(app.popup_visible());
    assert_eq!(items, vec!["@file:src/main.rs".to_string()]);

    // hover navigation wraps; Tab-order accept takes hover-or-first
    assert!(app.mention_hover_by(1));
    assert_eq!(app.hover.as_deref(), Some("@file:src/main.rs"));
    app.apply_mention_insert(start, end, "@file:src/main.rs");
    assert_eq!(app.input_text(), "see @file:src/main.rs ");
    // inserted key resolves at send with no warnings
    let resolved =
        crate::agent::mentions::resolve_mentions(temp.path(), &app.input_text());
    assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
    assert!(resolved.text.contains("@file:src/main.rs#sha256:"));
    assert_eq!(resolved.pre_reads.len(), 1);
    let _ = std::fs::remove_dir_all(temp.path());
}

/// Unresolved @ stays literal with a warning, and the mention popup
/// never fires for mail addresses.
#[test]
fn mention_unresolved_stays_literal() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let temp = tempfile::tempdir().unwrap();
    app.project_root = temp.path().to_path_buf();
    app.refresh_mention_files();
    app.input = App::fresh_input("mail user@host x".into());
    assert!(app.mention_fragment().is_none());
    app.input = App::fresh_input("see @nosuchfile".into());
    assert!(app.popup_visible());
    assert!(app.popup_items().is_empty());
    let resolved =
        crate::agent::mentions::resolve_mentions(temp.path(), &app.input_text());
    assert_eq!(resolved.text, "see @nosuchfile");
    assert_eq!(resolved.warnings.len(), 1);
    assert!(resolved.pre_reads.is_empty());
    let _ = std::fs::remove_dir_all(temp.path());
}

/// Mention pre-reads register single-take: an abandoned submit cannot
/// poison a later turn, and the agent takes what the turn registered.
#[test]
fn mention_prereads_register_single_take() {
    use crate::agent::tools::{register_mention_prereads, take_mention_prereads};
    let sess = format!("mention-reg-{}", std::process::id());
    assert!(take_mention_prereads(&sess).is_empty());
    register_mention_prereads(&sess, vec!["a.rs".into()]);
    register_mention_prereads(&sess, vec!["b.rs".into()]);
    assert_eq!(take_mention_prereads(&sess), vec![std::path::PathBuf::from("b.rs")]);
    assert!(take_mention_prereads(&sess).is_empty());
    register_mention_prereads(&sess, Vec::new());
    assert!(take_mention_prereads(&sess).is_empty());
}

#[test]
fn busy_status_is_a_replacing_toast_and_expires() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.show_busy_status();
    app.show_busy_status();
    // one toast, never a chat segment
    assert!(
        app.toast
            .as_ref()
            .is_some_and(|t| t.text == App::BUSY_STATUS)
    );
    assert!(!app.segments.iter().any(|segment| {
        matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS)
    }));
    // expiry drops it
    app.toast.as_mut().expect("toast").until =
        std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(app.live_toast().is_none());
    assert!(app.toast.is_none());
}

#[test]
fn busy_statuses_are_deduplicated_and_cleared_on_finish() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.show_busy_status();
    app.show_busy_status();
    assert!(
        app.toast
            .as_ref()
            .is_some_and(|t| t.text == App::BUSY_STATUS)
    );
    app.streaming = true;
    app.finish_turn(Err("aborted".into()));
    assert!(!app.segments.iter().any(|segment| {
        matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS)
    }));
}
#[test]
fn edit_row_shows_colored_change_counts() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "edit".into(),
        args: "src/a-very-long-file-name.rs".into(),
        ok: Some(true),
        output: "done".into(),
        diff: Some("--- old\n+++ new\n-old\n+new\n+more".into()),
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    let rows = app.render_segment(&app.segments, 0, 80, true);
    let text: String = rows[0]
        .0
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("+2"));
    assert!(text.contains("-1"));
}

#[test]
fn expanded_tool_output_uses_left_rail_and_truncates() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "patch".into(),
        args: String::new(),
        ok: Some(true),
        output: "a very long line with wide chars 界界界界 and more text".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: true,
        flash: None,
    });
    let rows = app.render_segment(&app.segments, 0, 18, true);
    use unicode_width::UnicodeWidthStr;
    let text: Vec<String> = rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert!(text.iter().any(|line| line == "    │"));
    assert!(
        text.iter()
            .filter(|line| line.contains('│'))
            .all(|line| line.matches('│').count() == 1)
    );
    assert!(text.iter().any(|line| line.contains("…")));
    assert!(
        text.iter()
            .filter(|line| line.starts_with("    │ "))
            .all(|line| { UnicodeWidthStr::width(line.as_str()) <= 18 })
    );
}
#[test]
fn user_prompt_is_a_padded_full_width_surface_strip() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::User("one\ntwo".into()));
    let rows = app.render_segment(&app.segments, 0, 30, true);
    let text: Vec<String> = rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect()
        })
        .collect();
    assert_eq!(text.len(), 4, "one blank row above and below");
    assert!(text[0].trim().is_empty() && text[3].trim().is_empty());
    assert!(text[1].starts_with("› "), "first line carries the marker");
    assert!(
        text[2].starts_with("  ") && !text[2].starts_with("›"),
        "continuation aligns under the marker: {text:?}"
    );
    assert!(
        text.iter()
            .all(|line| !line.contains(['╭', '╮', '╰', '╯', '│'])),
        "user strip must have no frame: {text:?}"
    );
    assert!(
        rows.iter().all(|(line, _)| line
            .spans
            .iter()
            .all(|s| s.style.bg == Some(Theme::USER_SURFACE()))),
        "every user-strip cell must use the stronger user-strip background"
    );
}

#[test]
fn user_surface_strip_fills_the_entire_terminal_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("full width".into()));
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let buffer = terminal.backend().buffer();
    let user_row = (0..buffer.area.height)
        .find(|y| {
            (0..buffer.area.width)
                .any(|x| buffer.cell((x, *y)).is_some_and(|c| c.symbol() == "›"))
        })
        .expect("user prefix rendered");
    for y in [user_row - 1, user_row, user_row + 1] {
        for x in 0..buffer.area.width {
            assert_eq!(
                buffer.cell((x, y)).unwrap().bg,
                Theme::USER_SURFACE(),
                "row {y}, column {x} must be filled"
            );
        }
    }
}

#[test]
fn resized_terminal_rebuilds_tool_frames_at_chat_width() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "patch".into(),
        args: String::new(),
        ok: Some(true),
        output: "a very long line with wide chars 界界界界 and more text".into(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: true,
        flash: None,
    });
    app.startup = false;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.cache_w, 78);
    terminal.backend_mut().resize(30, 24);
    app.dirty = true;
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.cache_w, 28);
    let buffer = terminal.backend().buffer();
    for row in buffer.content.chunks(buffer.area.width as usize) {
        let text: String = row.iter().map(|cell| cell.symbol().to_string()).collect();
        if text.contains("│") {
            assert_eq!(text.matches("│").count(), 1, "broken tool rail row: {text}");
        }
    }
}

#[test]
fn settings_hub_reuses_existing_menus() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let labels: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref().to_string())
                .collect::<String>()
        })
        .collect();
    assert!(labels.iter().any(|row| row.contains("Appearance")));
    assert!(labels.iter().any(|row| row.contains("Providers")));
    assert!(labels.iter().any(|row| row.contains("MCP")));
    assert!(labels.iter().any(|row| row.contains("LSP")));
    assert!(labels.iter().any(|row| row.contains("Skills")));

    app.run_action(MenuAction::OpenAppearance);
    assert!(matches!(app.cur_menu(), Some(Menu::Appearance)));
    app.menu_back();
    app.run_action(MenuAction::OpenProviders);
    assert!(matches!(app.cur_menu(), Some(Menu::Providers)));
}

#[test]
fn settings_hub_rows_have_no_hint_subtitles() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let texts: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref().to_string())
                .collect::<String>()
        })
        .collect();
    assert!(!texts.iter().any(|t| t.contains("themes and UI")));
    assert!(!texts.iter().any(|t| t.contains("models and API")));
    assert!(texts.iter().any(|t| t.contains("Appearance")));
}

#[test]
fn providers_default_rows_are_spaced() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Providers);
    let texts: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref().to_string())
                .collect::<String>()
        })
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("default effort  ")),
        "value must not stick to the label: {texts:?}"
    );
}

#[test]
fn ctrl_shortcuts_open_providers_plan_settings() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();
    let press = |tx: &std::sync::mpsc::Sender<Event>, c: char| {
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    };
    press(&tx, 'p');
    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Providers)));
    app.menu_home();

    press(&tx, 'o');
    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Settings)));
    app.menu_home();

    press(&tx, 'l');
    app.poll_input(&rx).unwrap();
    assert!(matches!(app.cur_menu(), Some(Menu::Plan)));
}

#[test]
fn settings_hub_lists_agent_safety_undo_sections() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let labels: Vec<String> = app
        .menu_rows
        .iter()
        .map(|(line, _)| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref().to_string())
                .collect::<String>()
        })
        .collect();
    assert!(labels.iter().any(|row| row.contains("Agent")));
    assert!(labels.iter().any(|row| row.contains("Safety")));
    assert!(labels.iter().any(|row| row.contains("Undo")));

    app.run_action(MenuAction::OpenAgent);
    assert!(matches!(app.cur_menu(), Some(Menu::Agent)));
    app.menu_back();
    app.run_action(MenuAction::OpenSafety);
    assert!(matches!(app.cur_menu(), Some(Menu::Safety)));
    app.menu_back();
    app.run_action(MenuAction::OpenUndo);
    assert!(matches!(app.cur_menu(), Some(Menu::Undo)));
}

#[test]
fn settings_scalar_edit_valid_and_invalid_reset() {
    use super::super::menus::ScalarSetting;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::EditScalar(ScalarSetting::PlanMaxSteps));
    assert!(app.is_form_menu());
    // prefilled with the current value
    assert_eq!(app.form_fields[0].trimmed(), "24");

    app.form_fields[0] = super::super::forms::FormField::text("max steps", "32".into());
    app.form_save();
    assert_eq!(app.cfg.plan.max_steps, 32);

    // garbage resets to the default instead of corrupting the config
    app.open_menu(Menu::EditScalar(ScalarSetting::PlanMaxSteps));
    app.form_fields[0] = super::super::forms::FormField::text("max steps", "abc".into());
    app.form_save();
    assert_eq!(app.cfg.plan.max_steps, 24);
    // notices never land in chat segments anymore — this one toasts
    assert!(matches!(
        &app.toast,
        Some(t) if t.text.contains("reset to default")
    ));
}

#[test]
fn settings_cycles_and_toggles() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let effort = app.cfg.diary.effort;
    app.run_action(MenuAction::CycleDiaryEffort);
    assert_ne!(app.cfg.diary.effort.as_str(), effort.as_str());

    app.run_action(MenuAction::CycleCompactionSummary);
    assert_eq!(app.cfg.compaction.summary.as_str(), "off");
    app.run_action(MenuAction::CycleCompactionSummary);
    assert_eq!(app.cfg.compaction.summary.as_str(), "short");

    app.run_action(MenuAction::CycleUndoShadow);
    assert_eq!(app.cfg.undo.shadow.as_str(), "user");

    let auto = app.cfg.skills.auto_load;
    app.run_action(MenuAction::ToggleSkillsAutoLoad);
    assert_eq!(app.cfg.skills.auto_load, !auto);
}

#[test]
fn settings_safety_list_add_and_remove() {
    use super::super::menus::ListSection;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::AddListItem(ListSection::SafetyBlocked));
    app.form_fields[0] = super::super::forms::FormField::text("blocked patterns", "rm -rf /".into());
    app.form_save();
    assert!(
        app.cfg
            .safety
            .blocked_patterns
            .contains(&"rm -rf /".to_string())
    );

    app.run_action(MenuAction::DeleteListItem(ListSection::SafetyBlocked, 0));
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
    app.run_confirm_action();
    assert!(
        !app.cfg
            .safety
            .blocked_patterns
            .contains(&"rm -rf /".to_string())
    );
}

#[test]
fn switching_model_records_last_model() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut other = app.cfg.models["m"].clone();
    other.id = "other-model".into();
    app.cfg.models.insert("m2".into(), other);
    app.run_action(MenuAction::UseModel("m2".into()));
    assert_eq!(app.session.model_key, "m2");
    assert_eq!(app.cfg.last_model, "m2");
}

#[test]
fn settings_mcp_server_crud() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(app.cfg.mcp.servers.is_empty());

    // add (stdio)
    app.open_menu(Menu::EditMcpServer { index: None });
    let set = |app: &mut App, i: usize, v: &str| {
        let label = app.form_fields[i].label().to_string();
        app.form_fields[i] = super::super::forms::FormField::text(&label, v.into());
    };
    set(&mut app, 0, "files");
    set(&mut app, 2, "mcp-files");
    set(&mut app, 3, "--root /tmp");
    set(&mut app, 4, "TOKEN=abc");
    app.form_save();
    assert_eq!(app.cfg.mcp.servers.len(), 1);
    assert!(app.cfg.mcp.servers[0].enabled);

    // toggle off via the server submenu
    app.run_action(MenuAction::ToggleMcpServer(0));
    assert!(!app.cfg.mcp.servers[0].enabled);

    // edit keeps the toggle state
    app.open_menu(Menu::EditMcpServer { index: Some(0) });
    set(&mut app, 2, "mcp-files-v2");
    app.form_save();
    assert!(!app.cfg.mcp.servers[0].enabled);

    // delete through the confirmation prompt
    app.run_action(MenuAction::DeleteMcpServer(0));
    assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
    app.run_confirm_action();
    assert!(app.cfg.mcp.servers.is_empty());
}

#[test]
fn settings_lsp_server_add_and_delete() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::EditLspServer { index: None });
    let set = |app: &mut App, i: usize, v: &str| {
        let label = app.form_fields[i].label().to_string();
        app.form_fields[i] = super::super::forms::FormField::text(&label, v.into());
    };
    set(&mut app, 0, "rust");
    set(&mut app, 1, "rust");
    set(&mut app, 2, "rust-analyzer");
    app.form_save();
    assert_eq!(app.cfg.lsp.servers.len(), 1);
    assert_eq!(app.cfg.lsp.servers[0].language, "rust");

    app.run_action(MenuAction::DeleteLspServer(0));
    app.run_confirm_action();
    assert!(app.cfg.lsp.servers.is_empty());
}

#[test]
fn settings_skills_dirs_add_and_remove() {
    use super::super::menus::ListSection;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::AddListItem(ListSection::SkillsDirs));
    app.form_fields[0] = super::super::forms::FormField::text("skill directories", "/tmp/s".into());
    app.form_save();
    assert_eq!(app.cfg.skills.dirs.len(), 1);

    app.run_action(MenuAction::DeleteListItem(ListSection::SkillsDirs, 0));
    app.run_confirm_action();
    assert!(app.cfg.skills.dirs.is_empty());
}

#[test]
fn ctrl_v_does_not_submit_following_enter() {
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL,
    )))
    .unwrap();
    tx.send(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert!(!app.streaming);
}

#[test]
fn appearance_toggles_ui_settings() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Appearance);
    let before = app.cfg.ui.typewriter;
    app.run_action(MenuAction::ToggleTypewriter);
    assert_eq!(app.cfg.ui.typewriter, !before);
    assert!(matches!(app.cur_menu(), Some(Menu::Appearance)));
}

#[test]
fn empty_startup_session_is_not_persisted() {
    let app = test_app("http://127.0.0.1:9/v1".into());
    // a bare launch opens a fresh session with no messages yet; it must
    // NOT be written to disk on exit (no 'n', no send)
    assert!(
        !app.session_has_messages(),
        "fresh launch session carries no messages"
    );
    // once a real message lands it becomes persistable
    let mut app = app;
    app.session.messages.push(crate::providers::Message::new(
        crate::providers::Role::User,
        "hi",
    ));
    assert!(
        app.session_has_messages(),
        "session with a message is persisted on exit"
    );
}

#[test]
fn sessions_filter_narrows_and_esc_clears() {
    use crossterm::event::{Event, KeyCode, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let mut a = Session::new("m".into(), 100);
    a.title = "alpha task".into();
    let mut b = Session::new("m".into(), 100);
    b.title = "beta task".into();
    app.sessions = vec![
        SessionHeader::from_session(&a),
        SessionHeader::from_session(&b),
    ];
    app.open_menu(Menu::Sessions);

    let send = |app: &mut App, code: KeyCode| {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Event::Key(crossterm::event::KeyEvent::new(
            code,
            KeyModifiers::empty(),
        )))
        .unwrap();
        app.poll_input(&rx).unwrap();
    };

    send(&mut app, KeyCode::Char('b'));
    assert_eq!(app.sessions_filter, "b");
    let open: Vec<_> = app
        .menu_rows
        .iter()
        .filter_map(|(_, a)| match a {
            MenuAction::OpenSession(_) => Some(()),
            _ => None,
        })
        .collect();
    assert_eq!(open.len(), 1, "filter 'b' must leave one session");

    // esc clears the filter first, only then closes the menu
    send(&mut app, KeyCode::Esc);
    assert!(app.sessions_filter.is_empty());
    assert!(matches!(app.cur_menu(), Some(Menu::Sessions)));
    send(&mut app, KeyCode::Esc);
    assert!(app.menu_stack.is_empty(), "second esc closes");
}

/// The debug menu holds the http-log switch and the effort declaration,
/// and nothing in the UI opened it — it was reachable from tests only, so
/// every setting in it was effectively config-file-only. Every section
/// offered by `/settings` must open a menu that has rows.
/// §3.3 exists so that after compaction the model works from the anchor
/// the host assembled. A continuation reference points at the provider's
/// copy of the history compaction just removed, so it cannot survive one.
#[test]
fn compaction_invalidates_the_continuation_reference() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.last_response_id = Some("resp_1".into());
    app.session.last_response_model = Some(app.session.model_key.clone());
    app.context_bootstrap_pending = false;

    app.note_compaction(true, 90_000, 30_000);

    assert!(
        app.context_bootstrap_pending,
        "the next request must carry the transcript the host owns"
    );
}

#[test]
fn compaction_noop_reports_fits_instead_of_lying() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.note_compaction(false, 12_000, 12_000);
    assert!(
        toast_text(&app).contains("already fits"),
        "no-op compact must not report X → X: {}",
        toast_text(&app)
    );
}

/// Same reasoning for undo: the provider still remembers the work that was
/// just reverted on disk.
#[test]
fn undo_invalidates_the_continuation_reference() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.context_bootstrap_pending = false;
    // no checkpoints: undo refuses, and must not clear anything
    app.undo(1);
    assert!(
        !app.context_bootstrap_pending,
        "an undo that did nothing must not invalidate anything"
    );
}

/// The switch from #50: a provider whose endpoint does not really keep the
/// conversation can be told to stop being asked.
#[test]
fn continuation_can_be_turned_off_per_provider() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(app.continuation_enabled(), "default is on");
    let provider = app.model_cfg.provider.clone();
    app.cfg
        .providers
        .get_mut(&provider)
        .expect("the test provider exists")
        .continuation = false;
    assert!(!app.continuation_enabled());
}

#[test]
fn every_settings_section_opens_a_menu_with_rows() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let sections: Vec<(String, MenuAction)> = app
        .menu_rows
        .iter()
        .map(|(line, action)| {
            (
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                action.clone(),
            )
        })
        .collect();
    assert!(
        sections.iter().any(|(label, _)| label.contains("Debug")),
        "no way to reach the debug menu: {:?}",
        sections.iter().map(|(l, _)| l).collect::<Vec<_>>()
    );
    for (label, action) in sections {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::Settings);
        app.run_action(action);
        assert!(
            !app.menu_rows.is_empty(),
            "section {label:?} opened nothing"
        );
    }
}

/// The switch the http log lives behind has to be clickable, since the log
/// is the only place the effort mapping can be inspected from outside.
#[test]
fn the_http_log_switch_is_reachable_from_settings() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let before = app.cfg.ui.http_log;
    app.open_menu(Menu::Settings);
    let open_debug = app
        .menu_rows
        .iter()
        .find(|(line, _)| line.spans.iter().any(|s| s.content.contains("Debug")))
        .map(|(_, action)| action.clone())
        .expect("settings offers a debug section");
    app.run_action(open_debug);
    let toggle = app
        .menu_rows
        .iter()
        .find(|(line, _)| {
            line.spans
                .iter()
                .any(|s| s.content.contains("http debug log"))
        })
        .map(|(_, action)| action.clone())
        .expect("the debug menu offers the http log switch");
    app.run_action(toggle);
    assert_ne!(app.cfg.ui.http_log, before);
}

#[test]
fn debug_toggles_flip_config() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let before = app.cfg.ui.typewriter;
    app.open_menu(Menu::Debug);
    app.run_action(MenuAction::ToggleTypewriter);
    assert_ne!(app.cfg.ui.typewriter, before);
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Status { .. })),
        "debug toggles must not write to the chat"
    );
    assert!(app.toast.is_some(), "toggle notice toasts");
}

#[test]
fn undo_reopens_only_done_steps_with_reverted_recorded_evidence() {
    let mut active = crate::plan::create(
        "restore evidence".into(),
        Vec::new(),
        Vec::new(),
        vec![
            crate::plan::NewStep {
                title: "changed file".into(),
                refs: Vec::new(),
            },
            crate::plan::NewStep {
                title: "unrelated file".into(),
                refs: Vec::new(),
            },
        ],
        1000,
        &crate::plan::Limits::default(),
    )
    .unwrap();
    active.steps[0].status = crate::plan::StepStatus::Done;
    active.steps[0].evidence = vec![crate::plan::EvidenceRef {
        session: "session".into(),
        seq: 10,
    }];
    active.steps[1].status = crate::plan::StepStatus::Done;
    active.steps[1].evidence = vec![crate::plan::EvidenceRef {
        session: "session".into(),
        seq: 11,
    }];
    let record = |seq: u64, step: &str, path: &str| crate::agent::journal::Record {
        seq,
        ts: "now".into(),
        session: "session".into(),
        step: Some(step.into()),
        plan: Some(active.id.clone()),
        agent: "main".into(),
        epoch: None,
        kind: "file_diff".into(),
        fields: serde_json::from_value(serde_json::json!({"path": path})).unwrap(),
    };
    let records = vec![
        record(10, "1", "src/changed.rs"),
        record(11, "2", "src/other.rs"),
    ];

    let reopened = reopened_step_ids(&active, &records, "session", &["src/changed.rs".into()]);

    assert_eq!(reopened, vec!["1"]);
}

#[test]
fn undo_reopens_step_on_any_partial_revert() {
    let mut active = crate::plan::create(
        "partial revert".into(),
        Vec::new(),
        Vec::new(),
        vec![crate::plan::NewStep {
            title: "two files".into(),
            refs: Vec::new(),
        }],
        1000,
        &crate::plan::Limits::default(),
    )
    .unwrap();
    active.steps[0].status = crate::plan::StepStatus::Done;
    active.steps[0].evidence = vec![
        crate::plan::EvidenceRef {
            session: "session".into(),
            seq: 10,
        },
        crate::plan::EvidenceRef {
            session: "session".into(),
            seq: 11,
        },
    ];
    let record = |seq: u64, path: &str| crate::agent::journal::Record {
        seq,
        ts: "now".into(),
        session: "session".into(),
        step: Some("1".into()),
        plan: Some(active.id.clone()),
        agent: "main".into(),
        epoch: None,
        kind: "file_diff".into(),
        fields: serde_json::from_value(serde_json::json!({"path": path})).unwrap(),
    };
    let records = vec![record(10, "src/a.rs"), record(11, "src/b.rs")];

    // Only one of the two files reverted: conservative rule (§3.6)
    // still reopens — a partial revert cannot leave "done" standing.
    let reopened = reopened_step_ids(&active, &records, "session", &["src/a.rs".into()]);
    assert_eq!(reopened, vec!["1"]);
    // Nothing reverted: stays done.
    let reopened = reopened_step_ids(&active, &records, "session", &["src/z.rs".into()]);
    assert!(reopened.is_empty());
}

#[test]
fn undo_reopened_step_ids_matches_across_path_separators() {
    let mut active = crate::plan::create(
        "restore evidence".into(),
        Vec::new(),
        Vec::new(),
        vec![crate::plan::NewStep {
            title: "changed file".into(),
            refs: Vec::new(),
        }],
        1000,
        &crate::plan::Limits::default(),
    )
    .unwrap();
    active.steps[0].status = crate::plan::StepStatus::Done;
    active.steps[0].evidence = vec![crate::plan::EvidenceRef {
        session: "session".into(),
        seq: 10,
    }];
    let record = crate::agent::journal::Record {
        seq: 10,
        ts: "now".into(),
        session: "session".into(),
        step: Some("1".into()),
        plan: Some(active.id.clone()),
        agent: "main".into(),
        epoch: None,
        kind: "file_diff".into(),
        fields: serde_json::from_value(serde_json::json!({"path": "src\\changed.rs"})).unwrap(),
    };
    // Git provides forward slashes, journal had backslashes
    let reopened = reopened_step_ids(&active, &[record], "session", &["src/changed.rs".into()]);
    assert_eq!(reopened, vec!["1"]);
}

