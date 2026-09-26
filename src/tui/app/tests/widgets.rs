use super::*;

/// The status bar is laid out in terminal columns and its click targets are
/// derived from the same numbers. Now that the directory label and the plan
/// label are state rather than calls into the environment, the layout can
/// be driven from a test — including with a wide-character project
/// directory, which is what actually broke it.
#[test]
fn status_bar_fits_and_keeps_click_targets_inside_a_wide_directory() {
    for (label, plan) in [
        ("sqwai", ""),
        ("仕事プロジェクト", "step 2/5"),
        ("Проекты-агента", "step 12/12"),
    ] {
        for w in [70u16, 100, 120] {
            let mut app = test_app("http://127.0.0.1:9/v1".into());
            app.startup = false;
            app.cwd_label = label.to_string();
            app.plan_step_label = plan.to_string();
            // The widest effort label there is: `ef:max (ignored)` spends
            // ten more columns than `ef:off`, and the row must still fit
            // with the directory budget absorbing the difference.
            app.model_cfg.effort = EffortLevel::Max;
            app.model_cfg.effort_control = Some(crate::config::EffortControl::None);

            let spans = app.status_bar_spans(w);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            assert!(
                used <= w as usize,
                "{label:?} at width {w}: status bar is {used} columns: {text:?}"
            );

            for (what, click) in [("model", app.ef_click), ("agents", app.agents_click)] {
                if let Some((from, to)) = click {
                    assert!(
                        to <= w && from <= to,
                        "{label:?} at width {w}: {what} click target {from}..{to} \
                         falls outside the row"
                    );
                }
            }
        }
    }
}

#[test]
fn status_bar_with_activity_respects_width_and_click_targets() {
    for activity_setup in [
        |app: &mut App| app.last_checkpoint = Some("edit src/main.rs".into()),
        |app: &mut App| app.status("network timeout 408", StatusKind::Err),
        |app: &mut App| app.retry_line = Some("retrying in 2s (1/3)".into()),
    ] {
        for w in [60u16, 80, 100, 120] {
            let mut app = test_app("http://127.0.0.1:9/v1".into());
            app.startup = false;
            app.cwd_label = "my-project".into();
            app.plan_step_label = "step 1/3".into();
            activity_setup(&mut app);
            let spans = app.status_bar_spans(w);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            assert!(
                used <= w as usize,
                "at width {w}: status bar is {used} columns: {text:?}"
            );
            if let Some((from, to)) = app.ef_click {
                assert!(
                    to <= w && from <= to,
                    "click target out of bounds: {from}..{to} at {w}"
                );
            }
        }
    }
}
#[test]
fn running_tool_name_shimmers_like_activity_header() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("go".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read".into(),
        args: "a.rs".into(),
        ok: None,
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    // mid-sweep tick: the wave sits on the name, like the live header
    app.spinner_tick = crate::tui::shimmer::SHIMMER_PERIOD_TICKS / 4;
    app.rebuild_cache(80);
    let row = app
        .cache_lines
        .iter()
        .find(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains("read")
                && l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .contains("a.rs")
        })
        .expect("running tool row");
    let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("read"), "name intact: {text}");
    // the wave splits the name into per-char spans on every path
    // (static render keeps it one span); the summary "a.rs" rides one
    // span, so a lone "r" can only come from the split name
    assert!(
        row.spans.iter().any(|s| s.content == "r"),
        "running tool name must split into wave spans: {text}"
    );
}

#[test]
fn live_activity_header_shimmers_finished_stays_dim() {
    use super::super::view::{ActivityGroup, activity_header_line};
    let g = ActivityGroup {
        seg_start: 0,
        seg_end: 0,
        calls: 3,
        thinking: 0,
        duration_ms: 5000,
        errors: 0,
        rejected: 0,
        expanded: true,
        turn_user: None,
    };
    // finished header: static dim text
    let still = activity_header_line(&g, None);
    let text: String = still.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("activity · 3 calls"), "{text:?}");
    assert!(
        still.spans.iter().all(|s| s.style == Theme::dim()),
        "finished header must stay dim: {still:?}"
    );
    // live header at mid-sweep: same text, shaded letters
    let live = activity_header_line(&g, Some(crate::tui::shimmer::SHIMMER_PERIOD_TICKS / 4));
    let live_text: String = live.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(live_text, text, "shimmer must not change the text");
    // the 8 "activity" letters carry more than one brightness step
    let word: String = live
        .spans
        .iter()
        .skip(1)
        .take(8)
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(word, "activity");
    let styles: std::collections::HashSet<String> = live
        .spans
        .iter()
        .skip(1)
        .take(8)
        .map(|s| format!("{:?}", s.style))
        .collect();
    assert!(
        styles.len() > 1,
        "live header must shade the word differently: {styles:?}"
    );
}

