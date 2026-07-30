//! Real-terminal `Prompter` implementation. Pickers use a small inline
//! arrow-key viewport; text and confirmation prompts remain line-oriented,
//! and password input is masked. The terminal never enters an alternate
//! screen, so completed prompts remain in normal scrollback.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::error::{Result, SetupError};
use crate::prompt::Prompter;

const MAX_MENU_ROWS: usize = 12;
const DEFAULT_TERMINAL_COLUMNS: usize = 80;
const DEFAULT_TERMINAL_ROWS: usize = 24;
const SINGLE_SELECT_HINT: &str = "↑/↓ move · Enter select";
const MULTI_SELECT_HINT: &str = "↑/↓ move · Space toggle · Enter confirm";
const INTERACTIVE_INPUT_CANCELLED: &str = "interactive input cancelled";
const INTERACTIVE_STDIN_CLOSED: &str = "stdin closed while reading interactive input";
pub(crate) const PROMPT_DIVIDER: &str = "──────────────────────────────────────────────";

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
// Free helpers — same shapes as their `baybo-cli` counterparts, with the
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
        return Err(SetupError::Prompt(INTERACTIVE_STDIN_CLOSED.into()));
    }
    Ok(buf.trim().to_string())
}

/// Print one line to the prompt writer. Shared by the menu rows, the
/// re-prompt hints, and the selection echo so they all map I/O errors the
/// same way.
fn emit<W: Write>(writer: &mut W, line: &str) -> Result<()> {
    writeln!(writer, "{line}").map_err(|e| SetupError::Prompt(format!("write line: {e}")))
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

// ---------------------------------------------------------------------------
// Inline arrow-key pickers.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct TerminalDimensions {
    columns: usize,
    rows: usize,
}

impl Default for TerminalDimensions {
    fn default() -> Self {
        Self {
            columns: DEFAULT_TERMINAL_COLUMNS,
            rows: DEFAULT_TERMINAL_ROWS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuKey {
    Up,
    Down,
    Toggle,
    Submit,
    Cancel,
}

#[derive(Debug, Default)]
struct MenuKeyDecoder {
    escape: EscapeState,
}

#[derive(Debug, Default)]
enum EscapeState {
    #[default]
    Ground,
    Escape,
    Csi,
    Ss3,
}

impl MenuKeyDecoder {
    fn feed(&mut self, byte: u8) -> Option<MenuKey> {
        match self.escape {
            EscapeState::Ground => self.feed_ground(byte),
            EscapeState::Escape => match byte {
                b'[' => {
                    self.escape = EscapeState::Csi;
                    None
                }
                b'O' => {
                    self.escape = EscapeState::Ss3;
                    None
                }
                _ => {
                    self.escape = EscapeState::Ground;
                    self.feed_ground(byte)
                }
            },
            EscapeState::Csi | EscapeState::Ss3 => {
                self.escape = EscapeState::Ground;
                match byte {
                    b'A' => Some(MenuKey::Up),
                    b'B' => Some(MenuKey::Down),
                    _ => None,
                }
            }
        }
    }

    fn feed_ground(&mut self, byte: u8) -> Option<MenuKey> {
        match byte {
            0x1b => {
                self.escape = EscapeState::Escape;
                None
            }
            b'\n' | b'\r' => Some(MenuKey::Submit),
            b' ' => Some(MenuKey::Toggle),
            0x03 | 0x04 | 0x1a | 0x1c => Some(MenuKey::Cancel),
            _ => None,
        }
    }
}

pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    ensure_options(options)?;
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let tty_fd = stdin.as_raw_fd();
    let dimensions = terminal_dimensions(tty_fd);
    let _guard = RawModeGuard::new(tty_fd)?;
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    select_one_from(&mut reader, &mut writer, label, options, dimensions)
}

fn select_one_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    options: &[&str],
    dimensions: TerminalDimensions,
) -> Result<usize> {
    ensure_options(options)?;
    emit(writer, label)?;
    let mut cursor = 0;
    let mut decoder = MenuKeyDecoder::default();
    render_single_menu(writer, options, cursor, dimensions, false)?;

    loop {
        match read_menu_key(reader, &mut decoder)? {
            MenuKey::Up => {
                cursor = previous_index(cursor, options.len());
                render_single_menu(writer, options, cursor, dimensions, true)?;
            }
            MenuKey::Down => {
                cursor = next_index(cursor, options.len());
                render_single_menu(writer, options, cursor, dimensions, true)?;
            }
            MenuKey::Submit => {
                finish_menu(
                    writer,
                    &format!("  selected: {}", options[cursor]),
                    dimensions.columns,
                )?;
                return Ok(cursor);
            }
            MenuKey::Cancel => return cancel_menu(writer, dimensions.columns),
            MenuKey::Toggle => {}
        }
    }
}

fn render_single_menu<W: Write>(
    writer: &mut W,
    options: &[&str],
    cursor: usize,
    dimensions: TerminalDimensions,
    redraw: bool,
) -> Result<()> {
    render_menu(
        writer,
        options.len(),
        cursor,
        dimensions,
        redraw,
        format!("{}/{} · {SINGLE_SELECT_HINT}", cursor + 1, options.len()),
        |idx, highlighted| {
            let pointer = if highlighted { "›" } else { " " };
            format!("{pointer} {}", options[idx])
        },
    )
}

pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    ensure_options(options)?;
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let tty_fd = stdin.as_raw_fd();
    let dimensions = terminal_dimensions(tty_fd);
    let _guard = RawModeGuard::new(tty_fd)?;
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    select_many_from(
        &mut reader,
        &mut writer,
        label,
        options,
        initial,
        dimensions,
    )
}

