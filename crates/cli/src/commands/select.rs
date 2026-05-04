use std::io::{self, IsTerminal, Write};

use crossterm::cursor::{Hide, MoveToPreviousLine, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use crate::error::{CliError, Result};

/// Render a single-select radio list on stderr and return the index
/// the user picked. `Esc` or `Ctrl-C` raise a cancellation error
/// instead of returning an index, so callers never have to interpret
/// "0" as "no choice".
pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    if options.is_empty() {
        return Err(CliError::Config("no options to pick from".into()));
    }

    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive picker requires a terminal".into(),
        ));
    }

    let mut out = stderr.lock();
    writeln!(out, "{label}")?;

    enable_raw_mode()
        .map_err(|e| CliError::Io(format!("failed to enable raw terminal mode: {e}")))?;
    let _guard = RawModeGuard;
    execute!(out, Hide).map_err(|e| CliError::Io(format!("failed to hide cursor: {e}")))?;

    let mut cursor = 0usize;
    render(&mut out, options, cursor)?;

    let picked = loop {
        match event::read()
            .map_err(|e| CliError::Io(format!("failed to read terminal event: {e}")))?
        {
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

    execute!(out, Show).map_err(|e| CliError::Io(format!("failed to restore cursor: {e}")))?;
    // Leave raw mode before the trailing newline — in raw mode `\n`
    // is pure LF (no CR), so the cursor would stay in the middle of
    // the line and the next prompt would inherit the indentation.
    drop(_guard);
    writeln!(out, "  selected: {}", options[picked])?;
    Ok(picked)
}

fn render(out: &mut impl Write, options: &[&str], cursor: usize) -> Result<()> {
    for (i, opt) in options.iter().enumerate() {
        let marker = if i == cursor { ">" } else { " " };
        writeln!(out, "{marker} {opt}\r")?;
    }
    out.flush()?;
    Ok(())
}

fn redraw(out: &mut impl Write, options: &[&str], cursor: usize) -> Result<()> {
    execute!(out, MoveToPreviousLine(options.len() as u16))
        .map_err(|e| CliError::Io(format!("cursor move failed: {e}")))?;
    for (i, opt) in options.iter().enumerate() {
        execute!(out, Clear(ClearType::CurrentLine))
            .map_err(|e| CliError::Io(format!("clear line failed: {e}")))?;
        let marker = if i == cursor { ">" } else { " " };
        writeln!(out, "{marker} {opt}\r")?;
    }
    out.flush()?;
    Ok(())
}

fn step(current: usize, len: usize, delta: i32) -> usize {
    let n = len as i32;
    ((current as i32 + delta).rem_euclid(n)) as usize
}

fn cancel(out: &mut impl Write, rendered_lines: usize) -> Result<usize> {
    execute!(out, Show).ok();
    let _ = disable_raw_mode();
    for _ in 0..rendered_lines {
        let _ = execute!(out, MoveToPreviousLine(1), Clear(ClearType::CurrentLine));
    }
    writeln!(out, "cancelled.")?;
    Err(CliError::Config("interactive picker cancelled".into()))
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), Show);
    }
}
