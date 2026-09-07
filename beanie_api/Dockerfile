# --- Stage 1: Builder Stage ---
FROM rust:latest AS builder

WORKDIR /app

# Install build tools and SSL libraries
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# 1. Copy the local dependency first
COPY beanie_keeper ./beanie_keeper

# 2. Copy the main API manifests for dependency caching
COPY beanie_api/Cargo.toml beanie_api/Cargo.loc[k] ./beanie_api/

# Create a dummy main.rs inside the api folder to build dependencies
RUN mkdir -p beanie_api/src && echo "fn main() {}" > beanie_api/src/main.rs

# Pin the transitive 'time' crate inside the api directory context
WORKDIR /app/beanie_api
RUN cargo update -p time --precise 0.3.36 || true
RUN cargo build --release
RUN rm -rf src

# 3. Copy the actual source code and compile the real binary
COPY beanie_api/src ./src
RUN touch src/main.rs
RUN cargo build --release --bin beani_api

# --- Stage 2: Runtime Stage ---
FROM debian:trixie-slim AS runner

WORKDIR /app

# Install runtime SSL certificates and SSL library
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Copy compiled binary and static assets (adjust target path since we built inside /app/beanie_api)
COPY --from=builder /app/beanie_api/target/release/beani_api /app/beanie-api
COPY beanie_api/public /app/public

EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

CMD ["./beanie-api"]