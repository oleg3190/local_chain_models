mod config;
mod handlers;
mod key_pool;
mod providers;
mod rate_limiter;
mod state;
mod thought_signatures;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use tracing_subscriber::EnvFilter;

use config::AppConfig;
use providers::{build_clients, build_opencode_go_pool};
use rate_limiter::GeminiLimiter;
use state::AppState;
use thought_signatures::ThoughtSignatures;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = AppConfig::from_env()?;
    let clients = build_clients(&config)?;
    let opencode_go = build_opencode_go_pool(&config.opencode_go, config.global_outbound_proxy.as_deref())?;

    tracing::info!(
        proxy = ?config.global_outbound_proxy,
        deepseek_fallback = config.enable_deepseek_fallback,
        gemini = config.gemini_enabled,
        gemini_tools_bypass = config.gemini_tools_bypass,
        openrouter = config.openrouter_enabled,
        deepseek = config.deepseek_enabled,
        cloudflare = config.cloudflare_enabled,
        opencode_go_keys = config.opencode_go.api_keys.len(),
        opencode_go_model = %config.opencode_go.model,
        "starting llm-router"
    );

    let gemini_limiter = GeminiLimiter::new(config.gemini_rpm_limit);
    let addr = format!("{}:{}", config.host, config.port);

    // Replay the persisted signature cache so a restart does not force
    // every in-flight conversation onto the fallback chain.
    let thought_sigs = Arc::new(ThoughtSignatures::from_setting(
        config.thought_sig_cache_path.as_deref(),
    ));
    tracing::info!(
        cached = thought_sigs.len(),
        persist = config.thought_sig_cache_path.is_some(),
        "Gemini thought-signature cache ready"
    );

    let state = Arc::new(AppState {
        config,
        gemini: clients.gemini,
        openrouter: clients.openrouter,
        deepseek: clients.deepseek,
        cloudflare: clients.cloudflare,
        opencode_go,
        gemini_limiter,
        thought_sigs,
    });

    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/v1/chat/completions", post(handlers::chat_completions))
        .route("/v1/:provider/chat/completions", post(handlers::provider_chat_completions))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;

    Ok(())
}
