use super::loop_compact::{
    CompactionPrefix, MIN_ZERO_TURNS_BEFORE_REPORTING, compact_history, compaction_request,
    effort_ignored_reason, record_compaction, rejects_effort_parameter, turn_shows_no_reasoning,
};
use super::loop_subagent::{SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step};
use super::loop_turn::{TurnOutcome, run_turn};
use super::*;
use crate::providers::StreamEvent;

/// The prose match that replaces the one in the error classifier. It has
/// to catch the shapes gateways actually use, and — more importantly — not
/// fire on unrelated failures, since a false positive silently drops the
/// user's effort level for the rest of the session.
#[test]
fn a_rejected_effort_parameter_is_recognised_across_gateways() {
    for err in [
        "provider returned 400: Function tools with reasoning_effort are not supported",
        "provider returned 400: {\"error\":{\"message\":\"Unrecognized request argument \
         supplied: reasoning_effort\"}}",
        "provider returned 400: unsupported parameter: 'reasoning.effort'",
        "provider returned 400: thinking is not supported for this model",
        "provider returned 400: invalid value for budget_tokens",
    ] {
        assert!(rejects_effort_parameter(err), "should have matched: {err}");
    }
}

/// The behaviour the string match exists for: one retry without the
/// parameter, and a signal so the session stops sending it. Before this,
/// the same 400 ended the turn with "provider returned 400".
#[tokio::test]
async fn a_rejected_parameter_is_retried_once_without_it() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let server = std::thread::spawn(move || {
        for (n, stream) in listener.incoming().take(2).enumerate() {
            let stream = stream.unwrap();
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
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok();
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());

            let mut out = stream;
            if n == 0 {
                let body = "{\"error\":{\"message\":\"Unrecognized request argument \
                            supplied: reasoning_effort\"}}";
                write!(
                    out,
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                            data: [DONE]\n\n";
                write!(
                    out,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            let _ = out.flush();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    let provider = crate::providers::create(&crate::config::ResolvedProvider {
        name: "p".into(),
        format: crate::config::WireFormat::Openai,
        base_url: format!("http://{addr}/v1"),
        api_key: Some("k".into()),
    })
    .unwrap();
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "hi")],
        effort: Some(crate::config::EffortLevel::High),
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let (_ctl_tx, mut ctl) = mpsc::channel(4);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let mut response_id = None;
    let mut prompt_size = 0u64;
    let outcome = run_turn(
        &provider,
        &req,
        &tx,
        &mut ctl,
        &mut response_id,
        &mut prompt_size,
        false,
    )
    .await
    .unwrap_or_else(|e| {
        panic!(
            "the retry without the parameter must succeed: {}",
            e.message
        )
    });
    drop(tx);
    drain.await.unwrap();
    server.join().unwrap();

    assert_eq!(outcome.text, "ok");
    assert!(
        outcome.effort_rejected,
        "the caller has to learn that the parameter was refused"
    );
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "exactly one retry: {bodies:?}");
    assert!(
        bodies[0].contains("reasoning_effort"),
        "first attempt carried the parameter: {}",
        bodies[0]
    );
    assert!(
        !bodies[1].contains("reasoning_effort"),
        "the retry must drop it: {}",
        bodies[1]
    );
}

fn turn(reasoning_tokens: Option<u64>, effort_rejected: bool) -> TurnOutcome {
    TurnOutcome {
        text: String::new(),
        calls: Vec::new(),
        retries: 0,
        reasoning_tokens,
        saw_reasoning: false,
        reasoning_text: String::new(),
        reasoning_ms: 0,
        effort_rejected,
        provider_state: None,
    }
}

/// Silence is not evidence: a provider that reports no reasoning counter
/// at all (Anthropic) must never be read as "it ignored you".
#[test]
fn a_missing_or_positive_counter_is_never_evidence() {
    for tokens in [None, Some(1), Some(4096)] {
        assert!(!turn_shows_no_reasoning(&turn(tokens, false)), "{tokens:?}");
        assert_eq!(effort_ignored_reason(&turn(tokens, false), 9), None);
    }
    assert!(turn_shows_no_reasoning(&turn(Some(0), false)));
}

/// One zero turn says nothing: a reasoning model may spend nothing on a
/// trivial question, and a relay may stub the usage details. Only a run of
/// them is worth reporting, and the wording says what was measured rather
/// than passing a verdict on the model.
#[test]
fn one_zero_turn_is_not_enough_to_report() {
    let t = turn(Some(0), false);
    assert_eq!(effort_ignored_reason(&t, 1), None);
    assert_eq!(
        effort_ignored_reason(&t, MIN_ZERO_TURNS_BEFORE_REPORTING),
        Some("no reasoning reported on 2 turns in a row".to_string())
    );
}

/// A refusal is the provider saying so out loud, so it needs no run-up and
/// keeps the stronger wording.
#[test]
fn a_refused_parameter_is_reported_at_once() {
    assert_eq!(
        effort_ignored_reason(&turn(None, true), 0),
        Some("the provider rejected the effort parameter".to_string())
    );
}

/// Field data from a third-party OpenAI-compatible gateway: the same
/// model (`gpt-5.6-terra`, which reasons) at the same level reported no
/// counter on one turn and zero on the next, 40 seconds apart. A zero that
/// arrives next to streamed reasoning content is the gateway's bookkeeping,
/// not an ignored request, and must not be reported as one.
/// The root cause of the false positive: two usage events in one turn,
/// the second a stub of zeros. Taking the last value let a real count of
/// 1088 decay to 0 and the host called the level ignored.
#[tokio::test]
async fn a_later_zero_usage_event_cannot_erase_a_real_count() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
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
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).ok();
        // first the real numbers, then the stub a gateway can append
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"4\"}}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":900,\
                    \"completion_tokens_details\":{\"reasoning_tokens\":1088}}}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":900,\
                    \"completion_tokens_details\":{\"reasoning_tokens\":0}}}\n\n\
                    data: [DONE]\n\n";
        let mut out = stream;
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        let _ = out.flush();
        std::thread::sleep(std::time::Duration::from_millis(150));
    });

    let provider = crate::providers::create(&crate::config::ResolvedProvider {
        name: "p".into(),
        format: crate::config::WireFormat::Openai,
        base_url: format!("http://{addr}/v1"),
        api_key: Some("k".into()),
    })
    .unwrap();
    let req = ChatRequest {
        model_id: "m".into(),
        system: vec![],
        messages: vec![Message::new(Role::User, "2+2")],
        effort: Some(crate::config::EffortLevel::High),
        effort_support: Default::default(),
        max_tokens: None,
        tools: vec![],
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let (_ctl_tx, mut ctl) = mpsc::channel(4);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let mut response_id = None;
    let mut prompt_size = 0u64;
    let outcome = run_turn(
        &provider,
        &req,
        &tx,
        &mut ctl,
        &mut response_id,
        &mut prompt_size,
        false,
    )
    .await
    .unwrap_or_else(|e| panic!("turn failed: {}", e.message));
    drop(tx);
    drain.await.unwrap();
    server.join().unwrap();

    assert_eq!(
        outcome.reasoning_tokens,
        Some(1088),
        "the stub event must not erase the real count"
    );
    assert!(
        !turn_shows_no_reasoning(&outcome),
        "and the turn must not count as a zero-reasoning turn"
    );
    assert_eq!(
        prompt_size, 30,
        "the same must hold for the prompt meter it sits next to"
    );
}

