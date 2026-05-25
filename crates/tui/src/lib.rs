//! Ratatui-based TUI for `aura tui`.
//!
//! The TUI is a thin client on top of an `aura-gateway` reached over
//! a WS+MessagePack channel socket (see [`client::WsTransport`]).
//!
//! Rendering model:
//! - **Chat mode** uses ratatui's `Viewport::Inline` — the TUI reserves a
//!   small live region at the bottom of the *main* screen (input box +
//!   streaming preview + pending approval) and emits historical lines
//!   into the terminal's own scrollback via [`Terminal::insert_before`].
//!   Native mouse-wheel scrolling and drag-to-select Just Work because
//!   we never enter the alternate screen or capture the mouse.
//! - **Dashboard mode** still owns the full screen: on entry the chat
//!   terminal is dropped, the alternate screen + mouse capture are
//!   enabled, and a fresh fullscreen-viewport terminal is constructed.
//!   On exit the reverse happens and the live chat region reappears.
//!
//! Terminal I/O: stdin is driven by [`crossterm::event::EventStream`];
//! do **not** read stdin via tokio here, as raw mode would conflict with
//! line-buffered readers.

mod app;
mod chat;
pub mod client;
mod dashboard;
pub(crate) mod event;
mod keymap;
pub mod transport;

pub use aura_tools::ApprovalQueue;
pub use event::{LogLevel, LogRecord, TuiLogSink};
pub use transport::{TransportEvent, TransportEventStream};

use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::Arc;
use std::sync::OnceLock;

use aura_channels::{
    ChannelError, DashboardProvider, IncomingMessage, Message, NoticeLevel, Result, SlashHandler,
    SlashOutcome, ViewKind,
};
use aura_model::{ChannelType, SessionId, User};
use aura_model::{ContentBlock, MessageMetadata};
use chrono::Utc;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, EventStream, KeyEvent,
    KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use parking_lot::Mutex;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};
use tokio::sync::{Notify, mpsc};
use tracing::{error, warn};
use uuid::Uuid;

use std::time::Instant;

use crate::app::{AppState, ApprovalChatEntry, ApprovalChatState, CONFIRM_EXIT_WINDOW, ViewMode};
use crate::client::WsTransport;
use crate::event::AppEvent;
use crate::keymap::{Action, KeyContext, translate};

/// Callback invoked exactly once when the TUI event loop exits, regardless
/// of cause (user-initiated quit, terminal disconnect, internal shutdown
/// notification). The typical wiring triggers the process-wide shutdown
/// signal so the rest of the runtime can tear down — without this the
/// router task blocks forever on `response_rx.recv()` since the supervisor
/// and cron scheduler keep their senders alive.
pub type OnExit = Arc<dyn Fn() + Send + Sync>;

/// Ratatui-based terminal channel adapter.
pub struct TuiAdapter {
    initial_session_id: SessionId,
    user: User,
    shutdown: Arc<Notify>,
    slash_handler: Option<Arc<dyn SlashHandler>>,
    dashboard_provider: Option<Arc<dyn DashboardProvider>>,
    on_exit: Option<OnExit>,
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: Arc<Mutex<Option<mpsc::Receiver<AppEvent>>>>,
    approval_queue: ApprovalQueue,
    transport: Arc<WsTransport>,
}

impl TuiAdapter {
    pub fn new(transport: Arc<WsTransport>) -> Self {
        let user_id = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "tui-user".to_string());
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(256);
        let approval_queue = transport.approval_queue();
        let initial_session_id = transport.current_session_id();
        Self {
            initial_session_id,
            user: User {
                id: user_id,
                name: None,
                channel: ChannelType::tui(),
            },
            shutdown: Arc::new(Notify::new()),
            slash_handler: None,
            dashboard_provider: None,
            on_exit: None,
            event_tx,
            event_rx: Arc::new(Mutex::new(Some(event_rx))),
            approval_queue,
            transport,
        }
    }

    pub fn with_on_exit(mut self, on_exit: OnExit) -> Self {
        self.on_exit = Some(on_exit);
        self
    }

    pub fn with_slash_handler(mut self, handler: Arc<dyn SlashHandler>) -> Self {
        self.slash_handler = Some(handler);
        self
    }

    pub fn with_dashboard_provider(mut self, provider: Arc<dyn DashboardProvider>) -> Self {
        self.dashboard_provider = Some(provider);
        self
    }

    pub fn log_sink(&self) -> TuiLogSink {
        TuiLogSink::new(self.event_tx.clone())
    }

    /// Spawn the TUI event loop. Returns once the loop task has been
    /// spawned; the loop runs until shutdown or user-initiated quit.
    pub async fn start(&self) -> Result<()> {
        let event_rx = self
            .event_rx
            .lock()
            .take()
            .ok_or_else(|| ChannelError::Send("TuiAdapter::start called twice".into()))?;

        spawn_transport_pump(Arc::clone(&self.transport), self.event_tx.clone()).await;

        let ctx = LoopCtx {
            initial_session_id: self.initial_session_id.clone(),
            user: self.user.clone(),
            shutdown: Arc::clone(&self.shutdown),
            input: Arc::clone(&self.transport),
            event_tx: self.event_tx.clone(),
            event_rx,
            slash_handler: self.slash_handler.clone(),
            dashboard_provider: self.dashboard_provider.clone(),
            on_exit: self.on_exit.clone(),
            approval_queue: self.approval_queue.clone(),
        };

        tokio::spawn(async move {
            if let Err(e) = run_loop(ctx).await {
                error!("TUI event loop exited with error: {e}");
            }
        });

        Ok(())
    }
}