/// Bottom spinner is removed for now (a replacement from the
/// `/test animations` gallery is coming): while streaming the status
/// bar must show no spinner glyph anywhere.
#[test]
fn no_bottom_spinner_while_streaming() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.streaming = true;

    let spans = app.status_bar_spans(100);
    assert!(
        !spans.iter().any(|s| {
            let text = s.content.trim();
            text.chars().count() == 1
                && text
                    .chars()
                    .next()
                    .is_some_and(|c| WORKING_SPINNER.contains(&c))
        }),
        "status bar must not carry a spinner until the replacement lands"
    );
}
/// The provider menu carries a connection probe: dispatching it marks the
/// provider as checking, and the worker thread reports back without
/// blocking the UI tick.
#[test]
fn provider_check_marks_checking_then_reports_refused() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.run_action(MenuAction::CheckProvider("p".into()));
    assert!(
        matches!(app.provider_checks.get("p"), Some(ProviderCheck::Checking)),
        "dispatch must mark the provider as checking"
    );
    // nothing listens on port 9: the refusal lands fast and red
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(app.provider_checks.get("p"), Some(ProviderCheck::Checking)) {
        app.poll_provider_check();
        assert!(
            std::time::Instant::now() < deadline,
            "a refused connection must settle, not hang the menu"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        matches!(app.provider_checks.get("p"), Some(ProviderCheck::Err(_))),
        "a refused connection lights red"
    );
}
/// The check row renders the outcome: green with the detail on success,
/// red with the reason on failure, and stays a button for a re-check.
#[test]
fn provider_check_row_lights_green_or_red() {
    use crate::tui::theme::Theme;

    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Models {
        provider: "p".into(),
    });
    let row_style = |app: &App, needle: &str| {
        app.menu_rows
            .iter()
            .flat_map(|(line, _)| line.spans.iter())
            .find(|span| span.content.contains(needle))
            .map(|span| span.style)
    };

    app.provider_checks
        .insert("p".into(), ProviderCheck::Ok("2 models".into()));
    app.build_menu_rows();
    assert_eq!(
        row_style(&app, "connection ok"),
        Some(Theme::ok()),
        "success lights green"
    );

    app.provider_checks
        .insert("p".into(), ProviderCheck::Err("401 Unauthorized".into()));
    app.build_menu_rows();
    assert_eq!(
        row_style(&app, "connection failed"),
        Some(Theme::err()),
        "failure lights red"
    );

    let still_button = app.menu_rows.iter().any(|(line, action)| {
        line.spans.iter().any(|s| s.content.contains("connection"))
            && matches!(action, MenuAction::CheckProvider(_))
    });
    assert!(still_button, "the row stays clickable for a re-check");
}
/// `/undo step 3` used to parse as `/undo 1`: `nth(1)` yielded "step",
/// `parse::<usize>()` failed and `unwrap_or(1)` reverted the most recent
/// checkpoint instead — a destructive command acting on the wrong target
/// and reporting success. Per-step undo is not built yet, so an argument
/// that is not a count has to be refused.
/// §3.7 / §7 S: Esc must tell a running tool from plain text streaming.
/// Only one tool executes at a time, so the open row — `ok: None` — is
/// the signal; a closed row or no tool row at all means there is nothing
/// "mid-tool" to cancel.
#[test]
fn tool_running_reflects_whether_the_last_tool_row_is_still_open() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert!(!app.tool_running(), "no tool segment at all");

    app.push_segment(Segment::Tool {
        call_id: None,
        name: "bash".into(),
        args: "sleep 30".into(),
        ok: None,
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });
    assert!(app.tool_running());

    // In real execution, a live Assistant segment trails behind the tool call
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    assert!(
        app.tool_running(),
        "tool running with live assistant trailing"
    );

    if let Some(Segment::Tool { ok, .. }) = app
        .segments
        .iter_mut()
        .find(|s| matches!(s, Segment::Tool { .. }))
    {
        *ok = Some(false);
    }
    assert!(!app.tool_running(), "the row closed, nothing is running");
}

#[test]
fn undo_refuses_an_argument_that_is_not_a_count() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.session
        .checkpoints
        .push(("deadbeef".into(), "write src/a.rs".into()));

    let last_status = |app: &App| {
        app.toast
            .as_ref()
            .map(|t| t.text.clone())
            .unwrap_or_default()
    };

    // An argument that is neither a count nor a step is refused with the
    // forms that do work, rather than silently reverting the last
    // checkpoint and reporting success.
    app.command("undo yesterday");
    let status = last_status(&app);
    assert!(
        status.contains("takes a count or a step") && status.contains("yesterday"),
        "must explain instead of undoing the wrong thing: {status:?}"
    );

    // `step` without an id is refused too, not read as `/undo 1`
    app.command("undo step");
    assert!(
        last_status(&app).contains("needs a step id"),
        "{:?}",
        last_status(&app)
    );

    assert_eq!(
        app.session.checkpoints.len(),
        1,
        "nothing may be reverted for an argument we cannot honour"
    );
}

