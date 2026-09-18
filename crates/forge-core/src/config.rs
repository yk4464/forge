use serde::{Deserialize, Serialize};

/// Top-level config.toml shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub context: ContextConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    /// Which protocol adapter to use: "openai" (chat/completions),
    /// "responses", "anthropic" (messages).
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub base_url: String,
    /// Env var name holding the API key.
    #[serde(default)]
    pub api_key_env: String,
    /// Inline API key (takes precedence over env var). Keep out of VCS.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
}

fn default_protocol() -> String {
    "openai".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default)]
    pub name: String,
    /// Sampling temperature; None = provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default = "default_max_tokens", skip_serializing_if = "is_zero")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    8192
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// Context-window management parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Raw model context window in tokens.
    #[serde(default = "default_window")]
    pub context_window: i64,
    /// Optional explicit auto-compact threshold; clamped to 90% of the
    /// effective window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<i64>,
    /// Per-tool-output byte cap when recording into history.
    #[serde(default = "default_tool_output_bytes")]
    pub tool_output_max_bytes: usize,
    /// Optional explicit bash.exe path; None = auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    /// Default shell tool timeout in seconds (max 600).
    #[serde(default = "default_shell_timeout")]
    pub shell_timeout_secs: u64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            context_window: default_window(),
            auto_compact_token_limit: None,
            tool_output_max_bytes: default_tool_output_bytes(),
            shell_path: None,
            shell_timeout_secs: default_shell_timeout(),
        }
    }
}

fn default_window() -> i64 {
    128_000
}

fn default_tool_output_bytes() -> usize {
    1 << 20 // 1 MiB
}

fn default_shell_timeout() -> u64 {
    120
}

impl Config {
    /// Load from a TOML file path.
    pub fn load(path: &std::path::Path) -> crate::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let cfg: Config = toml::from_str(&raw)
            .map_err(|e| crate::Error::Config(format!("invalid TOML in {}: {e}", path.display())))?;
        Ok(cfg)
    }

    /// Resolve the API key: inline provider.api_key wins, else env var
    /// named by provider.api_key_env (default FORGE_API_KEY).
    pub fn resolve_api_key(&self) -> crate::Result<String> {
        if !self.provider.api_key.trim().is_empty() {
            return Ok(self.provider.api_key.trim().to_string());
        }
        let name = if self.provider.api_key_env.is_empty() {
            "FORGE_API_KEY"
        } else {
            &self.provider.api_key_env
        };
        std::env::var(name)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().to_string())
            .ok_or_else(|| {
                crate::Error::Config(format!(
                    "API key not set: fill provider.api_key in config.toml or set env {name}"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_toml() {
        let raw = r#"
[provider]
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[model]
name = "deepseek-chat"

[context]
context_window = 131072
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.model.name, "deepseek-chat");
        assert_eq!(cfg.context.context_window, 131072);
        assert_eq!(cfg.context.shell_timeout_secs, 120);
        assert_eq!(cfg.model.max_tokens, 8192);
    }
}
