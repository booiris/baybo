#!/bin/sh
# Baybo browser-sidecar container entrypoint.
#
# Boot order:
#   1. Sanity-check Chrome can load (`chrome --version`); on failure, ldd
#      the binary and surface "not found" libs so `docker logs` makes
#      missing-shared-library failures actionable.
#   2. Verify /data/profile is writable as the runtime UID so we don't
#      stall later inside Chrome with a cryptic profile-init error.
#   3. Xvfb on :99 with the operator-configured viewport, waited on via
#      `xdpyinfo` so we know it's actually accepting X clients (not just
#      that the socket file appeared).
#   4. Optional fluxbox + x11vnc + websockify + noVNC web bridge on
#      WEB_VNC_PORT, all keyed off the single env var. x11vnc binds a
#      fixed internal :5900 (loopback only); websockify proxies the
#      operator's WEB_VNC_PORT to it. Native-VNC clients are not
#      supported by design — browser-only access via noVNC.
#   5. exec chrome (loopback CDP on 9222) + socat relay (0.0.0.0:9223 ->
#      127.0.0.1:9222). Chrome 134+ silently ignores
#      `--remote-debugging-address=0.0.0.0` (DNS-rebind protection), so the
#      relay is what makes the host-published port reachable. The host
#      wrapper resolves the published 9223 port via `docker port` and
#      connects via `args.browserUrl = http://127.0.0.1:<published>`.
#
# Env vars (set by `docker run -e ...` from the TS wrapper):
#   VIEWPORT      required, format `<W>x<H>` (e.g. 1920x1080).
#   WEB_VNC_PORT  optional. When set, x11vnc + websockify + noVNC start
#                 on this port (web-only — no native-VNC port is exposed).
#                 Open `http://127.0.0.1:<WEB_VNC_PORT>/vnc.html` from
#                 any browser.

set -eu

log() { echo "[entrypoint] $*" >&2; }
fatal() { log "FATAL: $*"; exit 1; }

: "${VIEWPORT:=1920x1080}"

log "starting; uid=$(id -u) gid=$(id -g) viewport=${VIEWPORT} web_vnc_port=${WEB_VNC_PORT:-unset}"

# Step 1 — sanity-check Chrome can at least exec. A missing shared library
# is the single most common cause of "Chrome silently doesn't come up" in
# docker mode; without this check the failure surfaces as a 30s CDP timeout
# with empty `docker logs`.
if ! /usr/local/bin/chrome --version >/dev/null 2>&1; then
    log "chrome --version failed; running ldd to find missing libs:"
    ldd /usr/local/bin/chrome 2>&1 | grep -i "not found" >&2 || log "(ldd reported no missing libs — chrome may be crashing for another reason)"
    fatal "chrome binary is not loadable"
fi
log "chrome version: $(/usr/local/bin/chrome --version 2>&1 | tr -d '\n')"

# Step 2 — profile dir writability. Bind-mounted from host; if the host
# operator's UID doesn't match what wrote the dir, Chrome can't write its
# profile and dies with no useful log (it just hangs in user-data-dir init).
if ! touch /data/profile/.baybo-write-test 2>/dev/null; then
    log "/data/profile is not writable as uid $(id -u)"
    log "  dir owner: $(stat -c '%u:%g' /data/profile 2>/dev/null || echo '?')"
    log "  fix: run Baybo under the same OS user that owns the profile dir, or"
    log "       remove the dir on the host and let Baybo recreate it under your UID."
    fatal "cannot write to /data/profile"
fi
rm -f /data/profile/.baybo-write-test