/// `/undo step N` reverts through layer 1, so it must reach that path
/// rather than the checkpoint one — with no journal records for the step
/// there is nothing to put back, and saying so is the correct answer.
#[test]
fn undo_step_reports_a_step_with_no_recorded_writes() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.session
        .checkpoints
        .push(("deadbeef".into(), "write src/a.rs".into()));

    app.command("undo step 3");
    let status = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        status.contains("step 3") && status.contains("no file writes"),
        "{status:?}"
    );
    assert_eq!(
        app.session.checkpoints.len(),
        1,
        "a per-step revert must not touch the checkpoint stack"
    );
}

/// A server that answers every request with the same status and body.
fn mock_status_server(status: u16, reason: &str, body: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let reason = reason.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        // Serve for as long as anyone asks. If the retry policy is wrong
        // this loop is what would keep answering for an hour.
        for stream in listener.incoming().take(16) {
            let Ok(stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut content_length = 0usize;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {
                        if let Some(value) = line
                            .to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|rest| rest.trim().parse::<usize>().ok())
                        {
                            content_length = value;
                        }
                        // NOTE: review-checklist TUI perf tests live at the end of this module.
                    }
                }
            }
            // Drain the request body before answering: closing the socket
            // while the client is still writing looks like a network
            // failure, which is retryable — and the test would then be
            // exercising the wrong path entirely.
            let mut discard = vec![0u8; content_length];
            let _ = reader.read_exact(&mut discard);
            let mut out = stream;
            let _ = write!(
                out,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = out.flush();
            // Same reason as mock_sse_server: do not tear the socket down
            // in the same instant, the client must be able to read it all.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    format!("http://{addr}/v1")
}

/// An expired key cannot be fixed by waiting, so the turn has to end now.
///
/// The retry policy used to be a substring match on the error message, and
/// a 401 matched nothing — so it was retried with backoff for a full hour
/// while the user watched. This test would hang under that behaviour, which
/// is what makes it a regression test: it is bounded by a timeout.
#[tokio::test]
async fn an_auth_failure_ends_the_turn_instead_of_retrying() {
    let url = mock_status_server(401, "Unauthorized", r#"{"error":"invalid x-api-key"}"#);
    let mut app = test_app(url);
    app.startup = false;
    app.input = App::fresh_input("hi".into());
    app.submit();

    // Two seconds is generous: the first backoff alone is one second, and a
    // retrying implementation would still be sleeping when this expires.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while app.streaming && std::time::Instant::now() < deadline {
        app.poll_agent();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert!(
        !app.streaming,
        "an auth failure must end the turn, not retry it"
    );
    let reported = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        reported.contains("API key") || reported.contains("401"),
        "the user should be told what to fix: {reported:?}"
    );
}

#[test]
fn inline_ask_select_and_confirm_freezes_with_answer() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    assert!(app.active_ask_seg().is_some(), "ask must be active");
    assert!(app.menu_stack.is_empty(), "no overlay menu for inline ask");

    app.inline_ask_select(0, 1);
    assert_eq!(
        app.inline_ask_text(app.active_ask_seg().unwrap()),
        "Q1: beta | Q2: (no answer)"
    );

    app.inline_ask_toggle(1, 0);
    app.inline_ask_toggle(1, 1);
    assert_eq!(
        app.inline_ask_text(app.active_ask_seg().unwrap()),
        "Q1: beta | Q2: x, y"
    );

    app.inline_ask_confirm();
    assert!(app.active_ask_seg().is_none(), "confirm closes the ask");
    match app.segments.last() {
        Some(Segment::AskUser {
            answered: Some(a), ..
        }) => assert_eq!(a, "Q1: beta | Q2: x, y"),
        other => panic!("ask must freeze with the answer, got {other:?}"),
    }
}

#[test]
fn inline_ask_single_choice_replaces_previous_pick() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    app.inline_ask_select(0, 0);
    app.inline_ask_select(0, 1);
    let seg = app.active_ask_seg().unwrap();
    match &app.segments[seg] {
        Segment::AskUser { picked, .. } => {
            assert_eq!(picked[0], vec![false, true], "single pick replaces");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn inline_ask_skip_blurs_custom_first_then_freezes() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    app.ask_custom_focus = Some(0);
    app.inline_ask_skip();
    assert!(app.active_ask_seg().is_some(), "first Esc only blurs");
    assert!(app.ask_custom_focus.is_none());
    app.inline_ask_skip();
    assert!(app.active_ask_seg().is_none(), "second Esc skips");
}

#[test]
fn ask_user_answer_collapses_to_head_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    push_inline_ask(&mut app, ask_fixture());
    let seg = app.active_ask_seg().expect("live ask");
    assert!(
        matches!(app.segments[seg], Segment::AskUser { expanded: true, .. }),
        "live questions open unfolded"
    );
    app.inline_ask_select(0, 1);
    app.inline_ask_confirm();
    assert!(
        matches!(
            app.segments[seg],
            Segment::AskUser {
                answered: Some(_),
                expanded: false,
                ..
            }
        ),
        "the answer settles to the one-line head"
    );
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("ask_user"), "head names the tool:\n{s}");
    assert!(s.contains("beta"), "head carries the answer:\n{s}");
    assert!(
        !s.contains("Pick many"),
        "the questionnaire folds away:\n{s}"
    );
}

