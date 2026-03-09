# Build stage
FROM rust:1.86-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY api/ api/

# Stub examples (excluded by .dockerignore but referenced in Cargo.toml)
RUN mkdir -p examples && touch examples/chat.rs examples/stream.rs

# Build with rustls (no OpenSSL needed) + strip binary
RUN cargo build --release --features proxy && \
    strip target/release/llmshim

# Runtime stage — distroless (just glibc + CA certs, no shell)
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/llmshim /llmshim

EXPOSE 3000

ENV LLMSHIM_HOST=0.0.0.0
ENV LLMSHIM_PORT=3000

ENTRYPOINT ["/llmshim"]
CMD ["proxy"]
