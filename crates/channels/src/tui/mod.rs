//! Ratatui-based channel adapter.
//!
//! Serves as the default interactive channel when `aura` is run without a
//! subcommand. Layout is minimal by design: a scrollback pane plus an input
//! line. Slash commands that map to [`ViewKind`] open a dedicated dashboard
//! view; other slash commands render as text into the scrollback.
//!
//! I/O architecture:
//! - stdin is driven by [`crossterm::event::EventStream`]; do **not** read
//!   stdin via tokio here, as raw mode would conflict with line-buffered
//!   readers.
//! - On entry the adapter flips the terminal into raw mode and the
//!   alternate screen; a panic hook restores both on unwind.
//! - `send_response` only appends to the app state and wakes the event
//!   loop; rendering happens on the loop task.

mod app;
mod chat;
mod dashboard;
pub(crate) mod event;
mod keymap;

use std::io;
use std::panic;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use aura_model::{ContentBlock, MessageMetadata};
use aura_session::{ChannelType, User};
use chrono::Utc;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, EventStream, KeyEvent,
    KeyEventKind, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{error, warn};
use uuid::Uuid;

use std::time::Instant;

use crate::tui::app::{AppState, CONFIRM_EXIT_WINDOW, ViewMode};
use crate::tui::event::{AppEvent, TuiLogSink};
use crate::tui::keymap::{Action, KeyContext, translate};
use crate::{
    ChannelAdapter, ChannelError, DashboardProvider, IncomingMessage, Message, OutgoingMessage,
    Result, SlashHandler, SlashOutcome, ViewKind,
};

/// Callback invoked exactly once when the TUI event loop exits, regardless
/// of cause (user-initiated quit, terminal disconnect, internal shutdown
/// notification). The typical wiring triggers the process-wide shutdown
/// signal so the rest of the runtime can tear down — without this the
/// router task blocks forever on `response_rx.recv()` since the supervisor
/// and cron scheduler keep their senders alive.
pub type OnExit = Arc<dyn Fn() + Send + Sync>;

/// Ratatui-based terminal channel adapter.
pub struct TuiAdapter {
    session_id: String,
    user: User,
    shutdown: Arc<Notify>,
    slash_handler: Option<Arc<dyn SlashHandler>>,
    dashboard_provider: Option<Arc<dyn DashboardProvider>>,
    on_exit: Option<OnExit>,
    /// Event channel: the sender is cloned to `send_response`, the tracing
    /// bridge, and the event loop; the receiver is taken out once at
    /// `start()`. The channel is created eagerly in `new()` so boot-time log
    /// events that happen before the loop is spawned still queue up and land
    /// in the scrollback as soon as rendering begins.
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: Arc<Mutex<Option<mpsc::Receiver<AppEvent>>>>,
}

impl Default for TuiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiAdapter {
    pub fn new() -> Self {
        let user_id = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "tui-user".to_string());
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(256);
        Self {
            session_id: format!("tui-{}", Uuid::new_v4()),
            user: User {
                id: user_id,
                name: None,
                channel: ChannelType::Tui,
            },
            shutdown: Arc::new(Notify::new()),
            slash_handler: None,
            dashboard_provider: None,
            on_exit: None,
            event_tx,
            event_rx: Arc::new(Mutex::new(Some(event_rx))),
        }
    }

    /// Attach an exit callback. Invoked exactly once when the event loop
    /// exits so the caller can propagate the request (e.g. trigger a
    /// process-wide `ShutdownSignal`).
    pub fn with_on_exit(mut self, on_exit: OnExit) -> Self {
        self.on_exit = Some(on_exit);
        self
    }

    /// Attach a slash-command handler.
    pub fn with_slash_handler(mut self, handler: Arc<dyn SlashHandler>) -> Self {
        self.slash_handler = Some(handler);
        self
    }

    /// Attach a dashboard data provider. Without one, `OpenView` outcomes are
    /// silently ignored.
    pub fn with_dashboard_provider(mut self, provider: Arc<dyn DashboardProvider>) -> Self {
        self.dashboard_provider = Some(provider);
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Hand out a non-blocking sink for forwarding warn/error tracing events
    /// into the scrollback. Callers typically wire this into a
    /// `tracing_subscriber::Layer` that calls `emit` from `on_event`.
    pub fn log_sink(&self) -> TuiLogSink {
        TuiLogSink::new(self.event_tx.clone())
    }
}