#[test]
fn ask_user_head_click_folds_and_unfolds() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    push_inline_ask(&mut app, ask_fixture());
    app.inline_ask_confirm();
    let seg = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::AskUser { .. }))
        .expect("ask segment");
    // collapsed to a single head row
    render_to_string(&mut app, 100, 30);
    let rows = app.cache_rowseg.iter().filter(|t| **t == Some(seg)).count();
    assert_eq!(rows, 1, "answered ask is one head row");
    let abs = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(seg))
        .unwrap();
    // head click unfolds the questionnaire back
    app.click(abs);
    assert!(
        matches!(app.segments[seg], Segment::AskUser { expanded: true, .. }),
        "head click unfolds"
    );
    render_to_string(&mut app, 100, 30);
    let rows = app.cache_rowseg.iter().filter(|t| **t == Some(seg)).count();
    assert!(rows > 5, "questionnaire is back: {rows} rows");
    // and folds again on the first row
    let abs = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(seg))
        .unwrap();
    app.click(abs);
    assert!(
        matches!(
            app.segments[seg],
            Segment::AskUser {
                expanded: false,
                ..
            }
        ),
        "head click folds again"
    );
}

#[test]
fn ask_user_option_click_still_selects_when_expanded() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    push_inline_ask(&mut app, ask_fixture());
    render_to_string(&mut app, 100, 30);
    let seg = app.active_ask_seg().expect("live ask");
    let start = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(seg))
        .expect("ask block");
    // expanded layout: header, question, alpha, beta at offset 3
    app.click(start + 3);
    match &app.segments[seg] {
        Segment::AskUser { picked, .. } => {
            assert_eq!(picked[0], vec![false, true], "option click selects")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        matches!(app.segments[seg], Segment::AskUser { expanded: true, .. }),
        "option click must not fold the questionnaire"
    );
}

#[test]
fn ask_user_tool_rows_are_suppressed_live() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let before = app.segments.len();
    app.handle_tool_start("ask_user".to_string(), "anything".to_string(), None);
    assert_eq!(app.segments.len(), before, "no Tool row for ask_user");
    app.handle_tool_notice(
        "ask_user".to_string(),
        "answer".to_string(),
        true,
        None,
        None,
    );
    assert_eq!(app.segments.len(), before, "no Tool row on notice either");
    // ordinary tools are unaffected
    app.handle_tool_start("read".to_string(), "a.rs".to_string(), None);
    assert_eq!(app.segments.len(), before + 1);
}

#[test]
fn inline_ask_rows_respect_narrow_width() {
    use unicode_width::UnicodeWidthStr;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    for width in [20u16, 40, 80] {
        app.rebuild_cache(width);
        for line in &app.cache_lines {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(
                UnicodeWidthStr::width(text.as_str()) <= width as usize,
                "row overflows width {width}: {text:?}"
            );
        }
    }
}

#[test]
fn inline_ask_click_targets_decode_options_custom_confirm() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    app.rebuild_cache(80);
    // layout for the fixture: header, question, 2 options, custom,
    // separator, header, question, 2 options, confirm
    let seg = app.active_ask_seg().unwrap();
    let (start, _) = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(seg))
        .map(|s| {
            let mut e = s;
            while e < app.cache_rowseg.len() && app.cache_rowseg[e] == Some(seg) {
                e += 1;
            }
            (s, e)
        })
        .expect("ask block must be cached");
    let at = |off: usize| app.ask_row_at(start + off);
    assert_eq!(at(0), None, "header is not clickable");
    assert_eq!(at(1), None, "question is not clickable");
    assert_eq!(
        at(2),
        Some((seg, super::super::view::AskRow::Option { q: 0, opt: 0 }))
    );
    assert_eq!(
        at(3),
        Some((seg, super::super::view::AskRow::Option { q: 0, opt: 1 }))
    );
    assert_eq!(at(4), Some((seg, super::super::view::AskRow::Custom { q: 0 })));
    assert_eq!(at(5), None, "separator is not clickable");
    assert_eq!(
        at(9),
        Some((seg, super::super::view::AskRow::Option { q: 1, opt: 1 }))
    );
    assert_eq!(at(10), Some((seg, super::super::view::AskRow::Confirm)));
}

