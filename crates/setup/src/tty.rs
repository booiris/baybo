//! Real-terminal `Prompter` implementation plus the building-block
//! helpers (`prompt_line`, masked-secret reader, raw-mode picker) it's
//! built from. Kept private to the crate so the only public surface is
//! the [`crate::Prompter`] trait and [`TtyPrompter`].

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};

use crate::error::{Result, SetupError};
use crate::prompt::Prompter;

/// `Prompter` impl driving the real terminal. Construct with
/// [`TtyPrompter::new`] which validates that stdin and stderr are both
/// TTYs — every method assumes that invariant holds.
pub struct TtyPrompter {
    tty_fd: i32,
}

impl TtyPrompter {
    pub fn new() -> Result<Self> {
        let stdin = io::stdin();
        let stderr = io::stderr();
        if !stdin.is_terminal() || !stderr.is_terminal() {
            return Err(SetupError::NotATerminal);
        }
        Ok(Self {
            tty_fd: stdin.as_raw_fd(),
        })
    }
}

impl Prompter for TtyPrompter {
    fn select(&mut self, label: &str, options: &[&str]) -> Result<usize> {
        select_one(label, options)
    }

    fn multi_select(
        &mut self,
        label: &str,
        options: &[&str],
        initial: &[bool],
    ) -> Result<Vec<usize>> {
        select_many(label, options, initial)
    }

    fn text(&mut self, label: &str, default: &str) -> Result<String> {
        prompt_with_default(label, default)
    }

    fn confirm(&mut self, label: &str, default: bool) -> Result<bool> {
        confirm_with_default(label, default)
    }

    fn password(&mut self, label: &str) -> Result<String> {
        let _guard = RawModeGuard::new(self.tty_fd)?;
        let stdin = io::stdin();
        let stderr = io::stderr();
        let mut reader = stdin.lock();
        let mut writer = stderr.lock();
        read_masked_secret(&mut reader, &mut writer, label)
    }
}

// ---------------------------------------------------------------------------
// Free helpers — same shapes as their `aura-cli` counterparts, with the
// error type swapped for `SetupError`. Kept `pub(crate)` so other modules
// in this crate can reuse them without going through `Prompter`.
// ---------------------------------------------------------------------------

pub(crate) fn prompt_line<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer
        .write_all(label.as_bytes())
        .map_err(|e| SetupError::Prompt(format!("write prompt: {e}")))?;
    writer
        .flush()
        .map_err(|e| SetupError::Prompt(format!("flush prompt: {e}")))?;
    let mut buf = String::new();
    let bytes = reader
        .read_line(&mut buf)
        .map_err(|e| SetupError::Prompt(format!("read line: {e}")))?;
    if bytes == 0 {
        return Err(SetupError::Prompt(
            "stdin closed while reading interactive input".into(),
        ));
    }
    Ok(buf.trim().to_string())
}

pub(crate) fn confirm_with_default(question: &str, default: bool) -> Result<bool> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let label = format!("{question} {suffix}: ");
    let ans = prompt_line(&mut reader, &mut writer, &label)?;
    Ok(match ans.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

pub(crate) fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let display = if default.is_empty() {
        format!("{label}: ")
    } else {
        format!("{label} [{default}]: ")
    };
    let value = prompt_line(&mut reader, &mut writer, &display)?;
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    if options.is_empty() {
        return Err(SetupError::Prompt("no options to pick from".into()));
    }
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }

    let mut out = stderr.lock();
    let _guard = PickerGuard::enter(&mut out)?;

    let mut cursor = 0usize;
    let mut view = Viewport::new(options.len());
    let picked = loop {
        draw_screen(&mut out, label, &view, cursor, |i| {
            single_row(i, options[i], cursor)
        })?;
        match event::read().map_err(|e| SetupError::Prompt(format!("read event: {e}")))? {
            Event::Resize(_, rows) => view.resize(rows as usize, cursor),
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => {}
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = step(cursor, options.len(), -1);
                    view.follow(cursor);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = step(cursor, options.len(), 1);
                    view.follow(cursor);
                }
                KeyCode::Enter => break Some(cursor),
                KeyCode::Esc => break None,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break None,
                _ => {}
            },
            _ => {}
        }
    };

    drop(_guard);
    match picked {
        Some(idx) => {
            echo_outcome(&mut out, label, &format!("  selected: {}", options[idx]))?;
            Ok(idx)
        }
        None => {
            echo_outcome(&mut out, label, "cancelled.")?;
            Err(SetupError::Cancelled)
        }
    }
}

