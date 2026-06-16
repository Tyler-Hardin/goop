//! Configuration for goop: provider selection, model, tuning knobs,
//! and tool-group toggles.
//!
//! Reads from `~/.config/goop/config.toml` with env-var overrides.
//! Falls back to DeepSeek defaults for backward compatibility.
//!
//! Configuration layering (highest wins):
//! 1. CLI flags (`--model`)
//! 2. Environment variables (`GOOP_MODEL`)
//! 3. Session config (`<name>.state.toml` → `config`)
//! 4. Global config (`~/.config/goop/config.toml`)
//! 5. Hard-coded defaults

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ── error type ───────────────────────────────────────────────────────

/// Errors that can occur during configuration loading and parsing.
#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    /// The model string is not in `provider/model` format, or the
    /// provider is unknown, or the model name is empty.
    #[error("invalid model: {0}")]
    InvalidModel(String),

    /// An environment variable required for the chosen provider is
    /// missing (e.g. `DEEPSEEK_API_KEY`).
    #[error("missing API key: {0}")]
    MissingApiKey(String),

    /// I/O error reading or writing config files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The config file is not valid TOML.
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    /// Failed to render the default config template.
    #[error("template error: {0}")]
    Template(String),

    /// Catch-all for figment extraction errors.
    #[error("config extraction: {0}")]
    Extraction(String),
}

// ── paths ───────────────────────────────────────────────────────────

/// Return the user's home directory (via the `dirs` crate).
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Return the goop config directory: `$XDG_CONFIG_HOME/goop` (or
/// platform-appropriate equivalent via the `dirs` crate).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .map(|p| p.join("goop"))
        .unwrap_or_else(|| PathBuf::from(".config/goop"))
}

pub fn global_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

// ── provider ────────────────────────────────────────────────────────

/// Supported LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    #[default]
    DeepSeek,
    OpenAI,
    OpenRouter,
    Groq,
    Ollama,
    Anthropic,
}

impl Provider {
    /// Parse a provider from the first segment of a `provider/model` string.
    pub fn from_model_prefix(s: &str) -> Option<Self> {
        match s {
            "deepseek" => Some(Provider::DeepSeek),
            "openai" => Some(Provider::OpenAI),
            "openrouter" => Some(Provider::OpenRouter),
            "groq" => Some(Provider::Groq),
            "ollama" => Some(Provider::Ollama),
            "anthropic" => Some(Provider::Anthropic),
            _ => None,
        }
    }

    /// The provider segment as it appears in a `provider/model` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek",
            Provider::OpenAI => "openai",
            Provider::OpenRouter => "openrouter",
            Provider::Groq => "groq",
            Provider::Ollama => "ollama",
            Provider::Anthropic => "anthropic",
        }
    }

    /// Environment variable name for this provider's API key.
    pub fn api_key_env(self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::Groq => "GROQ_API_KEY",
            Provider::Ollama => "", // local, no key needed
            Provider::Anthropic => "ANTHROPIC_API_KEY",
        }
    }

    /// Default model for this provider when none is specified.
    pub fn default_model_str(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek/deepseek-v4-pro",
            Provider::OpenAI => "openai/gpt-4o",
            Provider::OpenRouter => "openrouter/openai/gpt-4o",
            Provider::Groq => "groq/llama-3.2-70b-versatile",
            Provider::Ollama => "ollama/llama3.2",
            Provider::Anthropic => "anthropic/claude-sonnet-4-6",
        }
    }

    /// Default [`Model`] for this provider.
    pub fn default_model(self) -> Model {
        Model {
            provider: self,
            name: self.model_name_from_default(),
        }
    }

    /// Extract the model-name portion from [`default_model_str`].
    fn model_name_from_default(self) -> String {
        let full = self.default_model_str();
        full.split_once('/')
            .map(|(_, name)| name)
            .unwrap_or(full)
            .to_string()
    }

    /// Human-readable label for logging.
    pub fn label(self) -> &'static str {
        match self {
            Provider::DeepSeek => "DeepSeek",
            Provider::OpenAI => "OpenAI",
            Provider::OpenRouter => "OpenRouter",
            Provider::Groq => "Groq",
            Provider::Ollama => "Ollama",
            Provider::Anthropic => "Anthropic",
        }
    }
}

// ── model newtype ───────────────────────────────────────────────────

