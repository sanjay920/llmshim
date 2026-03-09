# Build stage
FROM rust:1.86-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY api/ api/

RUN cargo build --release --features proxy

# Runtime stage — minimal image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/llmshim /usr/local/bin/

EXPOSE 3000

ENV LLMSHIM_HOST=0.0.0.0
ENV LLMSHIM_PORT=3000

CMD ["llmshim", "proxy"]
