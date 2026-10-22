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
    // `ALL`/`as_str`/`STRS`/`from_str` survive for config-file back-compat
    // (stored overrides are still honoured) and tests; the UI derives
    // control from the wire format now.
    #[allow(dead_code)]
    pub const ALL: [EffortControl; 4] = [
        EffortControl::None,
        EffortControl::Toggle,
        EffortControl::Named,
        EffortControl::Budget,
    ];

    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Toggle => "toggle",
            Self::Named => "named",
            Self::Budget => "budget",
        }
    }

    /// Option strings for pick-one UI, same order as `ALL`.
    #[allow(dead_code)]
    pub const STRS: [&'static str; 4] = ["none", "toggle", "named", "budget"];

    #[allow(dead_code)]
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
    /// Lifecycle marker from the catalog. `retired` models are hidden from
    /// pickers but never deleted (user configs referencing them keep working
    /// until the user switches). Absent means active.
    #[serde(default, skip_serializing_if = "ModelStatus::is_active")]
    pub status: ModelStatus,
    /// Whether the model accepts image input. Absent means unknown, which is
    /// treated as "yes": most current models are multimodal, and a wrong
    /// refusal would disable `view_image` on a capable model. A declared
    /// `false` blocks the tool before any request is paid for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Whether the model accepts PDF documents. Absent means unknown and is
    /// treated as "no": a PDF sent to a model without document support is a
    /// hard API error, unlike an image on a text-only model which most
    /// providers reject the same way but far less often.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<bool>,
}

/// Lifecycle of a catalog model. Tombstones retire without deleting:
/// nothing references a removed id, and a resurrected id stays retired
/// until the catalog says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    #[default]
    Active,
    Deprecated,
    Retired,
}

impl ModelStatus {
    fn is_active(&self) -> bool {
        *self == Self::Active
    }

    /// Retired models never surface in pickers; deprecated ones do.
    pub fn visible_in_picker(self) -> bool {
        self != Self::Retired
    }
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
    "https://raw.githubusercontent.com/hksae/sqwai-providers/master/builtin_providers.toml";

/// Minimum interval (3 hours) between automatic background catalog checks.
pub const CATALOG_CHECK_INTERVAL_SECS: u64 = 3 * 3600;

/// SSRF gate shared by webfetch and the catalog check: literal non-public
/// IPs and local metadata names never become request targets. The model
/// chooses webfetch URLs outright, and the catalog can re-point provider
/// base_urls (with the user's key following them), so both entries run
/// this. DNS names are NOT resolved here: resolution races the connect
/// (rebinding) — a hostile-DNS residual remains, documented not fixed.
///
/// `allow_hosts` is the user's `[web]` opt-in and opens exactly one
/// class: loopback. A dev server on `localhost:3000` or `127.0.0.1:8080` is a
/// legitimate target the gate used to refuse while bash `curl` reached the
/// same host silently — the honest path was missing, not the danger.
/// Everything else stays refused whatever the list says: the metadata names
/// are the IMDS target, and the other non-public ranges are the SSRF blast
/// radius. The catalog calls this with an empty list: remote catalog text
/// must not gain a host the user typed for their own tools.
pub(crate) fn url_host_allowed(url: &reqwest::Url, allow_hosts: &[String]) -> Result<(), String> {
    let host = url_host(url);
    let listed = entry_matches(&host, url.port_or_known_default(), allow_hosts);
    // the url crate normalizes WHATWG numeric forms (2130706433,
    // 0x7f.0.0.1) to dotted quads before we ever see them, so parsing
    // the normalized host catches the obfuscated literals too
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        if addr.is_loopback() {
            return match listed {
                true => Ok(()),
                false => Err(format!(
                    "refuses loopback IP literal {host}; list it in [web].allow_hosts"
                )),
            };
        }
        if ip_blocked(&addr) {
            return Err(format!("refuses non-public IP literal {host}"));
        }
        return Ok(());
    }
    let name = host.trim_end_matches('.');
    if name == "metadata.google.internal" || name == "metadata.google" {
        return Err(format!("refuses metadata host {host}"));
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return match listed {
            true => Ok(()),
            false => Err(format!(
                "refuses local host {host}; list it in [web].allow_hosts"
            )),
        };
    }
    Ok(())
}