# Step 2.5 — clear stale Chrome singleton locks. The profile dir is
# bind-mounted from the host and may carry a `SingletonLock` symlink
# left over from a previous host-headless run (or a previous docker
# container with a different generated hostname). Chrome's lock points
# at `<hostname>-<pid>` and Chrome refuses to start when the recorded
# hostname differs from the current one — it can't kill-probe across
# hosts to verify the PID is dead, so it conservatively assumes the
# other instance is still alive (the classic NFS-shared-profile case).
#
# Inside this container we know we're not the host that wrote the lock
# (different hostname) and the operator explicitly switched to docker
# mode, which is mutually exclusive with host-headless inside one Baybo
# process. So a stale lock here is safe to clear; if a *different* Baybo
# instance is sharing this profile that's a misconfiguration the
# entrypoint can't detect either way.
for lockname in SingletonLock SingletonCookie SingletonSocket; do
    lockpath="/data/profile/$lockname"
    if [ -L "$lockpath" ] || [ -e "$lockpath" ]; then
        target="$(readlink "$lockpath" 2>/dev/null || echo '<not a symlink>')"
        log "clearing stale Chrome lock $lockpath (was: $target)"
        rm -f "$lockpath"
    fi
done

mkdir -p /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix 2>/dev/null || true

# Step 3 — Xvfb. Wait on `xdpyinfo` instead of just the socket file so we
# know the X server is actually accepting connections; the socket can
# appear before Xvfb is ready, leading to Chrome racing the X server and
# failing to attach.
log "starting Xvfb on :99 (${VIEWPORT}x24)"
Xvfb :99 -screen 0 "${VIEWPORT}x24" -nolisten tcp -ac &
XVFB_PID=$!
trap 'kill $XVFB_PID 2>/dev/null || true' EXIT

export DISPLAY=:99
xvfb_ready=0
for _ in $(seq 1 60); do
    if xdpyinfo -display :99 >/dev/null 2>&1; then
        xvfb_ready=1
        break
    fi
    sleep 0.1
done
if [ "$xvfb_ready" != "1" ]; then
    fatal "Xvfb did not accept X connections within 6s"
fi
log "Xvfb ready on :99"

# Synthesize a fontconfig file when /data/fonts has anything in it; lets
# the host bind-mount custom fonts (e.g. CJK packs, brand fonts) without
# rebuilding the image. Empty / missing → no override, just system fonts.
if [ -d /data/fonts ] && [ -n "$(ls -A /data/fonts 2>/dev/null)" ]; then
    FONTCONFIG_FILE=/tmp/baybo-fonts.conf
    cat > "$FONTCONFIG_FILE" <<EOF
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>
  <dir>/data/fonts</dir>
</fontconfig>
EOF
    export FONTCONFIG_FILE
    log "fontconfig override: $FONTCONFIG_FILE (extra dir /data/fonts)"
fi

# Step 4 — web-only VNC observability via fluxbox + x11vnc + websockify
# + noVNC. Single env var (WEB_VNC_PORT) gates the whole stack — no raw-
# VNC port is exposed by design (browser-only access, easier remote
# tunnelling, no native VNC client install required on the operator side).
#
# x11vnc binds a fixed internal :5900 (loopback only); websockify proxies
# the operator's WEB_VNC_PORT to it. `--web=/usr/share/novnc` makes
# websockify also serve the noVNC HTML client on the same port, so a
# single host port covers both static HTML and the WS data channel.
# Operator opens `http://127.0.0.1:<WEB_VNC_PORT>/vnc.html` in any browser.
INTERNAL_VNC_PORT=5900
if [ -n "${WEB_VNC_PORT:-}" ]; then
    log "starting fluxbox + x11vnc on internal :${INTERNAL_VNC_PORT}"
    fluxbox -display :99 >/dev/null 2>&1 &
    x11vnc -display :99 -nopw -listen 127.0.0.1 -rfbport "$INTERNAL_VNC_PORT" \
           -shared -forever -bg -quiet
    log "starting websockify + noVNC on :${WEB_VNC_PORT} (proxying to localhost:${INTERNAL_VNC_PORT})"
    websockify --web=/usr/share/novnc/ "$WEB_VNC_PORT" \
               "127.0.0.1:$INTERNAL_VNC_PORT" >&2 &
fi

