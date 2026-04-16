//! Key-to-action mapping.
//!
//! Pure translation from a `KeyEvent` to a logical `Action` that the event
//! loop applies. Kept free of I/O so bindings can be tested without a
//! terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::app::ViewMode;

/// Logical action produced by interpreting a key press in the current mode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Character to insert at the cursor.
    Insert(char),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    Submit,
    HistoryPrev,
    HistoryNext,
    ScrollPageUp,
    ScrollPageDown,
    ClearScrollback,
    /// Clear input if non-empty; otherwise request shutdown.
    Cancel,
    Shutdown,
    /// First-press confirmation gate for Ctrl-D on empty input. The event
    /// loop shows a hint and only exits on a second consecutive press.
    ConfirmExit,
    DashboardExit,
    DashboardSelectPrev,
    DashboardSelectNext,
    DashboardPageUp,
    DashboardPageDown,
    DashboardRefresh,
    /// Move selection down in the open slash-completion popup.
    CompletionNext,
    /// Move selection up in the open slash-completion popup.
    CompletionPrev,
    /// Accept the highlighted slash-completion candidate.
    CompletionAccept,
    /// Toggle terminal mouse capture on/off. Turning it off restores
    /// native drag-to-select across all terminals.
    ToggleMouseCapture,
    /// Approve the head pending approval for this call only.
    ApprovalApprove,
    /// Approve the head pending approval and remember the resource for
    /// the rest of the session.
    ApprovalApproveAlways,
    /// Deny the head pending approval.
    ApprovalDeny,
    /// Move the selection up in the inline approval prompt.
    ApprovalSelectPrev,
    /// Move the selection down in the inline approval prompt.
    ApprovalSelectNext,
    /// Confirm the currently highlighted approval option.
    ApprovalConfirm,
    /// Key had no binding in the current mode.
    Nothing,
}

/// Flags that steer key translation. `input_empty` drives the Ctrl-C /
/// Ctrl-D semantics; `completion_open` reroutes Up/Down/Tab onto the
/// completion popup while it is visible.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KeyContext {
    pub input_empty: bool,
    pub completion_open: bool,
    /// An approval modal is overlaying the current view. When set, the
    /// modal eats input before any other bindings fire.
    pub approval_open: bool,
}

pub(crate) fn translate(mode: &ViewMode, key: KeyEvent, ctx: KeyContext) -> Action {
    let input_empty = ctx.input_empty;
    // Crossterm may report Release/Repeat; only act on Press to avoid double-fires.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Action::Nothing;
    }

    // Approval modal takes precedence over every other binding when open.
    // Single-char answers only; unrelated keys are swallowed so the user
    // cannot accidentally scroll / exit / type into a driver that owns
    // input right now.
    if ctx.approval_open {
        return translate_approval(key);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Global bindings first.
    if alt && matches!(key.code, KeyCode::Char('m')) {
        return Action::ToggleMouseCapture;
    }
    match (key.code, ctrl) {
        (KeyCode::Char('c'), true) => {
            return if input_empty {
                Action::Shutdown
            } else {
                Action::Cancel
            };
        }
        (KeyCode::Char('d'), true) if input_empty => return Action::ConfirmExit,
        (KeyCode::Char('l'), true) => return Action::ClearScrollback,
        _ => {}
    }

    match mode {
        ViewMode::Chat => translate_chat(key, ctx.completion_open),
        ViewMode::Dashboard { .. } => translate_dashboard(key),
    }
}

fn translate_chat(key: KeyEvent, completion_open: bool) -> Action {
    match key.code {
        // Shift-Enter / Alt-Enter insert a literal newline so the input can
        // span multiple lines. Shift-Enter requires the Kitty keyboard
        // protocol (pushed in `run_loop`); Alt-Enter works on terminals
        // without it. Both take precedence over completion-accept.
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            Action::Insert('\n')
        }
        KeyCode::Enter if completion_open => Action::CompletionAccept,
        KeyCode::Enter => Action::Submit,
        KeyCode::Tab if completion_open => Action::CompletionAccept,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::MoveLeft,
        KeyCode::Right => Action::MoveRight,
        KeyCode::Home => Action::MoveHome,
        KeyCode::End => Action::MoveEnd,
        KeyCode::Up if completion_open => Action::CompletionPrev,
        KeyCode::Down if completion_open => Action::CompletionNext,
        KeyCode::Up => Action::HistoryPrev,
        KeyCode::Down => Action::HistoryNext,
        KeyCode::PageUp => Action::ScrollPageUp,
        KeyCode::PageDown => Action::ScrollPageDown,
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Action::Insert(c)
        }
        _ => Action::Nothing,
    }
}

