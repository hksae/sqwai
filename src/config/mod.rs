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
    Xhigh,
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
    pub const SELECTABLE: [EffortLevel; 6] = [
        EffortLevel::Off,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Xhigh,
        EffortLevel::Max,
    ];
    pub const ALL: [EffortLevel; 6] = [
        EffortLevel::Off,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Xhigh,
        EffortLevel::Max,
    ];
    /// Option strings for pick-one UI (model edit form, etc.): derived from
    /// the same levels, so the UI can never lag behind a new level again.
    pub const STRS: [&'static str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
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
    /// the level name goes on the wire literally (`low`/`medium`/`high`/
    /// `xhigh`/`max`): transparent pass-through. If the endpoint does not
    /// know a name it answers 400 and the user picks another level.
    /// `levels`/`xhigh` are accepted as legacy spellings of the same thing.
    #[serde(alias = "levels", alias = "xhigh")]
    Named,
    /// a numeric thinking budget, so every level is a real distinct request
    Budget,
}

impl EffortControl {
    pub const ALL: [EffortControl; 4] = [
        EffortControl::None,
        EffortControl::Toggle,
        EffortControl::Named,
        EffortControl::Budget,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Toggle => "toggle",
            Self::Named => "named",
            Self::Budget => "budget",
        }
    }

    /// Option strings for pick-one UI, same order as `ALL`.
    pub const STRS: [&'static str; 4] = ["none", "toggle", "named", "budget"];

    pub fn from_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == s)
    }

    /// what a wire format supports when the model says nothing more specific
    pub fn default_for(format: WireFormat) -> Self {
        match format {
            // budget_tokens is part of the Messages API, not of a model's
            // optional feature set
            WireFormat::Anthropic => EffortControl::Budget,
            // `reasoning_effort` / `reasoning.effort` take a level name;
            // it goes on the wire literally (see `Named`)
            WireFormat::Openai | WireFormat::Responses => EffortControl::Named,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            control: EffortControl::Named,
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
            (WireFormat::Anthropic, EffortControl::Named) => EffortControl::Budget,
            (WireFormat::Openai | WireFormat::Responses, EffortControl::Budget) => {
                EffortControl::Named
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

    // No-op when the content is identical: the meta stamp still advances
    // (so tomorrow's launch skips the fetch), but the caller gets Ok(None)
    // — no rewrite, no apply_builtins pass, no "updated" noise.
    if let Ok(cache_path) = builtin_cache_path()
        && let Ok(cached) = std::fs::read_to_string(&cache_path)
        && cached == text
    {
        if let Ok(meta_path) = builtin_meta_path()
            && let Ok(meta_json) = serde_json::to_string(&BuiltinMeta {
                last_checked: today,
            })
        {
            let _ = atomic_write(&meta_path, &meta_json);
        }
        return Ok(None);
    }

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
    /// unlock /test ... commands (off by default, /settings -> Experimental)
    #[serde(default)]
    pub experimental_test: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            typewriter: true,
            http_log: false,
            experimental_test: false,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyConfig {
    /// named check commands for plan acceptance (`cmd: $name` substitutes
    /// on `plan create`). Project-scoped; seeded by `/init`, edited by hand.
    #[serde(default)]
    pub commands: std::collections::BTreeMap<String, String>,
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
    /// fraction of the context at which compaction triggers (whichever
    /// bites first with the answer reserve, see Policy::budget)
    #[serde(default = "default_compaction_threshold")]
    pub threshold: f64,
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

/// Project-level overrides (`.sqwai/config.toml`, §5.9).
///
/// A cloned repo must never be able to reconfigure trust: providers, models,
/// keys, safety, plan gates, MCP/LSP servers, and skills-dirs are NOT
/// allowlisted and are ignored with a warning. Only display- and
/// budget-class keys are accepted; everything else in the file is reported
/// back so a silently-ignored setting cannot confuse. The whole file is
/// rejected on parse error (fail-closed).
#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectOverrides {
    #[serde(default)]
    diary: DiaryOverride,
    #[serde(default)]
    ui: UiOverride,
    #[serde(default)]
    compaction: CompactionOverride,
    #[serde(default)]
    undo: UndoOverride,
    #[serde(default)]
    plan: PlanOverride,
    #[serde(default)]
    verify: VerifyOverride,
    #[serde(default)]
    memory: MemoryOverride,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DiaryOverride {
    token_budget: Option<u32>,
    effort: Option<EffortLevel>,
    timeout_secs: Option<u64>,
    batch_steps: Option<u8>,
    batch_minutes: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UiOverride {
    typewriter: Option<bool>,
    http_log: Option<bool>,
    experimental_test: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CompactionOverride {
    threshold: Option<f64>,
    keep_turns: Option<usize>,
    anchor_ratio: Option<f64>,
    summary: Option<CompactionSummary>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UndoOverride {
    keep_per_session: Option<u32>,
    blob_grace_secs: Option<u64>,
    shadow: Option<ShadowStore>,
    shadow_max_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PlanOverride {
    budget_ratio: Option<f64>,
    max_steps: Option<usize>,
    nudge_after: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VerifyOverride {
    commands: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MemoryOverride {
    load_budget_ratio: Option<f64>,
    max_tokens: Option<u32>,
    max_proposals_per_turn: Option<u8>,
}

/// (table, keys) accepted from a project file. Anything else present is
/// ignored and reported. Keep in sync with the `*Override` structs above.
const PROJECT_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "diary",
        &[
            "token_budget",
            "effort",
            "timeout_secs",
            "batch_steps",
            "batch_minutes",
        ],
    ),
    ("ui", &["typewriter", "http_log", "experimental_test"]),
    (
        "compaction",
        &["threshold", "keep_turns", "anchor_ratio", "summary"],
    ),
    (
        "undo",
        &[
            "keep_per_session",
            "blob_grace_secs",
            "shadow",
            "shadow_max_bytes",
        ],
    ),
    ("plan", &["budget_ratio", "max_steps", "nudge_after"]),
    ("verify", &["commands"]),
    (
        "memory",
        &["load_budget_ratio", "max_tokens", "max_proposals_per_turn"],
    ),
];

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
fn default_compaction_keep_turns() -> usize {
    4
}
fn default_compaction_anchor_ratio() -> f64 {
    0.08
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// model new sessions open with: always the last one used, never a
    /// separate "default". Updated on every model switch; migrated from
    /// legacy `default_model` on load; first catalog model on first run.
    /// Empty when absent from the file — that is what triggers migration.
    #[serde(default)]
    pub last_model: String,
    /// legacy name for `last_model`; read on load, never written back
    #[serde(default, skip_serializing, rename = "default_model")]
    pub(crate) legacy_default_model: String,
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
    pub verify: VerifyConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub undo: UndoConfig,
}

fn first_builtin_model() -> String {
    BuiltinCatalog::current()
        .models
        .keys()
        .next()
        .cloned()
        .unwrap_or_default()
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
            last_model: first_builtin_model(),
            legacy_default_model: String::new(),
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
            verify: VerifyConfig::default(),
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

    /// Merge built-in providers/models into config. Provider endpoints stay
    /// fresh (base_url/format sync); user key overrides are preserved.
    /// Models are user-owned by key: catalog entries fill in absent keys
    /// only, never overwrite. A customized built-in model survives because
    /// `save` persists it (see below).
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
            self.models.entry(k).or_insert(m);
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
        cfg.migrate_last_model();
        Ok(cfg)
    }

    /// Resolve which model new sessions open with: explicit `last_model`
    /// wins; legacy `default_model` migrates once; otherwise the first
    /// catalog model (first run). A dangling reference (model deleted
    /// since) falls back the same way instead of failing the load.
    fn migrate_last_model(&mut self) {
        if self.last_model.is_empty() || !self.models.contains_key(&self.last_model) {
            if !self.legacy_default_model.is_empty()
                && self.models.contains_key(&self.legacy_default_model)
            {
                self.last_model = std::mem::take(&mut self.legacy_default_model);
            } else {
                self.last_model = first_builtin_model();
            }
        }
        self.legacy_default_model.clear();
    }

    pub fn save(&self) -> Result<()> {
        // unit tests must never touch the real user config
        #[cfg(test)]
        return Ok(());
        #[allow(unreachable_code)]
        {
            let path = config_path()?;
            let save_cfg = self.without_pristine_builtins();

            atomic_write(
                &path,
                &toml::to_string_pretty(&save_cfg).context("serializing config")?,
            )?;
            Ok(())
        }
    }

    /// Copy of this config with pristine built-in entries removed, for
    /// persisting: untouched catalog models/providers stay out of the user
    /// file, customized ones stay in. Pure so tests can cover it (`save`
    /// itself never touches disk under `cfg(test)`).
    pub fn without_pristine_builtins(&self) -> Self {
        let builtin = BuiltinCatalog::current();
        let mut save_cfg = self.clone();
        save_cfg
            .models
            .retain(|k, m| builtin.models.get(k).is_none_or(|catalog_m| catalog_m != m));
        save_cfg.providers.retain(|name, p| {
            if let Some(bp) = builtin.providers.get(name) {
                p.api_key.is_some() || p.api_key_env != bp.api_key_env
            } else {
                true
            }
        });
        save_cfg
    }

    /// Overlay `.sqwai/config.toml` project overrides. Returns human-readable
    /// notes for everything ignored (unknown tables/keys, whole-file parse
    /// failure). A missing file is not an error and yields no notes.
    pub fn apply_project_overrides(&mut self, root: &Path) -> Vec<String> {
        let mut notes = Vec::new();
        let path = root.join(".sqwai").join("config.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return notes,
            Err(e) => {
                notes.push(format!(
                    "project config {} unreadable ({e}); ignoring",
                    path.display()
                ));
                return notes;
            }
        };
        let value: toml::Value = match toml::from_str(&raw) {
            Ok(value) => value,
            Err(e) => {
                notes.push(format!(
                    "project config {} does not parse ({e}); ignoring",
                    path.display()
                ));
                return notes;
            }
        };
        // report-then-ignore everything outside the allowlist, so a cloned
        // repo can neither reconfigure trust nor confuse by silently
        // dropping a setting the user thinks is active
        if let Some(table) = value.as_table() {
            for (section, keys) in table {
                match PROJECT_ALLOWLIST
                    .iter()
                    .find(|(allowed, _)| allowed == section)
                {
                    None => notes.push(format!(
                        "project config [{}] is not overridable; ignoring",
                        section
                    )),
                    Some((_, allowed_keys)) => {
                        if let Some(entries) = keys.as_table() {
                            for key in entries.keys() {
                                if !allowed_keys.contains(&key.as_str()) {
                                    notes.push(format!(
                                        "project config {section}.{key} is not overridable; ignoring"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        let overrides: ProjectOverrides = match toml::from_str(&raw) {
            Ok(overrides) => overrides,
            Err(e) => {
                notes.push(format!(
                    "project config {} has invalid values ({e}); ignoring",
                    path.display()
                ));
                return notes;
            }
        };
        let diary = &mut self.diary;
        let o = &overrides.diary;
        if let Some(v) = o.token_budget {
            diary.token_budget = v;
        }
        if let Some(v) = o.effort {
            diary.effort = v;
        }
        if let Some(v) = o.timeout_secs {
            diary.timeout_secs = v;
        }
        if let Some(v) = o.batch_steps {
            diary.batch_steps = v;
        }
        if let Some(v) = o.batch_minutes {
            diary.batch_minutes = v;
        }
        let ui = &mut self.ui;
        let o = &overrides.ui;
        if let Some(v) = o.typewriter {
            ui.typewriter = v;
        }
        if let Some(v) = o.http_log {
            ui.http_log = v;
        }
        if let Some(v) = o.experimental_test {
            ui.experimental_test = v;
        }
        let compaction = &mut self.compaction;
        let o = &overrides.compaction;
        if let Some(v) = o.threshold {
            compaction.threshold = v;
        }
        if let Some(v) = o.keep_turns {
            compaction.keep_turns = v;
        }
        if let Some(v) = o.anchor_ratio {
            compaction.anchor_ratio = v;
        }
        if let Some(v) = o.summary {
            compaction.summary = v;
        }
        let undo = &mut self.undo;
        let o = &overrides.undo;
        if let Some(v) = o.keep_per_session {
            undo.keep_per_session = v;
        }
        if let Some(v) = o.blob_grace_secs {
            undo.blob_grace_secs = v;
        }
        if let Some(v) = o.shadow {
            undo.shadow = v;
        }
        if let Some(v) = o.shadow_max_bytes {
            undo.shadow_max_bytes = v;
        }
        let plan = &mut self.plan;
        let o = &overrides.plan;
        if let Some(v) = o.budget_ratio {
            plan.budget_ratio = v;
        }
        if let Some(v) = o.max_steps {
            plan.max_steps = v;
        }
        if let Some(v) = o.nudge_after {
            plan.nudge_after = v;
        }
        let verify = &mut self.verify;
        let o = &overrides.verify;
        if let Some(v) = o.commands.clone() {
            verify.commands.extend(v);
        }
        let memory = &mut self.memory;

        let o = &overrides.memory;
        if let Some(v) = o.load_budget_ratio {
            memory.load_budget_ratio = v;
        }
        if let Some(v) = o.max_tokens {
            memory.max_tokens = v;
        }
        if let Some(v) = o.max_proposals_per_turn {
            memory.max_proposals_per_turn = v;
        }
        notes
    }

    /// Named verify commands for a project, read tolerantly straight from
    /// `.sqwai/config.toml` (`[verify] commands`). Used at `plan create`
    /// without threading the whole Config through dispatch. Anything
    /// unreadable or misshapen yields an empty map — absence just means
    /// no names (the allowlist still reports a bad file on load).
    pub fn project_verify_commands(root: &Path) -> std::collections::BTreeMap<String, String> {
        let Ok(raw) = std::fs::read_to_string(root.join(".sqwai").join("config.toml")) else {
            return Default::default();
        };
        toml::from_str::<ProjectOverrides>(&raw)
            .ok()
            .and_then(|o| o.verify.commands)
            .unwrap_or_default()
    }

    /// Detect check commands for `/init` seeding. Deterministic repo
    /// probing first (a file that exists beats a guess), then `verify:`
    /// lines from the project MEMORY.md (curated truth wins name clashes).
    /// Returns (name, command, source) for reporting; pure except reads.
    pub fn detect_verify_commands(root: &Path) -> Vec<(String, String, &'static str)> {
        let mut found: Vec<(String, String, &'static str)> = Vec::new();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut offer = |name: &str, cmd: String, source: &'static str| {
            if seen.insert(name.to_string()) {
                found.push((name.to_string(), cmd, source));
            }
        };
        if root.join("Cargo.toml").is_file() {
            offer("test", "cargo test".to_string(), "Cargo.toml");
        }
        if let Ok(makefile) = std::fs::read_to_string(root.join("Makefile")) {
            let mut targets: Vec<String> = Vec::new();
            for line in makefile.lines() {
                let line = line.trim_end();
                if line.starts_with('.') || line.starts_with('#') || line.starts_with('\t') {
                    continue;
                }
                if let Some((lhs, _)) = line.split_once(':') {
                    let name = lhs.trim();
                    if name.starts_with("test") && !name.contains([' ', '=']) && !name.is_empty() {
                        targets.push(name.to_string());
                    }
                }
            }
            targets.sort();
            let pick = targets
                .iter()
                .find(|t| t.as_str() == "test")
                .or_else(|| targets.first());
            if let Some(target) = pick {
                offer("test", format!("make {target}"), "Makefile");
            }
        }
        if let Ok(pkg) = std::fs::read_to_string(root.join("package.json"))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&pkg)
            && v.pointer("/scripts/test")
                .and_then(|s| s.as_str())
                .is_some()
        {
            offer("test", "npm test".to_string(), "package.json");
        }
        if root.join("pytest.ini").is_file()
            || root.join("tox.ini").is_file()
            || std::fs::read_to_string(root.join("pyproject.toml"))
                .map(|t| t.contains("[tool.pytest"))
                .unwrap_or(false)
        {
            offer("test", "pytest".to_string(), "pytest config");
        }
        if root.join("go.mod").is_file() {
            offer("test", "go test ./...".to_string(), "go.mod");
        }
        // curated project memory overrides probing on name clashes
        if let Ok(memory) =
            std::fs::read_to_string(root.join(".sqwai").join("memory").join("MEMORY.md"))
        {
            for line in memory.lines() {
                let t = line.trim();
                let Some(rest) = t.strip_prefix("verify:") else {
                    continue;
                };
                let Some((name, cmd)) = rest.split_once('=') else {
                    continue;
                };
                let (name, cmd) = (name.trim(), cmd.trim());
                if name.is_empty() || cmd.is_empty() {
                    continue;
                }
                if let Some(slot) = found.iter_mut().find(|(n, _, _)| n == name) {
                    slot.1 = cmd.to_string();
                    slot.2 = "MEMORY.md";
                } else {
                    seen.insert(name.to_string());
                    found.push((name.to_string(), cmd.to_string(), "MEMORY.md"));
                }
            }
        }
        found
    }

    /// Merge detected commands into `.sqwai/config.toml` (`[verify]`).
    /// Hand-written names always win: existing keys are never overwritten,
    /// only missing ones are added. Returns (added, already_there) for the
    /// status message. Writes the file only when something was added.
    pub fn seed_verify_commands(root: &Path) -> (Vec<(String, String)>, usize) {
        let detected = Self::detect_verify_commands(root);
        if detected.is_empty() {
            return (Vec::new(), 0);
        }
        let dir = root.join(".sqwai");
        let path = dir.join("config.toml");
        let mut value: toml::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or(toml::Value::Table(Default::default()));
        let table = match value.as_table_mut() {
            Some(t) => t,
            None => return (Vec::new(), 0),
        };
        let verify = table
            .entry("verify".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let Some(verify_table) = verify.as_table_mut() else {
            // hostile shape (`verify = 5`): leave the file alone, report nothing
            return (Vec::new(), 0);
        };
        let commands = verify_table
            .entry("commands".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let Some(map) = commands.as_table_mut() else {
            return (Vec::new(), 0);
        };
        let mut added = Vec::new();
        for (name, cmd, _) in &detected {
            if !map.contains_key(name) {
                map.insert(name.clone(), toml::Value::String(cmd.clone()));
                added.push((name.clone(), cmd.clone()));
            }
        }
        let already = map.len().saturating_sub(added.len());
        if !added.is_empty() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(text) = toml::to_string(&value) {
                let _ = std::fs::write(&path, text);
            }
        }
        (added, already)
    }

    pub fn last_model_config(&self) -> Result<&ModelConfig> {
        self.models.get(&self.last_model).ok_or_else(|| {
            anyhow::anyhow!("last_model {:?} not found in [models]", self.last_model)
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
    pub fn resolve_fallback_chain(
        &self,
        model_key: &str,
    ) -> Vec<(String, ModelConfig, ResolvedProvider)> {
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
                if let Some(next_mc) = self.models.get(next_key)
                    && let Ok(resolved) = self.resolve_provider(next_mc)
                {
                    chain.push((next_key.clone(), next_mc.clone(), resolved));
                    current_key = next_key.clone();
                    continue;
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
    fn detect_verify_commands_probes_repo_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // repo probing: cargo + make (test preferred over test-all) + npm
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("Makefile"), "test-all:\n\tt\ntest:\n\tt\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts": {"test": "jest"}}"#,
        )
        .unwrap();
        let found = Config::detect_verify_commands(root);
        let get = |n: &str| found.iter().find(|(k, _, _)| k == n).cloned();
        // first source wins the name; cargo probed before make/npm
        assert_eq!(
            get("test").map(|(_, c, _)| c),
            Some("cargo test".to_string())
        );
        // MEMORY.md overrides on clash + adds new names
        std::fs::create_dir_all(root.join(".sqwai/memory")).unwrap();
        std::fs::write(
            root.join(".sqwai/memory/MEMORY.md"),
            "notes\nverify: test = make test-ci\nverify: lint = cargo clippy\n",
        )
        .unwrap();
        let found = Config::detect_verify_commands(root);
        let get = |n: &str| found.iter().find(|(k, _, _)| k == n).cloned();
        assert_eq!(
            get("test"),
            Some(("test".to_string(), "make test-ci".to_string(), "MEMORY.md"))
        );
        assert_eq!(
            get("lint").map(|(_, c, _)| c),
            Some("cargo clippy".to_string())
        );
        // nothing anywhere: empty, not an error
        let empty = tempfile::tempdir().unwrap();
        assert!(Config::detect_verify_commands(empty.path()).is_empty());
    }

    #[test]
    fn seed_verify_commands_merges_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        // pre-existing hand-written values survive; unrelated content stays
        std::fs::create_dir_all(root.join(".sqwai")).unwrap();
        std::fs::write(
            root.join(".sqwai/config.toml"),
            "[plan]\nmax_steps = 9\n[verify]\ncommands = { test = \"hand command\" }\n",
        )
        .unwrap();
        let (added, already) = Config::seed_verify_commands(root);
        assert!(added.is_empty(), "hand value must win: {added:?}");
        assert_eq!(already, 1);
        // hostile shape: no panic, no write clobber
        std::fs::write(root.join(".sqwai/config.toml"), "verify = 5\n").unwrap();
        let (added, _) = Config::seed_verify_commands(root);
        assert!(added.is_empty());
        // missing file: dir created, map written, readable back
        let fresh = tempfile::tempdir().unwrap();
        std::fs::write(fresh.path().join("Cargo.toml"), "[package]\n").unwrap();
        let (added, _) = Config::seed_verify_commands(fresh.path());
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0, "test");
        let back = Config::project_verify_commands(fresh.path());
        assert_eq!(back.get("test").map(String::as_str), Some("cargo test"));
    }

    #[test]
    fn project_overrides_accept_verify_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sqwai")).unwrap();
        std::fs::write(
            dir.path().join(".sqwai/config.toml"),
            "[verify]\ncommands = { unit = \"cargo test --lib\" }\n",
        )
        .unwrap();
        // parsed through the same ProjectOverrides path as real files
        let raw = std::fs::read_to_string(dir.path().join(".sqwai/config.toml")).unwrap();
        let o: ProjectOverrides = toml::from_str(&raw).unwrap();
        let cmds = o.verify.commands.expect("verify.commands parses");
        assert_eq!(
            cmds.get("unit").map(String::as_str),
            Some("cargo test --lib")
        );
        // and the allowlist admits the section (no ignoring note)
        let mut cfg = Config::default();
        let notes = cfg.apply_project_overrides(dir.path());
        assert!(
            !notes.iter().any(|n| n.contains("verify")),
            "verify must be allowlisted: {notes:?}"
        );
        assert_eq!(
            cfg.verify.commands.get("unit").map(String::as_str),
            Some("cargo test --lib")
        );
    }

    #[test]
    fn default_config_template_has_resolvable_last_model() {
        let cfg = Config::default();
        assert!(!cfg.last_model.is_empty());
        assert!(cfg.last_model_config().is_ok());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_template(&path).unwrap();
        let mut loaded: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        loaded.apply_builtins();
        loaded.migrate_last_model();
        assert!(loaded.last_model_config().is_ok());
    }

    #[test]
    fn legacy_default_model_migrates_to_last_model_once() {
        let mut cfg: Config = toml::from_str(
            r#"
default_model = "my-old"
[models."my-old"]
provider = "p"
id = "my-old"
context = 1000
effort = "off"
"#,
        )
        .unwrap();
        assert_eq!(cfg.last_model, "");
        assert_eq!(cfg.legacy_default_model, "my-old");
        cfg.apply_builtins();
        cfg.migrate_last_model();
        assert_eq!(cfg.last_model, "my-old");
        assert!(cfg.legacy_default_model.is_empty());

        // dangling reference (model deleted since) falls back, not errors
        cfg.last_model = "gone".into();
        cfg.migrate_last_model();
        assert_eq!(cfg.last_model, first_builtin_model());
        assert!(cfg.last_model_config().is_ok());
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
            .and_then(|rest| rest.split("```").next());
        // no TOML example in the README (by design — config is
        // self-documented): nothing runnable to verify
        let Some(block) = block else {
            return;
        };
        let cfg: Config = toml::from_str(block)
            .unwrap_or_else(|e| panic!("README config example does not parse: {e}\n{block}"));
        assert!(
            cfg.providers.values().all(|p| !p.base_url.is_empty()),
            "every provider in the example needs a base_url"
        );
        assert!(
            cfg.models.contains_key(&cfg.last_model),
            "last_model {:?} must be one of the example's models",
            cfg.last_model
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

    #[test]
    fn effort_strs_cover_every_level_in_order() {
        // pick-one UI builds on this: a hardcoded copy once dropped xhigh
        let from_all: Vec<&str> = EffortLevel::ALL.iter().map(|l| l.as_str()).collect();
        assert_eq!(&EffortLevel::STRS[..], &from_all[..]);
    }

    #[test]
    fn effort_control_strs_cover_every_variant_in_order() {
        let from_all: Vec<&str> = EffortControl::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(&EffortControl::STRS[..], &from_all[..]);
        assert_eq!(EffortControl::from_str("named"), Some(EffortControl::Named));
        assert_eq!(EffortControl::from_str("auto"), None);
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
            EffortControl::Named,
            "an unknown OpenAI-compatible endpoint gets literal level names"
        );
        assert!(!m.effort_support(WireFormat::Openai).always_on);

        m.effort_control = Some(EffortControl::Named);
        assert_eq!(
            m.effort_support(WireFormat::Openai).control,
            EffortControl::Named,
            "a documented named-level model must be able to say so"
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
            EffortControl::Named
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
             effort_control = \"named\"\neffort_always_on = true\n",
        )
        .unwrap();
        assert_eq!(m.effort_control, Some(EffortControl::Named));
        assert!(m.effort_always_on);
        let back: ModelConfig = toml::from_str(&toml::to_string_pretty(&m).unwrap()).unwrap();
        assert_eq!(back.effort_control, m.effort_control);
        assert!(back.effort_always_on);

        // legacy spellings still parse to the same control
        for legacy in ["levels", "xhigh"] {
            let old: ModelConfig = toml::from_str(&format!(
                "provider = \"p\"\nid = \"m\"\ncontext = 1000\neffort = \"high\"\neffort_control = \"{legacy}\"\n"
            ))
            .unwrap();
            assert_eq!(old.effort_control, Some(EffortControl::Named), "{legacy}");
        }

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
                fallback: None,
            },
        );
        cfg.apply_builtins();
        assert!(cfg.models.contains_key("my-custom-model"));
        assert!(!cfg.is_builtin_model("my-custom-model"));
    }

    #[test]
    fn customized_builtin_model_survives_save_strip_and_reapply() {
        // the hand-edit path: user overrides one field of a built-in model
        let mut cfg = Config::default();
        cfg.apply_builtins();
        let key = "gemini-3.8-flash";
        assert!(cfg.is_builtin_model(key));
        let mut customized = cfg.models[key].clone();
        customized.effort = EffortLevel::High;
        cfg.models.insert(key.into(), customized.clone());

        // save-strip keeps the customized entry, drops pristine ones
        let stripped = cfg.without_pristine_builtins();
        assert_eq!(
            stripped.models.get(key),
            Some(&customized),
            "customized built-in must persist"
        );
        for k in stripped.models.keys() {
            if k != key {
                assert!(
                    !BuiltinCatalog::current().models.contains_key(k),
                    "pristine built-in {k} leaked into the user file"
                );
            }
        }

        // reload: user entry wins over the catalog, untouched keys refresh
        let mut reloaded = stripped;
        reloaded.apply_builtins();
        assert_eq!(
            reloaded.models.get(key),
            Some(&customized),
            "re-apply must not clobber the user override"
        );
    }

    #[test]
    fn project_overrides_apply_allowlisted_keys_and_report_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let sqwai = dir.path().join(".sqwai");
        std::fs::create_dir_all(&sqwai).unwrap();
        std::fs::write(
            sqwai.join("config.toml"),
            r#"
[diary]
token_budget = 500

[plan]
max_steps = 5
plan_first = "off"

[safety]
blocked_patterns = ["rm -rf /"]

[models."x"]
provider = "p"
"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        let notes = cfg.apply_project_overrides(dir.path());
        assert_eq!(cfg.diary.token_budget, 500);
        assert_eq!(cfg.plan.max_steps, 5);
        // not allowlisted: values untouched, but reported
        assert_eq!(cfg.plan.plan_first, PlanFirstMode::Soft);
        assert!(
            notes.iter().any(|n| n.contains("plan.plan_first")),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n.contains("[safety]")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("[models]")), "{notes:?}");
        assert!(
            !cfg.models.contains_key("x"),
            "project file must not inject models"
        );
    }

    #[test]
    fn project_overrides_missing_file_is_silent_and_broken_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        assert!(cfg.apply_project_overrides(dir.path()).is_empty());

        let sqwai = dir.path().join(".sqwai");
        std::fs::create_dir_all(&sqwai).unwrap();
        std::fs::write(sqwai.join("config.toml"), "[diary\ntoken_budget = ").unwrap();
        let before = cfg.diary.token_budget;
        let notes = cfg.apply_project_overrides(dir.path());
        assert_eq!(
            cfg.diary.token_budget, before,
            "broken file must change nothing"
        );
        assert!(!notes.is_empty(), "broken file must be reported");
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