/// The gate's view of a URL's host: no IPv6 brackets, lowercased.
/// [`reqwest::Url::host_str`] keeps the brackets, and the comparison below
/// runs against user-typed entries.
fn url_host(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase()
}

/// `[web].allow_hosts` matching: an entry is `host` (any port) or
/// `host:port` (that port only). IPv6 entries keep their brackets
/// (`[::1]:8080`); a portless entry still matches a default port, since
/// [`reqwest::Url::port_or_known_default`] fills it in.
fn entry_matches(host: &str, port: Option<u16>, allow_hosts: &[String]) -> bool {
    allow_hosts.iter().any(|raw| {
        let entry = raw.trim().to_ascii_lowercase();
        let (want_host, want_port) = match entry.split_once("]:") {
            // [::1]:8080 — the compared host carries no brackets
            Some((h, p)) => (h.trim_start_matches('[').to_string(), p.parse::<u16>().ok()),
            None => match entry.rsplit_once(':') {
                // a second colon means a bare IPv6 address, not a port
                Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => {
                    (h.to_string(), p.parse::<u16>().ok())
                }
                _ => (
                    entry
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string(),
                    None,
                ),
            },
        };
        want_host == host && want_port.is_none_or(|p| Some(p) == port)
    })
}

/// True for every IPv4/IPv6 range that is not public unicast: loopback,
/// unspecified, private, link-local (cloud metadata lives at
/// 169.254.169.254), multicast, reserved, documentation, and the v6
/// wrappers that embed a v4 address (mapped/compat/6to4).
pub(crate) fn ip_blocked(addr: &std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0
                || o[0] == 10
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] == 127
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 172 && (16..32).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
                || (o[0] == 192 && o[1] == 88 && o[2] == 99)
                || (o[0] == 198 && (18..20).contains(&o[1]))
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || o[0] >= 224
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xff00) == 0xff00
                || s[0] == 0x2001 && (s[1] == 0xdb8 || s[1] == 0)
                || s[0] == 0x2002
                || s[0] == 0x0064 && s[1] == 0xff9b
                || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
                || (s[0..5] == [0, 0, 0, 0, 0] || (s[0..5] == [0, 0, 0, 0, 0xffff]))
                    && ip_blocked(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                        (s[6] >> 8) as u8,
                        (s[6] & 0xff) as u8,
                        (s[7] >> 8) as u8,
                        (s[7] & 0xff) as u8,
                    )))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuiltinCatalog {
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Monotonic serial stamped by the generator (CI), never by hand.
    /// Freshness compares serials, not dates: a content change without a
    /// stamp bump still propagates, and a replayed old file never wins.
    /// Missing (old files) reads as 0.
    #[serde(default)]
    pub serial: u64,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    /// File-declaration order of `[providers.<name>]` sections.
    #[serde(skip)]
    pub provider_order: Vec<String>,
    /// File-declaration order of `[models."<key>"]` sections (newest release_date first).
    #[serde(skip)]
    pub model_order: Vec<String>,
}

/// Pinned origins for built-in providers: (name, host, key env).
/// A remote catalog may refresh metadata, but it can never move a key to
/// a new host (that is key exfiltration, not an update) and it can never
/// introduce an unpinned provider over the wire (those ship with releases).
/// Unknown names in a fetched file are skipped, never applied.
pub const BUILTIN_ORIGINS: &[(&str, &str, &str)] = &[
    (
        "gemini",
        "generativelanguage.googleapis.com",
        "GEMINI_API_KEY",
    ),
    ("anthropic", "api.anthropic.com", "ANTHROPIC_API_KEY"),
    ("openai", "api.openai.com", "OPENAI_API_KEY"),
    ("deepseek", "api.deepseek.com", "DEEPSEEK_API_KEY"),
    ("grok", "api.x.ai", "XAI_API_KEY"),
    ("kimi", "api.moonshot.ai", "MOONSHOT_API_KEY"),
    (
        "alibaba",
        "dashscope-intl.aliyuncs.com",
        "DASHSCOPE_API_KEY",
    ),
    ("zai", "api.z.ai", "ZHIPU_API_KEY"),
    ("minimax", "api.minimax.io", "MINIMAX_API_KEY"),
    ("mistral", "api.mistral.ai", "MISTRAL_API_KEY"),
    ("cohere", "api.cohere.ai", "COHERE_API_KEY"),
    ("groq", "api.groq.com", "GROQ_API_KEY"),
    ("cerebras", "api.cerebras.ai", "CEREBRAS_API_KEY"),
    ("togetherai", "api.together.xyz", "TOGETHER_API_KEY"),
    ("fireworks", "api.fireworks.ai", "FIREWORKS_API_KEY"),
    ("deepinfra", "api.deepinfra.com", "DEEPINFRA_API_KEY"),
    ("nvidia", "integrate.api.nvidia.com", "NVIDIA_API_KEY"),
    ("openrouter", "openrouter.ai", "OPENROUTER_API_KEY"),
];

