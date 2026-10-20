/// Anthropic accepts at most four `cache_control` markers in one request
/// and rejects the request beyond that.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// The Messages API expresses effort as a token budget, which is what a
/// model on this provider resolves to in `ModelConfig::effort_support`.
fn budget_support() -> crate::config::EffortSupport {
    crate::config::EffortSupport {
        control: crate::config::EffortControl::Budget,
        always_on: false,
    }
}

use super::*;
use crate::providers::Message;

/// The caller's own `max_tokens` has to reach the wire. It used to be
/// ignored: `build_body` read only its default argument, so a request for
/// a longer answer was silently capped at 8192.
#[test]
fn requested_max_tokens_is_honoured() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![],
        effort: None,
        effort_support: Default::default(),
        max_tokens: Some(32_000),
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    assert_eq!(build_body(&req, 8192, false)["max_tokens"], 32_000);

    // no request of its own: the provider default still applies
    let mut without = req.clone();
    without.max_tokens = None;
    assert_eq!(build_body(&without, 8192, false)["max_tokens"], 8192);

    // with effort on, the budget is added to what the caller asked for,
    // not to the default
    let mut with_effort = req.clone();
    with_effort.effort = Some(EffortLevel::Medium);
    with_effort.effort_support = budget_support();
    let body = build_body(&with_effort, 8192, false);
    let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert_eq!(
        body["max_tokens"].as_u64().unwrap(),
        (32_000 + budget).min(64_000)
    );
}

/// Tools and the stable system prefix carry breakpoints; volatile parts
/// travel unmarked after the history. Markers stay within budget no
/// matter how many stable parts exist, because only region ends are
/// marked — never each part.
#[test]
fn tool_schemas_carry_the_first_cache_breakpoint() {
    let tool = |name: &str| crate::providers::ToolSpec {
        name: name.into(),
        description: "d".into(),
        parameters: json!({"type": "object"}),
    };
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![
            crate::providers::SystemPart::cached("stable"),
            crate::providers::SystemPart::volatile("anchor"),
        ],
        messages: vec![],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![tool("read"), tool("write"), tool("bash")],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };

    let body = build_body(&req, 8192, true);
    let tools = body["tools"].as_array().unwrap();
    assert!(
        tools[0].get("cache_control").is_none() && tools[1].get("cache_control").is_none(),
        "only the last tool carries the marker: {tools:?}"
    );
    assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
    // stable system keeps one trailing marker; the host block rides after it,
    // unmarked, in `system` — never as a user turn
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 2);
    assert_eq!(system[0]["text"], "stable");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert!(system[1].get("cache_control").is_none());
    assert!(
        system[1]["text"].as_str().unwrap().contains("anchor"),
        "the host block carries the volatile part: {}",
        system[1]["text"]
    );
    assert!(
        body["messages"].as_array().unwrap().is_empty(),
        "the host block must not create a user turn"
    );

    // and nothing is marked for a provider without a documented cache
    let uncached = build_body(&req, 8192, false);
    assert!(
        uncached["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("cache_control").is_none())
    );
}

/// Markers stay within budget by construction: tools, end of stable
/// system, end of history — three regions, never one per part, no
/// matter how many parts exist.
#[test]
fn cache_breakpoints_stay_within_the_provider_limit() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: (0..6)
            .map(|i| crate::providers::SystemPart::cached(format!("part {i}")))
            .collect(),
        messages: vec![crate::providers::Message::new(
            crate::providers::Role::User,
            "hi",
        )],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![crate::providers::ToolSpec {
            name: "read".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let body = build_body(&req, 8192, true);
    let marked = body["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part.get("cache_control").is_some())
        .count()
        + body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool.get("cache_control").is_some())
            .count()
        + body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block.get("cache_control").is_some())
            .count();
    assert_eq!(marked, 3, "{body}");
    assert!(marked <= MAX_CACHE_BREAKPOINTS, "{body}");
}

