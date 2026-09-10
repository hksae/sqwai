use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    Openai,
    Anthropic,
    Responses,
}

impl WireFormat {
    pub const ALL: [WireFormat; 3] = [
        WireFormat::Openai,
        WireFormat::Anthropic,
        WireFormat::Responses,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl<'de> Deserialize<'de> for EffortLevel {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = EffortLevel;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an effort level string or a bool")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                s: &str,
            ) -> std::result::Result<EffortLevel, E> {
                EffortLevel::from_str(s)
                    .ok_or_else(|| E::custom(format!("unknown effort level: {s}")))
            }
            // backward compatibility with the old `thinking = true/false`
            fn visit_bool<E: serde::de::Error>(
                self,
                b: bool,
            ) -> std::result::Result<EffortLevel, E> {
                Ok(if b {
                    EffortLevel::High
                } else {
                    EffortLevel::Off
                })
            }
        }
        d.deserialize_any(V)
    }
}

impl EffortLevel {
    /// all levels selectable from the status-bar `th:` menu
    pub const SELECTABLE: [EffortLevel; 5] = [
        EffortLevel::Off,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Max,
    ];
    pub const ALL: [EffortLevel; 5] = [
        EffortLevel::Off,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Max,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.as_str() == s)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What a model's API actually does with the effort slider.
///
/// The host cannot discover this: an endpoint that ignores `reasoning_effort`
/// answers exactly like one that honours it. So it is declared, and the
/// default is deliberately the conservative reading of the wire format rather
/// than a guess about a particular model — a claim we cannot verify is worse
/// than an admitted unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortControl {
    /// no reasoning control at all (local models, non-reasoning chat models)
    None,
    /// reasoning can be switched on and off, but not levelled (DeepSeek-like)
    Toggle,
    /// `low` / `medium` / `high`
    Levels,
    /// `low` / `medium` / `high` / `xhigh`, for models that document `xhigh`
    Xhigh,
    /// a numeric thinking budget, so every level is a real distinct request
    Budget,
}

impl EffortControl {
    pub const ALL: [EffortControl; 5] = [
        EffortControl::None,
        EffortControl::Toggle,
        EffortControl::Levels,
        EffortControl::Xhigh,
        EffortControl::Budget,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Toggle => "toggle",
            Self::Levels => "levels",
            Self::Xhigh => "xhigh",
            Self::Budget => "budget",
        }
    }

    /// what a wire format supports when the model says nothing more specific
    pub fn default_for(format: WireFormat) -> Self {
        match format {
            // budget_tokens is part of the Messages API, not of a model's
            // optional feature set
            WireFormat::Anthropic => EffortControl::Budget,
            // `reasoning_effort` / `reasoning.effort` are widely accepted and
            // widely ignored; assume the three documented levels and nothing
            // above them
            WireFormat::Openai | WireFormat::Responses => EffortControl::Levels,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub format: WireFormat,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// name of the env variable holding the key; overrides the implicit
    /// `<PROVIDER>_API_KEY` convention
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Continue from the provider's copy of the conversation when the wire
    /// format documents a reference for it, instead of resending the
    /// transcript. Cheaper — a measured 92% prompt-cache hit — but it hands
    /// the context to the provider, and `capabilities()` only knows what the
    /// format documents, not what the endpoint behind it actually keeps.
    /// Relays have been seen accepting the reference and storing nothing.
    #[serde(default = "default_continuation")]
    pub continuation: bool,
}

fn default_continuation() -> bool {
    true
}

impl ProviderConfig {
    /// env variable candidates for the key, explicit first
    fn env_candidates(&self, provider: &str) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(e) = &self.api_key_env {
            v.push(e.clone());
        }
        let conv = Config::conventional_env_name(provider);
        if !v.contains(&conv) {
            v.push(conv);
        }
        v
    }

    /// name of the env variable the key resolves from, if any
    pub fn key_env_name(&self, provider: &str) -> Option<String> {
        self.env_candidates(provider)
            .into_iter()
            .find(|n| std::env::var(n).ok().is_some_and(|v| !v.is_empty()))
    }

    /// effective key: inline value, else resolved from the environment
    pub fn effective_api_key(&self, provider: &str) -> Option<String> {
        if self.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
            return self.api_key.clone();
        }
        self.key_env_name(provider)
            .and_then(|n| std::env::var(n).ok())
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    /// model id sent in requests
    pub id: String,
    pub context: u64,
    /// how much work the user asks this model to spend; `thinking` is
    /// accepted as a legacy spelling of the same key
    #[serde(alias = "thinking")]
    pub effort: EffortLevel,
    /// what this model does with the slider; omitted means "whatever the wire
    /// format documents"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_control: Option<EffortControl>,
    /// true for models that always reason and cannot be told to stop, so that
    /// `off` is reported as unsupported instead of pretending to disable it
    #[serde(default, skip_serializing_if = "is_false")]
    pub effort_always_on: bool,
    /// $ per 1M input tokens (for the cost meter)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_in: Option<f64>,
    /// $ per 1M output tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_out: Option<f64>,
    /// Optional fallback model key (same or other provider)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// A model's declared effort behaviour, resolved against its wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffortSupport {
    pub control: EffortControl,
    /// the model always reasons; `off` cannot be honoured
    pub always_on: bool,
}