#[test]
fn a_zero_count_beside_streamed_reasoning_is_not_evidence() {
    let mut t = turn(Some(0), false);
    t.saw_reasoning = true;
    assert!(
        !turn_shows_no_reasoning(&t),
        "streamed reasoning content contradicts the counter"
    );
    assert_eq!(effort_ignored_reason(&t, 9), None);

    // with no reasoning content, the same zero does count towards the run
    assert!(turn_shows_no_reasoning(&turn(Some(0), false)));

    // and a refusal is evidence either way
    let mut t = turn(None, true);
    t.saw_reasoning = true;
    assert_eq!(
        effort_ignored_reason(&t, 0),
        Some("the provider rejected the effort parameter".to_string())
    );
}

#[test]
fn unrelated_failures_do_not_drop_the_effort_level() {
    for err in [
        "provider returned 401: invalid api key",
        "provider returned 400: messages must alternate",
        "provider returned 429: rate limit exceeded",
        "request failed: error sending request",
        // names the parameter, but not as the thing that failed
        "provider returned 500: internal error while streaming thinking blocks",
        "provider returned 400: prompt is too long: 300000 tokens > 200000 maximum",
    ] {
        assert!(!rejects_effort_parameter(err), "should not match: {err}");
    }
}

pub(crate) struct MockTestProvider {
    pub(crate) events: std::sync::Mutex<Vec<Vec<crate::providers::StreamResult>>>,
}

impl crate::providers::Provider for MockTestProvider {
    fn stream_chat(
        &self,
        _req: crate::providers::ChatRequest,
    ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
        use futures::StreamExt;
        let mut guard = self.events.lock().unwrap();
        let batch = if !guard.is_empty() {
            guard.remove(0)
        } else {
            vec![Err(anyhow::anyhow!(
                "provider returned 500: internal server error"
            ))]
        };
        futures::stream::iter(batch).boxed()
    }
}

#[tokio::test]
async fn test_provider_fallback_chain_switch_on_failure() {
    let primary_provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Err(anyhow::anyhow!(
            "provider returned 500: internal server error"
        ))]]),
    });

    let fallback_provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "fallback response".into(),
        ))]]),
    });

    let fallback = FallbackCandidate {
        key: "fallback-model".into(),
        model_id: "m-fallback".into(),
        provider: fallback_provider,
        effort_support: crate::config::EffortSupport::default(),
        context_limit: 10000,
    };

    let temp_dir = std::env::temp_dir().join(format!("sqwai-test-fallback-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider: primary_provider,
        model_id: "m-primary".into(),
        model_key: "primary-model".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "hello")],
        root: temp_dir.clone(),
        session_id: "test-fallback-sess".into(),
        blocked_patterns: vec![],
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        enable_tools: false,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        compact_only: false,
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![fallback],
    };

    let mut handle = spawn_agent(input);
    let mut switched = false;
    let mut got_fallback_text = false;

    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::FallbackSwitched { from, to } => {
                assert_eq!(from, "primary-model");
                assert_eq!(to, "fallback-model");
                switched = true;
            }
            AgentEvent::TextDelta(text) => {
                if text == "fallback response" {
                    got_fallback_text = true;
                }
            }
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
            _ => {}
        }
    }

    let _ = std::fs::remove_dir_all(&temp_dir);
    assert!(switched, "should have received FallbackSwitched event");
    assert!(
        got_fallback_text,
        "should have received text from fallback provider"
    );
}

/// Audit H8: a turn whose text was already streamed to the user must
/// not fall back — re-running it on another provider stacks a second
/// answer on top of the partial one.
#[tokio::test]
async fn partial_stream_failure_does_not_fall_back() {
    let primary_provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![
            Ok(crate::providers::StreamEvent::Text("partial answer".into())),
            Err(anyhow::anyhow!("request failed: connection reset by peer")),
        ]]),
    });
    let fallback_provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "fallback response".into(),
        ))]]),
    });
    let fallback = FallbackCandidate {
        key: "fallback-model".into(),
        model_id: "m-fallback".into(),
        provider: fallback_provider.clone(),
        effort_support: crate::config::EffortSupport::default(),
        context_limit: 10000,
    };
    let temp_dir = std::env::temp_dir().join(format!("sqwai-test-partial-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider: primary_provider,
        model_id: "m-primary".into(),
        model_key: "primary-model".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "hello")],
        root: temp_dir.clone(),
        session_id: "test-partial-sess".into(),
        blocked_patterns: vec![],
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        enable_tools: false,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        compact_only: false,
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![fallback],
    };

    let mut handle = spawn_agent(input);
    let mut switched = false;
    let mut completed_with_error = false;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::FallbackSwitched { .. } => switched = true,
            AgentEvent::Completed(Err(e)) => {
                assert!(
                    e.contains("partial answer kept"),
                    "the kept-partial reason must surface: {e}"
                );
                completed_with_error = true;
                break;
            }
            AgentEvent::Completed(Ok(_)) => panic!("the turn must not succeed"),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&temp_dir);
    assert!(completed_with_error, "loop must end with the partial error");
    assert!(!switched, "a partial turn must not fall back");
    assert_eq!(
        fallback_provider.events.lock().unwrap().len(),
        1,
        "the fallback provider must stay unconsumed"
    );
}

