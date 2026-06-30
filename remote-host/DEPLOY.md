# Deploying remote-host (the "C" role) with Docker Compose

`remote-host` is the operator-run **C** role: a single binary serving two roles
on one listener, reached by disjoint route paths.

| Role | When it runs | Routes | What it does |
|------|--------------|--------|--------------|
| **relay** | always on | `/pair/host/{rendezvous_id}`, `/pair/join/{rendezvous_id}`, `/control`, `/content/join/{node}`, `/content/host/{key}` | Blind WebSocket rendezvous for pairing + content (NAT'd gateways). Stateless. |
| **push** | auto, when an APNs `.p8` is configured (`APNS_P8_HOST_PATH`) | `POST /notify`, `POST /register` | Holds the APNs `.p8`; signs ES256 JWTs and POSTs the blind encrypted preview to Apple. |

So a bare config runs relay only; fill the APNs section in `.env` to add push.
It serves **plain `ws/http`** by default, or **`wss/https`** when you configure a
cert. (`remote-host-dashboard` runs on its own separate listener
(`DASHBOARD_BIND_ADDR`, default `:7778`, plain HTTP by default or HTTPS with its
own cert) when `DASHBOARD_TOKEN` is set — see [Operator dashboard](#operator-dashboard);
off otherwise.)

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

The allow-list of gateway `remote_api_key`s is a **SQLite table**, not env. The runtime polls it (every `ADMISSION_POLL_SECS`, default 30s), so you add/remove gateways **without a restart**. The easiest way to create a key is the [operator dashboard](#operator-dashboard) (generate / admit / edit / revoke, with a one-time reveal). The DB is also bind-mounted (`./data/admission.db` by default), so you can edit it directly from the host:

```bash
# admit the remote_api_key you pass to `baybo device pair --remote-api-key`.
# A key MUST declare its own max_conns + max_bps (bytes/sec):
sqlite3 ./data/admission.db \
  "INSERT INTO remote_api_keys(remote_api_key, label, max_conns, max_bps) \
   VALUES('<key>', 'my gateway', 64, 4194304);"
# revoke:
sqlite3 ./data/admission.db "DELETE FROM remote_api_keys WHERE remote_api_key='<key>';"
# list:
sqlite3 ./data/admission.db "SELECT remote_api_key, label, max_conns, max_bps FROM remote_api_keys;"
```

The `remote_api_keys` table is created on first start; an empty table admits no one (fail-closed). It gates the **relay** (pairing + content legs); the **push** routes are keyless (authorized by the device delegation chain, see below).

**Every key must set `max_conns` + `max_bps`.** A `CHECK` constraint rejects a row that leaves either NULL — a key is meant to carry explicit limits, so the bare `INSERT(remote_api_key, label)` is not accepted. `per_server_max_bps` stays optional (NULL → falls back to the row's `max_bps`). (The `CHECK` guards freshly-created DBs only — `CREATE TABLE IF NOT EXISTS` can't add it to a DB made under an older schema; a NULL that survives on a legacy row floors to the role default below.)

**Revoking is enforced on live connections, not just new ones.** On each poll, any key that was dropped from the table has its live relay connections (the gateway's control channel + any in-flight pairing/content legs) closed within the poll interval — so a revoked gateway is disconnected, not left running until it happens to drop. Push does not consult this table; `/register` and `/notify` remain governed by the device delegation chain and push-specific abuse limits.

**Per-key connection cap.** Each admitted `remote_api_key` may hold a bounded
number of simultaneous relay connections, so a buggy or abusive gateway can't
exhaust C. The limit is the row's `max_conns` column — **required** (see above). A
NULL that survives (a legacy row from a pre-`CHECK` DB) floors to the configured
`MAX_CONNS_PER_REMOTE_API_KEY_FALLBACK` (Docker Compose defaults it to **64**,
binary fallback **200**), hot-reloaded with the rest of the table. Pairing and
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
for that key. The rate is the row's `max_bps` column in bytes/sec — **required**
(see above); a NULL that survives floors to **1 MiB/s** — hot-reloaded with the
table. A per-`(remote_api_key, server)` sub-cap can be set with the still-optional
`per_server_max_bps` (NULL → falls back to the row's `max_bps`).
Enforcement is *throttle, not drop* — a gateway over its rate is paced via TCP
backpressure, nothing is lost. Pairing legs carry small Noise XXpsk0 frames and
are not bandwidth-throttled. Raise the cap for a key serving many concurrent
sessions or large attachments:

```bash
sqlite3 ./data/admission.db \
  "UPDATE remote_api_keys SET max_bps = 4194304 WHERE remote_api_key='<key>';"
```

**Optional expiry.** A row may carry an `expires_at` (SQLite `datetime` text, UTC;
NULL → never expires) for time-boxed access. Once that instant passes, the key is
dropped from the in-memory allow-list on the next poll and its relay legs are
refused; the row itself stays in the table (shown as **expired** in the dashboard)
until you revoke it. Back-dating `expires_at` on an existing row therefore expires
it on the next reload:

```bash
sqlite3 ./data/admission.db \
  "UPDATE remote_api_keys SET expires_at = '2026-12-31 23:59:59' \
   WHERE remote_api_key = '<key>';"
```

**Push is keyless.** `/register` + `/notify` carry **no admission key** — they are authorized entirely by the device→gateway Ed25519 **delegation chain** (the device delegates a gateway push key; C verifies the binding and every notify against it). Abuse is bounded by the per-device rate limit, the bounded device store, and the per-source-IP backstop below, not by an allow-list.

**Push frequency control.** `POST /notify` is rate-limited per **`device_id`** so a buggy or abusive gateway can't hammer APNs or spam a phone: **60 pushes/min sustained, burst 20** per device (override with `PUSH_NOTIFY_RATE_PER_MIN` / `PUSH_NOTIFY_BURST`). Over the limit `/notify` returns `429`.

**Push registration bounds.** The device-token store is bounded: a soft cap (**65 536** bindings, override `PUSH_DEVICE_STORE_CAP`) with eviction of idle, never-`/notify`-confirmed entries (TTL **1 h**, override `PUSH_UNCONFIRMED_TTL_SECS`), so a `/register` flood of valid self-signed chains can't grow it without bound — at the cap with nothing evictable a new `/register` is shed `503`. A `/register` whose `apns_token` exceeds a plausible length (**256 chars**) is refused `400` before it can bloat a stored binding; a bogus-but-bounded token is instead caught at `/notify` by APNs `BadDeviceToken` → prune. The notify-limiter map itself is bounded against an id-churn by a fixed soft cap (**16 384**).

> The `PUSH_*` overrides each fall back to their default when unset / unparseable / non-positive.

**Per-source-IP flood backstop.** Ahead of admission (relay) and body parsing (push), every relay WS-upgrade attempt **and every push `POST /register` / `POST /notify`** is throttled per **client IP** (a token bucket, default **10/s sustained, burst 60**), so a single host spraying requests across many rendezvous/node/device ids is shed with `429` before any upgrade work or signature verification. It is **always on** (no disable switch). (The relay also bounds the broker's pending map with a hard ceiling, `MAX_PENDING_LEGS`, **1024**: a new parked leg past the cap is refused `503` while matching an already-parked leg is exempt.) The **client-IP resolution** is shared across both roles (`CLIENT_IP_HEADERS`, below), but relay and push keep **separate** IP-bucket maps and are sized independently: `RELAY_IP_RATE_PER_SEC` / `RELAY_IP_BURST` / `RELAY_IP_BUCKET_CAP` and `PUSH_IP_RATE_PER_SEC` / `PUSH_IP_BURST` / `PUSH_IP_BUCKET_CAP` (defaults **10**, **60**, **16 384**).

By default the client IP is the **socket peer**, which is correct when remote-host terminates TLS itself. **Behind a proxy (e.g. Cloudflare) the socket peer is the proxy's edge IP, so every client shares one bucket — this would throttle legitimate traffic.** Resolve the real client IP from the proxy's header(s):

```bash
# Resolve the real client IP from the proxy's header(s), tried in order — shared by
# both roles (e.g. behind Cloudflare; first parseable IP wins, else socket peer):
CLIENT_IP_HEADERS=cf-connecting-ip
#     (or, with a fallback:)  CLIENT_IP_HEADERS=cf-connecting-ip,x-forwarded-for
# The limiter has no off switch; to neuter it behind a proxy that already rate-limits,
# set a very high RELAY_IP_RATE_PER_SEC / PUSH_IP_RATE_PER_SEC (and matching burst).
```

> **Trust a client-IP header ONLY when the origin is reachable solely via that proxy** — a [Cloudflare IP allowlist](https://www.cloudflare.com/ips/), a `cloudflared` Tunnel (no public origin), or Authenticated Origin Pulls (CF mTLS). Otherwise a direct-to-origin attacker can forge `cf-connecting-ip` to evade the limit or frame an arbitrary IP. For `x-forwarded-for` the **left-most** entry is taken as the original client, so it too is only safe behind a proxy that overwrites/anchors it.

## Gateway wiring (pair against this host)

The gateway holds **no** `.p8`, and there is **no `relay`/`push` block in `baybo.json`** — relay control + push are driven by the approved device row. Point them at this host by pairing with `--relay-url` (a one-time per-device choice, recorded on the row):

```sh
baybo device pair --relay-url wss://c.example.com --remote-api-key <admitted key>
```

That single WS URL covers both roles: the gateway dials `wss://c.example.com` for the relay control/content legs and POSTs push to `https://c.example.com/notify` (same host, scheme swapped). Omit the flags to use the built-in public proxy + its default key `guest` (an ordinary admitted key on that proxy, not a special admission class). The `remote_api_key` must be admitted in the `remote_api_keys` table (see **Admission** above) for the **relay** legs; the push routes are keyless. To move an already-paired device to a different host, re-pair with the new `--relay-url`.

## Notes

- **Relay-only / `.p8` isolation.** Leave the APNs section of `.env` blank and only the relay runs — no `.p8` on that host. Fill it in to add push. The `.p8` lives solely where you configure it.
- **State.** The admission allow-list is the SQLite table, persisted on the `./data` volume (survives restart). Device-token registrations are in-memory — dropped on restart, but the paired app can register when iOS delivers a token and the gateway re-registers an approved device before its first push attempt when it has non-empty APNs material.
- **APNs environment.** Push targets sandbox vs production **per device registration** (the token's env), so one deployment serves both — no env switch here. A debug-built app registers a sandbox token.
- **Logs** roll daily into `LOG_DIR` (default `/data/logs` → host `./data/logs`, on the `./data` volume so they survive `up --build`), **14 daily files** retained, and are mirrored to stderr so `docker compose logs -f` keeps working (that Docker copy is capped at `10m × 5` files). The hot path is silent by design — no per-message / per-byte / per-connection logging — so only session-level events appear (control connect/disconnect by `relay_node_id`, the per-IP/per-key/parked-leg limit backstops, admission revokes, startup banner, fatal errors). `relay_node_id` is a routing handle, **not** the secret `remote_api_key`. Tune verbosity with `RUST_LOG` (blank → the quiet default `warn,remote_host=info,remote_host_relay=info,remote_host_push=info`): `RUST_LOG=warn` for problems-only, append `remote_host_relay=debug` to trace a gateway.
- **Traffic ledger.** Three **hourly-bucketed** ledgers accumulate into a **separate** SQLite DB on the `./data` volume — `TRAFFIC_DB_PATH` (default `/data/traffic.db` → host `./data/traffic.db`), *not* the admission DB, so the periodic machine writes never contend with the admission poll or pollute the hand-curated allow-list:
  - `relay_traffic` — per `(remote_api_key, server_id, hour)`: up/down bytes + WS frames.
  - `push_traffic` — per `(device_id, hour)`: send count + payload bytes egressed.
  - `ip_traffic` — per `(ip, endpoint, hour)`: request count + bytes (each content leg's *relayed* bytes are attributed to its source IP; the `ip` is the same trusted-header/socket-peer client IP the rate limiter uses).

  The data path stays blind and lock-free (atomic counters only — no per-byte logging); a background task flushes the running totals every `TRAFFIC_FLUSH_SECS` (default 60s) by **adding** each interval's delta onto the current hour's row (so totals survive restart), reclaims idle counters to bound memory, and prunes rows older than `TRAFFIC_RETENTION_DAYS` (default 60). The relay entry cap auto-sizes to the live admission capacity (`2 × Σ max_conns`, recomputed each flush), so a `relay traffic map at its capacity cap` warning means either `relay_node_id` churn or that your admitted keys' `max_conns` sum is too low to cover the active set. Inspect from the host:
  ```bash
  # busiest tenants in the last 24h
  sqlite3 ./data/traffic.db \
    "SELECT remote_api_key, sum(bytes_up) up, sum(bytes_down) down FROM relay_traffic \
     WHERE hour >= datetime('now','-1 day') GROUP BY remote_api_key ORDER BY up+down DESC;"
  # per-IP endpoint calls + bytes in the last 24h
  sqlite3 ./data/traffic.db \
    "SELECT ip, endpoint, sum(requests) reqs, sum(bytes) bytes FROM ip_traffic \
     WHERE hour >= datetime('now','-1 day') GROUP BY ip, endpoint ORDER BY reqs DESC LIMIT 50;"
  sqlite3 ./data/traffic.db \
    "SELECT device_id, sum(sends) sends, sum(bytes) bytes FROM push_traffic GROUP BY device_id ORDER BY bytes DESC;"
  ```
  Set `TRAFFIC_DB_PATH=` (blank) to keep the in-memory accounting but persist nothing. Like the relay itself the ledger is **metadata-only** — counts, routing keys, and source IPs, never message content (`server_id` is the `relay_node_id` routing handle, not the secret `remote_api_key`; the relayed bytes themselves stay Noise ciphertext). The schema is created with `CREATE TABLE IF NOT EXISTS`, so there's no in-place migration: if you're upgrading a box that already has the **pre-hourly** `relay_traffic`/`push_traffic` tables, drop those two tables (or `rm ./data/traffic.db`) once before deploying — the `hour` column is now part of the primary key, which SQLite can't add in place, and the old cumulative rows are disposable.
- **Secrets.** `.env` and `*.p8` are gitignored. The `.p8` is mounted read-only as a Docker secret at `/run/secrets/apns_p8`; it never enters an image layer.
- **Hardening.** Containers run as root so the process can read a `0600` host `.p8`. To run non-root, make the `.p8` readable by that uid and add a `USER` to the Dockerfile.
- **WAL sidecars — what to back up.** Both SQLite DBs run in WAL mode, so each spawns two sidecar files next to the main `.db`: `admission.db-wal`/`admission.db-shm` and `traffic.db-wal`/`traffic.db-shm`. Recent writes live in the `-wal` until a checkpoint folds them back, so a backup that copies only the `.db` mid-write **loses un-checkpointed data**. Back up the `.db` **and** its `-wal`/`-shm` together (or stop the container first, or use `sqlite3 … ".backup"` which checkpoints).

## Operator dashboard

Off by default. Set `DASHBOARD_TOKEN` in `.env` to a strong secret and the server brings up a read + control dashboard on a **separate listener** — `DASHBOARD_BIND_ADDR` (default `0.0.0.0:7778`), **not** fronted by Cloudflare. It is a distinct listener from the relay/push `:443` one; the bare root (`/`) `302`-redirects to `/dashboard`. It serves **plain HTTP** by default, or **HTTPS** when you give it its own cert (see **TLS** below).

```bash
openssl rand -hex 32        # generate a token; paste into DASHBOARD_TOKEN= in .env
docker compose up -d        # restart to pick it up
```

- **Reaching it.** Because the token travels cleartext over plain HTTP, expose the dashboard only on a trusted network or via an SSH tunnel:
  ```bash
  ssh -L 7778:localhost:7778 <host>   # then open http://localhost:7778/
  ```
  Set `DASHBOARD_BIND_ADDR=127.0.0.1:7778` to bind it to the host loopback only (tunnel-required), or leave the default `0.0.0.0:7778` to reach it directly on a trusted LAN.
- **HTTPS (optional).** The dashboard isn't behind Cloudflare, so it gets no cert by default. To serve it over `https://` instead of cleartext, give it **its own** cert — point both `DASHBOARD_TLS_CERT_HOST_PATH` and `DASHBOARD_TLS_KEY_HOST_PATH` (in `.env`) at a PEM cert + key (e.g. a Let's Encrypt cert for the dashboard's hostname, or a self-signed pair for a LAN address) and the listener terminates TLS in-process with rustls. Independent of the relay/push `TLS_CERT`; leave both blank for plain HTTP. The startup log's `dashboard_scheme` field shows `https` once configured (`http` otherwise).
- **Auth.** The token is a bearer credential: the browser stores it in `localStorage` and sends it as an `Authorization: Bearer …` header on every `/dashboard/api/*` call — never a cookie, so CSRF is structurally impossible. **Any non-empty `DASHBOARD_TOKEN` enables the dashboard** (blank disables it); there is **no length requirement** and no rate-limit knobs — the token plus a trusted network are the gate. `openssl rand -hex 32` is still a good way to pick one.
- **What it serves.** Overview stats, the admission allow-list (view / admit / edit / revoke / kick, with one-time key reveal), per-key + per-device + per-IP traffic series from the ledger, and the device registrations — all token-gated; the static HTML/CSS/JS shell is open but leaks nothing.
