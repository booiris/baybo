# Aura browser-sidecar image: consumer Google Chrome inside an Xvfb display
# so the agent gets a non-headless Chrome (better simulates a real user,
# dodges `HeadlessChrome` fingerprint checks). The TS sidecar stays on
# the host and connects via CDP — `args.browserUrl =
# http://127.0.0.1:<published>`.
#
# **Why consumer Chrome (not Chrome for Testing)**: same Blink/V8/Skia
# source, same proprietary codecs and Widevine, but several anti-bot
# vendors (Cloudflare/Akamai/DataDome/etc.) maintain separate fingerprints
# for the "for Testing" build — different installer signature, no Google
# sign-in code paths, slightly different startup behavior. Consumer Chrome
# matches what real users have installed and so is harder to flag as
# automated infrastructure.
#
# Build context is `tool-src/browser/docker/`. Tag is computed by the TS
# wrapper as `aura-browser:<sha256(Dockerfile + entrypoint.sh)[..12]>` —
# any Aura update that changes either file triggers a rebuild and pulls
# whatever Chrome stable was current at build time. To force a refresh
# of an unchanged image (e.g. to pick up a Chrome security update),
# operators run `docker rmi aura-browser:<tag>` and restart Aura.

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

# Container infrastructure only — Xvfb display + VNC stack + CDP relay
# (socat) + tini + fonts. Chrome's runtime libraries (libnss3, libgbm1,
# libgtk-3-0, etc.) are pulled in transitively by the
# `google-chrome-stable` apt install below; no need to list them here.
# `fonts-noto-cjk` and `fonts-noto-color-emoji` cover the long tail of
# non-Latin scripts the agent might encounter; `fluxbox` is a tiny WM so
# windowed pages don't render against a blank root.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      tini \
      wget \
      xvfb \
      x11vnc \
      fluxbox \
      novnc \
      websockify \
      socat \
      x11-utils \
      fonts-noto-core \
      fonts-noto-cjk \
      fonts-noto-color-emoji \
 && rm -rf /var/lib/apt/lists/*

# Install consumer Google Chrome from Google's official .deb. We pull
# `_current` (whatever's stable at build time) because pinning a specific
# version doesn't gain much: the image tag is hashed from Dockerfile +
# entrypoint, so an Aura update that changes either auto-rebuilds and
# pulls fresh Chrome. For deliberate refreshes between Aura updates,
# operators `docker rmi aura-browser:<tag>` and restart.
#
# Trust path: HTTPS to dl.google.com (cert-pinned by the system CA
# bundle). `apt install /tmp/chrome.deb` resolves dependencies through
# the regular apt index.
#
# Note: Chrome's EULA applies. For automated/CI use this is the operator's
# responsibility to accept. We don't auto-update; the install is one-shot
# at image build time.
RUN wget -q -O /tmp/chrome.deb \
      https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb \
 && apt-get update \
 && apt-get install -y --no-install-recommends /tmp/chrome.deb \
 && rm /tmp/chrome.deb \
 && rm -rf /var/lib/apt/lists/* \
 && ln -sf /usr/bin/google-chrome /usr/local/bin/chrome

# COPY + chmod is what makes the entrypoint executable/readable. The
# host-side file's mode bit is lost via `decompress_to`'s umask handling
# (crates/gateway/src/sidecar/assets.rs), so set the full mode explicitly.
COPY entrypoint.sh /usr/local/bin/aura-browser-entrypoint.sh
RUN chmod 755 /usr/local/bin/aura-browser-entrypoint.sh

# Operator UID is overridden at `docker run` time via `--user $(id -u):$(id -g)`
# so files written into `/data/profile` are owned by the host operator
# (lets the same profile dir round-trip between host-headless and docker
# modes cleanly). The `aura` user just guarantees a writeable HOME for
# the rare case the operator skips `--user`.
RUN groupadd --system --gid 1000 aura \
 && useradd --system --gid aura --uid 1000 --create-home --home-dir /home/aura aura \
 && mkdir -p /data/profile /data/fonts \
 && chown -R aura:aura /data /home/aura
USER aura
WORKDIR /home/aura

# 9223 is the socat relay (entrypoint.sh) — Chrome 134+ silently ignores
# `--remote-debugging-address=0.0.0.0` so we bind Chrome to loopback 9222
# and relay 0.0.0.0:9223 → 127.0.0.1:9222. Only 9223 should ever be
# port-mapped.
EXPOSE 9223

ENTRYPOINT ["/usr/bin/tini", "-g", "--", "/usr/local/bin/aura-browser-entrypoint.sh"]
