use super::loop_compact::{
    CompactionGate, CompactionOutcome, CompactionPrefix, CompactionReason, compact_history,
    compaction_request, record_compaction,
};
use super::loop_subagent::{SUBAGENT_TIMEOUT_SECS, adopt_in_progress_step};
use super::*;
use crate::providers::StreamEvent;


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
        messages: vec![
            Message::new(Role::User, "hello"),
            Message::new(Role::Assistant, "first pass").with_provider_state(Some(
                serde_json::json!({"thinking_blocks": [
                    {"type": "thinking", "thinking": "check", "signature": "SIG"}
                ]}),
            )),
            Message::new(Role::User, "again"),
        ],
        root: temp_dir.clone(),
        session_id: "test-fallback-sess".into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: false,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
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
            AgentEvent::FallbackSwitched {
                from,
                to,
                reasoning_dropped,
            } => {
                assert_eq!(from, "primary-model");
                assert_eq!(to, "fallback-model");
                assert!(
                    reasoning_dropped,
                    "the switch drops the signed reasoning and must say so"
                );
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

/// A provider that keeps the requests it was handed, for asserting what the
/// loop planned after a switch.
pub(crate) struct RecordingTestProvider {
    pub(crate) events: std::sync::Mutex<Vec<Vec<crate::providers::StreamResult>>>,
    pub(crate) requests: std::sync::Mutex<Vec<crate::providers::ChatRequest>>,
}

/// Routes by tool schema: a request that can spawn is the parent; the child
/// (without `subagent` in its tools) gets a plain report.
pub(crate) struct BackgroundRoutingProvider {
    pub(crate) parent_calls: std::sync::atomic::AtomicUsize,
    pub(crate) child_saw_steer: std::sync::atomic::AtomicBool,
}

impl crate::providers::Provider for BackgroundRoutingProvider {
    fn stream_chat(
        &self,
        req: crate::providers::ChatRequest,
    ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
        use futures::StreamExt;
        use std::sync::atomic::Ordering;
        let parent = req.tools.iter().any(|tool| tool.name == "subagent");
        let events = if parent {
            match self.parent_calls.fetch_add(1, Ordering::Relaxed) {
                0 => vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "call-bg",
                        "subagent",
                        serde_json::json!({"tasks": ["look around"], "background": true}),
                    ),
                ))],
                1 => vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "call-send",
                        "subagents",
                        serde_json::json!({"action": "send", "id": 1, "message": "focus on the lexer"}),
                    ),
                ))],
                2 => vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "call-out",
                        "subagents",
                        serde_json::json!({"action": "output", "id": 1, "wait_secs": 5}),
                    ),
                ))],
                _ => vec![Ok(crate::providers::StreamEvent::Text(
                    "parent done".into(),
                ))],
            }
        } else {
            let saw = req.messages.iter().any(|m| {
                m.role == Role::User && m.content.contains("<parent_message>")
            });
            if saw {
                self.child_saw_steer.store(true, Ordering::Relaxed);
                vec![Ok(crate::providers::StreamEvent::Text(
                    "child report after steer".into(),
                ))]
            } else {
                // keep the child looping until the parent's message lands
                vec![Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "child-read",
                        "read",
                        serde_json::json!({"file_path": "no-such-file"}),
                    ),
                ))]
            }
        };
        futures::stream::iter(events).boxed()
    }
}

impl crate::providers::Provider for RecordingTestProvider {
    fn stream_chat(
        &self,
        req: crate::providers::ChatRequest,
    ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
        use futures::StreamExt;
        self.requests.lock().unwrap().push(req);
        let batch = self
            .events
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_default();
        futures::stream::iter(batch).boxed()
    }
}