struct LoopCtx {
    initial_session_id: SessionId,
    user: User,
    shutdown: Arc<Notify>,
    input: Arc<WsTransport>,
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: mpsc::Receiver<AppEvent>,
    slash_handler: Option<Arc<dyn SlashHandler>>,
    dashboard_provider: Option<Arc<dyn DashboardProvider>>,
    on_exit: Option<OnExit>,
    approval_queue: ApprovalQueue,
}

async fn spawn_transport_pump(transport: Arc<WsTransport>, event_tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut stream = match transport.subscribe().await {
            Ok(s) => s,
            Err(e) => {
                let _ = event_tx
                    .send(AppEvent::Log(LogRecord {
                        level: LogLevel::Error,
                        target: "tui.transport".into(),
                        message: format!("subscribe failed: {e}"),
                    }))
                    .await;
                return;
            }
        };
        use futures::StreamExt;
        while let Some(next) = stream.next().await {
            let event = match next {
                Ok(ev) => ev,
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Log(LogRecord {
                            level: LogLevel::Warn,
                            target: "tui.transport".into(),
                            message: format!("stream error: {e}"),
                        }))
                        .await;
                    continue;
                }
            };
            let forwarded = match event {
                TransportEvent::StreamDelta(text) => Some(AppEvent::StreamDelta(text)),
                TransportEvent::Response(blocks) => Some(AppEvent::Outgoing(blocks)),
                TransportEvent::Notice { level, text } => Some(AppEvent::Log(LogRecord {
                    level: match level {
                        NoticeLevel::Info => LogLevel::Info,
                        NoticeLevel::Warn => LogLevel::Warn,
                        NoticeLevel::Error => LogLevel::Error,
                    },
                    target: "agent".into(),
                    message: text,
                })),
                TransportEvent::ApprovalRequested => Some(AppEvent::ApprovalRequested),
                TransportEvent::ApprovalResolved { .. } => None,
            };
            if let Some(ev) = forwarded
                && event_tx.send(ev).await.is_err()
            {
                break;
            }
        }
    });
}

/// RAII guard that restores the terminal on unwind: pops keyboard
/// enhancement, disables mouse capture, leaves the alternate screen if
/// the session is currently in it (dashboard mode), and disables raw
/// mode, then fires the on_exit callback. Chat mode runs on the main
/// screen so the user's bash history stays visible across the session.
struct TuiTeardownGuard {
    on_exit: Option<OnExit>,
    keyboard_enhanced: bool,
    in_alt_screen: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for TuiTeardownGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        if self.keyboard_enhanced
            && let Err(e) = execute!(stdout, PopKeyboardEnhancementFlags)
        {
            warn!("pop keyboard enhancement failed: {e}");
        }
        if self
            .in_alt_screen
            .load(std::sync::atomic::Ordering::Relaxed)
            && let Err(e) = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen)
        {
            warn!("leave alternate screen failed: {e}");
        }
        if let Err(e) = disable_raw_mode() {
            warn!("disable_raw_mode failed: {e}");
        }
        if let Some(cb) = self.on_exit.take() {
            cb();
        }
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Build a fresh `Terminal` configured for chat mode (inline viewport)
/// at the given height. The viewport is sized to exactly fit the live
/// content (`input_h + approval_h`), so there's no empty gap between
/// the latest scrollback message and the input box. The cursor is left
/// wherever the shell put it — ratatui's `compute_inline_size` anchors
/// the viewport at that row.
fn new_chat_terminal(viewport_h: u16) -> io::Result<Term> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_h.max(1)),
        },
    )
}

/// Desired inline-viewport height for the current state: input box +
/// pending-approval prompt (if any). Streaming text doesn't add to this
/// because we commit each completed line directly into scrollback.
fn desired_viewport_height(state: &AppState) -> u16 {
    let mut h = chat::input_box_height(state);
    if let Some(entry) = state.pending_approval.as_ref() {
        h = h.saturating_add(chat::approval_pending_height(entry));
    }
    h.max(1)
}