/// Terminal rows reserved around the picker block: the label drawn at the
/// top of the screen and the position footer drawn below the list.
const PICKER_RESERVED_ROWS: usize = 2;

/// Visible option rows the picker may use, capped to the terminal height
/// minus [`PICKER_RESERVED_ROWS`] (label + footer) so the whole block fits
/// on one screen. The picker draws into the alternate screen from a fixed
/// origin, so an oversized list is windowed here rather than scrolled.
fn visible_rows(total: usize, term_rows: usize) -> usize {
    total.min(term_rows.saturating_sub(PICKER_RESERVED_ROWS).max(1))
}

/// Scrolling window over a (possibly oversized) option list: tracks which
/// contiguous slice is on screen and slides it to keep the cursor visible.
struct Viewport {
    total: usize,
    height: usize,
    first: usize,
}

impl Viewport {
    fn new(total: usize) -> Self {
        let term_rows = size().map(|(_, h)| h as usize).unwrap_or(24);
        Self {
            total,
            height: visible_rows(total, term_rows),
            first: 0,
        }
    }

    /// Recompute the window for a new terminal height (a resize event) and
    /// re-anchor it on the cursor.
    fn resize(&mut self, term_rows: usize, cursor: usize) {
        self.height = visible_rows(self.total, term_rows);
        if self.first + self.height > self.total {
            self.first = self.total.saturating_sub(self.height);
        }
        self.follow(cursor);
    }

    fn windowed(&self) -> bool {
        self.height < self.total
    }

    /// Slide the window so `cursor` stays within the visible slice.
    fn follow(&mut self, cursor: usize) {
        if cursor < self.first {
            self.first = cursor;
        } else if cursor >= self.first + self.height {
            self.first = cursor + 1 - self.height;
        }
    }

    fn footer(&self, cursor: usize) -> Option<String> {
        self.windowed()
            .then(|| format!("  {}/{}", cursor + 1, self.total))
    }
}

fn single_row(index: usize, opt: &str, cursor: usize) -> String {
    let marker = if index == cursor { ">" } else { " " };
    format!("{marker} {opt}")
}

/// Repaint the picker from the top-left of the alternate screen: the label,
/// the visible window of `row`-formatted options, then the position footer.
/// Drawing from a fixed origin every frame (rather than moving the cursor
/// back over the previous block) is what makes the picker immune to
/// scrolling and terminal resizes.
fn draw_screen(
    out: &mut impl Write,
    label: &str,
    view: &Viewport,
    cursor: usize,
    row: impl Fn(usize) -> String,
) -> Result<()> {
    execute!(out, MoveTo(0, 0)).map_err(|e| SetupError::Prompt(format!("cursor move: {e}")))?;
    clear_then(out, label)?;
    for i in view.first..view.first + view.height {
        clear_then(out, &row(i))?;
    }
    // Wipe anything left below the list (e.g. after a shrink) BEFORE the
    // footer. The footer is drawn last and WITHOUT a trailing newline:
    // emitting a newline on the bottom row would scroll the alternate
    // screen and break the fixed-origin redraw.
    execute!(out, Clear(ClearType::FromCursorDown))
        .map_err(|e| SetupError::Prompt(format!("clear tail: {e}")))?;
    if let Some(footer) = view.footer(cursor) {
        write!(out, "{footer}").map_err(|e| SetupError::Prompt(format!("write footer: {e}")))?;
    }
    out.flush()
        .map_err(|e| SetupError::Prompt(format!("flush picker: {e}")))?;
    Ok(())
}

fn clear_then(out: &mut impl Write, line: &str) -> Result<()> {
    execute!(out, Clear(ClearType::CurrentLine))
        .map_err(|e| SetupError::Prompt(format!("clear line: {e}")))?;
    writeln!(out, "{line}\r").map_err(|e| SetupError::Prompt(format!("write row: {e}")))?;
    Ok(())
}

