# Deploying remote-host (the "C" role) with Docker Compose

`remote-host` is the operator-run **C** role: a single binary serving two roles
on one listener, reached by disjoint route paths.

| Role | When it runs | Routes | What it does |
|------|--------------|--------|--------------|
| **relay** | always on | `/pair/host/{rendezvous_id}`, `/pair/join/{rendezvous_id}`, `/control`, `/content/join/{node}`, `/content/host/{key}` | Blind WebSocket rendezvous for pairing + content (NAT'd gateways). Stateless. |
| **push** | auto, when an APNs `.p8` is configured (`APNS_P8_HOST_PATH`) | `POST /notify`, `POST /register` | Holds the APNs `.p8`; signs ES256 JWTs and POSTs the blind encrypted preview to Apple. |

So a bare config runs relay only; fill the APNs section in `.env` to add push.
It serves **plain `ws/http`** by default, or **`wss/https`** when you configure a
cert. (`remote-host-dashboard` is a library, not a service.)

## Quick start

```bash
cd remote-host
cp .env.example .env
$EDITOR .env                      # (optional) fill the APNs section to enable push
docker compose up -d --build
docker compose logs -f            # "remote-host: listening on 0.0.0.0:7777 (http/ws) — roles: push + relay"
```

`.env` is all optional — a bare config runs **relay only** (push needs the APNs section). Then **admit your gateways in the DB** (below) so they can connect.

Cross-arch (e.g. building on an Apple-Silicon Mac for an x86_64 host):
```bash
docker buildx build --platform linux/amd64 -t remote-host:latest --load .
```

## TLS

Phones reach the relay as `wss://` and the gateway reaches push as `https://` —
both on the **same** host:port (one listener). It serves plain `ws/http` by
default; set a cert and it serves `wss/https` automatically — same compose file,
no overlay, no proxy:

```bash
# in .env:
#   TLS_CERT_HOST_PATH=/etc/letsencrypt/live/c.example.com/fullchain.pem
#   TLS_KEY_HOST_PATH=/etc/letsencrypt/live/c.example.com/privkey.pem
#   PORT=443        # optional — for port-less wss://host / https://host URLs
docker compose up -d --build
```

The binary terminates TLS itself with rustls. The startup log shows `https/wss`
once a cert is configured (`http/ws` otherwise), and one cert covers both roles.
Leave the cert vars blank for plain `ws/http`. Renew the cert out of band (e.g.
certbot) and `docker compose restart`.

### Fronting with a terminator instead

If you'd rather terminate TLS elsewhere — Caddy / nginx / a cloud LB / Cloudflare
— leave the cert vars blank (plain `ws/http`) and proxy the whole host. With one
listener you don't need path-routing:

```caddyfile
c.example.com {
    reverse_proxy remote-host:7777
}
```

```yaml
  caddy:
    image: caddy:2
    restart: unless-stopped
    ports: ["443:443", "80:80"]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
    depends_on: [remote-host]
volumes:
  caddy_data:
```

With a terminator on the same host you can bind the service's `ports:` to
`127.0.0.1` only (or drop the host-publish — Caddy reaches it over the compose
network).

## Admission (which gateways may connect)

The allow-list of gateway `remote_api_key`s is a **SQLite table**, not env. The runtime polls it (every `ADMISSION_POLL_SECS`, default 30s), so you add/remove gateways **without a restart**. The DB is bind-mounted (`./data/admission.db` by default), so you edit it from the host:

```bash
# admit the remote_api_key you pass to `baybo device pair --remote-api-key`.
# A registered key MUST declare its own max_conns + max_bps (bytes/sec):
sqlite3 ./data/admission.db \
  "INSERT INTO remote_api_keys(remote_api_key, label, max_conns, max_bps) \
   VALUES('<key>', 'my gateway', 64, 4194304);"
# revoke:
sqlite3 ./data/admission.db "DELETE FROM remote_api_keys WHERE remote_api_key='<key>';"
# list:
sqlite3 ./data/admission.db "SELECT remote_api_key, label, max_conns, max_bps FROM remote_api_keys;"
```

The `remote_api_keys` table is created on first start; an empty table admits no one (fail-closed). The same list gates both roles.

**Registered keys must set `max_conns` + `max_bps`.** A `CHECK` constraint rejects a registered (non-guest) row that leaves either NULL — a registered key is meant to carry explicit limits, so the bare `INSERT(remote_api_key, label)` is no longer accepted. `per_server_max_bps` stays optional (NULL → falls back to the row's `max_bps`). **Guest** rows are exempt: they may omit any limit and inherit it from the `guest` template row (see *Guest tier & its defaults* below). (The `CHECK` guards freshly-created DBs only — `CREATE TABLE IF NOT EXISTS` can't add it to a DB made under an older schema.)

**Revoking is enforced on live connections, not just new ones.** On each poll, any key that was dropped from the table has its live relay connections (the gateway's control channel + any in-flight pairing/content legs) closed within the poll interval — so a revoked gateway is disconnected, not left running until it happens to drop. (The push role is per-request, so a revoked key simply gets `401` on its next `/notify`.)

**Per-key connection cap.** Each admitted `remote_api_key` may hold a bounded
number of simultaneous relay connections, so a buggy or abusive gateway can't
exhaust C. The limit is the row's `max_conns` column — **required on a registered
row** (see above), inherited from the `guest` template on a guest row. A NULL that
survives (a guest with no template default, or a legacy registered row from a
pre-`CHECK` DB) floors to the configured `MAX_CONNS_PER_REMOTE_API_KEY` (Docker
Compose defaults it to **64**, binary fallback **200**), hot-reloaded with the rest
of the table. Pairing and
chat host legs over the cap are refused with `429`; blob host legs use
`cap - CHAT_CONN_RESERVE` so chat can still reconnect; the gateway's one control
channel is exempt. Raise it per key for a deployment serving many concurrent
device sessions:

```bash
sqlite3 ./data/admission.db \
  "UPDATE remote_api_keys SET max_conns = 200 WHERE remote_api_key='<key>';"
```

**Per-key relay bandwidth.** Content and blob legs are throttled per
`remote_api_key`: the relay only authenticates the gateway (the phone-side leg is
anonymous), so the cap aggregates both directions across all content/blob legs
for that key. The rate is the row's `max_bps` column in bytes/sec — **required on a
registered row** (see above), inherited from the `guest` template on a guest row; a
NULL that survives floors to **1 MiB/s** — hot-reloaded with the table. A
per-`(remote_api_key, server)` sub-cap can be set with the still-optional
`per_server_max_bps` (NULL → falls back to the row's `max_bps`).
Enforcement is *throttle, not drop* — a gateway over its rate is paced via TCP
backpressure, nothing is lost. Pairing legs carry small Noise XXpsk0 frames and
are not bandwidth-throttled. Raise the cap for a key serving many concurrent
sessions or large attachments:

```bash
sqlite3 ./data/admission.db \
  "UPDATE remote_api_keys SET max_bps = 4194304 WHERE remote_api_key='<key>';"
```

**Guest tier & its defaults.** A row's `tier` is `'registered'` (the default —
explicit per-row limits) or `'guest'` (the shared trial tier). A guest row that
leaves `max_conns` / `max_bps` / `per_server_max_bps` NULL inherits the **guest-tier
defaults** instead of the registered role floor — and those defaults are simply the
columns on the reserved **`guest`** row itself (the shared trial key doubles as the
tier template). So you tune the whole guest tier with one ordinary `UPDATE`, no
separate config — any column the `guest` row also leaves NULL falls back to the
built-in default (**conns 2000**, **bps 20 MiB/s**, **per-server 2 MiB/s**):

```bash
# retune the guest tier (bytes/sec for the two bps columns):
sqlite3 ./data/admission.db \
  "UPDATE remote_api_keys SET max_conns = 500, max_bps = 10485760 \
   WHERE remote_api_key = 'guest';"
# (if the guest row doesn't exist yet, insert it as tier='guest'):
sqlite3 ./data/admission.db \
  "INSERT OR IGNORE INTO remote_api_keys(remote_api_key, tier, label) \
   VALUES('guest', 'guest', 'shared trial tier');"
```

These set the tier *defaults*; an individual guest row with its own non-NULL limit
still wins for that column, and registered rows are unaffected.

**Push frequency control.** `POST /notify` is rate-limited per `(remote_api_key, device_id)` so a buggy or abusive gateway can't hammer APNs or spam a phone. Over the limit it returns `429` (the gateway backs off and retries). The limit is a fixed **60 pushes/min sustained, burst 20** per device; only admitted, registered devices are metered. Hardcoded, not configurable.

**Per-source-IP flood backstop.** Ahead of admission, every relay WS-upgrade attempt is throttled per **client IP** (a token bucket, **10/s sustained, burst 60**), so a single host spraying upgrades across many rendezvous/node ids — or failing admission on each — is shed with `429` before any upgrade work. It also bounds the broker's pending map with a hard ceiling (`MAX_PENDING_LEGS`, **1024**): a new parked leg past the cap is refused `503` while matching an already-parked leg is exempt.

By default the client IP is the **socket peer**, which is correct when remote-host terminates TLS itself. **Behind a proxy (e.g. Cloudflare) the socket peer is the proxy's edge IP, so every client shares one bucket — this would throttle legitimate traffic.** Two ways to handle a proxied deployment:

```bash
# (a) Turn the origin limiter off and rate-limit at the proxy (it sees the real client):
RELAY_PER_IP_LIMIT=0

# (b) Resolve the real client IP from the proxy's header(s), tried in order
#     (e.g. behind Cloudflare; the first parseable IP wins, else the socket peer):
RELAY_CLIENT_IP_HEADERS=cf-connecting-ip
#     (or, with a fallback:)  RELAY_CLIENT_IP_HEADERS=cf-connecting-ip,x-forwarded-for
```

> **Trust a client-IP header ONLY when the origin is reachable solely via that proxy** — a [Cloudflare IP allowlist](https://www.cloudflare.com/ips/), a `cloudflared` Tunnel (no public origin), or Authenticated Origin Pulls (CF mTLS). Otherwise a direct-to-origin attacker can forge `cf-connecting-ip` to evade the limit or frame an arbitrary IP. For `x-forwarded-for` the **left-most** entry is taken as the original client, so it too is only safe behind a proxy that overwrites/anchors it.

## Gateway wiring (pair against this host)

The gateway holds **no** `.p8`, and there is **no `relay`/`push` block in `baybo.json`** — relay control + push are driven by the approved device row. Point them at this host by pairing with `--relay-url` (a one-time per-device choice, recorded on the row):

```sh
baybo device pair --relay-url wss://c.example.com --remote-api-key <admitted key>
```

That single WS URL covers both roles: the gateway dials `wss://c.example.com` for the relay control/content legs and POSTs push to `https://c.example.com/notify` (same host, scheme swapped). Omit the flags to use the built-in public proxy + its trial key `guest`. The `remote_api_key` must be admitted in the `remote_api_keys` table (see **Admission** above) — one key serves both roles. To move an already-paired device to a different host, re-pair with the new `--relay-url`.

## Notes

- **Relay-only / `.p8` isolation.** Leave the APNs section of `.env` blank and only the relay runs — no `.p8` on that host. Fill it in to add push. The `.p8` lives solely where you configure it.
- **State.** The admission allow-list is the SQLite table, persisted on the `./data` volume (survives restart). Device-token registrations are in-memory — dropped on restart, but the paired app can register when iOS delivers a token and the gateway re-registers an approved device before its first push attempt when it has non-empty APNs material.
- **APNs environment.** Push targets sandbox vs production **per device registration** (the token's env), so one deployment serves both — no env switch here. A debug-built app registers a sandbox token.
- **Logs** go to stderr (the relay has no `tracing` subscriber wired, so only the `eprintln!` startup/error lines are guaranteed).
- **Secrets.** `.env` and `*.p8` are gitignored. The `.p8` is mounted read-only as a Docker secret at `/run/secrets/apns_p8`; it never enters an image layer.
- **Hardening.** Containers run as root so the process can read a `0600` host `.p8`. To run non-root, make the `.p8` readable by that uid and add a `USER` to the Dockerfile.
