# syntax=docker/dockerfile:1

# ---- build -------------------------------------------------------------
FROM rust:1-slim-bookworm AS build
WORKDIR /app

# Build dependencies first so they cache independently of the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY ui ./ui
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime -----------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /app genparse

WORKDIR /app
COPY --from=build /app/target/release/genparse /usr/local/bin/genparse
COPY config.toml ./config.toml
RUN mkdir -p /app/.genparse-cache && chown -R genparse:genparse /app
USER genparse

# Provider keys come from the environment: ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY.
ENV RUST_LOG=genparse=info,tower_http=info
EXPOSE 8080
VOLUME ["/app/.genparse-cache"]
HEALTHCHECK --interval=30s --timeout=3s CMD ["genparse", "--version"]

ENTRYPOINT ["genparse"]
CMD ["serve", "--ui"]