/// Long histories earn the middle anchor: a breakpoint reaches ~20
/// blocks back, so one huge tool batch would otherwise orphan the first
/// half. Short histories stay at three markers.
#[test]
fn long_histories_carry_a_middle_anchor_marker() {
    fn count_marked(body: &serde_json::Value) -> usize {
        body["system"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part.get("cache_control").is_some())
            .count()
            + body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool.get("cache_control").is_some())
                .count()
            + body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
                .filter(|block| block.get("cache_control").is_some())
                .count()
    }
    fn history_req(n: usize) -> ChatRequest {
        ChatRequest {
            model_id: "m".into(),
            system: vec![crate::providers::SystemPart::cached("stable")],
            messages: (0..n)
                .map(|i| {
                    // alternating roles: same-role turns merge, which
                    // would collapse the history under test
                    let role = if i % 2 == 0 {
                        crate::providers::Role::User
                    } else {
                        crate::providers::Role::Assistant
                    };
                    crate::providers::Message::new(role, format!("turn {i}"))
                })
                .collect(),
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![crate::providers::ToolSpec {
                name: "read".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        }
    }
    // short history: tools + system + end, no middle anchor
    let short = build_body(&history_req(4), 8192, true);
    assert_eq!(count_marked(&short), 3, "{short}");
    // long history: the middle message carries the fourth marker
    let long = build_body(&history_req(12), 8192, true);
    assert_eq!(count_marked(&long), 4, "{long}");
    let msgs = long["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 12);
    // consecutive user turns merge (alternation), so count distinct
    // marked messages rather than positions: exactly two of them
    let marked_msgs = msgs
        .iter()
        .filter(|m| {
            m["content"]
                .as_array()
                .is_some_and(|arr| arr.iter().any(|b| b.get("cache_control").is_some()))
        })
        .count();
    assert_eq!(marked_msgs, 2, "middle anchor plus end marker: {long}");
}

#[test]
fn body_has_cache_control_and_thinking() {
    let req = ChatRequest {
        model_id: "claude-x".into(),
        system: vec![
            crate::providers::SystemPart::cached("stable prefix"),
            crate::providers::SystemPart::volatile("git: on branch main"),
        ],
        messages: vec![Message::new(Role::User, "hi")],
        effort: Some(EffortLevel::High),
        effort_support: budget_support(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 8192, true);
    assert_eq!(b["model"], "claude-x");
    // stable system keeps its trailing marker; when messages are non-empty,
    // the volatile host block sits immediately before the last user turn's
    // text block so the request ends on the user's words (or tool result)
    let system = b["system"].as_array().unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(system[0]["text"], "stable prefix");
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
    assert!(msgs[0]["content"][0].get("cache_control").is_none());
    assert!(
        msgs[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("git: on branch main")
    );
    assert_eq!(msgs[0]["content"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(msgs[0]["content"][1]["text"], "hi");
    assert_eq!(b["thinking"]["budget_tokens"], 16384);
    assert_eq!(b["max_tokens"], 8192 + 16384);
}

/// The cache bug in one assertion: a changed host block must not move a
/// single byte of the cached prefix (`tools -> system -> earlier turns`).
#[test]
fn volatile_change_keeps_the_prefix_bytes() {
    let body_for = |volatile: &str| {
        build_body(
            &ChatRequest {
                model_id: "m".into(),
                system: vec![
                    crate::providers::SystemPart::cached("stable prefix"),
                    crate::providers::SystemPart::volatile(volatile),
                ],
                messages: vec![
                    Message::new(Role::User, "turn 1"),
                    Message::new(Role::Assistant, "ans 1"),
                    Message::new(Role::User, "hi"),
                ],
                effort: None,
                effort_support: Default::default(),
                max_tokens: None,
                tools: vec![],
                previous_response_id: None,
                context_transport: crate::providers::ContextTransport::Stateless,
            },
            8192,
            true,
        )
    };
    let before = body_for("git: on branch main");
    let after = body_for("git: on branch other");
    assert_eq!(before["system"], after["system"]);
    assert_eq!(before["messages"][0], after["messages"][0]);
    assert_eq!(before["messages"][1], after["messages"][1]);
    // And messages[1] (right before the volatile block in messages[2]) carries
    // an explicit cache_control breakpoint so Anthropic actually caches the
    // clean history prefix before `host_tail`!
    assert_eq!(
        before["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_ne!(
        before["messages"][2]["content"][0],
        after["messages"][2]["content"][0]
    );
    assert_eq!(
        before["messages"][2]["content"][1],
        after["messages"][2]["content"][1]
    );
}

#[test]
fn body_without_documented_cache_has_no_breakpoints() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![crate::providers::SystemPart::cached("stable prefix")],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 1024, false);
    assert!(b["system"][0].get("cache_control").is_none());
    assert_eq!(b["system"][0]["text"], "stable prefix");
}

#[test]
fn empty_system_block_is_omitted() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 1024, true);
    assert!(b.get("system").is_none());
}

#[test]
fn body_without_thinking_omits_field() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 8192, true);
    assert!(b.get("thinking").is_none());
}

#[test]
fn body_maps_tools_and_results() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::User, "list"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
                "tu_1",
                "ls",
                json!({"path": "."}),
            )]),
            Message::tool_result("tu_1", "a.txt", false),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![super::super::ToolSpec {
            name: "ls".into(),
            description: "list dir".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 8192, true);

    assert_eq!(b["tools"][0]["name"], "ls");
    assert_eq!(b["tools"][0]["input_schema"]["type"], "object");

    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "alternation preserved");
    // assistant carries the tool_use block
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["id"], "tu_1");
    assert_eq!(msgs[1]["content"][0]["input"]["path"], ".");
    // result rides in the following user turn
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "tu_1");
    assert_eq!(msgs[2]["content"][0]["content"], "a.txt");
}

