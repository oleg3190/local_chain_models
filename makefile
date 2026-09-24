.PHONY: run build release check fmt lint clean docker-build docker-run env kill

# Останавливает старый инстанс llm-router на :8981, чтобы
# `make release` / `make run` не падали с "Address already in use".
kill:
	@echo "killing old llm-router on :8981 (if any)..."
	@ss -lptn 'sport = :8981' 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2 | xargs -r kill -9 2>/dev/null || true
	@lsof -ti :8981 2>/dev/null | xargs -r kill -9 2>/dev/null || true

# Копирует .env.example -> .env, если .env ещё нет
env:
	@test -f .env || cp .env.example .env
	@echo "-> .env готов (заполните ключи, если ещё не сделали)"

run: env kill
	cargo run

release: env kill
	cargo run --release

build:
	cargo build

check:
	cargo check

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets --all-features -- -D warnings

clean:
	cargo clean

docker-build:
	docker build -t llm-router .

docker-run: env
	docker run --rm -p 8080:8080 --env-file .env llm-router