#[test]
fn inline_ask_click_selects_instead_of_dismissing() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    app.rebuild_cache(80);
    let seg = app.active_ask_seg().unwrap();
    let start = app
        .cache_rowseg
        .iter()
        .position(|t| *t == Some(seg))
        .expect("ask block");
    // click the second option of Q1 (offset 3 in the layout above)
    app.click(start + 3);
    assert!(
        app.active_ask_seg().is_some(),
        "click must not dismiss the ask"
    );
    match &app.segments[seg] {
        Segment::AskUser { picked, .. } => assert_eq!(picked[0], vec![false, true]),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn ask_user_summary_covers_multi_question_mode() {
    let summary = crate::agent::tools::call_summary(
        "ask_user",
        &serde_json::json!({
            "questions": [
                {"header": "Q1", "question": "first?", "options": [{"label": "a"}, {"label": "b"}]},
                {"header": "Q2", "question": "second?", "options": [{"label": "c"}]}
            ]
        }),
    );
    assert!(
        summary.contains("first?"),
        "summary must not be empty: {summary:?}"
    );
    assert!(
        summary.contains("second?"),
        "all questions visible: {summary:?}"
    );
}

#[test]
fn inline_ask_lands_before_the_answer_inside_activity() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    // a running turn: thinking, a tool, the live answer slot
    app.push_segment(Segment::Thinking {
        text: "hmm".to_string(),
        expanded: false,
        started: None,
        duration_ms: 0,
        live: false,
    });
    app.handle_tool_start("read".to_string(), "a.rs".to_string(), None);
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    push_inline_ask(&mut app, ask_fixture());
    let ask = app.active_ask_seg().expect("ask must be active");
    let answer = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Assistant { live: true, .. }))
        .expect("live answer");
    assert!(
        ask < answer,
        "the question must sit before the answer, with the tools: ask={ask} answer={answer}"
    );
    // the work run covers tools and the question together…
    let (start, end) = app.trailing_work_run().expect("a work run must exist");
    assert!(start <= ask && ask < end, "ask must fold into activity");
    // …and a successful turn folds it collapsed, not below the answer
    app.finalize_activity_group();
    let g = app.activity_groups.last().expect("group must be frozen");
    assert!(!g.expanded, "successful turns fold by default");
    assert!(
        g.seg_start <= ask && ask < g.seg_end,
        "ask must be inside the group"
    );
    assert_eq!(g.calls, 2, "read + ask_user count as calls");
}

#[test]
fn active_ask_survives_index_shifts() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Thinking {
        text: String::new(),
        expanded: false,
        started: None,
        duration_ms: 0,
        live: false,
    });
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });
    push_inline_ask(&mut app, ask_fixture());
    assert_eq!(app.active_ask_seg(), Some(1));
    // finish_turn drops empty thinking rows, shifting every later index
    // — index-based tracking would now point at the answer slot
    app.remove_segment(0);
    let after = app.active_ask_seg().expect("ask must survive the shift");
    assert_eq!(after, 0);
    match &app.segments[after] {
        Segment::AskUser { id: 7, .. } => {}
        other => panic!("wrong segment resolved: {other:?}"),
    }
    app.inline_ask_select(0, 0);
    match &app.segments[after] {
        Segment::AskUser { picked, .. } => assert_eq!(picked[0], vec![true, false]),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn plain_click_in_composer_does_not_start_selection() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.last_input = ratatui::layout::Rect::new(0, 10, 40, 3);
    app.input_mouse_down(10, 5);
    assert!(!app.input.is_selecting());
    assert!(!app.input_dragging);
    app.input_mouse_up();
    assert!(!app.input.is_selecting());
    assert!(!app.input_dragging);
}

#[test]
fn mouse_selection_maps_screen_column_to_char_index_with_wide_chars() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.last_chat = ratatui::layout::Rect::new(1, 0, 80, 20);
    app.cache_lines = vec![ratatui::text::Line::from("日本語 hello")];

    // Click on 'h' at screen col 8 (chat_x=1 + 7 display columns: 3 CJK * 2 + 1 space = 7)
    app.mouse_down(0, 8);
    assert_eq!(
        app.press.unwrap().col,
        4,
        "screen column 8 must map to char index 4 ('h')"
    );

    // Drag to after 'hello' at screen col 13 (chat_x=1 + 7 + 5 = 13 display columns)
    app.mouse_drag(0, 13);
    let sel = app.sel.unwrap();
    assert_eq!(sel.a.col, 4);
    assert_eq!(sel.b.col, 9);
}

#[test]
fn draw_menu_at_height_6_or_7_does_not_panic() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.open_menu(Menu::Settings);
    let backend = ratatui::backend::TestBackend::new(80, 6);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();

    let backend = ratatui::backend::TestBackend::new(80, 7);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| app.draw(f)).unwrap();
}

