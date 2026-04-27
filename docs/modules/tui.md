# tui - Built-in Terminal UI Channel

## Overview

`TuiAdapter` is the interactive channel for Aura, launched via `aura tui`. Bare `aura` prints `--help`; the TUI is an explicit opt-in to avoid surprising users with a full-screen app. It is implemented with [Ratatui] over a [Crossterm] async event stream and lives in its own crate (`crates/tui/`, published as `aura-tui`). It depends on `aura-channels` for shared type definitions (`SlashHandler`, `DashboardProvider`, `IncomingMessage`, `wire`) but nothing in `aura-channels` depends back on it.

The layout is intentionally minimal:

- **Scrollback pane** — rendered chat lines (user, assistant, system, approval).
- **Input line** — single-line editor with emacs-style cursor motions and a history ring.
- **Dashboard view** (modal) — opened by dashboard-style slash commands; returns to chat on `Esc`.

No status bar, no sidebars. Aura's operator surface lives in the CLI subcommands; the TUI only hosts the conversation and a handful of read-only views.

`aura tui` is a thin `/v1/channel-ws` client of `aura gateway`,
speaking the same WebSocket + MessagePack protocol every out-of-process
sidecar uses ([`aura_channels::wire`]). The TUI ships its own private
`WsClient` (`crates/tui/src/client/ws.rs`); the only public-SDK form
of this protocol is the TypeScript package under `sdks/channel-ts/`.
It does **not** take the
workspace singleton lock, does **not** build a manager graph, and does
**not** own a local `Router`. One workspace runs a long-lived `aura
gateway` as a service and opens `aura tui` against it — the gateway is
the only process that holds state. See [Boot flow](#boot-flow) for
endpoint and token resolution; see [`gateway.md`](./gateway.md) for the
server side.

[Ratatui]: https://docs.rs/ratatui
[Crossterm]: https://docs.rs/crossterm

## Views

### Chat

- Scrollback is a fixed-capacity ring (`SCROLLBACK_CAP = 5000`) of `ChatLine::{User,Assistant,System,Log,Approval}` entries.
- Assistant lines render each `ContentBlock`: text inline, and `Image`/`Audio`/`File` as a bracketed placeholder.
- Input history keeps up to `HISTORY_CAP = 500` non-empty submissions with trivial de-duplication of consecutive identical lines. The ring is **persistent**: see [Persistent input history](#persistent-input-history) below.
- `scroll_offset` is measured in rendered rows from the tail: `0` keeps the newest line pinned at the bottom; `PageUp`/`PageDown` grow or shrink the offset by 10.
- While the LLM is responding, an ephemeral streaming buffer is drawn immediately below the scrollback tail. Each `AgentOutput::Delta` extends the buffer; the final `AgentOutput::Message` replaces it with a persisted `ChatLine::Assistant` carrying the canonical `ContentBlock` list.

### Slash completion

- When the input starts with `/` and the cursor sits on the command token (no whitespace between `/` and cursor), a popup renders above the input box listing matching commands.
- Candidates come from `SlashHandler::commands()`; `CliSlashHandler` derives them from clap's subcommand tree, every user-invocable skill in `SkillRegistry` (name surfaces as `/<skill>`, description — prefixed with the `argument-hint` when present — surfaces as the popup hint), plus adapter-reserved tokens (`/quit`, `/exit`, `/clear`). Clap wins on name collisions so a workspace skill cannot shadow `/config` or `/skills`.
- `Tab` accepts the highlighted candidate, rewriting the prefix up to the next whitespace and appending a trailing space so arguments can follow. `Enter` submits without accepting the completion.
- `Up`/`Down` follow zsh/bash conventions, which also cleanly resolves the popup ambiguity:
    - **Empty input** — `Up`/`Down` walk the input-history ring. A slash popup never opens on an empty line, so there is no conflict.
    - **Non-empty input** — the user is actively drafting, so `Up`/`Down` drive the popup selection (or are inert if no popup is open). History is gated off until the draft clears or is submitted. This matches shell behavior: once you've typed content, pressing `Up` doesn't silently replace it with a history entry.
    - **While already walking history** — `Up`/`Down` keep walking even if the loaded entry makes the input non-empty and opens a popup. The first non-`Up`/`Down` key exits history mode and snaps the cursor back to the newest slot, so the entry stays on screen and further edits treat the ring as idle.

### Inline approval prompt

- Approval requests are rendered **inline in the scrollback** as a `ChatLine::Approval(ApprovalChatEntry)` entry — no overlay modal.
- When pending, the entry is expanded: tool name, resource accesses, params preview, and three selectable options (`Approve` / `Always approve` / `Deny`). The user navigates options with `Up`/`Down` (or `k`/`j`) and confirms with `Enter`, or presses a direct shortcut (`a`/`A`/`d`).
- After resolution the entry collapses to a single `aura>` line with the decision, tool name, and the first resource access detail — e.g. `aura> approved: Bash (echo hello)` or `aura> denied: Read (/etc/shadow)`. Normal input resumes immediately.
- Approvals originate on the gateway. The WS transport observes `Frame::ApprovalRequested` and mirrors the entry into a *local* `ApprovalQueue` so the existing TUI modal logic picks it up unchanged. The queue's resolver callback (installed by `GatewayTransport::new`) wraps the TUI's "approve/deny" decision in a `Frame::ResolveApproval` echoed back over the same socket, so the gateway-side gate unblocks. Inbound `Frame::ApprovalResolved` frames drop any stale local mirror — useful when a second frontend resolves the same entry.
- Dropping the responder (e.g. loop shutdown) still surfaces as `ApprovalDecision::Deny` on the local side; the gateway's own 5-minute timeout covers the server side.
- If multiple approvals are queued (concurrent tool calls), resolving one auto-surfaces the next into the scrollback.

### Dashboard

- Single-table layout: title, bold header row, equal-width columns, footer hint.
- Backed by a `DashboardSnapshot { title, columns, rows, footer }` value fetched from a `DashboardProvider`.
- Refresh (`r`) re-fetches on a background task; the snapshot swap is transactional (existing selection clamps to the new row count).
- Four built-in views map to `ViewKind::{Skills, Jobs, Sessions, Memory}`.
- `GatewayDashboardProvider` (`crates/tui/src/client/dashboard.rs`) fans out to `list_skills` / `list_jobs` / `list_sessions` / `list_memory` on the `GatewayClient` and shapes the result into a `DashboardSnapshot` client-side. There is deliberately no aggregate `/v1/dashboard` endpoint — per-kind REST routes already exist, and each snapshot function picks only the columns the TUI renders. HTTP errors degrade to an empty table with the message in the footer rather than exploding the TUI.

### Persistent input history

The input history ring survives across TUI sessions. Because users routinely
paste API keys, tokens, and other secrets into prompts, the ring is stored
encrypted at rest rather than in a plaintext history file — but the TUI
process itself never opens the vault. The gateway is the single writer,
and the TUI exchanges the ring over the channel WS like any other state.

- Wire protocol: two `aura_channels::wire::Frame` variants carry the history end-to-end. `Frame::HistorySnapshot { session_id, entries }` is pushed from the server once, right after `Frame::RegisterAck { ok: true }`, for session-scoped TUI clients only — sidecars never see it. `Frame::HistoryAppend { session_id, entry }` is sent by the TUI after every accepted submission.
- Gateway side: `aura_gateway::channel::TuiHistoryStore` (`crates/gateway/src/channel/history.rs`) wraps `Arc<SecretVault>` behind a `tokio::sync::Mutex`, so concurrent appends from multiple TUIs on the same gateway serialize into the same vault blob. It reads the current ring from the fixed key `aura.tui.input_history`, pushes the new entry (de-duping consecutive duplicates), caps the ring at 500 newest entries, and writes it back (see [`security.md`](./security.md)). Load failures or write errors are logged `warn!` and are non-fatal.
- TUI side: `WsTransport` (`crates/tui/src/client/ws.rs`, `transport.rs`) buffers the one-shot snapshot inside `initial_history: Mutex<Option<Vec<String>>>` during `connect_tui`. The main loop calls `ctx.input.take_history_snapshot().await` before the first `terminal.draw`, so the prior ring is populated when the input box first renders. `Action::Submit` calls `ctx.input.append_history(&entry)` in a detached `tokio::spawn` — no vault handle, no lock file, no local `aura-security` dependency.
- The TUI crate no longer carries an `InputHistoryStore` trait or any history builder on `TuiAdapter`. The store is implicit in the transport; tests that construct an adapter without a live gateway just get no snapshot and no-op appends.

## Slash Commands

### Dashboard shortcuts

Bare commands with no arguments open the matching dashboard view:

| Slash input | Outcome                                      |
| ----------- | -------------------------------------------- |
| `/skills`   | `SlashOutcome::OpenView(ViewKind::Skills)`   |
| `/jobs`     | `SlashOutcome::OpenView(ViewKind::Jobs)`     |
| `/sessions` | `SlashOutcome::OpenView(ViewKind::Sessions)` |
| `/memory`   | `SlashOutcome::OpenView(ViewKind::Memory)`   |

Anything with additional tokens (e.g. `/skills info foo`) dispatches to the corresponding gateway REST route and returns text that is appended to the chat scrollback.

### Gateway slash handler

`TuiSlashHandler` (`crates/tui/src/client/slash.rs`) is the TUI's `SlashHandler` implementation. The WS channel surface is narrow, so the handler only ships what it can actually satisfy: `/clear`, `/quit`, `/exit`, and the dashboard shortcuts (`/sessions`, `/jobs`, `/memory`, `/skills`). Tool approvals are handled through the modal keybindings (`a` / `A` / `d`), not a slash command. Any other `/<name>` falls through as `SlashOutcome::PassThrough` so skill invocations keep working without a client-side allow-list.

Skill names from the gateway's `/v1/skills` response are appended as `SlashOutcome::PassThrough` entries at startup — `TuiAdapter` forwards the raw line to the gateway for normal skill selection, same as before.

### Adapter-reserved tokens

- `/quit`, `/exit` — terminate the event loop.
- `/clear` — clear the chat scrollback (equivalent to `Ctrl-L`).

These are intercepted by `TuiAdapter` before `SlashHandler::handle` is called.

## Keybindings

### Global

| Key      | Action                                                                                                                                                              |
| -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Ctrl-C` | Clear input if non-empty, otherwise request shutdown                                                                                                                |
| `Ctrl-D` | Two-press exit (only when input is empty): the first press prints a hint; a second press within 2s exits. Any other key or a longer pause cancels the confirmation. |
| `Ctrl-L` | Clear chat scrollback                                                                                                                                               |
| `Alt-M`  | Toggle terminal mouse capture. Off restores native drag-to-select across terminals (wheel-scroll stops working until re-enabled).                                   |

### Chat

| Key                                              | Action                                                                       |
| ------------------------------------------------ | ---------------------------------------------------------------------------- |
| `Enter`                                          | Submit input (or run slash command)                                          |
| `Shift-Enter` / `Alt-Enter`                      | Insert newline in the input (see terminal support below)                     |
| `Up`/`Down`                                      | Move cursor within a multi-line draft; on the first/last line walks history  |
| `PageUp`/`PageDown`                              | Scroll scrollback                                                            |
| `Home`/`End`/`Left`/`Right`/`Backspace`/`Delete` | Standard cursor edits                                                        |
| Any printable character                          | Insert at cursor                                                             |

`Shift-Enter` requires the Kitty keyboard protocol (`DISAMBIGUATE_ESCAPE_CODES`), which `run_loop` pushes at startup and pops on teardown. Supported by Kitty, WezTerm, Foot, recent Alacritty, iTerm2, and Ghostty; terminals without it fall back to treating Shift-Enter as plain Enter, so `Alt-Enter` is offered as a universal alternative that does not depend on the protocol. The input box grows up to `INPUT_MAX_ROWS = 10` rows; beyond that the caret may clip.

### Dashboard

| Key                 | Action               |
| ------------------- | -------------------- |
| `Esc`, `q`          | Exit back to chat    |
| `Up`/`Down`         | Move row selection   |
| `PageUp`/`PageDown` | Page selection       |
| `r`                 | Refresh the snapshot |

### Inline approval prompt

When an approval is pending, keymap translation short-circuits — chat edits, scroll, and dashboard keys are all suppressed until the entry is resolved.

| Key           | Action                                                      |
| ------------- | ----------------------------------------------------------- |
| `Up`/`k`      | Select previous option                                      |
| `Down`/`j`    | Select next option                                          |
| `Enter`       | Confirm the highlighted option                              |
| `a`           | Approve once (direct shortcut)                              |
| `A`           | Approve always (persists touched resources to session)      |
| `d`, `Esc`    | Deny (tool call surfaces as `ToolError::Denied`)            |

## Boot flow

`src/tui_cmd.rs::run` drives the boot. There is no call to
`singleton::acquire`, no `build_managers`, and no `wire_router` —
none of those exist on the TUI side.

1. **Resolve the gateway endpoint** — read
   `http://{config.gateway.bind_address}:{port}` straight from
   workspace config (with `0.0.0.0` rewritten to `127.0.0.1` for
   outbound dialling). No CLI or env overrides: the TUI and the
   gateway share the same workspace, so the same config is
   authoritative for both.
2. **Resolve the bearer token** — `GatewayToken::new(vault).get()`
   against the workspace secret vault. The vault read uses
   `runtime::build_secret_vault`, which only opens the libsql store —
   no singleton lock, no manager graph.
3. **`GET /healthz` probe**. On failure the TUI aborts with a concrete
   block telling the operator how to fix it:

   ```
   error: no aura gateway reachable at <url>
     - start it with:       aura gateway start
     - or install service:  aura gateway install && aura gateway enable
     - (dev only) retry with --dev-auto-gateway to spawn one inline
     (underlying error: ...)
   ```
4. **Session resolution** — `--session <id>` pins an explicit id (for
   resuming a workspace session across restarts); without the flag the
   TUI mints a fresh `tui-{uuid}` client-side and pins it via
   `with_session_id`. The gateway's router auto-creates the session on
   the first inbound frame via `SessionManager::get_or_create`, so no
   REST round-trip is needed to provision one.
5. **Connect the WS channel** — read the per-start TUI token from
   the secret vault under the key
   `aura_gateway::TUI_TOKEN_VAULT_KEY` ("gateway.tui_token"),
   dial `/v1/channel-ws` with the shared `x-aura-channel-token`
   header carrying that value, send `Frame::Register { channel_type:
   "tui", protocol_version, token: "", session_id:
   Some(<this-process-session>) }`, and wait for `RegisterAck { ok:
   true }`. Pinning the TUI's session into the
   handshake is what lets multiple `aura tui` processes coexist on
   the same gateway — the `ChannelRegistry` routes events for that
   session to this connection only. The gateway rejects a TUI
   handshake without a `session_id` (it's only optional for sidecars,
   which register type-level). The TUI's `GatewayTransport` owns the
   connection and surfaces inbound
   `Frame::{Message, Delta, Notice, ApprovalRequested, ApprovalResolved}`
   as `TransportEvent`s.
6. **Wire the gateway providers** — construct `GatewayTransport`,
   `GatewaySlashHandler` (seeded with the skill catalog from
   `/v1/skills`), and `GatewayDashboardProvider`. Attach them to
   `TuiAdapter` via the `with_transport`, `with_slash_handler`, and
   `with_dashboard_provider` builders. Input history is delivered over
   the WS itself (see [Persistent input history](#persistent-input-history)),
   so no history store is wired in.
7. **Start the adapter**. There is no local `ChannelRegistry` and no
   cron trigger receiver: the TUI has no router. User input is
   framed as `Frame::Message` and sent through the transport; the
   gateway registers the connection on its side.
8. **Graceful shutdown** — a stripped-down `install_signal_handler`
   wires SIGINT/SIGTERM into the adapter's `ShutdownSignal`. A 5 s
   `force_exit_watchdog` bounds teardown so a stalled WS pump never
   pins the process.

### `--dev-auto-gateway`

Debug builds add a `--dev-auto-gateway` flag. When `/healthz` is
unreachable and the flag is set, `src/tui_cmd.rs::dev_auto` spawns
`Command::new(std::env::current_exe()).args(["gateway", "start"])`
as a subprocess, polls `/healthz` with exponential backoff (100 ms →
1 s, 15 s deadline), and returns an RAII guard that sends SIGKILL on
drop. A loud banner prints before the alternate screen takes over so
the operator sees that a background gateway was spawned. The flag is
gated by `#[cfg(debug_assertions)]` so `cargo build --release` cannot
compile it in.

## Architecture

### Event sources

The loop multiplexes three sources with `tokio::select!`:

1. **Shutdown** — `Arc<Notify>` toggled by `TuiAdapter::stop` or the WS pump's teardown guard.
2. **Terminal input** — a `crossterm::event::EventStream`. Raw reads are required because the terminal is in raw-mode + alternate screen; a `tokio::io::stdin` reader would fight crossterm for `/dev/tty`.
3. **Internal events** — `AppEvent` sent by the `WsTransport` pump (streaming deltas, responses, approval events) and by the background dashboard-fetch task.

### Transport

`WsTransport` (`crates/tui/src/client/transport.rs`) is the single
concrete transport used by the TUI. The adapter holds an
`Arc<WsTransport>` and calls three methods on it:

- `submit(msg)` — flatten an `IncomingMessage`'s text blocks and
  send a `Frame::Message` over the WS.
- `subscribe(session_id) -> TransportEventStream` — decode inbound
  frames from the same WS and translate them into
  `TransportEvent::{StreamDelta, Response, Notice, ApprovalRequested, ApprovalResolved}`.
- `approval_queue()` — returns the transport's local `ApprovalQueue`.
  On construction, `WsTransport::connect` installs a resolver so
  `queue.resolve_head(decision)` also sends a
  `Frame::ResolveApproval { call_id, decision }` back over the WS.
  Without that hook the local modal would "work" but the server-side
  gate would time out.

The old `TuiTransport` trait was collapsed away — there was only
ever one production impl, no mocks used it, and the wire path always
went through the channel WS socket.

### Output path

All output flows through the `subscribe` stream. The loop translates
each `TransportEvent` onto an `AppEvent` variant so rendering code
does not need to know about the transport:

- `StreamDelta(text)` → `AppEvent::StreamDelta(String)`. Appended to `AppState.streaming`, redrawn live.
- `Response(blocks)` → `AppEvent::Outgoing(Vec<ContentBlock>)`. Clears `AppState.streaming`; pushes `ChatLine::Assistant`.
- `Notice { level, text }` → `AppEvent::Log(LogRecord { level, target: "agent", message })`. Reuses the log surface.
- `ApprovalRequested` / `ApprovalResolved` — see [Inline approval prompt](#inline-approval-prompt).

Single-consumer ordering on the mpsc keeps delta/response ordering correct as WS frames are fed in.

### Raw-mode discipline

- Entry (`run_loop`): `enable_raw_mode()` + `EnterAlternateScreen` + `Terminal::new`.
- Teardown: `disable_raw_mode()` + `LeaveAlternateScreen` + `DisableMouseCapture`, best-effort, logged on failure.
- Panic hook: installed once via `OnceLock`, calls the same restoration sequence before delegating to the previous hook. Without it a panic would leave the terminal unusable.

### Logging in chat mode

Writing `tracing` records to stdout while ratatui owns raw mode corrupts the frame. Chat mode therefore uses a two-layer subscriber:

- **File layer** — `tracing_appender::rolling::daily("<workspace>/logs", "aura.log")` wrapped in a non-blocking writer. A `WorkerGuard` held on the stack of `main` flushes pending lines on shutdown.
- **TUI echo layer** — `TuiLogLayer` (`src/tui_log.rs`) filters events to `WARN` and `ERROR`, extracts `message` + structured fields, and forwards them through `TuiLogSink::emit` as `AppEvent::Log(LogRecord)`. The event loop pushes the record onto the scrollback as a coloured line (`warn` yellow, `error` red, with the event `target` in grey).

The sink is plumbed into the layer via `Arc<OnceLock<TuiLogSink>>`: `init_tracing` allocates the cell, then `main` sets it from `TuiAdapter::log_sink()` after the adapter is constructed. Events emitted before the cell is filled still reach the file layer; they simply don't appear in the TUI.

Argv mode keeps the old stdout layer — one-shot commands don't own the terminal, so normal formatting works.

## Session IDs and `ChannelType`

- `TuiAdapter` stamps every message with `session_id = format!("tui-{uuid}")` and `ChannelType::Tui`.
- Older persisted sessions recorded as `ChannelType::Cli` deserialize transparently thanks to `#[serde(alias = "cli")]` on the `Tui` variant (`crates/session/src/types.rs`). No migration step is needed.
- The `cron` CLI accepts both `tui` and `cli` as channel identifiers so existing cron entries keep working.

## Constraints

- Each `AgentOutput::Message` delivered to the TUI produces exactly one persisted chat line. `AgentOutput::Delta` chunks may precede it, but they are ephemeral — the final message supersedes whatever was streamed and is the canonical record.
- Renderer state (`AppState`) is mutated only on the event-loop task. External code uses the mpsc event channel; there is no shared `Mutex<AppState>`.
- Input/state mutation in `app.rs` and key translation in `keymap.rs` are pure — unit tests exercise them without a terminal. Renderer tests use Ratatui's `TestBackend` when needed.
- Dashboard providers must not block; all built-in providers are `async` and call manager methods directly on `tokio`.

## Collaboration

| Module     | Role                                                                                         |
| ---------- | -------------------------------------------------------------------------------------------- |
| `model`    | `ContentBlock` for rendering assistant messages                                              |
| `session`  | `ChannelType::Tui`, `User` used when constructing `IncomingMessage`                          |
| `gateway`  | Server-side owner of sessions, approvals, outbound frame fan-out, and the vault-encrypted input-history store. The TUI talks to it over `/v1/channel-ws` (WebSocket + MessagePack) |
| `channels` | Trait definitions only: `SlashHandler`, `SlashOutcome`, `ViewKind`, `DashboardProvider`, `DashboardSnapshot`, `IncomingMessage`, `NoticeLevel`, `ChannelError`. No TUI code. |

## Verification

```bash
cargo test -p aura-tui             # keymap, AppState, transport, slash, frame codec, WS client
cargo run -- gateway start         # terminal A: long-lived backend
cargo run -- tui                   # terminal B: WS+MessagePack client
```

Manual smoke:

- `aura tui` against a running `aura gateway` opens the Ratatui UI and the chat pane is live. Bare `aura` prints help instead.
- With no gateway reachable, `aura tui` exits with the concrete "no aura gateway reachable at <url>" block.
- `cargo run -- tui --dev-auto-gateway` (debug build) in a fresh workspace with no gateway running spawns the backend inline, prints the banner, and connects.
- Typing + `Enter` appends a user line and sends a `Frame::Message` to the gateway; inbound `Frame::Delta`s render live, the final `Frame::Message` replaces the streaming buffer.
- `/skills` opens the skills dashboard (fan-out to `/v1/skills`); `r` refreshes; `Esc` returns to chat.
- A tool call that requires approval queues an inline prompt; `a` resolves it, the gateway-side gate unblocks, and the tool result renders.
- Killing the gateway mid-session surfaces the next inbound frame as an error notice rather than crashing the TUI.
- `Ctrl-C` on an empty input line exits cleanly with the terminal restored.
