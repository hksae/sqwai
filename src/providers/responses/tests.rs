use super::*;
use crate::providers::Message;

/// Serves one canned SSE body and closes. Named events, unlike the Chat
/// Completions stream, so the helper writes `event:` lines verbatim.
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
    let p = ResponsesProvider::new(&ResolvedProvider {
        name: "p".into(),
        format: crate::config::WireFormat::Responses,
        base_url: url,
        api_key: Some("k".into()),
    })
    .unwrap();
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "go")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![crate::providers::ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({"type": "object"}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    p.stream_chat(req)
        .filter_map(|e| async move { e.ok() })
        .collect()
        .await
}

fn request_at(level: EffortLevel, control: crate::config::EffortControl) -> ChatRequest {
    ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: Some(level),
        effort_support: crate::config::EffortSupport {
            control,
            always_on: false,
        },
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    }
}

/// Named levels go on the wire literally (transparent slider).
#[test]
fn effort_reaches_the_body_at_the_declared_level() {
    use crate::config::EffortControl;
    let b = build_body(&request_at(EffortLevel::Max, EffortControl::Named));
    assert_eq!(b["reasoning"]["effort"], "max");
    // without summaries asked for, the wire carries no reasoning text
    assert_eq!(b["reasoning"]["summary"], "auto");
    let b = build_body(&request_at(EffortLevel::Xhigh, EffortControl::Named));
    assert_eq!(b["reasoning"]["effort"], "xhigh");
    let b = build_body(&request_at(EffortLevel::Low, EffortControl::Named));
    assert_eq!(b["reasoning"]["effort"], "low");
    // a model with no reasoning control gets no parameter at all
    let b = build_body(&request_at(EffortLevel::High, EffortControl::None));
    assert!(b.get("reasoning").is_none(), "unexpected reasoning: {b}");
    let b = build_body(&request_at(EffortLevel::Off, EffortControl::Named));
    assert!(b.get("reasoning").is_none());
}

#[test]
fn body_roles_and_reasoning() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![crate::providers::SystemPart::cached("s")],
        messages: vec![Message::new(Role::User, "hi")],
        effort: Some(EffortLevel::Medium),
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req);
    assert_eq!(b["model"], "gpt-x");
    assert_eq!(b["reasoning"]["effort"], "medium");
    assert_eq!(b["input"][0]["role"], "system");
    assert_eq!(b["input"][1]["role"], "user");
    // Content is a plain string now. The structured form this used to
    // build tagged *every* role with `input_text`, which is only correct
    // for input roles — an assistant item takes `output_text`, so the
    // moment assistant turns had to be replayed for a tool loop the old
    // shape was wrong. A string is documented for every role and has no
    // such trap.
    assert_eq!(b["input"][0]["content"], "s");
    assert_eq!(b["input"][1]["content"], "hi");
}

/// Volatile system parts travel last as a marked user entry, never
/// glued into the leading system block.
/// The host block trails the history as a `system` item, one slot before the
/// newest user turn — the model must answer the human, not the state block
///. Inside a tool loop it stays last, because a host item must not
/// be wedged between a `function_call` and its `function_call_output`.
#[test]
fn host_block_trails_the_history_as_system() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![
            crate::providers::SystemPart::cached("s"),
            crate::providers::SystemPart::volatile("v"),
        ],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["role"], "system");
    assert_eq!(input[0]["content"], "s");
    assert_eq!(input[1]["role"], "system");
    assert!(
        input[1]["content"].as_str().unwrap().contains('v'),
        "the host block carries the volatile part: {}",
        input[1]["content"]
    );
    assert_eq!(input[2]["role"], "user");
    assert_eq!(input[2]["content"], "hi");
    assert!(
        !input.iter().any(
            |item| item["role"] == "user" && item["content"].to_string().contains("Host state")
        ),
        "no host block may wear the user role: {b}"
    );
}

