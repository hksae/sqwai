use super::{AgentEvent, ControlMsg};
use super::loop_compact::{TurnFailure, rejects_continuation, rejects_effort_parameter};
use crate::providers::{
    ChatRequest, ContextTransport, Message, Role, SharedProvider, StreamEvent, ToolCallReq,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;


const RETRY_WINDOW: Duration = Duration::from_secs(3600);

pub(crate) fn backoff(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        4 => 15,
        5 => 30,
        _ => 60,
    };
    Duration::from_secs(secs)
}

/// Which part of the local transcript goes on the wire.
///
/// Only a documented continuation reference shortens it: the provider already
/// holds those turns, and resending them would duplicate the remote history.
/// The system block travels separately and is always sent.
/// What a request carries under each transport.
///
/// Continuing from a previous response means the provider already holds
/// everything up to and including its own last message, so we send only what
/// it has not seen: the trailing run of tool results and user turns after the
/// last assistant message.
///
/// This used to send "the last user message", which is right for a plain
/// exchange and wrong the moment tools are in play: a tool result is
/// `Role::Tool`, so the run of results the model is waiting for was dropped
/// and the turn continued as if the tools had never been called.
/// Transport for one request.
///
/// Continuation is dropped whenever the request answers a tool call: the
/// matching `function_call` items live in the provider's chain, and a provider
/// that does not really keep that chain rejects the outputs outright. Sending
/// the transcript ourselves costs a few hundred bytes and cannot fail that
/// way. `capabilities()` describes the wire format, not the endpoint behind
/// it, so it cannot answer this on its own — relays accept the field and
/// ignore it.
pub(crate) fn turn_transport(
    caps: crate::providers::ProviderCapabilities,
    previous_response_id: Option<&str>,
    messages: &[Message],
    continuation_usable: bool,
) -> ContextTransport {
    let answering_a_call = messages.last().is_some_and(|m| m.role == Role::Tool);
    if !continuation_usable || answering_a_call {
        let mut caps = caps;
        caps.previous_response = false;
        return crate::providers::select_transport(caps, None);
    }
    crate::providers::select_transport(caps, previous_response_id)
}

pub(crate) fn request_messages(messages: &[Message], transport: ContextTransport) -> Vec<Message> {
    if transport != ContextTransport::PreviousResponse {
        return messages.to_vec();
    }
    let unseen = messages
        .iter()
        .rev()
        .take_while(|m| matches!(m.role, Role::Tool | Role::User))
        .count();
    messages[messages.len() - unseen..].to_vec()
}

pub(crate) struct TurnOutcome {
    pub(crate) text: String,
    pub(crate) calls: Vec<ToolCallReq>,
    /// how many retries it took to get this answer, for the journal
    pub(crate) retries: u32,
    /// reasoning tokens the provider reported for this turn, when it counts
    /// them at all. `Some(0)` is evidence that a requested effort level was
    /// not acted on; `None` says nothing either way.
    pub(crate) reasoning_tokens: Option<u64>,
    /// reasoning content was streamed in this turn, which contradicts a zero
    /// count whatever the usage block says
    pub(crate) saw_reasoning: bool,
    /// the provider rejected the effort parameter outright, so the turn was
    /// retried without it and the session must stop sending it
    pub(crate) effort_rejected: bool,
    /// provider-owned opaque state attached to this turn (reasoning items, phase)
    pub(crate) provider_state: Option<serde_json::Value>,
}


