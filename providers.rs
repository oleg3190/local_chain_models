use crate::config::{ApiKeyPoolConfig, AppConfig, ProviderConfig};
use crate::key_pool::ApiKeyPool;
use std::sync::Arc;
use crate::thought_signatures::ThoughtSignatures;
use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use reqwest::{Client, Response};
use serde_json::Value;
use thiserror::Error;

/// One upstream provider: its own isolated `reqwest::Client` (so
/// proxy routing never leaks between providers), plus the static
/// bits needed to build a request.
#[derive(Clone)]
pub struct ProviderClient {
    pub name: &'static str,
    pub client: Client,
    pub url: String,
    pub api_key: String,
    pub model: String,
}

impl ProviderClient {
    pub fn build(
        name: &'static str,
        url: &str,
        cfg: &ProviderConfig,
        global_proxy: Option<&str>,
    ) -> Result<Self> {
        let mut builder = Client::builder();

        if cfg.use_proxy {
            let proxy_url = global_proxy
                .with_context(|| format!("{name} requires a proxy but none is configured"))?;
            builder = builder.proxy(
                reqwest::Proxy::all(proxy_url)
                    .with_context(|| format!("invalid proxy URL for {name}: {proxy_url}"))?,
            );
        } else {
            // Explicitly disable any proxy env vars for this client so
            // "no proxy" really means direct traffic, matching the
            // per-provider isolation the spec calls for.
            builder = builder.no_proxy();
        }

        let client = builder
            .build()
            .with_context(|| format!("failed to build HTTP client for {name}"))?;

        Ok(Self {
            name,
            client,
            url: url.to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        })
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("{provider} returned HTTP {status}: {body}")]
    Upstream {
        provider: &'static str,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("network error calling {provider}: {source}")]
    Network {
        provider: &'static str,
        #[source]
        source: reqwest::Error,
    },
}

impl ProviderError {
    pub fn is_quota_429(&self) -> bool {
        matches!(self,
            ProviderError::Upstream { status, body, .. }
                if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    && body.to_lowercase().contains("quota")
        )
    }

    pub fn is_429(&self) -> bool {
        matches!(self,
            ProviderError::Upstream { status, .. }
                if *status == reqwest::StatusCode::TOO_MANY_REQUESTS
        )
    }

    /// Best-effort (status, body) pair for relaying the final error
    /// upstream in OpenAI-compatible shape.
    pub fn as_status_and_body(&self) -> (reqwest::StatusCode, Vec<u8>) {
        match self {
            ProviderError::Upstream { status, body, .. } => (*status, body.clone().into_bytes()),
            ProviderError::Network { source, .. } => (
                reqwest::StatusCode::BAD_GATEWAY,
                serde_json::json!({
                    "error": { "message": source.to_string(), "type": "network_error" }
                })
                .to_string()
                .into_bytes(),
            ),
        }
    }
}

/// Re-attaches cached Gemini `thought_signature`s to every assistant
/// `tool_calls[]` entry in the request history that is missing one.
/// Gemini's OpenAI-compatible endpoint rejects unsigned function calls
/// with HTTP 400, so this is what makes multi-turn tool use work when
/// the client (pi/Qwen Code) dropped the `extra_content` it received.
///
/// Returns `(injected, missing)`: how many signatures were attached and
/// how many function calls remain unsigned (those make Gemini answer
/// 400, so the caller will end up falling back to the reserve chain).
fn inject_thought_signatures(body: &mut Value, signatures: &ThoughtSignatures) -> (usize, usize) {
    /// How many unsigned tool-call ids to name in the debug log.
    const MISSING_ID_SAMPLE: usize = 8;

    let mut injected = 0usize;
    let mut missing = 0usize;
    let mut missing_sample: Vec<String> = Vec::new();

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return (injected, missing);
    };

    for message in messages {
        let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };

        for call in tool_calls {
            let already_has = call
                .pointer("/extra_content/google/thought_signature")
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if already_has {
                continue;
            }

            let Some(id) = call.get("id").and_then(Value::as_str).map(str::to_string) else {
                missing += 1;
                continue;
            };

            match signatures.get(&id) {
                Some(signature) => {
                    if let Some(obj) = call.as_object_mut() {
                        obj.insert(
                            "extra_content".to_string(),
                            serde_json::json!({ "google": { "thought_signature": signature } }),
                        );
                        injected += 1;
                    } else {
                        missing += 1;
                        record_missing(&mut missing_sample, &id, MISSING_ID_SAMPLE);
                    }
                }
                None => {
                    missing += 1;
                    record_missing(&mut missing_sample, &id, MISSING_ID_SAMPLE);
                }
            }
        }
    }

    if !missing_sample.is_empty() {
        debug!(
            ids = ?missing_sample,
            "unsigned Gemini tool calls (first few of {missing})"
        );
    }

    (injected, missing)
}

/// Keeps up to `cap` ids so the debug log stays readable.
fn record_missing(sample: &mut Vec<String>, id: &str, cap: usize) {
    if sample.len() < cap {
        sample.push(id.to_string());
    }
}

