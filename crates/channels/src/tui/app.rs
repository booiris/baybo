//! TUI application state: scrollback, input buffer, input history, view mode.
//!
//! State mutation is intentionally synchronous and side-effect-free — all I/O
//! happens in [`super::TuiAdapter`]'s event loop. This keeps logic unit-testable
//! against synthetic key events.

use std::collections::VecDeque;
use std::time::Instant;

use aura_model::ContentBlock;
use ratatui::widgets::TableState;

use crate::SlashCommand;
use crate::tui::event::LogRecord;
use crate::{DashboardSnapshot, ViewKind};

const SCROLLBACK_CAP: usize = 5000;
const HISTORY_CAP: usize = 500;

/// One rendered line in the chat scrollback.
#[derive(Debug, Clone)]
pub(crate) enum ChatLine {
    User(String),
    Assistant(Vec<ContentBlock>),
    System(String),
    Log(LogRecord),
}

/// Top-level view the TUI is displaying.
pub(crate) enum ViewMode {
    Chat,
    Dashboard {
        kind: ViewKind,
        snapshot: DashboardSnapshot,
        table_state: TableState,
    },
}

pub(crate) struct AppState {
    pub(crate) mode: ViewMode,
    /// Persistent one-line banner pinned above the scrollback in chat mode.
    /// Empty string means no banner is rendered.
    pub(crate) banner: String,
    pub(crate) scrollback: VecDeque<ChatLine>,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) history: VecDeque<String>,
    pub(crate) history_cursor: Option<usize>,
    /// Offset from the bottom of the scrollback, in rendered lines.
    /// `0` means "tail"; larger values scroll up.
    pub(crate) scroll_offset: u16,
    /// In-progress streaming response. `None` when no stream is active;
    /// `Some(buffer)` once `AppEvent::StreamDelta` has arrived. The final
    /// `AppEvent::Outgoing` replaces this with a persistent `ChatLine`.
    pub(crate) streaming: Option<String>,
    /// Full list of slash commands for completion; sourced once at startup
    /// from `SlashHandler::commands()`.
    pub(crate) commands: Vec<SlashCommand>,
    /// Selection cursor within the currently filtered completion list.
    pub(crate) completion_cursor: usize,
    /// Ctrl-D exit confirmation gate. Records the timestamp of the first
    /// Ctrl-D press with an empty input; a second Ctrl-D within the window
    /// (see [`CONFIRM_EXIT_WINDOW`]) commits the exit. Cleared by any other
    /// key so the confirmation doesn't linger across unrelated input.
    pub(crate) confirm_exit_at: Option<Instant>,
}

