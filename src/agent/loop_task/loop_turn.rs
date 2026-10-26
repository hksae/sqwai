use super::loop_compact::{TurnFailure, rejects_continuation};
use super::{AgentEvent, ControlMsg};
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

#[cfg(test)]
pub(crate) fn request_messages(messages: &[Message], transport: ContextTransport) -> Vec<Message> {
    request_messages_with_limit(messages, transport, 0)
}

pub(crate) fn request_messages_with_limit(
    messages: &[Message],
    transport: ContextTransport,
    context_limit: u64,
) -> Vec<Message> {
    let mut out = if transport != ContextTransport::PreviousResponse {
        messages.to_vec()
    } else {
        let unseen = messages
            .iter()
            .rev()
            .take_while(|m| matches!(m.role, Role::Tool | Role::User))
            .count();
        messages[messages.len() - unseen..].to_vec()
    };
    // Old tool output is trimmed here, on the way to the wire — never in the
    // session, where every result stays whole and can be handed back by
    // `recall`. The store and the request are different objects on purpose.
    crate::agent::tool_output::trim_for_wire_with_limit(&mut out, context_limit);
    out
}

pub(crate) type PrefetchedToolHandle = tokio::task::JoinHandle<(
    usize,
    u128,
    crate::agent::tools::Outcome,
    Option<crate::agent::tools::ToolCtx>,
)>;

