use std::sync::Arc;

use crate::config::AppConfig;
use crate::providers::ProviderClient;
use crate::rate_limiter::GeminiLimiter;
use crate::thought_signatures::ThoughtSignatures;

pub struct AppState {
    pub config: AppConfig,
    /// `None` when the provider is disabled via its `ENABLE_*` env flag.
    pub gemini: Option<ProviderClient>,
    pub openrouter: Option<ProviderClient>,
    pub deepseek: Option<ProviderClient>,
    pub cloudflare: Option<ProviderClient>,
    pub gemini_limiter: GeminiLimiter,
    /// Gemini thought signatures captured from responses and replayed
    /// into later requests (see `thought_signatures.rs`).
    pub thought_sigs: Arc<ThoughtSignatures>,
}