#[test]
fn host_block_stays_last_behind_tool_results() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![
            crate::providers::SystemPart::cached("s"),
            crate::providers::SystemPart::volatile("v"),
        ],
        messages: vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq::new("c1", "git_diff", serde_json::json!({})),
            ]),
            Message::tool_result("c1", "diff --git a/x", false),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let input = build_body(&req)["input"].as_array().unwrap().clone();
    // the host block stays anchored before the turn's user message, and the
    // tool call and its output stay adjacent at the very end
    assert_eq!(input[1]["role"], "system");
    assert_eq!(input[2]["role"], "user");
    let at = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(input[at - 1]["type"], "function_call");
    assert_eq!(input.len(), at + 1);
}

/// The round trip that #47 was about: a call the model made and the result
/// we send back have to be two items joined by `call_id`. As user turns
/// (what this did before) the model has nothing to match a result against.
#[test]
fn a_tool_call_and_its_result_are_joined_by_call_id() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::User, "read the file"),
            Message::new(Role::Assistant, "on it").with_tool_calls(vec![
                crate::providers::ToolCallReq::new("call_1", "read", json!({"file_path": "a.rs"})),
            ]),
            Message::tool_result("call_1", "fn main() {}", false),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![crate::providers::ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req);

    // tools are flat here, not nested under `function` as in Chat Completions
    assert_eq!(b["tools"][0]["type"], "function");
    assert_eq!(b["tools"][0]["name"], "read");
    assert!(b["tools"][0].get("parameters").is_some());
    assert_eq!(b["tools"][0]["strict"], false);

    let input = b["input"].as_array().unwrap();
    assert_eq!(
        input.len(),
        4,
        "user, assistant text, call, output: {input:#?}"
    );
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[2]["name"], "read");
    // arguments are a JSON string on the wire, not an object
    assert_eq!(input[2]["arguments"], "{\"file_path\":\"a.rs\"}");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
    assert_eq!(input[3]["output"], "fn main() {}");
}

/// An assistant turn that is only a tool call must not produce an empty
/// message item: a blank assistant message is a wasted item at best and
/// rejected at worst.
#[test]
fn a_silent_tool_call_produces_no_empty_message() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![Message::new(Role::Assistant, "").with_tool_calls(vec![
            crate::providers::ToolCallReq::new("c1", "ls", json!({})),
        ])],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let input = build_body(&req)["input"].as_array().unwrap().clone();
    assert_eq!(input.len(), 1, "{input:#?}");
    assert_eq!(input[0]["type"], "function_call");
    // and a request without tools carries no `tools` key at all
    assert!(build_body(&req).get("tools").is_none());
}

/// Assembling a call from the three events the API sends for it. The item
/// id and the `call_id` are different values, and it is the `call_id` the
/// result has to quote — mixing them up breaks the loop on the next turn.
#[tokio::test]
async fn a_streamed_function_call_is_assembled_from_its_events() {
    let body = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_9\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"item_id\":\"fc_1\",\"delta\":\"{\\\"file_path\\\":\"}\n\n",
        "event: response.function_call_arguments.delta\n",
        "data: {\"item_id\":\"fc_1\",\"delta\":\"\\\"src/main.rs\\\"}\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_9\",\"name\":\"read\",\"arguments\":\"{\\\"file_path\\\":\\\"src/main.rs\\\"}\"}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
    )
    .to_string();
    let (url, h) = sse_server(body);
    let events = collect(url).await;
    h.join().unwrap();

    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "exactly one call: {events:?}");
    assert_eq!(calls[0].name, "read");
    assert_eq!(
        calls[0].id, "call_9",
        "the result must quote call_id, not the item id"
    );
    assert_eq!(calls[0].args["file_path"], "src/main.rs");
}

/// A server that closes the response without an `output_item.done` must
/// not swallow the call it already announced.
#[tokio::test]
async fn a_call_left_open_at_completion_is_still_emitted() {
    let body = concat!(
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"function_call\",\"id\":\"fc_2\",\"call_id\":\"call_7\",\"name\":\"ls\",\"arguments\":\"\"}}\n\n",
        "event: response.function_call_arguments.done\n",
        "data: {\"item_id\":\"fc_2\",\"arguments\":\"{}\"}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"id\":\"resp_2\"}}\n\n",
    )
    .to_string();
    let (url, h) = sse_server(body);
    let events = collect(url).await;
    h.join().unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1, "{events:?}");
    assert_eq!(calls[0].id, "call_7");
    assert_eq!(calls[0].name, "ls");
}