/// Build a fresh `Terminal` configured for dashboard mode (fullscreen).
/// The session is already inside the alternate screen; the caller toggles
/// mouse capture around this if dashboard interactions need it.
fn new_dashboard_terminal() -> io::Result<Term> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend)
}

async fn run_loop(mut ctx: LoopCtx) -> anyhow::Result<()> {
    install_panic_hook();

    enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
    // Push the Kitty keyboard protocol once for the whole session so
    // Shift+Enter / Alt+Enter / etc. reach us with modifier bits set
    // regardless of which mode we're rendering. Unsupported terminals
    // silently ignore the sequence.
    let keyboard_enhanced = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )
    .is_ok();
    // Tracks whether we've switched into the alternate screen for the
    // dashboard. Chat mode runs on the main screen so the user's bash
    // history stays scrolled above the live region; only entering a
    // dashboard flips this on. The teardown guard uses it to decide
    // whether to issue `LeaveAlternateScreen` on unwind.
    let in_alt_screen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // From here on, any early return must be matched by the guard
    // restoring the terminal — its Drop impl handles every cleanup step.
    let _guard = TuiTeardownGuard {
        on_exit: ctx.on_exit.take(),
        keyboard_enhanced,
        in_alt_screen: Arc::clone(&in_alt_screen),
    };

    let mut state = AppState::new().with_approval(ctx.approval_queue.clone());
    if let Some(handler) = ctx.slash_handler.as_ref() {
        state.set_commands(handler.commands());
    }
    if let Some(entries) = ctx.input.take_history_snapshot().await {
        state.set_history(entries);
    }
    let mut current_viewport_h = desired_viewport_height(&state);
    let mut terminal = new_chat_terminal(current_viewport_h)
        .map_err(|e| anyhow::anyhow!("new_chat_terminal: {e}"))?;
    let mut term_events = EventStream::new();

    commit_banner(&mut terminal, &ctx.initial_session_id)?;
    terminal.draw(|f| render_chat(f, &mut state))?;

    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.notified() => break Ok(()),
            term = term_events.next() => {
                match term {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match handle_key(&mut state, &mut ctx, &mut terminal, key).await? {
                                KeyOutcome::Exit => break Ok(()),
                                KeyOutcome::Continue => {}
                                KeyOutcome::DashboardExit => {
                                    exit_dashboard_mode(
                                        &mut terminal,
                                        &in_alt_screen,
                                        &mut state,
                                        &mut current_viewport_h,
                                    )?;
                                }
                            }
                        }
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    Some(Ok(CrosstermEvent::Mouse(_))) => {
                        // Mouse capture is only enabled in dashboard mode and
                        // dashboard navigation is keyboard-only — ignore.
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(anyhow::anyhow!("terminal event error: {e}")),
                    None => break Ok(()),
                }
            }
            ev = ctx.event_rx.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                match ev {
                    AppEvent::Outgoing(blocks) => {
                        if matches!(state.mode, ViewMode::Chat) {
                            finalize_stream(&mut state, &mut terminal, &blocks)?;
                        }
                        state.clear_stream();
                        state.note_response_received();
                        // Drain as many queued items as possible while
                        // the loop stays idle. A queued plain message
                        // re-arms `outstanding_responses` and breaks
                        // out; a queued slash like `/skills` or
                        // `/clear` doesn't touch the agent and lets the
                        // next item run in the same cycle.
                        while !state.is_busy()
                            && let Some(next) = state.dequeue_submission()
                        {
                            dispatch_submission(&mut state, &ctx, &mut terminal, next).await?;
                        }
                        resize_chat_viewport_if_needed(
                            &state,
                            &mut terminal,
                            &mut current_viewport_h,
                        )?;
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    AppEvent::StreamDelta(delta) => {
                        state.append_stream_delta(&delta);
                        if matches!(state.mode, ViewMode::Chat) {
                            flush_complete_stream_lines(&mut state, &mut terminal)?;
                        }
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    AppEvent::DashboardReady(kind, snapshot) => {
                        if state.dashboard_kind() == Some(kind) {
                            state.refresh_dashboard(snapshot);
                        } else {
                            enter_dashboard_mode(
                                &mut terminal,
                                &in_alt_screen,
                                &mut state,
                                kind,
                                snapshot,
                            )?;
                        }
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    AppEvent::Log(record) => {
                        if matches!(state.mode, ViewMode::Chat) {
                            commit_lines(&mut terminal, chat::render_log_lines(&record))?;
                        }
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    AppEvent::ApprovalRequested => {
                        if !state.approval_pending()
                            && let Some(queue) = state.approval.as_ref()
                            && let Some(req) = queue.peek_head()
                        {
                            state.set_pending_approval(ApprovalChatEntry {
                                tool: req.tool,
                                accesses: req.accesses,
                                params_preview: req.params_preview,
                                state: ApprovalChatState::Pending { selected: 0 },
                            });
                        }
                        draw_active(&mut terminal, &mut state, &mut current_viewport_h)?;
                    }
                    AppEvent::Shutdown => break Ok(()),
                }
            }
        }
    }
}