#[async_trait]
impl ChannelAdapter for TuiAdapter {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Tui
    }

    async fn start(&self, incoming: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let event_rx = self
            .event_rx
            .lock()
            .await
            .take()
            .ok_or_else(|| ChannelError::Send("TuiAdapter::start called twice".into()))?;

        let ctx = LoopCtx {
            session_id: self.session_id.clone(),
            user: self.user.clone(),
            shutdown: Arc::clone(&self.shutdown),
            incoming,
            event_tx: self.event_tx.clone(),
            event_rx,
            slash_handler: self.slash_handler.clone(),
            dashboard_provider: self.dashboard_provider.clone(),
            on_exit: self.on_exit.clone(),
        };

        // Spawn the event loop. `start()` returns once spawned; the loop runs
        // until shutdown or user-initiated quit. The shutdown Notify is the
        // control signal, so the JoinHandle is intentionally dropped.
        tokio::spawn(async move {
            if let Err(e) = run_loop(ctx).await {
                error!("TUI event loop exited with error: {e}");
            }
        });

        Ok(())
    }

    async fn send_response(&self, response: OutgoingMessage) -> Result<()> {
        self.event_tx
            .send(AppEvent::Outgoing(response.content))
            .await
            .map_err(|_| ChannelError::Send("TUI event loop has exited".into()))?;
        Ok(())
    }

    async fn send_stream_delta(&self, _session_id: &str, delta: &str) -> Result<()> {
        self.event_tx
            .send(AppEvent::StreamDelta(delta.to_string()))
            .await
            .map_err(|_| ChannelError::Send("TUI event loop has exited".into()))?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.notify_one();
        Ok(())
    }
}

struct LoopCtx {
    session_id: String,
    user: User,
    shutdown: Arc<Notify>,
    incoming: mpsc::Sender<IncomingMessage>,
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: mpsc::Receiver<AppEvent>,
    slash_handler: Option<Arc<dyn SlashHandler>>,
    dashboard_provider: Option<Arc<dyn DashboardProvider>>,
    on_exit: Option<OnExit>,
}

/// RAII guard that restores the terminal and fires the on_exit callback
/// when the TUI task unwinds — whether via `break`, `?` propagation, or
/// panic. Without this, an early error in `handle_key` or `terminal.draw`
/// would leak raw mode AND skip the shutdown callback, leaving the process
/// running with the terminal unusable.
struct TuiTeardownGuard {
    stdout: io::Stdout,
    on_exit: Option<OnExit>,
}

impl Drop for TuiTeardownGuard {
    fn drop(&mut self) {
        if let Err(e) = disable_raw_mode() {
            warn!("disable_raw_mode failed: {e}");
        }
        if let Err(e) = execute!(self.stdout, LeaveAlternateScreen, DisableMouseCapture) {
            warn!("leave alternate screen failed: {e}");
        }
        if let Some(cb) = self.on_exit.take() {
            cb();
        }
    }
}

async fn run_loop(mut ctx: LoopCtx) -> anyhow::Result<()> {
    install_panic_hook();

    let mut stdout = io::stdout();
    enable_raw_mode().map_err(|e| anyhow::anyhow!("enable_raw_mode: {e}"))?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|e| anyhow::anyhow!("EnterAlternateScreen: {e}"))?;
    // From here on, any early return must restore the terminal and fire
    // on_exit — the guard's Drop impl handles both.
    let mut _guard = TuiTeardownGuard {
        stdout: io::stdout(),
        on_exit: ctx.on_exit.take(),
    };
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| anyhow::anyhow!("Terminal::new: {e}"))?;

    let mut state = AppState::new();
    if let Some(handler) = ctx.slash_handler.as_ref() {
        state.set_commands(handler.commands());
    }
    let mut term_events = EventStream::new();
    let mut mouse_captured = true;

    // Initial greet so the user sees something before their first message.
    state.push_system(format!(
        "Session {} — type / to see commands, /quit or Ctrl-D twice to leave. Alt-M toggles mouse wheel (off enables text selection).",
        ctx.session_id
    ));
    terminal.draw(|f| render(f, &mut state))?;

    loop {
        tokio::select! {
            biased;
            _ = ctx.shutdown.notified() => break Ok(()),
            term = term_events.next() => {
                match term {
                    Some(Ok(CrosstermEvent::Key(key))) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match handle_key(&mut state, &mut ctx, key).await? {
                                KeyOutcome::Exit => break Ok(()),
                                KeyOutcome::ToggleMouseCapture => {
                                    mouse_captured = !mouse_captured;
                                    let result = if mouse_captured {
                                        execute!(terminal.backend_mut(), EnableMouseCapture)
                                    } else {
                                        execute!(terminal.backend_mut(), DisableMouseCapture)
                                    };
                                    if let Err(e) = result {
                                        warn!("toggle mouse capture failed: {e}");
                                    }
                                    state.push_system(if mouse_captured {
                                        "mouse wheel scrolling ON".into()
                                    } else {
                                        "mouse wheel scrolling OFF — drag to select text".into()
                                    });
                                }
                                KeyOutcome::Continue => {}
                            }
                        }
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    Some(Ok(CrosstermEvent::Mouse(mouse))) => {
                        if handle_mouse(&mut state, mouse) {
                            terminal.draw(|f| render(f, &mut state))?;
                        }
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
                        state.finish_stream(blocks);
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    AppEvent::StreamDelta(delta) => {
                        state.append_stream_delta(&delta);
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    AppEvent::DashboardReady(kind, snapshot) => {
                        if state.dashboard_kind() == Some(kind) {
                            state.refresh_dashboard(snapshot);
                        } else {
                            state.enter_dashboard(kind, snapshot);
                        }
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    AppEvent::Log(record) => {
                        state.push_log(record);
                        terminal.draw(|f| render(f, &mut state))?;
                    }
                    AppEvent::Shutdown => break Ok(()),
                }
            }
        }
    }
    // Teardown + on_exit are driven by `_guard`'s Drop impl, so they run
    // even if a `?` earlier in this function returns early.
}

/// Handle a mouse event. Currently only wheel scrolling is wired: it
/// scrolls the chat scrollback by 3 rows per tick. Dashboard mode is
/// unaffected — table navigation stays on the arrow keys. Returns
/// `true` if the event mutated state and warrants a redraw.
fn handle_mouse(state: &mut AppState, mouse: MouseEvent) -> bool {
    if !matches!(state.mode, ViewMode::Chat) {
        return false;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            state.scroll_up(3);
            true
        }
        MouseEventKind::ScrollDown => {
            state.scroll_down(3);
            true
        }
        _ => false,
    }
}