/// Multi-select sibling of [`select_one`]: a checkbox list where space
/// toggles the row under the cursor and enter confirms the whole set.
/// `initial[i]` seeds row `i`'s checked state. Returns the checked
/// indices in ascending order.
pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    if options.is_empty() {
        return Err(SetupError::Prompt("no options to pick from".into()));
    }
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }

    let mut checked: Vec<bool> = (0..options.len())
        .map(|i| initial.get(i).copied().unwrap_or(false))
        .collect();

    let mut out = stderr.lock();
    let _guard = PickerGuard::enter(&mut out)?;

    let mut cursor = 0usize;
    let mut view = Viewport::new(options.len());
    let confirmed = loop {
        draw_screen(&mut out, label, &view, cursor, |i| {
            multi_row(i, options[i], checked[i], cursor)
        })?;
        match event::read().map_err(|e| SetupError::Prompt(format!("read event: {e}")))? {
            Event::Resize(_, rows) => view.resize(rows as usize, cursor),
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => {}
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = step(cursor, options.len(), -1);
                    view.follow(cursor);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = step(cursor, options.len(), 1);
                    view.follow(cursor);
                }
                KeyCode::Char(' ') => {
                    checked[cursor] = !checked[cursor];
                }
                KeyCode::Enter => break true,
                KeyCode::Esc => break false,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break false,
                _ => {}
            },
            _ => {}
        }
    };

    drop(_guard);
    if !confirmed {
        echo_outcome(&mut out, label, "cancelled.")?;
        return Err(SetupError::Cancelled);
    }
    let picked: Vec<usize> = checked
        .iter()
        .enumerate()
        .filter_map(|(i, &on)| on.then_some(i))
        .collect();
    let summary = if picked.is_empty() {
        "(none)".to_string()
    } else {
        picked
            .iter()
            .map(|&i| options[i])
            .collect::<Vec<_>>()
            .join(", ")
    };
    echo_outcome(&mut out, label, &format!("  selected: {summary}"))?;
    Ok(picked)
}

fn multi_row(index: usize, opt: &str, checked: bool, cursor: usize) -> String {
    let pointer = if index == cursor { ">" } else { " " };
    let mark = if checked { "[x]" } else { "[ ]" };
    format!("{pointer} {mark} {opt}")
}

fn step(current: usize, len: usize, delta: i32) -> usize {
    let n = len as i32;
    ((current as i32 + delta).rem_euclid(n)) as usize
}

/// Print the picker's outcome to the main screen after [`PickerGuard`] has
/// restored it: the label followed by the result (or cancellation) line.
fn echo_outcome(out: &mut impl Write, label: &str, line: &str) -> Result<()> {
    writeln!(out, "{label}").map_err(|e| SetupError::Prompt(format!("write label: {e}")))?;
    writeln!(out, "{line}").map_err(|e| SetupError::Prompt(format!("write echo: {e}")))?;
    Ok(())
}

/// Owns the raw-mode + alternate-screen state for the lifetime of a picker.
/// Constructed via [`PickerGuard::enter`]; `Drop` restores the terminal on
/// every exit path (confirm, cancel, `?`-propagated error, or panic).
struct PickerGuard;

impl PickerGuard {
    fn enter(out: &mut impl Write) -> Result<Self> {
        enable_raw_mode().map_err(|e| SetupError::Prompt(format!("enable raw mode: {e}")))?;
        // Undo raw mode if switching to the alternate screen fails, so a
        // partial entry never leaves the terminal wedged.
        if let Err(e) = execute!(out, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(SetupError::Prompt(format!("enter picker screen: {e}")));
        }
        Ok(Self)
    }
}

impl Drop for PickerGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// Masked-secret reader. termios `ECHO` + `ICANON` disabled while bytes flow.
// ---------------------------------------------------------------------------

pub(crate) struct RawModeGuard {
    fd: i32,
    original: libc::termios,
}