fn pinned_origin(name: &str) -> Option<(&'static str, &'static str)> {
    BUILTIN_ORIGINS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, host, key_env)| (*host, *key_env))
}

pub fn builtin_cache_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("builtin_providers.toml"))
}

pub fn builtin_meta_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("builtin_providers_meta.json"))
}

/// One-line catalog provenance for the providers menu: serial, stamp and
/// last check. Never empty — even the embedded fallback reports itself,
/// so "no info" itself tells the user updates never landed.
/// Kept while the providers surface runs without it: the owner cut the
/// status line for now and may bring it back.
#[allow(dead_code)]
pub fn catalog_status_line() -> String {
    let catalog = BuiltinCatalog::current();
    let date = catalog
        .updated_at
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("undated");
    let checked = builtin_meta_path()
        .and_then(|path| std::fs::read_to_string(&path).map_err(|e| e.into()))
        .ok()
        .and_then(|raw| serde_json::from_str::<BuiltinMeta>(&raw).ok())
        .map(|meta| meta.last_checked)
        .unwrap_or_else(|| "never".to_string());
    format!(
        "catalog #{} · {} · checked {}",
        catalog.serial, date, checked
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuiltinMeta {
    pub last_checked: String,
    /// Unix timestamp (seconds) of the last check, for the 3-hour poll interval.
    #[serde(default)]
    pub last_checked_ts: u64,
    /// ETag of the cached catalog body, for conditional refresh.
    #[serde(default)]
    pub etag: Option<String>,
}

static BUILTIN_CACHE: std::sync::RwLock<Option<BuiltinCatalog>> = std::sync::RwLock::new(None);
static PREVIOUS_BUILTIN_CACHE: std::sync::RwLock<Option<BuiltinCatalog>> =
    std::sync::RwLock::new(None);

impl BuiltinCatalog {
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut catalog: BuiltinCatalog =
            toml::from_str(text).context("parsing builtin providers TOML")?;
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed
                .strip_prefix("[providers.")
                .and_then(|s| s.strip_suffix(']'))
            {
                let name = rest.trim_matches('"');
                if catalog.providers.contains_key(name) {
                    catalog.provider_order.push(name.to_string());
                }
            } else if let Some(rest) = trimmed
                .strip_prefix("[models.")
                .and_then(|s| s.strip_suffix(']'))
            {
                let key = rest.trim_matches('"');
                if catalog.models.contains_key(key) {
                    catalog.model_order.push(key.to_string());
                }
            }
        }
        Ok(catalog)
    }

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
        if let Ok(mut guard) = BUILTIN_CACHE.write()
            && let Some(prev) = guard.take()
            && let Ok(mut prev_guard) = PREVIOUS_BUILTIN_CACHE.write()
        {
            *prev_guard = Some(prev);
        }
    }

    pub fn load_local() -> Self {
        #[cfg(test)]
        return Self::from_toml_str(BUILTIN_PROVIDERS_FALLBACK).unwrap_or_default();
        #[allow(unreachable_code)]
        {
            let fallback = Self::from_toml_str(BUILTIN_PROVIDERS_FALLBACK).unwrap_or_default();
            if let Ok(path) = builtin_cache_path()
                && let Ok(raw) = std::fs::read_to_string(&path)
                && let Ok(catalog) = validated_disk_catalog(&raw, &fallback)
            {
                return catalog;
            }
            fallback
        }
    }
}

