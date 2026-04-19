# tui - Built-in Terminal UI Channel

## Overview

`TuiAdapter` is the interactive channel for Aura, launched via `aura tui`. Bare `aura` prints `--help`; the TUI is an explicit opt-in to avoid surprising users with a full-screen app. It is implemented with [Ratatui] over a [Crossterm] async event stream and lives in its own crate (`crates/tui/`, published as `aura-tui`). It depends on `aura-channels` for shared trait definitions (`SlashHandler`, `DashboardProvider`, `IncomingMessage`) but nothing in `aura-channels` depends back on it.

The layout is intentionally minimal:

- **Scrollback pane** — rendered chat lines (user, assistant, system, approval).
- **Input line** — single-line editor with emacs-style cursor motions and a history ring.
- **Dashboard view** (modal) — opened by dashboard-style slash commands; returns to chat on `Esc`.

No status bar, no sidebars. Aura's operator surface lives in the CLI subcommands; the TUI only hosts the conversation and a handful of read-only views.

`aura tui` is a thin HTTP+SSE client of `aura gateway`. It does
**not** take the workspace singleton lock, does **not** build a
manager graph, and does **not** own a local `Router`. One workspace
runs a long-lived `aura gateway` as a service and opens `aura tui`
against it — the gateway is the only process that holds state. See
[Boot flow](#boot-flow) for endpoint and token resolution; see
[`gateway.md`](./gateway.md) for the server side.

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
- `Up`/`Down` cycle the selection; `Tab` accepts the highlighted candidate, rewriting the prefix up to the next whitespace and appending a trailing space so arguments can follow. `Enter` submits without accepting the completion.

### Inline approval prompt

- Approval requests are rendered **inline in the scrollback** as a `ChatLine::Approval(ApprovalChatEntry)` entry — no overlay modal.
- When pending, the entry is expanded: tool name, resource accesses, params preview, and three selectable options (`Approve` / `Always approve` / `Deny`). The user navigates options with `Up`/`Down` (or `k`/`j`) and confirms with `Enter`, or presses a direct shortcut (`a`/`A`/`d`).
- After resolution the entry collapses to a single `aura>` line with the decision, tool name, and the first resource access detail — e.g. `aura> approved: Bash (echo hello)` or `aura> denied: Read (/etc/shadow)`. Normal input resumes immediately.
- Approvals originate on the gateway. `GatewayTransport` subscribes to `/v1/approvals/stream`; on an `ApprovalEvent::Added` frame it mirrors the entry into a *local* `ApprovalQueue` so the existing TUI modal logic picks it up unchanged. The queue's resolver callback (installed by `GatewayTransport::new`) wraps the TUI's "approve/deny" decision in a `POST /v1/approvals/:call_id` that runs on a background `tokio::spawn`, so the gateway-side gate unblocks. Gateway-authored `ApprovalEvent::Resolved` frames drop any stale local mirror — useful when a second frontend resolves the same entry.
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
encrypted at rest rather than in a plaintext history file.

- The trait `aura_tui::InputHistoryStore` (`crates/tui/src/history.rs`) defines the contract: `load() -> Vec<String>` runs once at start-of-loop; `save(&[String])` runs after every accepted submission.
- The TUI itself does not depend on `aura-security`. The production wiring is `aura_cli::CliInputHistoryStore`, which wraps `Arc<SecretVault>` and serializes the chronological ring as JSON under the fixed key `aura.tui.input_history` (see [`security.md`](./security.md)). Plaintext only ever exists in `AppState.history` — on disk it is AES-256-GCM ciphertext under the same master key the rest of the vault uses.
- Load happens before the first `terminal.draw`, so the prior ring is already populated when the input box first renders.
- Save runs from `Action::Submit` immediately after `take_input()`, in a detached `tokio::spawn`. The full history snapshot is persisted every time (no diffing); the disk/encryption latency therefore never blocks the key-handling path.
- Failures (missing master key, corrupt JSON, libsql write error) log a `warn!` and are non-fatal: load failures yield an empty ring; save failures drop the persistence for that submission only. The TUI continues to function with the in-memory history.
- `TuiAdapter::with_input_history` is optional. Tests construct adapters without a store; in-memory history then behaves exactly as before.

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

`GatewaySlashHandler` (`crates/tui/src/client/slash.rs`) is the TUI's `SlashHandler` implementation. It ships an **allow-list** of commands that have an HTTP equivalent — bare dashboard openers, `/sessions`, `/jobs`, `/memory`, `/skills`, `/tools`, `/channels`, `/status`, `/llm`, `/trace`, `/config {get,set,unset}`, `/approve`, `/deny`, `/clear`, `/quit`, `/exit`. Anything not on the list (workspace-only `/doctor`, host-local `/workspace`) is hidden from completion and produces an "unknown command" error on submit, rather than half-working against the gateway.

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
4. **Session resolution** — `--session <id>` resumes an existing
   session via `GET /v1/sessions/:id` (surfaces typos early); with no
   flag, `POST /v1/sessions` mints a fresh one. The id is pinned into
   the `TuiAdapter` via `with_session_id`.
5. **Wire the gateway providers** — construct `GatewayClient`,
   `GatewayTransport`, `GatewaySlashHandler` (seeded with the skill
   catalog from `/v1/skills`), `GatewayDashboardProvider`, and the
   vault-backed `CliInputHistoryStore`. Attach them to `TuiAdapter`
   via the `with_transport`, `with_slash_handler`,
   `with_dashboard_provider`, and `with_input_history` builders.
6. **Start the adapter**. There is no `ChannelRegistry` registration
   and no cron trigger receiver: the TUI has no router. The
   `incoming` mpsc passed to `ChannelAdapter::start` is a dead-letter
   — the transport routes user input directly to the gateway.
7. **Graceful shutdown** — a stripped-down `install_signal_handler`
   wires SIGINT/SIGTERM into the adapter's `ShutdownSignal`. A 5 s
   `force_exit_watchdog` bounds teardown so a stalled SSE pump never
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

1. **Shutdown** — `Arc<Notify>` toggled by `TuiAdapter::stop` (invoked by the `ChannelRegistry` during teardown).
2. **Terminal input** — a `crossterm::event::EventStream`. Raw reads are required because the terminal is in raw-mode + alternate screen; a `tokio::io::stdin` reader would fight crossterm for `/dev/tty`.
3. **Internal events** — `AppEvent` sent by the `TuiTransport` pump (SSE deltas, responses, approval events) and by the background dashboard-fetch task.

### Transport

`TuiTransport` (`crates/tui/src/transport.rs`) abstracts the
outbound message path from the inbound event source. The adapter
holds an `Arc<dyn TuiTransport>` and calls three things on it:

- `submit(msg)` — handle a user-typed `IncomingMessage`.
  `GatewayTransport` flattens the text blocks and `POST`s to
  `/v1/sessions/:id/messages`.
- `subscribe(session_id) -> BoxStream<Result<TransportEvent>>` —
  opens both the session stream (`/v1/sessions/:id/stream`) and the
  gateway-wide approval stream (`/v1/approvals/stream`) and merges
  them via `futures::stream::select`. SSE frames are decoded into
  `TransportEvent::{StreamDelta, Response, Notice, ApprovalRequested, ApprovalResolved}`.
- `approval_queue()` — returns the transport's local `ApprovalQueue`.
  On construction, `GatewayTransport::new` installs a resolver so
  `queue.resolve_head(decision)` also fires a background
  `POST /v1/approvals/:id` back to the gateway. Without that hook the
  local modal would "work" but the server-side gate would time out.

### Output path

All output flows through the `subscribe` stream. The loop translates
each `TransportEvent` onto an `AppEvent` variant so rendering code
does not need to know about the transport:

- `StreamDelta(text)` → `AppEvent::StreamDelta(String)`. Appended to `AppState.streaming`, redrawn live.
- `Response(blocks)` → `AppEvent::Outgoing(Vec<ContentBlock>)`. Clears `AppState.streaming`; pushes `ChatLine::Assistant`.
- `Notice { level, text }` → `AppEvent::Log(LogRecord { level, target: "agent", message })`. Reuses the log surface.
- `ApprovalRequested` / `ApprovalResolved` — see [Inline approval prompt](#inline-approval-prompt).

Single-consumer ordering on the mpsc keeps delta/response ordering correct as SSE frames are fed in.

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

- Each `send_response` produces exactly one persisted chat line. Deltas may precede it, but they are ephemeral — the final message supersedes whatever was streamed and is the canonical record.
- Renderer state (`AppState`) is mutated only on the event-loop task. External code uses the mpsc event channel; there is no shared `Mutex<AppState>`.
- Input/state mutation in `app.rs` and key translation in `keymap.rs` are pure — unit tests exercise them without a terminal. Renderer tests use Ratatui's `TestBackend` when needed.
- Dashboard providers must not block; all built-in providers are `async` and call manager methods directly on `tokio`.

## Collaboration

| Module     | Role                                                                                         |
| ---------- | -------------------------------------------------------------------------------------------- |
| `model`    | `ContentBlock` for rendering assistant messages                                              |
| `session`  | `ChannelType::Tui`, `User` used when constructing `IncomingMessage`                          |
| `cli`      | `CliInputHistoryStore` for vault-encrypted input history (host-local; works without the gateway) |
| `gateway`  | Server-side owner of sessions, approvals, and SSE fan-out. The TUI talks to it over HTTP+SSE |
| `channels` | Trait definitions only: `SlashHandler`, `SlashOutcome`, `ViewKind`, `DashboardProvider`, `DashboardSnapshot`, `IncomingMessage`, `NoticeLevel`, `ChannelError`. No TUI code. |

## Verification

```bash
cargo test -p aura-tui             # keymap, AppState, transport, slash, SSE parser, HTTP client
cargo run -- gateway start         # terminal A: long-lived backend
cargo run -- tui                   # terminal B: HTTP+SSE client
```

Manual smoke:

- `aura tui` against a running `aura gateway` opens the Ratatui UI and the chat pane is live. Bare `aura` prints help instead.
- With no gateway reachable, `aura tui` exits with the concrete "no aura gateway reachable at <url>" block.
- `cargo run -- tui --dev-auto-gateway` (debug build) in a fresh workspace with no gateway running spawns the backend inline, prints the banner, and connects.
- Typing + `Enter` appends a user line and `POST`s to the gateway; SSE deltas render live, the final response replaces the streaming buffer.
- `/skills` opens the skills dashboard (fan-out to `/v1/skills`); `r` refreshes; `Esc` returns to chat.
- A tool call that requires approval queues an inline prompt; `a` resolves it, the gateway-side gate unblocks, and the tool result renders.
- Killing the gateway mid-session surfaces the next SSE frame as an error notice rather than crashing the TUI.
- `Ctrl-C` on an empty input line exits cleanly with the terminal restored.
