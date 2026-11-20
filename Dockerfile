# syntax=docker/dockerfile:1
# Multi-stage сборка schedule-bot для Railway.
# Этап 1 — сборка release-бинаря, этап 2 — минимальный runtime-образ.

FROM rust:1.100-bookworm AS builder
WORKDIR /app

# Сначала только манифесты — слой с зависимостями кэшируется,
# пока не меняются Cargo.toml / Cargo.lock.
COPY Cargo.toml Cargo.lock ./
# Фиктивный main для предзагрузки зависимостей (код ещё не скопирован).
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked 2>/dev/null || true

COPY src ./src
# Ассеты обязательны на этапе сборки: teachers.rs вшивает их в бинарь
# через include_str!("../assets/teachers.json") и spec_teachers.json.
# Без этой строки сборка на Railway падает с "file not found".
COPY assets ./assets
# Реальная сборка (touch, чтобы пересобрать lib/bin после фиктивных).
RUN touch src/lib.rs src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m app
WORKDIR /app

COPY --from=builder /app/target/release/schedule-bot /app/schedule-bot
COPY webapp /app/webapp
RUN chown -R app:app /app

USER app
# Railway пробрасывает свой PORT; по умолчанию бот слушает 8080.
ENV PORT=8080
EXPOSE 8080
CMD ["./schedule-bot"]
