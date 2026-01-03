# Build Stage
FROM rust:slim-bookworm AS builder

WORKDIR /usr/src/app

# Install build dependencies for OpenSSL (required by reqwest/native-tls)
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests to cache dependencies
COPY Cargo.toml Cargo.lock ./

# Create a dummy main.rs to build dependencies first
# This allows Docker to cache the dependency build layer
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

# Copy actual source code
COPY src ./src

# Touch main.rs to force rebuild of the app itself
RUN touch src/main.rs
RUN cargo build --release

# Runtime Stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binary from builder
COPY --from=builder /usr/src/app/target/release/plex-radio-rust ./plex-radio

# Expose the port
EXPOSE 3000

CMD ["./plex-radio"]