fn big(text: &str) -> String {
    text.repeat(600)
}

fn three_turns() -> Vec<Message> {
    vec![
        Message::new(Role::User, big("first task ")),
        Message::new(Role::Assistant, big("did first ")),
        Message::new(Role::User, big("second task ")),
        Message::new(Role::Assistant, "reading").with_tool_calls(vec![
            crate::providers::ToolCallReq::new(
                "c1",
                "read",
                serde_json::json!({"file_path": "a.rs"}),
            ),
        ]),
        Message::tool_result("c1", big("file bytes "), false),
        Message::new(Role::User, big("third task ")),
        Message::new(Role::Assistant, big("doing third ")),
    ]
}

fn summary_policy() -> context::Policy {
    // limit 16k: threshold binds (0.8 × 16k = 12800 < 16k − 8k
    // reserve), the ~9900-token fixture is over, the ~6600-token kept
    // tail plus summary still fits — so stage 4 must not run
    context::Policy::with_compaction(16_000, 0.08, 2, 0.80, true)
}

/// First identical re-run: reported with a note. Second one: the bytes are
/// withheld (§18.1.8). A changed output, a different command, or a first run
/// stays untouched.
#[test]
fn repeat_verdict_notes_once_then_withholds() {
    use crate::providers::ToolCallReq;
    let bash_call = |id: &str, command: &str| {
        ToolCallReq::new(id, "bash", serde_json::json!({"command": command}))
    };
    let first = vec![
        Message::new(Role::User, "check"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![bash_call("c1", "netstat")]),
        Message::tool_result("c1", "TCP 1.2.3.4:443", false),
    ];
    let again = bash_call("c2", "netstat");
    assert!(
        matches!(
            repeat_verdict(&[], &again, "TCP 1.2.3.4:443"),
            RepeatVerdict::Fresh
        ),
        "a first run ever is fresh"
    );
    let noted = repeat_verdict(&first, &again, "TCP 1.2.3.4:443");
    assert!(
        matches!(&noted, RepeatVerdict::Note(n) if n.contains("already called bash")),
        "the first re-run must be noted, not silenced: {noted:?}"
    );
    // the note rides on the stored result; a second re-run must still match
    // the first, or the counter would restart every iteration and the loop
    // would run forever on noted bytes
    let mut noted_text = "TCP 1.2.3.4:443".to_string();
    if let RepeatVerdict::Note(note) = &noted {
        noted_text.push_str(note);
    }
    let twice = vec![
        Message::new(Role::User, "check"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![bash_call("c1", "netstat")]),
        Message::tool_result("c1", "TCP 1.2.3.4:443", false),
        Message::new(Role::Assistant, "").with_tool_calls(vec![bash_call("c2", "netstat")]),
        Message::tool_result("c2", noted_text, false),
    ];
    let third = repeat_verdict(&twice, &bash_call("c3", "netstat"), "TCP 1.2.3.4:443");
    assert!(
        matches!(&third, RepeatVerdict::Withheld(stub) if stub.contains("not re-reported")),
        "the third look at identical bytes must not carry them again: {third:?}"
    );
    // changed output: fresh state
    assert!(matches!(
        repeat_verdict(&first, &again, "TCP 9.9.9.9:80"),
        RepeatVerdict::Fresh
    ));
    // different command: not a repeat
    assert!(matches!(
        repeat_verdict(&first, &bash_call("c3", "Get-Process"), "TCP 1.2.3.4:443"),
        RepeatVerdict::Fresh
    ));
    // a bash re-run with a different timeout is still the same attempt
    let with_timeout = ToolCallReq::new(
        "c4",
        "bash",
        serde_json::json!({"command": "netstat", "timeout": 30}),
    );
    assert!(matches!(
        repeat_verdict(&first, &with_timeout, "TCP 1.2.3.4:443"),
        RepeatVerdict::Note(_)
    ));
}

/// The gap a live session fell into: five byte-identical `git_diff {}`
/// calls in a row, none flagged — the guard looked only at `bash`, and a
/// call with no arguments had no key at all. An argument-less call is now
/// keyed by its name, which is exactly what `git_diff {}` repeats as.
#[test]
fn repeat_verdict_covers_argument_free_tools() {
    use crate::providers::ToolCallReq;
    let diff = |id: &str, args: serde_json::Value| ToolCallReq::new(id, "git_diff", args);
    let messages = vec![
        Message::new(Role::User, "check"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![diff("c1", serde_json::json!({}))]),
        Message::tool_result("c1", "diff --git a/src/x.rs", false),
        Message::new(Role::Assistant, "").with_tool_calls(vec![diff("c2", serde_json::json!({}))]),
        Message::tool_result("c2", "diff --git a/src/x.rs", false),
    ];
    assert!(
        matches!(
            repeat_verdict(
                &messages,
                &diff("c3", serde_json::json!({})),
                "diff --git a/src/x.rs"
            ),
            RepeatVerdict::Withheld(_)
        ),
        "the third empty-argument git_diff over the same bytes must be cut off"
    );
    // different arguments are a different question, not a repeat
    assert!(matches!(
        repeat_verdict(
            &messages,
            &diff("c4", serde_json::json!({"target": "src/y.rs"})),
            "diff --git a/src/x.rs"
        ),
        RepeatVerdict::Fresh
    ));
    // an empty result has nothing to match on
    assert!(matches!(
        repeat_verdict(&messages, &diff("c5", serde_json::json!({})), ""),
        RepeatVerdict::Fresh
    ));
    // polling and waiting are the mechanism, not a loop
    let poll = ToolCallReq::new("c6", "bash_output", serde_json::json!({"id": 3}));
    assert!(matches!(
        repeat_verdict(&messages, &poll, "diff --git a/src/x.rs"),
        RepeatVerdict::Fresh
    ));
    let nap = ToolCallReq::new("c7", "sleep", serde_json::json!({"seconds": 5}));
    assert!(matches!(
        repeat_verdict(&messages, &nap, "diff --git a/src/x.rs"),
        RepeatVerdict::Fresh
    ));
}

