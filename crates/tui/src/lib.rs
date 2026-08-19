//! Ratatui-based TUI for `baybo tui`.
//!
//! The TUI is a thin client on top of an `baybo-gateway` reached over
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
mod markdown;
/// Scenario contract shared by the `chat_smoke` probe and its render test.
#[cfg(any(test, feature = "test-support"))]
pub mod smoke_contract;
pub mod transport;

pub use baybo_tools::ApprovalQueue;
pub use event::{LogLevel, LogRecord, TuiLogSink};
pub use transport::{TransportEvent, TransportEventStream};

use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::Arc;
use std::sync::OnceLock;

use baybo_channels::{
    ChannelError, DashboardProvider, IncomingMessage, Message, NoticeLevel, Result, STOP_COMMAND,
    STOP_COMMAND_NAME, SlashHandler, SlashOutcome, ViewKind,
};
use baybo_model::{ChannelType, SessionId, User};
use baybo_model::{ContentBlock, MessageMetadata};
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
use tokio::time::Instant as TokioInstant;
use tracing::{error, warn};
use uuid::Uuid;

use std::time::{Duration, Instant};

use crate::app::{
    AppState, ApprovalChatEntry, ApprovalChatState, BlockKind, CONFIRM_EXIT_WINDOW,
    TranscriptBlock, ViewMode,
};
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
    model_label: Option<String>,
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
            model_label: None,
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

    pub fn with_model_label(mut self, label: Option<String>) -> Self {
        self.model_label = label
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
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
            model_label: self.model_label.clone(),
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
    model_label: Option<String>,
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
                TransportEvent::ToolStarted {
                    call_id,
                    tool,
                    label,
                } => Some(AppEvent::ToolStarted {
                    call_id,
                    tool,
                    label,
                }),
                TransportEvent::ToolCompleted {
                    call_id,
                    status,
                    summary,
                } => Some(AppEvent::ToolCompleted {
                    call_id,
                    status,
                    summary,
                }),
                TransportEvent::Status { phase } => Some(AppEvent::Status { phase }),
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
/// content, so there's no empty gap between the latest scrollback message
/// and the live region. The cursor is left wherever the shell put it —
/// ratatui's `compute_inline_size` anchors the viewport at that row.
fn new_chat_terminal(viewport_h: u16) -> io::Result<Term> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_h.max(1)),
        },
    )
}

/// Desired inline-viewport height for the current state. The working zone keeps
/// one spacer row while idle and adds the indicator row only while busy.
/// Streaming/tool text commits straight into scrollback, so it doesn't add
/// here.
fn desired_viewport_height(state: &AppState) -> u16 {
    let mut h = chat::input_box_height(state).saturating_add(chat::working_zone_height(state));
    h = h.saturating_add(chat::model_footer_height(state));
    h = h.saturating_add(state.queued_display_lines().count() as u16);
    if let Some(entry) = state.pending_approval.as_ref() {
        h = h.saturating_add(chat::approval_pending_height(entry));
    }
    h = h.saturating_add(chat::completion_popup_height(state));
    h.max(1)
}

/// Build a fresh `Terminal` configured for dashboard mode (fullscreen).
/// The session is already inside the alternate screen; the caller toggles
/// mouse capture around this if dashboard interactions need it.
fn new_dashboard_terminal() -> io::Result<Term> {
    let backend = CrosstermBackend::new(io::stdout());
    Terminal::new(backend)
}

/// How long a terminal-resize burst must go quiet before we rebuild the inline
/// viewport. A drag-resize emits many `Resize` events in quick succession;
/// reacting to each one re-anchors the inline viewport and prints newlines (see
/// [`rebuild_chat_terminal_after_resize`]), so we coalesce them and rebuild once
/// the size stops changing. The window sits above a per-frame (~16ms) event
/// cadence so a continuous drag never fires it mid-gesture, yet it feels instant
/// on release.
const RESIZE_COALESCE_WINDOW: std::time::Duration = std::time::Duration::from_millis(25);