#[test]
fn streaming_text_appends_preserve_past_segment_caches() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;

    // Populate a conversation with multiple past segments
    for i in 0..10 {
        if i % 2 == 0 {
            app.push_segment(Segment::User(format!("User question {i} with code `foo`")));
        } else {
            app.push_segment(Segment::Assistant {
                text: format!("Assistant answer {i} with details"),
                live: false,
            });
        }
    }
    // Add a live assistant segment at the end
    app.push_segment(Segment::Assistant {
        text: "Initial token".into(),
        live: true,
    });

    // First cache build
    app.rebuild_cache(80);
    assert_eq!(app.seg_cache.len(), 11);

    // Snapshot cache entries of all previous segments by stable id
    let previous: Vec<(u64, u64, u16, usize)> = (0..10)
        .map(|i| {
            let id = app.seg_meta[i].id;
            let cached = app.seg_cache.get(&id).expect("segment cached");
            (id, cached.rev, cached.width, cached.rows.len())
        })
        .collect();

    // Simulate streaming: append text to the live segment at index 10
    if let Some(Segment::Assistant { text, .. }) = app.segments.get_mut(10) {
        text.push_str(
            " and more streamed tokens across multiple lines\n```rust\nfn bar() {}\n```\n",
        );
    }
    app.touch_segment(10);

    // Rebuild cache (as happens on each frame during streaming)
    app.rebuild_cache(80);

    // Past segments must keep identical cache entries (same id+rev)
    for (id, rev, width, rows) in &previous {
        let cached = app
            .seg_cache
            .get(id)
            .expect("past segment must remain cached");
        assert_eq!(cached.rev, *rev, "past segment {id} rev must not change");
        assert_eq!(
            cached.width, *width,
            "past segment {id} width must not change"
        );
        assert_eq!(
            cached.rows.len(),
            *rows,
            "past segment {id} row count must not change"
        );
    }

    // The live segment at index 10 MUST have updated cache
    let live_id = app.seg_meta[10].id;
    let live_cached = app
        .seg_cache
        .get(&live_id)
        .expect("live segment must be cached");
    assert!(
        live_cached.rows.len() > 1,
        "live segment should have rendered the code block"
    );
}

#[test]
fn structural_insert_preserves_other_segment_caches() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;

    app.push_segment(Segment::User("Question".into()));
    app.push_segment(Segment::Assistant {
        text: "Answer".into(),
        live: false,
    });

    app.rebuild_cache(80);
    let ids_before: Vec<u64> = app.seg_meta.iter().map(|m| m.id).collect();
    assert_eq!(ids_before.len(), 2);
    let rows_before: Vec<usize> = ids_before
        .iter()
        .map(|id| app.seg_cache.get(id).expect("cached").rows.len())
        .collect();

    // Insert a tool segment between question and answer: unlike the old
    // positional cache, id-keyed entries for the other segments survive.
    app.insert_segment(
        1,
        Segment::Tool {
            call_id: None,
            name: "bash".into(),
            args: "echo hi".into(),
            ok: Some(true),
            output: "hi".into(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        },
    );

    app.rebuild_cache(80);
    assert_eq!(app.seg_meta.len(), 3);
    // the unshifted prefix keeps its rows without re-rendering; the
    // shifted tail re-renders exactly once so its rows carry the NEW
    // positional tag — reusing the old rows would misroute clicks and
    // selection onto the wrong segment.
    let qid = ids_before[0];
    let cached_q = app.seg_cache.get(&qid).expect("prefix entry survives");
    assert_eq!(cached_q.rows.len(), rows_before[0]);
    assert!(
        cached_q.rows.iter().all(|(_, t)| *t != Some(2)),
        "prefix rows must not carry the shifted tag"
    );
    let aid = ids_before[1];
    let cached_a = app.seg_cache.get(&aid).expect("tail entry survives");
    assert!(
        cached_a
            .rows
            .iter()
            .all(|(_, t)| t.is_none() || *t == Some(2)),
        "shifted rows must carry the new positional tag"
    );
    assert_eq!(app.seg_cache.len(), 3);
}