/// the turn, with progress events bracketing each run.
fn parent_prefix<'a>(
    system: &'a [crate::providers::SystemPart],
    tools: &'a [crate::providers::ToolSpec],
) -> CompactionPrefix<'a> {
    CompactionPrefix { system, tools }
}

/// Cache-aware layout: parent system and schemas go on the wire
/// byte-identical, the full history stays in order, the prompt is the
/// last message. That is the whole trick — everything the provider
/// already holds reads at cache price.
#[test]
fn compaction_request_reuses_the_parent_prefix() {
    let system = vec![
        crate::providers::SystemPart::cached("rules"),
        crate::providers::SystemPart::volatile("today"),
    ];
    let tools = vec![crate::providers::ToolSpec {
        name: "read".into(),
        description: "d".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let history = vec![
        Message::new(Role::User, "do it"),
        Message::new(Role::Assistant, "done"),
    ];
    let prefix = parent_prefix(&system, &tools);
    let (req, cache_aware) =
        compaction_request(Some(&prefix), &history[..1], &history, None, "", "m", false);
    assert!(cache_aware);
    assert_eq!(req.system, system, "prefix must be byte-identical");
    let req_tools: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(req_tools, vec!["read"], "schemas travel to hold the prefix");
    assert_eq!(req.messages.len(), history.len() + 1);
    for (got, want) in req.messages.iter().zip(history.iter()) {
        assert_eq!(got.role, want.role);
        assert_eq!(got.content, want.content, "history intact and in order");
    }
    let prompt = req.messages.last().unwrap();
    assert_eq!(prompt.role, Role::User);
    assert!(prompt.content.contains("Rules:"), "{}", prompt.content);
    assert!(
        !prompt.content.contains("do it"),
        "the transcript must not ride twice: {}",
        prompt.content
    );
    assert_eq!(req.effort, None, "the summarizer thinks nothing");
}

/// No prefix handed over (tests, side calls): the compact standalone
/// request — tiny system, transcript as text, no schemas.
#[test]
fn compaction_request_falls_back_to_standalone_without_a_prefix() {
    let older = vec![Message::new(Role::User, "do it")];
    let (req, cache_aware) = compaction_request(None, &older, &older, None, "", "m", false);
    assert!(!cache_aware);
    assert!(req.tools.is_empty());
    assert_eq!(req.messages.len(), 1);
    assert!(req.messages[0].content.contains("do it"));
}

/// A history with thinking blocks cannot travel as structured messages
/// on an effort-off request: the tool_use blocks would arrive without
/// their thinking and the API refuses them. Such histories compact
/// through the standalone text transcript instead.
#[test]
fn compaction_request_dodges_thinking_histories() {
    let history = vec![
        Message::new(Role::Assistant, "checking")
            .with_provider_state(Some(serde_json::json!({
                "thinking_blocks": [
                    {"type": "thinking", "thinking": "hmm", "signature": "SIG"}
                ]
            })))
            .with_tool_calls(vec![crate::providers::ToolCallReq::new(
                "c1",
                "check",
                serde_json::json!({}),
            )]),
        Message::tool_result("c1", "ok", false),
    ];
    let system = vec![crate::providers::SystemPart::cached("rules")];
    let tools = vec![];
    let prefix = parent_prefix(&system, &tools);
    let (req, cache_aware) =
        compaction_request(Some(&prefix), &history, &history, None, "", "m", false);
    assert!(!cache_aware, "thinking histories take the text path");
    assert!(req.tools.is_empty());
    assert_eq!(req.messages.len(), 1);
}

/// Live A/B of the compaction request shape: prime the cache with a
/// regular turn, then send the cache-aware summary request (parent
/// prefix + history + prompt) and the standalone one (tiny system +
/// transcript). The aware request should read the history at cache
/// price; the standalone one pays full price for the transcript.
/// Unique padding per run so older probes cannot warm this prefix.
#[tokio::test]
#[ignore = "live wire; requires configured provider"]
async fn live_compaction_request_reuses_prefix() {
    use futures::StreamExt;
    crate::providers::set_conversation_id("cache-probe-compact");
    let bench = crate::agent::bench_harness::bench_model()
        .expect("bench_model must resolve: check config + SQWAI_BENCH_MODEL");

    async fn send(
        bench: &crate::agent::bench_harness::BenchModel,
        req: ChatRequest,
    ) -> crate::providers::Usage {
        let mut wait_secs = 5u64;
        for _ in 0..6 {
            let mut usage = crate::providers::Usage::default();
            let mut rate_limited = false;
            let mut stream = bench.provider.stream_chat(req.clone());
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(StreamEvent::Text(_)) => {}
                    Ok(StreamEvent::Usage(u)) => {
                        if u.prompt_tokens > 0 {
                            usage = u;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.contains("429") || msg.contains("rate_limit") {
                            rate_limited = true;
                            break;
                        }
                        panic!("stream failed: {msg}");
                    }
                }
            }
            if !rate_limited {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                return usage;
            }
            eprintln!("  429: waiting {wait_secs}s");
            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
            wait_secs = (wait_secs * 2).min(60);
        }
        panic!("still rate-limited after retries");
    }

    fn report(turn: &str, u: &crate::providers::Usage) {
        let cached = u.cached_tokens.unwrap_or(0);
        let frac = if u.prompt_tokens > 0 {
            cached as f64 / u.prompt_tokens as f64
        } else {
            0.0
        };
        println!(
            "{turn}: prompt={} cached={} completion={} cached_frac={:.2}",
            u.prompt_tokens, cached, u.completion_tokens, frac
        );
    }

    let nonce: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let system = vec![
        crate::providers::SystemPart::cached(format!(
            "Probe rules {nonce}. {}",
            "Obey the project instructions. ".repeat(30)
        )),
        crate::providers::SystemPart::cached(format!(
            "Probe AGENTS.md {nonce}. {}",
            "Use rustfmt. ".repeat(30)
        )),
    ];
    let tools = vec![crate::providers::ToolSpec {
        name: "read".into(),
        description: "read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let history = three_turns();
    let older = &history[..4];

    // prime: a regular turn on the same prefix + history
    let mut prime_msgs = history.clone();
    prime_msgs.push(Message::new(Role::User, "go on"));
    let prime = ChatRequest {
        model_id: bench.model_id.clone(),
        system: system.clone(),
        messages: prime_msgs,
        effort: None,
        effort_support: Default::default(),
        max_tokens: Some(64),
        tools: tools.clone(),
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
    };
    // keep the prime small enough to leave room: reuse three_turns as-is
    let u = send(&bench, prime).await;
    report("prime", &u);

    // cache-aware: same prefix + history, prompt appended
    let prefix = CompactionPrefix {
        system: &system,
        tools: &tools,
    };
    let (aware, is_aware) = compaction_request(
        Some(&prefix),
        older,
        &history,
        None,
        "",
        &bench.model_id,
        false,
    );
    assert!(is_aware);
    let u = send(&bench, aware).await;
    report("aware", &u);

    // standalone: tiny system + rendered transcript
    let (alone, is_aware) =
        compaction_request(None, older, &history, None, "", &bench.model_id, false);
    assert!(!is_aware);
    let u = send(&bench, alone).await;
    report("alone", &u);
}

/// No pressure, no touch (Codex shape): a long history of large tool
/// results passes through compact_history unchanged — no silent
/// per-turn rewrite. Regression test: the old stage-1 prune cut every
/// aged-out output to a head stub and trained the model to re-read the
/// same files in circles.
#[tokio::test]
async fn compact_history_leaves_full_history_alone_without_pressure() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.5, false);
    let mut messages = Vec::new();
    for i in 0..30 {
        messages.push(Message::new(Role::User, format!("task {i}")));
        messages.push(Message::new(Role::Assistant, format!("work {i}")));
        messages.push(Message::tool_result(
            format!("c{i}"),
            "r".repeat(6_000),
            false,
        ));
    }
    assert_eq!(messages.len(), 90);
    let snapshot: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await;
    assert!(out.is_none(), "no pressure → no compaction: {out:?}");
    assert!(summary.is_none());
    let after: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
    assert_eq!(snapshot, after, "history must pass through byte-identical");
}

/// Prod-shape replication: 95 mixed messages (~46k tokens, like the
/// T1 shakedown transcript), 1M limit, 0.01 threshold, summary off.
/// compact_history must shrink and report — not silently pass through.

#[tokio::test]
async fn compact_history_trims_a_long_plain_transcript() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
    assert_eq!(policy.budget(), 10_000);
    let mut messages = Vec::new();
    for i in 0..30 {
        // user prose dominates: masking the tool results must NOT
        // resolve pressure alone, or stage 4 never fires below
        messages.push(Message::new(
            Role::User,
            format!("task {i} {}", "q".repeat(2000)),
        ));
        messages.push(Message::new(Role::Assistant, format!("work {i}")));
        messages.push(Message::tool_result(
            format!("c{i}"),
            "r".repeat(3000),
            false,
        ));
    }
    // +5 stray user messages to reach 95 like the field transcript
    for i in 0..5 {
        messages.push(Message::new(Role::User, format!("extra {i}")));
    }
    assert_eq!(messages.len(), 95);
    let before = context::estimated_tokens(&messages);
    assert!(before > 10_000, "fixture must be over budget, got {before}");
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await
    .expect("must compact a 4x-over-budget transcript");
    assert!(out.1 < out.0, "must shrink: {out:?}");
    assert!(messages.len() < 95, "messages must drop");
}