/// How long the "press Ctrl-D again to exit" prompt stays armed. Matches
/// the typical double-press window in shells with `ignoreeof`.
pub(crate) const CONFIRM_EXIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl AppState {
    pub(crate) fn new() -> Self {
        Self {
            mode: ViewMode::Chat,
            banner: String::new(),
            scrollback: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_cursor: None,
            scroll_offset: 0,
            streaming: None,
            commands: Vec::new(),
            completion_cursor: 0,
            confirm_exit_at: None,
        }
    }

    pub(crate) fn set_commands(&mut self, commands: Vec<SlashCommand>) {
        self.commands = commands;
    }

    pub(crate) fn set_banner(&mut self, text: String) {
        self.banner = text;
    }

    /// Return the filtered completion list for the current input. Only
    /// active when the buffer starts with `/` and the cursor sits on the
    /// command token (i.e. no whitespace between `/` and cursor). Returns
    /// an empty slice whenever completion should not be shown.
    pub(crate) fn completion_candidates(&self) -> Vec<&SlashCommand> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let prefix_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        if self.cursor > prefix_end {
            return Vec::new();
        }
        let prefix = &self.input[..prefix_end];
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .collect()
    }

    pub(crate) fn completion_select_prev(&mut self) {
        let n = self.completion_candidates().len();
        if n == 0 {
            self.completion_cursor = 0;
            return;
        }
        self.completion_cursor = if self.completion_cursor == 0 {
            n - 1
        } else {
            self.completion_cursor - 1
        };
    }

    pub(crate) fn completion_select_next(&mut self) {
        let n = self.completion_candidates().len();
        if n == 0 {
            self.completion_cursor = 0;
            return;
        }
        self.completion_cursor = (self.completion_cursor + 1) % n;
    }

    /// Accept the current completion candidate. Replaces the prefix up to
    /// the first whitespace with the candidate name + trailing space, so
    /// arguments can follow naturally.
    pub(crate) fn completion_accept(&mut self) -> bool {
        let candidates = self.completion_candidates();
        if candidates.is_empty() {
            return false;
        }
        let idx = self.completion_cursor.min(candidates.len() - 1);
        let name = candidates[idx].name.clone();
        let prefix_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        let suffix = self.input[prefix_end..].to_string();
        self.input = format!("{name} {}", suffix.trim_start());
        self.cursor = name.len() + 1;
        self.completion_cursor = 0;
        true
    }

    pub(crate) fn push_user(&mut self, text: String) {
        self.push(ChatLine::User(text));
    }

    pub(crate) fn push_assistant(&mut self, blocks: Vec<ContentBlock>) {
        self.push(ChatLine::Assistant(blocks));
    }

    /// Append a chunk of text to the currently streaming assistant response.
    /// Creates a fresh streaming buffer on the first chunk.
    pub(crate) fn append_stream_delta(&mut self, delta: &str) {
        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
        self.scroll_offset = 0;
    }

    /// Finalise the streaming response. `blocks` comes from the router's
    /// full `OutgoingMessage`; it supersedes whatever we streamed so the
    /// persisted chat line exactly reflects the canonical content
    /// (including tool calls, images, etc. that deltas don't carry).
    pub(crate) fn finish_stream(&mut self, blocks: Vec<ContentBlock>) {
        self.streaming = None;
        self.push(ChatLine::Assistant(blocks));
    }

    pub(crate) fn push_system(&mut self, text: String) {
        self.push(ChatLine::System(text));
    }

    pub(crate) fn push_log(&mut self, record: LogRecord) {
        self.push(ChatLine::Log(record));
    }

    fn push(&mut self, line: ChatLine) {
        if self.scrollback.len() >= SCROLLBACK_CAP {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(line);
        self.scroll_offset = 0;
    }

    pub(crate) fn take_input(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_cursor = None;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            self.remember(trimmed.to_string());
            Some(trimmed.to_string())
        }
    }

    fn remember(&mut self, line: String) {
        if self.history.back().is_some_and(|last| last == &line) {
            return;
        }
        if self.history.len() >= HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(line);
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.input, self.cursor);
        self.input.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub(crate) fn delete_char(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = next_char_boundary(&self.input, self.cursor);
        self.input.drain(self.cursor..next);
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.input, self.cursor);
    }

    pub(crate) fn move_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.input, self.cursor);
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// True when no `\n` precedes the cursor — i.e. the insertion point sits
    /// on the first logical line of the input buffer.
    pub(crate) fn cursor_at_first_line(&self) -> bool {
        !self.input[..self.cursor].contains('\n')
    }

    /// True when no `\n` follows the cursor — the cursor is on the last
    /// logical line of the buffer.
    pub(crate) fn cursor_at_last_line(&self) -> bool {
        !self.input[self.cursor..].contains('\n')
    }

    /// Move the cursor up one logical line, preserving byte-column where
    /// possible and clamping to the previous line's length. No-op when
    /// already on the first line.
    pub(crate) fn move_up_line(&mut self) {
        if self.cursor_at_first_line() {
            return;
        }
        let prefix = &self.input[..self.cursor];
        let cur_line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = self.cursor - cur_line_start;
        let prev_newline = cur_line_start - 1;
        let prev_line_start = self.input[..prev_newline]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line_len = prev_newline - prev_line_start;
        let target = prev_line_start + col.min(prev_line_len);
        self.cursor = clamp_to_boundary(&self.input, target);
    }

    /// Move the cursor down one logical line, preserving byte-column where
    /// possible and clamping to the next line's length. No-op when already
    /// on the last line.
    pub(crate) fn move_down_line(&mut self) {
        if self.cursor_at_last_line() {
            return;
        }
        let prefix = &self.input[..self.cursor];
        let cur_line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = self.cursor - cur_line_start;
        // Cursor is not on last line, so there is a '\n' at or after cursor.
        let next_newline = cur_line_start + self.input[cur_line_start..].find('\n').unwrap();
        let next_line_start = next_newline + 1;
        let next_line_end = self.input[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.input.len());
        let next_line_len = next_line_end - next_line_start;
        let target = next_line_start + col.min(next_line_len);
        self.cursor = clamp_to_boundary(&self.input, target);
    }

    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_cursor = match self.history_cursor {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(new_cursor);
        self.input = self.history[new_cursor].clone();
        self.cursor = self.input.len();
    }

    pub(crate) fn history_next(&mut self) {
        let Some(i) = self.history_cursor else { return };
        if i + 1 >= self.history.len() {
            self.history_cursor = None;
            self.input.clear();
            self.cursor = 0;
        } else {
            self.history_cursor = Some(i + 1);
            self.input = self.history[i + 1].clone();
            self.cursor = self.input.len();
        }
    }

    pub(crate) fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.scroll_offset = 0;
    }

    pub(crate) fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    pub(crate) fn scroll_down(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub(crate) fn enter_dashboard(&mut self, kind: ViewKind, snapshot: DashboardSnapshot) {
        let mut table_state = TableState::default();
        if !snapshot.rows.is_empty() {
            table_state.select(Some(0));
        }
        self.mode = ViewMode::Dashboard {
            kind,
            snapshot,
            table_state,
        };
    }

    pub(crate) fn refresh_dashboard(&mut self, new_snapshot: DashboardSnapshot) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
        {
            if new_snapshot.rows.is_empty() {
                table_state.select(None);
            } else if let Some(sel) = table_state.selected()
                && sel >= new_snapshot.rows.len()
            {
                table_state.select(Some(new_snapshot.rows.len() - 1));
            }
            *snapshot = new_snapshot;
        }
    }

    pub(crate) fn exit_dashboard(&mut self) {
        self.mode = ViewMode::Chat;
    }

    pub(crate) fn dashboard_kind(&self) -> Option<ViewKind> {
        match &self.mode {
            ViewMode::Dashboard { kind, .. } => Some(*kind),
            ViewMode::Chat => None,
        }
    }

    pub(crate) fn dashboard_select_prev(&mut self) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let current = table_state.selected().unwrap_or(0);
            let next = current.saturating_sub(1);
            table_state.select(Some(next));
        }
    }

    pub(crate) fn dashboard_select_next(&mut self) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let last = snapshot.rows.len() - 1;
            let current = table_state.selected().unwrap_or(0);
            let next = (current + 1).min(last);
            table_state.select(Some(next));
        }
    }

    pub(crate) fn dashboard_page(&mut self, delta: isize) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let last = snapshot.rows.len() as isize - 1;
            let current = table_state.selected().unwrap_or(0) as isize;
            let next = (current + delta).clamp(0, last) as usize;
            table_state.select(Some(next));
        }
    }
}

