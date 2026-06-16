//! Configuration for goop: provider selection, model, tuning knobs,
//! and tool-group toggles.
//!
//! Reads from `~/.config/goop/config.toml` with env-var overrides.
//! Falls back to DeepSeek defaults for backward compatibility.

use std::path::PathBuf;

use serde::Deserialize;

// ── config file ──────────────────────────────────────────────────

/// Return the user's home directory, computing it once at startup.
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    home_dir().join(".config").join("goop")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Supported LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
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

// ── tool groups ──────────────────────────────────────────────────

/// Groups of tools that can be enabled/disabled in config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

// ── config struct ────────────────────────────────────────────────

/// Parsed configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Computed once at startup; not serialized.
    #[serde(skip)]
    pub home_dir: PathBuf,

    #[serde(default = "default_provider")]
    pub provider: Provider,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    #[serde(default = "default_max_turns")]
    pub default_max_turns: usize,
    #[serde(default = "default_tool_groups")]
    pub enabled_tool_groups: Vec<ToolGroup>,
}

fn default_provider() -> Provider {
    Provider::DeepSeek
}

fn default_max_tokens() -> u64 {
    100_000
}

fn default_max_turns() -> usize {
    100
}

impl Config {
    pub fn has_tool_group(&self, group: ToolGroup) -> bool {
        self.enabled_tool_groups.contains(&group)
    }
}

// ── loading ──────────────────────────────────────────────────────

/// Load configuration from disk + environment, returning the effective config.
///
/// Precedence (highest wins):
/// 1. `GOOP_MODEL` env var
/// 2. `GOOP_PROVIDER` env var
/// 3. `~/.config/goop/config.toml`
/// 4. hard-coded defaults (DeepSeek, deepseek-v4-pro)
///
/// If no config file exists, a default one is written to disk so the user
/// has something to inspect and edit.
pub fn load_config() -> anyhow::Result<Config> {
    let config_path = config_path();

    let mut config = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)?;
        toml::from_str(&contents)?
    } else {
        let defaults = Config {
            home_dir: home_dir(),
            provider: default_provider(),
            model: None, // will be resolved below
            max_tokens: default_max_tokens(),
            default_max_turns: default_max_turns(),
            enabled_tool_groups: default_tool_groups(),
        };
        write_default_config(&config_path, &defaults)?;
        defaults
    };

    // Ensure home_dir is always set (won't be from TOML).
    config.home_dir = home_dir();

    // Env-var overrides.
    if let Ok(provider_str) = std::env::var("GOOP_PROVIDER") {
        config.provider = parse_provider(&provider_str)?;
    }
    if let Ok(model) = std::env::var("GOOP_MODEL") {
        config.model = Some(model);
    }

    // Resolve the effective model name.
    if config.model.is_none() {
        config.model = Some(config.provider.default_model().to_string());
    }

    Ok(config)
}

/// Write a well-commented default config file so users have something to
/// inspect and edit.  Creates the parent directory if needed.
///
/// The template lives in `assets/default_config.toml` and is embedded at
/// compile time via `include_str!`, then rendered with Tera.
fn write_default_config(path: &std::path::Path, config: &Config) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let model = config
        .model
        .as_deref()
        .unwrap_or_else(|| config.provider.default_model());

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
    context.insert("model", model);
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

fn parse_provider(s: &str) -> anyhow::Result<Provider> {
    match s.to_lowercase().as_str() {
        "deepseek" => Ok(Provider::DeepSeek),
        "openai" => Ok(Provider::OpenAI),
        "openrouter" => Ok(Provider::OpenRouter),
        "groq" => Ok(Provider::Groq),
        "ollama" => Ok(Provider::Ollama),
        "anthropic" => Ok(Provider::Anthropic),
        other => anyhow::bail!(
            "unknown provider '{other}'. Supported: deepseek, openai, openrouter, groq, ollama, anthropic"
        ),
    }
}

// ── helpers ──────────────────────────────────────────────────────

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
        let defaults = Config {
            home_dir: PathBuf::from("/home/test"),
            provider: default_provider(),
            model: None,
            max_tokens: default_max_tokens(),
            default_max_turns: default_max_turns(),
            enabled_tool_groups: default_tool_groups(),
        };

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
}
