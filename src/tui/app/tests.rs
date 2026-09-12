#![allow(unused_imports)]
use super::menus::{Menu, MenuAction};
use super::view::{GROUP_BASE, blank};
use super::*;

#[allow(clippy::module_inception)] // this file IS the tests module; the inner mod is required by the test framework
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
                price_in: None,
                price_out: None,
                fallback: None,
            },
        );
        let cfg = Config {
            default_model: "m".into(),
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
            secrets: Default::default(),
            undo: Default::default(),
        };
        let session = Session::new("m".into(), 1000);
        App::new(cfg, session, false, false).unwrap()
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
                continuation: true,
            },
        );
        let mut models = BTreeMap::new();
        models.insert(
            "m".to_string(),
            ModelConfig {
                provider: "zen".into(),
                id: "x-preview-f-free".into(),
                context: 1_000_000,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                price_in: None,
                price_out: None,
                fallback: None,
            },
        );
        let cfg = Config {
            default_model: "m".into(),
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
            secrets: Default::default(),
            undo: Default::default(),
        };
        let mut app = App::new(cfg, Session::new("m".into(), 1_000_000), false, false).unwrap();
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
                Segment::AskUser { questions, .. } => {
                    println!("seg[{i}] ASKUSER {} questions", questions.len())
                }
                Segment::Assistant { text, live } => {
                    println!(
                        "seg[{i}] ASSISTANT live={live} len={} {:?}",
                        text.chars().count(),
                        text.chars().take(60).collect::<String>()
                    )
                }
                Segment::Thinking {
                    text,
                    live,
                    started,
                    ..
                } => {
                    println!(
                        "seg[{i}] THINKING live={live} len={} started={}",
                        text.chars().count(),
                        started.is_some()
                    )
                }
                Segment::Status { text, kind, .. } => println!("seg[{i}] STATUS {kind:?}: {text}"),
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
                Segment::Commentary(t) => println!("seg[{i}] COMMENTARY: {t}"),
                Segment::PlanProposal { draft, decided, .. } => println!(
                    "seg[{i}] PLANPROPOSAL {} steps decided={decided:?}",
                    draft.steps.len()
                ),
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
                AgentEvent::ToolStart { name, summary } => app.handle_tool_start(name, summary),
                AgentEvent::ToolNotice {
                    name,
                    summary,
                    ok,
                    diff,
                } => app.handle_tool_notice(name, summary, ok, diff),
                AgentEvent::TextDelta(t) => app.handle_text_delta(t),
                _ => {}
            }
        };
        ev(AgentEvent::ThinkingDelta("first".into()), &mut app);
        ev(
            AgentEvent::ToolStart {
                name: "read".into(),
                summary: "a.rs".into(),
            },
            &mut app,
        );
        ev(
            AgentEvent::ToolNotice {
                name: "read".into(),
                summary: "lines".into(),
                ok: true,
                diff: None,
            },
            &mut app,
        );
        ev(AgentEvent::ThinkingDelta("second".into()), &mut app);
        ev(
            AgentEvent::ToolStart {
                name: "edit".into(),
                summary: "a.rs".into(),
            },
            &mut app,
        );
        ev(
            AgentEvent::ToolNotice {
                name: "edit".into(),
                summary: "ok".into(),
                ok: true,
                diff: None,
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

    fn rendered(app: &App) -> String {
        app.cache_lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
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
        app.handle_tool_start("read".into(), "a.rs".into());
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
        app.finalize_activity_group(false);

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
    fn failed_turn_leaves_its_activity_group_open() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        finished_turn(&mut app, false);
        app.finalize_activity_group(true);

        let g = &app.activity_groups[0];
        assert_eq!(g.errors, 1);
        assert!(g.expanded, "a failed turn must not collapse silently");

        app.rebuild_cache(80);
        let text = rendered(&app);
        assert!(text.contains("1 error"), "error marker missing: {text}");
        assert!(
            text.contains("read"),
            "the failed call stays visible: {text}"
        );
    }

    #[test]
    fn abort_remaps_activity_group_ranges_past_dropped_rows() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.push_segment(Segment::User("go".into()));
        app.push_segment(Segment::Tool {
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
        app.finalize_activity_group(false);
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
        app.finalize_activity_group(true);

        assert_eq!(app.activity_groups.len(), 1);
        let g = &app.activity_groups[0];
        assert_eq!((g.seg_start, g.seg_end), (1, 3));
        assert!(g.expanded, "a failed turn stays open");

        // and the header folds the block on click
        app.rebuild_cache(80);
        let header = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(GROUP_BASE))
            .expect("header row tagged");
        app.click(header);
        assert!(!app.activity_groups[0].expanded);
        app.rebuild_cache(80);
        assert!(
            !rendered(&app).contains("a.rs"),
            "folded group hides tool rows"
        );
    }

    /// A group must never overlap the previous one: before the fix, an error
    /// with no answer slot anchored on an older turn's answer and the new
    /// group landed on top of the old range.
    #[test]
    fn activity_groups_never_overlap_previous_ranges() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        finished_turn(&mut app, true);
        app.finalize_activity_group(false);
        // second turn: tools ran, then a provider error with no streamed text
        app.push_segment(Segment::User("again".into()));
        app.push_segment(Segment::Tool {
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
        app.finalize_activity_group(true);

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
        app.handle_tool_start("read".into(), "a.rs".into());

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
                price_in: None,
                price_out: None,
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
        assert!(g1.expanded, "a stopped turn's group restores expanded");
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
            app.sessions.push(crate::session::SessionHeader::from_session(&s));
        }
        app.open_menu(Menu::Sessions);
        let area = Rect::new(0, 0, 100, 14);
        let mut buf = Buffer::empty(area);
        app.draw_menu(&mut buf, area);
        let row_text = |y: u16| -> String {
            (0..area.width).map(|x| buf[(x, y)].symbol()).collect()
        };
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
        let row_text = |y: u16| -> String {
            (0..area.width).map(|x| buf[(x, y)].symbol()).collect()
        };
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
        let row_text = |y: u16| -> String {
            (0..area.width).map(|x| buf[(x, y)].symbol()).collect()
        };
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
            .find(|(x, c)| {
                c.symbol() == "│" && *x != r.x && *x != r.right().saturating_sub(1)
            })
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
            super::perf::FrameStat {
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
        assert!(content.contains("# frame t_ms draw_us"), "header:\n{content}");
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
            !app.segments.iter().any(|s| matches!(s, Segment::Status { .. })),
            "toast never lands in the chat"
        );
    }

    #[test]
    fn toast_replaces_previous_notice() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.status("first", StatusKind::Info);
        app.status("second", StatusKind::Warn);
        assert_eq!(toast_text(&app), "second", "new notice wins outright");
        assert!(app.toast.as_ref().is_some_and(|t| t.kind == StatusKind::Warn));
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
                Message::new(Role::Assistant, "").with_tool_calls(vec![
                    ToolCallReq::new("c1", "read", serde_json::json!({})),
                ]),
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
                Message::new(Role::Assistant, "").with_tool_calls(vec![
                    ToolCallReq::new("c1", "read", serde_json::json!({})),
                ]),
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

    #[test]
    fn subagent_chat_click_expands_its_tool_without_switching_chat() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.active_subagent = Some(7);
        app.subagent_chats.insert(
            7,
            vec![Segment::Tool {
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
            super::MODE_PLAN_RGB,
            std::time::Instant::now() - std::time::Duration::from_secs(5),
        ));
        let spans = app.status_bar_spans(120);
        assert_eq!(spans[0].content.as_ref(), " PLAN ");
        assert_eq!(
            spans[0].style,
            crate::tui::theme::Theme::mode_chip_plan()
        );
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
        assert!(
            all.contains("xhigh"),
            "xhigh must be offered: {all}"
        );
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
        let dots: String = buf
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
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
    fn effort_slider_colors_span_gray_to_magenta() {        use crate::tui::theme::Theme;
        use ratatui::style::Color;
        assert_eq!(
            Theme::effort_color(EffortLevel::Off),
            Color::DarkGray
        );
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
        app.handle_tool_start("read".into(), "a.rs".into());
        app.handle_tool_notice("read".into(), "done".into(), true, None);
        let seg = app
            .segments
            .iter()
            .find(|s| matches!(s, Segment::Tool { .. }))
            .expect("tool row");
        assert!(
            matches!(seg, Segment::Tool { ok: Some(true), flash: Some(_), .. }),
            "notice resolves the row and arms the wave"
        );
    }

    #[test]
    fn finish_wave_keeps_geometry_and_settles_static() {
        // env-independent: geometry holds on both paths, and an expired
        // wave renders exactly the static row
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.push_segment(Segment::Tool {
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
        let text: String = rows[0]
            .0
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.starts_with("  "), "{text:?}");
        assert!(text.contains("read"), "{text:?}");

        // expired flash: byte-identical static row
        if let Some(Segment::Tool { flash, .. }) = app.segments.get_mut(0) {
            *flash =
                Some(std::time::Instant::now() - std::time::Duration::from_secs(5));
        }
        let rows = app.render_segment(&app.segments, 0, 80, true);
        assert_eq!(rows[0].0.spans[0].content.as_ref(), "  ✓ ");
        assert_eq!(rows[0].0.spans[0].style, crate::tui::theme::Theme::ok());
        assert_eq!(rows[0].0.spans[1].content.as_ref(), "read");
    }

    #[test]
    fn expired_flash_key_matches_static_key() {
        // the live wave must not pin the cache: expiry returns the key to
        // the static one, so the row repaints settled (no stuck wave frame)
        let app = test_app("http://127.0.0.1:9/v1".into());
        let mk = |flash| Segment::Tool {
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
        assert_eq!(
            app.cfg.models.get("m").unwrap().effort,
            EffortLevel::Xhigh
        );
    }

    #[test]
    fn edit_model_form_options_match_control_cycle() {
        // the form must offer auto-then-every-control, like CycleEffortControl
        use super::forms::{ALWAYS_OPTS, EFFORT_CONTROL_OPTS};
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
        assert_eq!(app.menu_scroll, n - vis, "scroll={} n={n} vis={vis}", app.menu_scroll);
    }

    #[test]
    fn menu_wheel_may_scroll_selection_out_of_view_and_draw_keeps_it() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let (mut app, vis) = wheel_test_app();
        app.menu_wheel(10000);
        assert!(
            app.menu_sel < app.menu_scroll
                || app.menu_sel >= app.menu_scroll + vis,
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
        assert!(app.menu_sel < app.menu_scroll + vis, "page sel must be visible");
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
        use super::menus::ScalarSetting;
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
        assert_eq!(app.popup_items(), vec!["/plan waive".to_string()]);
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

    #[test]
    fn busy_status_is_a_replacing_toast_and_expires() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.show_busy_status();
        app.show_busy_status();
        // one toast, never a chat segment
        assert!(app.toast.as_ref().is_some_and(|t| t.text == App::BUSY_STATUS));
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
        assert!(app.toast.as_ref().is_some_and(|t| t.text == App::BUSY_STATUS));
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
        assert!(texts.iter().any(|t| t.contains("default model  ")));
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
        use super::menus::ScalarSetting;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::EditScalar(ScalarSetting::PlanMaxSteps));
        assert!(app.is_form_menu());
        // prefilled with the current value
        assert_eq!(app.form_fields[0].trimmed(), "24");

        app.form_fields[0] = super::forms::FormField::text("max steps", "32".into());
        app.form_save();
        assert_eq!(app.cfg.plan.max_steps, 32);

        // garbage resets to the default instead of corrupting the config
        app.open_menu(Menu::EditScalar(ScalarSetting::PlanMaxSteps));
        app.form_fields[0] = super::forms::FormField::text("max steps", "abc".into());
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
        assert_eq!(app.cfg.compaction.summary.as_str(), "short");
        app.run_action(MenuAction::CycleCompactionSummary);
        assert_eq!(app.cfg.compaction.summary.as_str(), "off");

        app.run_action(MenuAction::CycleUndoShadow);
        assert_eq!(app.cfg.undo.shadow.as_str(), "user");

        let auto = app.cfg.skills.auto_load;
        app.run_action(MenuAction::ToggleSkillsAutoLoad);
        assert_eq!(app.cfg.skills.auto_load, !auto);
    }

    #[test]
    fn settings_safety_list_add_and_remove() {
        use super::menus::ListSection;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::AddListItem(ListSection::SafetyBlocked));
        app.form_fields[0] = super::forms::FormField::text("blocked patterns", "rm -rf /".into());
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
    fn settings_default_model_pick() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::PickDefaultModel);
        let key = app
            .menu_rows
            .iter()
            .find_map(|(_, action)| match action {
                MenuAction::SetDefaultModel(key) => Some(key.clone()),
                _ => None,
            })
            .expect("picker lists models");
        app.run_action(MenuAction::SetDefaultModel(key.clone()));
        assert_eq!(app.cfg.default_model, key);
    }

    #[test]
    fn settings_mcp_server_crud() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        assert!(app.cfg.mcp.servers.is_empty());

        // add (stdio)
        app.open_menu(Menu::EditMcpServer { index: None });
        let set = |app: &mut App, i: usize, v: &str| {
            let label = app.form_fields[i].label().to_string();
            app.form_fields[i] = super::forms::FormField::text(&label, v.into());
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
            app.form_fields[i] = super::forms::FormField::text(&label, v.into());
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
        use super::menus::ListSection;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.open_menu(Menu::AddListItem(ListSection::SkillsDirs));
        app.form_fields[0] = super::forms::FormField::text("skill directories", "/tmp/s".into());
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
        let before = app.cfg.ui.show_cost;
        app.run_action(MenuAction::ToggleShowCost);
        assert_eq!(app.cfg.ui.show_cost, !before);
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
                    kind: Some(crate::plan::StepKind::Change),
                    refs: Vec::new(),
                },
                crate::plan::NewStep {
                    title: "unrelated file".into(),
                    kind: Some(crate::plan::StepKind::Change),
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
                kind: Some(crate::plan::StepKind::Change),
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
                kind: Some(crate::plan::StepKind::Change),
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
        };
        let seg2 = Segment::AskUser {
            id: 1,
            questions: q,
            picked: vec![vec![false, true, false]],
            custom: vec![String::new()],
            focus: 0,
            answered: None,
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
        let mut data = App::collect_startup_data(&app.cfg, &app.model_cfg, app.read_only);
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

    // ──────────────────────────────────────────────────────────────────────
    // Render-fixture helpers and snapshot suite (§12)
    // ──────────────────────────────────────────────────────────────────────

    /// Build a terminal, draw the app, and return all rows joined by '\n'.
    /// Trailing spaces on every row are trimmed so snapshots stay readable.
    ///
    /// The status bar labels are pinned here so fixtures never read the
    /// checkout they happen to run in: `cwd_label` would otherwise render
    /// the real directory name — including its width, which no output
    /// filter can normalise back — and `plan_step_label` would leak an
    /// active plan from the checkout's live `.sqwai/` state. The literal
    /// `sqwai` keeps the layout byte-identical to the committed snapshots.
    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
        app.cwd_label = "sqwai".to_string();
        app.plan_step_label = String::new();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|row| {
                let s: String = row.iter().map(|cell| cell.symbol()).collect();
                s.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
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
    fn live_activity_header_shimmers_finished_stays_dim() {
        use super::view::{ActivityGroup, activity_header_line};
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
        let text: String = still
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("activity · 3 calls"), "{text:?}");
        assert!(
            still.spans.iter().all(|s| s.style == Theme::dim()),
            "finished header must stay dim: {still:?}"
        );
        // live header at mid-sweep: same text, shaded letters
        let live = activity_header_line(&g, Some(crate::tui::shimmer::SHIMMER_PERIOD_TICKS / 4));
        let live_text: String = live
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
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
        let reported = app.toast.map(|t| t.text).unwrap_or_default();
        assert!(
            reported.contains("API key") || reported.contains("401"),
            "the user should be told what to fix: {reported:?}"
        );
    }

    fn ask_fixture() -> Vec<crate::agent::loop_task::AskQuestion> {
        vec![
            crate::agent::loop_task::AskQuestion {
                header: "Q1".to_string(),
                question: "Pick one".to_string(),
                options: vec![
                    crate::agent::loop_task::AskOption {
                        label: "alpha".to_string(),
                        description: None,
                        recommended: false,
                    },
                    crate::agent::loop_task::AskOption {
                        label: "beta".to_string(),
                        description: Some("second".to_string()),
                        recommended: true,
                    },
                ],
                multiple: false,
                allow_free: true,
            },
            crate::agent::loop_task::AskQuestion {
                header: "Q2".to_string(),
                question: "Pick many".to_string(),
                options: vec![
                    crate::agent::loop_task::AskOption {
                        label: "x".to_string(),
                        description: None,
                        recommended: false,
                    },
                    crate::agent::loop_task::AskOption {
                        label: "y".to_string(),
                        description: None,
                        recommended: false,
                    },
                ],
                multiple: true,
                allow_free: false,
            },
        ]
    }

    fn push_inline_ask(app: &mut App, questions: Vec<crate::agent::loop_task::AskQuestion>) {
        // exercise the real insertion path (before the live answer)
        app.push_ask_segment(7, questions);
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
    fn ask_user_tool_rows_are_suppressed_live() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let before = app.segments.len();
        app.handle_tool_start("ask_user".to_string(), "anything".to_string());
        assert_eq!(app.segments.len(), before, "no Tool row for ask_user");
        app.handle_tool_notice("ask_user".to_string(), "answer".to_string(), true, None);
        assert_eq!(app.segments.len(), before, "no Tool row on notice either");
        // ordinary tools are unaffected
        app.handle_tool_start("read".to_string(), "a.rs".to_string());
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
            Some((seg, super::view::AskRow::Option { q: 0, opt: 0 }))
        );
        assert_eq!(
            at(3),
            Some((seg, super::view::AskRow::Option { q: 0, opt: 1 }))
        );
        assert_eq!(at(4), Some((seg, super::view::AskRow::Custom { q: 0 })));
        assert_eq!(at(5), None, "separator is not clickable");
        assert_eq!(
            at(9),
            Some((seg, super::view::AskRow::Option { q: 1, opt: 1 }))
        );
        assert_eq!(at(10), Some((seg, super::view::AskRow::Confirm)));
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
        app.handle_tool_start("read".to_string(), "a.rs".to_string());
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
        app.finalize_activity_group(false);
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
        app.handle_tool_start("read".into(), "a.rs".into());
        app.handle_tool_notice("read".into(), "file contents".into(), true, None);

        // Turn 2: model outputs preamble text, then calls tool 'edit'
        app.handle_text_delta("Now I see the issue, editing line 10.".into());
        app.handle_tool_start("edit".into(), "a.rs".into());
        app.handle_tool_notice("edit".into(), "done".into(), true, None);

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
        app.handle_tool_start("read".into(), "main.rs".into());
        app.handle_tool_notice("read".into(), "ok".into(), true, None);
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

        let comm_text: String = commentary_rows[0].0.spans.iter().map(|s| s.content.as_ref()).collect();
        let tool_text: String = tool_rows[0].0.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(comm_text.starts_with("  "), "commentary must start with 2 spaces: {comm_text:?}");
        assert!(!comm_text.starts_with("   "), "commentary must not start with 3 spaces");
        assert!(tool_text.starts_with("  "), "tool must start with 2 spaces: {tool_text:?}");
        assert!(!tool_text.starts_with("   "), "tool must not start with 3 spaces");
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
        app.handle_tool_start("bash".into(), "cargo build".into());

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

        // Activity group is expanded so user sees what happened
        assert_eq!(app.activity_groups.len(), 1);
        assert!(app.activity_groups[0].expanded);
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
                price_in: None,
                price_out: None,
                fallback: None,
            },
        );
        let cfg = Config {
            default_model: "m".into(),
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

    /// Transient notices never become chat segments anymore — they toast
    /// for 3s at the bottom bar. Tests assert on the toast text.
    fn toast_text(app: &App) -> String {
        app.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default()
    }

    #[test]
    fn plan_delete_user_command_flow() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-plan-delete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();
        app.session.plan_id = None;

        // 1. When no active plan exists:
        app.plan_command("/plan delete");
        assert!(
            app.menu_stack.is_empty(),
            "menu should not open when no active plan"
        );
        assert!(
            toast_text(&app) == "no active plan",
            "refusal toasts: {:?}",
            toast_text(&app)
        );

        // 2. Create an active plan in temp_dir
        let limits = plan::Limits { max_steps: 10 };
        let created = plan::create(
            "test delete goal".into(),
            vec![],
            vec![],
            vec![plan::NewStep {
                title: "step 1".into(),
                kind: None,
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap();
        plan::store(&temp_dir, &created).unwrap();
        app.session.plan_id = Some(created.id.clone());
        let plan_file = plan::plans_dir(&temp_dir).join(format!("{}.json", created.id));
        assert!(plan_file.exists());

        // 3. /plan delete should open confirmation dialog
        app.plan_command("/plan delete");
        assert!(matches!(
            app.cur_menu(),
            Some(Menu::ConfirmDelete { label, .. }) if label == "Are you sure? (y/N)"
        ));

        // 4. Confirming delete removes file, clears session.plan_id, and records journal event
        app.run_confirm_action();
        assert!(!plan_file.exists(), "plan file must be deleted");
        assert_eq!(app.session.plan_id, None, "session plan_id must be cleared");
        assert!(
            toast_text(&app) == "plan deleted; no active plan",
            "success toasts, it must not vanish with the menu: {:?}",
            toast_text(&app)
        );

        // Verify journal has plan_deleted event
        let records =
            crate::agent::journal::Journal::records_for(&temp_dir, &app.session.id.to_string())
                .unwrap();
        assert!(
            records.iter().any(|r| r.kind == "plan_deleted"
                && r.fields.get("plan_id").and_then(|v| v.as_str()) == Some(&created.id)),
            "journal must record plan_deleted event"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn plan_confirm_user_command_flow() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir =
            std::env::temp_dir().join(format!("sqwai-test-plan-confirm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();
        app.session.plan_id = None;

        // usage without a reason
        app.plan_command("/plan confirm 0");
        assert!(
            toast_text(&app).starts_with("usage: /plan confirm"),
            "usage toasts: {:?}",
            toast_text(&app)
        );

        let limits = plan::Limits { max_steps: 10 };
        let created = plan::create(
            "test confirm goal".into(),
            vec![],
            vec!["manual: eyeball it".into()],
            vec![plan::NewStep {
                title: "step 1".into(),
                kind: None,
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap();
        plan::store(&temp_dir, &created).unwrap();
        app.session.plan_id = Some(created.id.clone());

        app.plan_command("/plan confirm 0 looks good");
        assert_eq!(toast_text(&app), "acceptance 0 confirmed");
        let reloaded = plan::read_plan_file(&temp_dir, &created.id).unwrap();
        assert_eq!(
            reloaded.acceptance[0].validation.status,
            plan::ValidationStatus::Passed
        );
        assert_eq!(
            reloaded.acceptance[0]
                .validation
                .receipts
                .last()
                .and_then(|r| r.runner.as_deref()),
            Some("manual")
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn plan_delete_prefers_session_plan_over_most_recent() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir = std::env::temp_dir().join(format!(
            "sqwai-test-plan-delete-multi-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();

        let limits = plan::Limits { max_steps: 10 };
        let mk_plan = |goal: &str| {
            plan::create(
                goal.into(),
                vec![],
                vec![],
                vec![plan::NewStep {
                    title: "step 1".into(),
                    kind: None,
                    refs: vec![],
                }],
                1000,
                &limits,
            )
            .unwrap()
        };
        // session's own (older) plan and a newer unrelated active plan
        let mut plan_x = mk_plan("session plan");
        plan_x.created = "2026-01-01T00:00:00+00:00".to_string();
        plan::store(&temp_dir, &plan_x).unwrap();
        let mut plan_y = mk_plan("other plan");
        plan_y.created = "2026-09-09T00:00:00+00:00".to_string();
        plan::store(&temp_dir, &plan_y).unwrap();
        app.session.plan_id = Some(plan_x.id.clone());

        let file_x = plan::plans_dir(&temp_dir).join(format!("{}.json", plan_x.id));
        let file_y = plan::plans_dir(&temp_dir).join(format!("{}.json", plan_y.id));

        app.plan_command("/plan delete");
        assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
        app.run_confirm_action();

        assert!(!file_x.exists(), "session plan file must be deleted");
        assert!(file_y.exists(), "unrelated active plan must survive");
        assert_eq!(app.session.plan_id, None);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn completed_linked_plan_refuses_tui_mutations() {
        // Defect A: a linked completed plan is read-only history. TUI
        // mutations must refuse it explicitly instead of silently rewriting
        // a finished plan the agent itself can no longer touch.
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir = std::env::temp_dir().join(format!(
            "sqwai-test-plan-completed-guard-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();

        let limits = plan::Limits { max_steps: 10 };
        let mut finished = plan::create(
            "done goal".into(),
            vec![],
            vec!["manual: eyeball it".into()],
            vec![plan::NewStep {
                title: "step 1".into(),
                kind: None,
                refs: vec![],
            }],
            1000,
            &limits,
        )
        .unwrap();
        finished.status = plan::PlanStatus::Completed;
        plan::store(&temp_dir, &finished).unwrap();
        app.session.plan_id = Some(finished.id.clone());
        let revision = finished.revision;

        let last_status = |app: &App| toast_text(app);

        app.plan_command("/plan complete");
        assert!(
            last_status(&app).contains("completed") && last_status(&app).contains("read-only"),
            "complete must refuse: {}",
            last_status(&app)
        );
        app.plan_command("/plan waive 0 looks fine");
        assert!(
            last_status(&app).contains("read-only"),
            "waive must refuse: {}",
            last_status(&app)
        );
        app.plan_command("/plan abandon");
        assert!(
            last_status(&app).contains("read-only"),
            "abandon must refuse: {}",
            last_status(&app)
        );
        app.goal_command("/goal a new direction");
        assert!(
            last_status(&app).contains("read-only"),
            "goal must refuse: {}",
            last_status(&app)
        );
        app.constraints_command("/constraints add something");
        assert!(
            last_status(&app).contains("read-only"),
            "constraints must refuse: {}",
            last_status(&app)
        );

        let after: plan::Plan = serde_json::from_str(
            &std::fs::read_to_string(
                plan::plans_dir(&temp_dir).join(format!("{}.json", finished.id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(after.status, plan::PlanStatus::Completed);
        assert_eq!(after.revision, revision, "refused mutations must not write");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn plan_delete_then_session_has_no_plan() {
        // #171: after delete, another session's active plan must NOT
        // surface as this session's — the session simply has no plan.
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir = std::env::temp_dir().join(format!(
            "sqwai-test-plan-delete-foreign-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();
        let sid = app.session.id.to_string();

        let limits = plan::Limits { max_steps: 10 };
        let mk_plan = |goal: &str| {
            plan::create(
                goal.into(),
                vec![],
                vec![],
                vec![plan::NewStep {
                    title: "step 1".into(),
                    kind: None,
                    refs: vec![],
                }],
                1000,
                &limits,
            )
            .unwrap()
        };
        let mut own = mk_plan("own finished work");
        own.created = "2026-01-01T00:00:00+00:00".to_string();
        own.sessions = vec![sid.clone()];
        plan::store(&temp_dir, &own).unwrap();
        let mut foreign = mk_plan("foreign active work");
        foreign.created = "2026-09-09T00:00:00+00:00".to_string();
        foreign.sessions = vec!["someone-else".into()];
        plan::store(&temp_dir, &foreign).unwrap();
        // `foreign` stays stored but unreferenced below: its presence is
        // the point — it must not surface as this session's plan.
        let _ = &foreign;
        app.session.plan_id = Some(own.id.clone());

        app.plan_command("/plan delete");
        app.run_confirm_action();

        assert_eq!(app.session.plan_id, None);
        let text = toast_text(&app);
        assert!(
            text.contains("no active plan"),
            "delete leaves the session plan-less: {text}"
        );
        assert!(
            app.session_plan().is_none(),
            "no silent fallback onto the foreign plan"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn plan_delete_reports_failure_instead_of_lying() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir = std::env::temp_dir().join(format!(
            "sqwai-test-plan-delete-locked-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();

        // an unremovable entry where the plan file should be: remove_file
        // fails on it on every platform (as with a locked file on Windows)
        let ghost_id = "ghostplan";
        let ghost_path = plan::plans_dir(&temp_dir).join(format!("{ghost_id}.json"));
        std::fs::create_dir_all(&ghost_path).unwrap();
        app.session.plan_id = Some(ghost_id.into());

        app.plan_command("/plan delete");
        assert!(matches!(app.cur_menu(), Some(Menu::ConfirmDelete { .. })));
        app.run_confirm_action();

        assert!(ghost_path.exists(), "unremovable entry must still be there");
        assert_eq!(
            app.session.plan_id.as_deref(),
            Some(ghost_id),
            "plan_id must be kept while the plan still exists"
        );
        assert!(
            toast_text(&app).contains("plan delete failed"),
            "toast must report the failure, not a fake success: {:?}",
            toast_text(&app)
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn tui_commands_operate_on_session_plan_not_newest_global() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp_dir = std::env::temp_dir().join(format!(
            "sqwai-test-plan-session-scope-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        app.project_root = temp_dir.clone();
        let sid = app.session.id.to_string();

        let limits = plan::Limits { max_steps: 10 };
        let mk_plan = |goal: &str| {
            plan::create(
                goal.into(),
                vec![],
                vec!["manual: eyeball it".into()],
                vec![plan::NewStep {
                    title: "step 1".into(),
                    kind: None,
                    refs: vec![],
                }],
                1000,
                &limits,
            )
            .unwrap()
        };
        // session's own older plan vs a newer unrelated active plan
        let mut plan_a = mk_plan("session goal");
        plan_a.created = "2026-01-01T00:00:00+00:00".to_string();
        plan_a.sessions = vec![sid.clone()];
        plan::store(&temp_dir, &plan_a).unwrap();
        let mut plan_b = mk_plan("other goal");
        plan_b.created = "2026-09-09T00:00:00+00:00".to_string();
        plan_b.sessions = vec!["someone-else".into()];
        plan::store(&temp_dir, &plan_b).unwrap();
        app.session.plan_id = Some(plan_a.id.clone());

        // resolver prefers the linked plan over the newest global one
        assert_eq!(app.session_plan().map(|p| p.id), Some(plan_a.id.clone()));

        // /plan waive mutates A, leaves B alone
        app.plan_command("/plan waive 0 looks good");
        let a_after = plan::open(&temp_dir, &plan_a.id).unwrap();
        assert!(matches!(
            a_after.acceptance[0].status,
            plan::AcceptanceStatus::Waived
        ));
        let b_after = plan::open(&temp_dir, &plan_b.id).unwrap();
        assert!(matches!(
            b_after.acceptance[0].status,
            plan::AcceptanceStatus::Pending
        ));

        // /plan complete mutates A, leaves B alone
        let mut a_ready = a_after;
        a_ready.steps[0].status = plan::StepStatus::Done;
        plan::store(&temp_dir, &a_ready).unwrap();
        app.plan_command("/plan complete");
        let a_completed = plan::open(&temp_dir, &plan_a.id).unwrap();
        assert_eq!(a_completed.status, plan::PlanStatus::Completed);
        let b_after_complete = plan::open(&temp_dir, &plan_b.id).unwrap();
        assert_eq!(b_after_complete.status, plan::PlanStatus::Active);

        // /plan abandon mutates linked plan, leaves B alone
        let mut plan_c = mk_plan("session C goal");
        plan_c.sessions = vec![sid.clone()];
        plan::store(&temp_dir, &plan_c).unwrap();
        app.session.plan_id = Some(plan_c.id.clone());
        app.plan_command("/plan abandon");
        let c_abandoned = plan::open(&temp_dir, &plan_c.id).unwrap();
        assert_eq!(c_abandoned.status, plan::PlanStatus::Abandoned);
        let b_still_active = plan::open(&temp_dir, &plan_b.id).unwrap();
        assert_eq!(b_still_active.status, plan::PlanStatus::Active);

        app.session.plan_id = Some(plan_a.id.clone());
        // Menu::Plan renders the session's plan, not the newest global one
        app.open_menu(Menu::Plan);
        let rendered: String = app
            .menu_rows
            .iter()
            .flat_map(|(line, _)| line.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect();
        assert!(rendered.contains(&plan_a.id), "menu must show session plan");
        assert!(
            !rendered.contains(&plan_b.id),
            "menu must not show another session's plan"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn builtin_providers_and_models_are_immutable_and_updateable() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());

        // Built-in provider and model checks
        assert!(app.cfg.is_builtin_provider("gemini"));
        assert!(app.cfg.is_builtin_model("gemini-3.8-flash"));

        // 1. Trying to delete built-in provider is refused
        app.run_action(MenuAction::DeleteProvider("gemini".into()));
        assert!(toast_text(&app).contains("cannot be deleted"));

        // 2. Trying to delete built-in model is refused
        app.run_action(MenuAction::DeleteModel(
            "gemini".into(),
            "gemini-3.8-flash".into(),
        ));
        assert!(toast_text(&app).contains("cannot be deleted"));

        // 3. Trying to edit built-in model is refused
        app.run_action(MenuAction::EditModel(
            "gemini".into(),
            "gemini-3.8-flash".into(),
        ));
        assert!(toast_text(&app).contains("cannot be modified"));

        // 4. /providers update triggers update status
        app.builtin_update_rx = None; // clear in-flight check from test_app startup
        app.command("providers update");
        assert!(toast_text(&app).contains("checking for built-in provider updates"));
    }

    #[test]
    fn model_menu_row_formats_ctx_and_prices() {
        use super::menus::{fmt_ctx, fmt_price};

        assert_eq!(fmt_ctx(1048576), "1m");
        assert_eq!(fmt_ctx(1000000), "1m");
        assert_eq!(fmt_ctx(1050000), "1.05m");
        assert_eq!(fmt_ctx(262144), "256k");
        assert_eq!(fmt_ctx(200000), "200k");
        assert_eq!(fmt_ctx(131072), "128k");
        assert_eq!(fmt_ctx(128000), "128k");
        assert_eq!(fmt_ctx(32768), "32k");
        assert_eq!(fmt_ctx(8192), "8k");
        assert_eq!(fmt_ctx(512), "512");

        assert_eq!(fmt_price(2.0), "2");
        assert_eq!(fmt_price(0.15), "0.15");
        assert_eq!(fmt_price(1.1), "1.1");
        assert_eq!(fmt_price(0.28), "0.28");

        // row shows compact ctx and $in/$out prices
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.cfg.models.insert(
            "gpt-5.5".into(),
            ModelConfig {
                provider: "openai".into(),
                id: "gpt-5.5".into(),
                context: 1050000,
                effort: EffortLevel::High,
                effort_control: None,
                effort_always_on: false,
                price_in: Some(5.0),
                price_out: Some(30.0),
                fallback: None,
            },
        );
        app.open_menu(Menu::Models {
            provider: "openai".into(),
        });
        app.build_menu_rows();
        let texts: Vec<String> = app
            .menu_rows
            .iter()
            .map(|(line, _)| {
                line.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("1.05m") && t.contains("$5/$30")),
            "rows: {texts:?}"
        );
    }

    /// Review checklist 1–2: composer input, scrolling and selection are
    /// paint-only — an idle frame must not re-render any segment nor wrap
    /// any row.
    #[test]
    fn idle_draw_skips_transcript_pipeline() {
        use std::sync::atomic::Ordering;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::User("hello".into()));
        app.push_segment(Segment::Assistant {
            text: "world".into(),
            live: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.test_renders = 0;
        crate::tui::markdown::WRAP_TAGGED_CALLS.swap(0, Ordering::SeqCst);
        app.input.insert_str("typing");
        app.scroll(4);
        app.sel = Some(Selection {
            a: CellPos { row: 0, col: 0 },
            b: CellPos { row: 1, col: 2 },
        });
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.test_renders, 0, "idle frame re-rendered segments");
        assert_eq!(
            crate::tui::markdown::WRAP_TAGGED_CALLS.load(Ordering::SeqCst),
            0,
            "idle frame wrapped rows"
        );
    }

    /// Review checklist 3: appending one segment renders exactly that
    /// segment; every previously assembled row stays byte-identical.
    #[test]
    fn append_renders_only_the_new_segment() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        for i in 0..5 {
            app.push_segment(Segment::User(format!("q{i}")));
            app.push_segment(Segment::Assistant {
                text: format!("a{i}"),
                live: false,
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let before: Vec<String> = app
            .cache_lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        app.test_renders = 0;
        app.push_segment(Segment::Status {
            text: "done".into(),
            kind: StatusKind::Info,
            expanded: false,
        });
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.test_renders, 1, "append re-rendered old segments");
        let after: Vec<String> = app
            .cache_lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(
            &after[..before.len()],
            &before[..],
            "append rewrote previously assembled rows"
        );
    }

    /// Review checklist 4 (patch 1): expanding a tool keeps its header on
    /// the same screen row instead of jumping with `follow`.
    #[test]
    fn toggle_keeps_header_on_screen_row() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        for i in 0..3 {
            app.push_segment(Segment::User(format!("q{i}")));
        }
        let tool_idx = app.segments.len();
        app.push_segment(Segment::Tool {
            name: "read".into(),
            args: "f".into(),
            ok: Some(true),
            output: "l1\nl2\nl3".into(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
        for i in 0..25 {
            app.push_segment(Segment::Assistant {
                text: format!("filler {i}"),
                live: false,
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let header = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(tool_idx))
            .expect("tool header assembled");
        app.follow = false;
        app.view_top = header - 5;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let h = app.last_chat.height;
        let screen_before = header - app.chat_top(h);
        assert_eq!(screen_before, 5);
        app.click(header);
        assert!(!app.follow, "toggle must leave follow disengaged");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let header_after = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(tool_idx))
            .expect("tool header survives expand");
        assert_eq!(
            header_after - app.chat_top(h),
            5,
            "header moved on screen after expand"
        );
        assert!(
            matches!(
                app.segments.get(tool_idx),
                Some(Segment::Tool { expanded: true, .. })
            ),
            "tool did not expand"
        );
    }

    /// Review checklist 5: a repeat draw of an unchanged subagent view
    /// clones no transcript and renders nothing; closing restores the main
    /// rows whole, again with zero renders.
    #[test]
    fn subagent_redraw_reuses_cache_and_switch_restores_whole() {
        use std::sync::atomic::Ordering;
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::User("main q".into()));
        app.push_segment(Segment::Assistant {
            text: "main a".into(),
            live: false,
        });
        app.subagent_chats.insert(
            7,
            vec![Segment::Tool {
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
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.open_subagent_view(7);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.test_renders = 0;
        crate::tui::markdown::WRAP_TAGGED_CALLS.swap(0, Ordering::SeqCst);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.test_renders, 0, "subagent redraw re-rendered rows");
        assert_eq!(
            crate::tui::markdown::WRAP_TAGGED_CALLS.load(Ordering::SeqCst),
            0,
            "subagent redraw wrapped rows"
        );
        app.close_subagent_view();
        app.test_renders = 0;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(
            app.test_renders, 0,
            "returning to main reassembled instead of restoring"
        );
        assert_eq!(app.active_subagent, None);
    }

    /// A press resolved against one layout must not fire at whatever slid
    /// under the old row number: the pressed tool is removed before release,
    /// so the surviving tool stays collapsed.
    #[test]
    fn stale_click_target_ignored() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        for name in ["first", "second"] {
            app.push_segment(Segment::Tool {
                name: name.into(),
                args: String::new(),
                ok: Some(true),
                output: "out".into(),
                diff: None,
                preview: Vec::new(),
                preview_total: 0,
                expanded: false,
                flash: None,
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let abs0 = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(0))
            .expect("first tool header assembled");
        let row = app.last_chat.y + (abs0 - app.chat_top(app.last_chat.height)) as u16;
        let col = app.last_chat.x + 2;
        app.mouse_down(row, col);
        app.remove_segment(0);
        app.mouse_up(row, col);
        assert_eq!(app.segments.len(), 1);
        assert!(
            matches!(
                app.segments.first(),
                Some(Segment::Tool {
                    expanded: false,
                    flash: None,
                    ..
                })
            ),
            "stale press toggled the wrong tool"
        );
    }

    /// Multi-line selections order whole points: a bottom-up drag keeps the
    /// start's column at the start and the end's column at the end.
    #[test]
    fn selection_orders_whole_points_before_columns() {
        let rev = Selection {
            a: CellPos { row: 12, col: 3 },
            b: CellPos { row: 10, col: 20 },
        };
        let (first, second) = rev.ordered();
        assert!(first.row == 10 && first.col == 20);
        assert!(second.row == 12 && second.col == 3);
        let same_row = Selection {
            a: CellPos { row: 5, col: 9 },
            b: CellPos { row: 5, col: 2 },
        };
        let (first, second) = same_row.ordered();
        assert!(first.row == 5 && first.col == 2);
        assert!(second.row == 5 && second.col == 9);
    }

    /// Copying strips UI chrome but never real indentation: only exact known
    /// decorations go, code indent survives byte-for-byte.
    #[test]
    fn strip_row_chrome_preserves_code_indent() {
        use super::view::strip_row_chrome;
        assert_eq!(strip_row_chrome("    │ body"), "body");
        assert_eq!(strip_row_chrome("│ code"), "code");
        assert_eq!(strip_row_chrome("› hi"), "hi");
        assert_eq!(strip_row_chrome("      indented"), "      indented");
        assert_eq!(strip_row_chrome("  two spaces"), "  two spaces");
        assert_eq!(strip_row_chrome("text │"), "text");
    }

    /// Hovering an ask option highlights it; moving the mouse out of the
    /// chat rectangle clears the highlight instead of saturating onto row 0
    /// and lighting up whatever sits on top.
    #[test]
    fn hover_outside_chat_clears_ask_highlight() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        push_inline_ask(&mut app, ask_fixture());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let ask_idx = app.active_ask_seg().expect("ask must be active");
        let start = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(ask_idx))
            .expect("ask block assembled");
        // header row + question row, then option 0 (lockstep with ask_decode)
        let opt_abs = start + 2;
        let y = app.last_chat.y;
        let h = app.last_chat.height;
        app.mouse_move(y + (opt_abs - app.chat_top(h)) as u16);
        assert!(app.ask_hover.is_some(), "option hover must highlight");
        app.mouse_move(y + h + 1); // input area: below the chat rectangle
        assert!(
            app.ask_hover.is_none(),
            "leaving the chat must clear the highlight"
        );
    }

    /// Drag selection tracks pressed CONTENT, not a stale row number: rows
    /// inserted above the press point (a stream event racing the drag) shift
    /// the layout, and the selection start must follow the pressed segment.
    #[test]
    fn drag_anchor_survives_rows_inserted_above() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::User("question".into()));
        app.push_segment(Segment::Assistant {
            text: "first answer line with enough text to map columns".into(),
            live: false,
        });
        app.push_segment(Segment::Assistant {
            text: "second answer line also fairly long for column mapping".into(),
            live: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let abs_a2 = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(2))
            .expect("second answer assembled");
        let y = app.last_chat.y;
        let x = app.last_chat.x;
        app.mouse_down(
            y + (abs_a2 - app.chat_top(app.last_chat.height)) as u16,
            x + 2,
        );
        // a stream event lands above the press point before the drag continues
        app.insert_segment(0, Segment::User("new first".into()));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let new_a2 = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(3))
            .expect("second answer shifted");
        assert!(new_a2 > abs_a2, "insert must shift rows down");
        // drag within the same (shifted) row, wide column so it counts as a drag
        let h = app.last_chat.height;
        app.mouse_drag(y + (new_a2 - app.chat_top(h)) as u16, x + 40);
        let sel = app.sel.expect("drag must select");
        let (first, _) = sel.ordered();
        assert_eq!(
            first.row, new_a2,
            "selection start must track the pressed content"
        );
    }

    /// Toggling a tool whose output carries terminal controls (tabs from
    /// `read`'s `{:>6}\tline` rows, `\r` progress bars) must never leak
    /// them into terminal cells: raw controls desync ratatui's cursor model
    /// and spray neighbor-row fragments in both toggle directions.
    #[test]
    fn toggled_tool_with_tabbed_output_leaves_no_control_cells() {
        fn assert_clean_buffer(terminal: &Terminal<TestBackend>, when: &str) {
            let buffer = terminal.backend().buffer();
            for cell in buffer.content.iter() {
                assert!(
                    !cell.symbol().contains(['\t', '\r']),
                    "{when}: control cell {:?}",
                    cell.symbol()
                );
            }
        }
        fn screen_text(terminal: &Terminal<TestBackend>) -> Vec<String> {
            let buffer = terminal.backend().buffer();
            let area = buffer.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| {
                            buffer
                                .cell((x, y))
                                .map(|c| c.symbol())
                                .unwrap_or_default()
                                .to_string()
                        })
                        .collect()
                })
                .collect()
        }
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::Assistant {
            text: "Проверь файл и кратко ответь.".into(),
            live: false,
        });
        let tool_idx = app.segments.len();
        app.push_segment(Segment::Tool {
            name: "read".into(),
            args: "Cargo.toml".into(),
            ok: Some(true),
            output: "     1\t[package]\n     2\tname = \"sqwai\"\nprogress 50%\rprogress 100%"
                .into(),
            diff: None,
            preview: Vec::new(),
            preview_total: 0,
            expanded: false,
            flash: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_clean_buffer(&terminal, "collapsed");
        // expand: numbered rows keep number, tab stop, and text, in order
        let header = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(tool_idx))
            .expect("tool header assembled");
        app.click(header);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_clean_buffer(&terminal, "expanded");
        let rows = screen_text(&terminal);
        // tab stops are absolute to the row start: the 6-cell tool rail
        // pushes the content tab to column 16, i.e. four spaces here
        assert!(
            rows.iter().any(|r| r.contains("1    [package]")),
            "tab stop lost: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("progress 100%")),
            "carriage overwrite lost: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("50%")),
            "overwritten progress survived: {rows:?}"
        );
        // collapse again: the dialog keeps no tool fragments either
        let header = app
            .cache_rowseg
            .iter()
            .position(|t| *t == Some(tool_idx))
            .expect("tool header survives expand");
        app.click(header);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_clean_buffer(&terminal, "re-collapsed");
        let rows = screen_text(&terminal);
        assert!(
            !rows.iter().any(|r| r.contains("[package]")),
            "collapsed tool body visible: {rows:?}"
        );
    }

    /// A resize drag repaints but never re-renders: the width rebuild waits
    /// until no resize event arrived for the settle window, then runs once.
    #[test]
    fn resize_defers_full_rebuild_until_size_settles() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::User("q".into()));
        app.push_segment(Segment::Assistant {
            text: "a".into(),
            live: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.cache_w, 78);
        // drag in flight: repaint only, stale width kept for the retry
        app.last_resize = Some(std::time::Instant::now());
        app.test_renders = 0;
        terminal.backend_mut().resize(100, 24);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_eq!(app.test_renders, 0, "resize drag must not re-render");
        assert_eq!(app.cache_w, 78, "width rebuild must wait for settle");
        assert!(app.defer_rebuild);
        // settled: the same draw rebuilds everything once
        app.last_resize = None;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.test_renders > 0, "settled resize must rebuild");
        assert_eq!(app.cache_w, 98);
        assert!(!app.defer_rebuild);
    }

    /// Subagent summary rows belong to the call site: with a live answer
    /// slot open, two spawns must land before it (in spawn order) — never
    /// appended after the finished answer as stray `✓ subagent-N` lines.
    #[test]
    fn subagent_rows_land_at_call_site_not_after_answer() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        app.startup = false;
        app.push_segment(Segment::User("q".into()));
        app.push_segment(Segment::Assistant {
            text: String::new(),
            live: true,
        });
        app.handle_subagent_start(1, "t1".into());
        app.handle_subagent_start(2, "t2".into());
        // the answer streams into the live slot afterwards
        let live = app
            .segments
            .iter()
            .position(|s| matches!(s, Segment::Assistant { live: true, .. }))
            .expect("live slot survives");
        if let Some(Segment::Assistant { text, live }) = app.segments.get_mut(live) {
            *text = "done".to_string();
            *live = false;
        }
        app.touch_segment(live);
        let kinds: Vec<&str> = app
            .segments
            .iter()
            .map(|s| match s {
                Segment::User(_) => "user",
                Segment::Assistant { .. } => "answer",
                Segment::Subagent { .. } => "sub",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["user", "sub", "sub", "answer"]);
    }

    #[test]
    fn graph_view_navigation_and_details() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp = tempfile::tempdir().unwrap();
        app.project_root = temp.path().to_path_buf();

        use crate::agent::graph::{GraphStore, Node, NodeKind, SqliteGraphStore};
        let mut store = SqliteGraphStore::open(&app.project_root).unwrap();
        let sym_node = Node {
            stable_key: "sym:src/main.rs::run".into(),
            kind: NodeKind::Function,
            name: Some("run".into()),
            path: Some("src/main.rs".into()),
            language: Some("rust".into()),
            line_start: Some(10),
            line_end: Some(25),
            signature: Some("pub fn run()".into()),
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        let dec_node = Node {
            stable_key: "dec:journal:01956789-abcd".into(),
            kind: NodeKind::Decision,
            name: Some("01956789-abcd".into()),
            path: Some(".sqwai/journal/2026-03.jsonl".into()),
            language: None,
            line_start: None,
            line_end: None,
            signature: None,
            roles: vec![],
            properties: {
                let mut p = std::collections::BTreeMap::new();
                p.insert("author".into(), serde_json::json!("model"));
                p.insert("journal_ref".into(), serde_json::json!("01956789-abcd"));
                p.insert("text".into(), serde_json::json!("Refactored run to use async"));
                p
            },
            content_hash: None,
        };
        store.upsert_node(&sym_node).unwrap();
        store.upsert_node(&dec_node).unwrap();
        store
            .upsert_edge(&crate::agent::graph::Edge {
                from: dec_node.stable_key.clone(),
                to: sym_node.stable_key.clone(),
                kind: "about".into(),
                confidence: Some(100),
                source: Some("model".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();

        // Open graph view with sym_node
        app.open_menu(Menu::GraphView {
            focus_key: sym_node.stable_key.clone(),
            trail: Vec::new(),
            depth: 1,
            search_filter: None,
        });
        assert!(matches!(app.cur_menu(), Some(Menu::GraphView { .. })));
        app.build_menu_rows();

        // Check rows contain connected decision node
        let rendered: Vec<String> = app
            .menu_rows
            .iter()
            .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        assert!(rendered.iter().any(|r| r.contains("sym:src/main.rs::run")));
        assert!(rendered.iter().any(|r| r.contains("[dec]") && r.contains("01956789-abcd")));

        // Test depth adjustment
        app.run_action(MenuAction::GraphDepth(1));
        if let Some(Menu::GraphView { depth, .. }) = app.cur_menu() {
            assert_eq!(*depth, 2);
        } else {
            panic!("Expected GraphView");
        }

        app.run_action(MenuAction::GraphDepth(-1));
        if let Some(Menu::GraphView { depth, .. }) = app.cur_menu() {
            assert_eq!(*depth, 1);
        } else {
            panic!("Expected GraphView");
        }

        // Test GraphFocus to decision node
        app.run_action(MenuAction::GraphFocus(dec_node.stable_key.clone()));
        if let Some(Menu::GraphView { focus_key, trail, .. }) = app.cur_menu() {
            assert_eq!(focus_key, &dec_node.stable_key);
            assert_eq!(trail, &vec![sym_node.stable_key.clone()]);
        } else {
            panic!("Expected GraphView");
        }

        // Test GraphBack
        app.run_action(MenuAction::GraphBack);
        if let Some(Menu::GraphView { focus_key, trail, .. }) = app.cur_menu() {
            assert_eq!(focus_key, &sym_node.stable_key);
            assert!(trail.is_empty());
        } else {
            panic!("Expected GraphView");
        }

        // Close menu
        app.menu_back();
        assert!(app.menu_stack.is_empty());

        // Test open_graph_view helper
        app.open_graph_view();
        assert!(matches!(app.cur_menu(), Some(Menu::GraphView { .. })));
        app.menu_back();
        assert!(app.menu_stack.is_empty());
    }

    #[test]
    fn graph_view_render_wide_and_narrow() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp = tempfile::tempdir().unwrap();
        app.project_root = temp.path().to_path_buf();

        use crate::agent::graph::{GraphStore, Node, NodeKind, SqliteGraphStore};
        let mut store = SqliteGraphStore::open(&app.project_root).unwrap();
        let sym_node = Node {
            stable_key: "sym:src/main.rs::run".into(),
            kind: NodeKind::Function,
            name: Some("run".into()),
            path: Some("src/main.rs".into()),
            language: Some("rust".into()),
            line_start: Some(10),
            line_end: Some(25),
            signature: Some("pub fn run()".into()),
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        store.upsert_node(&sym_node).unwrap();

        app.open_menu(Menu::GraphView {
            focus_key: sym_node.stable_key.clone(),
            trail: Vec::new(),
            depth: 1,
            search_filter: None,
        });

        // Test narrow terminal (< 110)
        let backend_narrow = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal_narrow = ratatui::Terminal::new(backend_narrow).unwrap();
        terminal_narrow
            .draw(|f| {
                let area = f.area();
                app.draw_menu(f.buffer_mut(), area);
            })
            .unwrap();

        // Test wide terminal (>= 110)
        let backend_wide = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal_wide = ratatui::Terminal::new(backend_wide).unwrap();
        terminal_wide
            .draw(|f| {
                let area = f.area();
                app.draw_menu(f.buffer_mut(), area);
            })
            .unwrap();
    }

    #[test]
    fn test_node_badge_method_and_memory() {
        use crate::agent::graph::NodeKind;
        use crate::tui::app::menus::node_badge;

        assert_eq!(node_badge(&NodeKind::Method), "[meth]");
        assert_eq!(node_badge(&NodeKind::Memory), "[mem]");
        assert_eq!(node_badge(&NodeKind::Function), "[fn]");
        assert_eq!(node_badge(&NodeKind::Struct), "[st]");
        assert_eq!(node_badge(&NodeKind::Enum), "[e]");
    }

    #[test]
    fn test_graph_view_aggregates_duplicate_edges() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp = tempfile::tempdir().unwrap();
        app.project_root = temp.path().to_path_buf();

        use crate::agent::graph::{Edge, GraphStore, Node, NodeKind, SqliteGraphStore};
        let mut store = SqliteGraphStore::open(&app.project_root).unwrap();

        let file_mcp = Node {
            stable_key: "file:src/mcp.rs".into(),
            kind: NodeKind::File,
            name: Some("mcp.rs".into()),
            path: Some("src/mcp.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(100),
            signature: None,
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        let file_main = Node {
            stable_key: "file:src/main.rs".into(),
            kind: NodeKind::File,
            name: Some("main.rs".into()),
            path: Some("src/main.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(100),
            signature: None,
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        let meth_node = Node {
            stable_key: "sym:src/mcp.rs::impl<Registry>::fn::from_config".into(),
            kind: NodeKind::Method,
            name: Some("from_config".into()),
            path: Some("src/mcp.rs".into()),
            language: Some("rust".into()),
            line_start: Some(20),
            line_end: Some(30),
            signature: Some("pub async fn from_config(...)".into()),
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };

        store.upsert_node(&file_mcp).unwrap();
        store.upsert_node(&file_main).unwrap();
        store.upsert_node(&meth_node).unwrap();

        // Edge 1: file_mcp contains meth_node
        store
            .upsert_edge(&Edge {
                from: file_mcp.stable_key.clone(),
                to: meth_node.stable_key.clone(),
                kind: "contains".into(),
                confidence: Some(100),
                source: Some("rust-tree-sitter".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();

        // Two duplicate edges from file_main to file_mcp (e.g. from different sources or duplicate imports)
        store
            .upsert_edge(&Edge {
                from: file_main.stable_key.clone(),
                to: file_mcp.stable_key.clone(),
                kind: "imports".into(),
                confidence: Some(100),
                source: Some("source_a".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();
        store
            .upsert_edge(&Edge {
                from: file_main.stable_key.clone(),
                to: file_mcp.stable_key.clone(),
                kind: "imports".into(),
                confidence: Some(100),
                source: Some("source_b".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();

        app.open_menu(Menu::GraphView {
            focus_key: file_mcp.stable_key.clone(),
            trail: Vec::new(),
            depth: 1,
            search_filter: None,
        });
        app.build_menu_rows();

        let rendered: Vec<String> = app
            .menu_rows
            .iter()
            .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();

        // Check that meth_node has [meth] badge, NOT [m]
        assert!(rendered.iter().any(|r| r.contains("[meth] from_config")));

        // Check that the two duplicate imports edges from main.rs are aggregated with ×2
        assert!(rendered.iter().any(|r| r.contains("main.rs") && r.contains("×2")));
        // Verify main.rs only appears once in the connection list
        let main_rows: Vec<_> = rendered.iter().filter(|r| r.contains("file:src/main.rs")).collect();
        assert_eq!(main_rows.len(), 1);
    }

    #[test]
    fn test_graph_view_multi_hop_attribution() {
        let mut app = test_app("http://127.0.0.1:9/v1".into());
        let temp = tempfile::tempdir().unwrap();
        app.project_root = temp.path().to_path_buf();

        use crate::agent::graph::{Edge, GraphStore, Node, NodeKind, SqliteGraphStore};
        let mut store = SqliteGraphStore::open(&app.project_root).unwrap();

        let mcp = Node {
            stable_key: "file:src/mcp.rs".into(),
            kind: NodeKind::File,
            name: Some("mcp.rs".into()),
            path: Some("src/mcp.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(100),
            signature: None,
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        let main = Node {
            stable_key: "file:src/main.rs".into(),
            kind: NodeKind::File,
            name: Some("main.rs".into()),
            path: Some("src/main.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(100),
            signature: None,
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };
        let lock = Node {
            stable_key: "file:src/lock.rs".into(),
            kind: NodeKind::File,
            name: Some("lock.rs".into()),
            path: Some("src/lock.rs".into()),
            language: Some("rust".into()),
            line_start: Some(1),
            line_end: Some(50),
            signature: None,
            roles: vec![],
            properties: Default::default(),
            content_hash: None,
        };

        store.upsert_node(&mcp).unwrap();
        store.upsert_node(&main).unwrap();
        store.upsert_node(&lock).unwrap();

        // main -> imports -> mcp
        store
            .upsert_edge(&Edge {
                from: main.stable_key.clone(),
                to: mcp.stable_key.clone(),
                kind: "imports".into(),
                confidence: Some(100),
                source: Some("rust".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();

        // main -> imports -> lock
        store
            .upsert_edge(&Edge {
                from: main.stable_key.clone(),
                to: lock.stable_key.clone(),
                kind: "imports".into(),
                confidence: Some(100),
                source: Some("rust".into()),
                source_hash: None,
                limitations: vec![],
                properties: Default::default(),
            })
            .unwrap();

        // Focus on mcp with depth = 2
        app.open_menu(Menu::GraphView {
            focus_key: mcp.stable_key.clone(),
            trail: Vec::new(),
            depth: 2,
            search_filter: None,
        });
        app.build_menu_rows();

        let rendered: Vec<String> = app
            .menu_rows
            .iter()
            .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();

        // main.rs should be direct (depth 1)
        assert!(rendered.iter().any(|r| r.contains("<- (imports)") && r.contains("main.rs")));
        // lock.rs should be 2nd-hop (+2) via main.rs, NOT rendered as incoming edge to mcp
        assert!(rendered.iter().any(|r| r.contains("+2") && r.contains("lock.rs") && r.contains("via main.rs")));
        // main.rs should NOT be duplicated as a fake incoming edge
        let main_rows: Vec<_> = rendered.iter().filter(|r| r.contains("file:src/main.rs")).collect();
        assert_eq!(main_rows.len(), 1);
    }
}