/// Round `idx` down to the nearest UTF-8 char boundary. Used when
/// translating byte-column targets (from multi-line cursor movement) back
/// into safe insertion indices.
fn clamp_to_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_round_trip_ascii() {
        let mut app = AppState::new();
        for c in "hello".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "hello");
        app.backspace();
        app.backspace();
        assert_eq!(app.input, "hel");
    }

    #[test]
    fn multiline_cursor_predicates_track_newlines() {
        let mut app = AppState::new();
        app.input = "a\nbc".to_string();
        app.cursor = 0;
        assert!(app.cursor_at_first_line());
        assert!(!app.cursor_at_last_line());
        app.cursor = 2; // just past '\n', start of line 2
        assert!(!app.cursor_at_first_line());
        assert!(app.cursor_at_last_line());
    }

    #[test]
    fn move_up_and_down_line_clamp_to_line_length() {
        let mut app = AppState::new();
        app.input = "abcd\nef\nghij".to_string();
        app.cursor = 12; // end of "ghij"
        app.move_up_line();
        // Middle line "ef" has len 2; col 4 clamps to 2, so cursor lands at
        // the byte just past "ef" (the next '\n' boundary).
        assert_eq!(app.cursor, 7);
        app.move_up_line();
        // No sticky preferred column: col is recomputed from the current
        // cursor (col 2 on "ef"), then applied to "abcd" → cursor = 2.
        assert_eq!(app.cursor, 2);
        app.move_down_line();
        // Col 2 from "abcd" applies to "ef" (len 2) → end-of-line, cursor 7.
        assert_eq!(app.cursor, 7);
    }

    #[test]
    fn move_up_line_is_noop_on_first_line() {
        let mut app = AppState::new();
        app.input = "only line".to_string();
        app.cursor = 3;
        app.move_up_line();
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn insert_and_backspace_multibyte() {
        let mut app = AppState::new();
        app.insert_char('中');
        app.insert_char('文');
        assert_eq!(app.input, "中文");
        app.backspace();
        assert_eq!(app.input, "中");
    }

    #[test]
    fn take_input_trims_and_remembers() {
        let mut app = AppState::new();
        for c in "  hi  ".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.take_input().as_deref(), Some("hi"));
        assert_eq!(app.history.back().map(String::as_str), Some("hi"));
        assert_eq!(app.input, "");
    }

    #[test]
    fn take_input_rejects_empty() {
        let mut app = AppState::new();
        assert!(app.take_input().is_none());
    }

    #[test]
    fn take_input_dedupes_consecutive_history() {
        let mut app = AppState::new();
        for c in "foo".chars() {
            app.insert_char(c);
        }
        app.take_input();
        for c in "foo".chars() {
            app.insert_char(c);
        }
        app.take_input();
        assert_eq!(app.history.len(), 1);
    }

    #[test]
    fn history_prev_walks_back_then_clamps() {
        let mut app = AppState::new();
        for cmd in ["a", "b", "c"] {
            app.input = cmd.to_string();
            app.cursor = app.input.len();
            app.take_input();
        }
        app.history_prev();
        assert_eq!(app.input, "c");
        app.history_prev();
        assert_eq!(app.input, "b");
        app.history_prev();
        app.history_prev();
        assert_eq!(app.input, "a");
    }

    #[test]
    fn history_next_returns_to_empty_buffer() {
        let mut app = AppState::new();
        app.input = "only".to_string();
        app.cursor = app.input.len();
        app.take_input();
        app.history_prev();
        assert_eq!(app.input, "only");
        app.history_next();
        assert_eq!(app.input, "");
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn scrollback_caps_at_limit() {
        let mut app = AppState::new();
        for i in 0..(SCROLLBACK_CAP + 10) {
            app.push_user(format!("m{i}"));
        }
        assert_eq!(app.scrollback.len(), SCROLLBACK_CAP);
    }

    #[test]
    fn enter_and_exit_dashboard() {
        let mut app = AppState::new();
        let snap = DashboardSnapshot {
            title: "Skills".into(),
            columns: vec!["name".into()],
            rows: vec![vec!["calc".into()]],
            footer: None,
        };
        app.enter_dashboard(ViewKind::Skills, snap);
        assert_eq!(app.dashboard_kind(), Some(ViewKind::Skills));
        app.exit_dashboard();
        assert_eq!(app.dashboard_kind(), None);
    }
}
