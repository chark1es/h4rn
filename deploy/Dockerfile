# syntax=docker/dockerfile:1.7

# h4rn-connect is the self-hostable relay. The daemon image is a separate
# concern (it needs harness binaries and a writable state directory), so this
# image ships only the relay process.
#
# The build is a normal Rust workspace build. Migrations are embedded by
# `sqlx::migrate!` at compile time, so the runtime image carries no source tree.

FROM rust:bookworm AS builder

WORKDIR /src

# Only the workspace inputs are copied. `Cargo.lock` is required so the image
# is reproducible; `.dockerignore` keeps `target/` and the UI out of context.
COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates

RUN cargo build --release --locked -p h4rn-connect

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home h4rn

COPY --from=builder /src/target/release/h4rn-connect /usr/local/bin/h4rn-connect

USER h4rn
EXPOSE 8787

# `H4RN_CONNECT_BIND` is what the process reads. The public URL, database URL,
# KEK and providers are deployment configuration and intentionally have no
# compiled-in defaults (ADR-018).
ENV H4RN_CONNECT_BIND=0.0.0.0:8787
ENV RUST_LOG=h4rn_connect=info,tower_http=info

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1

ENTRYPOINT ["h4rn-connect"]