/// Render whichever mode the state is currently in. Chat mode uses the
/// inline viewport; dashboard mode owns the full alt-screen. Resizes
/// the inline viewport first so its height tracks `input_h +
/// approval_h` and there's no leftover gap above the input box.
fn draw_active(
    terminal: &mut Term,
    state: &mut AppState,
    current_viewport_h: &mut u16,
) -> io::Result<()> {
    match &state.mode {
        ViewMode::Chat => {
            resize_chat_viewport_if_needed(state, terminal, current_viewport_h)?;
            terminal.draw(|f| render_chat(f, state)).map(|_| ())
        }
        ViewMode::Dashboard { .. } => terminal.draw(|f| render_dashboard(f, state)).map(|_| ()),
    }
}

/// Resize the inline chat viewport when the live content's footprint
/// changes (input grew a line, approval popped/resolved). ratatui's
/// `Viewport::Inline(_)` height is fixed at construction, so this drops
/// the existing terminal and rebuilds one with the new height. We
/// clear the old viewport first so its tail content doesn't linger
/// beyond the new (potentially smaller) viewport area.
fn resize_chat_viewport_if_needed(
    state: &AppState,
    terminal: &mut Term,
    current: &mut u16,
) -> io::Result<()> {
    let desired = desired_viewport_height(state);
    if desired == *current {
        return Ok(());
    }
    terminal.clear()?;
    *terminal = new_chat_terminal(desired)?;
    *current = desired;
    Ok(())
}

/// Stream-mode helpers — see [`AppState::streaming`] / `streaming_committed_any`.
fn flush_complete_stream_lines(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    let lines = state.drain_complete_stream_lines();
    for line in lines {
        let leader_is_continuation = state.streaming_committed_any;
        commit_lines_compact(
            terminal,
            chat::render_stream_line(&line, leader_is_continuation),
        )?;
        state.streaming_committed_any = true;
    }
    Ok(())
}

/// Build the scrollback lines for a finalised agent response.
///
/// `started` is whether any streamed line already reached the scrollback
/// for this response; `partial` is the trailing streamed line still
/// buffered, if any.
///
/// When the body streamed, its text is already in the scrollback, so we
/// append only the trailing partial plus any non-text extras the stream
/// didn't carry (e.g. the CronCreate recurring-trigger hint). When
/// nothing streamed — the non-streaming delivery path where `delta_tx`
/// was `None`: cron fires and subagent-notification turns (see
/// `AgentActor::run_agent_loop` callers) deliver only a final Message,
/// no deltas — the body never reached the scrollback, so the full
/// message is rendered from `blocks`. A trailing blank separates this
/// response from the next, emitted only when something was committed (an
/// empty Outgoing is a no-op).
fn finalize_lines(
    blocks: &[ContentBlock],
    started: bool,
    partial: Option<String>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut committed = started;
    if let Some(partial) = partial {
        out.extend(chat::render_stream_line(&partial, committed));
        committed = true;
    }
    if committed {
        out.extend(chat::render_non_text_blocks(blocks, true));
    } else {
        let body = chat::render_assistant_lines(blocks);
        if !body.is_empty() {
            committed = true;
            out.extend(body);
        }
    }
    if committed {
        out.push(Line::from(""));
    }
    out
}

/// Finalise the in-flight agent response and commit it to scrollback.
/// See [`finalize_lines`] for the streamed-vs-non-streamed split. Caller
/// clears streaming state and decrements the outstanding-response
/// counter afterwards.
fn finalize_stream(
    state: &mut AppState,
    terminal: &mut Term,
    blocks: &[ContentBlock],
) -> io::Result<()> {
    let started = state.streaming_committed_any;
    let partial = state.take_stream_partial();
    let lines = finalize_lines(blocks, started, partial);
    if !lines.is_empty() {
        state.streaming_committed_any = true;
        commit_lines_compact(terminal, lines)?;
    }
    Ok(())
}

fn render_chat(frame: &mut ratatui::Frame, state: &mut AppState) {
    chat::render(frame, frame.area(), state);
}

fn render_dashboard(frame: &mut ratatui::Frame, state: &mut AppState) {
    dashboard::render(frame, frame.area(), state);
}

/// Number of blank rows appended after each scrollback commit (user
/// message, assistant reply, log line, resolved approval, banner…) to
/// visually separate consecutive entries. Bumping this widens the
/// breathing room between messages.
const ENTRY_SPACING_ROWS: u16 = 1;