fn select_many_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    options: &[&str],
    initial: &[bool],
    dimensions: TerminalDimensions,
) -> Result<Vec<usize>> {
    ensure_options(options)?;
    let mut checked: Vec<bool> = (0..options.len())
        .map(|idx| initial.get(idx).copied().unwrap_or(false))
        .collect();
    let mut cursor = 0;
    let mut decoder = MenuKeyDecoder::default();
    emit(writer, label)?;
    render_multi_menu(writer, options, &checked, cursor, dimensions, false)?;

    loop {
        match read_menu_key(reader, &mut decoder)? {
            MenuKey::Up => {
                cursor = previous_index(cursor, options.len());
                render_multi_menu(writer, options, &checked, cursor, dimensions, true)?;
            }
            MenuKey::Down => {
                cursor = next_index(cursor, options.len());
                render_multi_menu(writer, options, &checked, cursor, dimensions, true)?;
            }
            MenuKey::Toggle => {
                checked[cursor] = !checked[cursor];
                render_multi_menu(writer, options, &checked, cursor, dimensions, true)?;
            }
            MenuKey::Submit => {
                let picks: Vec<usize> = checked
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, selected)| selected.then_some(idx))
                    .collect();
                finish_menu(
                    writer,
                    &format!("  selected: {}", summarize(options, &picks)),
                    dimensions.columns,
                )?;
                return Ok(picks);
            }
            MenuKey::Cancel => return cancel_menu(writer, dimensions.columns),
        }
    }
}

fn render_multi_menu<W: Write>(
    writer: &mut W,
    options: &[&str],
    checked: &[bool],
    cursor: usize,
    dimensions: TerminalDimensions,
    redraw: bool,
) -> Result<()> {
    let checked_count = checked.iter().filter(|selected| **selected).count();
    render_menu(
        writer,
        options.len(),
        cursor,
        dimensions,
        redraw,
        format!(
            "{}/{} · {checked_count} checked · {MULTI_SELECT_HINT}",
            cursor + 1,
            options.len()
        ),
        |idx, highlighted| {
            let pointer = if highlighted { "›" } else { " " };
            let mark = if checked[idx] { "[x]" } else { "[ ]" };
            format!("{pointer} {mark} {}", options[idx])
        },
    )
}

fn render_menu<W, F>(
    writer: &mut W,
    option_count: usize,
    cursor: usize,
    dimensions: TerminalDimensions,
    redraw: bool,
    footer: String,
    mut render_row: F,
) -> Result<()>
where
    W: Write,
    F: FnMut(usize, bool) -> String,
{
    let visible_rows = visible_menu_rows(option_count, dimensions.rows);
    if redraw {
        rewind_lines(writer, visible_rows + 2)?;
    }
    let start = menu_window_start(cursor, option_count, visible_rows);
    for idx in start..start + visible_rows {
        write_menu_line(writer, &render_row(idx, idx == cursor), dimensions.columns)?;
    }
    let divider: String = PROMPT_DIVIDER
        .chars()
        .take(dimensions.columns.saturating_sub(1).max(1))
        .collect();
    write_menu_line(writer, &divider, dimensions.columns)?;
    write_menu_line(writer, &footer, dimensions.columns)?;
    writer
        .flush()
        .map_err(|e| SetupError::Prompt(format!("flush menu: {e}")))
}

fn read_menu_key<R: Read>(reader: &mut R, decoder: &mut MenuKeyDecoder) -> Result<MenuKey> {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return Err(SetupError::Prompt(INTERACTIVE_STDIN_CLOSED.into())),
            Ok(_) => {
                if let Some(key) = decoder.feed(byte[0]) {
                    return Ok(key);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(SetupError::Prompt(format!("read interactive input: {e}")));
            }
        }
    }
}

