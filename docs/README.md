# Baybo documentation

Start with [Architecture](architecture.md) for the system overview, then use the
[module index](modules/README.md) for the domain you are changing. Contributor
rules and build commands live in [CLAUDE.md](../CLAUDE.md).

## Where documentation belongs

| Location | Purpose |
| --- | --- |
| `docs/modules/` | Module responsibilities, contracts, constraints and implementation design. Read the relevant spec before changing a crate. |
| `docs/modules/mobile/` | Contracts spanning the gateway, relay and iOS app: pairing, transport, security and blobs. |
| `docs/*.md` | Cross-module behavior, development guides, operations and design rationale. |
| `app/ios/docs/` | Native iOS implementation, UI behavior, build and device testing. |
| `docs/todo/` | Proposals, unresolved questions and partially implemented designs; check each document's status. |
| `docs/openapi.json` | Generated API contract. Update through the gateway's OpenAPI synchronization test. |
| `docs/assets/` | Documentation diagrams and their generation sources. |

Keep each rule in its owning document and link to it from other guides. A
proposal's location or an implementation branch does not establish that a
feature has shipped. [Roadmap](roadmap.md) indexes planned work; individual
specs carry their detailed status and unresolved decisions.

## Runtime and conversation behavior

- [Architecture](architecture.md) and [module index](modules/README.md).
- [Chat sync protocol](sync-protocol.md) and its [terminology](CONTEXT.md).
- [Turn progress events](turn-progress-events.md).
- [Background-job notifications](background-notifications.md).
- [Cron groups](cron-groups.md).
- [Transcript search](search.md).
- [Mid-turn user interjection](mid-turn-user-interjection.md) — design rationale.
- [External agents](external-agents.md).

## Configuration, security and integrations

- [Config hot reload](config-hot-reload.md).
- [Permission policy](permission.md).
- [Secret management](secret-management.md) — cross-module design rationale.
- [Embedded sidecars](sidecars.md).
- [External command dependencies](external-commands.md).

## Clients and mobile connections

- [Web dashboard](webui.md) and [web chat](web-chat.md).
- [iOS guide and documentation index](../app/ios/CLAUDE.md#docs).
- [Mobile companion overview](modules/mobile/companion.md).
- [Direct carriers](modules/mobile/direct-carriers.md) — LAN, IPv6/IPv4,
  traversal, relay fallback and protocol invariants.
- [Pairing security](modules/mobile/pairing-security.md) and
  [relay/push security](modules/mobile/relay-push-security.md).
- [Relay API tunnel](modules/mobile/relay-api-tunnel.md) and
  [blob transfers](modules/mobile/blob-transfer.md).
- [iOS connection lifecycle and diagnostics](../app/ios/docs/connection.md).

## Development, testing and releases

- [Testing guide](testing.md) — Rust test layout, shared fixtures and runners.
- [Direct-carrier NAT matrix](testing.md#direct-carrier-nat-matrix) — Linux
  prerequisites, network isolation, cleanup limitations and CI behavior.
- [iOS tests and device checklist](../app/ios/docs/testing.md).
- [Web unit tests](web-unit-tests.md).
- [Security fuzzing](fuzzing.md).
- [Benchmark results viewer](bench-web.md).
- [CLI releases](releasing.md) and [iOS build/release workflow](../app/ios/docs/build.md).
- [OpenAPI contract](openapi.json).

## Planned work

[Roadmap](roadmap.md) is the entry point for the proposals and remaining work in
[todo/](todo/). Module specs describe implemented behavior; proposal documents
may also retain rationale for shipped portions and explicitly deferred work.
