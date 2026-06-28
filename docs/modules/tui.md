# tui - Built-in Terminal UI Channel

## Overview

`TuiAdapter` is the interactive channel for Baybo, launched via `baybo tui`. Bare `baybo` prints `--help`; the TUI is an explicit opt-in to avoid surprising users with a full-screen app. It is implemented with [Ratatui] over a [Crossterm] async event stream and lives in its own crate (`crates/tui/`, published as `baybo-tui`). It depends on `baybo-channels` for shared type definitions (`SlashHandler`, `DashboardProvider`, `IncomingMessage`, `wire`) but nothing in `baybo-channels` depends back on it.

The layout is intentionally minimal:

- **Scrollback pane** — rendered chat lines (user, assistant, system, approval).
- **Input line** — editor with emacs-style cursor motions, a history ring, and a compact current-model footer below it when the gateway reports one.
- **Dashboard view** (modal) — opened by dashboard-style slash commands; returns to chat on `Esc`.

No sidebars. Baybo's operator surface lives in the CLI subcommands; the TUI only hosts the conversation, the current-model footer, and a handful of read-only views.

`baybo tui` is a thin `/v1/channel-ws` client of `baybo gateway`,
speaking the same WebSocket + MessagePack protocol every out-of-process
sidecar uses ([`baybo_channels::wire`]). The TUI ships its own private
`WsClient` (`crates/tui/src/client/ws.rs`); the only public-SDK form
of this protocol is the TypeScript package under `sidecars/sdk/channel-ts/`.
It does **not** take the
workspace singleton lock, does **not** build a manager graph, and does
**not** own a local `Router`. One workspace runs a long-lived `baybo
gateway` as a service and opens `baybo tui` against it — the gateway is
the only process that holds state. See [Boot flow](#boot-flow) for
endpoint and token resolution; see [`gateway.md`](./gateway.md) for the
server side.

[Ratatui]: https://docs.rs/ratatui
[Crossterm]: https://docs.rs/crossterm

## Views

### Chat

- The live chat history lives in the terminal's **native** scrollback, not an in-memory display buffer: completed lines (user / assistant / system / log / resolved approval) are committed straight in via `Terminal::insert_before`, and the TUI owns only a small inline live region (the animated working indicator, any queued-message lines, the pending-approval prompt, the input box, and the optional current-model footer — see [Working indicator & mid-turn steering](#working-indicator--mid-turn-steering)). A **bounded** `TranscriptBlock` log on `AppState` (capped at `TRANSCRIPT_MAX_LINES`, oldest blocks evicted) shadows what was committed, used **only** to re-render the conversation on a resize refresh (below) — never as the display source. The commit helpers record into it as they `insert_before`; width-dependent blocks (the user bar) store their **source text** so replay re-renders them at the new width, everything else stores rendered lines that re-wrap.
- **Resize = refresh, then replay.** A terminal **width** change reflows the native scrollback and leaves stale fragments from the old full-width live region (input box / message bars) that the inline viewport can't precisely erase (no post-reflow layout is exposed by ratatui/crossterm). Rather than live with the ghosting, a settled resize burst (coalesced by `RESIZE_COALESCE_WINDOW`) triggers `rebuild_chat_terminal_after_resize`, which **clears the screen + scrollback, reprints the banner, then replays the `TranscriptBlock` log** (`replay_transcript`) so the conversation re-renders cleanly at the new width. Replay goes through the same commit helpers, so the leading-separator spacing is reproduced exactly. `AppState.session_id` is carried so the banner can be reprinted without a transport handle. What does **not** come back: history beyond the transcript cap, and any shell output above the original launch point (the entry clear wiped it). `/clear`, `/new`, and `Ctrl-L` call `clear_transcript`, so a later resize doesn't resurrect intentionally-cleared history.
- Assistant lines render each `ContentBlock`: text inline, and `Image`/`Audio`/`File` as a bracketed placeholder.
- Input history keeps up to `HISTORY_CAP = 500` non-empty submissions with trivial de-duplication of consecutive identical lines. The ring is **persistent**: see [Persistent input history](#persistent-input-history) below.
- Chat scrolling is the terminal's own (mouse wheel / the emulator's scrollback); the TUI keeps no scroll offset for chat. `PageUp`/`PageDown` page the **dashboard** table only (`DashboardPageUp`/`DashboardPageDown`, ±10 rows).
- While the LLM is responding, an ephemeral streaming buffer is drawn immediately below the scrollback tail. Each `AgentEvent::AnswerDelta` extends it; complete lines are committed to the terminal scrollback (`insert_before`) as they arrive, leaving only the trailing partial in the buffer. The agent loop streams deltas on **every** iteration, so a final answer that lands after tool calls still streams.
- The final `AgentEvent::Message` does **not** replace the streamed text — those lines are already committed. `finalize_stream` (via the pure `finalize_lines`) commits the trailing partial plus any non-text extras the stream didn't carry (e.g. the CronCreate hint). When the response **never streamed** — the non-streaming delivery path, where the agent loop ran with `delta_tx = None` (cron fires, subagent-notification turns) and only a final Message arrives — it renders the full message body from the blocks so the text isn't dropped.
- **Message styling.** There are no `you>` / `baybo>` prefixes. A user message is a **full-width highlighted bar** — bright white background, black text (not `REVERSED`, which renders muted on many terminals) — with a `> ` quote leader, the row padded with spaces (via `render_user_lines(text, width)`) so the background spans the whole line. Everything else leads with a same-size `●` dot: an assistant answer (**pale `●`**, continuation lines indented under it), the tool line (cyan `●`), the resolved-approval summary, and the `cooked for` footer using the answer text foreground.
- **Spacing (leading separators, no trailing blanks).** Blocks are separated by **exactly one** blank row via a *leading* separator: `begin_block(kind)` emits one blank when the block kind changes (always for `Other`; same-kind `Answer`/`Tool` lines stack tight as one block). It's deduped against `AppState.last_row_blank` so it never doubles, and there are **no** trailing blanks — the reserved working row (below) separates the last block from the input, and the next block adds its own leading separator. `AppState.last_block: Option<BlockKind>` tracks the kind.
- **Turn-done footer.** When a turn (job) finishes, `finalize_stream` commits a `● cooked for <elapsed>` stamp (same foreground as answer text) with its own deduped separator above it, so it sits one blank below the answer/tool block. It's the turn's wall-clock from when its `working_since` clock armed; only turns the TUI actually clocked get it (a cancelled `/stop` turn or a non-streaming cron delivery does not).
- **Model footer.** At startup `baybo tui` queries the gateway admin endpoint `GET /v1/llm` with the local admin bearer token and passes the active `provider/model_id` label into `TuiAdapter::with_model_label`. When present, `AppState.model_label` adds one dim, slightly indented row below the input box (`chat::model_footer_height`); if the query fails, the footer is omitted and chat continues normally.
- **Turn-progress events** (see [`docs/turn-progress-events.md`](../turn-progress-events.md)) interleave with the answer, so a turn reads as `● answer → ● Tool(label) → ⎿ summary → ● answer`. A tool's scrollback block is **deferred and committed as one unit on completion**: `ToolStarted` records a `RunningTool` (rendered live in the working zone — see [Working indicator & mid-turn steering](#working-indicator--mid-turn-steering)) but commits nothing; `ToolCompleted` (matched by `call_id`) pops it and commits the whole `Tool` block at once — the `● tool(label)` head, any resolved-approval line buffered mid-call, then the `⎿ summary` (`chat::tool_completed_block`). This is the fix for the **concurrent-tool ordering bug**: the agent emits *every* `ToolStarted` for a response up front, runs the calls in parallel, then emits *every* `ToolCompleted` — so committing the `●` head at start would strand all the `⎿` results below all the heads in append-only native scrollback (`insert_before` can't insert under an earlier line). Deferring keeps each result directly under its own tool. A `ToolCompleted` whose `call_id` matches **no** tracked `RunningTool` is **dropped**, not rendered headless — after `/stop`, `reset_working` clears the running set, but a tool that observed cancellation can still emit a late completion, and committing it would strand a stray `⎿` result in the now-idle scrollback. The final `Message`'s `ToolUse` blocks are **not** a duplicate source — `render_block` only renders the CronCreate hint, never general tool calls.
- **Reasoning is not rendered.** `Frame::Reasoning` ("thinking") chunks are dropped by the WS pump (`map_frame` returns `None`); the TUI shows an animated *working* indicator in the live region instead of the model's reasoning trace. The wire frame still flows to other channels (e.g. the web UI). See [Working indicator & mid-turn steering](#working-indicator--mid-turn-steering).

