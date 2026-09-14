# llm-router (Rust)

Асинхронный переезд Python-прокси на `axum` + `tokio` + `reqwest`,
согласно ТЗ.

## Структура

```
src/
  config.rs        — типизированная загрузка .env (AppConfig, ProviderConfig)
  providers.rs      — изолированные reqwest::Client на провайдера, forward_request, ProviderError
  rate_limiter.rs    — скользящее окно (RPM) + отдельная блокировка на 1 час при quota-429
  state.rs           — AppState, передаётся в хендлеры через Arc<AppState>
  handlers.rs        — маршрутизация: bypass по tools, цепочка Gemini -> OpenRouter -> DeepSeek, SSE-проксирование
  main.rs             — сборка конфига/клиентов, роутер axum, запуск сервера
```

## Соответствие ТЗ

- **Изоляция прокси** (§4): `ProviderClient::build` создаёт отдельный `reqwest::Client`
  на провайдера в момент старта; `use_proxy=true` → `reqwest::Proxy::all(GLOBAL_OUTBOUND_PROXY)`,
  `use_proxy=false` → `.no_proxy()` (гарантированно без утечки прокси из окружения).
- **Bypass по tools** (§5): `providers::needs_tools` проверяет `tools` и `tool_calls`
  в сообщениях — при совпадении Gemini полностью пропускается.
- **Rate limiting** (§5): `GeminiLimiter` — `tokio::sync::Mutex<VecDeque<Instant>>`,
  неблокирующий `try_reserve()`.
- **Обработка 429/quota** (§5): `block_on_429(is_quota)` — при `quota` в теле ответа
  ставит `blocked_until = now + 3600s`; обычный 429 просто заполняет окно текущими
  запросами (провайдер "остывает" до конца минуты), не блокируя основной поток.
- **Chain of Responsibility** (§5): `handlers::fallback_chain` — Gemini → OpenRouter →
  (если `ENABLE_DEEPSEEK_FALLBACK`) DeepSeek → OpenAI-совместимый JSON с 502.
- **Стриминг** (§6): `resp.bytes_stream()` → `axum::body::Body::from_stream()`,
  без побайтового чтения и без риска разрыва многобайтовых UTF-8 последовательностей.

## Запуск

```bash
cp .env.example .env   # заполнить ключи
cargo run --release
```

Эндпоинты:
- `GET /health`
- `POST /v1/chat/completions`

## Docker

```bash
docker build -t llm-router .
docker run --rm -p 8080:8080 --env-file .env llm-router
```

## Не проверено компиляцией

В этом окружении нет доступа к сети/toolchain для `cargo build`, поэтому
код не был скомпилирован здесь. Логика и сигнатуры проверены вручную по
API `axum 0.7` / `reqwest 0.12`, но перед продакшн-использованием стоит
прогнать `cargo check` локально.
# local_chain_models
