use super::*;

/// A toast is static text with a deadline, so it must not hold the frame loop
/// at 20 FPS. The `/debug` perf log measured it as 60 full-transcript redraws
/// per notice, each one writing zero bytes to the terminal.
#[test]
fn a_toast_alone_does_not_ask_for_frames() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.status("saved", StatusKind::Info);
    assert!(app.toast.is_some(), "the notice is up");
    let (animating, why) = app.animating_terms();
    assert!(!animating, "a toast is not an animation");
    assert_eq!(why, 1 << 6, "the reason says bare dirty: {why:#x}");
    // a running tool row still asks for frames, and names itself
    app.handle_tool_start("read".into(), "a.rs".into(), Some("c1".into()));
    let (animating, why) = app.animating_terms();
    assert!(animating, "a running call animates");
    assert_eq!(why, 1 << 1, "{why:#x}");
}

/// The status bar is laid out in terminal columns and its click targets are
/// derived from the same numbers. Now that the directory label is state
/// rather than a call into the environment, the layout can be driven from
/// a test — including with a wide-character project directory, which is
/// what actually broke it. Plan progress stays out of the bar (the
/// checkpoint row already carries it); the bar keeps metrics only.
#[test]
fn status_bar_fits_and_keeps_click_targets_inside_a_wide_directory() {
    for label in ["sqwai", "仕事プロジェクト", "Проекты-агента"] {
        for w in [70u16, 100, 120] {
            let mut app = test_app("http://127.0.0.1:9/v1".into());
            app.startup = false;
            app.cwd_label = label.to_string();
            // The widest effort label there is: `max (ignored)` spends
            // ten more columns than `off`, and the row must still fit
            // with the directory budget absorbing the difference.
            app.model_cfg.effort = EffortLevel::Max;
            app.model_cfg.effort_control = Some(crate::config::EffortControl::None);

            let spans = app.status_bar_spans(w);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !text.contains("step "),
                "no plan progress in the bar: {text:?}"
            );
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            assert_eq!(
                used, w as usize,
                "{label:?} at width {w}: bar must fill exactly: {text:?}"
            );

            for (what, click) in [("effort", app.ef_click), ("model", app.md_click)] {
                let (from, to) = click.expect("{what} is clickable in the bar");
                assert!(
                    to <= w && from <= to,
                    "{label:?} at width {w}: {what} click target {from}..{to} \
                     falls outside the row"
                );
            }
        }
    }
}

/// The model name in the status bar is the target Ctrl+G binds to, so clicking
/// it must open the same menu — and the effort chip next to it must keep its
/// own.
#[test]
fn clicking_the_model_name_opens_the_model_menu() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.status_bar_spans(100);
    app.status_y = 10;

    let (from, to) = app.md_click.expect("the model name is clickable");
    assert_ne!(
        Some((from, to)),
        app.ef_click,
        "the two targets must not be the same cells"
    );
    app.mouse_up(10, (from + to) / 2);
    assert!(
        matches!(app.cur_menu(), Some(Menu::PickModel { .. })),
        "a click on the model opens the model menu"
    );

    app.menu_back();
    let (ef0, ef1) = app.ef_click.expect("the effort chip is clickable");
    app.mouse_up(10, (ef0 + ef1) / 2);
    assert!(
        matches!(app.cur_menu(), Some(Menu::Effort { .. })),
        "the chip keeps its own menu"
    );
}

/// The row above the composer is one slot: the queue drops into it when there
/// is no notice, so a queued message never leaves a hole over the input.
#[test]
fn queue_moves_into_the_free_slot_above_the_input() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.push_segment(Segment::User("go".into()));
    app.pending_queue.push("later message".into());
    let rows: Vec<String> = render_to_string(&mut app, 60, 12)
        .lines()
        .map(String::from)
        .collect();
    let input = rows
        .iter()
        .rposition(|l| l.trim_start().starts_with('›'))
        .expect("composer row");
    assert!(
        rows[input - 1].contains("later message"),
        "queue sits right above the composer: {rows:?}"
    );
    // with a notice the slot is taken: the queue keeps its own row above it
    app.status("network timeout 408", StatusKind::Err);
    let rows: Vec<String> = render_to_string(&mut app, 60, 12)
        .lines()
        .map(String::from)
        .collect();
    let input = rows
        .iter()
        .rposition(|l| l.trim_start().starts_with('›'))
        .expect("composer row");
    assert!(
        rows[input - 1].contains("network timeout"),
        "notice owns the slot: {rows:?}"
    );
    assert!(
        rows[input - 2].contains("later message"),
        "queue moves up one: {rows:?}"
    );
}

