# Deploying remote-host (the "C" role) with Docker Compose

`remote-host` is the operator-run **C** role: a single binary serving two roles
on one listener, each enabled by an env flag and reached by disjoint route paths.

| Role | Enable | Routes | What it does |
|------|--------|--------|--------------|
| **push** | `PUSH_ENABLE=1` | `POST /notify`, `POST /register` | Holds the APNs `.p8`; signs ES256 JWTs and POSTs the blind encrypted preview to Apple. |
| **relay** | `RELAY_ENABLE=1` | `/pair/host/{code}`, `/pair/join/{code}`, `/control`, `/content/join/{node}`, `/content/host/{key}` | Blind WebSocket rendezvous for pairing + content (NAT'd gateways). Stateless. |

Enable both to share one port, or just one for an isolated deployment (e.g. the
`.p8`-holding push role on its own host). It serves **plain http/ws** by default;
add the TLS overlay for direct `wss/https`. (`remote-host-dashboard` is a library,
not a service.)

## Quick start

```bash
cd remote-host
cp .env.example .env
$EDITOR .env                      # fill APNS_* + *_INSTANCE_KEYS, point APNS_P8_HOST_PATH at your .p8
docker compose up -d --build
docker compose logs -f            # "remote-host: listening on 0.0.0.0:8443 (http/ws) — roles: push + relay"
```

`docker compose` fails fast with a clear message if any required `.env` var is unset.

Cross-arch (e.g. building on an Apple-Silicon Mac for an x86_64 host):
```bash
docker buildx build --platform linux/amd64 -t remote-host:latest --load .
```

## TLS

Phones reach the relay as `wss://` and the gateway reaches push as `https://` —
both on the **same** host:port now (one listener). Two ways to terminate TLS:

### Option A — direct TLS in-process (no proxy)

The binary terminates TLS itself with rustls. Provide a PEM cert + key and run
with the TLS overlay:

```bash
# in .env:
#   TLS_CERT_HOST_PATH=/etc/letsencrypt/live/c.example.com/fullchain.pem
#   TLS_KEY_HOST_PATH=/etc/letsencrypt/live/c.example.com/privkey.pem
#   PORT=443        # optional — for port-less wss://host / https://host URLs
docker compose -f docker-compose.yml -f docker-compose.tls.yml up -d --build
```

The startup log then shows `https/wss`, and one cert covers both roles. TLS is
on whenever `TLS_CERT` + `TLS_KEY` are both set (the overlay does this). Renew
the cert out of band (e.g. certbot) and `docker compose restart`.

### Option B — front with a TLS terminator

Leave TLS off (plaintext) and put Caddy / nginx / a cloud LB / Cloudflare in
front. With one listener you don't need path-routing — just proxy the whole host:

```caddyfile
c.example.com {
    reverse_proxy remote-host:8443
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

## Gateway wiring (`baybo.json`)

The gateway holds **no** `.p8` — it only knows the C base URL + its admission key:

```jsonc
"push":  { "enabled": true, "gateway_url": "https://c.example.com", "instance_key": "<PUSH_INSTANCE_KEYS value>" },
"relay": { "enabled": true, "url": "wss://c.example.com",            "instance_key": "<RELAY_INSTANCE_KEYS value>" }
```

Both URLs resolve to the one `remote-host` listener (the disjoint paths route to the right role). `push.instance_key` / `relay.instance_key` must appear in the corresponding `*_INSTANCE_KEYS` allow-list in `.env`.

## Notes

- **Isolation.** To keep the `.p8`-holding push role off the relay host, run two instances: one with `RELAY_ENABLE=1` (no APNs env) and one — on a separate host — with `PUSH_ENABLE=1`. The unified binary with a single flag is exactly the old standalone role.
- **State is in-memory.** A restart drops device-token registrations and admission state; devices re-register on their next pairing/heartbeat, and the gateway re-registers an approved device before its first push. No volumes needed (the only mounts are the read-only `.p8` and, with the TLS overlay, the cert/key).
- **APNs environment.** Push targets sandbox vs production **per device registration** (the token's env), so one deployment serves both — no env switch here. A debug-built app registers a sandbox token.
- **Logs** go to stderr (the relay has no `tracing` subscriber wired, so only the `eprintln!` startup/error lines are guaranteed).
- **Secrets.** `.env` and `*.p8` are gitignored. The `.p8` is mounted read-only as a Docker secret at `/run/secrets/apns_p8`; it never enters an image layer.
- **Hardening.** Containers run as root so the process can read a `0600` host `.p8`. To run non-root, make the `.p8` readable by that uid and add a `USER` to the Dockerfile.