#[test]
fn consecutive_tool_results_merge_into_one_user_turn() {
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::Assistant, "").with_tool_calls(vec![
                ToolCallReq::new("a", "t", json!({})),
                ToolCallReq::new("b", "t", json!({})),
            ]),
            Message::tool_result("a", "res-a", false),
            Message::tool_result("b", "res-b", false),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req, 1024, true);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "results merged into single user turn");
    let arr = msgs[1]["content"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["tool_use_id"], "a");
    assert_eq!(arr[1]["tool_use_id"], "b");
}

#[test]
fn thinking_delta_pointer_extracts_thinking_field() {
    let v: Value = serde_json::from_str(
        r#"{"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "step by step"}}"#,
    )
    .unwrap();
    let thinking = v.pointer("/delta/thinking").and_then(|x| x.as_str());
    assert_eq!(thinking, Some("step by step"));
}

/// Serves one canned SSE body and closes. The Messages API uses named
/// events, so the helper writes `event:` lines verbatim.
fn sse_server(body: String) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        let mut len = 0usize;
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
                len = v;
            }
        }
        // the request body must be drained or the client sees a reset
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).ok();
        let mut out = stream;
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let _ = out.flush();
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    (format!("http://{addr}/v1"), h)
}

async fn collect(url: String) -> Vec<StreamEvent> {
    use futures::StreamExt;
    let p = AnthropicProvider::new(&crate::config::ResolvedProvider {
        name: "p".into(),
        format: crate::config::WireFormat::Anthropic,
        base_url: url,
        api_key: Some("k".into()),
    })
    .unwrap();
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "go")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    p.stream_chat(req)
        .filter_map(|e| async move { e.ok() })
        .collect()
        .await
}

/// Thinking text and its signature are captured from the stream and
/// emitted as provider state at the end of the turn: without both, the
/// next request with tool use is a 400.
#[tokio::test]
async fn streamed_thinking_and_signature_are_captured_as_provider_state() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me \"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"check\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG_1\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string();
    let (url, h) = sse_server(body);
    let events = collect(url).await;
    h.join().unwrap();

    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Reasoning(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        thinking, "let me check",
        "thinking still streams: {events:?}"
    );

    let state = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::ProviderState(s) => Some(s.clone()),
            _ => None,
        })
        .expect("provider state must be emitted");
    let blocks = state["thinking_blocks"].as_array().expect("blocks present");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["thinking"], "let me check");
    assert_eq!(blocks[0]["signature"], "SIG_1");
}

/// A redacted block has no text or signature to accumulate: the opaque
/// payload goes back exactly as received.
#[tokio::test]
async fn streamed_redacted_thinking_is_kept_verbatim() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"OPAQUE\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string();
    let (url, h) = sse_server(body);
    let events = collect(url).await;
    h.join().unwrap();

    let state = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::ProviderState(s) => Some(s.clone()),
            _ => None,
        })
        .expect("provider state must be emitted");
    let blocks = state["thinking_blocks"].as_array().expect("blocks present");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "redacted_thinking");
    assert_eq!(blocks[0]["data"], "OPAQUE");
}

fn thinking_request() -> ChatRequest {
    ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::User, "run check"),
            Message::new(Role::Assistant, "checking now")
                .with_provider_state(Some(json!({
                    "thinking_blocks": [
                        {"type": "thinking", "thinking": "let me check", "signature": "SIG_1"}
                    ]
                })))
                .with_tool_calls(vec![ToolCallReq::new("c1", "check", json!({}))]),
            Message::tool_result("c1", "all ok", false),
        ],
        effort: Some(EffortLevel::Medium),
        effort_support: budget_support(),
        max_tokens: None,
        tools: vec![crate::providers::ToolSpec {
            name: "check".into(),
            description: "run checks".into(),
            parameters: json!({"type": "object"}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    }
}

/// Saved thinking replays first in its own assistant turn, ahead of
/// text and tool_use: that order is what the API accepts.
#[test]
fn saved_thinking_replays_first_with_its_signature() {
    let b = build_body(&thinking_request(), 8192, true);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["role"], "assistant");
    let content = msgs[1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3, "thinking, text, tool_use: {content:#?}");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "let me check");
    assert_eq!(content[0]["signature"], "SIG_1");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[2]["type"], "tool_use");
}