impl Default for EffortSupport {
    fn default() -> Self {
        Self {
            control: EffortControl::Levels,
            always_on: false,
        }
    }
}

impl ModelConfig {
    /// What this model does with the slider, falling back to the wire format
    /// and clamped to what that format can actually express: a declaration of
    /// `levels` on the Anthropic API, or of `budget` on an OpenAI one, would
    /// otherwise make the request carry nothing while the UI reported the
    /// level as applied.
    pub fn effort_support(&self, format: WireFormat) -> EffortSupport {
        let declared = self
            .effort_control
            .unwrap_or_else(|| EffortControl::default_for(format));
        let control = match (format, declared) {
            (WireFormat::Anthropic, EffortControl::Levels | EffortControl::Xhigh) => {
                EffortControl::Budget
            }
            (WireFormat::Openai | WireFormat::Responses, EffortControl::Budget) => {
                EffortControl::Levels
            }
            _ => declared,
        };
        EffortSupport {
            control,
            always_on: self.effort_always_on,
        }
    }
}

pub const BUILTIN_PROVIDERS_FALLBACK: &str = include_str!("../../builtin_providers.toml");

pub const BUILTIN_PROVIDERS_URL: &str =
    "https://raw.githubusercontent.com/hksae/sqwai/master/builtin_providers.toml";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuiltinCatalog {
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

pub fn builtin_cache_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("builtin_providers.toml"))
}

pub fn builtin_meta_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("builtin_providers_meta.json"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuiltinMeta {
    pub last_checked: String,
}

static BUILTIN_CACHE: std::sync::RwLock<Option<BuiltinCatalog>> = std::sync::RwLock::new(None);

impl BuiltinCatalog {
    pub fn current() -> Self {
        if let Ok(guard) = BUILTIN_CACHE.read()
            && let Some(catalog) = guard.as_ref()
        {
            return catalog.clone();
        }
        let catalog = Self::load_local();
        if let Ok(mut guard) = BUILTIN_CACHE.write() {
            *guard = Some(catalog.clone());
        }
        catalog
    }

    pub fn invalidate_cache() {
        if let Ok(mut guard) = BUILTIN_CACHE.write() {
            *guard = None;
        }
    }

    pub fn load_local() -> Self {
        #[cfg(test)]
        return toml::from_str(BUILTIN_PROVIDERS_FALLBACK).unwrap_or_default();
        #[allow(unreachable_code)]
        {
            let fallback: BuiltinCatalog =
                toml::from_str(BUILTIN_PROVIDERS_FALLBACK).unwrap_or_default();
            if let Ok(path) = builtin_cache_path()
                && let Ok(raw) = std::fs::read_to_string(&path)
                && let Ok(catalog) = toml::from_str::<BuiltinCatalog>(&raw)
                && catalog.updated_at >= fallback.updated_at
            {
                return catalog;
            }
            fallback
        }
    }
}

pub async fn check_and_update_builtins(force: bool) -> Result<Option<BuiltinCatalog>> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    if !force
        && let Ok(meta_path) = builtin_meta_path()
        && let Ok(raw) = std::fs::read_to_string(&meta_path)
        && let Ok(meta) = serde_json::from_str::<BuiltinMeta>(&raw)
        && meta.last_checked == today
    {
        return Ok(None);
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let res = client.get(BUILTIN_PROVIDERS_URL).send().await?;
    if !res.status().is_success() {
        anyhow::bail!("server returned status {}", res.status());
    }
    let text = res.text().await?;
    let catalog: BuiltinCatalog =
        toml::from_str(&text).context("parsing builtin providers TOML")?;

    if let Ok(cache_path) = builtin_cache_path() {
        let _ = atomic_write(&cache_path, &text);
    }
    if let Ok(meta_path) = builtin_meta_path()
        && let Ok(meta_json) = serde_json::to_string(&BuiltinMeta {
            last_checked: today,
        })
    {
        let _ = atomic_write(&meta_path, &meta_json);
    }
    BuiltinCatalog::invalidate_cache();
    Ok(Some(catalog))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub blocked_patterns: Vec<String>,
}

/// MCP server transport configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub transport: McpTransport,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerDef {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub language: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspConfig {
    #[serde(default)]
    pub servers: Vec<LspServerDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    #[serde(default = "default_true")]
    pub auto_load: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            auto_load: true,
        }
    }
}

