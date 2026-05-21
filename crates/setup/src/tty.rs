//! Real-terminal `Prompter` implementation plus the building-block
//! helpers (`prompt_line`, masked-secret reader, raw-mode picker) it's
//! built from. Kept private to the crate so the only public surface is
//! the [`crate::Prompter`] trait and [`TtyPrompter`].

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;

use crossterm::cursor::{Hide, MoveToPreviousLine, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

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
    writeln!(out, "{label}").map_err(|e| SetupError::Prompt(format!("write label: {e}")))?;

    enable_raw_mode().map_err(|e| SetupError::Prompt(format!("enable raw mode: {e}")))?;
    let _guard = SelectRawGuard;
    execute!(out, Hide).map_err(|e| SetupError::Prompt(format!("hide cursor: {e}")))?;

    let mut cursor = 0usize;
    render(&mut out, options, cursor)?;

    let picked = loop {
        match event::read().map_err(|e| SetupError::Prompt(format!("read event: {e}")))? {
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => continue,
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = step(cursor, options.len(), -1);
                    redraw(&mut out, options, cursor)?;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = step(cursor, options.len(), 1);
                    redraw(&mut out, options, cursor)?;
                }
                KeyCode::Enter => break cursor,
                KeyCode::Esc => return cancel(&mut out, options.len()),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return cancel(&mut out, options.len());
                }
                _ => {}
            },
            _ => {}
        }
    };

    execute!(out, Show).map_err(|e| SetupError::Prompt(format!("show cursor: {e}")))?;
    drop(_guard);
    writeln!(out, "  selected: {}", options[picked])
        .map_err(|e| SetupError::Prompt(format!("write echo: {e}")))?;
    Ok(picked)
}

fn render(out: &mut impl Write, options: &[&str], cursor: usize) -> Result<()> {
    for (i, opt) in options.iter().enumerate() {
        let marker = if i == cursor { ">" } else { " " };
        writeln!(out, "{marker} {opt}\r")
            .map_err(|e| SetupError::Prompt(format!("write row: {e}")))?;
    }
    out.flush()
        .map_err(|e| SetupError::Prompt(format!("flush picker: {e}")))?;
    Ok(())
}

fn redraw(out: &mut impl Write, options: &[&str], cursor: usize) -> Result<()> {
    execute!(out, MoveToPreviousLine(options.len() as u16))
        .map_err(|e| SetupError::Prompt(format!("cursor move: {e}")))?;
    for (i, opt) in options.iter().enumerate() {
        execute!(out, Clear(ClearType::CurrentLine))
            .map_err(|e| SetupError::Prompt(format!("clear line: {e}")))?;
        let marker = if i == cursor { ">" } else { " " };
        writeln!(out, "{marker} {opt}\r")
            .map_err(|e| SetupError::Prompt(format!("write row: {e}")))?;
    }
    out.flush()
        .map_err(|e| SetupError::Prompt(format!("flush picker: {e}")))?;
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
    writeln!(out, "{label}").map_err(|e| SetupError::Prompt(format!("write label: {e}")))?;

    enable_raw_mode().map_err(|e| SetupError::Prompt(format!("enable raw mode: {e}")))?;
    let _guard = SelectRawGuard;
    execute!(out, Hide).map_err(|e| SetupError::Prompt(format!("hide cursor: {e}")))?;

    let mut cursor = 0usize;
    render_multi(&mut out, options, &checked, cursor)?;

    loop {
        match event::read().map_err(|e| SetupError::Prompt(format!("read event: {e}")))? {
            Event::Key(KeyEvent {
                kind: KeyEventKind::Release,
                ..
            }) => continue,
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = step(cursor, options.len(), -1);
                    redraw_multi(&mut out, options, &checked, cursor)?;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    cursor = step(cursor, options.len(), 1);
                    redraw_multi(&mut out, options, &checked, cursor)?;
                }
                KeyCode::Char(' ') => {
                    checked[cursor] = !checked[cursor];
                    redraw_multi(&mut out, options, &checked, cursor)?;
                }
                KeyCode::Enter => break,
                KeyCode::Esc => return cancel(&mut out, options.len()),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    return cancel(&mut out, options.len());
                }
                _ => {}
            },
            _ => {}
        }
    }

    execute!(out, Show).map_err(|e| SetupError::Prompt(format!("show cursor: {e}")))?;
    drop(_guard);
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
    writeln!(out, "  selected: {summary}")
        .map_err(|e| SetupError::Prompt(format!("write echo: {e}")))?;
    Ok(picked)
}

fn multi_row(index: usize, opt: &str, checked: bool, cursor: usize) -> String {
    let pointer = if index == cursor { ">" } else { " " };
    let mark = if checked { "[x]" } else { "[ ]" };
    format!("{pointer} {mark} {opt}")
}

fn render_multi(
    out: &mut impl Write,
    options: &[&str],
    checked: &[bool],
    cursor: usize,
) -> Result<()> {
    for (i, opt) in options.iter().enumerate() {
        writeln!(out, "{}\r", multi_row(i, opt, checked[i], cursor))
            .map_err(|e| SetupError::Prompt(format!("write row: {e}")))?;
    }
    out.flush()
        .map_err(|e| SetupError::Prompt(format!("flush picker: {e}")))?;
    Ok(())
}

fn redraw_multi(
    out: &mut impl Write,
    options: &[&str],
    checked: &[bool],
    cursor: usize,
) -> Result<()> {
    execute!(out, MoveToPreviousLine(options.len() as u16))
        .map_err(|e| SetupError::Prompt(format!("cursor move: {e}")))?;
    for (i, opt) in options.iter().enumerate() {
        execute!(out, Clear(ClearType::CurrentLine))
            .map_err(|e| SetupError::Prompt(format!("clear line: {e}")))?;
        writeln!(out, "{}\r", multi_row(i, opt, checked[i], cursor))
            .map_err(|e| SetupError::Prompt(format!("write row: {e}")))?;
    }
    out.flush()
        .map_err(|e| SetupError::Prompt(format!("flush picker: {e}")))?;
    Ok(())
}

fn step(current: usize, len: usize, delta: i32) -> usize {
    let n = len as i32;
    ((current as i32 + delta).rem_euclid(n)) as usize
}

fn cancel<T>(out: &mut impl Write, rendered_lines: usize) -> Result<T> {
    execute!(out, Show).ok();
    let _ = disable_raw_mode();
    for _ in 0..rendered_lines {
        let _ = execute!(out, MoveToPreviousLine(1), Clear(ClearType::CurrentLine));
    }
    let _ = writeln!(out, "cancelled.");
    Err(SetupError::Cancelled)
}

struct SelectRawGuard;

impl Drop for SelectRawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), Show);
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
}