/// A validated, parsed model identifier in `provider/model` format.
///
/// Construct via [`FromStr`] (or `"deepseek/deepseek-v4-pro".parse()`).
/// The provider and model-name portions are parsed and validated at
/// construction time — once you have a `Model`, the provider and name
/// accessors are infallible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    provider: Provider,
    name: String,
}

impl Model {
    /// The provider portion (e.g. [`Provider::DeepSeek`]).
    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// The model-name portion — everything after the first `/`.
    /// e.g. `"deepseek-v4-pro"` or `"openai/gpt-4o"`.
    pub fn model_name(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider.as_str(), self.name)
    }
}

impl FromStr for Model {
    type Err = ConfigError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (prefix, model_name) = s.split_once('/').ok_or_else(|| {
            ConfigError::InvalidModel(format!(
                "{s:?} — expected provider/model (e.g. deepseek/deepseek-v4-pro)"
            ))
        })?;

        let provider = Provider::from_model_prefix(prefix).ok_or_else(|| {
            ConfigError::InvalidModel(format!(
                "unknown provider {prefix:?} — supported: deepseek, openai, \
                 openrouter, groq, ollama, anthropic"
            ))
        })?;

        if model_name.is_empty() {
            return Err(ConfigError::InvalidModel(
                "model name is empty after provider prefix".into(),
            ));
        }

        Ok(Model {
            provider,
            name: model_name.to_string(),
        })
    }
}

// ── serde for Model ─────────────────────────────────────────────────
//
// Model serializes as a plain string like "deepseek/deepseek-v4-pro"
// so existing config files remain valid.

impl Serialize for Model {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.to_string().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Model {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ── tool groups ─────────────────────────────────────────────────────

/// Groups of tools that can be enabled/disabled in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    /// `read`, `write`, `replace`, `read_html`, `cd`
    FileOps,
    /// `shell`
    Shell,
    /// `ssh`, `disconnect`
    Ssh,
    /// `screenshot`, `cursor_position`, `mouse_*`, `key_*`, `window_*`, `open_url`
    ComputerUse,
    /// `web_fetch`
    WebFetch,
}

fn default_tool_groups() -> Vec<ToolGroup> {
    vec![
        ToolGroup::FileOps,
        ToolGroup::Shell,
        ToolGroup::Ssh,
        ToolGroup::WebFetch,
    ]
}

// ── MCP server definition ──────────────────────────────────────────

/// Transport for an MCP server — either HTTP or stdio subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// Streamable HTTP endpoint.
    Http { url: String },
    /// Spawn a child process, communicate over stdio.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
    },
}

/// A named MCP server entry in the config registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    /// Transport configuration.
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Whether this server should be instantiated once and shared across
    /// all sessions that enable it (default: false).
    #[serde(default)]
    pub shared: bool,
}

fn default_mcp_servers() -> std::collections::HashMap<String, McpServerDef> {
    std::collections::HashMap::new()
}

fn default_enabled_mcp_servers() -> Vec<String> {
    Vec::new()
}

// ── config ──────────────────────────────────────────────────────────

/// Compaction budget for the conversation memory.
///
/// When set, messages exceeding the budget are evicted from the active
/// window and replaced with a rolling text summary (no extra LLM call).
/// `None` disables compaction (unlimited context).
///
/// In config files this accepts either a bare integer (absolute tokens)
/// or a string like `"80%"` (percentage of the model's context window).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum CompactionMode {
    /// Absolute token budget.
    Tokens(usize),
    /// Percentage of the model's context window (0–100).
    Percent(u8),
}

