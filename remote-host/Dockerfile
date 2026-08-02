# syntax=docker/dockerfile:1
#
# The unified remote-host "C" binary — serves the push (APNs sender) and/or
# relay (blind pairing/content rendezvous) roles on one listener, selected at
# runtime by PUSH_ENABLE / RELAY_ENABLE. remote-host is a self-contained Cargo
# workspace (the repo root excludes it and it has no path-deps reaching outside
# this directory), so the build context is just remote-host/.

# Keep in step with the repo's rust-toolchain.toml: the build context is only
# remote-host/, so that file never reaches this image and the tag here is the
# whole pin. It is also a floor, not just a preference — libsqlite3-sys 0.38
# (rusqlite 0.40) builds with `cfg_select!`, stable only since 1.95.
FROM rust:1.97.1-bookworm AS builder
# rustls' crypto provider (aws-lc-rs) and ring (via jsonwebtoken) compile C +
# assembly: they need cmake plus a C toolchain and perl (the latter two already
# ship in the full rust image). No openssl anywhere, so no libssl-dev.
RUN apt-get update && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# BuildKit cache mounts keep the crates.io registry + target/ warm across
# rebuilds; the binaries are copied out to /out because cache-mount contents
# are not part of the image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin remote-host \
    && mkdir -p /out \
    && cp target/release/remote-host /out/

FROM debian:bookworm-slim AS runtime
# rustls validates TLS against the OS trust store (no roots are bundled), so
# APNs egress needs ca-certificates at runtime.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/remote-host /usr/local/bin/remote-host
# Roles are selected via PUSH_ENABLE / RELAY_ENABLE in the environment.
CMD ["remote-host"]
