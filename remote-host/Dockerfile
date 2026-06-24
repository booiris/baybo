# syntax=docker/dockerfile:1
#
# remote-host "C" role binaries: remote-host-push (APNs sender) and
# remote-host-relay (blind pairing/content rendezvous). remote-host is a
# self-contained Cargo workspace (the repo root excludes it and it has no
# path-deps reaching outside this directory), so the build context is just
# remote-host/ — no repo-root crates are pulled in.

FROM rust:1.94.1-bookworm AS builder
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
    cargo build --release --locked --bins \
    && mkdir -p /out \
    && cp target/release/remote-host-push target/release/remote-host-relay /out/

FROM debian:bookworm-slim AS runtime
# rustls validates TLS against the OS trust store (no roots are bundled), so
# APNs egress needs ca-certificates at runtime.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/remote-host-push /usr/local/bin/remote-host-push
COPY --from=builder /out/remote-host-relay /usr/local/bin/remote-host-relay
# Each compose service selects its binary via `command:`; this default lets the
# image also run standalone.
CMD ["remote-host-relay"]
