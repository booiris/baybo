# Lark / Feishu Channel for Aura — Gap Analysis vs `openclaw-lark`

> Reference upstream: `origin_channel_plugin/openclaw-lark/` (`@larksuite/openclaw-lark` 2026.4.1, ~192 source files, ~22 message-converter types, ~28 OAPI tools, 9 bundled skills).
> Reference local SDK: `sdks/channel-ts/` + `crates/gateway/src/{channel,sidecar}/` + existing `channel-src/{telegram,weixin}` sidecars.

This report compares what `openclaw-lark` does inside the OpenClaw plugin host with what an Aura sidecar can express today. The intent is to scope a Lark sidecar that lives under `channel-src/lark/` and to flag where Aura's runtime needs an extension before parity is even possible.

The two hosts are very different. OpenClaw is an in-process plugin model: a Lark plugin can register tools, commands, hooks, agentPrompt fragments, channel capabilities, multi-account configs, and rich card streaming controllers, all running in the host's address space. Aura's host is a Rust gateway and sidecars are out-of-process JS bundles spoken to over a typed WebSocket frame protocol whose surface is deliberately minimal: text in, text/notice/approval/slash-manifest out, plus a blob side-channel (`crates/gateway/src/channel/blobs.rs:1-60`). Most of openclaw-lark's value lives in places Aura's sidecar contract simply doesn't expose.

## 1. TL;DR

Three shippable phases. Some gateway/SDK extensions land alongside Phase 1 because the user has explicitly OK'd extending Aura's wire surface where it pays off across channels.

| Phase | Scope | Effort |
|---|---|---|
| **MVP** | `StartBot.metadata` wire field + SDK secret-vault helpers (gateway+SDK extensions, generic to all sidecars). Then: one bot, app-token only, text in/out, image+file attachments, typing indicator, approval inline-card, `/new` echoed via SlashManifest | ~1.5 weeks |
| **Pretty** | Streaming card replies (CardKit v2), reaction echo, mention/@-all parsing, group policy gate, message dedup, multi-account via `StartBot`/`StopBot` | ~2-3 weeks |
| **Power-user parity** | Skills bundle (doc/wiki/bitable/calendar/task/IM read), OAuth UAT device flow over the SDK secret API, ask-user-question card, comment-thread routing, VC-meeting-invite handling | ~1-2 months and 2-3 gateway PRs |

**Guiding principles** (from explicit user direction):
1. **SDK-first.** If a capability could be useful to a second channel, it goes into `sdks/channel-ts/` and not into the Lark sidecar. The Lark sidecar consumes; it doesn't carry generic plumbing.
2. **Extending the gateway is fair game** when the SDK needs a primitive Aura doesn't currently expose (e.g. secret storage, structured StartBot metadata, tool-use telemetry). Wire changes are additive so older sidecars keep working.

Each item below is annotated with **sidecar-only**, **SDK extension**, or **gateway change**.

## 2. Aura's sidecar contract — what's actually available

Read these together: `sdks/channel-ts/src/channel.ts`, `sdks/channel-ts/src/bot.ts`, `sdks/channel-ts/src/blobs.ts`, `sdks/channel-ts/src/generated/Frame.ts`.