/// Push lines into the terminal's native scrollback (above the inline
/// viewport). No-op if `lines` is empty. The buffer height passed to
/// `insert_before` is computed from wrapped width so long lines don't
/// get clipped. A trailing blank row is appended for visual separation
/// from whatever gets committed next.
fn commit_lines(terminal: &mut Term, mut lines: Vec<Line<'static>>) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    for _ in 0..ENTRY_SPACING_ROWS {
        lines.push(Line::from(""));
    }
    commit_lines_compact(terminal, lines)
}

/// Like [`commit_lines`] but skips the trailing blank-row separator.
/// Used by the per-line streaming committer so consecutive stream
/// lines stack tightly together (one assistant response reads as one
/// block); the trailing blank is emitted exactly once by
/// `finalize_stream` after the response completes.
fn commit_lines_compact(terminal: &mut Term, lines: Vec<Line<'static>>) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let width = terminal.size()?.width.max(1);
    let height = chat::wrapped_height(&lines, width);
    terminal.insert_before(height, |buf| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
        elide_wide_char_continuations(buf);
    })?;
    Ok(())
}

/// Work around a ratatui-side bug: `Terminal::insert_before` writes
/// every cell of the buffer to the backend regardless of whether the
/// cell is the trailing half of a wide (CJK / emoji / boxdrawing)
/// grapheme. Those trailing cells default to `Cell::EMPTY`, whose
/// `symbol()` returns `" "` — so the terminal emits a literal space
/// *after* each wide character, producing the "中 文 之 间 有 空 格"
/// effect when committing CJK content to scrollback. `Terminal::flush`
/// doesn't have the bug because `Buffer::diff` honours wide-char
/// `to_skip` counters; only the scrollback-insert path goes through
/// `draw_lines` which iterates raw cells.
///
/// The fix: rewrite the continuation cells' symbol to `""` so the
/// backend's `Print("")` is a no-op and the wide grapheme keeps its
/// natural double-column footprint.
fn elide_wide_char_continuations(buf: &mut ratatui::buffer::Buffer) {
    use unicode_width::UnicodeWidthStr;
    let width = buf.area.width as usize;
    let height = buf.area.height as usize;
    for y in 0..height {
        let mut x = 0;
        while x < width {
            let idx = y * width + x;
            let cell_width = buf.content[idx].symbol().width().max(1);
            for offset in 1..cell_width {
                let cont = x + offset;
                if cont < width {
                    buf.content[y * width + cont].set_symbol("");
                }
            }
            x += cell_width;
        }
    }
}

fn commit_banner(terminal: &mut Term, session_id: &SessionId) -> io::Result<()> {
    let width = terminal.size()?.width.max(20);
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".to_string());
    let lines =
        chat::render_banner_lines(session_id.as_str(), &cwd, env!("CARGO_PKG_VERSION"), width);
    // The banner lands just above the inline viewport (no top padding).
    // As the conversation grows, each `insert_before` call inserts a
    // message just above the viewport too, which pushes the banner
    // upward one row at a time — so new messages always appear directly
    // below the banner with no gap. The banner eventually scrolls off
    // the top once conversation length exceeds the screen height; until
    // then it stays adjacent to the latest message.
    //
    // No-wrap commit so the box-drawing chars don't get word-wrapped on
    // narrow terminals (the banner already clipped its content to fit).
    commit_lines_no_wrap(terminal, lines)
}

/// Insert lines into scrollback without word-wrapping. Used for content
/// like the framed banner where line lengths are pre-computed and any
/// wrap would mangle box-drawing characters. Trailing blank rows match
/// `commit_lines` so the banner is separated from the first message.
fn commit_lines_no_wrap(terminal: &mut Term, mut lines: Vec<Line<'static>>) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    for _ in 0..ENTRY_SPACING_ROWS {
        lines.push(Line::from(""));
    }
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf| {
        Paragraph::new(lines).render(buf.area, buf);
        elide_wide_char_continuations(buf);
    })?;
    Ok(())
}

enum KeyOutcome {
    Continue,
    Exit,
    /// Signal from `handle_key` that the user requested leaving the
    /// dashboard. The caller in `run_loop` owns the alt-screen atomic
    /// and performs the actual terminal-mode swap.
    DashboardExit,
}