#[test]
fn previous_response_id_only_reaches_the_wire_when_documented() {
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: Some("resp_1".into()),
        context_transport: crate::providers::ContextTransport::PreviousResponse,
    };
    assert_eq!(build_body(&req)["previous_response_id"], "resp_1");

    // a provider that does not document the field strips it in sanitize()
    let mut stripped = req.clone();
    stripped.previous_response_id = None;
    stripped.context_transport = crate::providers::ContextTransport::Stateless;
    assert!(build_body(&stripped).get("previous_response_id").is_none());
}

#[test]
fn opencode_gateway_is_stateless_no_continuation_offered() {
    use super::Provider;
    let p = ResponsesProvider::new(&ResolvedProvider {
        name: "go".into(),
        format: crate::config::WireFormat::Responses,
        base_url: "https://opencode.ai/zen/go/v1".into(),
        api_key: None,
    })
    .unwrap();
    assert!(
        !p.capabilities().previous_response,
        "Console Go retains nothing server-side"
    );
    // ...so sanitize drops the id and the loop resends everything
    let mut req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: Some("resp_old".into()),
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    p.sanitize(&mut req);
    assert!(req.previous_response_id.is_none());
    assert!(build_body(&req).get("previous_response_id").is_none());
}

#[test]
fn requested_max_tokens_mapped_to_max_output_tokens() {
    let req = ChatRequest {
        model_id: "gpt-4.5".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: Some(4096),
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let body = build_body(&req);
    assert_eq!(body["max_output_tokens"], 4096);
}

#[tokio::test]
async fn response_failed_event_yields_err() {
    use futures::StreamExt;
    let body = concat!(
        "event: response.failed\n",
        "data: {\"response\":{\"error\":{\"message\":\"server overloaded\"}}}\n\n",
    )
    .to_string();
    let (url, h) = sse_server(body);
    let p = ResponsesProvider::new(&ResolvedProvider {
        name: "p".into(),
        format: crate::config::WireFormat::Responses,
        base_url: url,
        api_key: Some("k".into()),
    })
    .unwrap();
    let req = ChatRequest {
        model_id: "gpt-x".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "go")],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let mut stream = p.stream_chat(req);
    let first = stream.next().await;
    h.join().unwrap();
    assert!(first.is_some());
    let err = first.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("server overloaded"), "{err:#}");
}

/// Mid-stream failures classify like finished responses: a quota event
/// must fail fast (never retried), a transient one stays retryable so
/// the turn loop and the fallback chain decide instead of substring
/// matching. Pre-fix both arrived class-less and retried for an hour.
#[tokio::test]
async fn response_failed_event_carries_its_class() {
    use futures::StreamExt;
    async fn first_err(payload: &str) -> anyhow::Error {
        let body = format!("event: response.failed\ndata: {payload}\n\n");
        let (url, h) = sse_server(body);
        let p = ResponsesProvider::new(&ResolvedProvider {
            name: "p".into(),
            format: crate::config::WireFormat::Responses,
            base_url: url,
            api_key: Some("k".into()),
        })
        .unwrap();
        let req = ChatRequest {
            model_id: "gpt-x".into(),
            system: vec![],
            messages: vec![Message::new(Role::User, "go")],
            effort: None,
            effort_support: Default::default(),
            max_tokens: None,
            tools: vec![],
            previous_response_id: None,
            context_transport: crate::providers::ContextTransport::Stateless,
        };
        let mut stream = p.stream_chat(req);
        let first = stream.next().await;
        h.join().unwrap();
        first.unwrap().unwrap_err()
    }

    let quota =
        first_err(r#"{"response":{"error":{"message":"insufficient_quota: out of credit"}}}"#)
            .await;
    assert_eq!(
        crate::providers::class_of(&quota),
        Some(crate::providers::ErrorClass::Quota)
    );
    assert!(!crate::providers::class_of(&quota).unwrap().retryable());

    let overload =
        first_err(r#"{"response":{"error":{"message":"server overloaded, try again"}}}"#).await;
    assert_eq!(
        crate::providers::class_of(&overload),
        Some(crate::providers::ErrorClass::Server)
    );
    assert!(crate::providers::class_of(&overload).unwrap().retryable());
}

/// Reasoning items (with encrypted_content) and phase are captured from
/// output_item.done and emitted as ProviderState at the end of the turn.
#[tokio::test]
async fn streamed_reasoning_items_and_phase_are_captured_in_provider_state() {
    let body = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[]}}\n\n",
        "event: response.reasoning_text.delta\n",
        "data: {\"item_id\":\"rs_1\",\"delta\":\"thinking\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking\"}],\"encrypted_content\":\"ENC_SECRET_BLOB\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[]}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"item_id\":\"msg_1\",\"delta\":\"I am looking into it\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"phase\":\"commentary\",\"content\":[{\"type\":\"output_text\",\"text\":\"I am looking into it\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
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

    let reasoning_items = state["reasoning_items"]
        .as_array()
        .expect("reasoning items present");
    assert_eq!(reasoning_items.len(), 1);
    assert_eq!(reasoning_items[0]["type"], "reasoning");
    assert_eq!(reasoning_items[0]["id"], "rs_1");
    assert_eq!(reasoning_items[0]["encrypted_content"], "ENC_SECRET_BLOB");
    assert_eq!(state["phase"], "commentary");
}