/// The fallback decides its own effort wire from its own declaration: the
/// level is the user's, the mapping is the target model's. Nothing may carry
/// the failed model's budget into a model that takes named levels (audit
/// L32) — that request is the "wasted request per switch".
#[tokio::test]
async fn fallback_replans_effort_for_the_new_model() {
    let primary_provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Err(anyhow::anyhow!(
            "provider returned 500: internal server error"
        ))]]),
    });
    let fallback_provider = std::sync::Arc::new(RecordingTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "ok".into(),
        ))]]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let budget_support = crate::config::EffortSupport {
        control: crate::config::EffortControl::Budget,
        always_on: false,
    };
    let named_support = crate::config::EffortSupport {
        control: crate::config::EffortControl::Named,
        always_on: false,
    };
    let fallback = FallbackCandidate {
        key: "fallback-model".into(),
        model_id: "m-fallback".into(),
        provider: fallback_provider.clone(),
        effort_support: named_support,
        context_limit: 10000,
    };
    let temp_dir = std::env::temp_dir().join(format!("sqwai-test-effort-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider: primary_provider,
        model_id: "m-primary".into(),
        model_key: "primary-model".into(),
        effort: Some(EffortLevel::High),
        effort_support: budget_support,
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "hello")],
        root: temp_dir.clone(),
        session_id: "test-effort-fallback".into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: false,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![fallback],
    };
    let mut handle = spawn_agent(input);
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    let requests = fallback_provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one request to the fallback");
    let req = &requests[0];
    assert_eq!(req.effort, Some(EffortLevel::High), "the user's level travels");
    assert_eq!(
        req.effort_support.control,
        crate::config::EffortControl::Named,
        "the fallback model's declaration, not the failed model's"
    );
    let wired = crate::providers::effort::plan(EffortLevel::High, req.effort_support);
    assert_eq!(
        wired.wire,
        crate::providers::effort::Wire::Level("high"),
        "the fallback's wire, not the primary's token budget"
    );
    let _ = std::fs::remove_dir_all(&temp_dir);
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
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: false,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
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
    context::Policy::with_compaction(16_000, 0.08, 2, 0.80)
}

/// First identical re-run: reported with a note. Second one: the bytes are
/// withheld. A changed output, a different command, or a first run
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
    let (req, cache_aware) = compaction_request(
        Some(&prefix),
        &history[..1],
        &history,
        1,
        None,
        "",
        None,
        "m",
        false,
    );
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

/// The cache-aware prompt names the kept tail so the model summarizes the
/// earlier part instead of restating the messages that survive compaction.
#[test]
fn compaction_request_instructs_the_model_about_the_kept_tail() {
    let system = vec![crate::providers::SystemPart::cached("rules")];
    let tools = vec![];
    let prefix = parent_prefix(&system, &tools);
    let history = vec![
        Message::new(Role::User, "do it"),
        Message::new(Role::Assistant, "done"),
    ];
    let (req, cache_aware) = compaction_request(
        Some(&prefix),
        &history[..1],
        &history,
        1,
        None,
        "Goal: fix the bug\nConstraints:\n- keep API stable",
        Some("keep the parser details"),
        "m",
        false,
    );
    assert!(cache_aware);
    let prompt = req.messages.last().unwrap().content.clone();
    assert!(prompt.contains("1 message(s) stay verbatim"), "{prompt}");
    assert!(prompt.contains("do not repeat the kept tail"), "{prompt}");
    assert!(prompt.contains("<user-compaction-focus>"), "{prompt}");
    assert!(prompt.contains("keep the parser details"), "{prompt}");
}

/// No prefix handed over (tests, side calls): the compact standalone
/// request — tiny system, transcript as text, no schemas.
#[test]
fn compaction_request_falls_back_to_standalone_without_a_prefix() {
    let older = vec![Message::new(Role::User, "do it")];
    let (req, cache_aware) =
        compaction_request(None, &older, &older, 0, None, "", None, "m", false);
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
    let (req, cache_aware) = compaction_request(
        Some(&prefix),
        &history,
        &history,
        0,
        None,
        "",
        None,
        "m",
        false,
    );
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
    let cfg = crate::config::Config::load().expect("user config must load");
    let mc = cfg
        .models
        .get(&cfg.last_model)
        .expect("last_model must exist in config");
    let resolved = cfg.resolve_provider(mc).expect("provider must resolve");
    let live_provider = crate::providers::create(&resolved).expect("provider must create");
    let live_model_id = mc.id.clone();

    async fn send(
        provider: &crate::providers::SharedProvider,
        req: ChatRequest,
    ) -> crate::providers::Usage {
        let mut wait_secs = 5u64;
        for _ in 0..6 {
            let mut usage = crate::providers::Usage::default();
            let mut rate_limited = false;
            let mut stream = provider.stream_chat(req.clone());
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
        model_id: live_model_id.clone(),
        system: system.clone(),
        messages: prime_msgs,
        effort: None,
        effort_support: Default::default(),
        max_tokens: Some(64),
        tools: tools.clone(),
        previous_response_id: None,
        context_transport: crate::providers::ContextTransport::Stateless,
        hosted_web_search: false,
    };
    let u = send(&live_provider, prime).await;
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
        history.len() - older.len(),
        None,
        "",
        None,
        &live_model_id,
        false,
    );
    assert!(is_aware);
    let u = send(&live_provider, aware).await;
    report("aware", &u);

    // standalone: tiny system + rendered transcript
    let (alone, is_aware) = compaction_request(
        None,
        older,
        &history,
        0,
        None,
        "",
        None,
        &live_model_id,
        false,
    );
    assert!(!is_aware);
    let u = send(&live_provider, alone).await;
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
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.5);
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
        CompactionReason::Pressure,
        "",
        None,
        None,
    )
    .await;
    assert!(out.is_none(), "no pressure → no compaction: {out:?}");
    assert!(summary.is_none());
    let after: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
    assert_eq!(snapshot, after, "history must pass through byte-identical");
}