#[test]
fn notice_row_fits_width_next_to_queue() {
    #[allow(clippy::type_complexity)]
    let setups: [(fn(&mut App), &str); 3] = [
        (
            |app: &mut App| app.last_checkpoint = Some("edit src/main.rs".into()),
            "edit src/main.rs",
        ),
        (
            |app: &mut App| app.status("network timeout 408", StatusKind::Err),
            "network timeout 408",
        ),
        (
            |app: &mut App| app.retry_line = Some("retrying in 2s (1/3)".into()),
            "retrying in 2s",
        ),
    ];
    for (notice_setup, needle) in setups {
        for w in [60u16, 80, 100, 120] {
            let mut app = test_app("http://127.0.0.1:9/v1".into());
            app.startup = false;
            notice_setup(&mut app);
            let line = app.notice_line(w).expect("a notice must fill the row");
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains(needle), "notice shown: {text:?}");
            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
            assert!(
                used <= w as usize,
                "at width {w}: notice row is {used} columns: {text:?}"
            );
            // queue and notice live in separate rows now: both visible
            app.pending_queue.push("later".into());
            let s = render_to_string(&mut app, w, 30);
            assert!(s.contains("queued 1:"), "queue in its row: {s}");
            assert!(s.contains(needle), "notice in its own row: {s}");
        }
    }
    // priority inside the row: retry first, then toast, then checkpoint
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.last_checkpoint = Some("cp".into());
    app.status("boom", StatusKind::Err);
    app.retry_line = Some("retrying".into());
    let text: String = app
        .notice_line(100)
        .expect("notice")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("retrying"), "retry first: {text:?}");
    app.retry_line = None;
    let text: String = app
        .notice_line(100)
        .expect("notice")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("boom"), "toast next: {text:?}");
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
        ask: None,
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
fn working_line_names_phase_and_hints_esc() {
    use super::super::view::working_line;
    // the live tail row of the transcript: what is happening, how long, how to
    // stop. No aggregate, no click target, and the verb is one span in one
    // static color — the shimmer is gone on purpose, only the marker moves.
    let work = working_line(false, Some(27), 0);
    let work_text: String = work.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        work_text,
        format!(
            "{} working · 27s · esc to interrupt",
            crate::tui::anim::working_mark(0)
        )
    );
    assert_eq!(
        work.spans.iter().filter(|s| s.content == "working").count(),
        1,
        "the verb must not split into brightness steps: {work:?}"
    );
    // a turn that has not started its clock yet still says how to stop it
    let think = working_line(true, None, 1);
    let think_text: String = think.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        think_text,
        format!(
            "{} thinking · esc to interrupt",
            crate::tui::anim::working_mark(1)
        )
    );
}