impl<'de> Deserialize<'de> for CompactionMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CompactionMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an integer (absolute tokens) or a string like \"80%\"")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(CompactionMode::Tokens(v as usize))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(CompactionMode::Tokens(v as usize))
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                // "80%" → Percent(80)
                if let Some(pct_str) = s.strip_suffix('%') {
                    let pct: u8 = pct_str
                        .trim()
                        .parse()
                        .map_err(|_| E::custom(format_args!("invalid percentage: {s:?}")))?;
                    if pct > 100 {
                        return Err(E::custom(format_args!(
                            "percentage out of range 0–100: {pct}"
                        )));
                    }
                    return Ok(CompactionMode::Percent(pct));
                }
                // Bare integer string → Tokens (for env vars like GOOP_COMPACTION=64000)
                if let Ok(n) = s.trim().parse::<usize>() {
                    return Ok(CompactionMode::Tokens(n));
                }
                Err(E::custom(format_args!(
                    "expected integer or percentage string like \"80%\", got {s:?}"
                )))
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// Effective configuration, built by merging all layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Computed once at startup; not serialized.
    #[serde(skip, default = "home_dir")]
    pub home_dir: PathBuf,

    /// Model in litellm-style `provider/model` format — parsed on
    /// deserialization via [`Model`]'s [`Deserialize`] impl.
    #[serde(default = "default_model")]
    pub model: Model,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_max_turns")]
    pub default_max_turns: usize,
    /// Compaction budget for the conversation memory.
    ///
    /// When set, messages exceeding the budget are evicted from the active
    /// window and replaced with a rolling text summary (no extra LLM call).
    /// `None` disables compaction (unlimited context).
    ///
    /// In config files, either a bare integer (absolute token limit) or a
    /// string like `"80%"` (percentage of the model's context window).
    #[serde(default)]
    pub compaction: Option<CompactionMode>,
    #[serde(default = "default_tool_groups")]
    pub enabled_tool_groups: Vec<ToolGroup>,

    /// Base URL for the Ollama API.  Only used when provider is `ollama`.
    /// Override via `GOOP_OLLAMA_BASE_URL` env var or `ollama_base_url` in config.toml.
    #[serde(default = "default_ollama_base_url")]
    pub ollama_base_url: String,

    /// MCP server registry — maps server names to their definitions.
    #[serde(default = "default_mcp_servers")]
    pub mcp_servers: std::collections::HashMap<String, McpServerDef>,

    /// Names of MCP servers to enable globally (for all sessions).
    #[serde(default = "default_enabled_mcp_servers")]
    pub enabled_mcp_servers: Vec<String>,
}

fn default_model() -> Model {
    Provider::DeepSeek.default_model()
}

fn default_max_tokens() -> u64 {
    100_000
}

fn default_max_turns() -> usize {
    100
}

fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home_dir: home_dir(),
            model: default_model(),
            max_tokens: default_max_tokens(),
            default_max_turns: default_max_turns(),
            compaction: None,
            enabled_tool_groups: default_tool_groups(),
            ollama_base_url: default_ollama_base_url(),
            mcp_servers: default_mcp_servers(),
            enabled_mcp_servers: default_enabled_mcp_servers(),
        }
    }
}

impl Config {
    /// Whether the given tool group is enabled.
    pub fn has_tool_group(&self, group: ToolGroup) -> bool {
        self.enabled_tool_groups.contains(&group)
    }

    /// The parsed provider.
    pub fn provider(&self) -> Provider {
        self.model.provider()
    }

    /// The model name (everything after the first `/`).
    pub fn model_name(&self) -> &str {
        self.model.model_name()
    }
}

// ── session config ──────────────────────────────────────────────────

/// Per-session overrides for [`Config`].  All fields are optional —
/// `None` means "defer to the global config".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionConfig {
    /// Override the model (provider/model format).  `None` means "defer to global".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_max_turns: Option<usize>,
    /// Override the compaction budget.  `None` means "defer to global".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_tool_groups: Option<Vec<ToolGroup>>,
    /// Override the Ollama base URL.  `None` means "defer to global".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_base_url: Option<String>,
    /// Names of MCP servers to enable for this session (adds to the
    /// global list — no need to repeat globally-enabled names here).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_mcp_servers: Option<Vec<String>>,
}

impl SessionConfig {
    /// Merge these overrides into a clone of `config`, returning a new
    /// [`Config`].  Any `Some` value here replaces the corresponding
    /// field from `config`.
    pub fn merge(&self, config: &Config) -> Config {
        let mut merged = config.clone();

        if let Some(ref m) = self.model {
            // Parse the session-level model string; fall back to
            // the global model if parsing fails.
            if let Ok(parsed) = m.parse::<Model>() {
                merged.model = parsed;
            }
        }
        if let Some(t) = self.max_tokens {
            merged.max_tokens = t;
        }
        if let Some(t) = self.default_max_turns {
            merged.default_max_turns = t;
        }
        if let Some(ref c) = self.compaction {
            merged.compaction = Some(c.clone());
        }
        if let Some(ref g) = self.enabled_tool_groups {
            merged.enabled_tool_groups = g.clone();
        }
        if let Some(ref u) = self.ollama_base_url {
            merged.ollama_base_url = u.clone();
        }
        // enabled_mcp_servers is NOT merged here — it's a union
        // (global + session) computed in Session::new.

        merged
    }