/// The on-disk cache is untrusted input until proven otherwise (audit C8):
/// the fetch path validates before writing, but the file can be rewritten
/// behind its back, and `apply_builtins` copies `base_url` straight into
/// provider config — the user's key follows. Same content rules as the
/// fetch, plus the freshness floor. Residual: a tampered cache pointing at
/// an attacker's *public https* endpoint is indistinguishable from a
/// legitimate update until catalogs are signed (tracked separately).
fn validated_disk_catalog(raw: &str, fallback: &BuiltinCatalog) -> Result<BuiltinCatalog> {
    let catalog = validate_fetched_catalog(raw, 0)?;
    if catalog.serial < fallback.serial {
        anyhow::bail!("cached catalog is older than the built-in one");
    }
    Ok(catalog)
}

/// Validate a fetched builtin catalog before it touches disk or config.
/// `apply_builtins` copies provider base_urls from the catalog and the
/// user's key follows them, so a compromised remote is key exfiltration,
/// not a cosmetic issue. Rules, all offline-testable:
/// - every provider endpoint is https on a public host (same SSRF gate as
///   webfetch), no inline secrets ride the catalog;
/// - a builtin provider's host and key env must equal the pinned origin —
///   a moved host is an attack, not an update (new providers never arrive
///   this way; they ship with releases and are skipped here);
/// - `serial` never moves backwards (rollback protection against a replayed
///   old file); `updated_at` is informational only.
///
/// Signature verification is the real fix and needs repo-side signing;
/// until then this is the enforced half.
pub fn validate_fetched_catalog(text: &str, cached_serial: u64) -> Result<BuiltinCatalog> {
    let catalog = BuiltinCatalog::from_toml_str(text)?;
    if catalog.serial < cached_serial {
        anyhow::bail!(
            "fetched catalog is older than the cached one (serial {} < {cached_serial})",
            catalog.serial
        );
    }
    for (name, provider) in &catalog.providers {
        if provider.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
            anyhow::bail!("fetched catalog plants an inline api_key for provider {name:?}");
        }
        let url = reqwest::Url::parse(&provider.base_url)
            .map_err(|e| anyhow::anyhow!("provider {name:?} has an unparsable base_url: {e}"))?;
        if url.scheme() != "https" {
            anyhow::bail!(
                "provider {name:?} base_url is not https: {}",
                provider.base_url
            );
        }
        // The catalog is remote text: it gets no share of the user's own
        // `[web].allow_hosts`.
        url_host_allowed(&url, &[])
            .map_err(|detail| anyhow::anyhow!("provider {name:?} base_url {detail}"))?;
        if let Some((host, key_env)) = pinned_origin(name) {
            if url.host_str() != Some(host) {
                anyhow::bail!(
                    "provider {name:?} moves off its pinned host ({host}): refusing catalog"
                );
            }
            if provider
                .api_key_env
                .as_deref()
                .is_some_and(|k| k != key_env)
            {
                anyhow::bail!(
                    "provider {name:?} rebinds its key env (pinned {key_env}): refusing catalog"
                );
            }
        }
    }
    Ok(catalog)
}

