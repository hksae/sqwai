pub mod anthropic;
pub mod openai;
pub mod responses;

use std::sync::Arc;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::config::{ResolvedProvider, ThinkingLevel, WireFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// tool result (openai: role=tool; anthropic: user/tool_result block)
    Tool,
}

/// a completed request from the model to run a tool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallReq {
    pub id: String,
    pub name: String,
    /// parsed JSON arguments
    pub args: serde_json::Value,
}

/// static definition of a tool exposed to the model
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// assistant message requesting tool executions
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
    /// for Role::Tool: which call this result belongs to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: output.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            is_error,
        }
    }

    pub fn with_tool_calls(mut self, calls: Vec<ToolCallReq>) -> Self {
        self.tool_calls = calls;
        self
    }
}

/// Token counters reported by the provider for **one** request.
///
/// `prompt_tokens` is the size of that request, not a running total: summing it
/// over a session would multiply the history by the number of turns. Cumulative
/// accounting lives in [`crate::session::Session::usage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: Option<u64>,
}

impl Usage {
    /// tokens billed for this request
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    /// true when the provider told us nothing at all
    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.cached_tokens.unwrap_or(0) == 0
    }
}

/// One part of the system block.
///
/// The system block is **never** part of the conversation transcript: it is
/// rebuilt for every request and travels separately from `messages`. Parts
/// marked `cacheable` must stay byte-identical between requests — that stable
/// prefix is what a provider-side prefix cache can key on. Volatile parts
/// (current date, git state, project tree) are always appended last so they
/// cannot invalidate the prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemPart {
    pub text: String,
    pub cacheable: bool,
}

impl SystemPart {
    /// stable prefix: role, rules, project instructions, durable plan
    pub fn cached(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cacheable: true,
        }
    }

    /// re-read every turn: runtime context and other volatile facts
    pub fn volatile(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cacheable: false,
        }
    }
}

/// Render the system block as one string (providers with a single system field).
pub fn system_text(system: &[SystemPart]) -> String {
    system
        .iter()
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Approximate request composition for diagnostics. This is intentionally
/// provider-neutral: exact tokenization still belongs to the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestBreakdown {
    pub system_bytes: u64,
    pub history_bytes: u64,
    pub user_bytes: u64,
    pub tool_schema_bytes: u64,
    pub total_bytes: u64,
}

impl RequestBreakdown {
    #[allow(clippy::field_reassign_with_default)] // system_bytes is accumulated in a loop after the initial set
    pub fn from_request(req: &ChatRequest) -> Self {
        let mut out = Self::default();
        out.system_bytes = req.system.iter().map(|p| p.text.len() as u64).sum::<u64>();
        for message in &req.messages {
            let bytes = message.content.len() as u64
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.len() as u64 + call.args.to_string().len() as u64)
                    .sum::<u64>();
            match message.role {
                Role::System => out.system_bytes += bytes,
                Role::User => out.user_bytes += bytes,
                Role::Assistant | Role::Tool => out.history_bytes += bytes,
            }
        }
        out.tool_schema_bytes = req
            .tools
            .iter()
            .map(|tool| {
                (tool.name.len() + tool.description.len()) as u64
                    + tool.parameters.to_string().len() as u64
            })
            .sum();
        out.total_bytes =
            out.system_bytes + out.history_bytes + out.user_bytes + out.tool_schema_bytes;
        out
    }
}

/// What a provider is actually documented to support.
///
/// Nothing here is inferred from "most servers do X": a capability is either
/// written down by the provider or it is false. Prompt caching additionally has
/// an observed side ([`crate::session::Session::cache_confirmed`]) — a
/// documented cache key only becomes real once the provider reports
/// `cached_tokens` back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The provider accepts a documented server-side conversation reference.
    pub server_conversation: bool,
    /// The provider accepts a documented previous-response reference.
    pub previous_response: bool,
    /// The provider documents a prompt-cache mechanism we can address
    /// (e.g. Anthropic `cache_control` breakpoints). Automatic prefix caching
    /// that we cannot address or verify does not count.
    pub prompt_cache_documented: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextTransport {
    /// the whole transcript is resent every request
    #[default]
    Stateless,
    /// the provider owns the conversation and we only send deltas
    ServerConversation,
    /// continuation via a documented previous-response reference
    PreviousResponse,
}