#[test]
fn preamble_before_tool_becomes_commentary_row_in_activity() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.streaming = true;
    app.push_segment(Segment::User("fix bug".into()));
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });

    // Turn 1: model outputs preamble text, then calls tool 'read'
    app.handle_text_delta("Let me inspect the file.\nHere is my plan.".into());
    app.handle_tool_start("read".into(), "a.rs".into(), None);
    app.handle_tool_notice("read".into(), "file contents".into(), true, None, None);

    // Turn 2: model outputs preamble text, then calls tool 'edit'
    app.handle_text_delta("Now I see the issue, editing line 10.".into());
    app.handle_tool_start("edit".into(), "a.rs".into(), None);
    app.handle_tool_notice("edit".into(), "done".into(), true, None, None);

    // Turn 3: model outputs final answer (no tools)
    app.handle_text_delta("I have finished the fix.".into());

    app.session.messages = vec![
        crate::providers::Message::new(crate::providers::Role::User, "fix bug"),
        crate::providers::Message::new(
            crate::providers::Role::Assistant,
            "Let me inspect the file.\nHere is my plan.",
        )
        .with_tool_calls(vec![crate::providers::ToolCallReq::new(
            "c1",
            "read",
            serde_json::json!({}),
        )]),
        crate::providers::Message::tool_result("c1", "file contents", false),
        crate::providers::Message::new(
            crate::providers::Role::Assistant,
            "Now I see the issue, editing line 10.",
        )
        .with_tool_calls(vec![crate::providers::ToolCallReq::new(
            "c2",
            "edit",
            serde_json::json!({}),
        )]),
        crate::providers::Message::tool_result("c2", "done", false),
        crate::providers::Message::new(
            crate::providers::Role::Assistant,
            "I have finished the fix.",
        ),
    ];

    app.finish_turn(Ok(()));

    // Verify segments: User, Commentary, Tool(read), Commentary, Tool(edit), Assistant
    assert_eq!(app.segments.len(), 6);
    assert!(matches!(app.segments[0], Segment::User(ref u) if u == "fix bug"));

    match &app.segments[1] {
        Segment::Commentary(text) => {
            assert_eq!(text, "Let me inspect the file.\nHere is my plan.");
        }
        other => panic!("expected commentary, got {other:?}"),
    }

    match &app.segments[2] {
        Segment::Tool { name, args, ok, .. } => {
            assert_eq!(name, "read");
            assert_eq!(args, "a.rs");
            assert_eq!(*ok, Some(true));
        }
        other => panic!("expected read tool, got {other:?}"),
    }

    match &app.segments[3] {
        Segment::Commentary(text) => {
            assert_eq!(text, "Now I see the issue, editing line 10.");
        }
        other => panic!("expected commentary, got {other:?}"),
    }

    match &app.segments[4] {
        Segment::Tool { name, args, ok, .. } => {
            assert_eq!(name, "edit");
            assert_eq!(args, "a.rs");
            assert_eq!(*ok, Some(true));
        }
        other => panic!("expected edit tool, got {other:?}"),
    }

    match &app.segments[5] {
        Segment::Assistant { text, live } => {
            assert_eq!(text, "I have finished the fix.");
            assert!(!live);
        }
        other => panic!("expected assistant answer, got {other:?}"),
    }

    // Verify activity group encompasses commentary + 2 real tool calls.
    // Commentary never counts as a call.
    assert_eq!(app.activity_groups.len(), 1);
    let g = &app.activity_groups[0];
    assert_eq!(g.seg_start, 1);
    assert_eq!(g.seg_end, 5);
    assert_eq!(g.calls, 2);

    // Rendering check: activity collapsed header
    app.rebuild_cache(80);
    let screen = rendered(&app);
    assert!(screen.contains("activity · 2 calls"), "header: {screen}");
    assert!(
        screen.contains("I have finished the fix."),
        "answer: {screen}"
    );
}

#[test]
fn commentary_renders_inside_activity_without_toggle() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.streaming = true;
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });

    app.handle_text_delta("First line of commentary.\nSecond line of commentary.".into());
    app.handle_tool_start("read".into(), "main.rs".into(), None);
    app.handle_tool_notice("read".into(), "ok".into(), true, None, None);
    app.finish_turn(Ok(()));

    // Commentary segment exists (not a tool row)
    let commentary_idx = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Commentary(_)))
        .expect("commentary segment must exist");

    // Expand activity group so rows are rendered
    app.activity_groups[0].expanded = true;
    app.rebuild_cache(80);
    let screen = rendered(&app);
    assert!(!screen.contains("talking"), "no pseudo-tool: {screen}");
    assert!(screen.contains("First line of commentary."));
    assert!(screen.contains("Second line of commentary."));

    // Commentary is not toggleable: no expanded flag, click leaves it alone
    assert!(matches!(
        app.segments[commentary_idx],
        Segment::Commentary(_)
    ));
    app.rebuild_cache(80);
    let screen_again = rendered(&app);
    assert!(screen_again.contains("First line of commentary."));
    assert!(screen_again.contains("Second line of commentary."));
}

#[test]
fn commentary_left_alignment_matches_tool_call() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Commentary("Checking files...".into()));
    app.push_segment(Segment::Tool {
        call_id: None,
        name: "read_file".into(),
        args: "src/main.rs".into(),
        ok: Some(true),
        output: String::new(),
        diff: None,
        preview: Vec::new(),
        preview_total: 0,
        expanded: false,
        flash: None,
    });

    let commentary_rows = app.render_segment(&app.segments, 0, 80, true);
    let tool_rows = app.render_segment(&app.segments, 1, 80, true);

    let comm_text: String = commentary_rows[0]
        .0
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let tool_text: String = tool_rows[0]
        .0
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();

    assert!(
        comm_text.starts_with("  "),
        "commentary must start with 2 spaces: {comm_text:?}"
    );
    assert!(
        !comm_text.starts_with("   "),
        "commentary must not start with 3 spaces"
    );
    assert!(
        tool_text.starts_with("  "),
        "tool must start with 2 spaces: {tool_text:?}"
    );
    assert!(
        !tool_text.starts_with("   "),
        "tool must not start with 3 spaces"
    );
}

