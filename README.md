# llm-router (Rust)

Асинхронный переезд Python-прокси на `axum` + `tokio` + `reqwest`,
согласно ТЗ.

## Структура

```
app/
  config.rs        — типизированная загрузка .env (AppConfig, ProviderConfig)
  providers.rs      — изолированные reqwest::Client на провайдера, forward_request, ProviderError
  rate_limiter.rs    — скользящее окно (RPM) + отдельная блокировка на 1 час при quota-429
  thought_signatures.rs — кэш Gemini thought_signature по tool_call_id (реплей в след. запросах)
  state.rs           — AppState, передаётся в хендлеры через Arc<AppState>
  handlers.rs        — маршрутизация: Gemini -> OpenRouter -> DeepSeek -> Cloudflare, SSE-проксирование
  main.rs             — сборка конфига/клиентов, роутер axum, запуск сервера
```

## Соответствие ТЗ

- **Изоляция прокси** (§4): `ProviderClient::build` создаёт отдельный `reqwest::Client`
  на провайдера в момент старта; `use_proxy=true` → `reqwest::Proxy::all(GLOBAL_OUTBOUND_PROXY)`,
  `use_proxy=false` → `.no_proxy()` (гарантированно без утечки прокси из окружения).
- **Bypass по tools** (§5): `providers::needs_tools` проверяет `tools` и `tool_calls`
  в сообщениях. По умолчанию Gemini **сам обрабатывает** такие запросы (с rate-limit);
  старый bypass включается явно через `GEMINI_TOOLS_BYPASS=true` — тогда запросы с
  tools пропускают Gemini и уходят сразу в fallback-цепочку.
- **Rate limiting** (§5): `GeminiLimiter` — `tokio::sync::Mutex<VecDeque<Instant>>`,
  неблокирующий `try_reserve()`.
- **Обработка 429/quota** (§5): `block_on_429(is_quota)` — при `quota` в теле ответа
  ставит `blocked_until = now + 3600s`; обычный 429 просто заполняет окно текущими
  запросами (провайдер "остывает" до конца минуты), не блокируя основной поток.
- **Chain of Responsibility** (§5): `handlers::fallback_chain` — Gemini → OpenRouter →
  (если `ENABLE_DEEPSEEK_FALLBACK`) DeepSeek → Cloudflare → OpenAI-совместимый JSON с 502.
- **Стриминг** (§6): `resp.bytes_stream()` → `axum::body::Body::from_stream()`,
  без побайтового чтения и без риска разрыва многобайтовых UTF-8 последовательностей.

## Включение/отключение провайдеров

Каждый провайдер можно выключить через env-флаг. По умолчанию все включены.

| Переменная            | По умолчанию | Описание                                  |
|-----------------------|--------------|-------------------------------------------|
| `ENABLE_GEMINI`       | `true`       | Основной провайдер (с rate-limit логикой) |
| `GEMINI_TOOLS_BYPASS` | `false`      | `true` — запросы с tools/tool_calls минуют Gemini и идут в fallback |
| `ENABLE_OPENROUTER`   | `true`       | Первый резерв                             |
| `ENABLE_DEEPSEEK`     | `true`       | Второй резерв                             |
| `ENABLE_CLOUDFLARE`  | `true`       | Финальный резерв                          |

Поведение:

- Отключённый провайдер **не создаётся** на старте и **полностью пропускается**
  в цепочке fallback (никаких запросов к нему).
- API-ключ отключённого провайдера **не обязателен**.
- Если выключить все четыре — приложение упадёт на старте с понятной ошибкой.
- `ENABLE_DEEPSEEK_FALLBACK=true` требует `ENABLE_DEEPSEEK=true` и наличия
  `DEEPSEEK_API_KEY`: DeepSeek участвует в цепочке только когда оба флага включены.

Пример: оставить только Cloudflare —

```env
ENABLE_GEMINI=false
ENABLE_OPENROUTER=false
ENABLE_DEEPSEEK=false
ENABLE_DEEPSEEK_FALLBACK=false
ENABLE_CLOUDFLARE=true
```

## Запуск

```bash
make release   # = env + kill + cargo run --release
# или вручную:
# cp .env.example .env   # заполнить ключи
# cargo run --release
```

`make release` и `make run` перед стартом выполняют target `kill` —
убивают старый инстанс на `:8981` (через `ss`/`lsof`), поэтому перезапуск
не падает с `Address already in use`. Только сборка — `make build`.

По умолчанию сервер слушает `0.0.0.0:8981` (меняется через `SERVER_PORT`).

Эндпоинты:
- `GET /health`
- `POST /v1/chat/completions`

## Thought signatures (Gemini)

Gemini-3 (и thinking-модели 2.5) требуют, чтобы каждый `functionCall` в
**входящей** истории нёс подпись `thought_signature`, которую модель выдала
в ответе (`tool_calls[].extra_content.google.thought_signature`). Иначе
Gemini отвечает `400 INVALID_ARGUMENT: Function call is missing a
thought_signature`.

Клиенты (pi/Qwen Code) эту подпись не сохраняют, поэтому прокси:

- вылавливает подписи из ответов Gemini (и из JSON, и из SSE-фреймов)
  и кладёт их в кэш `thought_signatures.rs` по `tool_call_id` (TTL 6 ч);
- перед запросом к Gemini возвращает подписи обратно в
  `messages[].tool_calls[].extra_content` (в том числе после
  перезапуска процесса — см. ниже).

Если подпись не найдена (например, tool call сделан другим провайдером или
кэш ещё пуст) — запрос штатно уходит в reserve-цепочку. Поведение видно в логе:
`replayed N cached Gemini thought signature(s)` / `N tool call(s) have no
cached thought signature`.

### Персистентность кэша

Кэш append-only пишется на диск и перечитывается при старте, поэтому
перезапуск прокси больше не ломает текущую сессию:

- путь: `THOUGHT_SIG_CACHE_PATH` (по умолчанию
  `~/.cache/llm-router/thought_signatures.jsonl`, `~/` раскрывается);
- `THOUGHT_SIG_CACHE_PATH=off` (или пусто) — только память;
- формат — по одной записи JSONL `{"id","sig","ts"}` на строку;
  при загрузке просроченные (TTL 6 ч) и битые строки пропускаются,
  для одного `id` побеждает последняя запись;
- раз в `COMPACT_AFTER_APPENDS` (50 000) дописываний лог атомарно
  перезаписывается только живыми записями, так что файл не растёт вечно;
- каталог создаётся автоматически; при любой ошибке I/O кэш деградирует
  до memory-only и пишет `warn`, а не роняет прокси.

Важно: подпись нельзя «сочинить». Прокси умеет вернуть только ту подпись,
которую он ранее выловил из ответа Gemini. Поэтому сессию, в которой
неподписанные tool calls появились **до** включения этого кэша, он не
спасёт — такую сессию нужно начать заново. Дальше подписи сохраняются.

## Docker

```bash
docker build -t llm-router .
docker run --rm -p 8981:8981 --env-file .env llm-router
```