/// Prod-shape replication: 95 mixed messages (~46k tokens, like the
/// T1 shakedown transcript), 1M limit, 0.01 threshold.
/// compact_history must shrink and report — not silently pass through.

#[tokio::test]
async fn compact_history_trims_a_long_plain_transcript() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01);
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
        CompactionReason::Pressure,
        "",
        None,
        None,
    )
    .await
    .expect("must compact a 4x-over-budget transcript");
    assert!(out.after < out.before, "must shrink: {out:?}");
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
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01);
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
            CompactionReason::Pressure,
            "",
            None,
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

/// Bench shape: ONE user prompt followed by assistant/tool cycles.
/// Regression test: the user-boundary-only cut never fired here, so
/// compactions stayed 0 while measured grew past the budget every turn.
#[tokio::test]
async fn compact_history_trims_a_single_prompt_tool_loop() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let policy = context::Policy::with_compaction(1_000_000, 0.08, 4, 0.01);
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
            CompactionReason::Pressure,
            "",
            None,
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
    let gate = |trigger: &'static str| CompactionGate {
        trigger,
        priced: 210_000,
        budget: 204_800,
        priced_after: 12_000,
        context_limit: 256_000,
        bytes_per_token: 3,
        anchored: true,
        tail_budget: 25_600,
        tail_kept: 7,
    };
    let root = std::env::temp_dir().join(format!("sqwai-compact-rec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let events = crate::agent::events::EventLog::open(&root, "sess").expect("open");
    record_compaction(
        &mut Some(events),
        &CompactionOutcome {
            before: 10_000,
            after: 2_500,
            summarized: true,
            trimmed: Some(context::TrimReport {
                dropped: 12,
                span: "user → assistant".into(),
                paths: vec!["src/a.rs".into()],
            }),
            gate: gate("pressure"),
        },
        40,
        10,
        "did things",
    );
    // None events: silent no-op, never panics
    record_compaction(
        &mut None,
        &CompactionOutcome {
            before: 1,
            after: 1,
            summarized: false,
            trimmed: None,
            gate: gate("manual"),
        },
        1,
        1,
        "",
    );
    let recs = crate::agent::events::EventLog::records(&root).expect("read");
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
    assert_eq!(c.fields["trimmed_msgs"], 12);
    assert_eq!(c.fields["trimmed_span"], "user → assistant");
    assert_eq!(c.fields["trimmed_paths"][0], "src/a.rs");
    // What the gate saw is as much a fact of the cut as what survived it: a
    // session read must answer "why did this fire" without re-deriving it.
    assert_eq!(c.fields["trigger"], "pressure");
    assert_eq!(c.fields["priced"], 210_000);
    assert_eq!(c.fields["budget"], 204_800);
    assert_eq!(c.fields["priced_after"], 12_000);
    assert_eq!(c.fields["context_limit"], 256_000);
    assert_eq!(c.fields["bpt"], 3);
    assert_eq!(c.fields["anchored"], true);
    assert_eq!(c.fields["tail_budget"], 25_600);
    assert_eq!(c.fields["tail_kept"], 7);
    let _ = std::fs::remove_dir_all(&root);
}

/// Until a billed measurement exists, the gate must price the fixed prefix
/// too: a small transcript with a huge system block does not fit a small
/// window even though its messages do.
#[tokio::test]
async fn a_big_prefix_makes_a_small_transcript_compact() {
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let system = vec![crate::providers::SystemPart::volatile("x".repeat(60_000))];
    let tools: Vec<crate::providers::ToolSpec> = Vec::new();
    let prefix = CompactionPrefix {
        system: &system,
        tools: &tools,
    };
    let mut messages = three_turns();
    let mut summary = None;
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &context::Policy::with_compaction(16_000, 0.08, 2, 0.80),
        CompactionReason::Pressure,
        "",
        None,
        Some(&prefix),
    )
    .await;
    assert!(
        out.is_some(),
        "60 KB of system parts must not fit a 16k window"
    );

    // the same transcript without the prefix is left alone
    let mut messages = three_turns();
    let mut summary = None;
    let empty = CompactionPrefix {
        system: &[],
        tools: &[],
    };
    let out = compact_history(
        &provider,
        "m",
        &mut messages,
        &mut summary,
        &context::Policy::with_compaction(40_000, 0.08, 2, 0.80),
        CompactionReason::Pressure,
        "",
        None,
        Some(&empty),
    )
    .await;
    assert!(out.is_none(), "messages alone fit: {out:?}");
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
        CompactionReason::Pressure,
        "",
        None,
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.before > out.after, "must shrink: {out:?}");
    assert!(out.summarized, "model summary must be reported");
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
        CompactionReason::Pressure,
        "",
        None,
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
        CompactionReason::Manual,
        "",
        None,
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
        CompactionReason::Pressure,
        "",
        None,
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.summarized);
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
        CompactionReason::Pressure,
        "",
        None,
        None,
    )
    .await
    .expect("pressure is over: must compact");
    assert!(out.summarized);
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
        CompactionReason::Pressure,
        "",
        None,
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
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
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