fn ensure_options(options: &[&str]) -> Result<()> {
    if options.is_empty() {
        Err(SetupError::Prompt("no options to pick from".into()))
    } else {
        Ok(())
    }
}

fn previous_index(cursor: usize, len: usize) -> usize {
    if cursor == 0 { len - 1 } else { cursor - 1 }
}

fn next_index(cursor: usize, len: usize) -> usize {
    (cursor + 1) % len
}

fn visible_menu_rows(option_count: usize, terminal_rows: usize) -> usize {
    option_count
        .min(MAX_MENU_ROWS)
        .min(terminal_rows.saturating_sub(4).max(1))
}

fn menu_window_start(cursor: usize, option_count: usize, visible_rows: usize) -> usize {
    cursor
        .saturating_sub(visible_rows / 2)
        .min(option_count - visible_rows)
}

fn rewind_lines<W: Write>(writer: &mut W, lines: usize) -> Result<()> {
    write!(writer, "\x1b[{lines}A\r").map_err(|e| SetupError::Prompt(format!("rewind menu: {e}")))
}

fn write_menu_line<W: Write>(writer: &mut W, line: &str, columns: usize) -> Result<()> {
    let fitted = fit_terminal_line(line, columns.saturating_sub(1).max(1));
    writeln!(writer, "\x1b[2K\r{fitted}")
        .map_err(|e| SetupError::Prompt(format!("write menu: {e}")))
}

fn fit_terminal_line(line: &str, max_width: usize) -> String {
    let mut fitted = String::new();
    let mut width = 0;
    let mut truncated = false;
    for ch in line.chars() {
        let visible = if ch.is_control() { ' ' } else { ch };
        let char_width = UnicodeWidthChar::width(visible).unwrap_or(0);
        if width + char_width > max_width {
            truncated = true;
            break;
        }
        fitted.push(visible);
        width += char_width;
    }
    if truncated {
        while UnicodeWidthStr::width(fitted.as_str()) + 1 > max_width {
            if fitted.pop().is_none() {
                break;
            }
        }
        fitted.push('…');
    }
    fitted
}

fn finish_menu<W: Write>(writer: &mut W, summary: &str, columns: usize) -> Result<()> {
    rewind_lines(writer, 1)?;
    write_menu_line(writer, summary, columns)?;
    writer
        .flush()
        .map_err(|e| SetupError::Prompt(format!("flush menu: {e}")))
}

fn cancel_menu<T, W: Write>(writer: &mut W, columns: usize) -> Result<T> {
    finish_menu(writer, "  cancelled", columns)?;
    Err(SetupError::Prompt(INTERACTIVE_INPUT_CANCELLED.into()))
}

