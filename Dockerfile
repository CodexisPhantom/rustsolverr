# syntax=docker/dockerfile:1

# Rust toolchain plus what BoringSSL (wreq's TLS library) needs to build.
FROM rust:1-slim-bookworm AS toolchain
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential cmake perl pkg-config libclang-dev git \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add clippy rustfmt
WORKDIR /src

FROM toolchain AS build
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rustsolverr \
    && cp target/release/rustsolverr /rustsolverr

FROM python:3.12-slim-bookworm
ARG CAMOUFOX_VERSION=0.5.9
# tini reaps zombie processes. xvfb serves HEADLESS=virtual. The libraries are
# the ones Firefox needs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        tini xvfb libgtk-3-0 libdbus-glib-1-2 libasound2 libx11-xcb1 libxtst6 libxt6 \
        fonts-liberation \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 solver \
    && mkdir /config \
    && chown solver:solver /config

USER solver
# The packages go into a virtual environment that the user owns: `camoufox
# fetch` writes its fingerprint model next to the installed package. The
# browser itself goes into the user's cache directory.
RUN python -m venv /home/solver/venv \
    && /home/solver/venv/bin/pip install --no-cache-dir "camoufox==${CAMOUFOX_VERSION}" \
    && /home/solver/venv/bin/python -m camoufox fetch

COPY --from=build /rustsolverr /usr/local/bin/rustsolverr
COPY scripts/smoke.py /usr/local/share/rustsolverr/smoke.py

ENV PORT=8191 \
    PYTHON=/home/solver/venv/bin/python \
    HEADLESS=virtual \
    CACHE_FILE=/config/cookies.json
EXPOSE 8191
VOLUME /config
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD python3 -c "import os, urllib.request; urllib.request.urlopen('http://127.0.0.1:' + os.environ.get('PORT', '8191') + '/health', timeout=4)"
ENTRYPOINT ["tini", "--", "rustsolverr"]
