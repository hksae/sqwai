use super::loop_compact::rejects_continuation;
use super::loop_turn::{request_messages, turn_transport};
use super::*;
use crate::providers::ContextTransport;

fn roles(messages: &[Message]) -> Vec<Role> {
    messages.iter().map(|m| m.role).collect()
}

/// Continuing from a previous response, the provider holds everything up
/// to its own last message; we send what it has not seen.
#[test]
fn a_continuation_sends_the_tool_results_the_provider_is_waiting_for() {
    let messages = vec![
        Message::new(Role::User, "read the file"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "c1",
            "read",
            serde_json::json!({}),
        )]),
        Message::tool_result("c1", "fn main() {}", false),
    ];
    assert_eq!(
        roles(&request_messages(
            &messages,
            ContextTransport::PreviousResponse
        )),
        vec![Role::Tool],
        "the result of the call must reach the model"
    );

    // several calls in one turn: every result travels
    let mut many = messages.clone();
    many.push(Message::tool_result("c2", "other", false));
    assert_eq!(
        roles(&request_messages(&many, ContextTransport::PreviousResponse)),
        vec![Role::Tool, Role::Tool]
    );

    // a plain exchange still sends just the new user turn
    let plain = vec![
        Message::new(Role::User, "hi"),
        Message::new(Role::Assistant, "hello"),
        Message::new(Role::User, "again"),
    ];
    assert_eq!(
        roles(&request_messages(
            &plain,
            ContextTransport::PreviousResponse
        )),
        vec![Role::User]
    );

    // and a user turn that follows tool results keeps both, in order
    let mixed = vec![
        Message::new(Role::Assistant, "done"),
        Message::tool_result("c1", "out", false),
        Message::new(Role::User, "now this"),
    ];
    assert_eq!(
        roles(&request_messages(
            &mixed,
            ContextTransport::PreviousResponse
        )),
        vec![Role::Tool, Role::User]
    );
}

fn caps() -> crate::providers::ProviderCapabilities {
    crate::providers::ProviderCapabilities {
        previous_response: true,
        ..Default::default()
    }
}

/// The live 400 this exists for, from an OpenAI-compatible relay that
/// accepts `previous_response_id` and does not chain by it:
/// `No tool call found for function call output with call_id ...`.
/// A request answering a tool call must carry the calls itself.
#[test]
fn a_request_answering_a_tool_call_does_not_rely_on_the_chain() {
    let mid_turn = vec![
        Message::new(Role::User, "read the file"),
        Message::new(Role::Assistant, "").with_tool_calls(vec![ToolCallReq::new(
            "c1",
            "read",
            serde_json::json!({}),
        )]),
        Message::tool_result("c1", "fn main() {}", false),
    ];
    assert_eq!(
        turn_transport(caps(), Some("resp_1"), &mid_turn, true),
        ContextTransport::Stateless,
        "tool outputs must travel with the calls they answer"
    );
    // and the whole transcript goes with it, calls included
    assert_eq!(
        request_messages(&mid_turn, ContextTransport::Stateless).len(),
        3
    );

    // between turns there is nothing to match, so continuation is fine
    let between = vec![
        Message::new(Role::Assistant, "done"),
        Message::new(Role::User, "next"),
    ];
    assert_eq!(
        turn_transport(caps(), Some("resp_1"), &between, true),
        ContextTransport::PreviousResponse
    );
}

/// Once a provider has refused the reference, the session stops offering
/// it — one failed request per session, not one per turn.
#[test]
fn a_refused_reference_is_not_offered_again() {
    let between = vec![
        Message::new(Role::Assistant, "done"),
        Message::new(Role::User, "next"),
    ];
    assert_eq!(
        turn_transport(caps(), Some("resp_1"), &between, false),
        ContextTransport::Stateless
    );
}

/// Prose matching again, so it is bounded on both sides: it must catch the
/// refusal and must not fire on a provider that simply has no such field.
#[test]
fn only_a_real_refusal_disables_the_continuation() {
    assert!(rejects_continuation(
        "provider returned 400: No tool call found for function call output with call_id call_x0"
    ));
    assert!(rejects_continuation(
        "provider returned 400: previous response not found"
    ));
    assert!(!rejects_continuation(
        "openai-compatible: previous_response_id dropped (not supported by Chat Completions)"
    ));
    assert!(!rejects_continuation(
        "provider returned 429: rate limit exceeded"
    ));
    assert!(!rejects_continuation(
        "provider returned 400: messages must alternate"
    ));
}

/// Stateless is the default for a reason: everything is resent, and the
/// shortening above must never leak into it.
#[test]
fn a_stateless_request_carries_the_whole_transcript() {
    let messages = vec![
        Message::new(Role::User, "one"),
        Message::new(Role::Assistant, "two"),
        Message::new(Role::User, "three"),
    ];
    assert_eq!(
        request_messages(&messages, ContextTransport::Stateless).len(),
        3
    );
    assert_eq!(
        request_messages(&messages, ContextTransport::ServerConversation).len(),
        3
    );
}