/// Pick the transport for a request. Only a documented continuation reference
/// may shorten the local transcript; otherwise everything is resent.
pub fn select_transport(
    caps: ProviderCapabilities,
    previous_response_id: Option<&str>,
) -> ContextTransport {
    if previous_response_id.is_some() && caps.previous_response {
        return ContextTransport::PreviousResponse;
    }
    if caps.server_conversation {
        return ContextTransport::ServerConversation;
    }
    ContextTransport::Stateless
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    Usage(Usage),
    /// Provider-native response identifier, when the protocol exposes one.
    ResponseId(String),
    /// the model finished a request to run a tool (arguments are complete)
    ToolCall(ToolCallReq),
}

pub type StreamResult = anyhow::Result<StreamEvent>;

/// Why a provider request failed (§5.1).
///
/// Decided from the response — status and body — and attached to the error, so
/// the retry policy is a decision about a class rather than a substring match
/// on an error message. "Retry for an hour" and "give up now" are opposite
/// answers, and telling them apart from prose does not work: a 401 and a 503
/// both read as "provider returned N: ...".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// bad or missing key — waiting cannot help
    Auth,
    /// out of credit or over a hard quota — waiting cannot help either
    Quota,
    /// rate limited; the whole point of backing off
    RateLimit,
    /// the request does not fit the context window; compaction can help
    ContextOverflow,
    /// malformed or unsupported request — deterministic, never retry
    BadRequest,
    /// provider-side failure, worth retrying
    Server,
    /// the request never got an answer, worth retrying
    Network,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::RateLimit => "rate_limit",
            Self::ContextOverflow => "context_overflow",
            Self::BadRequest => "bad_request",
            Self::Server => "server",
            Self::Network => "network",
        }
    }

    /// Whether waiting and asking again can plausibly succeed.
    pub fn retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Server | Self::Network)
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify an HTTP response from its status and body.
pub fn classify_response(status: u16, body: &str) -> ErrorClass {
    let lower = body.to_ascii_lowercase();
    // Providers disagree on the status for "your prompt is too long": OpenAI
    // uses 400 with a typed code, Anthropic 400 with prose, some gateways 413.
    let overflow = [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "reduce the length",
        "exceed context limit",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let out_of_credit = [
        "insufficient_quota",
        "insufficient credit",
        "credit balance",
        "billing",
        "exceeded your current quota",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    match status {
        401 | 403 => ErrorClass::Auth,
        402 => ErrorClass::Quota,
        413 => ErrorClass::ContextOverflow,
        429 => {
            // A 429 that is really "you have no money" never clears on its own.
            if out_of_credit {
                ErrorClass::Quota
            } else {
                ErrorClass::RateLimit
            }
        }
        s if s >= 500 => ErrorClass::Server,
        _ if overflow => ErrorClass::ContextOverflow,
        _ if out_of_credit => ErrorClass::Quota,
        _ => ErrorClass::BadRequest,
    }
}

/// The class attached to a provider error, when there is one.
pub fn class_of(error: &anyhow::Error) -> Option<ErrorClass> {
    // anyhow keeps context objects downcastable, which is the point of
    // attaching the class as context rather than formatting it into the text.
    error.downcast_ref::<ErrorClass>().copied()
}

/// Build a classified provider error for a failed HTTP response.
pub fn response_error(status: u16, body: &str) -> anyhow::Error {
    let class = classify_response(status, body);
    anyhow::anyhow!("provider returned {status}: {body}").context(class)
}

/// Build a classified provider error for a request that never completed.
pub fn network_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("request failed: {error}").context(ErrorClass::Network)
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model_id: String,
    /// system block for this request; never stored in the session transcript
    pub system: Vec<SystemPart>,
    /// conversation history (user / assistant / tool only)
    pub messages: Vec<Message>,
    /// sent only when the model supports it; mapping per provider (phase 1)
    #[allow(dead_code)]
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<u32>,
    /// tools available to the model this turn (empty = no tool support needed)
    pub tools: Vec<ToolSpec>,
    /// Optional documented continuation reference. Providers must opt in.
    pub previous_response_id: Option<String>,
    /// Selected transport for this request; defaults to stateless.
    pub context_transport: ContextTransport,
}

impl ChatRequest {
    /// true when this request may call tools
    pub fn tool_capable(&self) -> bool {
        !self.tools.is_empty()
    }
}

pub trait Provider: Send + Sync {
    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult>;

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Drop every request field this provider has no documented support for.
    ///
    /// Chat Completions has no continuation field at all, so an OpenAI
    /// compatible gateway must never see `previous_response_id` — even one
    /// silently ignored today can become a 400 after a server update.
    fn sanitize(&self, req: &mut ChatRequest) {
        let caps = self.capabilities();
        if !caps.previous_response {
            req.previous_response_id = None;
        }
        if req.context_transport == ContextTransport::ServerConversation
            && !caps.server_conversation
        {
            req.context_transport = ContextTransport::Stateless;
        }
    }
}

pub type SharedProvider = Arc<dyn Provider>;

use std::sync::atomic::{AtomicBool, Ordering};

static HTTP_LOG: AtomicBool = AtomicBool::new(false);

/// enable/disable the request debug log (`/debug` menu, persisted in `[ui]`)
pub fn set_http_log(on: bool) {
    HTTP_LOG.store(on, Ordering::Relaxed);
}

/// append one line to `debug.log` next to the config when logging is enabled
pub fn log_http(msg: &str) {
    use std::io::Write;
    if !HTTP_LOG.load(Ordering::Relaxed) {
        return;
    }
    let Ok(dir) = crate::config::data_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("debug.log"))
    else {
        return;
    };
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let _ = writeln!(f, "[{ts}] {msg}");
}