fn translate_dashboard(key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => Action::DashboardExit,
        KeyCode::Up => Action::DashboardSelectPrev,
        KeyCode::Down => Action::DashboardSelectNext,
        KeyCode::PageUp => Action::DashboardPageUp,
        KeyCode::PageDown => Action::DashboardPageDown,
        KeyCode::Char('r') => Action::DashboardRefresh,
        KeyCode::Char('q') => Action::DashboardExit,
        _ => Action::Nothing,
    }
}

fn translate_approval(key: KeyEvent) -> Action {
    // Uppercase `A` is produced when the user holds Shift; crossterm does
    // not add SHIFT to the modifiers for printable chars, so matching on
    // the character is sufficient.
    match key.code {
        KeyCode::Char('a') => Action::ApprovalApprove,
        KeyCode::Char('A') => Action::ApprovalApproveAlways,
        KeyCode::Char('d') | KeyCode::Esc => Action::ApprovalDeny,
        KeyCode::Up | KeyCode::Char('k') => Action::ApprovalSelectPrev,
        KeyCode::Down | KeyCode::Char('j') => Action::ApprovalSelectNext,
        KeyCode::Enter => Action::ApprovalConfirm,
        _ => Action::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DashboardSnapshot;
    use crate::tui::app::AppState;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ctx(input_empty: bool) -> KeyContext {
        KeyContext {
            input_empty,
            completion_open: false,
            approval_open: false,
        }
    }

    #[test]
    fn enter_submits_in_chat() {
        let app = AppState::new();
        assert_eq!(
            translate(&app.mode, press(KeyCode::Enter), ctx(false)),
            Action::Submit
        );
    }

    #[test]
    fn ctrl_c_cancels_with_input_and_quits_empty() {
        let app = AppState::new();
        assert_eq!(
            translate(&app.mode, ctrl(KeyCode::Char('c')), ctx(false)),
            Action::Cancel
        );
        assert_eq!(
            translate(&app.mode, ctrl(KeyCode::Char('c')), ctx(true)),
            Action::Shutdown
        );
    }

    #[test]
    fn ctrl_d_requests_exit_confirm_only_on_empty_input() {
        let app = AppState::new();
        assert_eq!(
            translate(&app.mode, ctrl(KeyCode::Char('d')), ctx(false)),
            Action::Nothing
        );
        assert_eq!(
            translate(&app.mode, ctrl(KeyCode::Char('d')), ctx(true)),
            Action::ConfirmExit
        );
    }

    #[test]
    fn esc_exits_dashboard_only() {
        let mut app = AppState::new();
        // In chat mode, Esc is a no-op.
        assert_eq!(
            translate(&app.mode, press(KeyCode::Esc), ctx(false)),
            Action::Nothing
        );
        app.enter_dashboard(
            crate::ViewKind::Skills,
            DashboardSnapshot {
                title: "s".into(),
                columns: vec![],
                rows: vec![],
                footer: None,
            },
        );
        assert_eq!(
            translate(&app.mode, press(KeyCode::Esc), ctx(false)),
            Action::DashboardExit
        );
    }

    #[test]
    fn shift_or_alt_enter_inserts_newline() {
        let app = AppState::new();
        assert_eq!(
            translate(
                &app.mode,
                with_mods(KeyCode::Enter, KeyModifiers::SHIFT),
                ctx(false)
            ),
            Action::Insert('\n')
        );
        assert_eq!(
            translate(
                &app.mode,
                with_mods(KeyCode::Enter, KeyModifiers::ALT),
                ctx(false)
            ),
            Action::Insert('\n')
        );
    }

    #[test]
    fn shift_enter_overrides_completion_accept() {
        let app = AppState::new();
        let open = KeyContext {
            input_empty: false,
            completion_open: true,
            approval_open: false,
        };
        assert_eq!(
            translate(
                &app.mode,
                with_mods(KeyCode::Enter, KeyModifiers::SHIFT),
                open
            ),
            Action::Insert('\n')
        );
    }

    #[test]
    fn printable_char_inserts_in_chat() {
        let app = AppState::new();
        assert_eq!(
            translate(&app.mode, press(KeyCode::Char('a')), ctx(false)),
            Action::Insert('a')
        );
    }

    #[test]
    fn up_arrow_scrolls_rows_in_dashboard_not_history() {
        let mut app = AppState::new();
        app.enter_dashboard(
            crate::ViewKind::Skills,
            DashboardSnapshot {
                title: "s".into(),
                columns: vec![],
                rows: vec![vec!["r".into()]],
                footer: None,
            },
        );
        assert_eq!(
            translate(&app.mode, press(KeyCode::Up), ctx(false)),
            Action::DashboardSelectPrev
        );
    }
}