/// Sends `payload` to a provider, overriding the model field with the
/// provider's configured model. Returns the raw upstream `Response`
/// on success (2xx) so the caller can stream it straight through.
pub async fn forward_request(
    provider: &ProviderClient,
    payload: &Value,
    signatures: &ThoughtSignatures,
    opencode_session: Option<&str>,
) -> Result<Response, ProviderError> {
    let mut body = payload.clone();

    // Cloudflare Workers AI doesn't support stream_options (e.g., include_usage)
    if provider.name == "cloudflare" {
        if let Value::Object(ref mut map) = body {
            map.remove("stream_options");
        }
    }

    if provider.name == "gemini" {
        // Gemini OpenAI-compat endpoint rejects OpenAI-only fields like `store`
        if let Value::Object(ref mut map) = body {
            map.remove("store");
        }
        // ...and requires the thought signature of every function call in history.
        let (injected, missing) = inject_thought_signatures(&mut body, signatures);
        if injected > 0 {
            info!("replayed {injected} cached Gemini thought signature(s)");
        }
        if missing > 0 {
            warn!(
                "{missing} tool call(s) have no cached thought signature; Gemini will reject them and the request will fall back"
            );
        }    }

    if let Value::Object(ref mut map) = body {
        map.insert("model".to_string(), Value::String(provider.model.clone()));
    }

    let mut req = provider
        .client
        .post(&provider.url)
        .bearer_auth(&provider.api_key)
        .json(&body);

    // Provider-specific headers, mirroring the Python implementation.
    match provider.name {
        "gemini" => {
            req = req.header("x-goog-api-key", &provider.api_key);
        }
        "openrouter" => {
            req = req
                .header("HTTP-Referer", "http://localhost:8080")
                .header("X-Title", "Local Qwen Agent");
        }
        "opencode-go" => {
            req = req.header("User-Agent", "local-chain-models/0.1");
            if let Some(session) = opencode_session.filter(|value| !value.is_empty()) {
                req = req.header("x-opencode-session", session);
            }
        }
        _ => {}
    }

    let resp = req.send().await.map_err(|e| ProviderError::Network {
        provider: provider.name,
        source: e,
    })?;

    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read upstream error body>".to_string());
        Err(ProviderError::Upstream {
            provider: provider.name,
            status,
            body: body_text,
        })
    }
}

/// The set of provider clients that are actually enabled by config.
/// A `None` entry means the provider was disabled via its `ENABLE_*`
/// env flag and must be skipped both at startup and in the fallback chain.
pub struct ProviderClients {
    pub gemini: Option<ProviderClient>,
    pub openrouter: Option<ProviderClient>,
    pub deepseek: Option<ProviderClient>,
    pub cloudflare: Option<ProviderClient>,
}

/// Builds an isolated client for every enabled provider. Disabled
/// providers are returned as `None` and never contacted.
/// Builds the OpenCode Go key pool. Each key gets its own isolated HTTP client,
/// while all keys point at the same OpenCode Go endpoint/model.
pub fn build_opencode_go_pool(cfg: &ApiKeyPoolConfig, global_proxy: Option<&str>) -> Result<Option<Arc<ApiKeyPool>>> {
    if cfg.api_keys.is_empty() {
        return Ok(None);
    }

    let clients = cfg
        .api_keys
        .iter()
        .map(|key| {
            let provider_cfg = ProviderConfig {
                api_key: key.clone(),
                model: cfg.model.clone(),
                use_proxy: cfg.use_proxy,
            };
            ProviderClient::build(
                "opencode-go",
                crate::config::OPENCODE_GO_URL,
                &provider_cfg,
                global_proxy,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(ApiKeyPool::new(
        clients,
        std::time::Duration::from_secs(cfg.cooldown_secs),
    )))
}

pub fn build_clients(cfg: &AppConfig) -> Result<ProviderClients> {
    let proxy = cfg.global_outbound_proxy.as_deref();

    let gemini = if cfg.gemini_enabled {
        Some(ProviderClient::build(
            "gemini",
            crate::config::GEMINI_URL,
            &cfg.gemini,
            proxy,
        )?)
    } else {
        None
    };

    let openrouter = if cfg.openrouter_enabled {
        Some(ProviderClient::build(
            "openrouter",
            crate::config::OPENROUTER_URL,
            &cfg.openrouter,
            proxy,
        )?)
    } else {
        None
    };

    let deepseek = if cfg.deepseek_enabled {
        Some(ProviderClient::build(
            "deepseek",
            crate::config::DEEPSEEK_URL,
            &cfg.deepseek,
            proxy,
        )?)
    } else {
        None
    };

    let cloudflare = if cfg.cloudflare_enabled {
        Some(ProviderClient::build(
            "cloudflare",
            crate::config::CLOUDFLARE_URL,
            &cfg.cloudflare,
            proxy,
        )?)
    } else {
        None
    };

    Ok(ProviderClients {
        gemini,
        openrouter,
        deepseek,
        cloudflare,
    })
}

/// Detects whether a chat-completions payload requires tool support,
/// in which case Gemini is bypassed entirely (matches the Python
/// `"tools" in payload or any("tool_calls" in m for m in messages)`).
pub fn needs_tools(payload: &Value) -> bool {
    if payload.get("tools").is_some() {
        return true;
    }
    payload
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| messages.iter().any(|m| m.get("tool_calls").is_some()))
        .unwrap_or(false)
}