/// Replaying reasoning items ahead of the assistant message in order, and
/// preserving phase on the assistant message.
#[test]
fn reasoning_items_and_phase_are_replayed_ahead_of_assistant_message() {
    let req = ChatRequest {
        model_id: "gpt-5.3-codex".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::User, "run check"),
            Message::new(Role::Assistant, "checking now")
                .with_provider_state(Some(json!({
                    "reasoning_items": [
                        {
                            "type": "reasoning",
                            "id": "rs_42",
                            "encrypted_content": "ENCRYPTED_OPAQUE_DATA",
                            "summary": [{"type": "summary_text", "text": "let me verify"}]
                        }
                    ],
                    "phase": "commentary"
                })))
                .with_tool_calls(vec![crate::providers::ToolCallReq::new(
                    "c1",
                    "check",
                    json!({}),
                )]),
            Message::tool_result("c1", "all ok", false),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![crate::providers::ToolSpec {
            name: "check".into(),
            description: "run checks".into(),
            parameters: json!({"type": "object"}),
        }],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req);
    let input = b["input"].as_array().unwrap();
    assert_eq!(
        input.len(),
        5,
        "user, reasoning, assistant with phase, call, output: {input:#?}"
    );

    // input[0]: user
    assert_eq!(input[0]["role"], "user");

    // input[1]: reasoning item replayed verbatim ahead of assistant
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["id"], "rs_42");
    assert_eq!(input[1]["encrypted_content"], "ENCRYPTED_OPAQUE_DATA");

    // input[2]: assistant message with phase
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"], "checking now");
    assert_eq!(input[2]["phase"], "commentary");

    // input[3]: function_call
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[3]["call_id"], "c1");

    // input[4]: function_call_output
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "c1");
}

/// Under previous_response_id continuation, reasoning items must not be
/// replayed because the provider already holds them.
#[test]
fn reasoning_items_are_not_replayed_under_previous_response_id_continuation() {
    let req = ChatRequest {
        model_id: "gpt-5.3-codex".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::Assistant, "done").with_provider_state(Some(json!({
                "reasoning_items": [
                    {
                        "type": "reasoning",
                        "id": "rs_100",
                        "encrypted_content": "OPAQUE"
                    }
                ],
                "phase": "final_answer"
            }))),
            Message::new(Role::User, "next question"),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: Some("resp_999".into()),
        context_transport: crate::providers::ContextTransport::PreviousResponse,
    };
    let b = build_body(&req);
    let input = b["input"].as_array().unwrap();
    assert!(
        !input
            .iter()
            .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning"))
    );
}