/// Outcome of handling a single key press, signalling whether the loop
/// should terminate or take a side-effect that requires `terminal.backend_mut()`
/// (currently just toggling mouse capture).
enum KeyOutcome {
    Continue,
    Exit,
    ToggleMouseCapture,
}

async fn handle_key(
    state: &mut AppState,
    ctx: &mut LoopCtx,
    key: KeyEvent,
) -> anyhow::Result<KeyOutcome> {
    let key_ctx = KeyContext {
        input_empty: state.input.is_empty(),
        completion_open: !state.completion_candidates().is_empty(),
    };
    let action = translate(&state.mode, key, key_ctx);
    // Any action other than ConfirmExit / Nothing cancels a pending
    // Ctrl-D confirmation so the gate doesn't linger across unrelated
    // keypresses (bash/zsh do the same with `set -o ignoreeof`).
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
        Action::HistoryPrev => state.history_prev(),
        Action::HistoryNext => state.history_next(),
        Action::ScrollPageUp => state.scroll_up(10),
        Action::ScrollPageDown => state.scroll_down(10),
        Action::ClearScrollback => state.clear_scrollback(),
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
                    state.push_system(format!(
                        "Press Ctrl-D again within {}s to exit.",
                        CONFIRM_EXIT_WINDOW.as_secs()
                    ));
                }
            }
        }
        Action::ToggleMouseCapture => return Ok(KeyOutcome::ToggleMouseCapture),
        Action::DashboardExit => state.exit_dashboard(),
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
                if text == "/quit" || text == "/exit" {
                    return Ok(KeyOutcome::Exit);
                }
                if text == "/clear" {
                    state.clear_scrollback();
                } else if text.starts_with('/') {
                    handle_slash(state, ctx, text).await;
                } else {
                    state.push_user(text.clone());
                    dispatch_user_message(ctx, text).await;
                }
            }
        }
        Action::Nothing => {}
    }
    Ok(KeyOutcome::Continue)
}

async fn handle_slash(state: &mut AppState, ctx: &LoopCtx, text: String) {
    let Some(handler) = ctx.slash_handler.as_ref() else {
        state.push_system(format!("(no slash handler; ignored: {text})"));
        return;
    };
    match handler.handle(&text).await {
        SlashOutcome::Handled(blocks) => state.push_assistant(blocks),
        SlashOutcome::OpenView(kind) => match ctx.dashboard_provider.as_ref() {
            Some(provider) => {
                spawn_dashboard_fetch(kind, Arc::clone(provider), ctx.event_tx.clone());
            }
            None => state.push_system(format!(
                "(no dashboard provider; cannot open view: {kind:?})"
            )),
        },
        SlashOutcome::PassThrough => {
            state.push_user(text.clone());
            dispatch_user_message(ctx, text).await;
        }
        SlashOutcome::Exit => {
            // Ask the loop to stop.
            let _ = ctx.event_tx.try_send(AppEvent::Shutdown);
        }
    }
}

async fn dispatch_user_message(ctx: &LoopCtx, text: String) {
    let msg = IncomingMessage {
        message: Message {
            id: Uuid::new_v4().to_string(),
            session_id: ctx.session_id.clone(),
            channel: ChannelType::Tui,
            sender: ctx.user.clone(),
            content: vec![ContentBlock::Text(text)],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
    };
    if let Err(e) = ctx.incoming.send(msg).await {
        warn!("failed to forward TUI input to router: {e}");
    }
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

fn render(frame: &mut ratatui::Frame, state: &mut AppState) {
    let area = frame.area();
    match &state.mode {
        ViewMode::Chat => chat::render(frame, area, state),
        ViewMode::Dashboard { .. } => dashboard::render(frame, area, state),
    }
}

/// Register a panic hook that restores the terminal before the default
/// hook runs. Idempotent across multiple `start()` calls.
fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            prev(info);
        }));
    });
}
