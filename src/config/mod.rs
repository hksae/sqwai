use std::collections::BTreeMap;
use std::path::PathBuf;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl<'de> Deserialize<'de> for ThinkingLevel {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = ThinkingLevel;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a thinking level string or a bool")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                s: &str,
            ) -> std::result::Result<ThinkingLevel, E> {
                ThinkingLevel::from_str(s)
                    .ok_or_else(|| E::custom(format!("unknown thinking level: {s}")))
            }
            // backward compatibility with the old `thinking = true/false`
            fn visit_bool<E: serde::de::Error>(
                self,
                b: bool,
            ) -> std::result::Result<ThinkingLevel, E> {
                Ok(if b {
                    ThinkingLevel::High
                } else {
                    ThinkingLevel::Off
                })
            }
        }
        d.deserialize_any(V)
    }
}

impl ThinkingLevel {
    /// the four selectable levels in the status-bar menu
    pub const SELECTABLE: [ThinkingLevel; 4] = [
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Max,
    ];
    pub const ALL: [ThinkingLevel; 5] = [
        ThinkingLevel::Off,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Max,
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
    pub thinking: ThinkingLevel,
    /// $ per 1M input tokens (for the cost meter)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_in: Option<f64>,
    /// $ per 1M output tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_out: Option<f64>,
}

/// built-in providers seeded on first run / merged into existing configs;
/// everything is prefilled except the api keys
pub struct SeedProvider {
    pub name: &'static str,
    pub format: WireFormat,
    pub base_url: &'static str,
    /// conventional env variable for the key
    pub key_env: &'static str,
    pub models: &'static [(&'static str, &'static str, u64, ThinkingLevel, f64, f64)],
}

macro_rules! m {
    ($key:expr, $ctx:expr, $th:expr, $pin:expr, $pout:expr) => {
        ($key, $key, $ctx, $th, $pin, $pout)
    };
}

pub const SEED_PROVIDERS: &[SeedProvider] = &[
    SeedProvider {
        name: "anthropic",
        format: WireFormat::Anthropic,
        base_url: "https://api.anthropic.com",
        key_env: "ANTHROPIC_API_KEY",
        models: &[
            m!("claude-opus-5", 1_000_000, ThinkingLevel::High, 5.0, 25.0),
            m!("claude-sonnet-5", 1_000_000, ThinkingLevel::High, 2.0, 10.0),
            m!("claude-haiku-4-5", 200_000, ThinkingLevel::Medium, 1.0, 5.0),
        ],
    },
    SeedProvider {
        name: "openai",
        format: WireFormat::Openai,
        base_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
        models: &[
            m!("gpt-5.6", 922_000, ThinkingLevel::High, 4.0, 20.0),
            m!("gpt-5.6-luna", 922_000, ThinkingLevel::Low, 0.2, 1.2),
            m!("gpt-5.3-codex", 272_000, ThinkingLevel::High, 1.75, 14.0),
        ],
    },
    SeedProvider {
        name: "gemini",
        format: WireFormat::Openai,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: "GEMINI_API_KEY",
        models: &[
            m!(
                "gemini-3.1-pro-preview",
                1_048_576,
                ThinkingLevel::High,
                2.0,
                12.0
            ),
            m!(
                "gemini-3.7-flash",
                1_048_576,
                ThinkingLevel::Medium,
                0.75,
                3.75
            ),
            m!(
                "gemini-3.5-flash-lite",
                1_048_576,
                ThinkingLevel::Off,
                0.3,
                2.5
            ),
        ],
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyConfig {
    #[serde(default)]
    #[allow(dead_code)]
    pub blocked_patterns: Vec<String>,
}

/// runtime-tweakable ui behavior (`/debug`, `/themes`)
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
pub struct Config {
    #[serde(default = "default_model_name")]
    pub default_model: String,
    #[serde(default = "default_thinking")]
    pub default_thinking: ThinkingLevel,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub safety: SafetyConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

fn default_model_name() -> String {
    "default".into()
}
fn default_thinking() -> ThinkingLevel {
    ThinkingLevel::Medium
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
        let mut cfg = Self {
            default_model: String::new(),
            default_thinking: ThinkingLevel::Medium,
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
            safety: SafetyConfig::default(),
            ui: UiConfig::default(),
        };
        cfg.ensure_seeds();
        cfg
    }
}

impl Config {
    /// add built-in providers/models that are not present yet; existing
    /// entries are never touched
    pub fn ensure_seeds(&mut self) {
        for s in SEED_PROVIDERS {
            self.providers
                .entry(s.name.to_string())
                .or_insert_with(|| ProviderConfig {
                    format: s.format,
                    base_url: s.base_url.to_string(),
                    api_key: None,
                    api_key_env: Some(s.key_env.to_string()),
                });
            for (key, id, ctx, th, pin, pout) in s.models {
                self.models
                    .entry(key.to_string())
                    .or_insert_with(|| ModelConfig {
                        provider: s.name.to_string(),
                        id: id.to_string(),
                        context: *ctx,
                        thinking: *th,
                        price_in: Some(*pin),
                        price_out: Some(*pout),
                    });
            }
        }
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
        // merge built-in providers/models into existing configs
        cfg.ensure_seeds();
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        // unit tests must never touch the real user config
        #[cfg(test)]
        return Ok(());
        #[allow(unreachable_code)]
        {
            let path = config_path()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &path,
                toml::to_string_pretty(self).context("serializing config")?,
            )
            .context("writing config")?;
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

pub fn write_template(path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&Config::default())?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_accepts_string_and_legacy_bool() {
        #[derive(Deserialize)]
        struct T {
            thinking: ThinkingLevel,
        }
        let t: T = toml::from_str("thinking = false").unwrap();
        assert_eq!(t.thinking, ThinkingLevel::Off);
        let t: T = toml::from_str("thinking = true").unwrap();
        assert_eq!(t.thinking, ThinkingLevel::High);
        let t: T = toml::from_str("thinking = \"max\"").unwrap();
        assert_eq!(t.thinking, ThinkingLevel::Max);
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
    fn seeds_merge_without_overwriting_user_edits() {
        let mut cfg = Config::default();
        assert!(cfg.providers.contains_key("anthropic"));
        assert!(cfg.providers.contains_key("openai"));
        assert!(cfg.providers.contains_key("gemini"));
        let opus = cfg.models.get("claude-opus-5").expect("seed model");
        assert_eq!(opus.context, 1_000_000);
        assert_eq!(opus.price_in, Some(5.0));

        // a user edit must survive a re-seed on the next load
        cfg.models.get_mut("claude-opus-5").unwrap().context = 42;
        cfg.ensure_seeds();
        assert_eq!(cfg.models.get("claude-opus-5").unwrap().context, 42);
    }
}