/// A stateless request must travel alone: when the loop resends the
/// full transcript (answering tool calls, post-compaction) the request
/// still carries the loop's remembered id, and the wire used to send
/// both — doubling the context, dropping the reasoning items the
/// provider was never given, and 400ing on a stale id that a clean
/// stateless request would never mention.
#[test]
fn stateless_request_sends_no_continuation_id_and_keeps_reasoning() {
    let req = ChatRequest {
        model_id: "gpt-5.3-codex".into(),
        system: vec![],
        messages: vec![
            Message::new(Role::Assistant, "done").with_provider_state(Some(json!({
                "reasoning_items": [
                    {
                        "type": "reasoning",
                        "id": "rs_100",
                        "encrypted_content": "OPAQUE"
                    }
                ],
                "phase": "final_answer"
            }))),
            Message::new(Role::User, "next question"),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: Some("resp_stale".into()),
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let b = build_body(&req);
    assert!(
        b.get("previous_response_id").is_none(),
        "stateless wire must not name a continuation: {b}"
    );
    let input = b["input"].as_array().unwrap();
    assert!(
        input
            .iter()
            .any(|item| item.get("type").and_then(|t| t.as_str()) == Some("reasoning")),
        "stateless wire must resend the reasoning the provider never got: {b}"
    );
}

/// Serializing a message without provider_state omits the field, and
/// legacy JSON without provider_state deserializes with None.
#[test]
fn message_without_provider_state_omits_field_and_deserializes() {
    let m = Message::new(Role::Assistant, "hello");
    let serialized = serde_json::to_string(&m).unwrap();
    assert!(
        !serialized.contains("provider_state"),
        "none field must be omitted: {serialized}"
    );

    let parsed: Message =
        serde_json::from_str(r#"{"role":"assistant","content":"hello"}"#).unwrap();
    assert!(parsed.provider_state.is_none());

    let with_state = m.with_provider_state(Some(json!({"phase": "final_answer"})));
    let serialized_state = serde_json::to_string(&with_state).unwrap();
    assert!(serialized_state.contains("provider_state"));
    let parsed_state: Message = serde_json::from_str(&serialized_state).unwrap();
    assert_eq!(
        parsed_state.provider_state.unwrap()["phase"],
        "final_answer"
    );
}

#[test]
fn images_survive_serde_and_old_messages_stay_readable() {
    let image = crate::providers::ImageAttachment {
        id: "a".repeat(64),
        name: "pic.png".into(),
        media_type: "image/png".into(),
        width: 10,
        height: 20,
        bytes: 3,
        marker: Some(2),
    };
    let message = Message::tool_result("c", "x", false).with_images(vec![image.clone()]);
    let mut value = serde_json::to_value(&message).unwrap();
    assert!(value.get("images").is_some());
    let back: Message = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(back.images, vec![image]);
    assert_eq!(back.images[0].marker, Some(2));
    value.as_object_mut().unwrap().remove("images");
    let old: Message = serde_json::from_value(value).unwrap();
    assert!(old.images.is_empty());
}

#[test]
fn user_images_interleave_at_their_markers() {
    let mut image = crate::media::test_image(48, 48, "pic.png");
    image.marker = Some(1);
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(crate::providers::Role::User, "before [image 1] after")
                .with_images(vec![image]),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let body = build_body(&req);
    let input = body["input"].as_array().unwrap();
    let user = input
        .iter()
        .find(|item| item["role"] == "user")
        .expect("user turn");
    let parts = user["content"].as_array().expect("user images use parts");
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[0]["text"], "before [image 1]");
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(parts[2]["type"], "input_text");
    assert_eq!(parts[2]["text"], " after");
}

#[test]
fn tool_images_use_the_function_call_output_array() {
    let image = crate::media::test_image(48, 48, "pic.png");
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![
            Message::new(crate::providers::Role::Assistant, "").with_tool_calls(vec![
                crate::providers::ToolCallReq::new("c1", "view_image", json!({"path": "pic.png"})),
            ]),
            Message::tool_result("c1", "48×48 · image/png · 1 KB · ~10 tokens", false)
                .with_images(vec![image]),
        ],
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let body = build_body(&req);
    let input = body["input"].as_array().unwrap();
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_array()
        .expect("image results use the array form");
    assert_eq!(output[0]["type"], "input_text");
    assert_eq!(output[1]["type"], "input_image");
    assert!(
        output[1]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert_eq!(output[1]["detail"], "auto");
}
