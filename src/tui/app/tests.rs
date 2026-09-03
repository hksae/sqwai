#![allow(unused_imports)]
use super::menus::{Menu, MenuAction};
use super::view::blank;
use super::*;

mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig, ProviderConfig, WireFormat};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn mock_sse_server(reasoning: &str, content: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reasoning = reasoning.to_string();
        let content = content.to_string();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut req = String::new();
            loop {
                req.clear();
                if reader.read_line(&mut req).unwrap() == 0 || req == "\r\n" {
                    break;
                }
            }
            let mut out = stream;
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{reasoning}\"}}}}]}}\n\n\
                 data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n\
                 data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5}}}}\n\n\
                 data: [DONE]\n\n"
            );
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            // do not tear the socket down in the same instant: the client must
            // be able to read the whole body before the connection dies
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(300));
        });
        (format!("http://{addr}/v1"), h)
    }

    fn test_app(url: String) -> App {
        let mut providers = BTreeMap::new();
        providers.insert(
            "p".to_string(),
            ProviderConfig {
                format: WireFormat::Openai,
                base_url: url,
                api_key: Some("test-key".into()),
                api_key_env: None,
            },
        );
        let mut models = BTreeMap::new();
        models.insert(
            "m".to_string(),
            ModelConfig {
                provider: "p".into(),
                id: "test-model".into(),
                context: 1000,
                thinking: ThinkingLevel::Off,
                price_in: None,
                price_out: None,
            },
        );
        let cfg = Config {
            default_model: "m".into(),
            default_thinking: crate::config::ThinkingLevel::Off,
            providers,
            models,
            safety: Default::default(),
            ui: Default::default(),
            mcp: Default::default(),
            lsp: Default::default(),
            skills: Default::default(),
        };
        let session = Session::new("m".into(), 1000);
        App::new(cfg, session, false).unwrap()
    }

    #[tokio::test]
    async fn dump_request_body() {
        use std::sync::{Arc, Mutex};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut content_length = 0usize;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|s| s.trim().parse::<usize>().ok())
                {
                    content_length = v;
                }
            }
            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).ok();
            *cap.lock().unwrap() = Some(String::from_utf8_lossy(&buf).into_owned());
            drop(reader);
            drop(stream); // close so the client stops waiting
        });
        let (_url, _) = mock_sse_server("x", "y");
        let mut app = test_app(format!("http://{addr}/v1"));
        app.input = App::fresh_input("hi".into());
        app.submit();
        let mut tries = 0;
        while app.streaming && tries < 600 {
            app.poll_agent();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            tries += 1;
        }
        h.join().unwrap();
        let body = captured.lock().unwrap().clone().unwrap_or_default();
        println!("=== BODY ===\n{body}");
        assert!(
            body.contains("\"model\":\"test-model\""),
            "model field missing: {body}"
        );
    }

    #[tokio::test]
    #[ignore = "hits the real API; run with --ignored"] // requires SQWAI_API_KEY
    async fn real_zen_smoke() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "zen".to_string(),
            ProviderConfig {
                format: WireFormat::Openai,
                base_url: "https://opencode.ai/zen/v1".into(),
                api_key: std::env::var("SQWAI_API_KEY").ok(),
                api_key_env: None,
            },
        );
        let mut models = BTreeMap::new();
        models.insert(
            "m".to_string(),
            ModelConfig {
                provider: "zen".into(),
                id: "x-preview-f-free".into(),
                context: 1_000_000,
                thinking: ThinkingLevel::Off,
                price_in: None,
                price_out: None,
            },
        );
        let cfg = Config {
            default_model: "m".into(),
            default_thinking: crate::config::ThinkingLevel::Off,
            providers,
            models,
            safety: Default::default(),
            ui: Default::default(),
            mcp: Default::default(),
            lsp: Default::default(),
            skills: Default::default(),
        };
        let mut app = App::new(cfg, Session::new("m".into(), 1_000_000), false).unwrap();
        app.input = App::fresh_input("say hi in 3 words".into());
        app.submit();
        let mut waited = 0u32;
        while app.streaming && waited < 1800 {
            app.poll_agent();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            waited += 1;
        }
        for (i, s) in app.segments.iter().enumerate() {
            match s {
                Segment::Assistant { text, live } => {
                    println!(
                        "seg[{i}] ASSISTANT live={live} len={} {:?}",
                        text.chars().count(),
                        text.chars().take(60).collect::<String>()
                    )
                }
                Segment::Thinking { text, live, .. } => {
                    println!("seg[{i}] THINKING live={live} len={}", text.chars().count())
                }
                Segment::Status { text, kind } => println!("seg[{i}] STATUS {kind:?}: {text}"),
                Segment::Subagent {
                    id, task, status, ..
                } => println!("seg[{i}] SUBAGENT {id} {status}: {task}"),
                Segment::Tool {
                    name,
                    args,
                    ok,
                    output,
                    ..
                } => println!("seg[{i}] TOOL {name} ({args}) ok={ok:?}: {output}"),
                Segment::User(t) => println!("seg[{i}] USER: {t}"),
            }
        }
        assert!(!app.streaming, "turn still streaming after 180s");
        assert!(
            app.segments
                .iter()
                .any(|s| matches!(s, Segment::Assistant { text, live: false } if !text.is_empty())),
            "no completed assistant answer"
        );
    }

    #[tokio::test]
    async fn full_turn_streams_and_cleans_placeholder() {
        let (url, server) = mock_sse_server("th", "Hello");
        let mut app = test_app(url);
        app.input = App::fresh_input("hi".into());
        app.submit();
        assert!(app.streaming);
        let mut tries = 0;
        while app.streaming && tries < 1500 {
            app.poll_agent();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            tries += 1;
        }
        server.join().unwrap();
        assert!(!app.streaming, "turn did not finish");
        let answered = app
            .segments
            .iter()
            .any(|s| matches!(s, Segment::Assistant { text, live: false } if text == "Hello"));
        assert!(answered, "no completed assistant segment");
        assert!(
            !app.segments
                .iter()
                .any(|s| matches!(s, Segment::Thinking { text, .. } if text.is_empty())),
            "empty thinking ghost left behind"
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
            app.segments.iter().any(|s| matches!(
                s,
                Segment::Status {
                    kind: StatusKind::Err,
                    ..
                }
            )),
            "no error status shown"
        );
    }

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
        assert!(app.cfg.providers.get("p").is_none(), "provider not deleted");
        assert!(
            app.cfg.models.get("m").is_none(),
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
            app.cfg.providers.get("p").is_some(),
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
                thinking: ThinkingLevel::Off,
                price_in: None,
                price_out: None,
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
    fn fork_at_copies_prefix_and_switches() {
        use crate::providers::Role;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let parent_id = app.session.id.to_string();
        app.session.push(Role::User, "one");
        app.session.push(Role::Assistant, "two");
        app.session.push(Role::User, "three");

        app.run_action(MenuAction::ForkAt(1));

        // now living in the fork with only the first two messages
        assert_ne!(app.session.id.to_string(), parent_id);
        assert_eq!(app.session.messages.len(), 2);
        assert_eq!(
            app.session.forked_from_id.as_deref(),
            Some(parent_id.as_str())
        );
        let chat: Vec<&Segment> = app
            .segments
            .iter()
            .filter(|s| !matches!(s, Segment::Status { .. }))
            .collect();
        assert_eq!(chat.len(), 2, "fork history rendered");
    }

    #[test]
    fn pin_from_menu_does_not_pollute_chat() {
        use crate::providers::Role;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let mut s = Session::new("m".into(), 1000);
        s.push(Role::User, "x");
        let id = s.id.to_string();
        app.sessions = vec![s];
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
            app.menu_status
                .as_ref()
                .is_some_and(|(t, _)| t.contains("pinned")),
            "no in-menu notice"
        );
        assert!(app.sessions[0].pinned);
    }

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
    fn error_status_shows_in_bar() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.status("provider boom", StatusKind::Err);
        let spans = app.status_bar_spans(120);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("err: provider boom"), "bar: {text}");
    }

    #[test]
    fn typewriter_reveals_gradually_and_drains() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.pending_reveal = "Hello".into();
        app.segments.push(Segment::Assistant {
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
        app.segments.push(Segment::Assistant {
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
        app.session.push(Role::User, "привет");
        app.session.push(Role::Assistant, "привет!");
        app.segments.push(Segment::User("привет".into()));
        app.segments.push(Segment::Assistant {
            text: "привет!".into(),
            live: false,
        });
        app.streaming = true;
        app.segments.push(Segment::User("как дела".into()));
        app.segments.push(Segment::Assistant {
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
        let dupes = answers.iter().filter(|a| *a == &"привет!").count();
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
    fn thinking_levels_include_off_for_status_bar_and_model_settings() {
        assert!(ThinkingLevel::SELECTABLE.contains(&ThinkingLevel::Off));
        assert_eq!(ThinkingLevel::SELECTABLE, ThinkingLevel::ALL);
    }

    #[test]
    fn command_popup_contains_only_command_names() {
        assert!(COMMANDS.iter().all(|command| !command.contains(' ')));
        assert!(COMMANDS.contains(&"/undo"));
        assert!(COMMANDS.contains(&"/skill"));
    }

    #[test]
    fn busy_status_is_unique_and_expires() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.show_busy_status();
        app.show_busy_status();
        assert_eq!(
            app.segments
                .iter()
                .filter(|segment| matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS))
                .count(),
            1
        );
        app.busy_until = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        if app
            .busy_until
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            app.clear_busy_statuses();
            app.busy_until = None;
        }
        assert!(!app.segments.iter().any(|segment| {
            matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS)
        }));
    }

    #[test]
    fn busy_statuses_are_deduplicated_and_cleared_on_finish() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.show_busy_status();
        app.show_busy_status();
        assert_eq!(
            app.segments
                .iter()
                .filter(|segment| matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS))
                .count(),
            1
        );
        app.streaming = true;
        app.finish_turn(Err("aborted".into()));
        assert!(!app.segments.iter().any(|segment| {
            matches!(segment, Segment::Status { text, .. } if text == App::BUSY_STATUS)
        }));
    }
    #[test]
    fn narrow_tool_frames_keep_both_borders_on_one_row() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.segments.push(Segment::Tool {
            name: "patch".into(),
            args: String::new(),
            ok: Some(true),
            output: "a very long line with wide chars 界界界界 and more text".into(),
            diff: None,
            expanded: true,
        });
        let rows = app.render_segment(0, 18);
        use unicode_width::UnicodeWidthStr;
        let frame_rows: Vec<String> = rows
            .iter()
            .map(|(line, _)| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .filter(|line: &String| line.matches('│').count() == 2)
            .collect();
        assert!(!frame_rows.is_empty());
        assert!(frame_rows.iter().all(|line| {
            UnicodeWidthStr::width(line.as_str()) == 18
                && line.starts_with("    │ ")
                && line.ends_with(" │")
        }));
    }
    #[test]
    fn long_user_prompt_keeps_both_frame_borders() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.segments.push(Segment::User("Проверь новый инструмент websearch. Выполни поиск по запросу: Rust ratatui TestBackend resize rendering. Покажи 3 результата с заголовками, URL и краткими описаниями.".into()));
        let rows = app.render_segment(0, 30);
        let text: Vec<String> = rows
            .iter()
            .map(|(line, _)| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert!(text.iter().any(|line| line.matches('│').count() == 2));
        assert!(
            text.iter()
                .filter(|line| line.contains('│'))
                .all(|line| line.matches('│').count() == 2)
        );
    }

    #[test]
    fn resized_terminal_rebuilds_tool_frames_at_chat_width() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.segments.push(Segment::Tool {
            name: "patch".into(),
            args: String::new(),
            ok: Some(true),
            output: "a very long line with wide chars 界界界界 and more text".into(),
            diff: None,
            expanded: true,
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
                let borders = text.matches("│").count();
                assert!(borders == 0 || borders == 2, "broken frame row: {text}");
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
        app.run_action(MenuAction::OpenThemes);
        assert!(matches!(app.cur_menu(), Some(Menu::Themes)));
        app.menu_back();
        app.menu_back();
        app.run_action(MenuAction::OpenProviders);
        assert!(matches!(app.cur_menu(), Some(Menu::Providers)));
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
        let before = app.cfg.ui.show_cost;
        app.run_action(MenuAction::ToggleShowCost);
        assert_eq!(app.cfg.ui.show_cost, !before);
        assert!(matches!(app.cur_menu(), Some(Menu::Appearance)));
    }

    #[test]
    fn themes_menu_applies_and_stays_open() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::Themes);
        // 21 static + 4 animated themes, listed back-to-back (no header/separator)
        let total = crate::tui::theme::THEMES.len() + crate::tui::theme::ANIMATED_THEMES.len();
        assert_eq!(app.menu_rows.len(), total, "all palettes listed");
        // every theme row carries its own name (swatch squares were removed)
        let rows: Vec<String> = app
            .menu_rows
            .iter()
            .map(|r| {
                r.0.spans
                    .iter()
                    .map(|s| s.content.as_ref().to_string())
                    .collect::<String>()
            })
            .collect();
        for t in crate::tui::theme::THEMES.iter() {
            assert!(
                rows.iter().any(|n| n.contains(t.name)),
                "static theme {} listed",
                t.name
            );
        }
        for t in crate::tui::theme::ANIMATED_THEMES.iter() {
            assert!(
                rows.iter().any(|n| n.contains(t.name)),
                "animated theme {} listed",
                t.name
            );
        }
        app.run_action(MenuAction::SetTheme(5));
        assert_eq!(crate::tui::theme::theme_index(), 5);
        assert_eq!(app.cfg.ui.theme, 5, "choice persisted");
        // render caches are invalidated so nothing keeps the old palette
        assert!(app.seg_cache.is_empty());
        assert!(app.cache_lines.is_empty());
        assert!(
            matches!(app.cur_menu(), Some(Menu::Themes)),
            "menu must stay open after applying"
        );
        assert!(
            app.menu_status.is_none(),
            "no status note is shown on theme switch"
        );
        // animated theme selects and persists too
        app.run_action(MenuAction::SetAnimTheme(0));
        assert_eq!(crate::tui::theme::anim_theme_index(), Some(0));
        assert_eq!(app.cfg.ui.anim_theme, Some(0), "anim choice persisted");
        // only one "*" marker total — picking an animated theme must deselect
        // the static one (no two active indicators at once)
        let stars = app
            .menu_rows
            .iter()
            .filter(|(l, _)| l.spans.iter().any(|s| s.content.contains(" *")))
            .count();
        assert_eq!(stars, 1, "exactly one theme is marked active");
        crate::tui::theme::set_theme(0); // restore default for other tests
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
        app.sessions = vec![a, b];
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
        assert!(app.menu_status.is_some(), "notice stays inside the menu");
    }

    #[test]
    fn animated_theme_makes_seg_key_tick_dependent() {
        // code block frames are baked into cached segments; for them to
        // animate, seg_key must change with the anim tick while a theme is on
        use crate::tui::app::Segment;
        let app = test_app("http://127.0.0.1:9/v1".into());
        let seg = Segment::User("hello".into());
        // static theme: key is independent of the tick
        let k_static = app.seg_key(&seg);
        crate::tui::theme::set_anim_tick(123);
        assert_eq!(app.seg_key(&seg), k_static, "static key ignores the tick");
        // animated theme: key tracks the tick -> segments re-render each frame
        crate::tui::theme::set_anim_theme(0); // lava (only animated theme left)
        crate::tui::theme::set_anim_tick(0);
        let a = app.seg_key(&seg);
        crate::tui::theme::set_anim_tick(40);
        let b = app.seg_key(&seg);
        crate::tui::theme::set_anim_theme_off();
        assert_ne!(a, b, "seg_key must change with the anim tick");
    }
}