pub(crate) async fn run_turn(
    provider: &SharedProvider,
    req: &ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
    _ctl: &mut mpsc::Receiver<ControlMsg>,
    response_id: &mut Option<String>,
    prompt_size: &mut u64,
    has_fallback: bool,
) -> Result<TurnOutcome, TurnFailure> {
    use futures::StreamExt;

    let mut attempt: u32 = 0;
    let mut deadline: Option<Instant> = None;
    // The request can lose its effort parameter mid-flight (see below), so the
    // loop works on its own copy rather than on the caller's.
    let mut req = req.clone();
    // what the user asked for, kept separately: the parameter can be stripped
    // mid-flight, and a turn that only succeeded without it is exactly the
    // turn worth reporting
    let requested_effort = req.effort;
    let mut effort_rejected = false;

    loop {
        let mut reasoning_tokens: Option<u64> = None;
        // reasoning content actually arrived in this turn
        let mut saw_reasoning = false;
        let mut got_delta = false;
        let mut failed: Option<anyhow::Error> = None;
        let mut text = String::new();
        let mut calls: Vec<ToolCallReq> = Vec::new();
        let mut provider_state: Option<serde_json::Value> = None;

        let mut stream = provider.stream_chat(req.clone());
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(StreamEvent::Text(t)) => {
                    if !t.is_empty() {
                        got_delta = true;
                        text.push_str(&t);
                        if tx.send(AgentEvent::TextDelta(t)).await.is_err() {
                            return Err(TurnFailure::new("tui closed", None, attempt));
                        }
                    }
                }
                Ok(StreamEvent::Reasoning(t)) => {
                    if !t.is_empty() {
                        got_delta = true;
                        saw_reasoning = true;
                        if tx.send(AgentEvent::ThinkingDelta(t)).await.is_err() {
                            return Err(TurnFailure::new("tui closed", None, attempt));
                        }
                    }
                }
                Ok(StreamEvent::Usage(u)) => {
                    // prompt size of *this* request: replaces, never accumulates.
                    // Some providers send a second output-only event with zero
                    // input — that must not reset the meter.
                    if u.prompt_tokens > 0 {
                        *prompt_size = u.prompt_tokens;
                    }
                    // Same hazard as the line above, and the one that bit us:
                    // a gateway can send a second usage event whose
                    // `completion_tokens_details` is a stub of zeros. Taking
                    // the last value let a real count decay to 0, and the host
                    // then reported the level as ignored. These counters only
                    // grow, so keep the largest one seen.
                    if let Some(n) = u.reasoning_tokens {
                        reasoning_tokens = Some(reasoning_tokens.unwrap_or(0).max(n));
                    }
                    // no-op unless the http log is on; this is the line that
                    // makes an inconsistent gateway diagnosable at all
                    crate::providers::log_http(&format!(
                        "usage event: prompt={} completion={} cached={:?} written={:?} reasoning={:?}",
                        u.prompt_tokens,
                        u.completion_tokens,
                        u.cached_tokens,
                        u.cache_write_tokens,
                        u.reasoning_tokens
                    ));
                    if tx.send(AgentEvent::Usage(u)).await.is_err() {
                        return Err(TurnFailure::new("tui closed", None, attempt));
                    }
                }
                Ok(StreamEvent::ResponseId(id)) => {
                    // A continuation chain moves forward: the next iteration
                    // must reference this response, not the one before it.
                    *response_id = Some(id.clone());
                    let _ = tx.send(AgentEvent::ResponseId(id)).await;
                }
                Ok(StreamEvent::ToolCall(tc)) => {
                    // only tool-capable requests may schedule tools; a call
                    // emitted for a tools-less request is ignored
                    if req.tool_capable() {
                        calls.push(tc);
                    }
                }
                Ok(StreamEvent::ProviderState(s)) => {
                    provider_state = Some(s);
                }
                Err(e) => failed = Some(e),
            }
            if failed.is_some() {
                break;
            }
        }

        let Some(error) = failed else {
            // The one line that makes the effort machinery inspectable: what
            // was asked for, what went on the wire, and what the provider
            // counted. Without it, "the level was honoured" and "the provider
            // says nothing about reasoning" look identical from outside.
            if let Some(level) = requested_effort {
                let sent = match req.effort {
                    Some(still) => format!(
                        "{:?}",
                        crate::providers::effort::plan(still, req.effort_support).wire
                    ),
                    // stripped by the rejection retry above
                    None => "nothing (parameter refused by the provider)".to_string(),
                };
                crate::providers::log_http(&format!(
                    "turn: effort requested={} sent={} reasoning_tokens={}",
                    level.as_str(),
                    sent,
                    match reasoning_tokens {
                        Some(n) => n.to_string(),
                        None => "not reported".into(),
                    }
                ));
            }
            return Ok(TurnOutcome {
                text,
                calls,
                reasoning_tokens,
                saw_reasoning,
                effort_rejected,
                retries: attempt,
                provider_state,
            });
        };
        let class = crate::providers::class_of(&error);
        let err = format!("{error:#}");

        // A provider that took a continuation reference and cannot resolve it
        // leaves the caller a way out: resend the transcript. run_turn cannot
        // do that itself — the request it holds was already shortened — so it
        // reports the cause and stops.
        if req.previous_response_id.is_some() && rejects_continuation(&err) {
            crate::providers::log_http(&format!(
                "provider refused the continuation reference: {err}"
            ));
            return Err(TurnFailure::continuation_rejected(err, attempt));
        }

        // A gateway that refuses the reasoning parameter is telling us the
        // model's declared support is wrong. Retrying the same body cannot
        // help, but retrying without the parameter can — and the caller then
        // stops sending it for the rest of the session. This is checked before
        // the class, because the rejection arrives as an ordinary 400 that
        // would otherwise end the turn.
        if req.effort.is_some() && rejects_effort_parameter(&err) {
            crate::providers::log_http(&format!(
                "provider rejected the effort parameter, retrying without it: {err}"
            ));
            req.effort = None;
            effort_rejected = true;
            attempt += 1;
            continue;
        }

        // Decide from the class, not from the prose. Retrying an expired key
        // or an exhausted quota for an hour is as wrong as giving up on a 503.
        if let Some(class) = class {
            use crate::providers::ErrorClass;
            if !class.retryable() {
                // ContextOverflow lands here too: asking the same oversized
                // request again cannot help, so the caller compacts and runs
                // the turn once more.
                let advice = match class {
                    ErrorClass::Auth => " — check the API key for this provider",
                    ErrorClass::Quota => " — the account is out of quota or credit",
                    _ => "",
                };
                return Err(TurnFailure::new(
                    format!("{err}{advice}"),
                    Some(class),
                    attempt,
                ));
            }
        } else if err.contains("provider returned 400 Bad Request")
            || err.contains("invalid_request_error")
        {
            // Unclassified, but recognisably deterministic: a gateway that
            // answers 200 with an error body lands here.
            return Err(TurnFailure::new(err, None, attempt));
        }

        if got_delta {
            // partial answer already streamed; a retry would duplicate it
            return Err(TurnFailure::new(
                format!("{err} — partial answer kept, not retried"),
                class,
                attempt,
            ));
        }

        let now = Instant::now();
        let dl = *deadline.get_or_insert(now + RETRY_WINDOW);
        if now >= dl || (has_fallback && attempt >= 1) {
            return Err(TurnFailure::new(
                if has_fallback && attempt >= 1 {
                    format!("{err} — giving up after {attempt} retries (fallback model configured)")
                } else {
                    format!("{err} — giving up after 1h of retries")
                },
                class,
                attempt,
            ));
        }
        let delay = backoff(attempt);
        attempt += 1;
        if tx
            .send(AgentEvent::Retry {
                attempt,
                delay_secs: delay.as_secs(),
                error: err,
            })
            .await
            .is_err()
        {
            return Err(TurnFailure::new("tui closed", class, attempt));
        }
        tokio::time::sleep(delay).await;
    }
}