pub async fn check_and_update_builtins(force: bool) -> Result<Option<BuiltinCatalog>> {
    // Air-gapped escape hatch: no network, embedded catalog stays.
    if std::env::var("SQWAI_NO_CATALOG_UPDATE")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        return Ok(None);
    }
    let url = std::env::var("SQWAI_CATALOG_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| BUILTIN_PROVIDERS_URL.to_string());
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let meta_now = builtin_meta_path()
        .and_then(|path| std::fs::read_to_string(&path).map_err(|e| e.into()))
        .ok()
        .and_then(|raw| serde_json::from_str::<BuiltinMeta>(&raw).ok());
    if !force
        && let Some(meta) = meta_now.as_ref()
        && meta.last_checked_ts > 0
        && now_ts.saturating_sub(meta.last_checked_ts) < CATALOG_CHECK_INTERVAL_SECS
    {
        return Ok(None);
    }

    let mut request = crate::providers::http_client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5));
    if let Some(etag) = meta_now.as_ref().and_then(|m| m.etag.clone()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let res = request.send().await?;
    if res.status() == reqwest::StatusCode::NOT_MODIFIED {
        // Edge: 304 with no usable cache on disk falls through to the
        // embedded fallback below via Ok(None) — same as "up to date".
        write_meta(&today, now_ts, meta_now.as_ref().and_then(|m| m.etag.clone()));
        return Ok(None);
    }
    if !res.status().is_success() {
        anyhow::bail!("server returned status {}", res.status());
    }
    let etag = res
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let text = res.text().await?;
    // the cached serial is the rollback floor; a refusal leaves the meta
    // stamp alone so the next interval retries (and recovers on its own)
    let cached_serial = builtin_cache_path()
        .and_then(|path| std::fs::read_to_string(&path).map_err(|e| e.into()))
        .ok()
        .and_then(|raw| toml::from_str::<BuiltinCatalog>(&raw).ok())
        .map(|catalog| catalog.serial)
        .unwrap_or(0);
    let catalog = validate_fetched_catalog(&text, cached_serial)?;

    // No-op when the content is identical: the meta stamp still advances
    // (so the next launch within 3h skips the fetch), but the caller gets
    // Ok(None) — no rewrite, no apply_builtins pass, no "updated" noise.
    if let Ok(cache_path) = builtin_cache_path()
        && let Ok(cached) = std::fs::read_to_string(&cache_path)
        && cached == text
    {
        write_meta(
            &today,
            now_ts,
            etag.or(meta_now.as_ref().and_then(|m| m.etag.clone())),
        );
        return Ok(None);
    }

    if let Ok(cache_path) = builtin_cache_path() {
        let _ = atomic_write(&cache_path, &text);
    }
    write_meta(&today, now_ts, etag);
    BuiltinCatalog::invalidate_cache();
    Ok(Some(catalog))
}

fn write_meta(today: &str, now_ts: u64, etag: Option<String>) {
    if let Ok(meta_path) = builtin_meta_path()
        && let Ok(meta_json) = serde_json::to_string(&BuiltinMeta {
            last_checked: today.to_string(),
            last_checked_ts: now_ts,
            etag,
        })
    {
        let _ = atomic_write(&meta_path, &meta_json);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub blocked_patterns: Vec<String>,
}

/// `[web]` — the opt-in exceptions to the SSRF gate.
///
/// `allow_hosts` lists `host` or `host:port` targets the loopback class may
/// serve. Deliberately narrow: only loopback (127.0.0.0/8, `::1`,
/// `localhost`, `*.localhost`) can be listed. Other non-public ranges
/// (10/8, 172.16/12, 192.168/16, 169.254/16, CGNAT) and the cloud metadata
/// names stay refused, list or no list — those are the SSRF blast radius, and
/// a bare GET to them is never a legitimate dev-server visit.
///
/// Not project-overridable: a cloned repository must not be able to widen the
/// host its own tooling may reach.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebConfig {
    #[serde(default)]
    pub allow_hosts: Vec<String>,
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

/// runtime-tweakable ui behavior (`/debug`)
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
    /// open an `[image N]` marker in the OS viewer on click
    #[serde(default = "default_true")]
    pub open_attachments: bool,
    /// capture mouse events; off restores the terminal's native selection.
    /// Takes effect on the next launch.
    #[serde(default = "default_true")]
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            typewriter: true,
            http_log: false,
            experimental_test: false,
            open_attachments: true,
            mouse: true,
        }
    }
}

/// `[tools]` — per-tool caps. Defaults keep the old limits where they existed;
/// the new file-size and background-job caps are conservative.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub bash: BashToolConfig,
    #[serde(default)]
    pub read: ReadToolConfig,
    #[serde(default)]
    pub outline: OutlineToolConfig,
    #[serde(default)]
    pub web: WebToolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashToolConfig {
    #[serde(default = "default_bash_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_bash_output_bytes")]
    pub output_bytes: usize,
    /// 0 disables the cap
    #[serde(default = "default_bash_background_jobs")]
    pub background_jobs: usize,
}

impl Default for BashToolConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_bash_timeout_secs(),
            output_bytes: default_bash_output_bytes(),
            background_jobs: default_bash_background_jobs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadToolConfig {
    /// max lines per read and the default window when `limit` is omitted
    #[serde(default = "default_read_window")]
    pub window: usize,
    /// 0 disables the cap
    #[serde(default = "default_read_max_file_bytes")]
    pub max_file_bytes: u64,
}

