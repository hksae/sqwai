#![allow(unused_imports)]
use super::menus::{Menu, MenuAction};
use super::view::{GROUP_BASE, blank};
use super::*;


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

/// Transient notices never become chat segments anymore — they toast
/// for 3s at the bottom bar. Tests assert on the toast text.
fn toast_text(app: &App) -> String {
    app.toast
        .as_ref()
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

mod activity;
mod forms;
mod interaction;
mod plans;
mod sessions;
mod settings;
mod widgets;

