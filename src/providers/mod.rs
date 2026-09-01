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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: Option<u64>,
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
    pub fn from_request(req: &ChatRequest) -> Self {
        let mut out = Self::default();
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// The provider accepts a documented server-side conversation reference.
    pub server_conversation: bool,
    /// The provider accepts a documented previous-response reference.
    pub previous_response: bool,
    /// The provider supports explicit prompt-cache controls.
    pub prompt_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTransport {
    Stateless,
    ServerConversation,
    PreviousResponse,
    PromptCached,
}

impl Default for ContextTransport {
    fn default() -> Self {
        Self::Stateless
    }
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

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model_id: String,
    pub messages: Vec<Message>,
    /// sent only when the model supports it; mapping per provider (phase 1)
    #[allow(dead_code)]
    pub thinking: Option<ThinkingLevel>,
    pub max_tokens: Option<u32>,
    /// tools available to the model this turn
    pub tools: Vec<ToolSpec>,
    /// Optional documented continuation reference. Providers must opt in.
    pub previous_response_id: Option<String>,
    /// Selected transport for this request; defaults to stateless.
    pub context_transport: ContextTransport,
}

pub trait Provider: Send + Sync {
    fn stream_chat(&self, req: ChatRequest) -> BoxStream<'static, StreamResult>;

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
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
