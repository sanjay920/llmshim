# Build stage
FROM rust:1.86-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY api/ api/

# Build release binary with proxy feature
RUN cargo build --release --features proxy --bin llmshim-proxy --bin llmshim-config

# Runtime stage — minimal image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/llmshim-proxy /usr/local/bin/
COPY --from=builder /app/target/release/llmshim-config /usr/local/bin/

EXPOSE 3000

# Config can be mounted at /root/.llmshim/config.toml
# or API keys can be passed as env vars
ENV LLMSHIM_HOST=0.0.0.0
ENV LLMSHIM_PORT=3000

CMD ["llmshim-proxy"]