/// Tick cadence for the live "working" indicator: each tick advances the
/// dot's colour pulse and repaints (which also refreshes the elapsed
/// counter). ~300ms is a calm pulse — slower than the original 100ms shape
/// spinner — while keeping the counter within its 1s granularity. The select
/// arm is gated on [`AppState::show_working`], so an idle TUI never wakes.
const WORKING_TICK: std::time::Duration = std::time::Duration::from_millis(300);

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
    state.session_id = ctx.initial_session_id.clone();
    state.set_model_label(ctx.model_label.clone());
    if let Some(handler) = ctx.slash_handler.as_ref() {
        state.set_commands(handler.commands());
    }
    if let Some(entries) = ctx.input.take_history_snapshot().await {
        state.set_history(entries);
    }
    // Clear the screen on entry so the TUI starts on a clean slate (cursor
    // home) rather than below whatever was in the shell. Done before the
    // inline terminal is created so its viewport anchors at the top.
    home_and_clear_screen()?;
    let mut current_viewport_h = desired_viewport_height(&state);
    let mut terminal = new_chat_terminal(current_viewport_h)
        .map_err(|e| anyhow::anyhow!("new_chat_terminal: {e}"))?;
    let mut term_events = EventStream::new();

    commit_banner(&mut state, &mut terminal, &ctx.initial_session_id)?;
    terminal.draw(|f| render_chat(f, &mut state))?;

    // Debounce timer for coalescing terminal-resize bursts. Parked far in the
    // future and re-armed on each `Resize`; the select arm is gated on
    // `resize_pending` so the parked timer never fires spuriously.
    let resize_debounce = tokio::time::sleep(std::time::Duration::from_secs(86_400));
    tokio::pin!(resize_debounce);
    let mut resize_pending = false;

    // Refresh clock for the working indicator's elapsed counter. Only polled
    // while a turn is in flight (its select arm is gated on `working`), so an
    // idle loop never wakes on it. Skip missed ticks so resuming after an
    // idle stretch doesn't fire a burst.
    let mut working_refresh = tokio::time::interval(WORKING_TICK);
    working_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Recomputed each iteration so the refresh arm enables/disables as
        // turns start and end. Cheap (Copy bool); the borrow ends before any
        // arm body mutably borrows `state`.
        let working = state.show_working();
        tokio::select! {
            biased;
            _ = ctx.shutdown.notified() => break Ok(()),
            term = term_events.next() => {
                match term {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match handle_key(
                                &mut state,
                                &mut ctx,
                                &mut terminal,
                                &mut current_viewport_h,
                                key,
                            ).await? {
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
                        // Coalesce the burst: arm/re-arm the debounce and let the
                        // resize_debounce arm rebuild once it settles. Redrawing
                        // here would route through ratatui's in-place inline
                        // resize, which re-anchors from a pre-reflow cursor offset
                        // and garbles the live region.
                        resize_pending = true;
                        resize_debounce
                            .as_mut()
                            .reset(TokioInstant::now() + RESIZE_COALESCE_WINDOW);
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
                            // Any steer still pending never met a tool
                            // boundary this turn, so it fell to the next turn
                            // server-side — commit its `you>` line now, after
                            // this turn's reply and before that turn arrives.
                            commit_pending_interjections(&mut state, &mut terminal)?;
                        }
                        state.clear_stream();
                        state.note_response_received();
                        // Drain deferred slash commands now that the turn has
                        // ended. A passthrough slash re-arms
                        // `outstanding_responses` (busy) and breaks out; a
                        // client-side slash like `/skills` or `/clear` doesn't
                        // touch the agent and lets the next item run in the
                        // same cycle.
                        while !state.is_busy()
                            && let Some(next) = state.dequeue_submission()
                        {
                            dispatch_submission(
                                &mut state,
                                &ctx,
                                &mut terminal,
                                &mut current_viewport_h,
                                next,
                            ).await?;
                        }
                        // This redraw can rebuild the inline terminal (a pending
                        // resize, or a viewport-height change as an approval
                        // clears) — a cursor query. We're on a non-keyboard
                        // wake-up with the reader parked, so route it through the
                        // guard that drops the stream around any such query.
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::StreamDelta(delta) => {
                        state.ensure_working_clock();
                        state.append_stream_delta(&delta);
                        if matches!(state.mode, ViewMode::Chat) {
                            term_events = resize_chat_viewport_before_scrollback(
                                term_events,
                                &mut terminal,
                                &mut state,
                                &mut current_viewport_h,
                            )?;
                            flush_complete_stream_lines(&mut state, &mut terminal)?;
                        }
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::ToolStarted {
                        call_id,
                        tool,
                        label,
                    } => {
                        state.ensure_working_clock();
                        if matches!(state.mode, ViewMode::Chat) {
                            term_events = resize_chat_viewport_before_scrollback(
                                term_events,
                                &mut terminal,
                                &mut state,
                                &mut current_viewport_h,
                            )?;
                            // Flush any answer text the model emitted before
                            // invoking the tool, so it lands above the tool
                            // block.
                            flush_answer_boundary(&mut state, &mut terminal)?;
                        }
                        // The `● tool` line is deferred: it renders live in the
                        // working zone now and commits to scrollback paired with
                        // its `⎿` result only on completion, so concurrent tools
                        // never detach their results. See docs/modules/tui.md.
                        state.push_running_tool(call_id, tool, label);
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::ToolCompleted {
                        call_id,
                        status,
                        summary,
                    } => {
                        // Drop completions for a call we're no longer tracking:
                        // after `/stop`, `reset_working` clears the running set,
                        // but a tool that observes cancellation can still emit a
                        // late `ToolCompleted` before the agent loop unwinds.
                        // Committing it would strand a stray `⎿` result in the
                        // now-idle scrollback, so a non-matching id is ignored.
                        if let Some(running) = state.take_running_tool(&call_id)
                            && matches!(state.mode, ViewMode::Chat)
                        {
                            term_events = resize_chat_viewport_before_scrollback(
                                term_events,
                                &mut terminal,
                                &mut state,
                                &mut current_viewport_h,
                            )?;
                            flush_answer_boundary(&mut state, &mut terminal)?;
                            // Commit the whole tool block as one unit now that it
                            // is done — the deferred `● tool` head, any approval
                            // resolved mid-call, then the `⎿` result — so the
                            // result sits under its own tool even when siblings
                            // ran concurrently. See `chat::tool_completed_block`.
                            let block = chat::tool_completed_block(&running, &status, &summary);
                            commit_tool_lines(&mut state, &mut terminal, block)?;
                            // The agent runs each tool batch concurrently and only
                            // folds a mid-turn steer in once the whole batch
                            // drains, so hold the queued `you>` line until the
                            // last tool completes (`running_tools` empties).
                            // Committing it after an earlier sibling would place
                            // it above results the model actually saw first.
                            if state.running_tools().is_empty() {
                                commit_pending_interjections(&mut state, &mut terminal)?;
                            }
                        }
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::Status { phase } => {
                        if matches!(state.mode, ViewMode::Chat) {
                            commit_below_answer(
                                &mut state,
                                &mut terminal,
                                chat::render_status_line(&phase),
                            )?;
                        }
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
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
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::Log(record) => {
                        if matches!(state.mode, ViewMode::Chat) {
                            commit_below_answer(&mut state, &mut terminal, chat::render_log_lines(&record))?;
                        }
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::ApprovalRequested => {
                        if !state.approval_pending()
                            && let Some(queue) = state.approval.as_ref()
                            && let Some(req) = queue.peek_head()
                        {
                            state.set_pending_approval(ApprovalChatEntry {
                                call_id: req.call_id,
                                tool: req.tool,
                                accesses: req.accesses,
                                params_preview: req.params_preview,
                                state: ApprovalChatState::Pending { selected: 0 },
                            });
                        }
                        // Showing the approval grows the viewport, so this draw
                        // rebuilds the inline terminal (a cursor query) on a
                        // non-keyboard wake-up — the guard pauses the reader for it.
                        term_events = redraw_after_event(
                            term_events,
                            &mut terminal,
                            &mut state,
                            &mut current_viewport_h,
                            &mut resize_pending,
                        )?;
                    }
                    AppEvent::Shutdown => break Ok(()),
                }
            }
            _ = &mut resize_debounce, if resize_pending => {
                // The resize burst settled with no AppEvent in between (the idle
                // path — an interleaved AppEvent would have applied the resize via
                // `redraw_after_event` and cleared the flag, disabling this arm).
                // Re-anchor the inline viewport at the new size; the guard drops
                // the stream for the cursor query and clears `resize_pending`.
                term_events = redraw_after_event(
                    term_events,
                    &mut terminal,
                    &mut state,
                    &mut current_viewport_h,
                    &mut resize_pending,
                )?;
            }
            _ = working_refresh.tick(), if working => {
                // Advance the dot's colour pulse and repaint (also refreshes
                // the elapsed counter). No scrollback commit and (steady
                // state) no height change, so `redraw_after_event` does a
                // cheap in-place draw without a cursor query.
                state.tick_spinner();
                term_events = redraw_after_event(
                    term_events,
                    &mut terminal,
                    &mut state,
                    &mut current_viewport_h,
                    &mut resize_pending,
                )?;
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

/// Resize the inline chat viewport when the live content's footprint changes
/// (input grew a line, approval popped/resolved, working indicator toggled).
/// ratatui's `Viewport::Inline(_)` height is fixed at construction, so this
/// drops the existing terminal and rebuilds one with the new height. We clear
/// the old viewport first so its tail content doesn't linger beyond the new
/// (potentially smaller) viewport area.
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

/// Ensure the inline viewport has its current height before writing historical
/// lines above it. With a dynamic working row, inserting into scrollback before
/// growing the viewport can put the new line where the live region is about to
/// be, making tool/status text appear to overwrite the working indicator.
fn resize_chat_viewport_before_scrollback(
    term_events: EventStream,
    terminal: &mut Term,
    state: &mut AppState,
    current_viewport_h: &mut u16,
) -> anyhow::Result<EventStream> {
    if desired_viewport_height(state) == *current_viewport_h {
        return Ok(term_events);
    }
    drop(term_events);
    resize_chat_viewport_if_needed(state, terminal, current_viewport_h)?;
    terminal.draw(|f| render_chat(f, state))?;
    Ok(EventStream::new())
}

/// Refresh the screen after a terminal-resize burst settles.
///
/// A width change makes the terminal reflow its native scrollback (shell
/// history + our committed lines), and the old full-width live region (input
/// box border, message bars) reflows into stale fragment rows that the inline
/// viewport can't precisely erase — `insert_before` scrolls rather than
/// overwrites, so they drift upward as ghosting. The post-reflow physical
/// layout isn't exposed by stock ratatui/crossterm, so there's no surgical
/// clear (that's Codex `custom_terminal` fork territory).
///
/// Instead we **refresh**: clear the screen + scrollback for a clean slate,
/// anchor a fresh viewport, reprint the banner, then replay the retained
/// [`TranscriptBlock`] log so the conversation re-renders cleanly at the new
/// width (width-dependent blocks like the user bar are rebuilt to fit). The
/// session itself is untouched; only the bounded replay log is reprinted, so
/// history beyond [`crate::app`]'s cap — and any shell output above the
/// original launch point — does not come back.
fn rebuild_chat_terminal_after_resize(
    terminal: &mut Term,
    state: &mut AppState,
    current: &mut u16,
) -> io::Result<()> {
    // A resize mid-turn would otherwise replay answer source whose rows were
    // never rendered, and the block would commit again when it closes —
    // duplicating the text. Flushing first makes record and screen agree before
    // either is reprinted.
    flush_answer_boundary(state, terminal)?;
    // Home the cursor + wipe, then drop the old terminal for a fresh one whose
    // viewport anchors at the homed cursor (row 0) — so the banner lands at the
    // top. Calling `Terminal::clear` here would move the cursor off home before
    // the new viewport anchors. See [`home_and_clear_screen`].
    home_and_clear_screen()?;
    let desired = desired_viewport_height(state);
    *terminal = new_chat_terminal(desired)?;
    *current = desired;
    let session_id = state.session_id.clone();
    commit_banner(state, terminal, &session_id)?;
    replay_transcript(state, terminal)?;
    Ok(())
}

/// Clear the visible screen + scrollback for an intentional chat reset
/// (`/clear`, `/new`, Ctrl-L), then recreate the inline terminal so the next
/// banner anchors at row 0 instead of the previous bottom viewport.
fn reset_chat_scrollback(
    state: &mut AppState,
    terminal: &mut Term,
    current: &mut u16,
    session_id: &SessionId,
) -> io::Result<()> {
    home_and_clear_screen()?;
    let desired = desired_viewport_height(state);
    *terminal = new_chat_terminal(desired)?;
    *current = desired;
    state.clear_transcript();
    commit_banner(state, terminal, session_id)?;
    Ok(())
}

/// Re-commit the retained transcript after a resize refresh wiped the screen.
/// Blocks replay in commit order through the same commit helpers, so the
/// leading-separator spacing is reproduced exactly and the helpers re-record
/// each block — rebuilding `state.transcript` to the same content (taking it
/// out first avoids a borrow clash and double-counting against the cap).
fn replay_transcript(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    // Live, a response draws the `● ` dot once and indents every row after it,
    // even across an interleaved tool block. Replay sees that response as several
    // `Answer` records, so it has to remember whether the dot is already spent.
    // A user message opens a response; the `cooked for` footer closes it.
    let mut answered = false;
    for block in state.take_transcript() {
        match block {
            TranscriptBlock::User(text) => {
                answered = false;
                commit_user_message(state, terminal, &text)?;
            }
            TranscriptBlock::Answer(source) => {
                replay_answer(state, terminal, source, answered)?;
                answered = true;
            }
            TranscriptBlock::Tool(lines) => commit_tool_lines(state, terminal, lines)?,
            TranscriptBlock::Other(lines) => commit_lines(state, terminal, lines)?,
            TranscriptBlock::Cooked(elapsed) => {
                answered = false;
                commit_cooked(state, terminal, elapsed)?;
            }
        }
    }
    Ok(())
}

/// Re-render a recorded answer run at the current width and re-commit it. The
/// run's whole source is re-parsed as one document, so a code fence that spanned
/// several live commits comes back as a fence, and every row is re-wrapped to the
/// new width instead of replaying the old geometry.
fn replay_answer(
    state: &mut AppState,
    terminal: &mut Term,
    source: Vec<String>,
    continuation: bool,
) -> io::Result<()> {
    let width = chat::answer_content_width(scrollback_width(terminal));
    let rows = markdown::render_document(&source.join("\n"), width);
    let rows = chat::render_answer_rows(rows, continuation);
    commit_answer(state, terminal, rows, &source, None)
}

/// Redraw after a non-keyboard wake-up (an `AppEvent`, or the resize debounce),
/// dropping the terminal event stream around the draw when it will query the
/// cursor. Two things make a chat-mode draw query the cursor (DSR `ESC[6n`):
///
/// - **A pending terminal resize** we haven't applied yet: `terminal`'s known
///   size is stale, so `draw_active`'s `autoresize` re-anchors the inline
///   viewport via `cursor::position()`. We pre-empt that with our own clean
///   rebuild ([`rebuild_chat_terminal_after_resize`]) so the viewport doesn't
///   get the stale-offset garble, then let the draw run without a resize.
/// - **A live-region height change** (approval shown/cleared, input grew):
///   `resize_chat_viewport_if_needed` inside `draw_active` rebuilds.
///
/// `crossterm::cursor::position()` can only read its reply off stdin when no
/// `EventStream` is polling it; on these wake-ups the stream's reader thread is
/// parked holding crossterm's internal reader, so the query would time out and
/// the error would tear down the loop. Dropping the stream releases the reader
/// for the query; we recreate it after (keys typed meanwhile stay buffered in
/// the tty). `resize_pending` is consumed — this redraw applies it.
///
/// Returns the (possibly recreated) stream so the caller can rebind it.
/// Keyboard-event redraws don't use this: they run while the reader is free
/// (the key was just delivered), so a plain `draw_active` is safe there.
fn redraw_after_event(
    term_events: EventStream,
    terminal: &mut Term,
    state: &mut AppState,
    current_viewport_h: &mut u16,
    resize_pending: &mut bool,
) -> anyhow::Result<EventStream> {
    let pending_resize = std::mem::replace(resize_pending, false);
    let queries_cursor = matches!(state.mode, ViewMode::Chat)
        && (pending_resize || desired_viewport_height(state) != *current_viewport_h);
    if !queries_cursor {
        draw_active(terminal, state, current_viewport_h)?;
        return Ok(term_events);
    }
    drop(term_events);
    let mut result: io::Result<()> = Ok(());
    if pending_resize {
        result = rebuild_chat_terminal_after_resize(terminal, state, current_viewport_h);
    }
    if result.is_ok() {
        result = draw_active(terminal, state, current_viewport_h);
    }
    let stream = EventStream::new();
    result?;
    Ok(stream)
}

/// Commit a single blank scrollback row.
fn commit_blank(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    commit_lines_compact(state, terminal, vec![Line::from("")])
}

/// Emit one separator blank row *unless* the last committed row is already
/// blank — so consecutive blocks are always single-spaced, never doubled.
fn commit_separator(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    if !state.last_row_blank {
        commit_blank(state, terminal)?;
    }
    Ok(())
}

/// Open a scrollback block of the given kind, emitting a single leading
/// separator blank when the kind changes (and always for an `Other` block).
/// Same-kind `Answer`/`Tool` lines stack tight as one block; the separator
/// dedups against `last_row_blank`. There are no trailing blanks — the
/// reserved working row separates the final block from the input box.
fn begin_block(state: &mut AppState, terminal: &mut Term, kind: BlockKind) -> io::Result<()> {
    let continues = kind != BlockKind::Other && state.last_block == Some(kind);
    if !continues {
        commit_separator(state, terminal)?;
    }
    state.last_block = Some(kind);
    Ok(())
}

/// Commit a tool-block line (`●` / `⎿` / a resolved-approval summary) as part
/// of the current tool block (opening it with a leading separator if the
/// previous block was a different kind) and record it for resize replay.
fn commit_tool_lines(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let recorded = lines.clone();
    begin_block(state, terminal, BlockKind::Tool)?;
    commit_lines_compact(state, terminal, lines)?;
    state.record_block(TranscriptBlock::Tool(recorded));
    Ok(())
}

/// Commit an assistant-answer block (opening it with a leading separator if the
/// previous block was a different kind) and record its markdown `source` for
/// resize replay.
///
/// `rows` are already wrapped to [`scrollback_width`], so this takes the no-wrap
/// insert where the buffer height is `rows.len()` exactly — `wrapped_height`'s
/// word-wrap estimate under-counts and would drop the tail. `source` is recorded
/// even when a chunk renders no rows (a bare fence opener), because replay
/// re-parses the run as one document and needs every source line.
fn commit_answer(
    state: &mut AppState,
    terminal: &mut Term,
    rows: Vec<Line<'static>>,
    source: &[String],
    reopen_fence: Option<String>,
) -> io::Result<()> {
    if !rows.is_empty() {
        begin_block(state, terminal, BlockKind::Answer)?;
        commit_lines_no_wrap(state, terminal, rows)?;
    }
    state.record_answer_source(source, reopen_fence);
    Ok(())
}

/// Render the answer rows a markdown chunk produced, tag them with the answer
/// leader, commit them, and latch `streaming_committed_any` once a row has
/// actually reached the screen (which is what selects the `● ` dot).
fn commit_answer_rows(
    state: &mut AppState,
    terminal: &mut Term,
    rows: Vec<Line<'static>>,
    source: &[String],
    reopen_fence: Option<String>,
) -> io::Result<()> {
    if rows.is_empty() && source.is_empty() {
        return Ok(());
    }
    let rows = chat::render_answer_rows(rows, state.streaming_committed_any);
    let committed = !rows.is_empty();
    commit_answer(state, terminal, rows, source, reopen_fence)?;
    if committed {
        state.streaming_committed_any = true;
    }
    Ok(())
}

/// Commit the `cooked for …` footer — its own deduped separator blank above it,
/// so it sits one blank below the answer/tool block regardless of which
/// preceded — and record it for resize replay (re-rendered from `elapsed`).
fn commit_cooked(state: &mut AppState, terminal: &mut Term, elapsed: Duration) -> io::Result<()> {
    commit_separator(state, terminal)?;
    commit_lines_compact(state, terminal, chat::render_cooked_for_line(elapsed))?;
    state.record_block(TranscriptBlock::Cooked(elapsed));
    Ok(())
}

/// Feed every newly-completed source line to the markdown stream and commit
/// whatever blocks that closed. A line inside an open block produces no rows
/// yet — see [`crate::markdown`] for why a block cannot be committed early.
fn flush_complete_stream_lines(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    let drained = state.drain_complete_stream_lines();
    if drained.is_empty() {
        return Ok(());
    }
    let width = chat::answer_content_width(scrollback_width(terminal));
    // Captured before the push: it describes the fence these lines continue, not
    // one they may open themselves.
    let reopen_fence = state.markdown.fence_continuation();
    let mut rows = Vec::new();
    for line in &drained {
        rows.extend(state.markdown.push_line(line, width));
    }
    commit_answer_rows(state, terminal, rows, &drained, reopen_fence)
}

/// Commit every pending mid-turn steer to scrollback as a user block,
/// emptying the pending queue. Called once a tool batch fully drains (so the
/// steer lands below the batch's results, where the agent picks it up) and
/// again as the turn-end fallback for a steer that never met a batch boundary.
fn commit_pending_interjections(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    let pending = state.take_pending_interjections();
    if pending.is_empty() {
        return Ok(());
    }
    for text in pending {
        commit_user_message(state, terminal, &text)?;
    }
    Ok(())
}

/// Commit every scrap of answer text held back so far — the trailing partial
/// line *and* any block the markdown stream is still accumulating — so the next
/// scrollback commit (a tool line, a status line, finalize) lands *below* it.
/// Without this, held-back answer text would surface at finalize *under* that
/// block, reordering the turn: `insert_before` can only append above the
/// viewport, never insert beneath a committed row.
///
/// An open code fence is deliberately left open: it buffers nothing, so its tail
/// keeps rendering as code after the interruption.
fn flush_answer_boundary(state: &mut AppState, terminal: &mut Term) -> io::Result<()> {
    let width = chat::answer_content_width(scrollback_width(terminal));
    let reopen_fence = state.markdown.fence_continuation();
    let (mut rows, mut source) = drain_answer_source(state, width);
    let closed = state.markdown.flush(width);
    if !closed.is_empty() {
        // The forced flush ended a block early. Record that boundary as a blank
        // source line, or replay re-parses the block it closed as continuing
        // into whatever the answer says next. A flush inside a fence emits
        // nothing, so this can never inject a blank into a code body.
        source.push(String::new());
        rows.extend(closed);
    }
    commit_answer_rows(state, terminal, rows, &source, reopen_fence)
}

/// Hand every source line still buffered to the markdown stream: the complete
/// lines first, then the trailing partial as its own line.
///
/// Draining before taking the partial matters because the buffer can hold
/// several lines — `flush_complete_stream_lines` only runs in Chat mode, so a
/// dashboard visit leaves whole lines behind — and `push_line` takes exactly one
/// line, so a multi-line blob would be mis-scanned.
fn drain_answer_source(state: &mut AppState, width: usize) -> (Vec<Line<'static>>, Vec<String>) {
    let mut rows = Vec::new();
    let mut source = Vec::new();
    for line in state.drain_complete_stream_lines() {
        rows.extend(state.markdown.push_line(&line, width));
        source.push(line);
    }
    if let Some(partial) = state.take_stream_partial() {
        rows.extend(state.markdown.push_line(&partial, width));
        source.push(partial);
    }
    (rows, source)
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
/// nothing streamed — a cron job with `delta_tx = None`, or a direct
/// synthetic Message such as the background-completion reply — the body never
/// reached the scrollback, so the full message is rendered from `blocks`. The
/// `cooked for` footer is committed separately by [`finalize_stream`] (with its
/// own separator), not here.
fn finalize_stream(
    state: &mut AppState,
    terminal: &mut Term,
    blocks: &[ContentBlock],
) -> io::Result<()> {
    let term_width = scrollback_width(terminal);
    let width = chat::answer_content_width(term_width);
    // Stamp the elapsed turn time only when we actually clocked this turn
    // (a local dispatch / streaming turn). Cron / non-streaming deliveries
    // leave `working_since` unset and get no footer.
    let cooked = state.working_since.map(|since| since.elapsed());

    let reopen_fence = state.markdown.fence_continuation();
    let (mut rows, source) = drain_answer_source(state, width);
    rows.extend(state.markdown.finish(width));
    commit_answer_rows(state, terminal, rows, &source, reopen_fence)?;

    // When the body streamed it is already in scrollback, so only the extras
    // the stream never carried remain (today the CronCreate hint). When nothing
    // streamed — a cron trigger with no `delta_tx`, or a synthetic message — the
    // whole body still has to be rendered from `blocks`.
    let streamed = state.streaming_committed_any;
    let extras = finalize_extras(blocks, streamed, term_width);
    if !extras.is_empty() {
        let mut source = block_source(blocks, streamed);
        if streamed && !source.is_empty() {
            // The extras render as their own block below the streamed body, so
            // the recorded source needs the separator that implies.
            source.insert(0, String::new());
        }
        state.streaming_committed_any = true;
        commit_answer(state, terminal, extras, &source, None)?;
    }

    if let Some(elapsed) = cooked {
        commit_cooked(state, terminal, elapsed)?;
    }
    Ok(())
}

/// What a finalised response still owes scrollback. A body that streamed is
/// already there, so only the blocks the stream never carried remain (today the
/// CronCreate hint); a body that never streamed — a cron trigger with no
/// `delta_tx`, or a synthetic message — has to render in full.
fn finalize_extras(blocks: &[ContentBlock], streamed: bool, width: u16) -> Vec<Line<'static>> {
    if streamed {
        chat::render_non_text_blocks(blocks, true, width)
    } else {
        chat::render_assistant_lines(blocks, width)
    }
}

/// The markdown source behind a finalised response's blocks, for resize replay.
/// Mirrors the `skip_text` split in [`chat::render_non_text_blocks`] so replay
/// reproduces exactly what was committed.
fn block_source(blocks: &[ContentBlock], streamed: bool) -> Vec<String> {
    let mut out = Vec::new();
    for block in blocks {
        if streamed && matches!(block, ContentBlock::Text(_)) {
            continue;
        }
        if let Some(text) = chat::block_source_text(block) {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.extend(text.lines().map(str::to_string));
        }
    }
    out
}

fn render_chat(frame: &mut ratatui::Frame, state: &mut AppState) {
    chat::render(frame, frame.area(), state);
}

fn render_dashboard(frame: &mut ratatui::Frame, state: &mut AppState) {
    dashboard::render(frame, frame.area(), state);
}

/// Whether a rendered line is visually blank (no non-whitespace content) —
/// used to keep [`AppState::last_row_blank`] accurate so separators dedup.
/// Commit a non-answer block that must land *below* the answer text so far.
///
/// The flush is the ordering invariant: `insert_before` only appends above the
/// viewport, so answer text the markdown stream is still holding would resurface
/// *under* this block. Every event-driven non-answer commit goes through here so
/// a new one cannot forget — [`replay_transcript`] deliberately does not, since
/// replay must not inject live buffered content into recorded history.
fn commit_below_answer(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    flush_answer_boundary(state, terminal)?;
    commit_lines(state, terminal, lines)
}

/// The width the next `insert_before` buffer will actually have.
///
/// `Terminal::size` is a live backend query, but `insert_before` builds its
/// buffer from `viewport_area.width`, and the run loop deliberately defers
/// applying a resize (`resize_pending` plus a debounce). Pre-wrapped rows must
/// be laid out for the buffer they land in, or a row would overflow it and be
/// truncated. `Frame::area` returns that exact field.
fn scrollback_width(terminal: &mut Term) -> u16 {
    terminal.get_frame().area().width.max(1)
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Push a self-contained "Other" block (log, status, system note) into
/// scrollback: a deduplicated leading separator blank, then the content. No
/// trailing blank — the next block adds its own leading separator, and the
/// reserved working row separates the last block from the input. No-op if
/// `lines` is empty. Records the block for resize replay; user messages use
/// [`commit_user_message`] instead so they replay from text at the new width.
fn commit_lines(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let recorded = lines.clone();
    commit_other_core(state, terminal, lines)?;
    state.record_block(TranscriptBlock::Other(recorded));
    Ok(())
}

/// The "Other" block commit without the transcript record — shared by
/// [`commit_lines`] and [`commit_user_message`] so the latter records a `User`
/// entry (text, re-rendered at replay width) rather than a width-baked `Other`.
fn commit_other_core(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    begin_block(state, terminal, BlockKind::Other)?;
    commit_lines_compact(state, terminal, lines)
}

/// Commit a user message / mid-turn steer as an "Other" block and record it for
/// resize replay. Stored as raw text (not rendered lines) because its
/// full-width highlighted bar is width-dependent — replay rebuilds it at the
/// current width via `render_user_lines`.
fn commit_user_message(state: &mut AppState, terminal: &mut Term, text: &str) -> io::Result<()> {
    let width = terminal.size()?.width;
    commit_other_core(state, terminal, chat::render_user_lines(text, width))?;
    state.record_block(TranscriptBlock::User(text.to_string()));
    Ok(())
}

/// Like [`commit_lines`] but with no separator and no trailing blank — the
/// raw scrollback insert. Used by the per-line streaming committer (so an
/// answer reads as one tight block) and by the tool-run / separator helpers
/// that manage their own blanks. Tracks whether the last row landed blank so
/// [`commit_separator`] can dedup.
fn commit_lines_compact(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let width = terminal.size()?.width.max(1);
    let height = chat::wrapped_height(&lines, width);
    let last_blank = lines.last().is_some_and(is_blank_line);
    terminal.insert_before(height, |buf| {
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(buf.area, buf);
        elide_wide_char_continuations(buf);
    })?;
    state.last_row_blank = last_blank;
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

fn commit_banner(
    state: &mut AppState,
    terminal: &mut Term,
    session_id: &SessionId,
) -> io::Result<()> {
    let width = terminal.size()?.width.max(20);
    let lines = chat::render_banner_lines(session_id.as_str(), env!("CARGO_PKG_VERSION"), width);
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
    commit_lines_no_wrap(state, terminal, lines)?;
    // The banner is a fresh start (startup / `/clear` / `/new`); reset the
    // block context so the next block separates cleanly from it.
    state.last_block = None;
    Ok(())
}

/// Insert lines into scrollback without word-wrapping. Used for content
/// like the framed banner where line lengths are pre-computed and any wrap
/// would mangle box-drawing characters. No trailing blank — the reserved
/// working row separates the banner from the input, and the first message
/// adds its own leading separator.
fn commit_lines_no_wrap(
    state: &mut AppState,
    terminal: &mut Term,
    lines: Vec<Line<'static>>,
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len() as u16;
    let last_blank = lines.last().is_some_and(is_blank_line);
    terminal.insert_before(height, |buf| {
        Paragraph::new(lines).render(buf.area, buf);
        elide_wide_char_continuations(buf);
    })?;
    state.last_row_blank = last_blank;
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
    current_viewport_h: &mut u16,
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
            reset_chat_scrollback(
                state,
                terminal,
                current_viewport_h,
                &ctx.input.current_session_id(),
            )?;
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
                    commit_below_answer(state, terminal, chat::render_system_lines(&msg))?;
                }
            }
        }
        Action::ApprovalApprove => {
            resolve_approval(state, terminal, baybo_tools::ApprovalDecision::Approve)?;
        }
        Action::ApprovalApproveAlways => {
            resolve_approval(
                state,
                terminal,
                baybo_tools::ApprovalDecision::ApproveAlways,
            )?;
        }
        Action::ApprovalDeny => {
            resolve_approval(state, terminal, baybo_tools::ApprovalDecision::Deny)?;
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
                if is_stop_command(&text) {
                    // `/stop` is the interrupt: dispatch it immediately so
                    // the Router cancels the in-flight turn, and reset our
                    // live state ourselves — a cancelled turn delivers no
                    // `Outgoing` that would otherwise clear the indicator.
                    // A no-op on an idle session.
                    interrupt_turn(state, ctx, terminal).await?;
                } else if !state.is_busy() {
                    dispatch_submission(state, ctx, terminal, current_viewport_h, text).await?;
                } else if text.starts_with('/') {
                    // A slash mid-turn would either disrupt the live region
                    // (client-side `/clear`, `/new`, dashboards) or spawn a
                    // concurrent turn (passthrough), so defer it until the
                    // current turn ends and the `Outgoing` arm drains it.
                    state.queue_submission(text);
                } else {
                    // A plain message mid-turn steers the running turn:
                    // dispatch it now so the agent folds it in at its next
                    // tool boundary, and show it as a pending `↳` line until
                    // that boundary commits it to scrollback. No
                    // `note_response_pending` — the steer rides the current
                    // turn's single `Outgoing`, it doesn't add one.
                    state.queue_interjection(text.clone());
                    dispatch_user_message(ctx, text).await;
                }
            }
        }
        Action::Nothing => {}
    }
    Ok(KeyOutcome::Continue)
}

/// Commit + dispatch a single user submission. Used by the Submit key
/// handler when the agent is idle, and by the `Outgoing` arm when draining
/// a slash command that was deferred while the agent was busy. Plain
/// mid-turn messages do **not** come here — they steer via
/// [`AppState::queue_interjection`] and an immediate `dispatch_user_message`.
async fn dispatch_submission(
    state: &mut AppState,
    ctx: &LoopCtx,
    terminal: &mut Term,
    current_viewport_h: &mut u16,
    text: String,
) -> io::Result<()> {
    if text == "/clear" {
        reset_chat_scrollback(
            state,
            terminal,
            current_viewport_h,
            &ctx.input.current_session_id(),
        )?;
    } else if text.starts_with('/') {
        handle_slash(state, ctx, terminal, current_viewport_h, text).await?;
    } else {
        commit_user_message(state, terminal, &text)?;
        state.note_response_pending();
        dispatch_user_message(ctx, text).await;
    }
    Ok(())
}

/// Whether `text` is the `/stop` interrupt command (case-insensitive,
/// trailing args ignored). Mirrors the gateway's own recognizer so the TUI
/// can short-circuit it locally and reset the live region without waiting
/// for a reply the cancelled turn never sends.
fn is_stop_command(text: &str) -> bool {
    text.trim()
        .strip_prefix('/')
        .and_then(|rest| rest.split_whitespace().next())
        .is_some_and(|tok| tok.eq_ignore_ascii_case(STOP_COMMAND_NAME))
}

/// Run `/stop`: dispatch it so the Router cancels the in-flight turn, then
/// return the live region to idle. The cancel is out-of-band server-side
/// and delivers no `Outgoing`, so we flush the partial answer, commit any
/// pending steer (it stays queued server-side and runs as its own turn),
/// reset the working/streaming state, and discard any deferred slash
/// commands here. Those slashes were parked for "after this turn"; the
/// cancelled turn sends no `Outgoing` to drain them, so without this they'd
/// linger as `↳` lines and later fire after an unrelated turn. The gateway's
/// stop `Notice` then lands as the confirmation line.
async fn interrupt_turn(
    state: &mut AppState,
    ctx: &LoopCtx,
    terminal: &mut Term,
) -> io::Result<()> {
    flush_answer_boundary(state, terminal)?;
    commit_pending_interjections(state, terminal)?;
    state.reset_working();
    state.clear_deferred_submissions();
    dispatch_user_message(ctx, STOP_COMMAND.to_string()).await;
    Ok(())
}

/// Record the resolved approval. The decision sits between its tool's `●` and
/// `⎿`, but that block is deferred until `ToolCompleted`, so buffer the line
/// onto the in-flight tool (matched by `call_id`) and let it commit with the
/// block. If the tool isn't tracked (shouldn't happen), commit it standalone so
/// the decision isn't lost.
fn resolve_approval(
    state: &mut AppState,
    terminal: &mut Term,
    decision: baybo_tools::ApprovalDecision,
) -> io::Result<()> {
    let Some(outcome) = state.resolve_active_approval(decision) else {
        return Ok(());
    };
    let lines = chat::render_approval_resolved_lines(&outcome.resolved);
    if let Err(lines) = state.buffer_approval_line(&outcome.resolved.call_id, lines) {
        flush_answer_boundary(state, terminal)?;
        commit_tool_lines(state, terminal, lines)?;
    }
    Ok(())
}

async fn handle_slash(
    state: &mut AppState,
    ctx: &LoopCtx,
    terminal: &mut Term,
    current_viewport_h: &mut u16,
    text: String,
) -> io::Result<()> {
    let Some(handler) = ctx.slash_handler.as_ref() else {
        let msg = format!("(no slash handler; ignored: {text})");
        commit_below_answer(state, terminal, chat::render_system_lines(&msg))?;
        return Ok(());
    };
    match handler.handle(&text).await {
        SlashOutcome::Handled(blocks) => {
            let width = scrollback_width(terminal);
            commit_below_answer(
                state,
                terminal,
                chat::render_assistant_lines(&blocks, width),
            )?;
        }
        SlashOutcome::OpenView(kind) => match ctx.dashboard_provider.as_ref() {
            Some(provider) => {
                spawn_dashboard_fetch(kind, Arc::clone(provider), ctx.event_tx.clone());
            }
            None => {
                let msg = format!("(no dashboard provider; cannot open view: {kind:?})");
                commit_below_answer(state, terminal, chat::render_system_lines(&msg))?;
            }
        },
        SlashOutcome::PassThrough => {
            commit_user_message(state, terminal, &text)?;
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
                    state.session_id = new_id.clone();
                    reset_chat_scrollback(state, terminal, current_viewport_h, &new_id)?;
                    let msg = format!("Started a fresh session: {new_id}");
                    commit_below_answer(state, terminal, chat::render_system_lines(&msg))?;
                }
                Err(e) => {
                    let msg = format!("/new failed: {e}");
                    commit_below_answer(state, terminal, chat::render_system_lines(&msg))?;
                }
            }
        }
    }
    Ok(())
}

/// Escape sequence that wipes the visible screen + scrollback and homes the
/// cursor: `H` = cursor home, `2J` = erase visible screen, `3J` = erase
/// scrollback. Written on entry, on `/clear` / Ctrl-L, and on resize-refresh.
const CLEAR_SCREEN_AND_SCROLLBACK: &str = "\x1b[H\x1b[2J\x1b[3J";

/// Write the screen + scrollback wipe escape (which homes the cursor) and
/// flush. Used on entry and on resize-refresh — both then construct a *fresh*
/// inline terminal, whose viewport anchors at the current cursor row, so the
/// homed cursor lands the banner at the very top. It deliberately does **not**
/// call [`ratatui::Terminal::clear`]: for an inline viewport that re-homes the
/// cursor to the *old* viewport's row (near the screen bottom), which would
/// anchor the new viewport — and the banner — mid-screen with a blank gap above.
fn home_and_clear_screen() -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "{CLEAR_SCREEN_AND_SCROLLBACK}")?;
    stdout.flush()?;
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
    snapshot: baybo_channels::DashboardSnapshot,
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
        bot_id: String::new(),
    };
    if let Err(e) = ctx.input.submit(msg).await {
        warn!("failed to forward TUI input: {e}");
    }
}

