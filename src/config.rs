//! Configuration for goop: provider selection, model, tuning knobs,
//! and tool-group toggles.
//!
//! Reads from `~/.config/goop/config.toml` with env-var overrides.
//! Falls back to DeepSeek defaults for backward compatibility.
//!
//! Configuration layering (highest wins):
//! 1. CLI flags (`--model`, `--provider`)
//! 2. Environment variables (`GOOP_PROVIDER`, `GOOP_MODEL`)
//! 3. Session config (`<name>.state.json` → `config`)
//! 4. Global config (`~/.config/goop/config.toml`)
//! 5. Hard-coded defaults

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── paths ───────────────────────────────────────────────────────────

/// Return the user's home directory, computing it once at startup.
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    home_dir().join(".config").join("goop")
}

pub fn global_config_path() -> PathBuf {
    config_dir().join("config.toml")
}

// ── provider ────────────────────────────────────────────────────────

/// Supported LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
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
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::DeepSeek => "deepseek-v4-pro",
            Provider::OpenAI => "gpt-4o",
            Provider::OpenRouter => "openai/gpt-4o",
            Provider::Groq => "llama-3.2-70b-versatile",
            Provider::Ollama => "llama3.2",
            Provider::Anthropic => "claude-sonnet-4-6",
        }
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

// ── config ──────────────────────────────────────────────────────────

/// Effective configuration, built by merging all layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Computed once at startup; not serialized.
    #[serde(skip, default = "home_dir")]
    pub home_dir: PathBuf,

    #[serde(default)]
    pub provider: Provider,
    /// Model name.  `None` means "use the provider default".
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_max_turns")]
    pub default_max_turns: usize,
    #[serde(default = "default_tool_groups")]
    pub enabled_tool_groups: Vec<ToolGroup>,
}

fn default_max_tokens() -> u64 {
    100_000
}

fn default_max_turns() -> usize {
    100
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home_dir: home_dir(),
            provider: Provider::default(),
            model: None,
            max_tokens: default_max_tokens(),
            default_max_turns: default_max_turns(),
            enabled_tool_groups: default_tool_groups(),
        }
    }
}

impl Config {
    /// Whether the given tool group is enabled.
    pub fn has_tool_group(&self, group: ToolGroup) -> bool {
        self.enabled_tool_groups.contains(&group)
    }

    /// Resolve the effective model name (provider default if none set).
    pub fn effective_model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| self.provider.default_model().to_string())
    }
}

// ── session config ──────────────────────────────────────────────────

/// Per-session overrides for [`Config`].  All fields are optional —
/// `None` means "defer to the global config".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_max_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_tool_groups: Option<Vec<ToolGroup>>,
}

impl SessionConfig {
    /// Merge these overrides into a [`Config`].  Any `Some` value here
    /// replaces the corresponding field in `config`.
    pub fn merge_into(&self, config: &mut Config) {
        if let Some(p) = self.provider {
            config.provider = p;
        }
        if let Some(ref m) = self.model {
            config.model = Some(m.clone());
        }
        if let Some(t) = self.max_tokens {
            config.max_tokens = t;
        }
        if let Some(t) = self.default_max_turns {
            config.default_max_turns = t;
        }
        if let Some(ref g) = self.enabled_tool_groups {
            config.enabled_tool_groups = g.clone();
        }
    }
}

// ── CLI overrides ───────────────────────────────────────────────────

/// Overrides coming from CLI flags (highest precedence).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ── loading ─────────────────────────────────────────────────────────