/// ASK confirms network reads before the parallel read-only batch: the
/// approval carries no "always" option and a denial reaches the model as a
/// tool result without the URL ever being fetched.
#[tokio::test]
async fn ask_webfetch_prompts_without_always_before_fetching() {
    let provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::ToolCall(
                crate::providers::ToolCallReq::new(
                    "c1",
                    "webfetch",
                    serde_json::json!({"url": "https://example.com/private"}),
                ),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });
    let temp_dir = std::env::temp_dir().join(format!("sqwai-test-netask-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "netask".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "fetch it")],
        root: temp_dir.clone(),
        session_id: "test-netask-sess".into(),
        blocked_patterns: vec![],
        approval_mode: crate::config::ApprovalMode::Ask,
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10_000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    let mut prompted = false;
    let mut denied_result = false;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::Approval {
                id,
                subject,
                allow_always,
                ..
            } => {
                assert!(subject.contains("https://example.com/private"), "{subject}");
                assert!(!allow_always, "network reads must ask every time");
                prompted = true;
                handle
                    .control
                    .send(ControlMsg::ApprovalAnswer {
                        id,
                        decision: ApprovalDecision::Deny,
                    })
                    .await
                    .unwrap();
            }
            AgentEvent::Completed(Ok(outcome)) => {
                denied_result = outcome.messages.iter().any(|m| {
                    m.role == Role::Tool && m.content.contains("network read denied by user")
                });
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
            _ => {}
        }
    }
    assert!(prompted, "webfetch in ASK must ask before fetching");
    assert!(denied_result, "the denial must reach the transcript");
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// A child never joins the parent plan: its plan and evidence belong to its
/// own session, and the host archives whatever plan it leaves active.
#[tokio::test]
async fn subagent_does_not_join_its_parent_plan() {
    let root = std::env::temp_dir().join(format!("sqwai-subjoin-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let mut plan = plan::create(
        "parent goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![plan::NewStep {
            title: "step".into(),
            check: None,
        }],
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["parent-sess".into()];
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
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);

    let reloaded = plan::open(&root, &plan_id).unwrap();
    assert_eq!(
        reloaded.sessions,
        vec!["parent-sess".to_string()],
        "the child owns its own plan and never joins the parent's"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The parent event log carries one start/done pair per child, which is what
/// the reload uses to rebuild every row of a batched call.
#[tokio::test]
async fn subagent_lifecycle_lands_in_the_parent_event_log() {
    let root = std::env::temp_dir().join(format!("sqwai-sublog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let provider: SharedProvider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "child done".into(),
        ))]]),
    });
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "call-42",
        "subagent",
        serde_json::json!({"tasks": ["look around"]}),
    );
    let outcome = run_subagent(
        &call,
        "parent-events",
        &parent_tx,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &provider,
        "m",
        &root,
        &[],
        &[],
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    assert!(
        outcome.output.starts_with("[subagent ")
            && outcome.output.contains("| completed |")
            && outcome.output.contains("plan: none"),
        "host envelope is not the receipt: {}",
        outcome.output
    );

    let records = crate::agent::events::EventLog::records_for(&root, "parent-events").unwrap();
    let starts: Vec<_> = records
        .iter()
        .filter(|r| r.kind == "subagent_start")
        .collect();
    let dones: Vec<_> = records
        .iter()
        .filter(|r| r.kind == "subagent_done")
        .collect();
    assert_eq!(starts.len(), 1, "{records:?}");
    assert_eq!(dones.len(), 1, "{records:?}");
    assert_eq!(starts[0].fields["call_id"], "call-42");
    assert_eq!(starts[0].fields["summary"], "look around");
    assert_eq!(dones[0].fields["status"], "completed");
    assert_eq!(
        starts[0].fields["child_id"], dones[0].fields["child_id"],
        "the pair names the same child"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_child_approval_answer_reaches_that_childs_control_channel() {
    let (tx, mut rx) = mpsc::channel(4);
    super::loop_subagent::register_subagent_control_for_test(9_001, tx);
    assert!(super::loop_subagent::answer_subagent_approval(
        9_001,
        42,
        ApprovalDecision::RunOnce,
    ));
    match rx.recv().await {
        Some(ControlMsg::ApprovalAnswer { id, decision }) => {
            assert_eq!(id, 42);
            assert_eq!(decision, ApprovalDecision::RunOnce);
        }
        other => panic!("expected the child approval answer, got {other:?}"),
    }
    assert!(
        !super::loop_subagent::answer_subagent_approval(9_002, 1, ApprovalDecision::Deny),
        "an unknown child has no channel"
    );
}

#[test]
fn live_child_ids_skip_restored_ones() {
    super::loop_subagent::bump_subagent_id_at_least(1_000_000);
    let next = super::loop_subagent::next_subagent_id();
    assert!(next > 1_000_000, "restored ids must not be reused: {next}");
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
            check: None,
        }],
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["sess".into()];
    plan::store(&root, &plan).unwrap();
    // step 1 starts automatically on create
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
            check: None,
        }],
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["other-sess".into()];
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
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        None,
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
/// A child's active plan is its own; when the child ends the host archives it
/// instead of leaving an active plan nobody owns.
#[test]
fn an_active_child_plan_is_abandoned_when_the_child_finishes() {
    let root = std::env::temp_dir().join(format!("sqwai-subplan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let mut plan = plan::create(
        "child goal".into(),
        Vec::new(),
        vec!["note".to_string()],
        vec![plan::NewStep {
            title: "child step".into(),
            check: None,
        }],
        &plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec!["child-sess".into()];
    plan::store(&root, &plan).unwrap();
    let id = plan.id.clone();

    super::loop_subagent::abandon_child_plan(&root, "parent-sess", "child-sess");

    let reloaded = plan::open(&root, &id).unwrap();
    assert_eq!(reloaded.status, plan::PlanStatus::Abandoned);
    assert!(
        plan::open_active_for_session(&root, Some("child-sess"))
            .unwrap()
            .is_none(),
        "no active plan is left behind"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The tool schema a child receives never offers the two tools its runtime
/// refuses; the plan it owns stays available.
#[tokio::test]
async fn a_child_request_omits_subagent_and_ask_user_tools() {
    let root = std::env::temp_dir().join(format!("sqwai-subtools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let recorder = std::sync::Arc::new(RecordingTestProvider {
        events: std::sync::Mutex::new(vec![vec![Ok(crate::providers::StreamEvent::Text(
            "done".into(),
        ))]]),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let provider: SharedProvider = recorder.clone();
    let (parent_tx, _parent_rx) = mpsc::channel(64);
    let call = crate::providers::ToolCallReq::new(
        "call-tools",
        "subagent",
        serde_json::json!({"tasks": ["look around"]}),
    );
    let outcome = run_subagent(
        &call,
        "parent-tools",
        &parent_tx,
        &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        &provider,
        "m",
        &root,
        &[],
        &[],
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(SUBAGENT_TIMEOUT_SECS),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.output);
    let requests = recorder.requests.lock().unwrap();
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(!names.contains(&"subagent"), "{names:?}");
    assert!(!names.contains(&"subagents"), "{names:?}");
    assert!(!names.contains(&"ask_user"), "{names:?}");
    assert!(names.contains(&"plan"), "the child owns a plan: {names:?}");
    assert!(names.contains(&"read"), "{names:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A background spawn returns immediately; the parent answers while the
/// child runs, and the report joins the transcript before the turn ends.
#[tokio::test]
async fn a_background_subagent_reports_before_the_turn_ends() {
    let root = std::env::temp_dir().join(format!("sqwai-bgsub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let recorder = std::sync::Arc::new(BackgroundRoutingProvider {
        parent_calls: std::sync::atomic::AtomicUsize::new(0),
        child_saw_steer: std::sync::atomic::AtomicBool::new(false),
    });
    let provider: SharedProvider = recorder.clone();
    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "p/m".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "do it")],
        root: root.clone(),
        session_id: "bg-parent".into(),
        blocked_patterns: vec![],
        approval_mode: crate::config::ApprovalMode::Auto,
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10_000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };
    let mut handle = spawn_agent(input);
    let mut outcome = None;
    let mut note = false;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::SubagentReportNote { text } => {
                note = true;
                assert!(text.contains("background subagent finished"), "{text}");
            }
            AgentEvent::Completed(Ok(done)) => {
                outcome = Some(done);
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    let outcome = outcome.expect("turn completed");
    assert!(
        outcome.messages.iter().any(|m| {
            m.role == Role::Tool
                && m.tool_call_id.as_deref() == Some("call-out")
                && m.content.contains("child report after steer")
        }),
        "subagents output returns the report: {:#?}",
        outcome.messages
    );
    assert!(
        !outcome.messages.iter().any(|m| {
            m.role == Role::User && m.content.contains("<subagent_completion")
        }),
        "a report read through output is not repeated as a host note"
    );
    assert!(!note, "a delivered report needs no report note");
    assert!(
        recorder.child_saw_steer.load(std::sync::atomic::Ordering::Relaxed),
        "the child must receive the parent's message at its next request"
    );
    let _ = std::fs::remove_dir_all(&root);
}

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
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_millis(150),
        None,
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
        crate::config::ApprovalMode::Auto,
        10_000,
        true,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &[],
        None,
        crate::config::EffortSupport::default(),
        None,
        Vec::new(),
        crate::config::McpConfig::default(),
        crate::config::SkillsConfig::default(),
        false,
        crate::config::ShadowStore::Off,
        crate::config::CompactionConfig::default(),
        crate::config::PlanConfig::default(),
        Vec::new(),
        std::time::Duration::from_secs(60),
        None,
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
/// pairs). Rows, events and messages still come out in call order.
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
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
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

#[tokio::test]
async fn parallel_read_only_batch_merges_read_state_before_following_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.txt"), "old a\n").unwrap();
    std::fs::write(root.join("b.txt"), "old b\n").unwrap();

    // Two parallel reads (`a.txt` and `b.txt`) followed by an overwrite `write`
    // on `a.txt` in the same turn batch: `write` requires `a.txt` to be in
    // `ctx.files_read`, proving the parallel batch merged its read state back
    // before the mutating call ran.
    let provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "r1",
                        "read",
                        serde_json::json!({"file_path": "a.txt"}),
                    ),
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "r2",
                        "read",
                        serde_json::json!({"file_path": "b.txt"}),
                    ),
                )),
                Ok(crate::providers::StreamEvent::ToolCall(
                    crate::providers::ToolCallReq::new(
                        "w1",
                        "write",
                        serde_json::json!({"file_path": "a.txt", "content": "new a\n"}),
                    ),
                )),
            ],
            vec![Ok(crate::providers::StreamEvent::Text("done".into()))],
        ]),
    });

    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: Default::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "read both and overwrite a.txt")],
        root: root.clone(),
        session_id: "test-parallel-ro".into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    let mut notices: Vec<(String, bool)> = Vec::new();
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::ToolNotice { call_id, ok, .. } => notices.push((call_id, ok)),
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    assert_eq!(
        notices,
        vec![
            ("r1".to_string(), true),
            ("r2".to_string(), true),
            ("w1".to_string(), true),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "new a\n"
    );
}

