# Deploying remote-host (the "C" role) with Docker Compose

`remote-host` is the operator-run **C** role: a single binary serving two roles
on one listener, reached by disjoint route paths.

| Role | When it runs | Routes | What it does |
|------|--------------|--------|--------------|
| **relay** | always on | `/pair/host/{code}`, `/pair/join/{code}`, `/control`, `/content/join/{node}`, `/content/host/{key}` | Blind WebSocket rendezvous for pairing + content (NAT'd gateways). Stateless. |
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

The allow-list of gateway `instance_key`s is a **SQLite table**, not env. The runtime polls it (every `ADMISSION_POLL_SECS`, default 30s), so you add/remove gateways **without a restart**. The DB is bind-mounted (`./data/admission.db` by default), so you edit it from the host:

```bash
# admit a gateway (the instance_key from its baybo.json):
sqlite3 ./data/admission.db \
  "INSERT INTO admitted_instances(instance_key, label) VALUES('<key>', 'my gateway');"
# revoke:
sqlite3 ./data/admission.db "DELETE FROM admitted_instances WHERE instance_key='<key>';"
# list:
sqlite3 ./data/admission.db "SELECT instance_key, label, max_conns, max_bps FROM admitted_instances;"
```

The `admitted_instances` table is created on first start; an empty table admits no one (fail-closed). The same list gates both roles.

**Revoking is enforced on live connections, not just new ones.** On each poll, any key that was dropped from the table has its live relay connections (the gateway's control channel + any in-flight pairing/content legs) closed within the poll interval — so a revoked gateway is disconnected, not left running until it happens to drop. (The push role is per-request, so a revoked key simply gets `401` on its next `/notify`.)

**Per-gateway connection cap.** Each admitted gateway may hold a bounded number of simultaneous relay connections, so a buggy or abusive one can't exhaust C. The limit is the row's `max_conns` column (NULL → the server default `MAX_CONNS_PER_INSTANCE`, **64**), hot-reloaded with the rest of the table. Pairing/content host legs over the cap are refused with `429`; the gateway's one control channel is exempt, so a gateway at its limit can always reconnect control. Raise it per gateway for one serving many concurrent device sessions:

```bash
sqlite3 ./data/admission.db \
  "UPDATE admitted_instances SET max_conns = 200 WHERE instance_key='<key>';"
```

**Per-gateway relay bandwidth.** Content sessions (the bandwidth-heavy traffic) are throttled per gateway: the relay only authenticates the gateway (the phone-side leg is anonymous), so the cap is keyed by `instance_key` and aggregates **both directions across all of that gateway's content legs**. The default rate is a fixed **1 MiB/s**, overridable per gateway by the row's `max_bps` column in bytes/sec (NULL → the default), hot-reloaded with the table. Enforcement is *throttle, not drop* — a gateway over its rate is paced via TCP backpressure, nothing is lost. (Pairing legs are tiny SPAKE2 blobs and are not throttled.) Raise it for a gateway serving many concurrent sessions or large attachments:

```bash
sqlite3 ./data/admission.db \
  "UPDATE admitted_instances SET max_bps = 4194304 WHERE instance_key='<key>';"
```

**Push frequency control.** `POST /notify` is rate-limited per `(instance_key, device_id)` so a buggy or abusive gateway can't hammer APNs or spam a phone. Over the limit it returns `429` (the gateway backs off and retries). The limit is a fixed **60 pushes/min sustained, burst 20** per device; only admitted, registered devices are metered. Hardcoded, not configurable.

## Gateway wiring (`baybo.json`)

The gateway holds **no** `.p8` — it only knows the C base URL + its admission key:

```jsonc
"push":  { "enabled": true, "gateway_url": "https://c.example.com", "instance_key": "<admitted key>" },
"relay": { "enabled": true, "url": "wss://c.example.com",            "instance_key": "<admitted key>" }
```

Both URLs resolve to the one `remote-host` listener (the disjoint paths route to the right role). The `instance_key` must be admitted in the `admitted_instances` table (see **Admission** above) — one key serves both roles.

## Notes

- **Relay-only / `.p8` isolation.** Leave the APNs section of `.env` blank and only the relay runs — no `.p8` on that host. Fill it in to add push. The `.p8` lives solely where you configure it.
- **State.** The admission allow-list is the SQLite table, persisted on the `./data` volume (survives restart). Device-token registrations are in-memory — dropped on restart, but devices re-register on their next pairing/heartbeat and the gateway re-registers an approved device before its first push.
- **APNs environment.** Push targets sandbox vs production **per device registration** (the token's env), so one deployment serves both — no env switch here. A debug-built app registers a sandbox token.
- **Logs** go to stderr (the relay has no `tracing` subscriber wired, so only the `eprintln!` startup/error lines are guaranteed).
- **Secrets.** `.env` and `*.p8` are gitignored. The `.p8` is mounted read-only as a Docker secret at `/run/secrets/apns_p8`; it never enters an image layer.
- **Hardening.** Containers run as root so the process can read a `0600` host `.p8`. To run non-root, make the `.p8` readable by that uid and add a `USER` to the Dockerfile.