#[allow(unsafe_code)]
impl RawModeGuard {
    pub(crate) fn new(fd: i32) -> Result<Self> {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `fd` is the caller's raw stdin fd; tcgetattr writes
        // exactly one termios struct into the uninit slot we own.
        let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
        if rc != 0 {
            return Err(SetupError::Prompt(format!(
                "read terminal mode: {}",
                io::Error::last_os_error()
            )));
        }
        // SAFETY: tcgetattr returned 0, which guarantees the slot is
        // fully initialised.
        let original = unsafe { termios.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: same fd, fully-initialised termios passed by reference.
        let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        if rc != 0 {
            return Err(SetupError::Prompt(format!(
                "enable raw terminal mode: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(Self { fd, original })
    }
}

#[allow(unsafe_code)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: the fd was open when we captured `original`; the
        // original termios is unchanged and valid to pass back.
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

pub(crate) fn read_masked_secret<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer
        .write_all(label.as_bytes())
        .map_err(|e| SetupError::Prompt(format!("write label: {e}")))?;
    writer
        .flush()
        .map_err(|e| SetupError::Prompt(format!("flush label: {e}")))?;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .map_err(|e| SetupError::Prompt(format!("read masked input: {e}")))?;
        if n == 0 {
            let _ = writer.write_all(b"\n");
            let _ = writer.flush();
            return Err(SetupError::Prompt(
                "stdin closed while reading masked input".into(),
            ));
        }
        match byte[0] {
            b'\n' | b'\r' => {
                writer
                    .write_all(b"\n")
                    .map_err(|e| SetupError::Prompt(format!("write newline: {e}")))?;
                writer
                    .flush()
                    .map_err(|e| SetupError::Prompt(format!("flush newline: {e}")))?;
                break;
            }
            0x08 | 0x7f => {
                if buf.pop().is_some() {
                    let _ = writer.write_all(b"\x08 \x08");
                    let _ = writer.flush();
                }
            }
            b => {
                buf.push(b);
                let _ = writer.write_all(b"*");
                let _ = writer.flush();
            }
        }
    }

    String::from_utf8(buf)
        .map_err(|e| SetupError::Prompt(format!("input must be valid utf-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn masked_secret_allows_empty_string() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        let token = read_masked_secret(&mut input, &mut output, "p: ").unwrap();
        assert!(token.is_empty());
    }

    #[test]
    fn masked_secret_handles_backspace() {
        let mut input = Cursor::new(b"ab\x7fc\n".to_vec());
        let mut output = Vec::new();
        let token = read_masked_secret(&mut input, &mut output, "p: ").unwrap();
        assert_eq!(token, "ac");
    }

    #[test]
    fn masked_secret_does_not_echo_input_chars() {
        let mut input = Cursor::new(b"hunter2\n".to_vec());
        let mut output: Vec<u8> = Vec::new();
        read_masked_secret(&mut input, &mut output, "p: ").unwrap();
        let rendered = String::from_utf8_lossy(&output);
        assert!(!rendered.contains("hunter2"));
        assert_eq!(rendered.matches('*').count(), 7);
    }

    #[test]
    fn visible_rows_caps_to_height_minus_reserved() {
        assert_eq!(visible_rows(20, 12), 10); // 12 - 2 reserved
        assert_eq!(visible_rows(4, 12), 4); // list shorter than the cap
        assert_eq!(visible_rows(20, 1), 1); // degenerate height floors at 1
    }

    #[test]
    fn viewport_not_windowed_when_list_fits() {
        let view = Viewport {
            total: 4,
            height: 4,
            first: 0,
        };
        assert!(!view.windowed());
        assert_eq!(view.footer(0), None);
    }

    #[test]
    fn viewport_windowed_shows_a_position_footer() {
        let view = Viewport {
            total: 20,
            height: 8,
            first: 0,
        };
        assert!(view.windowed());
        assert_eq!(view.footer(2), Some("  3/20".to_string()));
    }

    #[test]
    fn viewport_follow_scrolls_down_then_back_up() {
        let mut view = Viewport {
            total: 10,
            height: 3,
            first: 0,
        };
        view.follow(2); // last visible row — window unchanged
        assert_eq!(view.first, 0);
        view.follow(3); // steps past the bottom edge — slide down one
        assert_eq!(view.first, 1);
        view.follow(9); // wrap to the end — window ends flush with the list
        assert_eq!(view.first, 7);
        view.follow(0); // wrap back to the top
        assert_eq!(view.first, 0);
    }

    #[test]
    fn viewport_follow_keeps_cursor_visible() {
        let mut view = Viewport {
            total: 20,
            height: 8,
            first: 0,
        };
        // Walk the cursor down the whole list and back up the way the arrow
        // keys drive it: the window must always contain the cursor and never
        // run past the end.
        for cursor in (0..view.total).chain((0..view.total).rev()) {
            view.follow(cursor);
            assert!(view.first <= cursor);
            assert!(cursor < view.first + view.height);
            assert!(view.first + view.height <= view.total);
        }
    }

    #[test]
    fn viewport_resize_rewindows_and_keeps_cursor_visible() {
        let mut view = Viewport {
            total: 20,
            height: 10,
            first: 8,
        };
        let cursor = 15;
        view.resize(7, cursor); // shrink: height 7 - 2 = 5
        assert_eq!(view.height, 5);
        assert!(view.first <= cursor && cursor < view.first + view.height);
        assert!(view.first + view.height <= view.total);

        view.resize(40, cursor); // grow past the list: whole list fits
        assert_eq!(view.height, 20);
        assert!(!view.windowed());
        assert_eq!(view.first, 0);
    }
}