impl Default for ReadToolConfig {
    fn default() -> Self {
        Self {
            window: default_read_window(),
            max_file_bytes: default_read_max_file_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineToolConfig {
    /// 0 disables the cap
    #[serde(default = "default_outline_max_file_bytes")]
    pub max_file_bytes: u64,
}

impl Default for OutlineToolConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: default_outline_max_file_bytes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebToolConfig {
    /// default number of search results; 1..=20
    #[serde(default = "default_web_results")]
    pub results: usize,
    /// default per-request timeout in seconds; 1..=60
    #[serde(default = "default_web_timeout_secs")]
    pub timeout_secs: u64,
    /// max response body bytes read by webfetch/websearch
    #[serde(default = "default_web_fetch_bytes")]
    pub fetch_bytes: usize,
}

impl Default for WebToolConfig {
    fn default() -> Self {
        Self {
            results: default_web_results(),
            timeout_secs: default_web_timeout_secs(),
            fetch_bytes: default_web_fetch_bytes(),
        }
    }
}

/// `[limits]` — per-turn and concurrency guardrails. Zero disables a per-turn
/// cap; `max_parallel_tools` is clamped to 1..=64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// tool calls executed in one user turn; 0 disables the cap
    #[serde(default)]
    pub max_tool_calls_per_turn: usize,
    /// provider-reported prompt+completion tokens in one user turn;
    /// 0 disables the cap
    #[serde(default)]
    pub max_tokens_per_turn: u64,
    /// concurrent read-only tool calls; clamped to 1..=64
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_tool_calls_per_turn: 0,
            max_tokens_per_turn: 0,
            max_parallel_tools: default_max_parallel_tools(),
        }
    }
}

fn default_bash_timeout_secs() -> u64 {
    120
}
fn default_bash_output_bytes() -> usize {
    30_000
}
fn default_bash_background_jobs() -> usize {
    8
}
fn default_read_window() -> usize {
    2_000
}
fn default_read_max_file_bytes() -> u64 {
    16 * 1024 * 1024
}
fn default_outline_max_file_bytes() -> u64 {
    2_097_152
}
fn default_web_results() -> usize {
    6
}
fn default_web_timeout_secs() -> u64 {
    15
}
fn default_web_fetch_bytes() -> usize {
    1_500_000
}
fn default_max_parallel_tools() -> usize {
    24
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_load_budget_ratio")]
    pub load_budget_ratio: f64,
    #[serde(default = "default_memory_max_tokens")]
    pub max_tokens: u32,
    #[serde(
        default = "default_memory_max_proposals",
        alias = "max_proposals_per_turn"
    )]
    pub max_writes_per_turn: u8,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            load_budget_ratio: default_memory_load_budget_ratio(),
            max_tokens: default_memory_max_tokens(),
            max_writes_per_turn: default_memory_max_proposals(),
        }
    }
}

/// `[undo]` — retention and storage for the two checkpoint layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoConfig {
    /// commits of a session's shadow chain to keep
    #[serde(default = "default_undo_keep_per_session")]
    pub keep_per_session: u32,
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
            blob_grace_secs: default_undo_blob_grace_secs(),
            shadow: default_undo_shadow(),
            shadow_max_bytes: default_undo_shadow_max_bytes(),
        }
    }
}

/// `[plan]` — the host's own limits on the structured plan.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PlanConfig {
    /// most steps a plan may hold before `add`/`split` are refused
    #[serde(default = "default_plan_max_steps")]
    pub max_steps: usize,
    /// actions attributed to a step before the tail block reminds the model
    /// to update the plan
    #[serde(default = "default_plan_nudge_after")]
    pub nudge_after: usize,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            max_steps: default_plan_max_steps(),
            nudge_after: default_plan_nudge_after(),
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

/// `[secrets]` — files the host must not read into durable state.
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
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold: default_compaction_threshold(),
            keep_turns: default_compaction_keep_turns(),
            anchor_ratio: default_compaction_anchor_ratio(),
        }
    }
}