/// Incremental replication of the live loop: grow the transcript turn
/// by turn, calling compact_history before each (like run_agent does).
/// Trims must show up as drops in length — never 40 silent passes.
#[tokio::test]
async fn compact_history_fires_repeatedly_as_history_grows() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
    let mut messages = Vec::new();
    let mut summary = None;
    let mut trims = 0;
    let mut peak_len = 0;
    for i in 0..40 {
        messages.push(Message::new(
            Role::User,
            format!("task {i} {}", "q".repeat(500)),
        ));
        messages.push(Message::new(Role::Assistant, format!("work {i}")));
        messages.push(Message::tool_result(
            format!("c{i}"),
            "r".repeat(3000),
            false,
        ));
        let before_len = messages.len();
        if compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .is_some()
        {
            trims += 1;
            assert!(
                messages.len() < before_len,
                "a reported trim must drop messages"
            );
        }
        peak_len = peak_len.max(messages.len());
    }
    assert!(
        trims >= 3,
        "40 over-budget turns must trim repeatedly, got {trims}"
    );
    assert!(
        peak_len < 120,
        "history must stay bounded near the budget, peak len {peak_len}"
    );
}

/// Bench shape: ONE user prompt followed by assistant/tool cycles, summary
/// off. Regression test: the user-boundary-only cut never fired here, so
/// compactions stayed 0 while measured grew past the budget every turn.
#[tokio::test]
async fn compact_history_trims_a_single_prompt_tool_loop() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01, false);
    let mut messages = vec![Message::new(Role::User, "do the thing")];
    let mut summary = None;
    let mut trims = 0;
    let mut peak_len = 0;
    for i in 0..40 {
        // user prose per cycle: masking tool results must NOT resolve
        // pressure alone, or no trim is ever reported below
        messages.push(Message::new(
            Role::User,
            format!("task {i} {}", "q".repeat(2000)),
        ));
        messages.push(Message::new(Role::Assistant, "").with_tool_calls(vec![
            crate::providers::ToolCallReq::new(
                format!("c{i}"),
                "bash",
                serde_json::json!({"command": "ls"}),
            ),
        ]));
        messages.push(Message::tool_result(
            format!("c{i}"),
            "r".repeat(3000),
            false,
        ));
        let before_len = messages.len();
        if compact_history(
            &provider,
            "m",
            &mut messages,
            &mut summary,
            &policy,
            false,
            "",
            None,
        )
        .await
        .is_some()
        {
            trims += 1;
            assert!(
                messages.len() < before_len,
                "a reported trim must drop messages"
            );
        }
        peak_len = peak_len.max(messages.len());
    }
    assert!(
        trims >= 3,
        "40 over-budget cycles must trim repeatedly, got {trims}"
    );
    assert!(
        peak_len < 81,
        "history must stay bounded near the budget, peak len {peak_len}"
    );
}

