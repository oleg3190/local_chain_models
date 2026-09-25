use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::Stream;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::agy::{AgyCompletion, AgyToolCall};
use crate::providers::{forward_request, needs_tools};
use crate::state::AppState;
use crate::thought_signatures::ThoughtSignatures;

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Headers that must never be copied straight through from the
/// upstream response — they describe the upstream's own transport
/// framing, not ours (axum recomputes these for the outgoing body).
const HOP_BY_HOP: [&str; 4] = [
    "content-length",
    "transfer-encoding",
    "content-encoding",
    "connection",
];

/// Human-readable provider name for log lines.
fn display_name(provider: &'static str) -> &'static str {
    match provider {
        "gemini" => "Gemini",
        "openrouter" => "OpenRouter",
        "deepseek" => "DeepSeek",
        "cloudflare" => "Cloudflare",
        other => other,
    }
}

/// Extracts the model that actually served the request from a
/// (possibly partial) OpenAI-compatible response body. Handles both a
/// plain JSON body and SSE frames (`data: {...}`), so it works for
/// streaming and non-streaming responses alike.
fn extract_model(buf: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(buf).ok()?;

    // Non-streaming: the whole body is a single JSON object.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            return Some(m.to_string());
        }
    }

    // Streaming: scan SSE `data:` frames for the first one carrying a model.
    for line in text.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                return Some(m.to_string());
            }
        }
    }
    None
}

/// Upper bound on how much of a response we buffer while looking for
/// the model name — never hold a whole stream just to log it.
const MODEL_PROBE_LIMIT: usize = 16 * 1024;

/// Upper bound on how much of a response we retain for thought-signature
/// extraction. Function-call payloads are small; this only stops a
/// pathological stream from pinning memory.
const SIGNATURE_CAPTURE_LIMIT: usize = 256 * 1024;

/// Pulls `(tool_call_id, thought_signature)` pairs out of a complete
/// (possibly SSE-framed) response body.
fn extract_thought_signatures(buf: &[u8]) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let Ok(text) = std::str::from_utf8(buf) else {
        return found;
    };

    // Non-streaming: the whole body is one JSON object.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(message) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
        {
            collect_message_signatures(message, &mut found);
        }
        if !found.is_empty() {
            return found;
        }
    }

    // Streaming: tool-call deltas are keyed by `index`, and the id and
    // the signature may arrive in different frames.
    let mut per_index: std::collections::BTreeMap<i64, (Option<String>, Option<String>)> =
        std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some(data) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            continue;
        };

        if let Some(message) = choice.get("message") {
            collect_message_signatures(message, &mut found);
        }

        let Some(tool_calls) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for call in tool_calls {
            let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
            let slot = per_index.entry(index).or_default();
            if slot.0.is_none() {
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.0 = Some(id.to_string());
                }
            }
            if slot.1.is_none() {
                if let Some(signature) = call
                    .pointer("/extra_content/google/thought_signature")
                    .and_then(Value::as_str)
                {
                    slot.1 = Some(signature.to_string());
                }
            }
        }
    }

    for (_, (id, signature)) in per_index {
        if let (Some(id), Some(signature)) = (id, signature) {
            found.push((id, signature));
        }
    }
    found
}

/// Collects signatures from a non-streaming `message.tool_calls[]` array.
fn collect_message_signatures(message: &Value, out: &mut Vec<(String, String)>) {
    let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return;
    };
    for call in tool_calls {
        let id = call.get("id").and_then(Value::as_str);
        let signature = call
            .pointer("/extra_content/google/thought_signature")
            .and_then(Value::as_str);
        if let (Some(id), Some(signature)) = (id, signature) {
            out.push((id.to_string(), signature.to_string()));
        }
    }
}

/// Passes bytes through untouched while sniffing the first frames for
/// the model that actually handled the request, logging it once found
/// (falling back to the configured model name if it never shows up).
struct ModelLoggingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    provider: &'static str,
    configured_model: String,
    probe: Vec<u8>,
    logged: bool,
    signatures: Arc<ThoughtSignatures>,
    sig_buf: Vec<u8>,
}

impl ModelLoggingStream {
    fn new(
        inner: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
        provider: &'static str,
        configured_model: String,
        signatures: Arc<ThoughtSignatures>,
    ) -> Self {
        Self {
            inner: Box::pin(inner),
            provider,
            configured_model,
            probe: Vec::new(),
            logged: false,
            signatures,
            sig_buf: Vec::new(),
        }
    }

