# --- Stage 1: Builder Stage ---
FROM rust:latest AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies by building a dummy binary first
COPY Cargo.toml Cargo.loc[k] ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
# Pin the transitive 'time' crate to prevent Rust 1.88+ edition mismatch
RUN cargo update -p time --precise 0.3.36 || true
RUN cargo build --release
RUN rm -rf src

# Copy real source code and compile
COPY src ./src
RUN touch src/main.rs
RUN cargo build --release --bin beanie_keeper

# --- Stage 2: Minimal Runtime Stage ---
FROM debian:trixie-slim AS runner

WORKDIR /app

# Install runtime SSL certificates and SSL library
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/beanie_keeper /usr/local/bin/beanie_keeper

CMD ["beanie_keeper"]