    /// Return the session-level MCP server enablement, if any.
    pub fn mcp_server_names(&self) -> &[String] {
        self.enabled_mcp_servers.as_deref().unwrap_or(&[])
    }
}

// ── CLI overrides ───────────────────────────────────────────────────

/// Overrides coming from CLI flags (highest precedence).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ── loading ─────────────────────────────────────────────────────────

/// Load configuration by merging all layers, then validate.
///
/// Precedence (highest wins):
/// 1. `cli` — CLI flags (`--model`)
/// 2. `GOOP_MODEL` env var
/// 3. Session state file (`<name>.state.toml` → `config` section)
/// 4. Global config file (`~/.config/goop/config.toml`)
/// 5. Hard-coded defaults
///
/// If no global config file exists, a default is written before
/// proceeding (see [`write_default_config`]).
pub fn load_config(
    cli: Option<&CliOverrides>,
    session_name: Option<&str>,
) -> std::result::Result<Config, anyhow::Error> {
    let global_path = global_config_path();

    // Write a default global config if none exists.
    if !global_path.exists() {
        let defaults = Config::default();
        write_default_config(&global_path, &defaults)?;
    }

    use figment::Figment;
    use figment::providers::Format;
    use figment::providers::{Env, Serialized, Toml};

    let mut fig = Figment::new()
        // Layer 5: hard-coded defaults
        .merge(Serialized::defaults(Config::default()))
        // Layer 4: global config file
        .merge(Toml::file(&global_path));

    // Layer 3: session config (from <name>.state.toml)
    if let Some(name) = session_name {
        let state_path = crate::session::session_state_path(name);
        if state_path.exists() {
            let session_fig = Figment::new()
                .merge(Toml::file(&state_path))
                .select("config");
            fig = fig.merge(session_fig);
        }
    }

    // Layer 2: environment variables
    fig = fig.merge(Env::prefixed("GOOP_"));

    // Layer 1: CLI overrides
    if let Some(cli) = cli {
        fig = fig.merge(Serialized::from(cli, figment::Profile::Default));
    }

    let mut config: Config = fig
        .extract()
        .map_err(|e| ConfigError::Extraction(e.to_string()))?;

    // home_dir is never in any provider — set it explicitly.
    config.home_dir = home_dir();

    Ok(config)
}

// ── default config file ─────────────────────────────────────────────

/// Write a well-commented default config file so users have something to
/// inspect and edit.  Creates the parent directory if needed.
///
/// The template lives in `assets/default_config.toml` and is embedded at
/// compile time via `include_str!`, then rendered with Tera.
fn write_default_config(
    path: &std::path::Path,
    config: &Config,
) -> std::result::Result<(), anyhow::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let groups: Vec<&str> = config
        .enabled_tool_groups
        .iter()
        .map(|g| match g {
            ToolGroup::FileOps => "file_ops",
            ToolGroup::Shell => "shell",
            ToolGroup::Ssh => "ssh",
            ToolGroup::WebFetch => "web_fetch",
            ToolGroup::ComputerUse => "computer_use",
        })
        .collect();

    let mut context = tera::Context::new();
    context.insert("model", &config.model.to_string());
    context.insert("max_tokens", &config.max_tokens);
    context.insert("default_max_turns", &config.default_max_turns);
    context.insert("groups", &groups);
    context.insert("ollama_base_url", &config.ollama_base_url);

    let template = include_str!("../assets/default_config.toml");
    let contents = tera::Tera::one_off(template, &context, false)
        .map_err(|e| ConfigError::Template(e.to_string()))?;

    std::fs::write(path, contents)?;
    tracing::info!("Wrote default config to {}", path.display());
    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────