/// The bottom spinner stays removed: while streaming the status bar must show
/// no spinner glyph anywhere. The running signal is the working row at the tail
/// of the transcript, and `/test anim` is where its marker is picked.
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
        (0..app.menu_len())
            .filter_map(|i| app.menu_row_at(i))
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

    let still_button = (0..app.menu_len()).any(|i| {
        let (line, action) = app.menu_row_at(i).expect("unified index space");
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
        ask: None,
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
/// Runs hermetic in a temp dir: the scan walks every journal under the root,
/// so the repo root would pick up live agent sessions.
#[test]
fn undo_step_reports_a_step_with_no_recorded_writes() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    app.session
        .checkpoints
        .push(("deadbeef".into(), "write src/a.rs".into()));

    let dir = tempfile::tempdir().unwrap();
    // the step belongs to a plan, so the command needs one to scope by
    let mut active = crate::plan::create(
        "revert one step".into(),
        Vec::new(),
        vec!["note".into()],
        vec![crate::plan::NewStep {
            title: "the only step".into(),
        }],
        20_000,
        &crate::plan::Limits::default(),
    )
    .unwrap();
    active.sessions = vec![app.session.id.to_string()];
    crate::plan::store(dir.path(), &active).unwrap();
    crate::plan::apply(
        &mut active,
        crate::plan::Op::Start {
            id: "1".into(),
            confirm: None,
        },
        &crate::plan::Limits::default(),
        None,
    )
    .unwrap();
    crate::plan::store(dir.path(), &active).unwrap();

    app.undo_step_in(dir.path(), "3");
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

/// Step numbers restart per plan, so without one the host cannot say which
/// step is meant — and cannot reopen it, which would leave the tree reverted
/// while the plan still claimed the work. Refusing is the answer.
#[test]
fn undo_step_refuses_without_an_active_plan() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    let dir = tempfile::tempdir().unwrap();

    app.undo_step_in(dir.path(), "3");

    let status = app
        .toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default();
    assert!(
        status.contains("needs an active plan") && status.contains("/undo <n>"),
        "the refusal must name the alternative: {status:?}"
    );
    assert!(
        !dir.path().join(".sqwai").exists(),
        "a refusal touches nothing: {:?}",
        dir.path()
    );
}

/// Undo coverage is silent in full mode and names the reason when degraded.
/// The tag leads the toast so it survives width truncation, which cuts the
/// tail on narrow terminals.
#[test]
fn undo_coverage_tag_is_empty_in_full_mode_and_names_the_reason_when_degraded() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert_eq!(app.undo_coverage_tag(true), "");
    app.cfg.undo.shadow = crate::config::ShadowStore::Off;
    assert_eq!(app.undo_coverage_tag(false), " (files-only, shadow off)");
    // the toast names what actually failed. With the store switched back on, a
    // setting is no longer the cause, and blaming one would send the user to
    // install something they already have.
    app.cfg.undo.shadow = crate::config::ShadowStore::Local;
    let tag = app.undo_coverage_tag(false);
    assert!(tag.starts_with(" (files-only, "), "{tag}");
    assert!(!tag.contains("shadow off"), "{tag}");
}

