# Lark / Feishu Channel Sidecar

The Lark sidecar lives at `channel-src/lark/` and ships embedded in
the gateway binary. It speaks the standard channel WS protocol (see
[`modules/channels.md`](modules/channels.md)) plus the optional
`secrets`, `mcp_tunnel`, `tool_telemetry`, and `diagnose`
capabilities. The full upstream gap analysis is at
[`todo/LARK_CHANNEL_REPORT.md`](todo/LARK_CHANNEL_REPORT.md); this
document covers the architecture that landed, not the design
exploration.

## Bot credentials

A Lark bot is registered with the four-tuple
`(app_id, app_secret, encrypt_key, verification_token)` plus an
optional `base_url`. On the wire:

- `Frame::StartBot.token` carries `app_id` (the user-facing identity)
- `Frame::StartBot.metadata` carries `{ app_secret, encrypt_key,
  verification_token, base_url? }`

`base_url` is a free-form OAPI origin (e.g. `https://open.feishu.cn`,
`https://open.larksuite.com`). The sidecar derives the open-platform
WSS endpoint and the OAuth host by simple host substitution
(`open.X` → `ws.X` for WSS; `open.X` stays for OAuth; `open.X` → `mcp.X`
for the hosted MCP gateway). Keeping `base_url` a string rather than
a `Domain` enum lets new regional Lark deployments and private clouds
ship without an SDK update.

## UAT pipeline (per-bot)

OAPI tools that act on user data require a **user access token (UAT)** —
delegated authorization, distinct from the tenant access token the
bot uses for its own actions. The UAT pipeline is per-bot and
implements RFC 8628 device authorization grant:

```
LarkPlatform.uat: UatPipeline { accessor, scheduler, authFlow, store }
```

- **`UATStore`** (`src/auth/uat-store.ts`) — typed wrapper over the
  SDK's `secrets().scope(botId)` namespace. Keys live under
  `uat/<userOpenId>`; values are JSON-serialised
  `{ accessToken, refreshToken, scope, expiresAt, refreshExpiresAt,
  grantedAt }`. Corrupted / legacy JSON returns `null` rather than
  throwing — guards against DoS via crafted vault content.