/// Thinking off means the saved blocks stay home: they are not legal
/// input for a request that does not think.
#[test]
fn saved_thinking_stays_home_when_effort_is_off() {
    let mut req = thinking_request();
    req.effort = None;
    let b = build_body(&req, 8192, true);
    let msgs = b["messages"].as_array().unwrap();
    let content = msgs[1]["content"].as_array().unwrap();
    assert_eq!(
        content.len(),
        2,
        "text, tool_use, no thinking: {content:#?}"
    );
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "tool_use");
    assert!(b.get("thinking").is_none());
}

#[test]
fn mid_history_anchor_stays_stationary_on_user_turns_and_precedes_volatile_tail() {
    // 1. Inside a long tool loop (last_user at index 0, followed by alternating
    //    Assistant tool_use and Tool tool_result turns), the mid-history marker
    //    stays frozen at index 8 across lengths 10, 12, 14, 16.
    let tool_loop_req = |n: usize| {
        let mut messages = vec![Message::new(Role::User, "initial prompt")];
        for i in 1..n {
            if i % 2 == 1 {
                messages.push(
                    Message::new(Role::Assistant, format!("step {i}")).with_tool_calls(vec![
                        ToolCallReq::new(format!("c{i}"), "read", json!({})),
                    ]),
                );
            } else {
                messages.push(Message::tool_result(format!("c{}", i - 1), "ok", false));
            }
        }
        ChatRequest {
            model_id: "claude-x".into(),
            system: vec![
                crate::providers::SystemPart::cached("stable"),
                crate::providers::SystemPart::volatile("git: main"),
            ],
            messages,
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        }
    };

    for n in [10, 12, 14, 16] {
        let b = build_body(&tool_loop_req(n), 8192, true);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[8]["role"], "user");
        let marked_at_8 = msgs[8]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blk| blk.get("cache_control").is_some());
        assert!(marked_at_8, "mid anchor must stay at index 8 for len={n}");
    }

    // 2. When a new user turn arrives after prior history (`u_idx > 0`), the
    //    pre-volatile breakpoint sits at `u_idx - 1` so the clean prior history
    //    is cached before the volatile `host_tail` in `msgs[u_idx]`.
    let multi_turn = ChatRequest {
        model_id: "claude-x".into(),
        system: vec![
            crate::providers::SystemPart::cached("stable"),
            crate::providers::SystemPart::volatile("git: main"),
        ],
        messages: vec![
            Message::new(Role::User, "turn 1"),
            Message::new(Role::Assistant, "reply 1"),
            Message::new(Role::User, "turn 2"),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&multi_turn, 8192, true);
    let msgs = b["messages"].as_array().unwrap();
    assert_eq!(
        msgs[1]["content"][0]["cache_control"]["type"], "ephemeral",
        "msgs[u_idx - 1] must be marked before host_tail"
    );
    assert_eq!(
        msgs[2]["content"][1]["cache_control"]["type"], "ephemeral",
        "msgs.last() is also marked"
    );
}

#[test]
fn cache_breakpoint_skips_thinking_and_empty_text_blocks() {
    let mut blk = json!({"type": "thinking", "thinking": "deep thought", "signature": "SIG"});
    let mut empty_txt = json!({"type": "text", "text": ""});
    assert!(!super::is_cacheable_block(&blk));
    assert!(!super::is_cacheable_block(&empty_txt));
    let mut msg = json!({
        "role": "assistant",
        "content": [blk.clone(), empty_txt.clone()]
    });
    assert!(
        !super::mark_message_cache_breakpoint(&mut msg),
        "must refuse to mark thinking or empty text block"
    );
    let _ = (&mut blk, &mut empty_txt);
}

fn find_kind<'a>(value: &'a serde_json::Value, kind: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some(kind) {
                return Some(value);
            }
            map.values().find_map(|child| find_kind(child, kind))
        }
        serde_json::Value::Array(items) => items.iter().find_map(|child| find_kind(child, kind)),
        _ => None,
    }
}

#[test]
fn tool_images_use_image_blocks_inside_the_tool_result() {
    let image = crate::media::test_image(40, 30, "shot.png");
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(crate::providers::Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq::new(
                    "c1",
                    "view_image",
                    serde_json::json!({"path": "shot.png"}),
                ),
            ]),
            Message::tool_result("c1", "view_image: shot.png", false).with_images(vec![image]),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let body = build_body(&req, 8192, false);
    let result = find_kind(&body, "tool_result").expect("tool_result block");
    let content = result["content"]
        .as_array()
        .expect("images force the array form");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert!(!content[1]["source"]["data"].as_str().unwrap().is_empty());
}
