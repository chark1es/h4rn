# syntax=docker/dockerfile:1.7

# h4rn-connect is the self-hostable relay. The daemon image is a separate
# concern (it needs harness binaries and a writable state directory), so this
# image ships only the relay process.

FROM rust:bookworm AS builder

WORKDIR /src

COPY Cargo.toml Cargo.lock rust-toolchain.toml rustfmt.toml ./
COPY crates ./crates

RUN cargo build --release --locked -p h4rn-connect -p h4rn-daemon

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home h4rn

COPY --from=builder /src/target/release/h4rn-connect /usr/local/bin/h4rn-connect
COPY --from=builder /src/target/release/h4rn-daemon /usr/local/bin/h4rn-daemon

USER h4rn
EXPOSE 8787

ENV H4RN_CONNECT_ADDR=0.0.0.0:8787
ENV RUST_LOG=h4rn_connect=info

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/healthz || exit 1

ENTRYPOINT ["h4rn-connect"]
