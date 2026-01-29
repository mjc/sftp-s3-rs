# Multi-stage Docker build for SFTP-S3 server
# Stage 1: Builder (use bookworm to match runtime glibc version)
FROM rust:bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

# Build release binary with optimizations
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    bash \
    openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1000 sftp

# Create application directories
RUN mkdir -p /data /keys /config && \
    chown sftp:sftp /data /keys /config

WORKDIR /app

# Copy binary from builder
COPY --from=builder /build/target/release/sftp-s3 /usr/local/bin/sftp-s3

# Copy entrypoint script
COPY --chown=sftp:sftp docker-entrypoint.sh /app/
RUN chmod +x /app/docker-entrypoint.sh

# Set user to non-root
USER sftp

# Expose SFTP port
EXPOSE 2222

# Health check: verify TCP connection on SFTP port (uses PORT env var, defaults to 2222)
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD timeout 5 bash -c "</dev/tcp/127.0.0.1/\${PORT:-2222}" || exit 1

# Default entrypoint
ENTRYPOINT ["/app/docker-entrypoint.sh"]