#[test]
fn compaction_record_carries_tokens_and_summary() {
    let root = std::env::temp_dir().join(format!("sqwai-compact-rec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let journal = crate::agent::journal::Journal::open(&root, "sess").expect("open");
    record_compaction(
        &mut Some(journal),
        10_000,
        2_500,
        40,
        10,
        true,
        "did things",
    );
    // None journal: silent no-op, never panics
    record_compaction(&mut None, 1, 1, 1, 1, false, "");
    let recs = crate::agent::journal::Journal::records(&root).expect("read");
    let c = recs
        .iter()
        .find(|r| r.kind == "compaction")
        .expect("record");
    assert_eq!(c.fields["before"], 10_000);
    assert_eq!(c.fields["after"], 2_500);
    assert_eq!(c.fields["msgs_before"], 40);
    assert_eq!(c.fields["msgs_after"], 10);
    assert_eq!(c.fields["summarized"], true);
    assert_eq!(c.fields["summary"], "did things");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn capture_nudge_fires_only_on_uncovered_restrictions() {
    let c = |s: &str| vec![s.to_string()];
    // no marker → silent, whatever the plan holds
    assert!(capture_nudge("please refactor the engine", &[]).is_none());
    assert!(capture_nudge("please refactor the engine", &c("keep API stable")).is_none());
    // marker + empty constraints → nudge
    let tail = capture_nudge("don't touch storage/btree.rs", &[]).expect("must nudge");
    assert!(
        tail.contains("host note"),
        "nudge must be marked host-owned"
    );
    // marker + covering constraint (shared token "touch"/"btree") → silent
    assert!(
        capture_nudge(
            "don't touch storage/btree.rs",
            &c("do not touch `storage/btree.rs` (deprecated)")
        )
        .is_none()
    );
    // marker + unrelated constraints → nudge
    assert!(capture_nudge("don't touch storage/btree.rs", &c("keep API stable")).is_some());
    // RU markers behave the same
    assert!(capture_nudge("не трогай btree", &[]).is_some());
    assert!(capture_nudge("не трогай btree", &c("btree не трогать")).is_none());
}

fn count_summaries(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| m.content.matches("<conversation-summary>").count())
        .sum()
}

/// Stage 2 end-to-end (mock model): old turns collapse into one summary
/// message, the recent tail stays verbatim, tool pairs stay joined.
#[tokio::test]
async fn compact_history_summarizes_through_the_model() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "MOCK-SUMMARY".into(),
        ))]]),
    });
    let policy = summary_policy();
    let mut messages = three_turns();
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.0 > out.1, "must shrink: {:?}", out);
    assert!(out.2, "model summary must be reported");
    assert_eq!(summary.as_deref(), Some("MOCK-SUMMARY"));
    assert_eq!(count_summaries(&messages), 1);
    assert!(messages[0].content.contains("MOCK-SUMMARY"));
    assert!(
        messages.iter().any(|m| m.content.contains("third task")),
        "recent turns stay verbatim"
    );
}

/// A second compaction must not stack a second summary block: the old
/// one is re-summarized through `<previous-summary>`, not duplicated.
#[tokio::test]
async fn compact_history_never_stacks_two_summaries() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::Text("SECOND".into()))],
            vec![Ok(crate::providers::StreamEvent::Text("THIRD".into()))],
        ]),
    });
    let policy = summary_policy();
    let mut messages = three_turns();
    let mut summary = None;
    compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await;
    // force again: history is small now, but the old summary message
    // plus kept tail still exceed keep_turns
    compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        true,
        "",
        None,
    )
    .await;
    assert_eq!(
        count_summaries(&messages),
        1,
        "exactly one summary block may exist"
    );
}

/// A failed summary call falls back to the extractive local summary.
/// (`summarized` stays true: a summary block was prepended, just not by
/// the model — the event reports the transcript shape, not the author.)
#[tokio::test]
async fn compact_history_falls_back_to_local_summary() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = summary_policy();
    let mut messages = three_turns();
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.2);
    assert_eq!(count_summaries(&messages), 1);
    assert!(
        summary
            .as_deref()
            .unwrap_or_default()
            .contains("## Earlier conversation summary"),
        "unexpected summary: {summary:?}"
    );
}