fn mint_new_session_id() -> SessionId {
    SessionId::new()
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

    const TEST_WIDTH: u16 = 40;

    #[test]
    fn finalize_renders_body_when_nothing_streamed() {
        // Direct/non-streaming delivery (cron or a synthetic completion
        // reply): the Message arrives with no preceding deltas, so its text
        // must render from the blocks rather than be dropped.
        let blocks = vec![ContentBlock::Text("cron result".into())];
        let lines = finalize_extras(&blocks, false, TEST_WIDTH);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("cron result"),
            "body must render: {joined:?}"
        );
        assert!(
            lines
                .first()
                .is_some_and(|l| line_text(l).starts_with("● ")),
            "first line carries the answer-dot leader: {joined:?}"
        );
    }

    #[test]
    fn finalize_renders_a_non_streamed_body_as_markdown() {
        let blocks = vec![ContentBlock::Text("a **bold** claim".into())];
        let joined = finalize_extras(&blocks, false, TEST_WIDTH)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("a bold claim") && !joined.contains("**"),
            "markup must be styled away, not printed: {joined:?}"
        );
    }

    #[test]
    fn finalize_skips_text_when_body_streamed() {
        // A streamed response already committed its text block by block, so
        // the final blocks' Text must not be re-rendered (no duplicate).
        let blocks = vec![ContentBlock::Text("already streamed".into())];
        let lines = finalize_extras(&blocks, true, TEST_WIDTH);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains("already streamed"),
            "streamed text must not be re-rendered: {joined:?}"
        );
    }

    #[test]
    fn finalize_empty_message_is_noop() {
        // Nothing streamed and no renderable blocks → no body lines. The
        // `cooked for` footer is committed separately by `finalize_stream`.
        assert!(finalize_extras(&[], false, TEST_WIDTH).is_empty());
        let body = finalize_extras(&[ContentBlock::Text("x".into())], true, TEST_WIDTH);
        let joined = body.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !joined.contains("cooked for"),
            "footer is not in the body: {joined:?}"
        );
    }

    #[test]
    fn block_source_keeps_text_only_when_it_did_not_stream() {
        let blocks = vec![ContentBlock::Text("prose".into())];
        assert_eq!(block_source(&blocks, false), vec!["prose".to_string()]);
        assert!(block_source(&blocks, true).is_empty());
    }

    /// What `finalize_extras` commits and what `block_source` records must render
    /// the same rows, or a resize rewrites the end of a non-streamed response.
    #[test]
    fn finalize_extras_and_block_source_render_the_same_rows() {
        let cases: Vec<Vec<ContentBlock>> = vec![
            vec![ContentBlock::Text("a **bold** claim".into())],
            vec![ContentBlock::Text(
                "# Heading

body text"
                    .into(),
            )],
            vec![
                ContentBlock::Text("prose".into()),
                ContentBlock::Image {
                    blob: baybo_model::BlobRef {
                        blob_id: "b-1".into(),
                    },
                    mime_type: "image/png".into(),
                    filename: None,
                    width: None,
                    height: None,
                },
            ],
            vec![ContentBlock::Text(
                "列表：

- 第一项
- 第二项"
                    .into(),
            )],
        ];
        for blocks in cases {
            let live = finalize_extras(&blocks, false, TEST_WIDTH);
            let source = block_source(&blocks, false);
            let replayed = chat::render_answer_rows(
                markdown::render_document(
                    &source.join(
                        "
",
                    ),
                    chat::answer_content_width(TEST_WIDTH),
                ),
                false,
            );
            assert_eq!(
                live.iter().map(line_text).collect::<Vec<_>>(),
                replayed.iter().map(line_text).collect::<Vec<_>>(),
                "live and replayed extras diverged for {blocks:?}"
            );
        }
    }

    #[test]
    fn is_stop_command_matches_stop_variants_only() {
        assert!(is_stop_command("/stop"));
        assert!(is_stop_command("  /Stop  "));
        assert!(is_stop_command("/STOP everything"));
        assert!(!is_stop_command("/stopwatch"));
        assert!(!is_stop_command("/compact"));
        assert!(!is_stop_command("stop"));
        assert!(!is_stop_command("please /stop"));
    }
}