# Step 5 — launch chrome + socat relay.
#
# Chrome 134+ silently ignores `--remote-debugging-address=0.0.0.0` for
# DNS-rebinding protection: Chrome's CDP server now refuses any TCP
# connection whose remote endpoint isn't loopback (verified empirically
# with Chrome for Testing 148.0.7778). The flag is accepted on the
# command line but Chrome still binds 127.0.0.1 only.
#
# Workaround: bind Chrome to loopback (the default), then run a `socat`
# relay on 0.0.0.0:9223 → 127.0.0.1:9222. From Chrome's perspective every
# CDP connection comes from 127.0.0.1 (the relay) → loopback check
# passes. From the host's perspective port 9223 is what gets `-p`-mapped
# to a published host port.
#
# `--remote-allow-origins=*` is required since Chrome 111: WebSocket
# upgrade requests need their Origin header allowlisted, otherwise Chrome
# rejects the WS even after the HTTP handshake works. Puppeteer's
# `connect()` doesn't set Origin, so `*` is the simplest fix.
#
# `--no-sandbox` is mandatory in this image (slim base ships no SUID
# `chrome-sandbox`; the container is the trust boundary).
#
# `--enable-logging=stderr` surfaces Chrome's own stderr in `docker logs`
# (default is to write to a logfile inside the user-data-dir).
#
# `--disable-dev-shm-usage` is belt-and-braces: docker.ts passes
# `--shm-size=2g` so /dev/shm is large enough for Chrome by default, but
# older docker engines silently ignore that flag and Chrome OOMs on
# memory-heavy pages with no obvious signal. Setting this flag makes
# Chrome back its shared-memory regions with /tmp regardless, so the
# /dev/shm size is no longer a hidden cliff.
log "launching chrome (loopback CDP on 9222) + socat relay (0.0.0.0:9223 -> 127.0.0.1:9222)"
/usr/local/bin/chrome \
    --remote-debugging-port=9222 \
    --remote-allow-origins=* \
    --user-data-dir=/data/profile \
    --no-first-run \
    --no-default-browser-check \
    --disable-default-apps \
    --no-sandbox \
    --disable-gpu \
    --disable-dev-shm-usage \
    --enable-logging=stderr \
    --window-position=0,0 \
    --window-size="${VIEWPORT%x*},${VIEWPORT#*x}" \
    --display=:99 &
CHROME_PID=$!

# socat is robust to "upstream not ready yet" — its `accept` loop only
# tries to connect to 127.0.0.1:9222 when a client arrives, and a failed
# connect just resets that one client. The host wrapper's `waitForCdp`
# retries on a 250ms cadence, so the first few resets while Chrome is
# still booting are harmless.
#
# `reuseaddr` so a fast container restart doesn't trip TIME_WAIT.
# `nodelay` so latency-sensitive small CDP frames aren't buffered.
socat TCP-LISTEN:9223,fork,reuseaddr,bind=0.0.0.0 \
      TCP:127.0.0.1:9222,nodelay &
SOCAT_PID=$!
log "socat relay PID=$SOCAT_PID; chrome PID=$CHROME_PID; waiting on chrome"

# Tini -g (set in Dockerfile ENTRYPOINT) sends SIGTERM to the whole
# process group on `docker stop`, so Chrome and socat both get it. This
# trap is belt-and-braces in case some signal arrives outside that path.
trap 'kill -TERM "$CHROME_PID" "$SOCAT_PID" 2>/dev/null; wait' TERM INT

# Block on Chrome; when it exits (graceful shutdown or crash), tear down
# the relay so the container exits too — the host wrapper's container-
# status probe needs to see `exited` rather than `running` to bail out
# of the CDP wait early.
wait "$CHROME_PID"
chrome_exit=$?
log "chrome exited (status=$chrome_exit); shutting down relay"
kill -TERM "$SOCAT_PID" 2>/dev/null || true
wait "$SOCAT_PID" 2>/dev/null || true
exit "$chrome_exit"
