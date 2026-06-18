# Build stage: compile the Rust binary with the toolchain image.
FROM rust:1-bookworm AS builder
ARG TARGETARCH
WORKDIR /src
ENV CARGO_TARGET_DIR=/src/target/${TARGETARCH}

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=cargo-target-${TARGETARCH},target=/src/target \
    cargo build --release --locked -p typedb-mcp \
    && cp "${CARGO_TARGET_DIR}/release/typedb-mcp" /typedb-mcp \
    && strip /typedb-mcp

# Runtime stage: slim Debian with just the TLS roots the gRPC driver needs.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /typedb-mcp /usr/local/bin/typedb-mcp
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

EXPOSE 8001
ENV LISTEN_HTTP=0.0.0.0:8001
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