/// runtime-tweakable ui behavior (`/debug`, `/theme`)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// reveal assistant text gradually instead of per-chunk
    #[serde(default = "default_true")]
    pub typewriter: bool,
    /// append failed request details to debug.log
    #[serde(default)]
    pub http_log: bool,
    /// index into tui::theme::THEMES
    #[serde(default)]
    pub theme: usize,
    /// show $ spent in the header (needs price_in/price_out on the model)
    #[serde(default)]
    pub show_cost: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            typewriter: true,
            http_log: false,
            theme: 0,
            show_cost: false,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_load_budget_ratio")]
    pub load_budget_ratio: f64,
    #[serde(default = "default_memory_heading_days")]
    pub heading_days: u8,
    #[serde(default = "default_memory_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_memory_max_proposals")]
    pub max_proposals_per_turn: u8,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            load_budget_ratio: default_memory_load_budget_ratio(),
            heading_days: default_memory_heading_days(),
            max_tokens: default_memory_max_tokens(),
            max_proposals_per_turn: default_memory_max_proposals(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiaryConfig {
    #[serde(default = "default_diary_token_budget")]
    pub token_budget: u32,
    /// Effort for the diary's own model call. §5.1 assigns effort per internal
    /// role and puts the diary writer at `low`; §2.3.4 describes the same call
    /// as running with effort off. The default keeps the cheaper reading — the
    /// entry is a summary of facts the host already extracted — and the key
    /// exists so the other one costs a line of config rather than a patch.
    #[serde(default)]
    pub effort: EffortLevel,
    #[serde(default = "default_diary_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_diary_batch_steps")]
    pub batch_steps: u8,
    #[serde(default = "default_diary_batch_minutes")]
    pub batch_minutes: u16,
}

/// `[undo]` — retention and storage for the two checkpoint layers (§2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoConfig {
    /// commits of a session's shadow chain to keep
    #[serde(default = "default_undo_keep_per_session")]
    pub keep_per_session: u32,
    /// above this many files, a pre-snapshot runs only for commands the
    /// classifier calls mutating
    #[serde(default = "default_undo_max_tree_files")]
    pub max_tree_files: u64,
    /// how long a layer-1 blob outlives the journal that references it
    #[serde(default = "default_undo_blob_grace_secs")]
    pub blob_grace_secs: u64,
    /// where the shadow repository lives; `off` disables layer 2 and leaves
    /// layer-1 file reverts working, which is the point of two layers
    #[serde(default = "default_undo_shadow")]
    pub shadow: ShadowStore,
    #[serde(default = "default_undo_shadow_max_bytes")]
    pub shadow_max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShadowStore {
    /// `.sqwai/checkpoints/git` inside the project
    #[default]
    Local,
    /// `~/.local/share/sqwai/checkpoints/<project-hash>/`
    User,
    /// no shadow repository; bash runs uninsured and says so
    Off,
}

fn default_undo_keep_per_session() -> u32 {
    50
}
fn default_undo_max_tree_files() -> u64 {
    100_000
}
fn default_undo_blob_grace_secs() -> u64 {
    86_400
}
fn default_undo_shadow() -> ShadowStore {
    ShadowStore::Local
}
fn default_undo_shadow_max_bytes() -> u64 {
    1_073_741_824
}

impl Default for UndoConfig {
    fn default() -> Self {
        Self {
            keep_per_session: default_undo_keep_per_session(),
            max_tree_files: default_undo_max_tree_files(),
            blob_grace_secs: default_undo_blob_grace_secs(),
            shadow: default_undo_shadow(),
            shadow_max_bytes: default_undo_shadow_max_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlanFirstMode {
    #[default]
    Soft,
    Off,
}

/// `[plan]` — the host's own limits on the structured plan (§5.9).
///
/// These are host values on purpose. The plan budget used to be computed from
/// a `context_limit` the model passed in its own tool arguments, which let it
/// raise its own ceiling and avoid the folding in §2.1.5.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PlanConfig {
    /// share of the model's context the injected plan may occupy
    #[serde(default = "default_plan_budget_ratio")]
    pub budget_ratio: f64,
    /// most steps a plan may hold before `add`/`split` are refused
    #[serde(default = "default_plan_max_steps")]
    pub max_steps: usize,
    /// actions attributed to a step before the tail block reminds the model
    /// to update the plan
    #[serde(default = "default_plan_nudge_after")]
    pub nudge_after: usize,
    /// plan-first gate: in Act mode, first mutating tool call without an active plan
    /// requires a plan first unless user message is heuristic-trivial
    #[serde(default)]
    pub plan_first: PlanFirstMode,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            budget_ratio: default_plan_budget_ratio(),
            max_steps: default_plan_max_steps(),
            nudge_after: default_plan_nudge_after(),
            plan_first: PlanFirstMode::default(),
        }
    }
}

/// `[secrets]` — files the host must not read into durable state (§2.3.6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretsConfig {
    /// glob patterns the project indexer skips entirely
    #[serde(default = "default_secrets_exclude_globs")]
    pub exclude_globs: Vec<String>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            exclude_globs: default_secrets_exclude_globs(),
        }
    }
}