/// The first /undo call sets the baseline silently; only a change toasts.
/// A degraded mode at startup is reported by the coverage tag, not by a
/// transition that has no baseline to compare against.
#[test]
fn undo_transition_is_silent_first_and_fires_only_on_change() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    assert_eq!(app.undo_transition(true), None);
    assert_eq!(app.undo_transition(true), None);
    let down = app.undo_transition(false).expect("must fire on change");
    assert!(down.contains("files-only"), "{down}");
    assert_eq!(app.undo_transition(false), None);
    let up = app.undo_transition(true).expect("must fire on recovery");
    assert!(up.contains("full"), "{up}");
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
    // the turn note is the chat's last row now, not a toast over it
    let reported: String = app
        .segments
        .iter()
        .filter_map(|s| match s {
            Segment::Status { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ");
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
        Some(Segment::Tool {
            ask: None,
            output: a,
            ..
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
        Segment::Tool { ask: Some(ask), .. } => {
            assert_eq!(ask.picked[0], vec![false, true], "single pick replaces");
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// The free-answer row is a peer of the options, so a single-choice question
/// carries one filled marker at a time — from either side.
#[test]
fn single_choice_free_text_and_options_are_one_radio() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    let seg = app.active_ask_seg().expect("live ask");
    let set_free = |app: &mut App, text: &str| {
        if let Some(Segment::Tool { ask: Some(ask), .. }) = app.segments.get_mut(seg) {
            ask.custom[0] = text.to_string();
        }
        app.touch_segment(seg);
    };
    let picks = |app: &App| -> Vec<bool> {
        match &app.segments[seg] {
            Segment::Tool { ask: Some(ask), .. } => ask.picked[0].clone(),
            other => panic!("no ask here: {other:?}"),
        }
    };

    app.inline_ask_open_custom(0);
    set_free(&mut app, "ffff");
    assert_eq!(app.inline_ask_text(seg), "Q1: ffff | Q2: (no answer)");
    // choosing an option retires the typed answer
    app.inline_ask_select(0, 1);
    assert_eq!(app.inline_ask_text(seg), "Q1: beta | Q2: (no answer)");
    // choosing the free row retires the option
    set_free(&mut app, "ffff");
    app.inline_ask_open_custom(0);
    assert_eq!(picks(&app), vec![false, false], "no option is filled");
    assert_eq!(app.inline_ask_text(seg), "Q1: ffff | Q2: (no answer)");
}

/// The typed answer is the item after the last option: it carries that number,
/// and the number opens the editor.
#[test]
fn the_free_answer_is_the_next_number() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture()); // Q1 has two options + free text
    let s = render_to_string(&mut app, 60, 30);
    assert!(
        s.contains("3. your answer"),
        "numbered after the options:\n{s}"
    );

    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Event::Key(KeyEvent::new(
        KeyCode::Char('3'),
        KeyModifiers::empty(),
    )))
    .unwrap();
    app.poll_input(&rx).unwrap();
    assert_eq!(app.ask_custom_focus, Some(0), "its digit opens the editor");
}

/// Choosing an option repaints one glyph in place — the block keeps its height
/// — so the view must stay where the user left it. Forcing follow made a
/// multi-question ask snap to its bottom on every click.
#[test]
fn picking_an_option_does_not_move_the_view() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    push_inline_ask(&mut app, ask_fixture());
    app.follow = false;
    app.inline_ask_select(0, 1);
    assert!(!app.follow, "single pick keeps the viewport");
    app.inline_ask_toggle(1, 0);
    assert!(!app.follow, "multi pick keeps the viewport");
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
        matches!(
            app.segments[seg],
            Segment::Tool {
                ask: Some(_),
                expanded: true,
                ..
            }
        ),
        "live questions open unfolded"
    );
    app.inline_ask_select(0, 1);
    app.inline_ask_confirm();
    assert!(
        matches!(
            app.segments[seg],
            Segment::Tool {
                ask: None,
                expanded: false,
                ..
            }
        ),
        "the answer settles to the one-line head"
    );
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("ask_user"), "head names the tool:\n{s}");
    assert!(s.contains("Pick one"), "head carries the question:\n{s}");
    assert!(!s.contains("beta"), "the questionnaire folds away:\n{s}");
}

