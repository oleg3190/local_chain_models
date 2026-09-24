FROM rust:1.80-slim AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml ./
COPY main.rs ./
COPY config.rs ./
COPY providers.rs ./
COPY rate_limiter.rs ./
COPY state.rs ./
COPY handlers.rs ./
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/llm-router /app/llm-router
EXPOSE 8080
ENTRYPOINT ["/app/llm-router"]