### Working indicator & mid-turn steering

While a turn is in flight the live region shows an **animated working zone**. The bottom row is always the **`Cooking…` indicator** — a **colour-pulsing `●`** (same glyph/size as the answer dot — equal-width, so it never reflows; the pulse cycles the dot's colour through `WORKING_PULSE` on each tick, driven by **repaint** rather than terminal `SLOW_BLINK`, which many terminals don't render), then `Cooking…`, the overall elapsed seconds, and the `/stop to interrupt` hint (`working_indicator_line`). When tools are running, each **in-flight tool** stacks **above** that indicator as a name-only `● tool(label)` row (`running_tool_line`, dots pulsing in unison so the list reads as alive); the indicator owns the elapsed + `/stop`, so the tool rows stay name-only and don't duplicate them. These tool rows are *live*, not scrollback: each tool's `●`/`⎿` block commits to scrollback only when it completes (see [Chat](#chat)). The list is sourced from `AppState::running_tools()`.

It's driven by a `tokio::time::interval` (`WORKING_TICK`, ~300 ms — each tick advances the pulse and refreshes the elapsed counter) select arm gated on `AppState::show_working()` (a turn is active and no approval is pending), so an idle TUI never wakes on it. The clock arms when a turn-initiating message is dispatched (`note_response_pending`) or on the first streaming/tool event of any turn (`ensure_working_clock`), and disarms when the last outstanding response lands. During a pending approval the zone is hidden — the agent is blocked on the user, not working.

The zone is sized by `working_zone_height` = one **blank** spacer row (always) + `working_content_height` (0 when idle, else `running_tools().len() + 1` — one row per in-flight tool plus the always-present `Cooking…` indicator row). The blank spacer gives the content one line of separation from the last scrollback message without leaving two idle blank rows above the input box; the content is bottom-aligned within the zone so the spacer stays on top.

Typing while the agent is busy no longer parks the message in a hidden queue with an `input · N queued` title. Instead:

- A **plain message** is dispatched to the agent **immediately** — mid-turn steering, so the agent folds it into the running turn once its current tool **batch** finishes (see [`docs/mid-turn-user-interjection.md`](../mid-turn-user-interjection.md)) — and shown as a dim `↳ <text>` line under the working indicator. The agent emits every `ToolStarted` for a batch up front, runs the calls in parallel, then emits every `ToolCompleted`, and only drains the mailbox once that batch is done — so the line is held until the **last** in-flight tool completes (`running_tools()` empties), then committed to scrollback as the full-width highlighted `> ` user bar below that batch's results. Committing it after an earlier sibling completes would place the steer above results the model actually saw first. A steer still pending when the turn finalises (no batch boundary followed it) commits at `Outgoing`; server-side it falls to the next turn, so the order still reads correctly. A steer does **not** bump `outstanding_responses` — it rides the current turn's single `Outgoing` rather than adding one.
- A **slash command** is *deferred* in `outgoing_queue` and drained one-at-a-time at `Outgoing` (unchanged), so a client-side slash (`/clear`, `/new`, a dashboard) doesn't disrupt the live region mid-turn and a passthrough slash doesn't spawn a concurrent, interleaving turn. It also shows as a `↳` line while queued.
- `/stop` is the **interrupt**: it is dispatched immediately (the gateway `Router` cancels the in-flight turn out-of-band) and the TUI resets its own live state to idle, because a cancelled turn delivers no `Outgoing`. Any partial answer is flushed and any pending steer committed first (the steer stays queued server-side and runs as its own turn), and any **deferred slash commands are discarded** (`clear_deferred_submissions`) — they were parked for "after this turn", but the cancelled turn sends no `Outgoing` to drain them, so leaving them queued would linger as `↳` lines and later fire after an unrelated turn. The gateway's stop `Notice` lands as the confirmation line. `is_stop_command` mirrors the gateway recognizer (`/stop`, case-insensitive, trailing args ignored).

### Slash completion

- When the input starts with `/` and the cursor sits on the command token (no whitespace between `/` and cursor), a popup lists matching commands just above the input box. It is a real **layout section inside the inline viewport** (sized into `desired_viewport_height` via `chat::completion_popup_height`), not a float drawn above the viewport — the inline viewport's buffer doesn't extend above itself, so drawing there would write out of bounds and panic.
- Candidates come from `SlashHandler::commands()`; `TuiSlashHandler` returns a name-sorted list of `/clear`, `/new`, `/stop`, `/quit`, `/exit`. Skill enumeration is not done client-side — any other `/<name>` falls through to the gateway as `SlashOutcome::PassThrough` so skill invocations keep working without a client-side allow-list.
- `Tab` accepts the highlighted candidate, rewriting the prefix up to the next whitespace and appending a trailing space so arguments can follow. `Enter` submits without accepting the completion.
- `Up`/`Down` follow zsh/bash conventions, which also cleanly resolves the popup ambiguity:
    - **Empty input** — `Up`/`Down` walk the input-history ring. A slash popup never opens on an empty line, so there is no conflict.
    - **Non-empty input** — the user is actively drafting, so `Up`/`Down` drive the popup selection (or are inert if no popup is open). History is gated off until the draft clears or is submitted. This matches shell behavior: once you've typed content, pressing `Up` doesn't silently replace it with a history entry.
    - **While already walking history** — `Up`/`Down` keep walking even if the loaded entry makes the input non-empty and opens a popup. The first non-`Up`/`Down` key exits history mode and snaps the cursor back to the newest slot, so the entry stays on screen and further edits treat the ring as idle.

### Inline approval prompt

- Approval requests render **inline** (no overlay modal): while pending, as a live prompt in the bottom region driven by `AppState.pending_approval: Option<ApprovalChatEntry>`; once resolved, as a committed scrollback line (see below).
- When pending, the entry is expanded: tool name, resource accesses, params preview, and three selectable options (`Approve` / `Always approve` / `Deny`). The user navigates options with `Up`/`Down` (or `k`/`j`) and confirms with `Enter`, or presses a direct shortcut (`a`/`A`/`d`).
- After resolution the entry collapses to a single dot-led line with the decision, tool name, and the first resource access detail — e.g. `● approved: Bash (echo hello)` or `● denied: Read (/etc/shadow)`. Because the tool's scrollback block is deferred until completion, this line is **buffered onto its `RunningTool`** (matched by `call_id`, via `AppState::buffer_approval_line`) rather than committed immediately, so it lands between the tool's `●` and `⎿` when the block commits. If no in-flight tool matches (shouldn't happen), it's committed standalone so the decision isn't lost. Normal input resumes immediately.
- Approvals originate on the gateway. The WS transport observes `Frame::ApprovalRequested` and mirrors the entry into a *local* `ApprovalQueue` so the existing TUI modal logic picks it up unchanged. The queue's resolver callback (installed by `WsTransport::connect`) wraps the TUI's "approve/deny" decision in a `Frame::ResolveApproval` echoed back over the same socket, so the gateway-side gate unblocks. Inbound `Frame::ApprovalResolved` frames drop any stale local mirror — useful when a second frontend resolves the same entry.
- Dropping the responder (e.g. loop shutdown) still surfaces as `ApprovalDecision::Deny` on the local side; the gateway's own 5-minute timeout covers the server side.
- If multiple approvals are queued (concurrent tool calls), resolving one auto-surfaces the next into the scrollback.

### Dashboard

- Single-table layout: title, bold header row, equal-width columns, footer hint.
- Backed by a `DashboardSnapshot { title, columns, rows, footer }` value fetched from a `DashboardProvider`.
- Refresh (`r`) re-fetches on a background task; the snapshot swap is transactional (existing selection clamps to the new row count).
- Three built-in views map to `ViewKind::{Skills, Jobs, Sessions}`.
- `TuiDashboardProvider` (`crates/tui/src/client/dashboard.rs`) renders each view as an admin-only placeholder: title and column headers for the requested `ViewKind`, an empty `rows: Vec::new()`, and a footer pointing the operator at the `baybo` CLI. The TUI's channel surface no longer carries session / job / skill CRUD — those views live in the CLI subcommands.

### Persistent input history

The input history ring survives across TUI sessions. Because users routinely
paste API keys, tokens, and other secrets into prompts, the ring is stored
encrypted at rest rather than in a plaintext history file — but the TUI
process itself never opens the vault. The gateway is the single writer,
and the TUI exchanges the ring over the channel WS like any other state.

- Wire protocol: two `baybo_channels::wire::Frame` variants carry the history end-to-end. `Frame::HistorySnapshot { session_id, entries }` is pushed from the server once, right after `Frame::RegisterAck { ok: true }`, for session-scoped TUI clients only — sidecars never see it. `Frame::HistoryAppend { session_id, entry }` is sent by the TUI after every accepted submission.
- Gateway side: `baybo_gateway::channel::TuiHistoryStore` (`crates/gateway/src/channel/history.rs`) wraps `Arc<SecretVault>` behind a `tokio::sync::Mutex`, so concurrent appends from multiple TUIs on the same gateway serialize into the same vault blob. It reads the current ring from the fixed key `baybo.tui.input_history`, pushes the new entry (de-duping consecutive duplicates), caps the ring at 500 newest entries, and writes it back (see [`security.md`](./security.md)). Load failures or write errors are logged `warn!` and are non-fatal.
- TUI side: `WsTransport` (`crates/tui/src/client/ws.rs`, `transport.rs`) buffers the one-shot snapshot inside `initial_history: Mutex<Option<Vec<String>>>` during `connect_tui`. The main loop calls `ctx.input.take_history_snapshot().await` before the first `terminal.draw`, so the prior ring is populated when the input box first renders. `Action::Submit` calls `ctx.input.append_history(&entry)` in a detached `tokio::spawn` — no vault handle, no lock file, no local `baybo-security` dependency.
- The TUI crate no longer carries an `InputHistoryStore` trait or any history builder on `TuiAdapter`. The store is implicit in the transport; tests that construct an adapter without a live gateway just get no snapshot and no-op appends.

## Slash Commands

### Dashboard shortcuts

Bare commands with no arguments open the matching dashboard view:

| Slash input | Outcome                                      |
| ----------- | -------------------------------------------- |
| `/skills`   | `SlashOutcome::OpenView(ViewKind::Skills)`   |
| `/jobs`     | `SlashOutcome::OpenView(ViewKind::Jobs)`     |
| `/sessions` | `SlashOutcome::OpenView(ViewKind::Sessions)` |

Dashboard shortcuts only fire when invoked with no arguments — anything with additional tokens (e.g. `/skills info foo`) falls through as `SlashOutcome::PassThrough` and is forwarded to the gateway like any other line.

### Slash handler

`TuiSlashHandler` (`crates/tui/src/client/slash.rs`) is the TUI's `SlashHandler` implementation. The WS channel surface is narrow, so the handler only ships what it can actually satisfy: `/clear`, `/new` (`SlashOutcome::NewSession` — abandon the current session and start a fresh one), `/quit`, `/exit`, `/help` (renders inline help), and the dashboard shortcuts (`/sessions`, `/jobs`, `/skills`). `/stop` is also recognised but returns `SlashOutcome::PassThrough` — it forwards to the agent runtime, where the `Router` cancels the in-flight turn and its subagents out-of-band (the TUI can't reach those handles). Tool approvals are handled through the modal keybindings (`a` / `A` / `d`), not a slash command. Any other `/<name>` falls through as `SlashOutcome::PassThrough` so skill invocations keep working without a client-side allow-list.

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

`crates/baybo/src/tui_cmd.rs::run` drives the boot. There is no call to
`singleton::acquire`, no `build_managers`, and no `wire_router` —
none of those exist on the TUI side.

1. **Resolve the admin address** — `admin_addr_from_config(&config)`
   builds a `SocketAddr` from `config.gateway.bind_address` and
   `config.gateway.port` (the same admin listener that co-hosts
   `/v1/channel-ws`). When the gateway is bound to a wildcard
   (`0.0.0.0` / `::`), a same-host TUI rewrites it to loopback — the
   wildcard is a server-side bind directive, not a dialable target.
   There is no port file and no `channel.port` discovery step.
2. **Resolve the bearer tokens** — open the workspace secret vault via
   `crate::runtime::build_secret_vault(config)` and read
   `baybo_gateway::TUI_TOKEN_VAULT_KEY` (`"gateway.tui_token"`) for
   `/v1/channel-ws`. The vault open is best-effort; a missing TUI token
   is treated as `ChannelError::NotReachable` so it flows into the same
   fallback path as an unreachable admin listener. The admin bearer token
   is also read through `baybo_gateway::AdminToken` for the optional
   active-model lookup.
3. **Connection probe** — `WsTransport::connect(addr, tui_token,
   session_id)` dials the resolved admin `addr`'s `/v1/channel-ws`
   upgrade, presents the token on the upgrade, and performs the
   `Frame::Register` handshake. There is no separate `/healthz`
   probe; the WS connect is what proves the gateway is up. Failure
   produces a concrete error block:

   ```
   no baybo gateway reachable at <addr>
     - start it with:       baybo gateway start
     (underlying error: ...)
   ```
4. **Session resolution** — `--session <id>` pins an explicit id (for
   resuming a workspace session across restarts); without the flag
   the TUI mints a fresh UUID client-side. The gateway's router
   auto-creates the session on the first inbound frame via
   `SessionManager::get_or_create`, so no REST round-trip is needed
   to provision one.
5. **Fetch the active model label** — after the WS connection proves the
   gateway is live, the TUI performs a best-effort `GET /v1/llm` request
   against the same admin address with `Authorization: Bearer <admin-token>`.
   A successful response formats `provider/model_id` for the input footer;
   any auth/network/JSON failure simply leaves the footer hidden.
6. **Wire the providers** — construct `WsTransport` (the connect
   above), `TuiSlashHandler::new()`, and `TuiDashboardProvider::new()`.
   The transport is a **required constructor argument** —
   `TuiAdapter::new(transport)` — while the slash handler and
   dashboard provider are attached via the `with_slash_handler` and
   `with_dashboard_provider` builders; the model label is attached via
   `with_model_label` when available (`with_on_exit` wires the shutdown
   trigger). Input history is delivered over the WS itself
   (see [Persistent input history](#persistent-input-history)), so no
   history store is wired in.
7. **Start the adapter**. There is no local `ChannelRegistry` and no
   cron trigger receiver: the TUI has no router. User input is
   framed as `Frame::Message` and sent through the transport; the
   gateway registers the connection on its side.
7. **Graceful shutdown** — `install_signal_handler` wires
   SIGINT/SIGTERM into the adapter's `ShutdownSignal`. A 5 s
   `force_exit_watchdog` bounds teardown so a stalled WS pump never
   pins the process.

### `--dev-auto-gateway`

Debug builds add a `--dev-auto-gateway` flag. When the initial
`WsTransport::connect` returns `ChannelError::NotReachable` and the
flag is set, `crates/baybo/src/tui_cmd.rs::dev_auto` spawns
`Command::new(std::env::current_exe()).args(["gateway", "start"])`
as a subprocess, polls the admin address with a loopback
`TcpStream::connect` with exponential backoff (100 ms → 1 s, 15 s
deadline), re-reads the freshly-rotated TUI token from the vault,
retries the WS connect, and returns an RAII guard that sends
SIGKILL on drop. A loud banner prints before the TUI takes over
the terminal. The flag is gated by `#[cfg(debug_assertions)]` so
`cargo build --release` cannot compile it in.

## Architecture

### Event sources

The loop multiplexes three sources with `tokio::select!`:

1. **Shutdown** — `Arc<Notify>` toggled by `TuiAdapter::stop` or the WS pump's teardown guard.
2. **Terminal input** — a `crossterm::event::EventStream`. Raw reads are required because the terminal is in raw mode and crossterm owns `/dev/tty`; a `tokio::io::stdin` reader would fight it for the device.
3. **Internal events** — `AppEvent` sent by the `WsTransport` pump (streaming deltas, responses, approval events) and by the background dashboard-fetch task.

### Transport

`WsTransport` (`crates/tui/src/client/transport.rs`) is the single
concrete transport used by the TUI. The adapter holds an
`Arc<WsTransport>` and calls three methods on it:

- `submit(msg)` — flatten an `IncomingMessage`'s text blocks and
  send a `Frame::Message` over the WS.
- `subscribe(session_id) -> TransportEventStream` — decode inbound
  frames from the same WS and translate them into
  `TransportEvent::{StreamDelta, ToolStarted, ToolCompleted, Status, Response, Notice, ApprovalRequested, ApprovalResolved}`. `Frame::Reasoning` is dropped (no variant) — the TUI shows a working indicator instead of the thinking trace.
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
- `ToolStarted { call_id, tool, label }` / `ToolCompleted { call_id, status, summary }` → the matching `AppEvent` variants. `ToolStarted` records a live `RunningTool` (working-zone row) and commits nothing; `ToolCompleted` pops it and commits the whole `● tool` + (buffered approval) + `⎿ summary` block as one unit, so results stay under their tool across concurrent calls — see [Chat](#chat). `Frame::Reasoning` has no `TransportEvent`/`AppEvent` mapping — the pump drops it (the working indicator stands in for the thinking trace).
- `Response(blocks)` → `AppEvent::Outgoing(Vec<ContentBlock>)`. Finalises the response via `finalize_stream` (commit the trailing partial + non-text extras, or render the body from blocks when nothing streamed — see [Chat](#chat)), then clears `AppState.streaming`.
- `Notice { level, text }` → `AppEvent::Log(LogRecord { level, target: "agent", message })`. Reuses the log surface.
- `ApprovalRequested` / `ApprovalResolved` — see [Inline approval prompt](#inline-approval-prompt).

Single-consumer ordering on the mpsc keeps delta/response ordering correct as WS frames are fed in.

### Raw-mode discipline

- Entry (`run_loop`): install the panic hook, `enable_raw_mode()`, then `PushKeyboardEnhancementFlags(DISAMBIGUATE_ESCAPE_CODES)` once for the session (Kitty protocol, so `Shift`/`Alt+Enter` reach us with modifier bits set; unsupported terminals ignore it). The screen + scrollback are then **cleared** (`CLEAR_SCREEN_AND_SCROLLBACK`) for a clean start before the inline viewport is created. Chat renders into an **inline viewport** (`Viewport::Inline`) on the **main** screen — it does **not** enter the alternate screen, so native mouse-wheel scroll / copy-paste keep working (the pre-launch shell history is wiped by the entry clear rather than scrolled above).
- Alternate screen + mouse capture are **dashboard-only**: `enter_dashboard_mode` issues `EnterAlternateScreen` + `EnableMouseCapture` and flips an `in_alt_screen` flag; leaving the dashboard reverses both.
- Teardown is RAII via `TuiTeardownGuard::drop`, so any early return restores the terminal: pop the keyboard-enhancement flags (if pushed), `LeaveAlternateScreen` + `DisableMouseCapture` **only if** `in_alt_screen`, `disable_raw_mode()`, then fire `on_exit`. Best-effort, logged on failure.
- Panic hook: installed once via `OnceLock` — pops the keyboard flags, disables raw mode, and best-effort leaves the alt screen + mouse capture, then delegates to the previous hook. Without it a panic would leave the terminal unusable.

### Logging in chat mode

Writing `tracing` records to stdout while ratatui owns raw mode corrupts the frame. Chat mode therefore uses a two-layer subscriber:

- **File layer** — `tracing_appender::rolling::daily("<workspace>/logs", "baybo.log")` wrapped in a non-blocking writer. A `WorkerGuard` held on the stack of `main` flushes pending lines on shutdown.
- **TUI echo layer** — `TuiLogLayer` (`crates/baybo/src/tui_log.rs`) filters **tracing** events to `WARN` and `ERROR` (lower `tracing` levels stay in the file only), extracts `message` + structured fields, and forwards them through `TuiLogSink::emit` as `AppEvent::Log(LogRecord)`. The event loop pushes the record onto the scrollback as a coloured line (`warn` yellow, `error` red, with the event `target` in grey).

The `WARN`/`ERROR` cut applies only to this tracing-echo layer. Agent **notices** take a separate route: the transport maps `Frame::Notice` → `TransportEvent::Notice` → `AppEvent::Log` (see [Output path](#output-path)), preserving all three `NoticeLevel`s — `Info` notices reach the same `LogRecord` surface and render as a cyan `info` line (`LogLevel::Info`), so an agent-emitted info notice is not filtered out the way a `tracing` INFO event is.

The sink is plumbed into the layer via `Arc<OnceLock<TuiLogSink>>`: `init_tracing` allocates the cell, then `main` sets it from `TuiAdapter::log_sink()` after the adapter is constructed. Events emitted before the cell is filled still reach the file layer; they simply don't appear in the TUI.

Argv mode keeps the old stdout layer — one-shot commands don't own the terminal, so normal formatting works.

## Session IDs and `ChannelType`

- `TuiAdapter` stamps every message with a UUID-based session id and `ChannelType::tui()` (the well-known constant `"tui"`).
- `ChannelType` (`crates/model/src/session.rs`) is a transparent newtype around `String` rather than a closed enum, so runtime-registered sidecars can declare arbitrary channel names without a core enum extension.

## Constraints

- The streamed deltas and the final `AgentEvent::Message` are **not** redundant: deltas commit the body to scrollback line-by-line as it streams, and the final Message only finalises it (trailing partial + non-text extras). The Message re-renders the body from its blocks **only** when nothing streamed (`delta_tx = None`: cron / subagent-notification, or reconnect catch-up of persisted rows) — so the body is never both streamed and re-rendered.
- Renderer state (`AppState`) is mutated only on the event-loop task. External code uses the mpsc event channel; there is no shared `Mutex<AppState>`.
- Input/state mutation in `app.rs` and key translation in `keymap.rs` are pure — unit tests exercise them without a terminal. Line-rendering helpers (e.g. `finalize_lines`, the `render_*` functions) are pure too — they return `Vec<Line>`, so tests assert on them directly; the one buffer-level fixup (`elide_wide_char_continuations`) is tested against a ratatui `Buffer`.
- Dashboard providers must not block; the `DashboardProvider` trait method is `async` and the bundled `TuiDashboardProvider` returns synchronously without I/O.

## Collaboration

| Module     | Role                                                                                         |
| ---------- | -------------------------------------------------------------------------------------------- |
| `model`    | `ContentBlock` for rendering assistant messages; `ChannelType::tui()`, `User` used when constructing `IncomingMessage` |
| `gateway`  | Server-side owner of sessions, approvals, outbound frame fan-out, and the vault-encrypted input-history store. The TUI talks to it over `/v1/channel-ws` (WebSocket + MessagePack) |
| `channels` | Trait definitions only: `SlashHandler`, `SlashOutcome`, `ViewKind`, `DashboardProvider`, `DashboardSnapshot`, `IncomingMessage`, `NoticeLevel`, `ChannelError`. No TUI code. |
| `tools`    | Approval-prompt machinery: `ApprovalQueue` (re-exported via `pub use baybo_tools::ApprovalQueue`), `ApprovalDecision`, `ApprovalRequest`, `ResourceAccess`. The transport mirrors gateway `Frame::ApprovalRequested` into a local `ApprovalQueue` and the modal renders/resolves from it. |

## Verification

```bash
cargo test -p baybo-tui             # keymap, AppState, transport, slash, frame codec, WS client
cargo run -- gateway start         # terminal A: long-lived backend
cargo run -- tui                   # terminal B: WS+MessagePack client
```

Manual smoke:

- `baybo tui` against a running `baybo gateway` opens the Ratatui UI and the chat pane is live. Bare `baybo` prints help instead.
- With no gateway reachable, `baybo tui` exits with the concrete "no baybo gateway reachable at <addr>" block.
- `cargo run -- tui --dev-auto-gateway` (debug build) in a fresh workspace with no gateway running spawns the backend inline, prints the banner, and connects.
- Typing + `Enter` appends a user line and sends a `Frame::Message` to the gateway; inbound `Frame::AnswerDelta`s render live and commit to scrollback line-by-line, and the final `Frame::Message` only finalises the trailing partial (it does **not** replace the already-committed lines).
- `/skills` opens the admin-placeholder skills view (footer points at the `baybo` CLI); `r` refreshes; `Esc` returns to chat.
- A tool call that requires approval queues an inline prompt; `a` resolves it, the gateway-side gate unblocks, and the tool result renders.
- Killing the gateway mid-session surfaces the next inbound frame as an error notice rather than crashing the TUI.
- `Ctrl-C` on an empty input line exits cleanly with the terminal restored.
