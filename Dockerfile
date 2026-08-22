# Build stage
FROM rust:1.86-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY api/ api/

# Feature set to build. `gateway` (default) includes the proxy plus the
# priority-queue gateway; use `gateway-redis` for the distributed fleet build.
ARG FEATURES=gateway

# Stub example/benchmark sources (excluded by .dockerignore but referenced in
# Cargo.toml). They aren't compiled by a plain `cargo build`, but the manifest
# paths must resolve.
RUN mkdir -p examples benchmarks && \
    touch examples/chat.rs examples/stream.rs \
          benchmarks/bench.rs benchmarks/loadtest.rs benchmarks/gateway_loadtest.rs

# Build with rustls (no OpenSSL needed) + strip binary
RUN cargo build --release --features ${FEATURES} && \
    strip target/release/llmshim

# Runtime stage — distroless (just glibc + CA certs, no shell)
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/llmshim /llmshim

EXPOSE 3000

ENV LLMSHIM_HOST=0.0.0.0
ENV LLMSHIM_PORT=3000

ENTRYPOINT ["/llmshim"]
# Default to the proxy; run the priority-queue gateway with:
#   docker run ... llmshim gateway
# Gateway env: LLMSHIM_GATEWAY_KEYS_FILE (auth), LLMSHIM_REDIS_URL (distributed;
# needs a gateway-redis build), LLMSHIM_RATE_LIMIT_RPM/TPM, LLMSHIM_GATEWAY_*.
CMD ["proxy"]