impl PlanConfig {
    /// Token budget for the injected plan, from the model's context.
    pub fn budget_tokens(&self, context_limit: u64) -> u64 {
        ((context_limit as f64) * self.budget_ratio.clamp(0.0, 1.0)) as u64
    }
}

fn default_plan_budget_ratio() -> f64 {
    0.10
}

fn default_plan_max_steps() -> usize {
    24
}

fn default_plan_nudge_after() -> usize {
    8
}

fn default_secrets_exclude_globs() -> Vec<String> {
    [
        ".env*",
        "*.pem",
        "*.key",
        "id_*",
        "*credentials*",
        "*secret*",
    ]
    .iter()
    .map(|pattern| (*pattern).to_string())
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_compaction_threshold")]
    pub threshold: f64,
    #[serde(default = "default_compaction_stage_ratio")]
    pub stage_ratio: f64,
    #[serde(default = "default_compaction_keep_turns")]
    pub keep_turns: usize,
    #[serde(default = "default_compaction_anchor_ratio")]
    pub anchor_ratio: f64,
    #[serde(default)]
    pub summary: CompactionSummary,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CompactionSummary {
    #[default]
    Off,
    Short,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold: default_compaction_threshold(),
            stage_ratio: default_compaction_stage_ratio(),
            keep_turns: default_compaction_keep_turns(),
            anchor_ratio: default_compaction_anchor_ratio(),
            summary: CompactionSummary::default(),
        }
    }
}

impl Default for DiaryConfig {
    fn default() -> Self {
        Self {
            token_budget: default_diary_token_budget(),
            effort: EffortLevel::Off,
            timeout_secs: default_diary_timeout_secs(),
            batch_steps: default_diary_batch_steps(),
            batch_minutes: default_diary_batch_minutes(),
        }
    }
}

impl CompactionSummary {
    pub const ALL: [CompactionSummary; 2] = [CompactionSummary::Off, CompactionSummary::Short];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Short => "short",
        }
    }
}

impl ShadowStore {
    pub const ALL: [ShadowStore; 3] = [ShadowStore::Local, ShadowStore::User, ShadowStore::Off];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::User => "user",
            Self::Off => "off",
        }
    }
}

fn default_memory_load_budget_ratio() -> f64 {
    0.06
}
fn default_memory_heading_days() -> u8 {
    7
}
fn default_memory_max_tokens() -> u32 {
    3000
}
fn default_memory_max_proposals() -> u8 {
    2
}
fn default_diary_token_budget() -> u32 {
    1500
}
fn default_diary_timeout_secs() -> u64 {
    30
}
fn default_diary_batch_steps() -> u8 {
    3
}
fn default_diary_batch_minutes() -> u16 {
    20
}
fn default_compaction_threshold() -> f64 {
    0.80
}
fn default_compaction_stage_ratio() -> f64 {
    0.60
}
fn default_compaction_keep_turns() -> usize {
    4
}
fn default_compaction_anchor_ratio() -> f64 {
    0.08
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_model_name")]
    pub default_model: String,
    #[serde(default = "default_effort", alias = "default_thinking")]
    pub default_effort: EffortLevel,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub diary: DiaryConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub plan: PlanConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub undo: UndoConfig,
}