/// Comma-joined option labels for the chosen indices, or `(none)`.
fn summarize(options: &[&str], picks: &[usize]) -> String {
    if picks.is_empty() {
        "(none)".to_string()
    } else {
        picks
            .iter()
            .filter_map(|&i| options.get(i).copied())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// Masked-secret reader. termios `ECHO` + `ICANON` + `ISIG` are disabled
// while bytes flow, so control-key cancellation can restore the terminal
// through `RawModeGuard::drop`.
// ---------------------------------------------------------------------------

#[allow(unsafe_code)]
fn terminal_dimensions(fd: i32) -> TerminalDimensions {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: `fd` is a live TTY descriptor and `size` points to writable
    // storage for the one `winsize` value populated by `TIOCGWINSZ`.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if rc != 0 {
        return TerminalDimensions::default();
    }
    // SAFETY: a successful `TIOCGWINSZ` initialized the whole structure.
    let size = unsafe { size.assume_init() };
    let fallback = TerminalDimensions::default();
    TerminalDimensions {
        columns: if size.ws_col == 0 {
            fallback.columns
        } else {
            usize::from(size.ws_col)
        },
        rows: if size.ws_row == 0 {
            fallback.rows
        } else {
            usize::from(size.ws_row)
        },
    }
}

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
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
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
            return Err(SetupError::Prompt(INTERACTIVE_STDIN_CLOSED.into()));
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
            0x03 | 0x1a | 0x1c => {
                let _ = writer.write_all(b"\n");
                let _ = writer.flush();
                return Err(SetupError::Prompt(INTERACTIVE_INPUT_CANCELLED.into()));
            }
            0x04 => {
                let _ = writer.write_all(b"\n");
                let _ = writer.flush();
                return Err(SetupError::Prompt(INTERACTIVE_STDIN_CLOSED.into()));
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

    const TEST_DIMENSIONS: TerminalDimensions = TerminalDimensions {
        columns: 80,
        rows: 24,
    };

    fn run_select(input: &[u8], options: &[&str]) -> (Result<usize>, String) {
        let mut reader = Cursor::new(input.to_vec());
        let mut out: Vec<u8> = Vec::new();
        let res = select_one_from(&mut reader, &mut out, "Pick one:", options, TEST_DIMENSIONS);
        (res, String::from_utf8_lossy(&out).into_owned())
    }

    fn run_multi(input: &[u8], options: &[&str], initial: &[bool]) -> (Result<Vec<usize>>, String) {
        let mut reader = Cursor::new(input.to_vec());
        let mut out: Vec<u8> = Vec::new();
        let res = select_many_from(
            &mut reader,
            &mut out,
            "Pick many:",
            options,
            initial,
            TEST_DIMENSIONS,
        );
        (res, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn arrow_decoder_accepts_csi_and_ss3_sequences() {
        let mut decoder = MenuKeyDecoder::default();
        assert_eq!(decoder.feed(0x1b), None);
        assert_eq!(decoder.feed(b'['), None);
        assert_eq!(decoder.feed(b'B'), Some(MenuKey::Down));
        assert_eq!(decoder.feed(0x1b), None);
        assert_eq!(decoder.feed(b'O'), None);
        assert_eq!(decoder.feed(b'A'), Some(MenuKey::Up));
    }

    #[test]
    fn select_moves_down_and_confirms_with_enter() {
        let (res, out) = run_select(b"\x1b[B\n", &["red", "green", "blue"]);
        assert_eq!(res.unwrap(), 1);
        assert!(out.contains("› red"));
        assert!(out.contains(PROMPT_DIVIDER));
        assert!(out.contains("selected: green"));
    }

    #[test]
    fn select_wraps_up_from_first_to_last() {
        let (res, out) = run_select(b"\x1b[A\r", &["red", "green", "blue"]);
        assert_eq!(res.unwrap(), 2);
        assert!(out.contains("selected: blue"));
    }

    #[test]
    fn select_errors_when_stdin_closes() {
        let (res, _out) = run_select(b"", &["a", "b"]);
        assert!(matches!(res, Err(SetupError::Prompt(_))));
    }

    #[test]
    fn select_ctrl_c_cancels() {
        let (res, out) = run_select(b"\x03", &["a", "b"]);
        assert!(matches!(res, Err(SetupError::Prompt(_))));
        assert!(out.contains("cancelled"));
    }

    #[test]
    fn multi_select_enter_keeps_initial_checks() {
        let (res, out) = run_multi(b"\n", &["a", "b", "c"], &[true, false, true]);
        assert_eq!(res.unwrap(), vec![0, 2]);
        assert!(out.contains("› [x] a"));
        assert!(out.contains("selected: a, c"));
    }

    #[test]
    fn multi_select_space_toggles_and_arrows_move() {
        let input = b" \x1b[B \x1b[B \n";
        let (res, out) = run_multi(input, &["a", "b", "c"], &[false, true, false]);
        assert_eq!(res.unwrap(), vec![0, 2]);
        assert!(out.contains("selected: a, c"));
    }

    #[test]
    fn multi_select_can_confirm_nothing() {
        let (res, out) = run_multi(b" \n", &["a", "b"], &[true, false]);
        assert_eq!(res.unwrap(), Vec::<usize>::new());
        assert!(out.contains("selected: (none)"));
    }

    #[test]
    fn long_menu_moves_a_bounded_viewport() {
        let options: Vec<String> = (1..=20).map(|idx| format!("option-{idx}")).collect();
        let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();
        let mut input = Vec::new();
        for _ in 0..15 {
            input.extend_from_slice(b"\x1b[B");
        }
        input.push(b'\n');
        let (res, out) = run_select(&input, &option_refs);
        assert_eq!(res.unwrap(), 15);
        assert!(out.contains("selected: option-16"));
        assert!(!out.contains("› option-20"));
    }

    #[test]
    fn terminal_line_truncation_counts_wide_characters() {
        let fitted = fit_terminal_line("ab中文cd", 7);
        assert_eq!(fitted, "ab中文…");
        assert_eq!(UnicodeWidthStr::width(fitted.as_str()), 7);
    }

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
    fn masked_secret_ctrl_c_cancels() {
        let mut input = Cursor::new(b"abc\x03".to_vec());
        let mut output = Vec::new();
        let result = read_masked_secret(&mut input, &mut output, "p: ");
        assert!(matches!(result, Err(SetupError::Prompt(_))));
    }
}
