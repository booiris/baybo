# tui - Built-in Terminal UI Channel

## Overview

`TuiAdapter` is the interactive channel for Aura, launched via `aura tui`. Bare `aura` prints `--help`; the TUI is an explicit opt-in to avoid surprising users with a full-screen app. It is implemented with [Ratatui] over a [Crossterm] async event stream and lives inside the `channels` crate (`crates/channels/src/tui/`).

The layout is intentionally minimal:

- **Scrollback pane** — rendered chat lines (user, assistant, system).
- **Input line** — single-line editor with emacs-style cursor motions and a history ring.
- **Dashboard view** (modal) — opened by dashboard-style slash commands; returns to chat on `Esc`.

No status bar, no sidebars. Aura's operator surface lives in the CLI subcommands; the TUI only hosts the conversation and a handful of read-only views.

[Ratatui]: https://docs.rs/ratatui
[Crossterm]: https://docs.rs/crossterm

## Views

### Chat

- Scrollback is a fixed-capacity ring (`SCROLLBACK_CAP = 5000`) of `ChatLine::{User,Assistant,System,Log}` entries.
- Assistant lines render each `ContentBlock`: text inline, and `Image`/`Audio`/`File` as a bracketed placeholder.
- Input history keeps up to `HISTORY_CAP = 500` non-empty submissions with trivial de-duplication of consecutive identical lines.
- `scroll_offset` is measured in rendered rows from the tail: `0` keeps the newest line pinned at the bottom; `PageUp`/`PageDown` grow or shrink the offset by 10.
- While the LLM is responding, an ephemeral streaming buffer is drawn immediately below the scrollback tail. Each `AgentOutput::Delta` extends the buffer; the final `AgentOutput::Message` replaces it with a persisted `ChatLine::Assistant` carrying the canonical `ContentBlock` list.

### Slash completion

- When the input starts with `/` and the cursor sits on the command token (no whitespace between `/` and cursor), a popup renders above the input box listing matching commands.
- Candidates come from `SlashHandler::commands()`; `CliSlashHandler` derives them from clap's subcommand tree plus adapter-reserved tokens (`/quit`, `/exit`, `/clear`).
- `Up`/`Down` cycle the selection; `Tab` accepts the highlighted candidate, rewriting the prefix up to the next whitespace and appending a trailing space so arguments can follow. `Enter` submits without accepting the completion.

### Dashboard

- Single-table layout: title, bold header row, equal-width columns, footer hint.
- Backed by a `DashboardSnapshot { title, columns, rows, footer }` value fetched from a `DashboardProvider`.
- Refresh (`r`) re-fetches on a background task; the snapshot swap is transactional (existing selection clamps to the new row count).
- Five built-in views map to `ViewKind::{Skills, Tools, Jobs, Sessions, Memory}`.

## Slash Commands

### Dashboard shortcuts

Bare commands with no arguments open the matching dashboard view:

| Slash input | Outcome                                      |
| ----------- | -------------------------------------------- |
| `/skills`   | `SlashOutcome::OpenView(ViewKind::Skills)`   |
| `/tools`    | `SlashOutcome::OpenView(ViewKind::Tools)`    |
| `/jobs`     | `SlashOutcome::OpenView(ViewKind::Jobs)`     |
| `/sessions` | `SlashOutcome::OpenView(ViewKind::Sessions)` |
| `/memory`   | `SlashOutcome::OpenView(ViewKind::Memory)`   |

Anything with additional tokens (e.g. `/skills info foo`) falls through to the normal clap dispatcher and returns text that is appended to the chat scrollback.

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

## Architecture

### Event sources

The loop multiplexes three sources with `tokio::select!`:

1. **Shutdown** — `Arc<Notify>` toggled by `TuiAdapter::stop` (invoked by the `ChannelRegistry` during teardown).
2. **Terminal input** — a `crossterm::event::EventStream`. Raw reads are required because the terminal is in raw-mode + alternate screen; a `tokio::io::stdin` reader would fight crossterm for `/dev/tty`.
3. **Internal events** — `AppEvent` sent by `send_response` (router-delivered assistant blocks) and by the background dashboard-fetch task.

### Output path

Neither `send_response` nor `send_stream_delta` renders directly. Both push into the `event_tx` mpsc captured in `start()`:

- `send_stream_delta(session_id, delta)` → `AppEvent::StreamDelta(String)`. The loop appends to `AppState.streaming`, redrawing on each chunk so the partial response appears live.
- `send_response(outgoing)` → `AppEvent::Outgoing(Vec<ContentBlock>)`. The loop clears `AppState.streaming` and pushes the canonical `ChatLine::Assistant` onto the scrollback.
- `send_notice(session_id, level, text)` → `AppEvent::Log(LogRecord { level, target: "agent", message })`. Reuses the log-line surface so a notice from the agent (e.g. a Suspicious skill advisory) appears with the same colour coding as warn/error tracing events; no dedicated chat-line variant was worth adding for a rare out-of-band signal.

Routing the final message through the same mpsc keeps ordering against deltas trivially correct (single consumer) and keeps the router task off the terminal.

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
| `cli`      | Provides `CliSlashHandler` (slash dispatch) and `CliDashboardProvider` (dashboard snapshots) |
| `agent`    | Router delivers `OutgoingMessage` via `send_response`, which becomes an `AppEvent::Outgoing` |
| `channels` | Hosts the `TuiAdapter`, `DashboardProvider` trait, `SlashOutcome::OpenView`, and `ViewKind`  |

## Verification

```bash
cargo test -p aura-channels        # keymap + AppState unit tests
cargo run                          # launches TuiAdapter on a live terminal
```

Manual smoke:

- `aura tui` opens the Ratatui UI with an empty chat pane and an input box. Bare `aura` prints help instead.
- Typing + `Enter` appends a user line and forwards to the router; the router's response lands in the same pane.
- `/skills` opens the skills dashboard; `Up`/`Down` moves selection; `r` refreshes; `Esc` returns to chat with scrollback preserved.
- `/config show` (any arg-bearing slash command) stays in chat and renders as a text block.
- `Ctrl-C` on an empty input line exits cleanly with the terminal restored.
