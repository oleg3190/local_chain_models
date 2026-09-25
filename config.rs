use anyhow::{Context, Result};
use std::env;
use std::time::Duration;

pub const GEMINI_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const DEEPSEEK_URL: &str = "https://api.deepseek.com/chat/completions";
pub const CLOUDFLARE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/96c931f18046f6f5ec2413b6acb3c163/ai/v1/chat/completions";

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub api_key: String,
    pub model: String,
    pub use_proxy: bool,
}

#[derive(Debug, Clone)]
pub struct AgyConfig {
    pub enabled: bool,
    pub as_fallback: bool,
    pub model_id: String,
    pub remote_model: Option<String>,
    pub agent: String,
    pub effort: Option<String>,
    pub agy_path: String,
    pub ssh_binary: String,
    pub ssh_host: String,
    pub ssh_args: Vec<String>,
    pub remote_cwd: String,
    pub timeout: Duration,
    pub print_timeout_seconds: u64,
    pub allowed_init_tools: String,
    pub allow_text_fallback: bool,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub global_outbound_proxy: Option<String>,
    pub gemini: ProviderConfig,
    pub gemini_rpm_limit: u32,
    pub gemini_tools_bypass: bool,
    pub thought_sig_cache_path: Option<String>,
    pub openrouter: ProviderConfig,
    pub deepseek: ProviderConfig,
    pub cloudflare: ProviderConfig,
    pub enable_deepseek_fallback: bool,
    pub agy: AgyConfig,
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
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let host = env_or("SERVER_HOST", "0.0.0.0");
        let port: u16 = env_or("SERVER_PORT", "8981")
            .parse()
            .context("SERVER_PORT must be a valid u16")?;

        let global_outbound_proxy = env::var("GLOBAL_OUTBOUND_PROXY")
            .ok()
            .filter(|s| !s.is_empty());

        let gemini = ProviderConfig {
            api_key: env::var("GEMINI_API_KEY").unwrap_or_default(),
            model: env_or("GEMINI_MODEL", "gemini-3.6-flash"),
            use_proxy: env_bool("GEMINI_USE_PROXY", true),
        };
        let gemini_rpm_limit: u32 = env_or("GEMINI_RPM_LIMIT", "5")
            .parse()
            .context("GEMINI_RPM_LIMIT must be a valid u32")?;

        let gemini_tools_bypass = env_bool("GEMINI_TOOLS_BYPASS", false);

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
            api_key: env::var("DEEPSEEK_API_KEY").unwrap_or_default(),
            model: env_or("DEEPSEEK_MODEL", "deepseek-chat"),
            use_proxy: env_bool("DEEPSEEK_USE_PROXY", false),
        };

        let cloudflare = ProviderConfig {
            api_key: env::var("CLOUDFLARE_API_KEY").unwrap_or_default(),
            model: env_or(
                "CLOUDFLARE_MODEL",
                "@cf/qwen/qwen2.5-coder-32b-instruct",
            ),
            use_proxy: env_bool("CLOUDFLARE_USE_PROXY", false),
        };

        let agy_enabled = env_bool("ENABLE_AGY", false);
        let agy_ssh_host = env::var("AGY_SSH_HOST").unwrap_or_default();
        let agy_args_raw = env_or(
            "AGY_SSH_ARGS_JSON",
            r#"["-T","-o","BatchMode=yes","-o","RequestTTY=no"]"#,
        );
        let agy_ssh_args: Vec<String> = serde_json::from_str(&agy_args_raw)
            .context("AGY_SSH_ARGS_JSON must be a JSON array of strings")?;

        if agy_enabled && agy_ssh_host.trim().is_empty() {
            anyhow::bail!("ENABLE_AGY=true but AGY_SSH_HOST is not set");
        }

        let agy_timeout_seconds: u64 = env_or("AGY_TIMEOUT_SECONDS", "600")
            .parse()
            .context("AGY_TIMEOUT_SECONDS must be a valid u64")?;
        if agy_timeout_seconds == 0 {
            anyhow::bail!("AGY_TIMEOUT_SECONDS must be greater than zero");
        }

        let agy = AgyConfig {
            enabled: agy_enabled,
            as_fallback: env_bool("AGY_AS_FALLBACK", false),
            model_id: env_or("AGY_MODEL_ID", "agy"),
            remote_model: env::var("AGY_REMOTE_MODEL").ok().filter(|v| !v.trim().is_empty()),
            agent: env_or("AGY_AGENT", "agy-llm"),
            effort: env::var("AGY_EFFORT").ok().filter(|v| !v.trim().is_empty()),
            agy_path: env_or("AGY_PATH", "agy"),
            ssh_binary: env_or("AGY_SSH_BINARY", "ssh"),
            ssh_host: agy_ssh_host,
            ssh_args: agy_ssh_args,
            remote_cwd: env_or("AGY_REMOTE_CWD", "."),
            timeout: Duration::from_secs(agy_timeout_seconds),
            print_timeout_seconds: agy_timeout_seconds,
            allowed_init_tools: env_or("AGY_ALLOWED_INIT_TOOLS", "finish"),
            allow_text_fallback: env_bool("AGY_ALLOW_TEXT_FALLBACK", false),
        };

        let gemini_enabled = env_bool("ENABLE_GEMINI", true);
        let openrouter_enabled = env_bool("ENABLE_OPENROUTER", true);
        let deepseek_enabled = env_bool("ENABLE_DEEPSEEK", true);
        let cloudflare_enabled = env_bool("ENABLE_CLOUDFLARE", true);

        if !gemini_enabled
            && !openrouter_enabled
            && !deepseek_enabled
            && !cloudflare_enabled
            && !agy_enabled
        {
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
            anyhow::bail!("ENABLE_DEEPSEEK_FALLBACK=true but ENABLE_DEEPSEEK=false");
        }
        if enable_deepseek_fallback && deepseek.api_key.is_empty() {
            anyhow::bail!("ENABLE_DEEPSEEK_FALLBACK=true but DEEPSEEK_API_KEY is not set");
        }

        let any_proxy_enabled = (gemini_enabled && gemini.use_proxy)
            || (openrouter_enabled && openrouter.use_proxy)
            || (deepseek_enabled && deepseek.use_proxy)
            || (cloudflare_enabled && cloudflare.use_proxy);

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
            enable_deepseek_fallback,
            agy,
            gemini_enabled,
            openrouter_enabled,
            deepseek_enabled,
            cloudflare_enabled,
        })
    }
}
