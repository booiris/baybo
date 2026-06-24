# Deploying remote-host (the "C" role) with Docker Compose

`remote-host` is the operator-run **C** role. Two binaries ship as services:

| Service | Binary | Port | Role |
|---------|--------|------|------|
| `push`  | `remote-host-push`  | `8443` | Holds the APNs `.p8`; signs ES256 JWTs and POSTs the blind encrypted preview to Apple. Routes: `POST /notify`, `POST /register`. |
| `relay` | `remote-host-relay` | `8444` | Blind WebSocket rendezvous for pairing + content (NAT'd gateways). Routes: `/pair/host/{code}`, `/pair/join/{code}`, `/control`, `/content/join/{node}`, `/content/host/{key}`. Stateless. |

(`remote-host-dashboard` is a library, not a service — nothing mounts it yet.)

Both serve **plain http/ws** and are designed to sit behind a **TLS terminator**.

## Quick start

```bash
cd remote-host
cp .env.example .env
$EDITOR .env                      # fill APNS_* + *_INSTANCE_KEYS, point APNS_P8_HOST_PATH at your .p8
docker compose up -d --build
docker compose logs -f            # push: "listening on 0.0.0.0:8443 (topic com.baybo.app)"
```

`docker compose` fails fast with a clear message if any required `.env` var is unset.

Cross-arch (e.g. building on an Apple-Silicon Mac for an x86_64 host):
```bash
docker buildx build --platform linux/amd64 -t remote-host:latest --load .
```

## TLS termination

The relay must be reachable by phones as `wss://`, and the gateway reaches push as `https://`. The push and relay route paths are disjoint, so a single domain can front both. Example **Caddy** (auto-HTTPS via Let's Encrypt) — add as a third service and drop in this `Caddyfile`:

```caddyfile
c.example.com {
    reverse_proxy /notify    push:8443
    reverse_proxy /register  push:8443
    reverse_proxy /pair/*    relay:8444
    reverse_proxy /content/* relay:8444
    reverse_proxy /control   relay:8444
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
    depends_on: [push, relay]
volumes:
  caddy_data:
```

With a terminator in front you can drop the `ports:` host-publishes on `push`/`relay` (Caddy reaches them over the compose network) — or keep them bound to `127.0.0.1` only.

If you already run a terminator (e.g. `proxy.baybo.space:7777`), just forward it to the container ports (`:7777 → relay:8444`, and a route to `push:8443`).

## Gateway wiring (`baybo.json`)

The gateway holds **no** `.p8` — it only knows the C base URL + its admission key:

```jsonc
"push":  { "enabled": true, "gateway_url": "https://c.example.com", "instance_key": "<PUSH_INSTANCE_KEYS value>" },
"relay": { "enabled": true, "url": "wss://c.example.com",            "instance_key": "<RELAY_INSTANCE_KEYS value>" }
```

`push.instance_key` / `relay.instance_key` must appear in the corresponding `*_INSTANCE_KEYS` allow-list in `.env`.

## Notes

- **State is in-memory.** A restart drops device-token registrations and admission state; devices re-register on their next pairing/heartbeat, and the gateway re-registers an approved device before its first push. No volumes needed (the only mount is the read-only `.p8`).
- **APNs environment.** Push targets sandbox vs production **per device registration** (the token's env), so one deployment serves both — no env switch here. A debug-built app registers a sandbox token.
- **Logs** go to stderr (the relay has no `tracing` subscriber wired, so only the `eprintln!` startup/error lines are guaranteed).
- **Secrets.** `.env` and `*.p8` are gitignored. The `.p8` is mounted read-only as a Docker secret at `/run/secrets/apns_p8`; it never enters an image layer.
- **Hardening.** Containers run as root so the process can read a `0600` host `.p8`. To run non-root, make the `.p8` readable by that uid and add a `USER` to the Dockerfile.
