# Build the `ladder` proxy as a small runtime image.
#
# Two stages: a Rust toolchain that compiles the release binary, and a slim
# Debian runtime that carries only the binary and a default configuration.

FROM rust:1-bookworm AS builder

WORKDIR /src

# Compile the dependency graph against stub sources first, so editing this
# crate does not invalidate the layer holding everything it depends on. Without
# this every source change rebuilds the whole tree.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src bin \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > bin/ladder.rs \
    && cargo build --release --locked --bin ladder \
    && rm -rf src bin

COPY src ./src
COPY bin ./bin
COPY config.example.toml ./config.example.toml

# Cargo decides what to rebuild from mtimes, and a fresh COPY can land with a
# timestamp older than the stub build; touching forces the real sources to win.
RUN touch src/lib.rs bin/ladder.rs \
    && cargo build --release --locked --bin ladder

# The container ships the committed example as its default configuration. The
# assertion is the point: loopback inside a container is reachable by nothing,
# so a config that ever went back to 127.0.0.1 must fail the build rather than
# produce an image that silently answers no one.
RUN mkdir -p /out \
    && cp config.example.toml /out/config.toml \
    && grep -q '^bind = "0\.0\.0\.0:' /out/config.toml

FROM debian:bookworm-slim AS runtime

# TLS roots are compiled into the binary through `webpki-roots`, so no CA bundle
# is needed. `curl` is here only for the health check below; it is what makes a
# container that has stopped serving visible to an orchestrator.
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl \
    && rm -rf /var/lib/apt/lists/*

# An unprivileged fixed uid, so a mounted config can be made readable to it.
RUN useradd --system --uid 10001 --user-group --no-create-home ladder

COPY --from=builder /src/target/release/ladder /usr/local/bin/ladder
COPY --from=builder /out/config.toml /etc/ladder/config.toml

USER ladder:ladder
EXPOSE 6969

# The router loads every order book before it binds, so a container is
# deliberately not healthy the instant it starts. `start-period` covers that
# without masking a genuine failure later.
HEALTHCHECK --interval=30s --timeout=5s --start-period=90s --retries=3 \
    CMD curl -fsS http://127.0.0.1:6969/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/ladder"]
CMD ["--config", "/etc/ladder/config.toml"]

LABEL org.opencontainers.image.title="llm-ladder-router" \
      org.opencontainers.image.description="Budget-aware routing across LLM marketplace tiers, behind an OpenAI- and Anthropic-compatible proxy." \
      org.opencontainers.image.source="https://github.com/senamakel/llm-ladder-router" \
      org.opencontainers.image.licenses="GPL-3.0-only"