async fn handle_key(
    state: &mut AppState,
    ctx: &mut LoopCtx,
    terminal: &mut Term,
    key: KeyEvent,
) -> anyhow::Result<KeyOutcome> {
    let completion_open = !state.completion_candidates().is_empty();
    let key_ctx = KeyContext {
        input_empty: state.input.is_empty(),
        completion_open,
        in_history_mode: state.history_cursor.is_some(),
        approval_open: state.approval_pending(),
    };
    let action = translate(&state.mode, key, key_ctx);
    if state.history_cursor.is_some()
        && !matches!(
            action,
            Action::HistoryPrev | Action::HistoryNext | Action::Nothing
        )
    {
        state.history_cursor = None;
    }
    if !matches!(action, Action::ConfirmExit | Action::Nothing) {
        state.confirm_exit_at = None;
    }
    match action {
        Action::Insert(c) => state.insert_char(c),
        Action::Backspace => state.backspace(),
        Action::Delete => state.delete_char(),
        Action::MoveLeft => state.move_left(),
        Action::MoveRight => state.move_right(),
        Action::MoveHome => state.move_home(),
        Action::MoveEnd => state.move_end(),
        Action::HistoryPrev => {
            if state.cursor_at_first_line() {
                state.history_prev();
            } else {
                state.move_up_line();
            }
        }
        Action::HistoryNext => {
            if state.cursor_at_last_line() {
                state.history_next();
            } else {
                state.move_down_line();
            }
        }
        Action::ClearScrollback => {
            wipe_terminal_scrollback(terminal)?;
            commit_banner(terminal, &ctx.input.current_session_id())?;
        }
        Action::CompletionPrev => state.completion_select_prev(),
        Action::CompletionNext => state.completion_select_next(),
        Action::CompletionAccept => {
            state.completion_accept();
        }
        Action::Cancel => {
            state.input.clear();
            state.cursor = 0;
        }
        Action::Shutdown => return Ok(KeyOutcome::Exit),
        Action::ConfirmExit => {
            let now = Instant::now();
            match state.confirm_exit_at {
                Some(at) if now.duration_since(at) <= CONFIRM_EXIT_WINDOW => {
                    return Ok(KeyOutcome::Exit);
                }
                _ => {
                    state.confirm_exit_at = Some(now);
                    let msg = format!(
                        "Press Ctrl-D again within {}s to exit.",
                        CONFIRM_EXIT_WINDOW.as_secs()
                    );
                    commit_lines(terminal, chat::render_system_lines(&msg))?;
                }
            }
        }
        Action::ApprovalApprove => {
            resolve_approval(state, terminal, aura_tools::ApprovalDecision::Approve)?;
        }
        Action::ApprovalApproveAlways => {
            resolve_approval(state, terminal, aura_tools::ApprovalDecision::ApproveAlways)?;
        }
        Action::ApprovalDeny => {
            resolve_approval(state, terminal, aura_tools::ApprovalDecision::Deny)?;
        }
        Action::ApprovalSelectPrev => {
            state.approval_select_prev();
        }
        Action::ApprovalSelectNext => {
            state.approval_select_next();
        }
        Action::ApprovalConfirm => {
            if let Some(decision) = state.active_approval_selected_decision() {
                resolve_approval(state, terminal, decision)?;
            }
        }
        Action::DashboardExit => {
            // The alt-screen flag is owned by run_loop; thread it through.
            // We re-fetch it from state's mode at the call boundary above
            // — see the AppEvent::DashboardReady arm for the matching
            // enter path. Both arms must use the same atomic.
            return Ok(KeyOutcome::DashboardExit);
        }
        Action::DashboardSelectPrev => state.dashboard_select_prev(),
        Action::DashboardSelectNext => state.dashboard_select_next(),
        Action::DashboardPageUp => state.dashboard_page(-10),
        Action::DashboardPageDown => state.dashboard_page(10),
        Action::DashboardRefresh => {
            if let (Some(kind), Some(provider)) =
                (state.dashboard_kind(), ctx.dashboard_provider.as_ref())
            {
                spawn_dashboard_fetch(kind, Arc::clone(provider), ctx.event_tx.clone());
            }
        }
        Action::Submit => {
            if let Some(text) = state.take_input() {
                persist_history_entry(ctx, text.clone());
                // Exit commands take effect immediately regardless of
                // whether the agent is mid-response.
                if text == "/quit" || text == "/exit" {
                    return Ok(KeyOutcome::Exit);
                }
                // Park submissions while a stream is in flight or an
                // approval is pending so they commit + dispatch *after*
                // the current response finalises. Otherwise the
                // scrollback order would interleave (`you>1 / you>2 /
                // aura>1 …`) which looks like message corruption.
                if state.is_busy() {
                    state.queue_submission(text);
                } else {
                    dispatch_submission(state, ctx, terminal, text).await?;
                }
            }
        }
        Action::Nothing => {}
    }
    Ok(KeyOutcome::Continue)
}

/// Commit + dispatch a single user submission. Used both by the Submit
/// key handler (when not busy) and by the `Outgoing` arm of the run
/// loop when draining a previously-queued submission. Slash commands
/// route through the same path so `/clear`, `/new`, etc. still work
/// when entered while the agent is busy.
async fn dispatch_submission(
    state: &mut AppState,
    ctx: &LoopCtx,
    terminal: &mut Term,
    text: String,
) -> io::Result<()> {
    if text == "/clear" {
        wipe_terminal_scrollback(terminal)?;
        commit_banner(terminal, &ctx.input.current_session_id())?;
    } else if text.starts_with('/') {
        handle_slash(state, ctx, terminal, text).await?;
    } else {
        commit_lines(terminal, chat::render_user_lines(&text))?;
        state.note_response_pending();
        dispatch_user_message(ctx, text).await;
    }
    Ok(())
}