/// A closed plan used to be the end of the run: the model wrote "Готово." with
/// its own list of undone work underneath and stopped, and in a live session it
/// resumed only after the user typed again — three times in a row. The host now
/// asks once, at the moment the turn would end with no plan open.
#[tokio::test]
async fn closing_a_plan_does_not_end_the_run_by_itself() {
    let root = std::env::temp_dir().join(format!("sqwai-test-postclose-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let session_id = "test-postclose-sess";

    let mut plan = crate::plan::create(
        "build the thing".into(),
        vec![],
        vec![],
        vec![crate::plan::NewStep {
            title: "implement it".into(),
            check: None,
        }],
        &crate::plan::Limits::default(),
    )
    .unwrap();
    plan.sessions = vec![session_id.to_string()];
    plan.steps[0].status = crate::plan::StepStatus::Done;
    crate::plan::store(&root, &plan).unwrap();

    let provider = std::sync::Arc::new(MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(crate::providers::StreamEvent::ToolCall(
                crate::providers::ToolCallReq::new(
                    "p1",
                    "plan",
                    serde_json::json!({"op": "close", "summary": "goal met by the step above"}),
                ),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text(
                "Готово. Мобы и loot tables ещё не реализованы.".into(),
            ))],
            vec![Ok(crate::providers::StreamEvent::Text(
                "then the request is delivered".into(),
            ))],
        ]),
    });

    let input = AgentInput {
        provider,
        model_id: "m".into(),
        model_key: "primary".into(),
        effort: None,
        effort_support: crate::config::EffortSupport::default(),
        max_tokens: None,
        system: vec![],
        messages: vec![Message::new(Role::User, "build the thing")],
        root: root.clone(),
        session_id: session_id.into(),
        blocked_patterns: vec![],
        approval_mode: Default::default(),
        approval_switch: None,
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: true,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };

    let mut handle = spawn_agent(input);
    let mut outcome = None;
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::Completed(Ok(done)) => {
                outcome = Some(done);
                break;
            }
            AgentEvent::Completed(Err(e)) => panic!("unexpected agent error: {e}"),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&root);

    let messages = outcome.expect("run completed").messages;
    let asked = messages
        .iter()
        .filter(|m| m.content.starts_with("[host: the plan you closed"))
        .count();
    assert_eq!(asked, 1, "the host asks exactly once after a close");
    let note = messages
        .iter()
        .position(|m| m.content.starts_with("[host: the plan you closed"))
        .expect("host note present");
    let final_answer = messages
        .iter()
        .rposition(|m| m.content == "then the request is delivered")
        .expect("the model got to answer again");
    assert!(
        note < final_answer,
        "the note rides before the answer that ends the turn"
    );
}

