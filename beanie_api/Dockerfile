# --- Stage 1: Builder Stage ---
FROM rust:latest AS builder

WORKDIR /app

# Install build tools and SSL libraries
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifest (and Cargo.lock if present) to cache dependencies
COPY Cargo.toml Cargo.loc[k] ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
# Pin the transitive 'time' crate to prevent Rust 1.88+ edition mismatch
RUN cargo update -p time --precise 0.3.36 || true
RUN cargo build --release
RUN rm -rf src

# Copy actual source code and compile the real binary
COPY src ./src
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

# Copy compiled binary and static assets
COPY --from=builder /app/target/release/beani_api /app/beanie-api
COPY public /app/public

EXPOSE 8080
ENV LISTEN_ADDR=0.0.0.0:8080

CMD ["./beanie-api"]