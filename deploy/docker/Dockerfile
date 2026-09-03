ARG NODE_VERSION=22.23.2
ARG BUN_VERSION=1.3.14
ARG UV_VERSION=0.8.17
ARG RUST_VERSION=1.97.1

FROM node:${NODE_VERSION}-bookworm-slim AS node
FROM oven/bun:${BUN_VERSION} AS bun
FROM ghcr.io/astral-sh/uv:${UV_VERSION} AS uv

FROM rust:${RUST_VERSION}-bookworm AS builder

ARG PNPM_VERSION=9.13.2

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        libssl-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY --from=node /usr/local/ /usr/local/
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun

RUN npm install --global "pnpm@${PNPM_VERSION}"

WORKDIR /src
COPY . .

RUN --mount=type=cache,id=baybo-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=baybo-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=baybo-target,target=/src/target \
    --mount=type=cache,id=baybo-pnpm-store,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile \
    && BAYBO_REQUIRE_SIDECARS=1 cargo build --release --locked -p baybo \
    && install -D -m 0755 target/release/baybo /out/baybo

FROM node:${NODE_VERSION}-bookworm-slim AS runtime

ARG BAYBO_UID=10001
ARG BAYBO_GID=10001

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        fonts-liberation \
        git \
        jq \
        libasound2 \
        libatk-bridge2.0-0 \
        libatk1.0-0 \
        libcairo2 \
        libcups2 \
        libdbus-1-3 \
        libdrm2 \
        libexpat1 \
        libfontconfig1 \
        libgbm1 \
        libglib2.0-0 \
        libgtk-3-0 \
        libnspr4 \
        libnss3 \
        libpango-1.0-0 \
        libpangocairo-1.0-0 \
        libx11-6 \
        libx11-xcb1 \
        libxcb1 \
        libxcomposite1 \
        libxdamage1 \
        libxext6 \
        libxfixes3 \
        libxkbcommon0 \
        libxrandr2 \
        openssh-client \
        openssl \
        ripgrep \
        tini \
        xdg-utils \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid "${BAYBO_GID}" baybo \
    && useradd --uid "${BAYBO_UID}" --gid "${BAYBO_GID}" --create-home --shell /bin/bash baybo \
    && mkdir -p /var/lib/baybo /var/cache/baybo \
    && chown -R baybo:baybo /var/lib/baybo /var/cache/baybo \
    && git config --system user.name "Baybo" \
    && git config --system user.email "baybo@localhost"

COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
COPY --from=uv /uv /usr/local/bin/uv
COPY --from=uv /uvx /usr/local/bin/uvx
COPY --from=builder /out/baybo /usr/local/bin/baybo
COPY --chmod=0755 deploy/docker/entrypoint.sh /usr/local/bin/baybo-entrypoint

ENV HOME=/var/lib/baybo/home \
    XDG_CACHE_HOME=/var/cache/baybo \
    UV_CACHE_DIR=/var/cache/baybo/uv \
    BUN_INSTALL_CACHE_DIR=/var/cache/baybo/bun \
    NPM_CONFIG_CACHE=/var/cache/baybo/npm \
    BAYBO_WORKSPACE=/var/lib/baybo \
    BAYBO_CONFIG_PATH=/var/lib/baybo/config/baybo.json \
    BAYBO_GATEWAY_PORT=8888

WORKDIR /var/lib/baybo
USER baybo

EXPOSE 8888

HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=6 \
    CMD curl --fail --silent --show-error http://127.0.0.1:8888/readyz >/dev/null || exit 1

ENTRYPOINT ["tini", "--", "/usr/local/bin/baybo-entrypoint"]
CMD ["baybo", "gateway", "start"]