pub fn create(p: &ResolvedProvider) -> anyhow::Result<SharedProvider> {
    match p.format {
        WireFormat::Openai => Ok(Arc::new(openai::OpenAiProvider::new(p)?)),
        WireFormat::Anthropic => Ok(Arc::new(anthropic::AnthropicProvider::new(p)?)),
        WireFormat::Responses => Ok(Arc::new(responses::ResponsesProvider::new(p)?)),
    }
}

#[cfg(test)]
mod error_class_tests {
    use super::*;

    /// The retry policy is a decision about a class. These are the cases where
    /// retrying is wrong, and they used to be retried for an hour because the
    /// policy matched on the words of an error message.
    #[test]
    fn hopeless_failures_are_not_retryable() {
        for (status, body, expected) in [
            (401u16, "invalid x-api-key", ErrorClass::Auth),
            (403, "forbidden", ErrorClass::Auth),
            (402, "payment required", ErrorClass::Quota),
            (
                429,
                r#"{"error":{"type":"insufficient_quota","message":"You exceeded your current quota"}}"#,
                ErrorClass::Quota,
            ),
            (
                400,
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                ErrorClass::ContextOverflow,
            ),
            (
                400,
                "prompt is too long: 210000 tokens",
                ErrorClass::ContextOverflow,
            ),
            (413, "payload too large", ErrorClass::ContextOverflow),
            (
                400,
                "invalid_request_error: unknown field",
                ErrorClass::BadRequest,
            ),
        ] {
            let class = classify_response(status, body);
            assert_eq!(class, expected, "status {status}, body {body:?}");
            assert!(
                !class.retryable(),
                "{class} must not be retried: {status} {body:?}"
            );
        }
    }

    /// And the cases where waiting is exactly right.
    #[test]
    fn transient_failures_are_retryable() {
        for (status, body, expected) in [
            (429u16, "slow down", ErrorClass::RateLimit),
            (500, "internal server error", ErrorClass::Server),
            (502, "bad gateway", ErrorClass::Server),
            (529, "overloaded_error", ErrorClass::Server),
        ] {
            let class = classify_response(status, body);
            assert_eq!(class, expected, "status {status}");
            assert!(class.retryable(), "{class} must be retried: {status}");
        }
    }

    /// A rate limit that is really "out of credit" never clears on its own, so
    /// it must not be treated as one.
    #[test]
    fn a_429_about_credit_is_a_quota_failure() {
        assert_eq!(
            classify_response(429, "Your credit balance is too low to access the API"),
            ErrorClass::Quota
        );
        assert_eq!(
            classify_response(429, "rate_limit_error"),
            ErrorClass::RateLimit
        );
    }

    /// The class has to survive the trip through anyhow, otherwise the caller
    /// is back to reading error text.
    #[test]
    fn the_class_survives_the_error_chain() {
        let error = response_error(401, "invalid x-api-key");
        assert_eq!(class_of(&error), Some(ErrorClass::Auth));
        // and the message a human reads is still the provider's own
        assert!(format!("{error:#}").contains("invalid x-api-key"));

        let network = network_error("connection reset by peer");
        assert_eq!(class_of(&network), Some(ErrorClass::Network));
        assert!(class_of(&network).unwrap().retryable());

        // an error with no class attached must not be mistaken for one
        assert_eq!(class_of(&anyhow::anyhow!("something else")), None);
    }
}