#[test]
fn ask_user_head_click_folds_and_unfolds() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.startup = false;
    push_inline_ask(&mut app, ask_fixture());
    app.inline_ask_confirm();
    app.handle_tool_notice(
        "ask_user".to_string(),
        "Q1: beta | Q2: x, y".to_string(),
        true,
        None,
        Some("call-ask".to_string()),
    );
    let seg = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Tool { name, .. } if name == "ask_user"))
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
    // head click reveals the recorded answer, like any finished call
    app.click(abs);
    assert!(
        matches!(app.segments[seg], Segment::Tool { expanded: true, .. }),
        "head click unfolds"
    );
    let s = render_to_string(&mut app, 100, 30);
    assert!(s.contains("Q1: beta"), "the answer is the body:\n{s}");
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
            Segment::Tool {
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
    // expanded layout: head row, Q1 header, question, alpha, beta at offset 4
    app.click(start + 4);
    match &app.segments[seg] {
        Segment::Tool { ask: Some(ask), .. } => {
            assert_eq!(ask.picked[0], vec![false, true], "option click selects")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        matches!(
            app.segments[seg],
            Segment::Tool {
                ask: Some(_),
                expanded: true,
                ..
            }
        ),
        "option click must not fold the questionnaire"
    );
}

/// The claim behind the merge: the live turn and a session read back from disk
/// paint the same rows for the same call, because there is one renderer.
#[test]
fn an_answered_ask_renders_identically_after_reload() {
    use crate::providers::{Message, Role, ToolCallReq};
    let args = serde_json::json!({
        "question": "Which part?",
        "options": [{"label": "prompts"}, {"label": "tui"}],
    });
    let summary = crate::agent::tools::call_summary("ask_user", &args);
    let answer = "tui";

    // the live turn: ToolStart opens the row, the ask fills it, the answer closes it
    let mut live = test_app("http://127.0.0.1:9/v1".into());
    live.startup = false;
    live.handle_tool_start("ask_user".into(), summary.clone(), Some("c1".into()));
    live.open_ask(
        "c1",
        7,
        vec![crate::agent::loop_task::AskQuestion {
            header: String::new(),
            question: "Which part?".into(),
            options: vec!["prompts", "tui"]
                .into_iter()
                .map(|l| crate::agent::loop_task::AskOption {
                    label: l.into(),
                    description: None,
                    recommended: false,
                })
                .collect(),
            multiple: false,
            allow_free: false,
        }],
    );
    live.inline_ask_select(0, 1);
    live.inline_ask_confirm();
    live.handle_tool_notice(
        "ask_user".into(),
        answer.into(),
        true,
        None,
        Some("c1".into()),
    );

    // the same transcript restored from disk
    let mut reloaded = test_app("http://127.0.0.1:9/v1".into());
    reloaded.startup = false;
    reloaded.session.messages = vec![
        Message::new(Role::User, "hi"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "c1",
            "ask_user",
            args.clone(),
        )]),
        Message::tool_result("c1", answer, false),
    ];
    reloaded.clear_segments();
    reloaded.load_history_segments();

    // collapsed head, then the unfolded body: both states must agree
    let paint = |app: &mut App, expanded: bool| -> Vec<String> {
        let i = app
            .segments
            .iter()
            .position(|s| matches!(s, Segment::Tool { name, .. } if name == "ask_user"))
            .expect("the ask row");
        if let Some(Segment::Tool {
            expanded: e, flash, ..
        }) = app.segments.get_mut(i)
        {
            *e = expanded;
            // the finish wave is an animation, not the settled row
            *flash = None;
        }
        app.touch_segment(i);
        app.rebuild_cache(80);
        app.cache_lines
            .iter()
            .zip(app.cache_rowseg.iter())
            .filter(|(_, tag)| **tag == Some(i))
            .map(|(line, _)| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    };
    for expanded in [false, true] {
        let live_rows = paint(&mut live, expanded);
        let reloaded_rows = paint(&mut reloaded, expanded);
        assert_eq!(
            live_rows, reloaded_rows,
            "one call, one renderer (expanded={expanded})"
        );
        if expanded {
            assert!(
                live_rows.iter().any(|r| r.contains("tui")),
                "the answer is the body: {live_rows:?}"
            );
        }
    }
}