/// A provider that replays an id inside one turn must not run the call twice:
/// a repeated `bash` or `edit` is a real second side effect, not a no-op.
#[test]
fn a_replayed_call_in_one_turn_runs_once() {
    let mut calls = vec![
        ToolCallReq::new("c1", "bash", serde_json::json!({"command": "make"})),
        ToolCallReq::new("c1", "bash", serde_json::json!({"command": "make"})),
    ];
    normalize_call_ids(&mut calls, &std::collections::HashSet::new());
    assert_eq!(calls.len(), 1, "the replay must not survive: {calls:?}");
    assert_eq!(calls[0].id, "c1");
}

/// The same id on two different calls is the provider's collision, not the
/// model's wish — renaming keeps both real calls alive.
#[test]
fn two_different_calls_sharing_an_id_both_survive() {
    let mut calls = vec![
        ToolCallReq::new("c1", "edit", serde_json::json!({"file_path": "a.rs"})),
        ToolCallReq::new("c1", "edit", serde_json::json!({"file_path": "b.rs"})),
    ];
    normalize_call_ids(&mut calls, &std::collections::HashSet::new());
    assert_eq!(calls.len(), 2, "a collision must not swallow a call");
    assert_ne!(calls[0].id, calls[1].id);
    assert!(calls[1].id.starts_with("c1__sq"), "{:?}", calls[1].id);
}

