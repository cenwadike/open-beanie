# Build with the repository root as the Docker context.
FROM rust:1-slim-trixie AS builder

WORKDIR /app

RUN apt-get update && apt-get upgrade -y && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY beanie_keeper ./beanie_keeper
COPY beanie_api/Cargo.toml beanie_api/Cargo.lock ./beanie_api/

RUN mkdir -p beanie_api/src && printf 'fn main() {}\n' > beanie_api/src/main.rs
WORKDIR /app/beanie_api
RUN cargo build --release --locked
RUN rm -rf src

COPY beanie_api/src ./src
RUN cargo build --release --locked --bin beanie_api

FROM debian:trixie-slim AS runner

WORKDIR /app

RUN apt-get update && apt-get upgrade -y && apt-get install -y \
    ca-certificates \
    libssl3t64 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder --chown=10001:10001 /app/beanie_api/target/release/beanie_api /app/beanie-api
COPY --chown=10001:10001 beanie_api/public /app/public
COPY --chown=10001:10001 beanie_api/stealth_chains.json /app/stealth_chains.json

RUN mkdir -p /app/data && chown -R 10001:10001 /app

EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080 \
    STEALTH_CHAINS_PATH=/app/stealth_chains.json \
    LOG_CACHE_PATH=/app/data/.beanie-chain-log-cache

USER 10001:10001
CMD ["/app/beanie-api"]