- **`AuthFlowController`** (`src/auth/auth-flow.ts`) — the device-flow
  driver. Posts to `accounts.feishu.cn/oauth/v1/device_authorization`,
  renders an in-chat OAuth pending card with the user-friendly URL +
  user code, polls `open.feishu.cn/open-apis/authen/v2/oauth/token`
  on the device-code interval, and verifies the resulting subject
  via `/authen/v1/user_info` to prevent group-chat impersonation
  (Codex review #1). Inflight requests are deduplicated by
  `userOpenId`; concurrent triggers from the same user share one card.
- **`UATAccessor`** (`src/auth/auto-auth.ts`) — invocation gateway.
  Reads the cached UAT via `UATStore`, verifies the granted scope
  covers the tool's required scopes (`grantedCovers`), drops + re-auths
  on insufficient scope (Codex #2). Auto-retries on `99991663` /
  `99991664` / `99991668` / `99991669` (stale UAT) by deleting the
  cached entry and re-running the auth flow once.
- **`UATRefreshScheduler`** (`src/auth/refresh-scheduler.ts`) —
  background sweeper. Runs every minute, refreshes any UAT whose
  `expiresAt` falls within `refreshAheadMs`, takes a per-user mutex
  via `UATMutex` so no two refreshes race (refresh tokens are
  one-time-use under OAuth 2.1). Drops the UAT on terminal
  `expired_token` errors, keeps it on transient `network_error` per
  the `TERMINAL_GRANT_ERRORS` classification (Codex #4 — was the
  source of UATs being wiped on transient Lark hiccups).

Per-tool scope manifest at `src/auth/scopes.ts` mirrors openclaw's
`tool-scopes.ts`. `scopesFor(toolName)` returns the exact scope array;
`grantedCovers(granted, required)` is the coverage check. Tools call
`accessor.invoke({ userOpenId, scopes, fn })` and never touch the
store directly.

## Write-approval framework

Mutating MCP tools route through an in-chat per-call approval card
parallel to the bash `LarkApprovals` pattern:

```
LarkPlatform.approveWrite(req): Promise<"approve" | "deny">
```

`buildWriteApprovalCard` (`src/messaging/write-approval.ts`) emits a
CardKit v2 card tagged `aura: "mcp_write"` (distinct from bash's
`aura: "approval"`), with optional truncated detail and Approve / Deny
buttons. `parseWriteApprovalCardValue` rejects anything that isn't
`mcp_write`-tagged so a bash-approval click can't approve a write.

The handler enforces an **operator filter** identical to bash
approvals: in a group chat, only the user whose message triggered the
write can resolve the card. Other clickers see a toast; the promise
stays pending. `LarkPlatform.stopBot` resolves all pending approvals
as denied so a bot stop never strands a write call.

`feishu_update_doc` is **mode-aware**: destructive modes
(`overwrite`, `replace_all`, `replace_range`, `delete_range`) prompt;
additive modes (`append`, `insert_*`, `update_block_*` with `task_id`
indicating a partial edit) skip the gate. The boundary is in
`src/mcp/server.ts`'s `gateWrite` helper — fail-closed when no
`approveWrite` handler is wired (a misconfigured operator can't
silently bypass the gate).

## MCP tool surface (27 tools)

The sidecar runs an MCP server (one per process) that bundles three
families:

- **Tenant-token reads (5)** — `feishu_get_chat_info`,
  `feishu_list_chat_members`, `feishu_get_chat_history`,
  `feishu_search_chats`, `feishu_get_message`. No UAT required; uses
  the bot's own tenant access token.
- **UAT reads (12)** — `feishu_who_am_i`, `feishu_get_user`,
  `feishu_search_user`, `feishu_calendar` (3 actions),
  `feishu_calendar_event` (2 actions), `feishu_freebusy`,
  `feishu_freebusy_batch`, `feishu_wiki` (2 actions),
  `feishu_search_doc`, `feishu_doc_comments`,
  `feishu_bitable_records`, `feishu_sheet_read_range`,
  `feishu_fetch_doc`.
- **UAT writes with in-chat approval (8)** —
  `feishu_calendar_event_{create,update,delete}`,
  `feishu_bitable_record_{create,update,delete}`,
  `feishu_create_doc`, `feishu_update_doc` (mode-aware).
- **Interactive (1)** — `feishu_ask_user`. Posts an interactive card
  and blocks on the user's tap (reply or button); 10-min self-timeout
  paired with the agent's `SIDECAR_MCP_TIMEOUT = 660s` cap so the
  sidecar's timer fires first and a late reply can never be consumed
  by an orphan waiter (Codex review #1).

Three of the doc tools (`feishu_fetch_doc`, `feishu_create_doc`,
`feishu_update_doc`) proxy to Feishu's hosted MCP gateway at
`mcp.feishu.cn` (or `mcp.larksuite.com` per the `base_url`
substitution). They send JSON-RPC over fetch with the
`X-Lark-MCP-UAT` header; the underlying APIs aren't in OAPI.

The agent identifies the calling user via `_meta.auraSessionId` on
every `tools/call`. The sidecar's MCP server decomposes the tuple via
`composeAuraUserId`'s inverse and hands the tool handler an
`McpToolContext { auraSessionId, channelType, botId, userId }`.

## Workspace skills

Five Lark-aware skills live under `skills/feishu-*/`:

- **`feishu-channel-rules`** — output formatting rules for the Lark
  card body (markdown subset, mention escaping, code-block sizing).
- **`feishu-calendar`** — when to use which calendar tool.
- **`feishu-bitable`** — record CRUD patterns + scope tips.
- **`feishu-docs`** — doc fetch / create / update + comment routing.
- **`feishu-people`** — user lookup + free/busy patterns.
- **`feishu-troubleshoot`** — error-code recovery (auth_failed:
  denied/expired, subject mismatch, 99991663/99991664/230002, etc.).

These were **adapted, not copied** from openclaw's bundle. Openclaw's
skill corpus references several tools we deliberately didn't port
(event search/reply, attendee CRUD, IM read with UAT, task family);
a 1:1 copy would mislead the agent into calling non-existent tools.

## Diagnose

`POST /v1/admin/channels/lark/diagnose` (admin token + bot id) routes
through `Frame::DiagnoseRequest` and returns a 6-check report:

| Check | Verifies |
|---|---|
| `bot_identity` | `LarkChannel.botIdentity` resolved (name + open_id) |
| `transport` | The Lark WSS connection's last heartbeat |
| `config` | Streaming + reaction-echo flags surface as expected |
| `mcp_tools` | The MCP server registered N tools (non-zero proves the harvest hook fired during the first session open) |
| `uat_pipeline` | Authorized user count from `UATStore.listUsers()` (warns when no UAT pipeline is wired) |
| `oauth_endpoint` | HTTPS reachability to `open.X` (separate from the WS host); a 401 still counts as reachable |

## Deferred

- **VC meeting-invited handler** — `@larksuiteoapi/node-sdk`'s
  `LarkChannel.on(...)` only exposes 9 events; VC events flow through
  the lower-level `EventDispatcher` which the wrapper keeps private.
  Re-evaluate when the SDK exposes them or a real workflow surfaces.
- **Bulk OAPI tool ports** — calendar attendee CRUD, bitable
  app/table/field/view CRUD, drive file/doc-media,
  sheets writes, task family, IM read with UAT. Each item documents
  why it wasn't worth it for v0; pull in by demand.
- **`bin/lark-diagnose` standalone CLI** — admin endpoint covers the
  operational case.
- **`Frame::AbortSession`** — wire frame exists but no Lark-specific
  consumer landed. Defer until a real abort UX surfaces.