/// Project-level overrides (`.sqwai/config.toml`).
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
struct UiOverride {
    typewriter: Option<bool>,
    http_log: Option<bool>,
    experimental_test: Option<bool>,
    open_attachments: Option<bool>,
    mouse: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CompactionOverride {
    threshold: Option<f64>,
    keep_turns: Option<usize>,
    anchor_ratio: Option<f64>,
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
    #[serde(alias = "max_proposals_per_turn")]
    max_writes_per_turn: Option<u8>,
}

/// (table, keys) accepted from a project file. Anything else present is
/// ignored and reported. Keep in sync with the `*Override` structs above.
const PROJECT_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "ui",
        &[
            "typewriter",
            "http_log",
            "experimental_test",
            "open_attachments",
            "mouse",
        ],
    ),
    ("compaction", &["threshold", "keep_turns", "anchor_ratio"]),
    (
        "undo",
        &[
            "keep_per_session",
            "blob_grace_secs",
            "shadow",
            "shadow_max_bytes",
        ],
    ),
    ("plan", &["max_steps", "nudge_after"]),
    ("verify", &["commands"]),
    (
        "memory",
        &[
            "load_budget_ratio",
            "max_tokens",
            "max_writes_per_turn",
            "max_proposals_per_turn",
        ],
    ),
];

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
fn default_memory_max_tokens() -> u32 {
    3000
}
fn default_memory_max_proposals() -> u8 {
    2
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
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
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
    let catalog = BuiltinCatalog::current();
    catalog
        .model_order
        .first()
        .cloned()
        .or_else(|| catalog.models.keys().next().cloned())
        .unwrap_or_default()
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
            providers: catalog.providers,
            models: catalog.models,
            safety: SafetyConfig::default(),
            web: WebConfig::default(),
            ui: UiConfig::default(),
            tools: ToolsConfig::default(),
            limits: LimitsConfig::default(),
            mcp: McpConfig::default(),
            lsp: LspConfig::default(),
            skills: SkillsConfig::default(),
            memory: MemoryConfig::default(),
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

    /// Transition pristine built-in models from `old` catalog to `new`:
    /// untouched built-in models update their specs in-place, and built-in
    /// models removed upstream are dropped unless the user customized them
    /// or currently has them selected as `last_model`.
    pub fn apply_catalog_transition(&mut self, old: &BuiltinCatalog, new: &BuiltinCatalog) {
        for (k, old_m) in &old.models {
            if self.models.get(k) != Some(old_m) {
                continue;
            }
            match new.models.get(k) {
                Some(new_m) => {
                    self.models.insert(k.clone(), new_m.clone());
                }
                None => {
                    if self.last_model != *k {
                        self.models.remove(k);
                    }
                }
            }
        }
    }

    /// Merge built-in providers/models into config. Provider endpoints stay
    /// fresh (base_url/format sync); user key overrides are preserved.
    /// Models are user-owned by key: catalog entries fill in absent keys
    /// only, never overwrite. A customized built-in model survives because
    /// `save` persists it (see below).
    pub fn apply_builtins(&mut self) {
        let catalog = BuiltinCatalog::current();
        if let Ok(mut prev_guard) = PREVIOUS_BUILTIN_CACHE.write()
            && let Some(prev) = prev_guard.take()
        {
            self.apply_catalog_transition(&prev, &catalog);
        }
        for (name, p) in catalog.providers {
            // New providers never arrive over the wire: they ship with
            // releases (which extend the pinned origins above), so anything
            // else is skipped, never applied.
            if pinned_origin(&name).is_none() {
                continue;
            }
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
            // Models of skipped (unknown) providers are unusable — skip
            // with them. Otherwise insert new ones whole; for known ones
            // only the lifecycle flows (a user-marked status always wins).
            if !self.providers.contains_key(&m.provider) {
                continue;
            }
            match self.models.get_mut(&k) {
                Some(u) => {
                    if u.status == ModelStatus::Active {
                        u.status = m.status;
                    }
                }
                None => {
                    self.models.insert(k, m);
                }
            }
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
        if let Some(v) = o.open_attachments {
            ui.open_attachments = v;
        }
        if let Some(v) = o.mouse {
            ui.mouse = v;
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
        if let Some(v) = o.max_writes_per_turn {
            memory.max_writes_per_turn = v;
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
mod tests;
