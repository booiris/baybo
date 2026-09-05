# Cross-crate contract and rule duplication

> **Status:** not started. Recorded 2026-08-28 after the provider-neutral mobile
> push refactor exposed several similarly named push types across the iOS app,
> gateway, and remote-host workspaces.

This is an audit backlog, not a request to merge every repeated declaration.
Baybo intentionally mirrors types at serialization and dependency boundaries;
the work is to distinguish those mirrors from duplicated rules and misleading
names, then establish one source of truth where doing so preserves the boundary.

## Survey snapshot

The initial read-only Rust survey covered the main workspace, `app/mobile/ffi`, and
`remote-host/crates`. Test directories and obvious test crates were excluded;
the numbers are mechanical signals, not defect counts.

| Surface | Mechanical result | Reviewed result |
|---|---:|---|
| visible type names | 50 cross-crate groups, excluding `Result` | 33 intentional boundary mirrors, 15 unrelated same-name types, 2 genuinely duplicated concepts |
| all type names, including private types | 95 cross-crate groups | useful only for finding naming collisions such as the private gateway `PushTarget` |
| constant names | 46 cross-crate groups | about 11 contract or policy families merit review |
| repeated string-valued constants | 30 cross-crate groups | most are standard labels or unrelated defaults; protocol headers and hosted endpoints are the important subset |
| visible function names | 161 cross-crate groups | dominated by conventional names such as `new`, `get`, and `run` |
| structurally repeated function bodies | 31 groups at the audit threshold | 12 production clone families remained after removing tests, trait boilerplate, and obvious coincidences; another 7 families implement the same rule differently |

Do not add these counts together: a single concern can contain a type, constants,
and functions, and therefore appears in several rows.

## The motivating push case

There are four push-target-shaped representations, but only three describe a
platform provider token:

- `remote-host/crates/protocol/src/push.rs::PushTarget` is the canonical JSON and
  signing-contract type (`Apns { token, environment } | Fcm { token }`).
- `app/mobile/ffi/src/api.rs::PushToken` is the local UniFFI-facing mirror. A local
  type is required at the Swift ABI boundary.
- `crates/gateway/src/api/admin/push.rs::PushTargetRequest` is the OpenAPI request
  mirror and converts into the protocol type.
- `crates/gateway/src/push/mod.rs::PushTarget` is not a provider target at all. It
  is a dispatch destination containing `device_id` and `push_url`. Rename it to
  `PushDestination` or `PushRoute`; do not merge it with the protocol enum.

The provider-token mirrors may remain, but their normalization rule must not
drift. Trimming, non-empty validation, and the `PUSH_TOKEN_MAX_LEN` check currently
exist independently on the iOS, gateway, and remote-host paths.

## Boundaries that must remain explicit

- `crates/gateway/src/api/dto.rs` deliberately mirrors domain types so `utoipa`
  does not leak into domain crates and HTTP v1 changes stay explicit. It contains
  53 public DTO types. Keep the DTOs and their `From` conversions; use schema and
  conversion tests to detect drift.
- `app/mobile/ffi/src/api.rs` contains 50 public UniFFI records/enums, and
  `app/mobile/ffi/src/gateway_api.rs` contains 31 private `Wire*` types. These bridge
  JSON and Swift ABI constraints. Do not replace them with external Rust types
  merely to reduce the declaration count.
- `remote-host` is a separate workspace by design. Sharing a small contract
  through `remote-host-protocol` is acceptable; making it depend on a `baybo-*`
  domain crate is not.
- Do not create a generic `utils` or `common` crate. A shared item needs a domain
  owner or an existing protocol boundary.

## High-value candidates

### Protocol and security rules

- Give provider-token normalization one implementation or typed constructor in
  `remote-host-protocol`, while keeping UniFFI/OpenAPI adapters local.
- Review the duplicated device-id layout constants (`DEVICE_ID_PREFIX`, public-key
  length, signature length) and parsing/verification helpers in `device-proto`
  and `remote-host-push`. Preserve the signer/verifier separation and the pinned
  cross-workspace vectors even if constants or parsing move into the protocol
  crate.
- Put the hosted relay default (`wss://proxy.baybo.space`) behind one main-workspace
  contract consumed by CLI, gateway, and iOS where the dependency graph permits.
- Give the mobile blob contract one home: the 100 MiB cap, the content-SHA header,
  the deck-card header, and SHA-256 hex validation currently have client/server
  mirrors. Account for the additional Swift attachment-size mirror.
- Move `x-baybo-channel-token` to a dependency-neutral wire owner rather than
  mirroring it in gateway and TUI.
- Give `MAX_LINEAGE_WALK_HOPS` one owner; gateway authorization and subagent spawn
  currently repeat the same value and invariant.

### Repeated parsers and policy helpers

- Unify the shared Markdown-frontmatter machinery in `baybo-skills` and
  `baybo-subagent`: `split_frontmatter`, `unquote`, line-ending normalization,
  and most of the YAML-subset parser currently evolve in parallel.
- Export and reuse the config crate's dotted-path-to-JSON-pointer conversion
  instead of keeping an exact copy in CLI.
- Centralize the vault-then-environment credential lookup policy shared by LLM,
  memory, and search without handing those consumers a general-purpose store.
- Decide one owner for the identical channel-visibility predicate represented by
  `tools::ToolManifest::allows_channel` and `skills::SkillSummary::allows_channel`.
- Consider a shared `ContentBlock` text-extraction helper and XML-escaping helper;
  both currently have exact cross-crate copies.

### Operational plumbing

- Review the CLI/setup TTY stack (`RawModeGuard`, prompt helpers, masked input,
  channel enumeration, and OAuth device-code rendering). Some copies already
  differ subtly, so behavior must be specified before deduplication.
- Extract only the genuinely identical seam from the edge, relay, and push
  traffic registries: their two-phase `collect -> durable write -> commit`
  lifecycle is the shared invariant, while keys, counters, and concurrency models
  legitimately differ.
- Review the repeated Bun executable override, restart-backoff policy, and
  supervisor plumbing in deck, gateway sidecars, and setup. Do not collapse the
  domain-specific crash/quarantine behavior.
- Give Baybo-authored Git commits one identity source instead of repeating
  `Baybo <baybo@local>` in workspace, tools, and deck.
- Keep dependency-driven copies such as POSIX shell quoting only when moving them
  would create the wrong crate edge; document and test the equivalence when a
  copy remains.

## Suggested order

1. Fix names that lie about meaning, beginning with the gateway's private
   `PushTarget`; this is low-risk and does not alter a wire format.
2. Consolidate push validation and push/blob protocol constants while the Android
   provider work is still pre-release.
3. Move the exact parser and config-path clones to their existing domain owners.
4. Address operational-policy families only after writing the invariant each
   implementation must share; avoid false deduplication behind a large `match`.
5. Add a lightweight audit check or checked-in baseline only after the existing
   cases are classified. A raw duplicate-name lint would create mostly noise.

For every item, the acceptance test is not fewer declarations. It is that one
place owns the rule, boundary adapters remain explicit, and drift is caught by a
conversion, schema, protocol-vector, or cross-workspace test.
