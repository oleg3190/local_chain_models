use anyhow::{Context, Result};
use std::env;

/// Static upstream URLs — these don't change per-deployment, only the
/// keys/models/proxy routing do.
pub const GEMINI_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const DEEPSEEK_URL: &str = "https://api.deepseek.com/chat/completions";
pub const CLOUDFLARE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/96c931f18046f6f5ec2413b6acb3c163/ai/v1/chat/completions";
pub const OPENCODE_GO_URL: &str = "https://opencode.ai/zen/go/v1/chat/completions";

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api_key: String,
    pub model: String,
    pub use_proxy: bool,
}

#[derive(Debug, Clone)]
pub struct ApiKeyPoolConfig {
    pub api_keys: Vec<String>,
    pub model: String,
    pub use_proxy: bool,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub global_outbound_proxy: Option<String>,

    pub gemini: ProviderConfig,
    pub gemini_rpm_limit: u32,
    /// When `true`, requests carrying `tools`/`tool_calls` skip Gemini
    /// and go straight to the fallback chain (legacy behaviour).
    /// Default `false`: Gemini handles tools itself (with rate limiting).
    pub gemini_tools_bypass: bool,
    /// File the Gemini thought-signature cache is persisted to, so a
    /// proxy restart does not lose signatures for an in-flight
    /// conversation. `None` means memory-only.
    pub thought_sig_cache_path: Option<String>,

    pub openrouter: ProviderConfig,
    pub deepseek: ProviderConfig,
    pub cloudflare: ProviderConfig,
    pub opencode_go: ApiKeyPoolConfig,
    pub enable_deepseek_fallback: bool,

    // Model enable/disable flags
    pub gemini_enabled: bool,
    pub openrouter_enabled: bool,
    pub deepseek_enabled: bool,
    pub cloudflare_enabled: bool,
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

impl AppConfig {
    /// Loads and validates configuration from `.env` / process environment.
    /// Fails fast (at startup, not mid-request) if a required key is missing.
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok(); // fine if no .env file is present

        let host = env_or("SERVER_HOST", "0.0.0.0");
        let port: u16 = env_or("SERVER_PORT", "8981")
            .parse()
            .context("SERVER_PORT must be a valid u16")?;

        let global_outbound_proxy = env::var("GLOBAL_OUTBOUND_PROXY").ok().filter(|s| !s.is_empty());

        let gemini = ProviderConfig {
            api_key: env::var("GEMINI_API_KEY").unwrap_or_default(),
            model: env_or("GEMINI_MODEL", "gemini-3.6-flash"),
            use_proxy: env_bool("GEMINI_USE_PROXY", true),
        };
        let gemini_rpm_limit: u32 = env_or("GEMINI_RPM_LIMIT", "5")
            .parse()
            .context("GEMINI_RPM_LIMIT must be a valid u32")?;

        let gemini_tools_bypass = env_bool("GEMINI_TOOLS_BYPASS", false);

        // Persisted Gemini thought-signature cache. "off" (or empty)
        // keeps everything in memory only.
        let thought_sig_cache_path = match env_or(
            "THOUGHT_SIG_CACHE_PATH",
            "~/.cache/llm-router/thought_signatures.jsonl",
        ) {
            v if v.trim().is_empty() || v.trim().eq_ignore_ascii_case("off") => None,
            v => Some(v),
        };

        let openrouter = ProviderConfig {
            api_key: env::var("OPENROUTER_API_KEY").unwrap_or_default(),
            model: env_or("OPENROUTER_MODEL", "openrouter/free"),
            use_proxy: env_bool("OPENROUTER_USE_PROXY", false),
        };

        let enable_deepseek_fallback = env_bool("ENABLE_DEEPSEEK_FALLBACK", true);
        let deepseek = ProviderConfig {
            // DeepSeek is optional unless the fallback is actually enabled.
            api_key: env::var("DEEPSEEK_API_KEY").unwrap_or_default(),
            model: env_or("DEEPSEEK_MODEL", "deepseek-chat"),
            use_proxy: env_bool("DEEPSEEK_USE_PROXY", false),
        };
        let cloudflare = ProviderConfig {
            api_key: env::var("CLOUDFLARE_API_KEY").unwrap_or_default(),
            model: env_or("CLOUDFLARE_MODEL", "@cf/qwen/qwen2.5-coder-32b-instruct"),
            use_proxy: env_bool("CLOUDFLARE_USE_PROXY", false),
        };

        // OpenCode Go supports multiple independent API keys. Keep the
        // first key as the primary one and switch to the next on 429/quota.
        let mut opencode_go_api_keys: Vec<String> = env::var("OPENCODE_GO_API_KEYS")
            .unwrap_or_default()
            .split(
        // Per-provider enable flags: disabled providers are never built and
        // never participate in the fallback chain, so their API keys are
        // not required.
        let gemini_enabled = env_bool("ENABLE_GEMINI", true);
        let openrouter_enabled = env_bool("ENABLE_OPENROUTER", true);
        let deepseek_enabled = env_bool("ENABLE_DEEPSEEK", true);
        let cloudflare_enabled = env_bool("ENABLE_CLOUDFLARE", true);
        let opencode_go_enabled = !opencode_go.api_keys.is_empty();

        if !gemini_enabled && !openrouter_enabled && !deepseek_enabled && !cloudflare_enabled && !opencode_go_enabled {
            anyhow::bail!("all providers are disabled; enable at least one via ENABLE_* flags");
        }

        if gemini_enabled && gemini.api_key.is_empty() {
            anyhow::bail!("ENABLE_GEMINI=true but GEMINI_API_KEY is not set");
        }
        if openrouter_enabled && openrouter.api_key.is_empty() {
            anyhow::bail!("ENABLE_OPENROUTER=true but OPENROUTER_API_KEY is not set");
        }
        if cloudflare_enabled && cloudflare.api_key.is_empty() {
            anyhow::bail!("ENABLE_CLOUDFLARE=true but CLOUDFLARE_API_KEY is not set");
        }
        if enable_deepseek_fallback && !deepseek_enabled {
            anyhow::bail!(
                "ENABLE_DEEPSEEK_FALLBACK=true but ENABLE_DEEPSEEK=false"
            );
        }
        if enable_deepseek_fallback && deepseek.api_key.is_empty() {
            anyhow::bail!(
                "ENABLE_DEEPSEEK_FALLBACK=true but DEEPSEEK_API_KEY is not set"
            );
        }

        let any_proxy_enabled = (gemini_enabled && gemini.use_proxy)
            || (openrouter_enabled && openrouter.use_proxy)
            || (deepseek_enabled && deepseek.use_proxy)
            || (cloudflare_enabled && cloudflare.use_proxy)
            || (opencode_go_enabled && opencode_go.use_proxy);
        if global_outbound_proxy.is_none() && any_proxy_enabled {
            anyhow::bail!(
                "a provider has *_USE_PROXY=true but GLOBAL_OUTBOUND_PROXY is not set"
            );
        }

        Ok(Self {
            host,
            port,
            global_outbound_proxy,
            gemini,
            gemini_rpm_limit,
            gemini_tools_bypass,
            thought_sig_cache_path,
            openrouter,
            deepseek,
            cloudflare,
            opencode_go,
            enable_deepseek_fallback,
            gemini_enabled,
            openrouter_enabled,
            deepseek_enabled,
            cloudflare_enabled,
        })
    }
}