/// Read the API key for the configured provider from its environment variable.
/// Returns an error if the env var is missing (except for Ollama which needs no key).
pub fn api_key_for(provider: Provider) -> std::result::Result<String, ConfigError> {
    let var = provider.api_key_env();
    if var.is_empty() {
        // Ollama — no key needed.
        return Ok(String::new());
    }
    std::env::var(var).map_err(|_| {
        ConfigError::MissingApiKey(format!(
            "{var} environment variable not set. Set it or use a different \
             model (GOOP_MODEL=provider/model)"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Model ──────────────────────────────────────────────────────

    #[test]
    fn model_parse_basic() {
        let m: Model = "deepseek/deepseek-v4-pro".parse().unwrap();
        assert_eq!(m.provider(), Provider::DeepSeek);
        assert_eq!(m.model_name(), "deepseek-v4-pro");
    }

    #[test]
    fn model_parse_nested() {
        let m: Model = "openrouter/openai/gpt-4o".parse().unwrap();
        assert_eq!(m.provider(), Provider::OpenRouter);
        assert_eq!(m.model_name(), "openai/gpt-4o");
    }

    #[test]
    fn model_parse_all_providers() {
        for input in [
            "deepseek/x",
            "openai/x",
            "openrouter/x",
            "groq/x",
            "ollama/x",
            "anthropic/x",
        ] {
            let m: Model = input.parse().unwrap();
            assert_eq!(m.to_string(), input);
        }
    }

    #[test]
    fn model_parse_unknown_provider() {
        assert!(matches!(
            "foobar/gpt-4".parse::<Model>().unwrap_err(),
            ConfigError::InvalidModel(_)
        ));
    }

    #[test]
    fn model_parse_no_slash() {
        assert!(matches!(
            "deepseek-v4-pro".parse::<Model>().unwrap_err(),
            ConfigError::InvalidModel(_)
        ));
    }

    #[test]
    fn model_parse_empty_name() {
        assert!(matches!(
            "deepseek/".parse::<Model>().unwrap_err(),
            ConfigError::InvalidModel(_)
        ));
    }

    #[test]
    fn model_display_roundtrips() {
        let inputs = [
            "deepseek/deepseek-v4-pro",
            "openai/gpt-4o",
            "openrouter/openai/gpt-4o",
            "ollama/llama3.2",
        ];
        for input in inputs {
            let m: Model = input.parse().unwrap();
            assert_eq!(m.to_string(), input);
        }
    }

    // ── Config methods ────────────────────────────────────────────

    #[test]
    fn config_provider_and_model_name() {
        let config = Config {
            model: "openai/gpt-4o-mini".parse().unwrap(),
            ..Config::default()
        };
        assert_eq!(config.provider(), Provider::OpenAI);
        assert_eq!(config.model_name(), "gpt-4o-mini");
    }

    #[test]
    fn default_config_uses_deepseek() {
        let config = Config::default();
        assert_eq!(config.provider(), Provider::DeepSeek);
        assert_eq!(config.model_name(), "deepseek-v4-pro");
    }

    // ── serde round-trip ──────────────────────────────────────────

    #[test]
    fn model_serialize_deserialize() {
        let m: Model = "openai/gpt-4o".parse().unwrap();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#""openai/gpt-4o""#);
        let m2: Model = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    // ── default config file round-trip ────────────────────────────

    #[test]
    fn default_config_round_trips() {
        let defaults = Config::default();

        let tmp = std::env::temp_dir().join("goop-test-config.toml");
        write_default_config(&tmp, &defaults).unwrap();

        let contents = std::fs::read_to_string(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);

        // Must be valid TOML and parse back to Config.
        let parsed: Config = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.model.to_string(), "deepseek/deepseek-v4-pro");
        assert_eq!(parsed.max_tokens, 100_000);
        assert_eq!(parsed.default_max_turns, 100);
        assert_eq!(parsed.enabled_tool_groups.len(), 4);
        assert!(parsed.has_tool_group(ToolGroup::FileOps));
        assert!(parsed.has_tool_group(ToolGroup::Shell));

        // The file should contain the model in litellm format.
        assert!(contents.contains("goop configuration"));
        assert!(contents.contains("GOOP_MODEL"));
        assert!(contents.contains("deepseek/deepseek-v4-pro"));
    }

    // ── session config merge ──────────────────────────────────────

    #[test]
    fn session_config_merge() {
        let config = Config::default();
        assert_eq!(config.model.to_string(), "deepseek/deepseek-v4-pro");

        let session = SessionConfig {
            model: Some("openai/gpt-4o-mini".into()),
            ..Default::default()
        };
        let merged = session.merge(&config);

        assert_eq!(merged.model.to_string(), "openai/gpt-4o-mini");
        assert_eq!(merged.provider(), Provider::OpenAI);
        assert_eq!(merged.model_name(), "gpt-4o-mini");
        // Unset fields retain defaults.
        assert_eq!(merged.max_tokens, 100_000);
        // Original is unchanged.
        assert_eq!(config.model.to_string(), "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn session_config_partial_merge() {
        let config = Config::default();
        let session = SessionConfig {
            max_tokens: Some(50_000),
            ..Default::default()
        };
        let merged = session.merge(&config);

        // Model unchanged.
        assert_eq!(merged.model.to_string(), "deepseek/deepseek-v4-pro");
        // Max tokens overridden.
        assert_eq!(merged.max_tokens, 50_000);
    }

    #[test]
    fn session_config_merge_preserves_original() {
        let config = Config::default();
        let session = SessionConfig {
            max_tokens: Some(42),
            ..Default::default()
        };
        let _merged = session.merge(&config);
        // Original must be unchanged.
        assert_eq!(config.max_tokens, 100_000);
    }

    // ── TOML deserialization of Config ────────────────────────────

    #[test]
    fn deserialize_config_minimal() {
        let toml_str = r#"model = "openai/gpt-4o""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model.to_string(), "openai/gpt-4o");
        assert_eq!(config.provider(), Provider::OpenAI);
        assert_eq!(config.model_name(), "gpt-4o");
        // Defaults for omitted fields.
        assert_eq!(config.max_tokens, 100_000);
        assert_eq!(config.enabled_tool_groups.len(), 4);
    }

    #[test]
    fn deserialize_config_invalid_model() {
        let toml_str = r#"model = "foobar/x""#;
        let result = toml::from_str::<Config>(toml_str);
        assert!(result.is_err());
    }

    // ── MCP config deserialization ─────────────────────────────────

    #[test]
    fn deserialize_mcp_http() {
        let toml_str = r#"
model = "openai/gpt-4o"

[mcp_servers.telegram]
type = "http"
url = "http://localhost:8080"
shared = true
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let def = config.mcp_servers.get("telegram").unwrap();
        assert!(def.shared);
        match &def.transport {
            McpTransport::Http { url } => assert_eq!(url, "http://localhost:8080"),
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_mcp_stdio() {
        let toml_str = r#"
model = "openai/gpt-4o"

[mcp_servers.code_indexer]
type = "stdio"
command = "my-indexer"
args = ["--project", "."]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let def = config.mcp_servers.get("code_indexer").unwrap();
        assert!(!def.shared); // default
        match &def.transport {
            McpTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "my-indexer");
                assert_eq!(args, &vec!["--project", "."]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn deserialize_mcp_enabled_servers() {
        let toml_str = r#"
model = "openai/gpt-4o"
enabled_mcp_servers = ["telegram", "code_indexer"]

[mcp_servers.telegram]
type = "http"
url = "http://localhost:8080"

[mcp_servers.code_indexer]
type = "stdio"
command = "my-indexer"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.enabled_mcp_servers, vec!["telegram", "code_indexer"]);
        assert_eq!(config.mcp_servers.len(), 2);
    }

    // ── Provider helpers ──────────────────────────────────────────

    // ── CompactionMode deserialization ────────────────────────────

    #[test]
    fn compaction_mode_tokens_integer() {
        let toml_str = r#"compaction = 64000"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        match config.compaction {
            Some(CompactionMode::Tokens(n)) => assert_eq!(n, 64000),
            other => panic!("expected Tokens(64000), got {other:?}"),
        }
    }

    #[test]
    fn compaction_mode_percent_string() {
        let toml_str = r#"compaction = "80%""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        match config.compaction {
            Some(CompactionMode::Percent(p)) => assert_eq!(p, 80),
            other => panic!("expected Percent(80), got {other:?}"),
        }
    }

    #[test]
    fn compaction_mode_none() {
        let toml_str = r#"model = "openai/gpt-4o""#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.compaction.is_none());
    }

    #[test]
    fn compaction_mode_invalid_string() {
        let toml_str = r#"compaction = "bogus""#;
        let result = toml::from_str::<Config>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn compaction_mode_percent_out_of_range() {
        let toml_str = r#"compaction = "150%""#;
        let result = toml::from_str::<Config>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn provider_as_str_roundtrips() {
        for (s, p) in [
            ("deepseek", Provider::DeepSeek),
            ("openai", Provider::OpenAI),
            ("openrouter", Provider::OpenRouter),
            ("groq", Provider::Groq),
            ("ollama", Provider::Ollama),
            ("anthropic", Provider::Anthropic),
        ] {
            assert_eq!(Provider::from_model_prefix(s), Some(p));
            assert_eq!(p.as_str(), s);
        }
    }
}