#[test]
fn ask_user_travels_on_the_tool_row_path() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    let before = app.segments.len();
    app.handle_tool_start(
        "ask_user".to_string(),
        "anything".to_string(),
        Some("c1".to_string()),
    );
    assert_eq!(app.segments.len(), before + 1, "the ask opens one row");
    app.open_ask("c1", 7, ask_fixture());
    app.handle_tool_notice(
        "ask_user".to_string(),
        "answer".to_string(),
        true,
        None,
        Some("c1".to_string()),
    );
    assert_eq!(
        app.segments.len(),
        before + 1,
        "the notice closes the same row: no second one"
    );
    let i = app
        .segments
        .iter()
        .position(|s| matches!(s, Segment::Tool { call_id: Some(id), .. } if id == "c1"))
        .expect("the ask row");
    match &app.segments[i] {
        Segment::Tool {
            ok,
            output,
            ask,
            name,
            ..
        } => {
            assert_eq!(name, "ask_user");
            assert_eq!(*ok, Some(true), "closed by the notice");
            assert_eq!(output, "answer");
            assert!(ask.is_none(), "the questionnaire goes with the answer");
        }
        other => panic!("unexpected {other:?}"),
    }
    // ordinary tools are unaffected
    app.handle_tool_start("read".to_string(), "a.rs".to_string(), None);
    assert_eq!(app.segments.len(), before + 2);
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
    // layout for the fixture: tool head row, then header, question, 2 options,
    // custom, separator, header, question, 2 options, confirm
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
    assert_eq!(
        at(0),
        None,
        "the head row folds the body, it answers nothing"
    );
    assert_eq!(at(1), None, "header is not clickable");
    assert_eq!(at(2), None, "question is not clickable");
    assert_eq!(
        at(3),
        Some((seg, super::super::view::AskRow::Option { q: 0, opt: 0 }))
    );
    assert_eq!(
        at(4),
        Some((seg, super::super::view::AskRow::Option { q: 0, opt: 1 }))
    );
    assert_eq!(
        at(5),
        Some((seg, super::super::view::AskRow::Custom { q: 0 }))
    );
    assert_eq!(at(6), None, "separator is not clickable");
    assert_eq!(at(7), None, "second header is not clickable");
    assert_eq!(at(8), None, "second question is not clickable");
    assert_eq!(
        at(9),
        Some((seg, super::super::view::AskRow::Option { q: 1, opt: 0 }))
    );
    assert_eq!(
        at(10),
        Some((seg, super::super::view::AskRow::Option { q: 1, opt: 1 }))
    );
    assert_eq!(at(11), Some((seg, super::super::view::AskRow::Confirm)));
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
    // click the second option of Q1 (offset 4 in the layout above)
    app.click(start + 4);
    assert!(
        app.active_ask_seg().is_some(),
        "click must not dismiss the ask"
    );
    match &app.segments[seg] {
        Segment::Tool { ask: Some(ask), .. } => assert_eq!(ask.picked[0], vec![false, true]),
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
    assert!(start <= ask && ask < end, "ask joins the work run");
    // …and a successful turn summarizes it the same way
    app.finalize_activity_group();
    let g = app.activity_groups.last().expect("group must be frozen");
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
        Segment::Tool { ask: Some(ask), .. } => assert_eq!(ask.id, 7),
        other => panic!("wrong segment resolved: {other:?}"),
    }
    app.inline_ask_select(0, 0);
    match &app.segments[after] {
        Segment::Tool { ask: Some(ask), .. } => assert_eq!(ask.picked[0], vec![true, false]),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn plain_click_in_composer_does_not_start_selection() {
    let mut app = test_app("http://127.0.0.1:9/v1".into());
    app.last_input = ratatui::layout::Rect::new(0, 10, 40, 3);
    app.input_mouse_down(10, 5);
    // The press captures the button — that is what routes a following drag to
    // the composer instead of the chat — but it selects nothing until the mouse
    // actually moves.
    assert!(!app.input.is_selecting());
    assert!(app.input_dragging, "the composer holds the press");
    app.input_mouse_up();
    assert!(!app.input.is_selecting(), "a click leaves no selection");
    assert!(!app.input_dragging, "and releases the button");
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
            ask: None,
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

    // Verify the turn summarizes commentary + 2 real tool calls.
    // Commentary never counts as a call.
    assert_eq!(app.activity_groups.len(), 1);
    let g = &app.activity_groups[0];
    assert_eq!(g.seg_start, 1);
    assert_eq!(g.seg_end, 5);
    assert_eq!(g.calls, 2);

    // Rendering check, flat: every row visible, no footer
    app.rebuild_cache(80);
    let screen = rendered(&app);
    assert!(
        screen.contains("Let me inspect the file."),
        "preamble: {screen}"
    );
    assert!(screen.contains("a.rs"), "tool row: {screen}");
    assert!(
        screen.contains("I have finished the fix."),
        "answer: {screen}"
    );
    assert!(!screen.contains("calls"), "no aggregate: {screen}");
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

    // rows render flat: no unfolding step anymore
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
        ask: None,
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
        !comm_text.starts_with(' '),
        "commentary flush left: {comm_text:?}"
    );
    assert!(
        !tool_text.starts_with(' '),
        "tool flush left like commentary: {tool_text:?}"
    );
}

#[test]
fn commentary_wrapped_rows_flush_left_and_plain() {
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
            !text.starts_with(' '),
            "every row flush left like tool rows: {text:?}"
        );
        assert!(
            l.spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::DIM)),
            "body spans render in normal colors like the final answer: {text:?}"
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

    // Activity summarized like any finished turn; the abort
    // surfaces through the status row, not an open block
    assert_eq!(app.activity_groups.len(), 1);
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
            status: crate::config::ModelStatus::Active,
        },
    );
    let cfg = Config {
        last_model: "m".into(),
        legacy_default_model: String::new(),
        providers,
        models,
        safety: Default::default(),
        web: Default::default(),
        ui: Default::default(),
        mcp: Default::default(),
        lsp: Default::default(),
        skills: Default::default(),
        memory: Default::default(),
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