fn default_model_name() -> String {
    "gemini-3.8-flash".into()
}
fn default_effort() -> EffortLevel {
    EffortLevel::Medium
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    #[allow(dead_code)]
    pub name: String,
    pub format: WireFormat,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug)]
pub enum LoadError {
    Missing(PathBuf),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for LoadError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(p) => write!(f, "config not found at {}", p.display()),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for LoadError {}

impl Default for Config {
    fn default() -> Self {
        let catalog = BuiltinCatalog::current();
        Self {
            default_model: default_model_name(),
            default_effort: EffortLevel::Medium,
            providers: catalog.providers,
            models: catalog.models,
            safety: SafetyConfig::default(),
            ui: UiConfig::default(),
            mcp: McpConfig::default(),
            lsp: LspConfig::default(),
            skills: SkillsConfig::default(),
            memory: MemoryConfig::default(),
            diary: DiaryConfig::default(),
            compaction: CompactionConfig::default(),
            plan: PlanConfig::default(),
            secrets: SecretsConfig::default(),
            undo: UndoConfig::default(),
        }
    }
}

impl Config {
    pub fn is_builtin_provider(&self, name: &str) -> bool {
        BuiltinCatalog::current().providers.contains_key(name)
    }

    pub fn is_builtin_model(&self, key: &str) -> bool {
        BuiltinCatalog::current().models.contains_key(key)
    }

    /// Merge built-in providers/models into config. User key overrides are preserved;
    /// built-in models and formats are kept up-to-date with the catalog.
    pub fn apply_builtins(&mut self) {
        let catalog = BuiltinCatalog::current();
        for (name, p) in catalog.providers {
            if let Some(user_p) = self.providers.get_mut(&name) {
                user_p.base_url = p.base_url;
                user_p.format = p.format;
                if user_p.api_key_env.is_none() {
                    user_p.api_key_env = p.api_key_env;
                }
            } else {
                self.providers.insert(name, p);
            }
        }
        for (k, m) in catalog.models {
            self.models.insert(k, m);
        }
    }

    #[allow(dead_code)]
    pub fn ensure_seeds(&mut self) {
        self.apply_builtins();
    }
}

impl Config {
    pub fn load() -> std::result::Result<Self, LoadError> {
        let path = config_path()?;
        if !path.exists() {
            return Err(LoadError::Missing(path));
        }
        let raw = std::fs::read_to_string(&path).context("reading config")?;
        let mut cfg: Config = toml::from_str(&raw).map_err(|e| anyhow::anyhow!("{e}"))?;
        cfg.apply_builtins();
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        // unit tests must never touch the real user config
        #[cfg(test)]
        return Ok(());
        #[allow(unreachable_code)]
        {
            let path = config_path()?;
            let builtin = BuiltinCatalog::current();

            // Strip built-in models and unmodified built-in providers from user's config.toml
            let mut save_cfg = self.clone();
            save_cfg
                .models
                .retain(|k, _| !builtin.models.contains_key(k));
            save_cfg.providers.retain(|name, p| {
                if let Some(bp) = builtin.providers.get(name) {
                    p.api_key.is_some() || p.api_key_env != bp.api_key_env
                } else {
                    true
                }
            });

            atomic_write(
                &path,
                &toml::to_string_pretty(&save_cfg).context("serializing config")?,
            )?;
            Ok(())
        }
    }

    pub fn default_model_config(&self) -> Result<&ModelConfig> {
        self.models.get(&self.default_model).ok_or_else(|| {
            anyhow::anyhow!(
                "default_model {:?} not found in [models]",
                self.default_model
            )
        })
    }

    pub fn resolve_provider(&self, m: &ModelConfig) -> Result<ResolvedProvider> {
        let pc = self.providers.get(&m.provider).ok_or_else(|| {
            anyhow::anyhow!("model {:?}: provider {:?} not found", m.id, m.provider)
        })?;
        Ok(ResolvedProvider {
            name: m.provider.clone(),
            format: pc.format,
            base_url: pc.base_url.clone(),
            api_key: pc.effective_api_key(&m.provider),
        })
    }