    fn log(&mut self, model: Option<String>) {
        if self.logged {
            return;
        }
        self.logged = true;
        let model = model.unwrap_or_else(|| self.configured_model.clone());
        info!("-> {} OK (модель: {})", display_name(self.provider), model);
    }

    /// Persists any thought signatures found in the response so they can
    /// be replayed on the next turn (see `thought_signatures.rs`).
    fn store_signatures(&self) {
        if self.sig_buf.is_empty() {
            return;
        }
        for (id, signature) in extract_thought_signatures(&self.sig_buf) {
            self.signatures.put(&id, &signature);
        }
    }
}

impl Stream for ModelLoggingStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if !this.logged {
                    this.probe.extend_from_slice(&chunk);
                    match extract_model(&this.probe) {
                        Some(model) => this.log(Some(model)),
                        None if this.probe.len() >= MODEL_PROBE_LIMIT => this.log(None),
                        None => {}
                    }
                }
                if this.sig_buf.len() < SIGNATURE_CAPTURE_LIMIT {
                    this.sig_buf.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.log(None);
                this.store_signatures();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.log(None);
                this.store_signatures();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Converts a successful upstream `reqwest::Response` into an axum
/// `Response`, streaming the body through with zero-copy semantics
/// via `bytes_stream()` — this is what keeps SSE chunks intact and
/// avoids splitting multi-byte UTF-8 sequences. The stream also logs
/// the model that actually served the request and captures any Gemini
/// thought signatures for replay.
fn stream_upstream(
    resp: reqwest::Response,
    provider: &'static str,
    configured_model: &str,
    signatures: Arc<ThoughtSignatures>,
) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    let mut headers = HeaderMap::new();
    for (name, value) in resp.headers().iter() {
        if HOP_BY_HOP.contains(&name.as_str()) {
            continue;
        }
        if let Ok(header_name) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            headers.insert(header_name, value.clone());
        }
    }

    let stream = ModelLoggingStream::new(
        resp.bytes_stream(),
        provider,
        configured_model.to_string(),
        signatures,
    );
    let body = Body::from_stream(stream);

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Renders a terminal failure (nothing left to fall back to) as an
/// OpenAI-compatible error envelope.
fn openai_error(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({
        "error": {
            "message": message.into(),
            "type": "proxy_error",
            "code": status.as_u16(),
        }
    });
    (status, Json(body)).into_response()
}

fn to_status(status: reqwest::StatusCode) -> StatusCode {
    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY)
}


fn requested_model(payload: &Value) -> Option<&str> {
    payload.get("model").and_then(Value::as_str)
}

fn agy_tool_calls_json(tool_calls: &[AgyToolCall]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": serde_json::to_string(&call.arguments)
                        .unwrap_or_else(|_| "{}".to_string())
                }
            })
        })
        .collect()
}

fn agy_openai_response(completion: AgyCompletion, model: &str) -> Response {
    let (id, created) = agy_request_id();
    let finish_reason = if completion.tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let tool_calls = agy_tool_calls_json(&completion.tool_calls);

    let mut message = json!({
        "role": "assistant",
        "content": if completion.content.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(completion.content)
        }
    });

    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    let mut response = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }]
    });

    if let Some(usage) = completion.usage {
        response["usage"] = usage;
    }

    Json(response).into_response()
}

fn agy_request_id() -> (String, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let created = now.as_secs();
    let id = format!("chatcmpl-agy-{}-{}", created, now.subsec_nanos());
    (id, created)
}

fn agy_sse(value: Value) -> Bytes {
    Bytes::from(format!(
        "data: {}\n\n",
        serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
    ))
}