Server → sidecar frames (`generated/Frame.ts:6`):
- `register_ack`
- `message` (the agent's reply, optionally with `attachments`)
- `delta` (incremental text — emitted only for streaming-aware sidecars)
- `notice` (warn/error banner)
- `approval_requested`
- `approval_resolved`
- `start_bot` / `stop_bot` (per-tenant credential push)
- `slash_manifest` (gateway-authoritative slash command list)

Sidecar → server frames:
- `register`
- `message` (one inbound user turn, optionally with `attachments` + `platform_msg_id`)
- `resolve_approval`
- `bot_status`
- `sidecar_log`

HTTP side-channel for media: `POST /v1/blobs` and `GET /v1/blobs/<id>` (auth via `AURA_CHANNEL_TOKEN`, originator gated by `x-aura-bot-id` + `x-aura-user-id` headers — see `crates/gateway/src/channel/blobs.rs:30-78`).

`BotChannel` (`sdks/channel-ts/src/bot.ts:430`) already gives any sidecar:
- `(channelType, botId, chatKey, platformUserId)` user-id composition (`sdks/channel-ts/src/bot.ts:362-371`)
- StartBot/StopBot lifecycle, idempotency, generation guards
- inbound queue + back-pressure
- typing indicators (refresh + safety timer)
- approval routing into a `PlatformApprovals` adapter
- `setMyCommands`-style slash registration (Lark *does* support this — `im.v1.bot.command.update`)
- text + media outbound dispatch via `BotPlatform.sendText`/`sendMedia`/`sendNotice`

Aura's gateway is the single source of truth for slash commands (`crates/gateway/src/channel/slash.rs:30` — currently only `/new` is recognised). A sidecar can publish more to the platform's own command UI, but the gateway dispatcher won't recognise them.

`ChannelType` is open-ended (`crates/model/src/session.rs:25`) — no enum extension needed to add `"lark"`. The supervisor enumerates `channel-src/*` automatically (`crates/gateway/build.rs:368-486`); creating the sidecar directory and adding it to `pnpm-workspace.yaml` is enough for it to ride into the gateway binary.

## 3. Feature-by-feature comparison

### 3.1 Channel registration & manifest

| Capability | openclaw-lark | Aura today | Verdict |
|---|---|---|---|
| Plugin manifest (id, channels, configSchema) | `openclaw.plugin.json`, `index.ts:104-111` | not modelled — sidecar self-identifies via `Register` frame `channel_type="lark"` | **No match needed.** Drop the plugin manifest. |
| `capabilities = { chatTypes, media, reactions, threads, polls, nativeCommands, blockStreaming }` | `src/channel/plugin.ts:118-126` | not modelled at all | Aura assumes text + optional media + optional approvals. **No gateway concept of capabilities;** any extra surface is an internal sidecar detail. |
| `agentPrompt.messageToolHints` (channel-specific guidance for the agent) | `src/channel/plugin.ts:132-139` | **Missing.** Aura has no per-channel agent-prompt injection. | Add a server-side hook in `crates/agent/` if we want the agent to know "Feishu reactions use UPPERCASE names". Otherwise prompt at the application level. |
| `groups.resolveToolPolicy` (per-group tool allow/deny) | `src/channel/plugin.ts:146` | Aura has tool gating in `crates/tools/src/approval.rs` but not group-scoped. | Sidecar can self-gate by passing/dropping inbounds, but tool policy proper is gateway-side. |
| `reload.configPrefixes` (hot reload on config change) | `src/channel/plugin.ts:153` | sidecars reload by being killed and respawned by the supervisor (`crates/gateway/src/sidecar/supervisor.rs:1-12`). | Acceptable; live reload not needed. |

### 3.2 Channel lifecycle (start/stop/health)

| Capability | openclaw-lark | Aura | Verdict |
|---|---|---|---|
| Multi-account: `listAccountIds`, `resolveAccount`, `defaultAccountId`, `setAccountEnabled`, `deleteAccount` | `src/channel/plugin.ts:168-201` | Aura's per-tenant credential model is `StartBot`/`StopBot` (`Frame::start_bot`, `BotChannel.onStartBot`). The bot store (`aura_storage::ChannelBotStore`) drives the supervisor's reconciler (`crates/gateway/src/channel/bot_reconciler.rs`). | **Map 1:1.** An openclaw "account" ≈ an Aura "bot". Use `botId = app_id` and let the gateway's reconciler hand us `StartBot { botId, token, metadata }`. **Decided:** add a `metadata: Map<string,string>` field to `Frame::StartBot` so we don't pack `app_secret`/`encrypt_key`/`verification_token` into the `token` string (gateway + SDK change, additive — see §4.0). |
| Account "probe" with TTL cache | `src/channel/probe.ts`, `src/channel/plugin.ts:295-312` | not modelled. `BotStatusReport` is the only liveness signal a sidecar can emit. | Sidecar implements probes internally and reports failures via `bot_status.message`. |
| Pairing / onboarding ("notifyApproval triggers onboarding") | `src/channel/plugin.ts:89-111`, `src/channel/onboarding.ts`, `src/tools/onboarding-auth.ts` | Aura has its own pairing flow (`aura_pairing` crate, `BlobPairingRequiredError` in `sdks/channel-ts/src/blobs.ts:46-53`). | Different model — keep Aura's gate. **But the default `BotChannel` notice rendering (`bot.ts:566` — `⚠️ <text>`) is too thin for a SaaS user who can't reach the host CLI.** Lark sidecar must intercept the first `Frame::Notice` carrying a pairing code and render it as a CardKit v2 onboarding card with the code, an "ask your admin" instruction, and a copy-to-clipboard button. See Phase 1 work item §4.1.A. |
| Long-poll/WebSocket monitor | `src/channel/monitor.ts` (Feishu open-platform WSS via `@larksuiteoapi/node-sdk`'s `WSClient`) | sidecar owns its own transport — there's no Aura equivalent because each sidecar speaks its own platform. | Sidecar reuses `@larksuiteoapi/node-sdk` 1:1. |
| Abort detection (stop streaming card on user "stop"/"abort"/"取消" message) | `src/channel/abort-detect.ts`, `src/channel/event-handlers.ts:95-113` | **Not modelled.** No abort frame exists on the wire. | **Decided:** add `Frame::AbortSession { session_id, reason }` (sidecar → gateway). Sidecar's heuristic detector (port `abort-detect.ts`) sends the frame; gateway's session manager cancels the in-flight agent turn. Phase 3, see §4.0.4. |
| Per-chat serial queue (one in-flight turn per `chat:thread`) | `src/channel/chat-queue.ts` | Aura serialises per-session at the gateway via `aura_session::SessionManager`. | Can reuse Aura's session serialisation; just need to compose `sessionId` deterministically from `(botId, chat, thread)`. |

### 3.3 Inbound message processing

The openclaw inbound pipeline runs ~21 files in `src/messaging/inbound/`. The Aura sidecar contract collapses all of this into "yield one `UserInbound` per platform event" (`channel.ts:197`). Most of the work disappears, but a few features matter:

| Capability | openclaw-lark | Aura | Verdict |
|---|---|---|---|
| Message dedup by `(account, msg_id)` | `src/messaging/inbound/dedup.ts`, `src/channel/event-handlers.ts:84-93` | `Message.platform_msg_id` is forwarded by the sidecar and the gateway dedups per `(channel_type, bot_id, platform_msg_id)` (`crates/gateway/src/channel/dedup.rs`). | **Free.** Just populate `platformMsgId = msg_id`. |
| Reaction dedup + emoji-to-name | `src/messaging/inbound/reaction-handler.ts` + `src/messaging/inbound/dedup.ts` | reactions don't have a wire frame at all. | **Sidecar-local concern only.** A reaction can be relayed as text content (e.g. `"[reaction] +OK"`) or dropped. No agent visibility. |
| Mention parsing (`@bot`, `@user`, `@all`) | `src/messaging/inbound/mention.ts` (~7 helpers exported from `index.ts:76-86`) | the sidecar passes `content: string` — Aura doesn't model mentions. | **Sidecar-local.** Strip the `@bot` mention before forwarding so the agent sees clean text. The bot's own `@` is the trigger; ignore other mentions or render as `@username` text. |
| Comment-thread context (Drive/Wiki doc comments → reply context) | `src/messaging/inbound/comment-context.ts`, `comment-handler.ts`, `src/core/comment-target.ts` | not modelled | **Phase 3 sidecar feature.** The sidecar treats a comment event as an inbound and on `onMessage` it routes back via Drive comments API. No gateway change. |
| VC meeting-invited handler (synthetic notification of a meeting starting) | `src/messaging/inbound/vc-meeting-invited-handler.ts`, `src/core/synthetic-target.ts`, tests `vc-invited-event-dedup.test.ts` + `vc-meeting-invited-handler.test.ts` + `vc-synthetic-notify.test.ts` | not modelled | **Sidecar-local.** Convert the event to an inbound with a synthetic `userId`. Outbound on synthetic targets is dropped (mirroring openclaw's `outbound.ts:175-178`). |
| Group policy gate (allowlist groups/senders, mention-required) | `src/messaging/inbound/gate.ts`, `policy.ts` | Aura's `aura_pairing` does pairing-style gating but not group allowlists. | **Sidecar-local.** Filter in `inbound()` before yielding. Config goes in the sidecar's own bot config blob. |
| Permission-error → user-facing card | `src/messaging/inbound/permission.ts`, `src/messaging/inbound/dispatch-commands.ts:25-80` | `Frame::Notice` is the closest analogue. | Use `notice` for now; no card. |

### 3.4 Outbound sending

| Capability | openclaw-lark | Aura sidecar contract | Verdict |
|---|---|---|---|
| `sendText`, `sendCard`, `sendMedia` (image/file/audio), `editMessage`, `forwardMessage` | `src/messaging/outbound/{send,deliver,media,forward}.ts` | `BotPlatform.sendText`, `sendMedia`, `sendNotice` (`bot.ts:158-186`). No `editMessage`/`forward`/`reaction` hooks. | **Editing & forwarding don't exist on the wire.** The sidecar can choose to render every `onMessage` as a fresh card (no edit) or as an edit of a single streaming card (sidecar's own bookkeeping). |
| Reactions: add/remove/list | `src/messaging/outbound/reactions.ts`, `FeishuEmoji`, `VALID_FEISHU_EMOJI_TYPES` (`index.ts:60-66`) | not modelled | **No wire.** Sidecar can emit reactions only as side effects of inbound events (e.g. ack with 👍 on receipt) — agent can't "react" on its own without a wire-level reaction frame. |
| Chat management (`updateChat`, `add/removeMembers`, `listMembers`) | `src/messaging/outbound/chat-manage.ts` | not modelled | Either expose as MCP-style tools (server-side, see §3.7) or skip. |
| `channelData.feishu.card` v1/v2 card payloads | `src/messaging/outbound/outbound.ts:84-106` | `AgentMessage.content` is plain string + `attachments`. **No structured card payload on the wire.** | The sidecar must render text → CardKit JSON locally. We lose the agent's ability to dictate card structure unless we add a `channelData` field to `Message` (gateway change). |
| Typing indicator | `src/messaging/outbound/typing.ts` | `BotPlatform.notifyTyping`, refresh + safety timer in `BotChannel` (`bot.ts:783-841`). | **Free** — implement `notifyTyping` against `im.v1.message.create({ msg_type: "system" })` or the typing API. |

### 3.5 Card rendering & streaming

This is the single biggest gap and the area where openclaw is most opinionated:

| Capability | openclaw-lark | Aura | Verdict |
|---|---|---|---|
| Streaming card lifecycle (`idle → creating → streaming → completed/aborted/terminated`) | `src/card/streaming-card-controller.ts:7-14` (1189 LOC) | `Channel.onDelta` (`channel.ts:139`) feeds incremental text; the gateway emits `Frame::Delta` for streaming-aware channels. | **Wire support exists, rendering does not.** The Lark sidecar must own the entire CardKit v2 update loop (`im.v1.message.patch`/CardKit `cards.update`), throttling, and unavailability detection. ~1200 LOC of port. Worth doing for visible "agent is thinking" UX. |
| Flush-throttle controller (rate-limit-aware) | `src/card/flush-controller.ts` (140 LOC) | none | **Sidecar-local.** Direct port. |
| Markdown → Feishu-flavoured card builder | `src/card/builder.ts`, `src/card/markdown-style.ts`, `src/card/cardkit.ts` | none | **Sidecar-local — port both v1 (Message Card) and v2 (CardKit).** Decided: support both schemas. v1 stays needed for older bots and for cards the agent might emit referencing v1-only elements (`action`, `button_group`, `note`, `div+lark_md`); v2 is the streaming path. The branching is self-contained inside the builder. |
| Tool-use display (per-tool config, full-paths/details toggles, error rendering) | `src/card/tool-use-display.ts`, `tool-use-config.ts`, `tool-use-trace-store.ts` | the gateway emits `approval_requested` for tool calls but **no per-tool-call telemetry frame**. The agent's tool-use surface is internal. | **Cannot reproduce without a gateway change.** Aura's wire has no `tool_call_started` / `tool_call_completed` frames. The sidecar can't render "Tool: Bash — running, 1.2 s, 0 err" in the card. **Gateway change required.** Workaround: render only what `Frame::Message`'s text mentions, and rely on `description` in `approval_requested` (`channel.ts:90-93`) for approval prompts. |
| Reasoning text extraction (`<reasoning>...</reasoning>`) | `src/card/reasoning-utils.ts` | Aura emits reasoning as text inside the agent reply (no semantic separation on the wire). | Sidecar parses `<reasoning>` itself if the agent uses that convention; otherwise drop. |
| Image resolver (re-host/upload images referenced in card content) | `src/card/image-resolver.ts` | the sidecar already pulls media via `fetchBlobStream` (`blobs.ts:159`). | **Free.** When the agent sends image attachments, fetch them and upload via `im.v1.image` for the card to reference. |
| Unavailable-message guard (skip updates after the card target was deleted) | `src/card/unavailable-guard.ts`, `src/core/message-unavailable.ts` | none | **Sidecar-local.** Direct port. |
| Reply mode resolution (streaming vs static, per chat type / per account) | `src/card/reply-mode.ts` | sidecar decides. | **Sidecar-local config.** |
| Streaming card × approval card interaction (deny mid-stream, approve mid-stream, multiple pending approvals) | `src/card/reply-dispatcher-types.ts`, `tests/reply-dispatcher-tool-use.test.ts` | not modelled | **Sidecar-local state machine, non-trivial.** Two cards live concurrently: the streaming reply + the approval prompt. Deny → streaming card adds a "tool denied" inline marker and continues. Approve → streaming card resumes. Concurrent approvals → multiple cards, each independent. See §4.1.C for the contract. |

### 3.6 Auth & OAuth

OpenClaw's OAuth machinery is the single largest sub-system aside from card streaming and tools — ~14 files in `src/core/{accounts,token-store,device-flow,permission-url,scope-manager,…}.ts` plus `src/tools/{oauth,oauth-cards,oauth-batch-auth,onboarding-auth,auto-auth}.ts`. The reason it's heavy is that openclaw distinguishes **app tokens** (server-to-server, "what the bot can do for itself") from **user access tokens (UAT)** (delegated, "what the bot can do on behalf of an end user"), and each tool family declares its required UAT scopes (`src/core/tool-scopes.ts`). Most Lark tool calls only work if the operator has run the device-flow consent.

| Capability | openclaw-lark | Aura | Verdict |
|---|---|---|---|
| App token bootstrap (app_id + app_secret → tenant_access_token) | `src/core/lark-client.ts`, `feishu-fetch.ts` | the credential is whatever `Frame::start_bot` carries. | **Use the new `StartBot.metadata` map** (decided): `token = app_id` (the user-facing identity), `metadata = { app_secret, encrypt_key, verification_token, base_url? }`. The CLI registration flow (`runRegistration` in `sdks/channel-ts/src/register.ts:30`) supports stdin/stdout multi-prompt, so we can prompt for each field. The host stores them as a single bot row; the sidecar receives them as a typed map. |
| User OAuth — UAT device flow | `src/core/device-flow.ts`, `src/tools/oauth.ts` | **No primitives.** Aura has no per-user OAuth, no per-bot secret store, and no device-flow rendering. | **Decided:** the SDK exposes a generic secret-vault helper backed by Aura's `aura_security` vault, scoped per `(channel_type, bot_id)`. The Lark sidecar uses it to persist UATs keyed by `user_open_id`. Device-flow rendering remains sidecar-local. See §4.0 for the wire/SDK shape. |
| Per-tool scope declaration (`requires: ["im:message:send_as_bot"]`) and pre-execution scope check | `src/core/{scope-manager,app-scope-checker,tool-scopes}.ts`, `src/tools/oapi/helpers.ts` | tools live in `crates/tools/`. They run inside the agent process; channels never see them. **No way to inject "missing scope" errors into the agent.** | If we want feishu tools at all, two options: (a) ship them as Aura tools (Rust crate `aura-tools-lark` with HTTP calls); (b) ship them as MCP-server endpoints exposed by the sidecar over stdio. **Option (b) is the cheaper path and matches the existing MCP scope feedback memory.** |
| Permission consent URL builder | `src/core/permission-url.ts` | n/a | Sidecar-local. |
| OAuth error cards (`oauth-cards.ts`) | renders pending/success/failure/identity-mismatch states | n/a — Aura would surface as a `Frame::Notice` text. | Sidecar can render its own card, but Aura can't drive the state machine. Sidecar-local. |
| Onboarding auth on first pair | `src/tools/onboarding-auth.ts` | Aura's pairing gate sends a code through `Frame::Notice`; the user enters it on the host's pairing CLI. | **Different UX, same end state.** Don't port; reuse Aura pairing. |
| Owner policy / app-owner fallback (app-owner-only tools) | `src/core/owner-policy.ts`, `app-owner-fallback.ts` | tool authorization in Aura is `aura_tools::approval::ApprovalGate` + `crates/tools/src/approval.rs`. | If the tool is implemented as an Aura tool, use Aura's gate; if implemented as an MCP server in the sidecar, the sidecar enforces owner-only. |

### 3.7 OAPI tools (28 tools across 10 families)

Inventory (from `src/tools/oapi/index.ts:46-87`):

- **calendar** (4): `feishu_calendar_calendar`, `feishu_calendar_event`, `feishu_calendar_event_attendee`, `feishu_calendar_freebusy`
- **task** (7): task, tasklist, attachment, section, comment, subtask, task_agent
- **bitable** (5): app, app_table, app_table_record, app_table_field, app_table_view
- **drive** (3): file, doc-comments, doc-media
- **wiki** (2): space, space_node
- **search** (1): doc-wiki
- **chat** (2): chat, members
- **im** (1): bot-image upload (TAT identity)
- **sheets** (~5)
- **common** (2): get_user, search_user

Plus three MCP doc tools (`src/tools/mcp/doc/{create,fetch,update}.ts`) and a parallel TAT IM tool (`src/tools/tat/im/index.ts`).

#### How openclaw actually exposes them — important clarification

These three categories use two different mechanisms in openclaw, and the distinction matters when picking Aura's strategy:

- **OAPI tools (28)** — registered as **in-process openclaw plugin tools** via `api.registerTool({ name, schema, execute })` (helper at `src/tools/helpers.ts:278`, used everywhere — e.g. `src/tools/oapi/calendar/calendar.ts`). Each tool is a TS function with a TypeBox schema; `execute` calls `@larksuiteoapi/node-sdk`'s `Client` directly. The plugin and the agent share an address space. **These tools are NOT MCP.**
- **TAT IM tool (1)** — same registration mechanism (`api.registerTool`), just authenticated with a tenant access token instead of a user access token (`src/tools/tat/im/index.ts:18-22`).
- **MCP doc tools (3 — `feishu_fetch_doc`, `feishu_create_doc`, `feishu_update_doc`)** — also registered via `api.registerTool`, but `execute` body proxies to **Feishu's hosted MCP gateway** at `mcp.larksuite.com` (or equivalent) over HTTP+JSON-RPC (`src/tools/mcp/shared.ts:164` `callMcpTool`, headers `X-Lark-MCP-UAT` + `X-Lark-MCP-Allowed-Tools`). openclaw is a **client** of that MCP service. The reason these three are MCP is that Feishu's hosted MCP gateway exposes doc-editing operations the OAPI doesn't cover; the MCP layer is a workaround for an API gap, not an architectural choice. Note `src/tools/mcp/doc/index.ts:18` literally says "search/list 已由 OAPI 替代" — they actively migrated tools off MCP onto OAPI when possible.

So openclaw's core model is **"all tools are same-process TS functions"**. MCP appears only as an outbound HTTP client to Feishu's hosted service.

#### What this means for Aura

Aura cannot literally copy this — `crates/tools/` is Rust, channel sidecars are separate JS processes, and the agent never dynamically loads sidecar code. The closest equivalents are:

| Strategy | What it looks like | Pros | Cons |
|---|---|---|---|
| **A. Native Aura Rust tools** (`crates/tools-lark/`) | Closest to openclaw's same-process model: 30+ Rust `Tool` impls calling Feishu OAPI directly. Aura's tool approval, leak detector, sensitive-paths gate apply. | Zero round-trip overhead. Native scope-aware approval. Aligns with the `aura-tools` ecosystem. | ~30 tools × ~50-200 LOC each Rust port. Need to re-derive Feishu's typed APIs (no `@larksuiteoapi/node-sdk` reuse). Doesn't reuse openclaw's TypeBox schemas. |
| **B. Sidecar-hosted MCP server** | The Lark sidecar exposes its **own** MCP server (stdio or unix socket) alongside its WS connection. Aura's MCP client connects to it; the agent sees ordinary MCP tools. Tools are 1:1 ports of openclaw's TS handlers. **This is MCP-as-server in the sidecar — opposite direction from openclaw's MCP-as-client usage.** | 1:1 reuse of openclaw's TS implementation, TypeBox schemas, `@larksuiteoapi/node-sdk` calls. UATs live next to the bot, sharing the SDK secret-vault from §4.0.2. Process boundary already exists for the channel — MCP server adds no new processes. | One extra IPC hop per tool call. Depends on the sidecar being alive — if the user disables the bot, MCP tools also disappear (acceptable IMO; a Lark tool without a Lark bot is meaningless). |
| **C. Proxy to Feishu's hosted MCP gateway** (mirroring openclaw's MCP doc tools) | Aura registers thin Rust shims that call `mcp.larksuite.com`. | Cheap to add for the 3 doc tools. | Hosted dependency, requires UAT injection, opaque scopes. Only worth it for capabilities OAPI doesn't expose. |

**Recommendation (no change from before but with the right framing):** **Strategy B for the OAPI bulk** (1:1 port from openclaw's TS), **Strategy C only for the 3 doc tools openclaw itself routes through MCP** (because the underlying APIs literally aren't in OAPI). Strategy A only if benchmark shows MCP IPC is a bottleneck — extremely unlikely for tools whose dominant cost is a Lark API round-trip.

This still respects the existing memory note ("MCP scope is agent-loop only — no slash/mention/elicitation bridges") because the MCP tools surface only through the agent's normal Tool path; the sidecar's MCP server is a tool source, not a control bridge into messaging.

### 3.8 Slash commands

OpenClaw exposes 4 chat slash commands (`/feishu_diagnose`, `/feishu_doctor`, `/feishu_auth`, `/feishu`) plus a CLI subcommand (`feishu-diagnose`).

Aura's gateway is the **single source of truth** for slash commands (`crates/gateway/src/channel/slash.rs:30`) and currently only `/new` is registered. The gateway pushes the manifest to every sidecar via `Frame::SlashManifest` (`channel.ts:162`), and the sidecar mirrors it to the platform via `BotPlatform.registerSlashCommands` (`bot.ts:209-213`).

Implications:

- The Lark sidecar **cannot register channel-specific commands the gateway dispatcher will recognise** without a gateway change. Adding `/lark_doctor`, `/lark_auth`, etc. to `slash.rs::manifest()` is the one and only path. **Gateway change required.**
- Diagnostics (`/lark_doctor`) are a sidecar concern; once `slash.rs::manifest()` lists them, the gateway dispatcher needs a handler. The cleanest split is: (a) gateway-side handler returns "ask the sidecar", (b) gateway forwards the slash invocation to the sidecar via a new `Frame::SlashInvoke`, (c) sidecar responds with text. **Without that wire change, slash diagnostics have to live behind a magic message string** (e.g. user types `/lark_doctor`, sidecar special-cases it before yielding the inbound). That's the cheap stopgap.
- The CLI command (`openclaw feishu-diagnose`) maps to an Aura subcommand under `crates/cli/`. Sidecar can ship a thin CLI as `bin/aura-channel-lark` that runs diagnostics out-of-band (no WS connection needed).

### 3.9 Bundled skills

OpenClaw ships 9 skill packs (`skills/feishu-{bitable,calendar,channel-rules,create-doc,fetch-doc,im-read,task,troubleshoot,update-doc}/`).

Aura's skills crate is `crates/skills/`. Skills are first-class — `crates/skills-assessor` even sandboxes them. **The skill bundle ports directly:** copy the markdown skill files into a `skills/` directory under the Lark sidecar package (or, if we want them shipped with the gateway, into the workspace `skills/` root).

Skills do *not* depend on the channel sidecar at runtime — they're text snippets surfaced to the agent. The only coupling is naming: `feishu-channel-rules/SKILL.md` carries `alwaysActive: true` and contains output-formatting rules specific to the channel; this needs to be activated only when the active channel type is `lark`. Aura's skill activation already has channel awareness via `crates/skills/`.

### 3.10 Diagnostics & observability

| Capability | openclaw-lark | Aura | Verdict |
|---|---|---|---|
| `runDiagnosis` — config validation, connectivity check, permission probe, multi-account warnings | `src/commands/diagnose.ts`, `src/commands/doctor.ts` | nothing channel-specific. | **Port to a sidecar `bin/lark-diagnose` CLI**, plus a gateway-side `/v1/admin/channels/lark/diagnose` JSON endpoint if we want the WebUI to surface health. |
| Trace by message_id (replay the inbound's full processing chain) | `src/commands/diagnose.ts:traceByMessageId` | `aura-trace` crate has session traces, but not platform-side. | Sidecar maintains a ring buffer of recent inbounds keyed by `platform_msg_id`. |
| Multi-account security warnings (duplicate app_id, etc.) | `src/core/security-check.ts`, `index.ts:209` | `crates/security/src` runs at agent execution time, not bot config time. | Run inside the sidecar at `StartBot`. |
| Tool-use trace store (per-call `start/end` with params, result, error, durationMs) | `src/card/tool-use-trace-store.ts` (locked down by `tests/tool-use-trace-store.test.ts`, 20683 bytes) | not modelled in the wire | **Gateway change required for parity.** Without `tool_call_started`/`tool_call_completed` frames the sidecar can't know what the agent's tool was doing. |

### 3.11 Channels-as-tools

OpenClaw also registers `feishu_oauth`, `feishu_oauth_batch_auth`, and `ask_user_question` as tools the agent can call (`index.ts:122-128`). All three are *channel-coupled* — the OAuth tools talk to the bot's accounts store, and `ask_user_question` posts an interactive card and waits for the user's tap.

In Aura:
- OAuth tools → either MCP server hosted by the sidecar, or skip (UATs aren't part of the agent's flow).
- `ask_user_question` → there's no Aura primitive for "agent asks user a question and waits for a card-button reply". The closest is `approval_requested`, which is binary. **Implementing this in full needs a wire-level question/reply pair.** Workaround: an MCP tool exposed by the sidecar that posts a card, blocks on a callback, and returns the text. Acceptable for Phase 3.

## 4. Concrete build plan

### Phase 0 — SDK + gateway extensions (lands first)

These are channel-agnostic and benefit Telegram/Weixin/future channels too. SDK-first principle: every Lark sidecar consumer below pulls from `@aura/channel-sdk`.

#### 4.0.1 `Frame::StartBot.metadata: HashMap<String, String>` *(gateway + SDK)*

Additive field on `aura-channels::wire::Frame::StartBot`. Old sidecars that don't read it stay wire-compatible (rmp-serde drops unknown / missing fields silently — same pattern as the existing `bot_id` and `attachments` extensions documented in `crates/model/.../Message.ts:27-37`).

Rust side, `crates/channels/src/wire.rs`:

```rust
Frame::StartBot {
    bot_id: String,
    token: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    metadata: HashMap<String, String>,
}
```

Gateway-side bot store (`aura_storage::ChannelBotStore`) gains a `metadata` column (libsql) with the same soft-delete discipline as the rest of the schema (CLAUDE.md storage section). The reconciler (`crates/gateway/src/channel/bot_reconciler.rs`) forwards it on every `StartBot`.

SDK side, `sdks/channel-ts/src/channel.ts`:

```ts
export interface StartBotCommand {
  botId: string;
  token: string;
  metadata: Readonly<Record<string, string>>;
}
```

Use it for: Lark `(app_secret, encrypt_key, verification_token, base_url)`, Discord intents bitmask, Slack signing-secret, anything else multi-secret per bot.

**Lark-specific convention for `base_url`:** the field is a free-form OAPI base origin (e.g. `https://open.feishu.cn`, `https://open.larksuite.com`, `https://open.volc-feishu.cn`). The sidecar derives the open-platform WSS endpoint and OAuth endpoint from it by simple host substitution (`open.` → `ws.`, `open.` stays for OAuth); the SDK's `Domain` enum is selected by matching the host against a small allowlist inside the sidecar. **The metadata stays a string, not an enum** — this keeps the wire transport-neutral and lets new Lark deployments (private clouds, future regional mirrors) ship without an SDK update. CLI/WebUI render a free-text field with three placeholder hints rather than a dropdown.

#### 4.0.2 SDK secret-vault client *(gateway + SDK, WS-based)*

The principle the user laid out: **the SDK exposes a `Secrets` interface; the sidecar uses it to persist tokens.** Storage lives in Aura's vault so per-user UATs participate in encryption-at-rest, redaction, and rotation alongside everything else `aura_security` already protects.

**Transport: WebSocket frames, not HTTP.** The blob side-channel is HTTP because blobs are large (up to 100 MiB) and have to stream — buffering them through the WS frame queue would block frame ordering across all flows. Secrets are < 1 KiB typical / 64 KiB hard-capped and never streamed. WS is the right transport because:

- One transport for the sidecar to manage (no HTTP base URL derivation).
- Auth implicit from the existing `Register` frame's `(pid, label)` binding — no header re-validation path to harden.
- Namespace scoping rides the connection's identity, can't be spoofed by header injection.
- Pattern precedent: `Frame::StartBot` → `Frame::BotStatus` is already a request/response pair on this WS.

New wire frames (gateway side, additive):

```rust
Frame::SecretRequest {
    request_id: String,    // SDK-supplied UUID, opaque to gateway
    bot_id: String,        // gateway validates against ChannelBotStore
    op: SecretOp,          // Get | Set | Delete | List
    key: Option<String>,
    value: Option<String>,
}
Frame::SecretReply {
    request_id: String,
    ok: bool,
    value: Option<String>,         // Get
    keys: Option<Vec<String>>,     // List
    error: Option<String>,         // bot_unknown | key_too_long | value_too_large | quota_exceeded | internal
}
```

**Scoping:** the gateway prepends `<channel_type>/<bot_id>/` to the stored key before hitting the vault. `bot_id` is validated server-side against the sidecar's known bot registry; an unknown one fails with `bot_unknown`. Cross-bot reads are impossible — the sidecar can't claim a `bot_id` it doesn't own.

Backing store: `aura_security` (`crates/security/src/`). The vault already encrypts at rest with AES-GCM (per the fuzz target list in CLAUDE.md), redacts on log lines via `RedactingMakeWriter` (`crates/gateway/src/sidecar/supervisor.rs:74`), and emits no plaintext in `aura.log`.

**Body limits:** value ≤ 64 KiB, key length ≤ 256 bytes, count per `(channel_type, bot_id)` ≤ 10 000.

**SDK module** (`sdks/channel-ts/src/secrets.ts`, new):

```ts
export interface Secrets {
  /** Scope to a specific bot. Gateway validates botId against the
   *  sidecar's registered bots; unknown botId rejects with "bot_unknown". */
  scope(botId: string): SecretsScope;
}

export interface SecretsScope {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<void>;
  list(): Promise<string[]>;
}

/** No options. baseUrl, token, and namespace are all gateway-controlled. */
export function secrets(): Secrets;

export class SecretQuotaExceededError extends Error { ... }
export class SecretBotUnknownError extends Error { ... }
```

Internally the SDK runner gains a `SecretRouter` alongside the existing `FrameQueue` (`sdks/channel-ts/src/runner.ts:342-379`), correlating outbound `SecretRequest` with inbound `SecretReply` by `request_id` with a 5s default timeout.

Lark consumer:

```ts
// in src/auth/uat-store.ts
const s = secrets().scope(botId);
await s.set(`uat/${userOpenId}`, JSON.stringify({ token, refresh, exp }));
const raw = await s.get(`uat/${userOpenId}`);
```

The same primitive lets future Discord OAuth, Slack workspace tokens, etc. share the vault without each sidecar reinventing storage.

Effort: ~1 day Rust (frame handlers + bot validation + vault wrapper + size guards), ~half a day SDK (`SecretRouter` + the typed wrapper), plus tests.

#### 4.0.3 Tool-use telemetry frames *(gateway only, deferred to Phase 3)*

Listed here for completeness but not Phase-0 work. Adds `Frame::ToolCallStarted` / `Frame::ToolCallCompleted` so streaming cards can render mid-agent tool runs. Defer until Phase 3 — no Phase-1 or Phase-2 capability blocks on it.

#### 4.0.4 Abort-session frame *(gateway, deferred to Phase 3)*

`Frame::AbortSession { session_id: String, reason: String }` — sidecar-initiated, instructs the gateway to cancel the in-flight agent turn for `session_id`. Reason is free-form (`"user_requested"`, `"chat_unavailable"`, …). Server side, `crates/agent/src/service.rs` (or equivalent) propagates a `tokio_util::sync::CancellationToken` cancel into the active task. Gated by capability `"abort"` (§4.0.6).

**Post-abort session state — option (a):** the session is preserved. The current turn stops at whatever step it was on (mid-tool-call, mid-streaming-text, mid-LLM-call), but `(session_id, history)` survives — the next user message resumes a fresh turn against the existing context, with no implicit reset.

**History annotation (agent-side, not SDK):** when the agent's history-append path detects that a turn was terminated by `AbortSession` (vs. natural completion), it prepends `[aborted by user]` to the last entry written for that turn — whether that's a partial tool-call result, a half-streamed assistant message, or a synthetic empty-step record. The next turn's LLM input includes this marker so the model knows the prior turn was interrupted rather than silently truncated. Implementation lives in `crates/agent/`, not the wire — the wire only carries the cancel signal.

SDK side, `Channel` gains an outbound helper (no new hook — abort is *outbound* from sidecar):

```ts
export interface ChannelOutbound {
  abortSession(sessionId: string, reason: string): Promise<void>;
}
```

`runChannel`/`BotChannel` expose this through a per-connection `outbound` accessor. Lark sidecar's `abort-detect.ts` port calls it when user input matches the abort heuristic. Calling `abortSession` when the gateway didn't advertise `"abort"` capability (§4.0.6) throws — older gateways are guaranteed never to receive the frame.

**Companion frames: Diagnose** *(also Phase 3, gated by capability `diagnose`)*

The Lark `bin/lark-diagnose` CLI and the WebUI admin "channel health" panel both need to ask the running sidecar to run its self-checks (config parse, `base_url` reachability, app-token issuance, scope inventory, secret-vault connectivity). Going through the existing WS keeps everything authenticated under one transport.

```rust
Frame::DiagnoseRequest {
    request_id: String,
    bot_id: Option<String>,    // None → channel-wide diagnostic; Some → per-bot
}
Frame::DiagnoseReply {
    request_id: String,
    ok: bool,
    report: DiagnosticReport,  // see below
    error: Option<String>,
}
```

`DiagnosticReport` is a serialisable struct shipped on the wire (rmp-serde), generic enough that any future channel can reuse the shape:

```rust
struct DiagnosticReport {
    overall_status: String,        // "healthy" | "degraded" | "unhealthy"
    sidecar_version: String,
    timestamp_ms: i64,
    checks: Vec<DiagnosticCheckResult>,
}

struct DiagnosticCheckResult {
    id: String,                    // "config", "connectivity", "scopes", "uat_store", ...
    label: String,
    status: String,                // "ok" | "warn" | "fail" | "skipped"
    detail: Option<String>,
    remediation: Option<String>,
}
```

**Consumer path:**

- `bin/lark-diagnose` → admin API `GET /v1/admin/channels/lark/diagnose?bot_id=<...>` → gateway dispatches `Frame::DiagnoseRequest` to the sidecar → renders the reply as markdown via the SDK's `formatReportMarkdown` / `formatReportCli` helpers.
- WebUI panel hits the same admin path, renders the report struct directly.
- The CLI never touches the sidecar's process directly — no `bun` spawn, no socket file. Goes through the gateway's existing auth.

**SDK side** (`sdks/channel-ts/src/channel.ts` extension): `Channel` gains an inbound hook the sidecar implements:

```ts
export interface Channel {
  // ... existing
  onDiagnoseRequested?(req: { botId?: string }): Promise<DiagnosticReport>;
}
```

The SDK runner serialises the returned report into `Frame::DiagnoseReply`. Sidecars that don't implement the hook auto-reply with `ok: false, error: "diagnose not implemented"`. Lark wires its `src/diagnostic/` module here. Report formatters (`formatReportMarkdown`, `formatReportCli`) live in the SDK because the report struct is wire-defined and shared across channels — this is the one diagnostic-related thing that earns its place in the SDK (vs the framework retraction in §4 Phase 3 task 5).

#### 4.0.5 Phase 0 tail work — CLI, WebUI, redaction

The wire/SDK additions in §4.0.1-4.0.2 only deliver value once the surrounding tooling consumes them. Lands together with Phase 0:

- **CLI** (`crates/cli/src/commands/channel.rs` or equivalent): `aura channel add <type> <bot_id>` learns `--metadata-file <path>` (JSON object) and `--metadata <key>=<value>` (repeatable). For Lark specifically, the registration helper in the sidecar (`src/auth/register-flow.ts`) prompts via `runRegistration` for each field interactively. Either path writes the same `bots` row.
- **WebUI** (`web/src/`): the "Add bot" form renders channel-type-conditional fields. Reads the per-channel field manifest from a new gateway endpoint (`GET /v1/admin/channels/<type>/registration-fields`) so the UI doesn't hardcode Lark's four fields. The endpoint serves a small JSON schema the sidecar declares (label, kind: `password|input`, required). Defers UI design but the JSON shape is locked Phase 0.
- **Redaction** (`crates/security/src/`): the leak-detector pattern table (`crates/security/fuzz/corpus/fuzz_leak_detector/`) gains regexes for `app_secret`, `encrypt_key`, `verification_token` so they get redacted in logs and trace dumps the same way Telegram bot tokens already are. The `RedactingMakeWriter` chain (`crates/gateway/src/sidecar/supervisor.rs:74`) automatically picks them up.
- **Bot-store schema** (`crates/storage/src/libsql/mod.rs`): `bots` table gains a `metadata TEXT` column (JSON-serialised `HashMap<String,String>`). Soft-delete discipline already in place stays as-is.
- **Secrets schema + lifecycle** (`crates/storage/src/libsql/mod.rs`): new `secrets` table keyed by `(bot_row_id, key)` with the standard `deleted_at` column. **Lifecycle is bonded to the `bots` row, not to the `(channel_type, bot_id)` string** — soft-deleting a bot soft-deletes all its secrets atomically; reviving the bot revives its secrets. This sidesteps the bot-id reuse pitfall: a future bot row reuses the string `bot_id` but has a different `bot_row_id`, so it starts with an empty namespace by default unless the user explicitly chose "restore" at registration time.
- **Bot re-registration UX** (CLI + WebUI): `aura channel add <type> <bot_id>` first checks for a soft-deleted row matching `(channel_type, bot_id)`. If one exists, the host responds (CLI: interactive prompt; WebUI: `409 Conflict` with options) with three choices:
  - **restore** (`r`) — clears `deleted_at` on the existing row; the row's secrets revive automatically (since they're keyed by `bot_row_id`).
  - **fresh** (`f`) — hard-deletes the soft-deleted row + cascade-purges its secrets, then inserts a new row with a new `bot_row_id`.
  - **cancel** (`c`) — abort.

  This lets the operator choose between "I deleted by mistake, bring it back" and "I want a clean start with the same name" without ambiguity. Soft-delete is the default everywhere; hard-delete is only triggered by explicit `f` confirmation.

Effort: ~1 day total alongside §4.0.1-4.0.2.

#### 4.0.6 Capability negotiation *(replaces `PROTOCOL_VERSION`, lands Phase 0)*

`PROTOCOL_VERSION` is **dropped**. The current strict-equality version check (`runner.ts:296-323`) brittle-fails any cross-version pairing, which forces a coordinated upgrade across gateway + every sidecar each time we add a frame. Replace with capability negotiation.

**Wire shape changes:**

```rust
Frame::Register {
    token: String,
    channel_type: String,
    capabilities: Vec<String>,        // new
    session_id: Option<String>,
    // protocol_version: REMOVED
}

Frame::RegisterAck {
    ok: bool,
    reason: Option<String>,
    capabilities: Vec<String>,        // new — gateway's own advertised set
}
```

**Core frames are always supported** and need no capability string: `register`, `register_ack`, `message`, `delta`, `notice`, `approval_requested`, `approval_resolved`, `resolve_approval`, `start_bot`, `stop_bot`, `bot_status`, `slash_manifest`, `sidecar_log`, `history_*`. These are the working set Telegram and Weixin already speak; legacy compatibility means they can never be gated.

**Capability strings introduced by this report:**

| String | Phase | Gates | Sender |
|---|---|---|---|
| `secrets` | 0 | `Frame::SecretRequest`, `Frame::SecretReply` | bidirectional |
| `abort` | 3 | `Frame::AbortSession` | sidecar → gateway |
| `tool_telemetry` | 3 | `Frame::ToolCallStarted`, `Frame::ToolCallCompleted` | gateway → sidecar |
| `mcp_tunnel` | 3 | `Frame::Mcp` | bidirectional |
| `diagnose` | 3 | `Frame::DiagnoseRequest`, `Frame::DiagnoseReply` | gateway → sidecar (request) |

`StartBot.metadata` is **not** gated — it's an additive field on an existing frame, defaults to `{}` on the wire when missing, both ends transparently compatible.

**Negotiation rule:** the effective capability set is the *intersection* of what each side advertised. The frame sender always checks the receiver's advertised set before emitting a non-core frame. SDK enforces this by throwing `CapabilityMissingError` when a helper (e.g. `secrets()`, `abortSession()`) is called against a gateway that didn't advertise the corresponding capability.

**Forward compat for legacy sidecars:** old sidecars built before this change send no `capabilities` field; rmp-serde decodes it as `vec![]`. Gateway treats `[]` as "core-only" and never sends them new frames. Old sidecars never send new frames either (they don't know about them). Zero-touch compatibility.

**SDK side** (`sdks/channel-ts/src/channel.ts`):

```ts
export interface RunOptions {
  // existing fields...
  capabilities?: ReadonlyArray<string>;   // sidecar's advertised set
}

// runChannel exposes the negotiated set on its outbound accessor:
export interface ChannelOutbound {
  readonly negotiated: ReadonlySet<string>;
  abortSession(sessionId: string, reason: string): Promise<void>;  // throws if !negotiated.has("abort")
  // ... other capability-gated helpers
}

export class CapabilityMissingError extends Error {
  constructor(public readonly capability: string) { super(`gateway did not advertise capability '${capability}'`); }
}
```

`runSidecar` auto-derives the SDK's capability set from which optional helpers the channel imports — but a sidecar can override with `runOptions.capabilities`.

#### 4.0.7 Consolidated SDK additions

Single-source-of-truth list of what `@aura/channel-sdk` (`sdks/channel-ts/`) gains across all phases. Everything below is **additive** — existing Telegram and Weixin sidecars keep working unchanged.

##### Phase 0 (lands first, blocks Lark MVP)

**A. `StartBotCommand.metadata` field** (`sdks/channel-ts/src/channel.ts`).

```ts
export interface StartBotCommand {
  botId: string;
  token: string;
  metadata: Readonly<Record<string, string>>;   // new
}
```

Wire frame `Frame::StartBot` gains a matching `metadata: HashMap<String, String>` (rmp-serde default-empty so missing on old senders, omitted on serialize when empty). Channel-agnostic: every multi-secret sidecar uses it. No new module / export.

**B. `secrets` module** (`sdks/channel-ts/src/secrets.ts`, new — added to `package.json` exports under `"./secrets"`).

```ts
export interface Secrets {
  scope(botId: string): SecretsScope;
}
export interface SecretsScope {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<void>;
  delete(key: string): Promise<void>;
  list(): Promise<string[]>;
}
export function secrets(): Secrets;          // no options — fully gateway-controlled
export class SecretBotUnknownError extends Error {}
export class SecretQuotaExceededError extends Error {}
```

Internally relies on a SDK-internal `SecretRouter` that correlates `Frame::SecretRequest` ↔ `Frame::SecretReply` by `request_id` (5s default timeout). Server side is `aura_security` vault, scoped by `(channel_type, bot_id)` validated at the gateway.

##### Phase 3 (functionality-driven, lands when needed)

**C. Tool-use telemetry hooks on `Channel`** (extend `sdks/channel-ts/src/channel.ts`).

```ts
export interface ToolCallStarted {
  callId: string;
  sessionId: string;
  userId: string;
  tool: string;
  paramsPreview: string;
}
export interface ToolCallCompleted {
  callId: string;
  resultPreview?: string;
  error?: string;
  durationMs: number;
}
export interface Channel {
  // existing hooks...
  onToolCallStarted?(ev: ToolCallStarted): Promise<void>;
  onToolCallCompleted?(ev: ToolCallCompleted): Promise<void>;
}
```

Backed by new wire frames `Frame::ToolCallStarted` / `Frame::ToolCallCompleted` (gateway-side change). Lark renders these inline in the streaming card; channels that don't implement the hooks silently drop the frames.

**D. `mcp-server` helper module** (`sdks/channel-ts/src/mcp-server.ts`, new — added to exports under `"./mcp-server"`).

Generic glue for sidecars that want to expose tools to Aura's agent via MCP-as-server (the recommendation in §3.7 for Lark's 28 OAPI tools). Wraps `@modelcontextprotocol/sdk` and integrates with the sidecar's logger, abort signal, and `secrets()` namespace.

**Transport: gateway-tunneled WS frames, no HTTP path.** The agent and gateway share a process; the gateway and sidecar share a WS. To bridge agent→sidecar MCP traffic without opening a third transport (no `/v1/mcp/*` path, no unix socket), JSON-RPC envelopes ride existing wire frames:

```rust
Frame::Mcp {
    body: ByteBuf,   // one MCP/JSON-RPC message per frame, opaque to the gateway
}
```

Bidirectional. Gateway forwards transparently between (a) the agent-side `ChannelTunnelTransport` (Rust trait impl in `crates/tools/`) and (b) the sidecar's `runMcpServer` consumer. The gateway never decodes the body — it's just a routed envelope per `(channel_type, bot_id)`. Gated by capability `mcp_tunnel`.

**Body size cap: 1 MiB per envelope.** The decode path on both gateway and SDK rejects oversized frames with `mcp_envelope_too_large`. Tools that may legitimately produce larger results MUST paginate or stream — for the Lark surface, `feishu_fetch_doc` already supports `offset` / `limit` (`origin_channel_plugin/openclaw-lark/src/tools/mcp/doc/fetch.ts`); the port keeps that contract. Truly large binary payloads (image attachments returned by tools, etc.) go through the blob side-channel: the tool returns a `{ blob_id }` reference and the agent's MCP client fetches via `/v1/blobs/<id>` if it cares to inline the bytes. This keeps WS frame ordering predictable — a single tool result never blocks the chat-message flow for more than the time-to-transfer of 1 MiB.

This means **no new HTTP path is added** — admin API surface stays unchanged; auth model unchanged; future MCP protocol versions transparent (gateway doesn't care about envelope contents).

**Lifecycle: per-sidecar, NOT per-bot.** One `runMcpServer` instance lives for the lifetime of the sidecar process and serves all bots running inside it. The static OAPI tool list (28 entries) is the same regardless of how many bots are registered, so per-bot servers would just churn lifecycle on `StartBot`/`StopBot` for no benefit. Per-tool dispatch routes to the correct bot via `McpToolContext.botId` (decoded from `_meta.auraSessionId`); when no bot matches, the handler returns `"no lark bot configured for session <id>"`. Single point of subscription, no `tools/list` mutation on bot churn.

**Boot-order handling: agent process can start before any sidecar is registered.** Aura's supervisor model spawns sidecars lazily (`crates/gateway/src/sidecar/supervisor.rs:1-19`) — only after the bot store has at least one row for that channel type. The agent, however, lives in the gateway process and may issue `tools/list` immediately on startup or as part of cron-triggered turns. Two contracts handle this cleanly without any new wire frames:

- **`tools/list` returns `[]`** while the sidecar is unregistered. The agent's MCP client treats the channel-tunnel as connected-but-empty rather than failing with "no transport". No retries, no errors logged.
- **MCP standard `notifications/tools/list_changed`** is emitted by the gateway on the channel-tunnel the moment a sidecar successfully registers and advertises `mcp_tunnel`. Agent's MCP client re-issues `tools/list` and picks up the now-populated tool catalogue. Equivalent notification fires when the sidecar disconnects (tools list goes back to `[]`).
- **Cron jobs that fire while the sidecar is down** see an empty tool list and the call site returns `"sidecar not running"`. No frame retries, no replay. The cron job's error-handling decides whether to retry on its next tick.

Total cost: ~50 LOC in the gateway's mcp-tunnel state machine plus the SDK exposing `notifications/tools/list_changed` from `runMcpServer` on registration. No `Frame::*` additions.

```ts
export interface McpServerOptions {
  name?: string;                 // default `${channelType}/${botId}`
  tools: McpToolHandler[];
  // No transport option — the SDK auto-binds to the channel's WS tunnel.
  logger?: Logger;
  signal?: AbortSignal;
}
export function runMcpServer(opts: McpServerOptions): Promise<McpServerHandle>;

/**
 * The session-identity context the helper extracts from each
 * `tools/call.params._meta` and passes to the tool handler. Aura's
 * MCP client injects it on every call (decision: §6 item 6); the
 * helper looks up the auraSessionId, decomposes it into
 * (channelType, botId, userId), and hands the tuple to the handler.
 */
export interface McpToolContext {
  auraSessionId: string;
  channelType: string;
  botId: string;
  userId: string;
}

export interface McpToolHandler<TArgs = unknown, TResult = unknown> {
  name: string;
  schema: object;                          // JSON Schema or TypeBox
  call(args: TArgs, ctx: McpToolContext): Promise<TResult>;
}
```

Lark consumes it for the OAPI tool bulk; future Discord / Slack channels with their own tool surfaces use the same helper. The 3 doc tools that openclaw itself proxies to Feishu's hosted MCP gateway stay as ordinary `McpToolHandler` entries whose body issues an outbound HTTP call.

**E. Outbound abort helper** (extends `sdks/channel-ts/src/channel.ts`).

```ts
export interface ChannelOutbound {
  abortSession(sessionId: string, reason: string): Promise<void>;
}
```

Returned by `runChannel` (and exposed on `BotChannel`). Sends `Frame::AbortSession` upstream. Lark consumes from its abort-detection inbound filter; other channels can call it freely (e.g. when a chat is deleted mid-stream).

**F. Diagnose hook + report formatters** (extends `sdks/channel-ts/src/channel.ts`, plus new `sdks/channel-ts/src/diagnose.ts`).

```ts
// channel.ts
export interface Channel {
  // ... existing
  onDiagnoseRequested?(req: { botId?: string }): Promise<DiagnosticReport>;
}

// diagnose.ts (new module — exported under "./diagnose")
export interface DiagnosticReport {
  overallStatus: "healthy" | "degraded" | "unhealthy";
  sidecarVersion: string;
  timestampMs: number;
  checks: DiagnosticCheckResult[];
}
export interface DiagnosticCheckResult {
  id: string;
  label: string;
  status: "ok" | "warn" | "fail" | "skipped";
  detail?: string;
  remediation?: string;
}

export function formatReportMarkdown(r: DiagnosticReport): string;
export function formatReportCli(r: DiagnosticReport): string;
```

The struct is on the wire (rmp-serde mirror), so the formatters live in SDK — they're the one piece of "diagnostics framework" that survived the §4 Phase 3 task 5 retraction. Per-channel check definitions stay sidecar-local; only the report shape and renderers are shared. Lark's `bin/lark-diagnose` and the WebUI both consume `formatReportMarkdown` for human display.

##### Explicitly NOT in the SDK

- **Streaming card rendering primitives** — each platform's card model is too different (Lark CardKit JSON vs Discord embeds vs Slack Block Kit). The SDK only delivers `Frame::Delta` to `Channel.onDelta`; the sidecar owns the rendering loop. The shared part ("accumulate deltas + throttle by tokens / time window") is < 50 LOC and not yet worth abstracting; revisit only when a second channel ships streaming UI.
- **Card builder / `FlushController` / `UnavailableGuard` / CardKit v1+v2 schemas** — Lark-local, under `channel-src/lark/src/card/`.
- **Mention parser, message dedup, group policy gates** — platform-specific; sidecar-local.
- **OAuth UAT device flow rendering** — only the *storage* (the Phase 0 `secrets` module) is generic; the device-flow UX is Lark-specific.
- **Diagnostics framework** — initially scoped to SDK in earlier draft; retracted. Lives under `channel-src/lark/src/diagnostic/` until a second channel needs it.
- **Synthetic-target outbound drop** — Lark-local for now; promote to a `BotInboundEvent.synthetic?: boolean` flag in `BotChannel` if Telegram/Discord ever need similar semantics.

### Phase 1 — MVP sidecar (~1 week, after Phase 0 lands)

Target: text in/out, single account, image+file attachments, typing, approvals, gateway slash commands.

```
channel-src/lark/
  package.json                    # @aura/channel-lark, deps on @larksuiteoapi/node-sdk
  src/
    index.ts                      # runSidecar({ channelType: "lark", build, register })
    platform.ts                   # LarkPlatform implements BotPlatform<Client, LarkChat>
    approvals.ts                  # LarkApprovals implements PlatformApprovals — interactive card
    types.ts                      # LarkChat = { chatId; threadId? }
    auth/
      register-flow.ts            # ctx.password("app_id:"), ctx.password("app_secret:"), ...
      app-credentials.ts          # parse StartBotCommand.metadata → AppCredentials
    messaging/
      inbound.ts                  # WSClient subscribe, parseMessageEvent, dedup, mention strip
      send-text.ts                # im.v1.message.create(msg_type:"text")
      send-card.ts                # CardKit v2 from markdown
    media/
      inbound.ts                  # download from im.v1.message.resource → uploadBlob
      outbound.ts                 # fetchBlobStream → im.v1.image / im.v1.file
```

`StartBot` shape after Phase 0:

```
token    = "<app_id>"
metadata = {
  "app_secret":            "<secret>",
  "encrypt_key":           "<key>",
  "verification_token":    "<token>",
  "base_url":              "https://open.feishu.cn"   // optional, switches Lark/Feishu domain
}
```

Host registers via `aura channel add lark <app_id> --metadata-file lark-app.json` (CLI already supports stdin file ingestion for credentials).

What we get for free from `BotChannel`:
- StartBot/StopBot lifecycle, idempotency, generation guards (`bot.ts:603-710`)
- typing indicator, refresh, safety cancel (`bot.ts:783-861`)
- approval routing, route purge on bot-stop (`bot.ts:578-602`)
- inbound queue + back-pressure (`bot.ts:909-937`)

What we get from Phase 0:
- typed multi-secret StartBot (no JSON-in-string hack)
- `secrets().scope(botId)` ready for Phase 3 UATs (we don't need it in Phase 1 itself, but the import surface is there)

#### 4.1.A Pairing-UX onboarding card *(Phase 1, sidecar-only)*

Aura's pairing gate emits `Frame::Notice` carrying a short pairing code on the first message from an unpaired `(channel_type, bot_id, user_id)` triple. The default `BotChannel` rendering (`bot.ts:566` — `⚠️ <text>`) is fine for Telegram/Weixin where the user can DM their dev who runs `aura pair approve`. Lark serves SaaS users who likely **cannot reach the host CLI**.

Lark sidecar overrides `BotPlatform.sendNotice` so the first pairing-code notice is rendered as a CardKit v2 onboarding card with:

- the pairing code in a `monospace` block (large, copy-friendly)
- a one-line instruction: "Ask your Aura admin to run `aura pair approve <code>`"
- a "Refresh status" button (callback that re-issues the silent inbound to check whether pairing has been approved)

Detection: parse `notice.text` for the pairing-code pattern (the gateway's pairing notice has a stable shape — see `aura_pairing` crate). All other notices fall through to the default text rendering.

Effort: ~150 LOC (notice classifier + CardKit builder + button-callback router into the sidecar's existing `card_action` handler).

#### 4.1.B Phase 1 risk-validation checklist (do these on day 1)

Before writing the platform layer, verify the bundle pipeline works:

- [ ] `pnpm install && pnpm --filter @aura/channel-lark bundle` produces a non-empty `dist/bundle.mjs`
- [ ] `bun ./channel-src/lark/dist/bundle.mjs --version` exits cleanly without deadlock or `__filename`/`require` errors
- [ ] Lark's `WSClient.start()` from inside the bundle connects to `wss://open.feishu.cn` (use a sandbox tenant)
- [ ] `client.im.v1.message.create({...})` round-trips a text message

Reasoning: per the existing Weixin trap inventory in CLAUDE.md ("Channel sidecars use host bun"), Node-flavored deps regularly fail under bun's bundler — we know `node-fetch@2`, `whatwg-url@5`, and CJS dual-package shims have all bitten previous sidecars. Lark's `@larksuiteoapi/node-sdk` 1.60.0 transitively pulls `axios` + a custom HTTP layer; assume nothing works until proven. Catching a packaging failure on day 1 saves a week of "why does this work locally but not in the embedded gateway build".

If validation fails: file an issue against `@larksuiteoapi/node-sdk`, patch via `pnpm.patchedDependencies`, or in the worst case drop down to `fetch` + a hand-rolled OAPI client. Do **not** push the discovery to Phase 1 end.

#### 4.1.C Approval card — operator filter + streaming-card state machine *(Phase 1, sidecar-only)*

Two correctness concerns the default `BotChannel` approval flow doesn't cover:

**Operator filter (security).** In a group chat where the bot was @-mentioned by user *A*, an approval card sent to that group is clickable by anyone in the group. Without filtering, user *B* could approve a Bash command user *A*'s message asked the agent to run. Lark's `card.action.trigger` event payload carries `operator.user_id` (or `operator.open_id`); the sidecar must cross-check it against the original `ApprovalRequest.userId` (decomposed via `composeAuraUserId`'s inverse).

```ts
// LarkApprovals.attach() in src/approvals.ts
client.event.cardAction.add(async (event) => {
  const callId = event.action.value.call_id;
  const pending = this.pending.get(callId);
  if (!pending) return; // unknown / already-resolved approval

  const operatorOpenId = event.operator.open_id;
  const triggererOpenId = decomposeAuraUserId(pending.req.userId).platformUserId;

  if (operatorOpenId !== triggererOpenId) {
    // Toast back to the operator — does NOT resolve the promise
    return {
      toast: {
        type: "warning",
        content: `仅 @${pending.triggererName} 可操作此审批`,
      },
    };
  }
  pending.resolve(event.action.value.decision);
});
```

In a 1:1 chat, the filter is a no-op (there's only one other user). In a group, it prevents cross-user approval bypass.

**Streaming-card × approval-card state machine.** Concurrent live cards during a single agent turn:

- *T0* — user mentions bot. Sidecar sends streaming card *S* (in `streaming` state).
- *T1* — agent emits `Frame::ApprovalRequested` mid-stream (e.g. before a Bash call). Sidecar sends approval card *A* (separate message). *S* stays in `streaming` but the streaming-card text gets a "⏸ waiting on approval for `<tool>`" inline marker.
- *T2a* — user denies. *A* card edits to "❌ denied". *S* card receives a synthetic delta `[tool denied]` and resumes streaming the agent's next output.
- *T2b* — user approves. *A* card edits to "✅ approved". *S* card resumes streaming whatever the agent emits next (typically the tool result inline).
- *T3* — multiple concurrent approvals possible (agent fires two tools in parallel). Each gets its own card; each evaluates independently. The streaming card's marker line lists all currently-pending approvals.
- *T4* — user sends a new inbound while *S* is still streaming. The abort-detect heuristic (Phase 3) fires `Frame::AbortSession`. *S* finalises with `[aborted by user]`; outstanding *A* cards close with "⊘ session aborted" and auto-deny their pending callbacks.

`StreamingCardController` owns the marker-line state; `LarkApprovals` owns the per-call card lifecycle; both consult a shared `ActiveApprovalsRegistry` for the user-id → call-id map. ~250-300 LOC across `card/` and `approvals.ts`. Lock down with tests mirroring openclaw's `tests/reply-dispatcher-tool-use.test.ts`.

### Phase 2 — Pretty (~2-3 weeks)

Add:

- **CardKit v2 streaming** — port `src/card/{streaming-card-controller,flush-controller,builder,cardkit,markdown-style,unavailable-guard}.ts`. ~1500 LOC of TypeScript. Implement `Channel.onDelta` to feed the streaming controller.
- **Mention parser & group gate** — port `src/messaging/inbound/{mention,gate,policy}.ts` (small).
- **Reaction echo** — sidecar can post a 👍 reaction on receipt as user feedback; agent can't drive reactions.
- **Multi-account** — already covered by `BotChannel`; add a small sidecar config blob for per-account overrides (footer, streaming on/off).
- **Comment-thread routing** — port `src/messaging/inbound/comment-handler.ts` and `src/core/comment-target.ts`. Detect `_doc:` / `_wiki:` synthetic targets; route outbound through Drive comments API.

No gateway changes needed yet.

### Phase 3 — Power-user parity (~1-2 months + 2-3 gateway PRs)

This is where Aura's runtime needs to grow:

1. **Tool-use telemetry frames** (gateway change) — `Frame::ToolCallStarted { call_id, tool, params_preview }` and `Frame::ToolCallCompleted { call_id, result_preview, error?, duration_ms }`. Channels can render mid-agent tool runs in the streaming card. ~200 LOC in `crates/gateway/src/channel/adapter.rs` + `crates/channels/src/wire.rs` + ts-rs export. **SDK-first:** the SDK adds `Channel.onToolCallStarted` / `onToolCallCompleted` hooks so any channel can render tool-use UI without re-decoding the wire.
2. **Channel-flavoured slash commands** (gateway change) — extend `crates/gateway/src/channel/slash.rs::manifest()` to be per-channel-type, and add a `Frame::SlashInvoke { command, args }` push to the sidecar with `Frame::SlashReply` back. Or: keep slash purely client-rendered and have the sidecar special-case command-shaped inbounds before yielding (the cheap path).
3. **Tool exposure** (sidecar work, no gateway change). Two parallel surfaces:
   - **MCP-as-server in the sidecar** for the 28 OAPI tools — the sidecar runs an MCP stdio (or unix socket) endpoint alongside its WS connection; Aura's existing MCP client wires it in. 1:1 ports of openclaw's `src/tools/oapi/**` TS handlers, reusing `@larksuiteoapi/node-sdk` and the TypeBox schemas. **OAuth UAT storage uses the SDK secret-vault helper from §4.0.2** — same primitive any future channel reuses for its own per-user OAuth.
   - **HTTP proxy to Feishu's hosted MCP gateway** for the 3 doc tools (`feishu_fetch_doc`, `feishu_create_doc`, `feishu_update_doc`) — openclaw itself uses `mcp.larksuite.com` here because Feishu OAPI doesn't expose those operations. Port `src/tools/mcp/shared.ts:callMcpTool` as another tool the MCP-server in the sidecar exposes; it's just a typed wrapper around an outbound HTTP call.
4. **Skill bundle** — copy `skills/feishu-*` markdown into the workspace `skills/` directory.
5. **Diagnostics CLI + admin endpoint** — `bin/lark-diagnose` plus `/v1/admin/channels/lark/diagnose` for the WebUI. **Stays Lark-local** (under `channel-src/lark/src/diagnostic/`). Lark is the only channel with a config × permission × OAuth × network matrix that warrants a dedicated doctor; Telegram's health is `bot.getMe()` and doesn't need a framework. Promote into the SDK only when there's a second consumer.
6. **Ask-user-question tool** — implement as an MCP tool in the sidecar that posts a card and blocks on the callback.
7. **VC meeting-invited handler** — port; emits a synthetic inbound. Sidecar drops outbound on synthetic targets (mirrors `outbound.ts:175-178`).

## 5. What we deliberately won't port

- **OAPI tools 1:1 in Rust** — too many, too thin a wrapper around the SDK. MCP is the right boundary.
- **Plugin manifest / `openclaw.plugin.json`** — Aura's discovery is build-time enumeration of `channel-src/*`.
- **Hot config reload** — supervisor restarts are good enough.
- **Reaction-driven agent input** — would need a wire-level reaction frame; defer until there's a clear use case beyond ack.
- **Edit-message** as a first-class outbound — streaming cards already provide visible "agent is updating" UX without `Frame::EditMessage`.
- **Owner policy / app-owner fallback** — Aura's tool approval already provides equivalent gating in a cleaner model.

## 6. Decisions

Resolved (user-directed):

1. **Sidecar binary size — accepted.** The 800 KiB-1.5 MiB Lark sidecar bundle is fine. No special handling needed.
2. **`StartBot` token shape — extend the wire.** Add `Frame::StartBot.metadata: HashMap<String, String>` (additive, see §4.0.1). The Lark sidecar passes `app_id` as `token` and `(app_secret, encrypt_key, verification_token, base_url)` as metadata. Channel-agnostic: any future multi-secret bot benefits.
3. **UAT storage — SDK secret-vault primitive.** The SDK exposes a `Secrets` interface (`get`/`set`/`delete`/`list`) backed by Aura's `aura_security` vault, scoped per `(channel_type, bot_id)` so cross-bot leakage is impossible. The Lark sidecar uses this for per-user UATs keyed by `user_open_id`. See §4.0.2.
4. **CardKit v1 + v2 — both supported.** Don't drop v1. Builder branches on the presence of `schema: "2.0"`. v1 remains needed because the upstream agent may emit cards using v1-only elements (`action`, `button_group`, `note`, `div + lark_md`) and because some Lark tenants haven't enabled CardKit v2 yet. The streaming path uses v2 (CardKit's `cards.update`); v1 is for one-shot static cards.
5. **Synthetic-target outbound — silently dropped.** Mirror openclaw's behavior at `outbound.ts:175-178`. When the sidecar generates a synthetic inbound (VC meeting-invited, etc.) and the agent replies, the sidecar drops the outbound `Frame::Message` rather than DM the user out of context. The drop is logged at `debug` for diagnostics. No gateway change.
6. **MCP tool-call user identity — `_meta.auraSessionId` injection.** Aura's MCP client adds `_meta = { auraSessionId: "..." }` to every `tools/call` request (per the MCP spec's standard extension field). The sidecar's MCP server helper (§4.0.7 D) extracts it, decomposes into `(channel_type, bot_id, user_id)` via the same `composeAuraUserId` formula `BotChannel` uses (`bot.ts:362-371`), and exposes the tuple to the tool handler as `McpToolContext`. Cleaner than implicit-param injection (no signature pollution) and safer than `tools/setContext` notification (no concurrent-call race). Phase 3 work.
7. **User-driven abort — `Frame::AbortSession` (gateway change, sidecar-initiated).** OpenClaw's heuristic (`abort-detect.ts`) ports to the Lark sidecar; on detection, the sidecar emits `Frame::AbortSession { session_id, reason }` upstream and the gateway cancels the in-flight agent turn via a session-scoped `CancellationToken`. Phase 3, see §4.0.4. SDK exposes a `ChannelOutbound.abortSession(sessionId, reason)` helper; any future channel can use it.
8. **Lark SDK bundling under bun — try and verify on day 1.** No special architectural plan; the validation checklist in §4.1.B catches breakage early. If `@larksuiteoapi/node-sdk` 1.60.0 fails the bun-bundler smoke test, patch via `pnpm.patchedDependencies` or fall back to hand-rolled `fetch` calls. Do not paper over with workarounds discovered late.
9. **MCP transport between agent and sidecar — gateway-tunneled WS frame (`Frame::Mcp`), no HTTP path.** JSON-RPC envelopes ride existing wire; gateway forwards opaquely. Avoids introducing a third transport, keeps admin auth surface untouched, makes future MCP protocol versions transparent. Gated by capability `mcp_tunnel`. See §4.0.7 D.
10. **Post-abort session state — option (a) preserve session + agent-side `[aborted by user]` history annotation.** Cancelling a turn does not reset the session; the next user message resumes against the existing context. The agent's history-append path stamps `[aborted by user]` on whatever entry was last written for the cancelled turn (partial tool result, half-streamed text, or synthetic empty-step record), so the next turn's LLM input shows the model that the prior turn was interrupted rather than naturally completed. Implementation lives in `crates/agent/`, not the wire.
11. **Drop `PROTOCOL_VERSION` in favor of capability negotiation.** Old `protocol_version` strict-equality check (`runner.ts:296-323`) is replaced by an advertised `capabilities: Vec<String>` on `Frame::Register` and `Frame::RegisterAck`. Effective set is the intersection. Core legacy frames are always supported and never need a string. Old sidecars that send no `capabilities` field are treated as core-only and stay zero-touch compatible. See §4.0.6.
12. **Soft-deleted bots and bot-id reuse — bond secret lifecycle to `bot_row_id`, expose restore/fresh prompt at re-registration.** Secrets are keyed by `bot_row_id` (not the `(channel_type, bot_id)` string), so a soft-deleted bot's secrets stay intact and revive together when the row revives. Re-registering with the same `(channel_type, bot_id)` triggers an interactive choice: **restore** (clear `deleted_at` on existing row → secrets revive), **fresh** (hard-delete + cascade-purge → start from empty namespace), or **cancel**. The default everywhere is soft-delete; hard-delete only fires on explicit `f` confirmation. See §4.0.5.
13. **Lark domain selection — keep free-form `metadata.base_url`, not a `domain` enum.** String stays transport-neutral; sidecar maps the host to the SDK's `Domain` enum and derives WSS / OAuth endpoints by host substitution. New regional Lark deployments and private clouds ship without an SDK update. CLI/WebUI render a free-text field with three placeholder hints (`open.feishu.cn`, `open.larksuite.com`, `open.volc-feishu.cn`) rather than a closed dropdown. See §4.0.1.
14. **Diagnose runs over the WS, not a separate transport.** New `Frame::DiagnoseRequest` / `Frame::DiagnoseReply` (gateway → sidecar request, sidecar → gateway reply), gated by capability `diagnose`. Admin API `GET /v1/admin/channels/<type>/diagnose?bot_id=<id>` becomes the single entry point — `bin/lark-diagnose` and the WebUI both consume it. The CLI does not talk to the sidecar process directly, no socket files. SDK exposes `Channel.onDiagnoseRequested` hook + `formatReportMarkdown`/`formatReportCli` formatters. Phase 3, see §4.0.4 companion section.

Standing principles (also user-directed):

- **SDK-first.** Anything reusable across channels lives in `sdks/channel-ts/`, not in `channel-src/lark/`. The Phase 3 follow-ups (tool-use telemetry hooks, diagnostic skeleton) explicitly factor into the SDK.
- **Extending the gateway is fair game** when the SDK needs a primitive it doesn't have. All extensions stay additive on the wire so Telegram/Weixin keep working unchanged.

Clarification on OAPI vs MCP (see §3.7 for the long form):

- openclaw's 28 OAPI tools are **same-process plugin tools registered via `api.registerTool`, not MCP.** Only 3 doc tools route through Feishu's hosted MCP gateway as a workaround for OAPI gaps.
- For Aura, the recommended strategy is **MCP-as-server** in the sidecar (Aura's MCP client connects to a sidecar-hosted MCP server) — this is a different use of MCP than openclaw's. The 3 doc tools that openclaw itself proxies to Feishu's hosted MCP gateway should be ported as the same kind of HTTP proxy in Aura.

## 7. Quick reference — file pointers

OpenClaw upstream:
- Channel plugin entry: `origin_channel_plugin/openclaw-lark/src/channel/plugin.ts`
- Outbound contract: `origin_channel_plugin/openclaw-lark/src/messaging/outbound/outbound.ts:84`
- Streaming card: `origin_channel_plugin/openclaw-lark/src/card/streaming-card-controller.ts`
- OAuth UAT device flow: `origin_channel_plugin/openclaw-lark/src/core/device-flow.ts`
- OAPI tool registry: `origin_channel_plugin/openclaw-lark/src/tools/oapi/index.ts:46`
- Skills root: `origin_channel_plugin/openclaw-lark/skills/`

Aura SDK + gateway:
- Sidecar `Channel` interface: `sdks/channel-ts/src/channel.ts:134`
- BotChannel multiplexer: `sdks/channel-ts/src/bot.ts:430`
- Blob side-channel: `sdks/channel-ts/src/blobs.ts` + `crates/gateway/src/channel/blobs.rs`
- Wire frames (Rust ↔ TS via ts-rs): `sdks/channel-ts/src/generated/Frame.ts`
- Gateway slash manifest: `crates/gateway/src/channel/slash.rs:30`
- Sidecar supervisor: `crates/gateway/src/sidecar/supervisor.rs`
- Sidecar bundling: `crates/gateway/build.rs:368`
- Existing sidecars to model: `channel-src/telegram/src/{index,platform,approvals}.ts`, `channel-src/weixin/src/`