/// An id reused from earlier history leaves two tool results under one
/// `tool_call_id`, and the endpoint rejects the payload: the session dies on a
/// protocol error that looks unrelated to anything the model did.
#[test]
fn an_id_already_in_history_is_renamed_not_dropped() {
    let mut history = std::collections::HashSet::new();
    history.insert("call_7".to_string());
    let mut calls = vec![ToolCallReq::new(
        "call_7",
        "read",
        serde_json::json!({"file_path": "a.rs"}),
    )];
    normalize_call_ids(&mut calls, &history);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_7__sq1");
}

#[test]
fn history_ids_are_read_from_both_sides_of_the_pair() {
    let mut messages = vec![Message::new(Role::Assistant, "").with_tool_calls(vec![
        ToolCallReq::new("a", "read", serde_json::json!({})),
    ])];
    messages.push(Message::tool_result("b", "x".to_string(), false));
    let ids = history_call_ids(&messages);
    assert!(ids.contains("a"), "{ids:?}");
    assert!(ids.contains("b"), "{ids:?}");
}

/// The approval axis is live: the TUI flips one cell and the very next tool
/// call of the same run observes the new mode. The classifier's verdict
/// record carries the mode, so it is the proof — no dialog is involved.
struct FlipOnSecondRequest {
    inner: MockTestProvider,
    switch: crate::config::ApprovalSwitch,
    to: crate::config::ApprovalMode,
    seen: std::sync::atomic::AtomicUsize,
}