pub(crate) struct TurnOutcome {
    pub(crate) text: String,
    pub(crate) calls: Vec<ToolCallReq>,
    /// how many retries it took to get this answer, for the journal
    pub(crate) retries: u32,
    /// provider-reported counters for this successful request
    pub(crate) usage: Option<crate::providers::Usage>,
    /// the reasoning text of this turn, for display only: the session stores
    /// it so a reloaded transcript can rebuild its thought rows
    pub(crate) reasoning_text: String,
    /// wall time the reasoning stream was open in this turn, in ms. Measured
    /// here because the provider cannot report it afterwards, and stored on
    /// the message so the `thought · 6s` label survives a restart.
    pub(crate) reasoning_ms: u64,
    /// provider-owned opaque state attached to this turn (reasoning items, phase)
    pub(crate) provider_state: Option<serde_json::Value>,
    /// read-only tool calls already spawned in the background mid-stream while
    /// the LLM was still generating subsequent tool calls
    pub(crate) prefetched: std::collections::HashMap<usize, PrefetchedToolHandle>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn_with_prefetch(
    provider: &SharedProvider,
    req: &ChatRequest,
    tx: &mpsc::Sender<AgentEvent>,
    _ctl: &mut mpsc::Receiver<ControlMsg>,
    response_id: &mut Option<String>,
    prompt_size: &mut u64,
    has_fallback: bool,
    prefetch_env: Option<(&crate::agent::tools::ToolCtx, &[String])>,
) -> Result<TurnOutcome, TurnFailure> {
    use futures::StreamExt;

    let mut attempt: u32 = 0;
    let mut deadline: Option<Instant> = None;
    // The request can lose its effort parameter mid-flight (see below), so the
    // loop works on its own copy rather than on the caller's.
    let req = req.clone();

    loop {
        let mut usage: Option<crate::providers::Usage> = None;
        let mut got_delta = false;
        let mut failed: Option<anyhow::Error> = None;
        let mut text = String::new();
        // reasoning of THIS attempt only: a retry that replaces a failed
        // request must not inherit the dead attempt's thoughts
        let mut reasoning_text = String::new();
        // open span of the reasoning phase, summed across bursts: reasoning
        // starts on the first reasoning chunk and stops the moment anything
        // else arrives, which is what "how long it thought" means on screen
        let mut reasoning_open: Option<Instant> = None;
        let mut reasoning_ms = 0u64;
        let mut calls: Vec<ToolCallReq> = Vec::new();
        let mut provider_state: Option<serde_json::Value> = None;
        let mut prefetched: std::collections::HashMap<usize, PrefetchedToolHandle> =
            std::collections::HashMap::new();
        let mut prefetch_open = prefetch_env.is_some();

        let mut stream = provider.stream_chat(req.clone());
        while let Some(ev) = stream.next().await {
            if !matches!(ev, Ok(StreamEvent::Reasoning(_)))
                && let Some(at) = reasoning_open.take()
            {
                reasoning_ms += at.elapsed().as_millis() as u64;
            }
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
                        reasoning_open.get_or_insert_with(Instant::now);
                        reasoning_text.push_str(&t);
                        if tx.send(AgentEvent::ThinkingDelta(t)).await.is_err() {
                            return Err(TurnFailure::new("tui closed", None, attempt));
                        }
                    }
                }
                Ok(StreamEvent::Usage(u)) => {
                    usage = Some(u);
                    if u.prompt_tokens > 0 {
                        *prompt_size = u.prompt_tokens;
                    }
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
                    *response_id = Some(id.clone());
                    let _ = tx.send(AgentEvent::ResponseId(id)).await;
                }
                Ok(StreamEvent::ToolCall(tc)) => {
                    if req.tool_capable() {
                        let idx = calls.len();
                        calls.push(tc);
                        if prefetch_open
                            && let Some((base_ctx, allow_hosts)) = prefetch_env
                        {
                            // An outside read that still needs its boundary
                            // approval must NOT execute here: the prefetch
                            // runs the call outright, and the sequential
                            // gate (the only place that asks) skips calls
                            // already sitting in `parallel_outcomes`. A live
                            // session's first `ls` outside the root was
                            // refused exactly this way — prefetched past
                            // the gate. Hand it back to the dispatcher.
                            let needs_outside_ask = !base_ctx.outside_all
                                && super::loop_ask::read_call_path(&calls[idx].name, &calls[idx].args)
                                    .is_some_and(|p| base_ctx.outside_uncovered(&p));
                            if !base_ctx
                                .cancel
                                .load(std::sync::atomic::Ordering::Relaxed)
                                && !needs_outside_ask
                                && super::parallel_read_only_batch_end(
                                    &calls,
                                    0,
                                    base_ctx.limits.max_parallel_tools.clamp(1, 64),
                                ) == calls.len()
                            {
                                let c = calls[idx].clone();
                                let mut child_ctx = base_ctx.clone();
                                child_ctx.current_call_id = Some(c.id.clone());
                                let hosts = allow_hosts.to_vec();
                                let handle = tokio::spawn(super::execute_parallel_read_only_call(
                                    idx,
                                    c,
                                    child_ctx,
                                    hosts,
                                ));
                                prefetched.insert(idx, handle);
                            } else {
                                prefetch_open = false;
                            }
                        }
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

        if let Some(at) = reasoning_open.take() {
            reasoning_ms += at.elapsed().as_millis() as u64;
        }

        let Some(error) = failed else {
            return Ok(TurnOutcome {
                text,
                calls,
                reasoning_text,
                reasoning_ms,
                retries: attempt,
                usage,
                provider_state,
                prefetched,
            });
        };
        for (_, h) in prefetched {
            h.abort();
        }
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
                return Err(
                    TurnFailure::new(format!("{err}{advice}"), Some(class), attempt)
                        .with_partial(got_delta),
                );
            }
        } else if err.contains("provider returned 400 Bad Request")
            || err.contains("invalid_request_error")
        {
            // Unclassified, but recognisably deterministic: a gateway that
            // answers 200 with an error body lands here.
            return Err(TurnFailure::new(err, None, attempt).with_partial(got_delta));
        }

        if got_delta {
            // partial answer already streamed; a retry would duplicate it.
            // Say so plainly: the upstream text often ends in its own
            // "retry the request" advice, and the old suffix turned the
            // line into that advice contradicting itself.
            return Err(TurnFailure::new(
                format!("{err} — partial answer kept (a retry would duplicate it); ask to continue"),
                class,
                attempt,
            )
            .with_partial(true));
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