#[test]
fn commentary_wrapped_rows_keep_indent_and_dim() {
    use ratatui::style::Modifier;
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.push_segment(Segment::Commentary(
        "word ".repeat(30).trim_end().to_string(),
    ));
    let rows = app.render_segment(&app.segments, 0, 40, true);
    assert!(rows.len() > 1, "must wrap at width 40");
    for (l, _) in &rows {
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        if text.trim().is_empty() {
            continue;
        }
        assert!(
            text.starts_with("  ") && !text.starts_with("   "),
            "every row indented by 2: {text:?}"
        );
        assert!(
            l.spans
                .iter()
                .skip(1)
                .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "body spans dimmed, hues kept: {text:?}"
        );
    }
}

#[test]
fn load_history_restores_commentary_when_assistant_message_has_content() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.session.messages = vec![
        crate::providers::Message::new(crate::providers::Role::User, "inspect"),
        crate::providers::Message::new(
            crate::providers::Role::Assistant,
            "I will read src/lib.rs first.\nThen edit it.",
        )
        .with_tool_calls(vec![crate::providers::ToolCallReq::new(
            "call-1",
            "read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
        crate::providers::Message::tool_result("call-1", "file contents", false),
        crate::providers::Message::new(crate::providers::Role::Assistant, "done"),
    ];
    app.clear_segments();
    app.load_history_segments();

    assert_eq!(app.segments.len(), 4);
    assert!(matches!(app.segments[0], Segment::User(ref t) if t == "inspect"));
    match &app.segments[1] {
        Segment::Commentary(text) => {
            assert_eq!(text, "I will read src/lib.rs first.\nThen edit it.");
        }
        other => panic!("expected commentary, got {other:?}"),
    }
    match &app.segments[2] {
        Segment::Tool {
            name, ok, output, ..
        } => {
            assert_eq!(name, "read");
            assert_eq!(*ok, Some(true));
            assert_eq!(output, "file contents");
        }
        other => panic!("expected read tool, got {other:?}"),
    }
    assert!(matches!(app.segments[3], Segment::Assistant { ref text, .. } if text == "done"));
    assert_eq!(app.activity_groups.len(), 1);
    assert_eq!(app.activity_groups[0].calls, 1);
}

#[test]
fn aborted_turn_during_tool_preserves_commentary_row() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.streaming = true;
    app.push_segment(Segment::Assistant {
        text: String::new(),
        live: true,
    });

    app.handle_text_delta("Starting build...".into());
    app.handle_tool_start("bash".into(), "cargo build".into(), None);

    // Abort mid-tool
    app.finish_turn(Err("aborted".into()));

    // Commentary and bash tool are preserved
    assert!(matches!(app.segments[0], Segment::Commentary(ref t) if t == "Starting build..."));
    let names: Vec<&str> = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Tool { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["bash"]);

    // No empty assistant segment remains
    assert!(
        !app.segments
            .iter()
            .any(|s| matches!(s, Segment::Assistant { .. }))
    );

    // Activity group folds shut like any finished turn; the abort
    // surfaces through the status row, not an open block
    assert_eq!(app.activity_groups.len(), 1);
    assert!(!app.activity_groups[0].expanded);
}

#[test]
fn session_recovery_with_deleted_or_broken_plan_id() {
    let mut session = Session::new("m".into(), 1000);
    session.plan_id = Some("deleted_or_nonexistent_plan_99999".into());

    let mut providers = BTreeMap::new();
    providers.insert(
        "p".to_string(),
        ProviderConfig {
            format: WireFormat::Openai,
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: Some("test-key".into()),
            api_key_env: None,
            continuation: true,
        },
    );
    let mut models = BTreeMap::new();
    models.insert(
        "m".to_string(),
        ModelConfig {
            provider: "p".into(),
            id: "test-model".into(),
            context: 1000,
            effort: EffortLevel::Off,
            effort_control: None,
            effort_always_on: false,
            fallback: None,
        },
    );
    let cfg = Config {
        last_model: "m".into(),
        legacy_default_model: String::new(),
        default_effort: crate::config::EffortLevel::Off,
        providers,
        models,
        safety: Default::default(),
        ui: Default::default(),
        mcp: Default::default(),
        lsp: Default::default(),
        skills: Default::default(),
        memory: Default::default(),
        diary: Default::default(),
        compaction: Default::default(),
        plan: Default::default(),
        verify: Default::default(),
        secrets: Default::default(),
        undo: Default::default(),
    };

    // Starting app with a deleted/broken plan_id should not panic and should reset the invalid plan_id
    let mut app = App::new(cfg, session, false, false).unwrap();
    assert_ne!(
        app.session.plan_id.as_deref(),
        Some("deleted_or_nonexistent_plan_99999")
    );

    // Switching to another session with a broken plan_id also clears the broken plan_id gracefully
    let mut session2 = Session::new("m".into(), 1000);
    session2.plan_id = Some("another_corrupt_or_deleted_plan_88888".into());
    app.apply_session(session2);
    assert_ne!(
        app.session.plan_id.as_deref(),
        Some("another_corrupt_or_deleted_plan_88888")
    );
}

