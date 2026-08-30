# Baybo with Docker Compose

This deployment builds one self-contained Baybo image and exposes the embedded
HTTP/WebSocket gateway directly. Caddy is not required unless a separate
deployment needs a public domain or TLS termination.

The runtime image includes Git, `sh`/Bash, ripgrep, Node.js, Bun, uv/uvx, and
the shared libraries needed by Chrome for Testing. The Web UI and all in-tree
sidecars are compiled into the Baybo binary during the image build.

## Start

```bash
cd deploy/docker
cp .env.example .env
# Edit .env and set BAYBO_LLM_API_KEY.
docker compose up --detach --build
docker compose ps
```

Open <http://localhost:8888>. To print the bearer token used by the admin API:

```bash
docker compose exec baybo baybo gateway token show
```

Use `BAYBO_HTTP_PORT` in `.env` when host port 8888 is already occupied. The
container still listens on 8888 internally.

## First-boot configuration

On the first boot, the entrypoint generates:

- `/var/lib/baybo/.key/encryption.key` with mode `0600`;
- `/var/lib/baybo/config/baybo.json`, bound to `0.0.0.0:8888`;
- one LLM entry based on `.env`, with `reasoning_effort` defaulting to
  `medium`.

These files are never overwritten on later starts. After the first boot, use
Baybo's `config set` command for individual values. For a full-file edit, copy
the config out and stream the edited JSON back through the non-root Baybo user:

```bash
docker compose cp baybo:/var/lib/baybo/config/baybo.json ./baybo.json
# Edit baybo.json.
docker compose exec -T baybo sh -c \
  'umask 077; jq . >"$BAYBO_CONFIG_PATH.tmp" && mv "$BAYBO_CONFIG_PATH.tmp" "$BAYBO_CONFIG_PATH"' \
  < ./baybo.json
docker compose restart baybo
```

`BAYBO_LLM_API_KEY` remains an environment-variable reference; the literal
secret is not written into `baybo.json`.

## Persistence and backup

All workspace state, including sessions and transcripts, is stored in the
named volume `baybo-data`. Downloaded browser and package caches use the
replaceable `baybo-cache` volume. A normal `docker compose down` keeps both
volumes. Never use `docker compose down --volumes` for normal lifecycle
operations: it deletes the session database and transcripts from `baybo-data`.

Back up the data volume while Baybo is stopped:

```bash
docker compose stop baybo
docker run --rm \
  --volume baybo-data:/data:ro \
  --volume "$PWD":/backup \
  alpine:3.22 tar -C /data -czf /backup/baybo-data.tgz .
docker compose start baybo
```

## Browser and sandbox notes

Set `BAYBO_BROWSER_ENABLE=true` before the first boot to enable browser tools.
Chrome for Testing is downloaded into `baybo-cache` on first use. Chrome runs
with its renderer sandbox disabled because the outer container is the isolation
boundary; the Compose service also drops Linux capabilities and enables
`no-new-privileges`.

Baybo detects that it is already inside a container, so it does not try to
start a second bwrap/Docker sandbox for shell commands. The Docker socket is
deliberately not mounted. If a workflow genuinely needs sibling containers,
add that access explicitly after reviewing the security implications.

Keep `browser.docker.enable` set to `false` in this deployment. Setting it to
`true` without a Docker CLI/socket only logs a warning and falls back to Chrome
inside the Baybo container. Mounting the host Docker socket would additionally
require explicit profile-volume and CDP-network wiring; a separately managed
browser service with `browser.docker.cdp_url` is the safer shape.

For a public deployment, put an existing reverse proxy in front of port 8888
and configure HTTPS there. The gateway serves both ordinary HTTP and WebSocket
traffic on the same port.
