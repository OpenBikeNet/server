# syntax=docker/dockerfile:1.7
#
# OpenBikeNet server image: the Rust server, the bundled routes and nothing else.

# --- Build -------------------------------------------------------------------
FROM rust:1-slim-bookworm@sha256:ed2c95ca377d6571b124b05e75e01207973ded639dce7d5542eb7bcd797e32aa AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin obn-server \
    && cp target/release/obn-server /usr/local/bin/obn-server

# --- Runtime -----------------------------------------------------------------
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS runtime

# Which build this is (git commit and date), passed by `make up` / `make update` and CI.
ARG OBN_BUILD_SHA=dev
ARG OBN_BUILD_DATE=

RUN groupadd --system --gid 10001 obn \
    && useradd --system --uid 10001 --gid obn --no-create-home --shell /usr/sbin/nologin obn

COPY --from=builder /usr/local/bin/obn-server /usr/local/bin/obn-server
# The routes come from the pinned protocol submodule, so they are baked into the image.
COPY protocol/routes /app/routes

ENV OBN_BIND=0.0.0.0:8080 \
    OBN_ROUTES_DIR=/app/routes \
    OBN_DB=/app/data/obn.sqlite \
    OBN_BUILD_SHA=${OBN_BUILD_SHA} \
    OBN_BUILD_DATE=${OBN_BUILD_DATE} \
    RUST_LOG=info

# The usage statistics (SQLite) live here (a volume in docker-compose.yml); owned by the server's user.
RUN mkdir -p /app/data && chown obn:obn /app/data

USER obn
WORKDIR /app
EXPOSE 8080

HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
    CMD ["obn-server", "healthcheck"]

ENTRYPOINT ["obn-server"]
