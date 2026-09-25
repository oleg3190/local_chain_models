use std::sync::Arc;

use crate::agy::AgyProvider;
use crate::config::AppConfig;
use crate::providers::ProviderClient;
use crate::rate_limiter::GeminiLimiter;
use crate::thought_signatures::ThoughtSignatures;

pub struct AppState {
    pub config: AppConfig,
    pub gemini: Option<ProviderClient>,
    pub openrouter: Option<ProviderClient>,
    pub deepseek: Option<ProviderClient>,
    pub cloudflare: Option<ProviderClient>,
    pub agy: Option<Arc<AgyProvider>>,
    pub gemini_limiter: GeminiLimiter,
    pub thought_sigs: Arc<ThoughtSignatures>,
}