    /// Resolve the chain of fallback models starting from `model_key`.
    /// Protects against cycles and returns an ordered list of resolved fallback model configs and providers.
    pub fn resolve_fallback_chain(&self, model_key: &str) -> Vec<(String, ModelConfig, ResolvedProvider)> {
        let mut chain = Vec::new();
        let mut visited = std::collections::BTreeSet::new();
        visited.insert(model_key.to_string());

        let mut current_key = model_key.to_string();
        while let Some(mc) = self.models.get(&current_key) {
            if let Some(ref next_key) = mc.fallback {
                if !visited.insert(next_key.clone()) {
                    // Cycle detected, break to prevent infinite fallback loop
                    break;
                }
                if let Some(next_mc) = self.models.get(next_key) {
                    if let Ok(resolved) = self.resolve_provider(next_mc) {
                        chain.push((next_key.clone(), next_mc.clone(), resolved));
                        current_key = next_key.clone();
                        continue;
                    }
                }
            }
            break;
        }
        chain
    }

    /// implicit env variable name for a provider: `open-router` -> `OPEN_ROUTER_API_KEY`
    pub fn conventional_env_name(provider: &str) -> String {
        let mut s = String::new();
        for ch in provider.chars() {
            if ch.is_ascii_alphanumeric() {
                s.extend(ch.to_uppercase());
            } else if !s.ends_with('_') && !s.is_empty() {
                s.push('_');
            }
        }
        while s.ends_with('_') {
            s.pop();
        }
        s.push_str("_API_KEY");
        s
    }
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "sqwai")
        .context("cannot determine platform config/data dirs")
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().to_path_buf())
}

fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, content).context("writing temp config")?;
    std::fs::rename(&tmp, path).context("installing config")?;
    Ok(())
}