/// Load configuration by merging all layers.
///
/// Precedence (highest wins):
/// 1. `cli` — CLI flags (`--model`, `--provider`)
/// 2. `GOOP_PROVIDER`, `GOOP_MODEL` env vars
/// 3. Session state file (`<name>.state.json` → `config` section)
/// 4. Global config file (`~/.config/goop/config.toml`)
/// 5. Hard-coded defaults
///
/// If no global config file exists, a default is written before
/// proceeding (see [`write_default_config`]).
pub fn load_config(
    cli: Option<&CliOverrides>,
    session_name: Option<&str>,
) -> anyhow::Result<Config> {
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

    // Layer 3: session config (from <name>.state.json)
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

    let mut config: Config = fig.extract()?;

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
fn write_default_config(path: &std::path::Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let model = config.effective_model();

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
    context.insert("provider", serde_lowercase(&config.provider));
    context.insert("model", &model);
    context.insert("max_tokens", &config.max_tokens);
    context.insert("default_max_turns", &config.default_max_turns);
    context.insert("groups", &groups);

    let template = include_str!("../assets/default_config.toml");
    let contents = tera::Tera::one_off(template, &context, false)
        .map_err(|e| anyhow::anyhow!("Failed to render default config template: {e}"))?;

    std::fs::write(path, contents)?;
    tracing::info!("Wrote default config to {}", path.display());
    Ok(())
}

/// Return the serde-expected lowercase name for a provider.
fn serde_lowercase(p: &Provider) -> &'static str {
    match p {
        Provider::DeepSeek => "deepseek",
        Provider::OpenAI => "openai",
        Provider::OpenRouter => "openrouter",
        Provider::Groq => "groq",
        Provider::Ollama => "ollama",
        Provider::Anthropic => "anthropic",
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Read the API key for the configured provider from its environment variable.
/// Returns an error if the env var is missing (except for Ollama which needs no key).
pub fn api_key_for(provider: Provider) -> anyhow::Result<String> {
    let var = provider.api_key_env();
    if var.is_empty() {
        // Ollama — no key needed.
        return Ok(String::new());
    }
    std::env::var(var).map_err(|_| {
        anyhow::anyhow!(
            "{var} environment variable not set. Set it or use a different provider (GOOP_PROVIDER=...)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips() {
        // Simulate what happens on first run: create defaults, write, re-parse.
        let defaults = Config::default();

        let tmp = std::env::temp_dir().join("goop-test-config.toml");
        write_default_config(&tmp, &defaults).unwrap();

        let contents = std::fs::read_to_string(&tmp).unwrap();
        let _ = std::fs::remove_file(&tmp);

        // Must be valid TOML and parse back to Config.
        let parsed: Config = toml::from_str(&contents).unwrap();
        assert_eq!(parsed.provider, Provider::DeepSeek);
        assert_eq!(parsed.model.as_deref().unwrap(), "deepseek-v4-pro");
        assert_eq!(parsed.max_tokens, 100_000);
        assert_eq!(parsed.default_max_turns, 100);
        assert_eq!(parsed.enabled_tool_groups.len(), 4);
        assert!(parsed.has_tool_group(ToolGroup::FileOps));
        assert!(parsed.has_tool_group(ToolGroup::Shell));

        // The file should contain comments we wrote.
        assert!(contents.contains("goop configuration"));
        assert!(contents.contains("GOOP_PROVIDER"));
        assert!(contents.contains("deepseek"));
    }

    #[test]
    fn session_config_merge() {
        let mut config = Config::default();
        assert_eq!(config.provider, Provider::DeepSeek);

        let session = SessionConfig {
            provider: Some(Provider::OpenAI),
            model: Some("gpt-4o-mini".into()),
            ..Default::default()
        };
        session.merge_into(&mut config);

        assert_eq!(config.provider, Provider::OpenAI);
        assert_eq!(config.model.as_deref(), Some("gpt-4o-mini"));
        // Unset fields retain defaults.
        assert_eq!(config.max_tokens, 100_000);
    }

    #[test]
    fn load_config_layering() {
        // With no env vars set and no session, we get defaults.
        let config = load_config(None, None).unwrap();
        assert_eq!(config.provider, Provider::DeepSeek);
        // The default config file writes the resolved model, so it's
        // Some on load (not None waiting for lazy resolution).
        assert_eq!(config.effective_model(), "deepseek-v4-pro");
    }
}
