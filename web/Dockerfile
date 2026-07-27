# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.95.0
ARG NODE_VERSION=22.23.1
ARG NPM_VERSION=12.0.1
ARG DEBIAN_RUNTIME_SUITE=trixie
FROM rust:${RUST_VERSION}-bookworm AS builder

ARG RUST_VERSION
ARG WEB_REVISION=unknown
ARG UPSTREAM_REVISION=unknown
ARG BROTLI_QUALITY=11

WORKDIR /src
COPY script/linux /tmp/install-zed-linux
RUN /tmp/install-zed-linux \
    && apt-get install -y --no-install-recommends brotli perl \
    && rm -rf /var/lib/apt/lists/*

RUN rustup toolchain install "${RUST_VERSION}" \
    && rustup toolchain install nightly --component rust-src \
    && rustup target add wasm32-unknown-unknown --toolchain nightly \
    && cargo install wasm-bindgen-cli --version 0.2.120 --locked

COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    WEB_REVISION="${WEB_REVISION}" \
    UPSTREAM_REVISION="${UPSTREAM_REVISION}" \
    BROTLI_QUALITY="${BROTLI_QUALITY}" \
    RUST_STABLE_TOOLCHAIN="${RUST_VERSION}" \
    ./web/build.sh

FROM node:${NODE_VERSION}-${DEBIAN_RUNTIME_SUITE}-slim AS node-runtime
ARG NPM_VERSION
RUN npm install --global "npm@${NPM_VERSION}" \
    && npm cache clean --force

FROM debian:${DEBIAN_RUNTIME_SUITE}-slim AS runtime

RUN apt-get update \
    && apt-get upgrade -y --no-install-recommends \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        libasound2 \
        libfontconfig1 \
        libglib2.0-0 \
        libssl3 \
        libvulkan1 \
        libwayland-client0 \
        libx11-xcb1 \
        libxkbcommon0 \
        openssh-client \
        tini \
        zsh \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 --shell /bin/zsh zed \
    && mkdir -p /workspace /opt/zed-web/static \
    && chown -R zed:zed /workspace /home/zed

COPY --from=node-runtime /usr/local/ /usr/local/
COPY --from=builder /src/web/dist/bin/zed-web-server /usr/local/bin/zed-web-server
COPY --from=builder /src/web/dist/bin/zed-extension-runtime /usr/local/bin/zed-extension-runtime
COPY --from=builder /src/web/dist/static/ /opt/zed-web/static/
COPY --chmod=0755 web/entrypoint.sh /usr/local/bin/zed-web-entrypoint

RUN node --version && npm --version && npx --version

ENV HOME=/home/zed \
    ZED_EXTENSION_RUNTIME=/usr/local/bin/zed-extension-runtime \
    ZED_WEB_PORT=8090 \
    ZED_WEB_WORKSPACE=/workspace

USER zed
WORKDIR /workspace
EXPOSE 8090
VOLUME ["/workspace"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8090/favicon.ico >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/zed-web-entrypoint"]