fn agy_streaming_response(
    model: String,
    id: String,
    created: u64,
    usage_requested: bool,
    rx: tokio::sync::mpsc::Receiver<crate::agy::AgyStreamEvent>,
) -> Response {
    let first = agy_sse(json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant" },
            "finish_reason": Value::Null
        }]
    }));

    struct AgySseState {
        rx: tokio::sync::mpsc::Receiver<crate::agy::AgyStreamEvent>,
        model: String,
        id: String,
        created: u64,
        pending: Vec<Bytes>,
        usage_requested: bool,
    }

    let state = AgySseState {
        rx,
        model,
        id,
        created,
        pending: vec![first],
        usage_requested,
    };

    let stream = futures_util::stream::unfold(state, |mut state| async move {
        if let Some(chunk) = state.pending.pop() {
            return Some((Ok::<Bytes, std::convert::Infallible>(chunk), state));
        }

        match state.rx.recv().await {
            Some(crate::agy::AgyStreamEvent::TextDelta(text)) => {
                let chunk = agy_sse(json!({
                    "id": state.id,
                    "object": "chat.completion.chunk",
                    "created": state.created,
                    "model": state.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "content": text },
                        "finish_reason": Value::Null
                    }]
                }));
                Some((Ok(chunk), state))
            }
            Some(crate::agy::AgyStreamEvent::Heartbeat) => {
                Some((Ok(Bytes::from_static(b": ping\n\n")), state))
            }
            Some(crate::agy::AgyStreamEvent::Completed(completion)) => {
                let finish_reason = if completion.tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };

                if state.usage_requested {
                    if let Some(usage) = completion.usage.clone() {
                        state.pending.push(agy_sse(json!({
                            "id": state.id,
                            "object": "chat.completion.chunk",
                            "created": state.created,
                            "model": state.model,
                            "choices": [],
                            "usage": usage
                        })));
                    }
                }

                state.pending.push(Bytes::from_static(b"data: [DONE]\n\n"));
                state.pending.push(agy_sse(json!({
                    "id": state.id,
                    "object": "chat.completion.chunk",
                    "created": state.created,
                    "model": state.model,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish_reason
                    }]
                })));

                for (index, call) in completion.tool_calls.iter().enumerate().rev() {
                    state.pending.push(agy_sse(json!({
                        "id": state.id,
                        "object": "chat.completion.chunk",
                        "created": state.created,
                        "model": state.model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [{
                                    "index": index,
                                    "id": call.id,
                                    "type": "function",
                                    "function": {
                                        "name": call.name,
                                        "arguments": serde_json::to_string(&call.arguments)
                                            .unwrap_or_else(|_| "{}".to_string())
                                    }
                                }]
                            },
                            "finish_reason": Value::Null
                        }]
                    })));
                }

                let next = state.pending.pop().expect("AGY completion must have a pending frame");
                Some((Ok(next), state))
            }
            Some(crate::agy::AgyStreamEvent::Error(message)) => {
                state.pending.push(Bytes::from_static(b"data: [DONE]\n\n"));
                state.pending.push(agy_sse(json!({
                    "error": {
                        "message": message,
                        "type": "proxy_error",
                        "code": StatusCode::BAD_GATEWAY.as_u16()
                    }
                })));
                let next = state.pending.pop().expect("AGY error must have a pending frame");
                Some((Ok(next), state))
            }
            None => None,
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache, no-transform")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| openai_error(StatusCode::BAD_GATEWAY, "failed to build AGY stream"))
}

async fn call_agy(state: &Arc<AppState>, payload: &Value) -> Response {
    let Some(provider) = &state.agy else {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "AGY backend is disabled or not configured",
        );
    };

    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if stream {
        let (id, created) = agy_request_id();
        let usage_requested = payload
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return agy_streaming_response(
            state.config.agy.model_id.clone(),
            id,
            created,
            usage_requested,
            provider.stream(payload),
        );
    }

    match provider.complete(payload).await {
        Ok(completion) => {
            info!("-> AGY OK (model: {})", state.config.agy.model_id);
            agy_openai_response(completion, &state.config.agy.model_id)
        }
        Err(error) => {
            warn!("AGY failed: {error:#}");
            openai_error(StatusCode::BAD_GATEWAY, format!("AGY backend failed: {error}"))
        }
    }
}