impl crate::providers::Provider for FlipOnSecondRequest {
    fn stream_chat(
        &self,
        req: crate::providers::ChatRequest,
    ) -> futures::stream::BoxStream<'static, crate::providers::StreamResult> {
        if self
            .seen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            self.switch.set(self.to);
        }
        self.inner.stream_chat(req)
    }
}

#[tokio::test]
async fn shifting_the_mode_mid_run_reaches_the_next_tool_call() {
    use crate::config::{ApprovalMode, ApprovalSwitch};
    let switch = ApprovalSwitch::new(ApprovalMode::Auto);
    let inner = MockTestProvider {
        events: std::sync::Mutex::new(vec![
            vec![Ok(StreamEvent::ToolCall(crate::providers::ToolCallReq::new(
                "c1",
                "bash",
                serde_json::json!({"command": "echo one"}),
            )))],
            vec![Ok(StreamEvent::ToolCall(crate::providers::ToolCallReq::new(
                "c2",
                "bash",
                serde_json::json!({"command": "echo two"}),
            )))],
            vec![Ok(StreamEvent::Text("done".into()))],
        ]),
    };
    let provider = std::sync::Arc::new(FlipOnSecondRequest {
        inner,
        switch: switch.clone(),
        to: ApprovalMode::Full,
        seen: Default::default(),
    });
    let temp_dir =
        std::env::temp_dir().join(format!("sqwai-test-live-mode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let session = "live-mode-sess";
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
        session_id: session.into(),
        blocked_patterns: vec![],
        approval_mode: ApprovalMode::Auto,
        approval_switch: Some(switch.clone()),
        approval_grants: Default::default(),
        web_allow_hosts: vec![],
        context_limit: 10000,
        vision: true,
        pdf: false,
        tools: Default::default(),
        search: Default::default(),
        limits: Default::default(),
        active_skills: vec![],
        read_ledger: Default::default(),
        enable_tools: true,
        background_baselines: false,
        read_only: false,
        previous_response_id: None,
        summary: None,
        mcp: Default::default(),
        skills: Default::default(),
        compact_only: false,
        compact_focus: String::new(),
        compaction: Default::default(),
        plan_limits: Default::default(),
        shadow_store: crate::config::ShadowStore::Off,
        subagent_depth: 0,
        parent_step: None,
        parent_session: None,
        fallback_chain: vec![],
    };
    let mut handle = spawn_agent(input);
    while let Some(ev) = handle.rx.recv().await {
        match ev {
            AgentEvent::Completed(Ok(_)) => break,
            AgentEvent::Completed(Err(e)) => panic!("unexpected error: {e}"),
            _ => {}
        }
    }
    let modes: Vec<String> = crate::agent::events::EventLog::records_for(&temp_dir, session)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == "verdict")
        .filter_map(|r| {
            r.fields
                .get("mode")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        modes,
        vec!["auto".to_string(), "full".to_string()],
        "the second call must run under the mode the user switched to"
    );
    assert_eq!(switch.get(), ApprovalMode::Full);
    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// Retries used to fire at exactly the base delay, so every session that
/// hit the same outage retried in lockstep. The jitter must stay inside
/// 20% of the base and must actually vary with the clock sample.
#[test]
fn backoff_jitter_stays_inside_twenty_percent_and_varies() {
    use super::loop_turn::{backoff, backoff_with_nanos, base_backoff};
    let base = base_backoff(3);
    assert_eq!(base, std::time::Duration::from_secs(8));
    let mut seen = std::collections::BTreeSet::new();
    for nanos in 0..=200u32 {
        let delay = backoff_with_nanos(3, nanos);
        assert!(delay >= base, "{nanos}: {delay:?} below the base");
        assert!(
            delay <= base + std::time::Duration::from_millis(1600),
            "{nanos}: {delay:?} past 20%"
        );
        seen.insert(delay);
    }
    assert!(seen.len() > 1, "every sample produced the same delay");
    let live = backoff(3);
    assert!((base..=base + std::time::Duration::from_millis(1600)).contains(&live));
}