pub fn write_template(path: &Path) -> Result<()> {
    let mut cfg = Config::default();
    let builtin = BuiltinCatalog::current();
    cfg.models.retain(|k, _| !builtin.models.contains_key(k));
    cfg.providers
        .retain(|name, _| !builtin.providers.contains_key(name));
    atomic_write(path, &toml::to_string_pretty(&cfg)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_template_has_resolvable_default_model() {
        let cfg = Config::default();
        assert!(!cfg.default_model.is_empty());
        assert!(cfg.default_model_config().is_ok());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_template(&path).unwrap();
        let mut loaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        loaded.apply_builtins();
        assert_eq!(loaded.default_model, "gemini-3.8-flash");
        assert!(loaded.default_model_config().is_ok());
    }

    /// The config example in the README must actually load.
    ///
    /// It did not: it used a `preset = "..."` key that does not exist and
    /// omitted `format` and `base_url`, which are required, so anyone who
    /// copied it got a parse error instead of an agent. Documentation that
    /// claims to be runnable should be run.
    #[test]
    fn readme_config_example_parses() {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"),
        )
        .expect("README.md");
        let block = readme
            .split("```toml")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("README has a ```toml block");
        let cfg: Config = toml::from_str(block)
            .unwrap_or_else(|e| panic!("README config example does not parse: {e}\n{block}"));
        assert!(
            cfg.providers.values().all(|p| !p.base_url.is_empty()),
            "every provider in the example needs a base_url"
        );
        assert!(
            cfg.models.contains_key(&cfg.default_model),
            "default_model {:?} must be one of the example's models",
            cfg.default_model
        );
    }

    #[test]
    fn effort_accepts_string_and_legacy_bool() {
        #[derive(Deserialize)]
        struct T {
            effort: EffortLevel,
        }
        let t: T = toml::from_str("effort = false").unwrap();
        assert_eq!(t.effort, EffortLevel::Off);
        let t: T = toml::from_str("effort = true").unwrap();
        assert_eq!(t.effort, EffortLevel::High);
        let t: T = toml::from_str("effort = \"max\"").unwrap();
        assert_eq!(t.effort, EffortLevel::Max);
    }

    /// The declaration is an override, not a requirement: a model that says
    /// nothing gets the conservative reading of its wire format.
    #[test]
    fn effort_support_falls_back_to_the_wire_format() {
        let mut m: ModelConfig = toml::from_str(
            "provider = \"anthropic\"\nid = \"claude-sonnet-5\"\ncontext = 1000\neffort = \"high\"\n",
        )
        .unwrap();
        assert_eq!(m.effort_control, None);
        assert_eq!(
            m.effort_support(WireFormat::Anthropic).control,
            EffortControl::Budget,
            "the Messages API expresses effort as a token budget"
        );
        assert_eq!(
            m.effort_support(WireFormat::Openai).control,
            EffortControl::Levels,
            "an unknown OpenAI-compatible endpoint must not be assumed to do more"
        );
        assert!(!m.effort_support(WireFormat::Openai).always_on);

        m.effort_control = Some(EffortControl::Xhigh);
        assert_eq!(
            m.effort_support(WireFormat::Openai).control,
            EffortControl::Xhigh,
            "a documented xhigh model must be able to say so"
        );
    }

    /// A declaration the wire format cannot express would otherwise make the
    /// request carry nothing while the UI called the level applied.
    #[test]
    fn a_declaration_is_clamped_to_what_the_format_can_express() {
        let mut m: ModelConfig = toml::from_str(
            "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"high\"\neffort_control = \"levels\"\n",
        )
        .unwrap();
        assert_eq!(
            m.effort_support(WireFormat::Anthropic).control,
            EffortControl::Budget
        );
        m.effort_control = Some(EffortControl::Budget);
        assert_eq!(
            m.effort_support(WireFormat::Openai).control,
            EffortControl::Levels
        );
        // shapes the format *can* express are left alone
        m.effort_control = Some(EffortControl::Toggle);
        assert_eq!(
            m.effort_support(WireFormat::Openai).control,
            EffortControl::Toggle
        );
    }

    #[test]
    fn effort_declaration_parses_and_round_trips() {
        let m: ModelConfig = toml::from_str(
            "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"max\"\n\
             effort_control = \"xhigh\"\neffort_always_on = true\n",
        )
        .unwrap();
        assert_eq!(m.effort_control, Some(EffortControl::Xhigh));
        assert!(m.effort_always_on);
        let back: ModelConfig = toml::from_str(&toml::to_string_pretty(&m).unwrap()).unwrap();
        assert_eq!(back.effort_control, m.effort_control);
        assert!(back.effort_always_on);

        // a model that declares nothing writes nothing
        let plain: ModelConfig =
            toml::from_str("provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"low\"\n")
                .unwrap();
        let text = toml::to_string_pretty(&plain).unwrap();
        assert!(!text.contains("effort_control"), "{text}");
        assert!(!text.contains("effort_always_on"), "{text}");
    }

    /// configs written before the slider was renamed say `thinking`; they must
    /// keep loading, including the `thinking = true/false` spelling that
    /// predates the levels.
    #[test]
    fn legacy_thinking_keys_still_load() {
        let m: ModelConfig = toml::from_str(
            "provider = \"anthropic\"\nid = \"claude-sonnet-5\"\ncontext = 1000000\nthinking = \"max\"\n",
        )
        .unwrap();
        assert_eq!(m.effort, EffortLevel::Max);
        let m: ModelConfig = toml::from_str(
            "provider = \"openai\"\nid = \"gpt-5.6\"\ncontext = 922000\nthinking = true\n",
        )
        .unwrap();
        assert_eq!(m.effort, EffortLevel::High);
        let cfg: Config = toml::from_str("default_thinking = \"low\"\n").unwrap();
        assert_eq!(cfg.default_effort, EffortLevel::Low);
    }

    #[test]
    fn config_roundtrip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.providers.len(), cfg.providers.len());
    }

    #[test]
    fn conventional_env_name_uppercases_provider() {
        assert_eq!(Config::conventional_env_name("openai"), "OPENAI_API_KEY");
        assert_eq!(
            Config::conventional_env_name("open-router"),
            "OPEN_ROUTER_API_KEY"
        );
        assert_eq!(Config::conventional_env_name("zen"), "ZEN_API_KEY");
    }

    #[test]
    fn api_key_resolves_from_env() {
        unsafe { std::env::set_var("SQWAI_TEST_PROVIDER_KEY", "explicit") };
        unsafe { std::env::set_var("SQWAI_TEST_CONV_API_KEY", "implicit") };
        let mk = |api_key: Option<&str>, api_key_env: Option<&str>| ProviderConfig {
            format: WireFormat::Openai,
            base_url: "http://x".into(),
            api_key: api_key.map(str::to_string),
            api_key_env: api_key_env.map(str::to_string),
            continuation: true,
        };
        // explicit env variable
        assert_eq!(
            mk(None, Some("SQWAI_TEST_PROVIDER_KEY")).effective_api_key("p"),
            Some("explicit".into())
        );
        // implicit <PROVIDER>_API_KEY convention
        assert_eq!(
            mk(None, None).effective_api_key("sqwai_test_conv"),
            Some("implicit".into())
        );
        // inline key wins over the environment
        assert_eq!(
            mk(Some("inline"), Some("SQWAI_TEST_PROVIDER_KEY")).effective_api_key("p"),
            Some("inline".into())
        );
    }

    #[test]
    fn builtins_merge_without_overwriting_user_edits() {
        let mut cfg = Config::default();
        for p in ["gemini", "anthropic", "openai", "deepseek", "grok", "kimi"] {
            assert!(cfg.providers.contains_key(p), "missing provider {p}");
            assert!(cfg.is_builtin_provider(p), "not marked builtin {p}");
        }
        assert!(cfg.is_builtin_model("gemini-3.8-flash"));
        assert!(cfg.is_builtin_model("deepseek-chat"));
        // auto-discovered providers resolve at least one model each; exact
        // revisions float with the catalog, so assert structurally, not by id
        for (provider, min_models) in [("anthropic", 5), ("openai", 5), ("grok", 4), ("kimi", 3)] {
            let count = cfg
                .models
                .values()
                .filter(|m| m.provider == provider)
                .count();
            assert!(
                count >= min_models,
                "provider {provider} has only {count} models"
            );
        }
        // retired IDs must be gone, not kept alongside
        for retired in [
            "claude-3-7-sonnet-20250219",
            "gpt-4o",
            "o1-mini",
            "grok-2-1212",
            "grok-beta",
            "moonshot-v1-8k",
            "kimi-latest",
        ] {
            assert!(
                !cfg.is_builtin_model(retired),
                "retired model {retired} still builtin"
            );
        }

        // user-defined model survives apply_builtins
        cfg.models.insert(
            "my-custom-model".into(),
            ModelConfig {
                provider: "custom".into(),
                id: "my-id".into(),
                context: 128000,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                price_in: None,
                price_out: None,
                fallback: None,
            },
        );
        cfg.apply_builtins();
        assert!(cfg.models.contains_key("my-custom-model"));
        assert!(!cfg.is_builtin_model("my-custom-model"));
    }

    #[test]
    fn test_model_config_fallback_and_plan_first_deserialization() {
        let toml_str = r#"
            [models.primary]
            provider = "openai"
            id = "gpt-4o"
            context = 128000
            effort = "off"
            fallback = "secondary"

            [models.secondary]
            provider = "anthropic"
            id = "claude-3-5-sonnet"
            context = 200000
            effort = "off"

            [plan]
            plan_first = "off"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.models["primary"].fallback.as_deref(), Some("secondary"));
        assert_eq!(cfg.models["secondary"].fallback, None);
        assert_eq!(cfg.plan.plan_first, PlanFirstMode::Off);

        // Test default plan_first is Soft
        let default_plan: PlanConfig = toml::from_str("").unwrap();
        assert_eq!(default_plan.plan_first, PlanFirstMode::Soft);
    }

    #[test]
    fn test_resolve_fallback_chain_handles_order_and_cycles() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p1".into(),
            ProviderConfig {
                format: WireFormat::Openai,
                base_url: "http://localhost:1".into(),
                api_key: Some("dummy".into()),
                api_key_env: None,
                continuation: true,
            },
        );
        cfg.providers.insert(
            "p2".into(),
            ProviderConfig {
                format: WireFormat::Anthropic,
                base_url: "http://localhost:2".into(),
                api_key: Some("dummy".into()),
                api_key_env: None,
                continuation: true,
            },
        );

        // A -> B -> C -> A (cycle)
        cfg.models.insert(
            "m_a".into(),
            ModelConfig {
                provider: "p1".into(),
                id: "id_a".into(),
                context: 1000,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                price_in: None,
                price_out: None,
                fallback: Some("m_b".into()),
            },
        );
        cfg.models.insert(
            "m_b".into(),
            ModelConfig {
                provider: "p2".into(),
                id: "id_b".into(),
                context: 2000,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                price_in: None,
                price_out: None,
                fallback: Some("m_c".into()),
            },
        );
        cfg.models.insert(
            "m_c".into(),
            ModelConfig {
                provider: "p1".into(),
                id: "id_c".into(),
                context: 3000,
                effort: EffortLevel::Off,
                effort_control: None,
                effort_always_on: false,
                price_in: None,
                price_out: None,
                fallback: Some("m_a".into()), // cycle back to A
            },
        );

        let chain = cfg.resolve_fallback_chain("m_a");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].0, "m_b");
        assert_eq!(chain[0].1.id, "id_b");
        assert_eq!(chain[1].0, "m_c");
        assert_eq!(chain[1].1.id, "id_c");
        // m_c fallback to m_a was not added due to cycle protection
    }
}