/// Chain of responsibility over the *enabled* providers only:
/// OpenRouter, then DeepSeek (when `ENABLE_DEEPSEEK_FALLBACK` is on),
/// then Cloudflare. Providers disabled via `ENABLE_*` are skipped.
async fn fallback_chain(state: &Arc<AppState>, payload: &Value) -> Response {
    let mut last_error: Option<(StatusCode, Vec<u8>)> = None;

    if let Some(provider) = &state.openrouter {
        match forward_request(provider, payload, &state.thought_sigs).await {
            Ok(resp) => {
                return stream_upstream(
                    resp,
                    provider.name,
                    &provider.model,
                    state.thought_sigs.clone(),
                );
            }
            Err(e) => {
                warn!("OpenRouter failed: {e}");
                let (status, body) = e.as_status_and_body();
                last_error = Some((to_status(status), body));
            }
        }
    } else {
        info!("OpenRouter disabled -> skipping");
    }

    if state.config.agy.as_fallback {
        if let Some(agy) = &state.agy {
            info!("trying AGY...");
            let stream = payload
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if stream {
                let (id, created) = agy_request_id();
                let usage_requested = payload
                    .pointer("/stream_options/include_usage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                return agy_streaming_response(
                    state.config.agy.model_id.clone(),
                    id,
                    created,
                    usage_requested,
                    agy.stream(payload),
                );
            }

            match agy.complete(payload).await {
                Ok(completion) => {
                    info!("-> AGY OK (model: {})", state.config.agy.model_id);
                    return agy_openai_response(completion, &state.config.agy.model_id, false);
                }
                Err(error) => {
                    warn!("AGY failed: {error:#}");
                }
            }
        } else {
            info!("AGY fallback enabled but provider is not configured -> skipping");
        }
    }

    if state.config.enable_deepseek_fallback {
        if let Some(provider) = &state.deepseek {
            info!("trying DeepSeek...");
            match forward_request(provider, payload, &state.thought_sigs).await {
                Ok(resp) => {
                    return stream_upstream(
                        resp,
                        provider.name,
                        &provider.model,
                        state.thought_sigs.clone(),
                    );
                }
                Err(e) => {
                    warn!("DeepSeek failed: {e}");
                    let (status, body) = e.as_status_and_body();
                    last_error = Some((to_status(status), body));
                }
            }
        } else {
            info!("DeepSeek disabled -> skipping");
        }
    } else {
        info!("ENABLE_DEEPSEEK_FALLBACK is off -> skipping DeepSeek");
    }

    if let Some(provider) = &state.cloudflare {
        info!("trying Cloudflare Workers AI...");
        match forward_request(provider, payload, &state.thought_sigs).await {
            Ok(resp) => {
                return stream_upstream(
                    resp,
                    provider.name,
                    &provider.model,
                    state.thought_sigs.clone(),
                );
            }
            Err(e) => {
                warn!("Cloudflare failed: {e}");
                let (status, body) = e.as_status_and_body();
                last_error = Some((to_status(status), body));
            }
        }
    } else {
        info!("Cloudflare disabled -> skipping");
    }

    match last_error {
        Some((status, body)) => Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap_or_else(|_| openai_error(StatusCode::BAD_GATEWAY, "all enabled providers failed")),
        None => openai_error(
            StatusCode::BAD_GATEWAY,
            "no enabled fallback providers available",
        ),
    }
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<Value>,
) -> Response {
    if let Some(model) = requested_model(&payload) {
        if model == "agy" || (state.config.agy.enabled && model == state.config.agy.model_id) {
            return call_agy(&state, &payload).await;
        }
    }

    let has_tools = needs_tools(&payload);
    if has_tools {
        if state.config.gemini_tools_bypass {
            info!(
                "payload contains tools/tool_calls and GEMINI_TOOLS_BYPASS=true -> bypassing Gemini, routing to fallback chain"
            );
            return fallback_chain(&state, &payload).await;
        }
        info!("payload contains tools/tool_calls -> routing to Gemini");
    }

    let gemini = match &state.gemini {
        Some(provider) => provider,
        None => {
            info!("Gemini disabled -> routing to fallback chain");
            return fallback_chain(&state, &payload).await;
        }
    };

    if let Some(remaining) = state.gemini_limiter.blocked_for().await {
        info!("Gemini quota-blocked for {remaining}s more -> straight to fallback");
        return fallback_chain(&state, &payload).await;
    }

    if !state.gemini_limiter.try_reserve().await {
        info!("Gemini RPM limit reached -> straight to fallback");
        return fallback_chain(&state, &payload).await;
    }

    match forward_request(gemini, &payload, &state.thought_sigs).await {
        Ok(resp) => stream_upstream(
            resp,
            gemini.name,
            &gemini.model,
            state.thought_sigs.clone(),
        ),
        Err(e) => {
            if e.is_429() {
                let is_quota = e.is_quota_429();
                if is_quota {
                    warn!("Gemini quota exhausted -> blocking provider for 1 minute");
                } else {
                    warn!("Gemini rate limit hit -> waiting out the window");
                }
                state.gemini_limiter.block_on_429(is_quota).await;
            } else {
                warn!("Gemini call failed: {e}");
            }
            fallback_chain(&state, &payload).await
        }
    }
}