/// A severely over-budget summary gets one retry with an explicit hard
/// limit; the retry answer wins even though the mock serves it second.
#[tokio::test]
async fn compact_history_retries_a_blown_summary_budget() {
    let big = "x".repeat(3 * crate::agent::context::SUMMARY_SHORT_MAX_CHARS);
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::Text(big))],
            vec![Ok(crate::providers::StreamEvent::Text("SHORT".into()))],
        ]),
    });
    let policy = summary_policy();
    let mut messages = three_turns();
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.2);
    assert_eq!(summary.as_deref(), Some("SHORT"));
}

/// A moderately over-budget summary (under 2× the cap) is truncated
/// without spending a retry: the mock has only one answer scripted, so
/// a retry would fall back to the local summary instead.
#[tokio::test]
async fn compact_history_truncates_without_retry_near_the_cap() {
    let over = "y".repeat(
        crate::agent::context::SUMMARY_SHORT_MAX_CHARS
            + crate::agent::context::SUMMARY_SHORT_MAX_CHARS / 2,
    );
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(over))]]),
    });
    let policy = summary_policy();
    let mut messages = three_turns();
    let mut summary = None;
    compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &policy,
        false,
        "",
        None,
    )
    .await;
    let kept = summary.expect("a summary must be stored");
    assert!(
        kept.chars().count() <= crate::agent::context::SUMMARY_SHORT_MAX_CHARS,
        "host cap applies: {} chars",
        kept.chars().count()
    );
    assert!(kept.contains("truncated to fit"), "{kept:?}");
}