/// Commit the resolved approval summary to scrollback and clear (or roll
/// over) the live entry. Mirrors the previous in-scrollback collapse —
/// the user sees a one-line audit trail.
fn resolve_approval(
    state: &mut AppState,
    terminal: &mut Term,
    decision: aura_tools::ApprovalDecision,
) -> io::Result<()> {
    let Some(outcome) = state.resolve_active_approval(decision) else {
        return Ok(());
    };
    commit_lines(
        terminal,
        chat::render_approval_resolved_lines(&outcome.resolved),
    )?;
    Ok(())
}

async fn handle_slash(
    state: &mut AppState,
    ctx: &LoopCtx,
    terminal: &mut Term,
    text: String,
) -> io::Result<()> {
    let Some(handler) = ctx.slash_handler.as_ref() else {
        let msg = format!("(no slash handler; ignored: {text})");
        commit_lines(terminal, chat::render_system_lines(&msg))?;
        return Ok(());
    };
    match handler.handle(&text).await {
        SlashOutcome::Handled(blocks) => {
            commit_lines(terminal, chat::render_assistant_lines(&blocks))?;
        }
        SlashOutcome::OpenView(kind) => match ctx.dashboard_provider.as_ref() {
            Some(provider) => {
                spawn_dashboard_fetch(kind, Arc::clone(provider), ctx.event_tx.clone());
            }
            None => {
                let msg = format!("(no dashboard provider; cannot open view: {kind:?})");
                commit_lines(terminal, chat::render_system_lines(&msg))?;
            }
        },
        SlashOutcome::PassThrough => {
            commit_lines(terminal, chat::render_user_lines(&text))?;
            state.note_response_pending();
            dispatch_user_message(ctx, text).await;
        }
        SlashOutcome::Exit => {
            let _ = ctx.event_tx.try_send(AppEvent::Shutdown);
        }
        SlashOutcome::NewSession => {
            let new_id = mint_new_session_id();
            match ctx.input.switch_session(new_id.clone()).await {
                Ok(()) => {
                    wipe_terminal_scrollback(terminal)?;
                    commit_banner(terminal, &new_id)?;
                    let msg = format!("Started a fresh session: {new_id}");
                    commit_lines(terminal, chat::render_system_lines(&msg))?;
                }
                Err(e) => {
                    let msg = format!("/new failed: {e}");
                    commit_lines(terminal, chat::render_system_lines(&msg))?;
                }
            }
        }
    }
    Ok(())
}

/// Send the escape sequences that wipe the visible screen + scrollback
/// buffer, then reset ratatui's internal buffer so the next draw repaints
/// the live viewport cleanly. Used by Ctrl-L and `/clear`.
fn wipe_terminal_scrollback(terminal: &mut Term) -> io::Result<()> {
    let mut stdout = io::stdout();
    // 2J = erase visible screen, 3J = erase scrollback, H = cursor home.
    write!(stdout, "\x1b[H\x1b[2J\x1b[3J")?;
    stdout.flush()?;
    terminal.clear()?;
    Ok(())
}

/// Transition from chat (inline, main screen) → dashboard (fullscreen,
/// alternate screen). Enters the alt-screen so the chat content stays
/// preserved in the user's bash history above, enables mouse capture
/// for table navigation, and rebuilds the terminal in fullscreen mode.
fn enter_dashboard_mode(
    terminal: &mut Term,
    in_alt_screen: &Arc<std::sync::atomic::AtomicBool>,
    state: &mut AppState,
    kind: ViewKind,
    snapshot: aura_channels::DashboardSnapshot,
) -> io::Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    in_alt_screen.store(true, std::sync::atomic::Ordering::Relaxed);
    *terminal = new_dashboard_terminal()?;
    state.enter_dashboard(kind, snapshot);
    Ok(())
}

/// Transition from dashboard (fullscreen, alt-screen) → chat (inline,
/// main screen). Leaves the alt-screen so the user's bash history +
/// banner + prior conversation reappear as they were, then rebuilds the
/// inline chat terminal. ratatui anchors the new viewport at the
/// current cursor row, which is where it was before we entered the
/// dashboard (terminals restore cursor position on `LeaveAlternateScreen`).
fn exit_dashboard_mode(
    terminal: &mut Term,
    in_alt_screen: &Arc<std::sync::atomic::AtomicBool>,
    state: &mut AppState,
    current_viewport_h: &mut u16,
) -> io::Result<()> {
    state.exit_dashboard();
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    in_alt_screen.store(false, std::sync::atomic::Ordering::Relaxed);
    *current_viewport_h = desired_viewport_height(state);
    *terminal = new_chat_terminal(*current_viewport_h)?;
    Ok(())
}

async fn dispatch_user_message(ctx: &LoopCtx, text: String) {
    let msg = IncomingMessage {
        message: Message {
            id: Uuid::new_v4().to_string(),
            session_id: ctx.input.current_session_id(),
            channel: ChannelType::tui(),
            sender: ctx.user.clone(),
            content: vec![ContentBlock::Text(text)],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
        platform_msg_id: String::new(),
    };
    if let Err(e) = ctx.input.submit(msg).await {
        warn!("failed to forward TUI input: {e}");
    }
}

fn mint_new_session_id() -> SessionId {
    SessionId::from(Uuid::new_v4().to_string())
}

fn persist_history_entry(ctx: &LoopCtx, entry: String) {
    let transport = Arc::clone(&ctx.input);
    tokio::spawn(async move {
        if let Err(e) = transport.append_history(&entry).await {
            warn!("failed to append TUI input history: {e}");
        }
    });
}

fn spawn_dashboard_fetch(
    kind: ViewKind,
    provider: Arc<dyn DashboardProvider>,
    event_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let snapshot = provider.snapshot(kind).await;
        let _ = event_tx
            .send(AppEvent::DashboardReady(kind, snapshot))
            .await;
    });
}

fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            let _ = disable_raw_mode();
            // Best-effort: if we panicked while in dashboard mode, leave it.
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
            prev(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn elide_wide_char_continuations_strips_trailing_spaces() {
        // Render "中文" (each char is unicode width 2) into a 6-wide
        // buffer via Paragraph + insert_before's draw path. The
        // continuation cells default to `" "`, which would show up
        // visually as a gap after each wide char if printed verbatim.
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        Paragraph::new("中文").render(buf.area, &mut buf);
        // Pre-fix: cell 1 and cell 3 (continuations) symbol() == " ".
        assert_eq!(buf.content[1].symbol(), " ");
        assert_eq!(buf.content[3].symbol(), " ");
        elide_wide_char_continuations(&mut buf);
        // Post-fix: continuations are blanked so Print("") on the
        // backend is a no-op and the wide char keeps its 2-col span.
        assert_eq!(buf.content[0].symbol(), "中");
        assert_eq!(buf.content[1].symbol(), "");
        assert_eq!(buf.content[2].symbol(), "文");
        assert_eq!(buf.content[3].symbol(), "");
    }

    #[test]
    fn elide_wide_char_continuations_leaves_ascii_intact() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        Paragraph::new("hello!").render(buf.area, &mut buf);
        elide_wide_char_continuations(&mut buf);
        assert_eq!(buf.content[0].symbol(), "h");
        assert_eq!(buf.content[1].symbol(), "e");
        assert_eq!(buf.content[5].symbol(), "!");
    }

    /// Concatenate the visible text of every span on a line.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn finalize_renders_body_when_nothing_streamed() {
        // Non-streaming delivery (cron / subagent-notification): the
        // Message arrives with no preceding deltas, so its text must
        // render from the blocks rather than be dropped.
        let blocks = vec![ContentBlock::Text("cron result".into())];
        let lines = finalize_lines(&blocks, false, None);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("cron result"),
            "body must render: {joined:?}"
        );
        assert!(
            lines
                .first()
                .is_some_and(|l| line_text(l).starts_with("aura> ")),
            "first line carries the aura> leader: {joined:?}"
        );
    }

    #[test]
    fn finalize_skips_text_when_body_streamed() {
        // A streamed response already committed its text line-by-line, so
        // the final blocks' Text must not be re-rendered (no duplicate).
        let blocks = vec![ContentBlock::Text("already streamed".into())];
        let lines = finalize_lines(&blocks, true, None);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains("already streamed"),
            "streamed text must not be re-rendered: {joined:?}"
        );
    }

    #[test]
    fn finalize_uses_partial_not_blocks_for_short_stream() {
        // A short streamed response leaves a trailing partial (no newline)
        // without ever committing a complete line. The partial is the
        // body — render it once, don't also re-render from blocks.
        let blocks = vec![ContentBlock::Text("hello".into())];
        let lines = finalize_lines(&blocks, false, Some("hello".into()));
        let hits = lines
            .iter()
            .map(line_text)
            .filter(|t| t.contains("hello"))
            .count();
        assert_eq!(hits, 1, "exactly one 'hello' line, no duplicate");
    }

    #[test]
    fn finalize_empty_message_is_noop() {
        // Nothing streamed and no renderable blocks → no lines, not even
        // a stray trailing blank.
        assert!(finalize_lines(&[], false, None).is_empty());
    }
}