/// #192: an Esc that lands between two tool calls must stop the batch.
/// The per-request cancel flag is reset before each dispatch; the reset
/// used to swallow a flag set between calls, so the next call ran
/// anyway. Whatever the schedule, the second call must never start,
/// while both calls still get a tool_result (protocol shape).
#[tokio::test]
async fn esc_between_tool_calls_stops_the_batch() {
    let provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new("c1", "git_status", serde_json::json!({})),
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new("c2", "git_status", serde_json::json!({})),
                )),
            ],
            vec![Ok(crate::providers::StreamEvent::Text(
                "should not get here".into(),
            ))],
        ]),
    });

    let temp_dir =
        std::env::temp_dir().join(format!("sqwai-test-esccancel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider: provider.clone(),
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "go")],
        root: temp_dir.clone(),
        session_id: "test-esc-sess".into(),
        blocked_patterns: vec![],
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        compact_only: false,
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    // Esc lands while the batch is dispatching (or just before it).
    handle.request_tool_cancel();
    let mut tool_starts = 0;
    let mut outcome_msgs = Vec::new();
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolStart { name, .. } if name == "git_status" => {
                tool_starts += 1;
            }
            AgentEvent::Completed(Ok(outcome)) => {
                outcome_msgs = outcome.messages;
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    assert!(
        tool_starts <= 1,
        "the second call must never run after Esc, started {tool_starts}"
    );
    let results: Vec<&str> = outcome_msgs
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.as_str())
        .collect();
    assert_eq!(
        results.len(),
        2,
        "both calls need a tool_result (protocol shape): {results:?}"
    );
    assert_eq!(
        provider.events.lock().unwrap().len(),
        1,
        "the scripted follow-up turn must stay unconsumed"
    );
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// #171: a child spawned for its parent's step joins the parent plan
/// explicitly, so its evidence attaches under session-strict
/// resolution instead of relying on the removed silent fallback.
#[tokio::test]
async fn subagent_joins_its_parent_plan() {
    let root = std::env::temp_dir().join(format!("sqwai-subjoin-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let mut plan = plan::create(
        "parent goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![plan::NewStep {
            title: "step".into(),
        }],
        20_000,
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["parent-sess".into()];
    plan::store(&root, &plan).unwrap();
    plan::apply(
        &mut plan,
        plan::Op::Start {
            id: "1".into(),
            confirm: None,
        },
        &plan::Limits::default(),
        None,
    )
    .unwrap();
    plan::store(&root, &plan).unwrap();
    let plan_id = plan.id.clone();

    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "child done".into(),
        ))]]),
    });
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "c1",
        "subagent",
        serde_json::json!({"tasks": ["do it"]}),
    );
    let outcome = run_subagent(
        &call,
        "parent-sess",
        &parent_tx,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &provider,
        "m",
        &root,
        &[],
        &[],
        false,
        10_000,
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::LspConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::MemoryConfig::default(),
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);

    let reloaded = plan::open(&root, &plan_id).unwrap();
    let members: Vec<&str> = reloaded.sessions.iter().map(String::as_str).collect();
    assert!(members.contains(&"parent-sess"), "{members:?}");
    assert!(
        members
            .iter()
            .any(|s| s.starts_with("sub-") && *s != "parent-sess"),
        "child joined explicitly: {members:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Regression: a subagent finishing its parent's step retired the step
/// globally while the parent session still held it, so the next `plan
/// start` in the same turn was refused as "one step at a time".
/// The parent re-adopts from the plan when the child returns.
#[test]
fn adopt_follows_the_plans_in_progress_step() {
    let root = std::env::temp_dir().join(format!("sqwai-adopt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    // no plan at all: idle
    assert_eq!(adopt_in_progress_step(&root, "sess"), None);
    let mut plan = plan::create(
        "goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![plan::NewStep {
            title: "step".into(),
        }],
        20_000,
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["sess".into()];
    plan::store(&root, &plan).unwrap();
    // pending, nothing in progress: idle
    assert_eq!(adopt_in_progress_step(&root, "sess"), None);
    plan::apply(
        &mut plan,
        plan::Op::Start {
            id: "1".into(),
            confirm: None,
        },
        &plan::Limits::default(),
        None,
    )
    .unwrap();
    plan::store(&root, &plan).unwrap();
    assert_eq!(adopt_in_progress_step(&root, "sess"), Some("1".to_string()));
    // the child retires the held step behind the parent's back
    plan.steps[0].status = plan::StepStatus::Done;
    plan::store(&root, &plan).unwrap();
    assert_eq!(adopt_in_progress_step(&root, "sess"), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// The parent session resolves strictly: when it joined no plan, the
/// child must not inherit the most recent *other* session's plan via
/// the old global fallback.
#[tokio::test]
async fn subagent_ignores_a_foreign_active_plan() {
    let root = std::env::temp_dir().join(format!("sqwai-subforeign-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let mut plan = plan::create(
        "foreign goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![plan::NewStep {
            title: "step".into(),
        }],
        20_000,
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["other-sess".into()];
    plan::store(&root, &plan).unwrap();
    plan::apply(
        &mut plan,
        plan::Op::Start {
            id: "1".into(),
            confirm: None,
        },
        &plan::Limits::default(),
        None,
    )
    .unwrap();
    plan::store(&root, &plan).unwrap();
    let plan_id = plan.id.clone();

    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "child done".into(),
        ))]]),
    });
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "c1",
        "subagent",
        serde_json::json!({"tasks": ["do it"]}),
    );
    let outcome = run_subagent(
        &call,
        "lonely-sess",
        &parent_tx,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &provider,
        "m",
        &root,
        &[],
        &[],
        false,
        10_000,
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::LspConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::MemoryConfig::default(),
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);

    let reloaded = plan::open(&root, &plan_id).unwrap();
    assert_eq!(
        reloaded.sessions,
        vec!["other-sess".to_string()],
        "no foreign join: {:?}",
        reloaded.sessions
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Same-turn subagent calls overlap: both ToolStarts land before the
/// A hung child must not stall the parent turn forever: the timeout
/// cancels cooperatively, waits out the grace, aborts, and reports.
#[tokio::test]
async fn hung_subagent_times_out_and_aborts() {
    struct HangingProvider;
    impl crate::providers::Provider for HangingProvider {
        fn stream_chat(
            &self,
            _req: crate::providers::ChatRequest,
        ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
            use futures::StreamExt;
            futures::stream::pending().boxed()
        }
    }

    let root = std::env::temp_dir().join(format!("sqwai-subtimeout-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let provider: SharedProvider = std::sync::Arc::new(HangingProvider);
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "c1",
        "subagent",
        serde_json::json!({"tasks": ["hang forever"]}),
    );
    let outcome = run_subagent(
        &call,
        "parent-sess",
        &parent_tx,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &provider,
        "m",
        &root,
        &[],
        &[],
        false,
        10_000,
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::LspConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::MemoryConfig::default(),
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_millis(150),
    )
    .await;
    assert!(!outcome.ok, "a hung child must fail, not hang");
    assert!(
        outcome.output.contains("timed out"),
        "unexpected outcome: {}",
        outcome.output
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Esc during a subagent wait: the parent flag stops the wait on the
/// next poll instead of sitting out the whole timeout ("cancelling…"
/// forever). A pre-set flag with a hanging child must return fast.
#[tokio::test]
async fn esc_stops_a_subagent_wait_without_the_timeout() {
    struct HangingProvider;
    impl crate::providers::Provider for HangingProvider {
        fn stream_chat(
            &self,
            _req: crate::providers::ChatRequest,
        ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
            use futures::StreamExt;
            futures::stream::pending().boxed()
        }
    }

    let root = std::env::temp_dir().join(format!("sqwai-subcancel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let provider: SharedProvider = std::sync::Arc::new(HangingProvider);
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "c1",
        "subagent",
        serde_json::json!({"tasks": ["hang forever"]}),
    );
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let started = std::time::Instant::now();
    let outcome = run_subagent(
        &call,
        "parent-sess",
        &parent_tx,
        &cancel,
        &provider,
        "m",
        &root,
        &[],
        &[],
        false,
        10_000,
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::LspConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::MemoryConfig::default(),
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(60),
    )
    .await;
    assert!(!outcome.ok, "cancelled child must fail");
    assert!(
        outcome.output.contains("cancelled by user"),
        "unexpected outcome: {}",
        outcome.output
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "cancel must not wait out the timeout"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// first ToolNotice (a sequential loop would interleave start/notice
/// pairs). Rows, journal and messages still come out in call order.
#[tokio::test]
async fn same_turn_subagent_calls_overlap() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c1",
                        "subagent",
                        serde_json::json!({"tasks": ["first"]}),
                    ),
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "c2",
                        "subagent",
                        serde_json::json!({"tasks": ["second"]}),
                    ),
                )),
            ],
            // one single-text turn per child; interchangeable
            vec![Ok(crate::providers::StreamEvent::Text(
                "child one done".into(),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text(
                "child two done".into(),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text("all done".into()))],
        ]),
    });

    let temp_dir = std::env::temp_dir().join(format!("sqwai-subbatch-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "go")],
        root: temp_dir.clone(),
        session_id: "test-subbatch-sess".into(),
        blocked_patterns: vec![],
        web_allow_hosts: vec![],
        plan_mode: false,
        context_limit: 10000,
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        lsp: Default::default(),
        compact_only: false,
        memory: Default::default(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    let mut seq: Vec<String> = Vec::new();
    let mut dones = 0;
    let mut results = Vec::new();
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolStart { call_id, .. } => seq.push(format!("start:{call_id}")),
            AgentEvent::ToolNotice { call_id, .. } => seq.push(format!("notice:{call_id}")),
            AgentEvent::SubagentDone { .. } => dones += 1,
            AgentEvent::Completed(Ok(outcome)) => {
                results = outcome
                    .messages
                    .iter()
                    .filter(|m| m.role == Role::Tool)
                    .filter_map(|m| m.tool_call_id.clone())
                    .collect();
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&temp_dir);
    assert_eq!(dones, 2, "both children finished: {seq:?}");
    assert!(
        seq.len() >= 4 && seq[0] == "start:c1" && seq[1] == "start:c2",
        "both starts precede any notice: {seq:?}"
    );
    assert_eq!(results.len(), 2, "both calls got results");
    assert!(
        results.contains(&"c1".to_string()) && results.contains(&"c2".to_string()),
        "results address both calls: {results:?}"
    );
}
