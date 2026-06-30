//! Real-terminal `Prompter` implementation plus the line-oriented
//! building blocks (`prompt_line`, the numbered single/multi pickers, and
//! the masked-secret reader) it's built from. Every prompt prints its
//! menu and reads one full line from stdin — no alternate screen, no
//! raw-mode repaint — so the whole wizard stays in normal terminal
//! scrollback and behaves like ordinary line input. Masked password entry
//! is the only place that briefly toggles termios echo. Kept private to
//! the crate so the only public surface is the [`crate::Prompter`] trait
//! and [`TtyPrompter`].

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;

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
        return Err(SetupError::Prompt(
            "stdin closed while reading interactive input".into(),
        ));
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
// Numbered line pickers. The menu is printed once; the operator types the
// number(s) of their choice and presses enter, exactly like answering a
// `text`/`confirm` prompt. Invalid input re-prints just the input line
// (never the whole menu) so the scrollback stays readable.
// ---------------------------------------------------------------------------

pub(crate) fn select_one(label: &str, options: &[&str]) -> Result<usize> {
    if options.is_empty() {
        return Err(SetupError::Prompt("no options to pick from".into()));
    }
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    select_one_from(&mut reader, &mut writer, label, options)
}

/// Single-choice core split out so it can be unit-tested over in-memory
/// readers/writers. Prints the label, a 1-based numbered menu, then loops
/// reading a line until it parses to an in-range option number.
fn select_one_from<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    options: &[&str],
) -> Result<usize> {
    emit(writer, label)?;
    for (i, opt) in options.iter().enumerate() {
        emit(writer, &format!("  {}) {opt}", i + 1))?;
    }
    let prompt = format!("Enter choice [1-{}]: ", options.len());
    loop {
        let line = prompt_line(reader, writer, &prompt)?;
        match parse_choice(&line, options.len()) {
            Some(idx) => {
                emit(writer, &format!("  selected: {}", options[idx]))?;
                return Ok(idx);
            }
            None => emit(
                writer,
                &format!("  please enter a number between 1 and {}", options.len()),
            )?,
        }
    }
}

/// Parse a 1-based menu choice into a 0-based index, or `None` when the
/// input is empty, non-numeric, or out of range.
fn parse_choice(input: &str, len: usize) -> Option<usize> {
    let n: usize = input.trim().parse().ok()?;
    (1..=len).contains(&n).then(|| n - 1)
}

pub(crate) fn select_many(label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>> {
    if options.is_empty() {
        return Err(SetupError::Prompt("no options to pick from".into()));
    }
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(SetupError::NotATerminal);
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    select_many_from(&mut reader, &mut writer, label, options, initial)
}

/// Multi-choice core split out for unit testing. Prints the label and a
/// numbered menu with each row's pre-checked state, then reads a
/// comma/space-separated list of numbers. An empty line keeps the
/// pre-checked default; a lone `0` selects nothing. Returns the chosen
/// indices in ascending order.
fn select_many_from<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
    options: &[&str],
    initial: &[bool],
) -> Result<Vec<usize>> {
    let checked = |i: usize| initial.get(i).copied().unwrap_or(false);
    emit(writer, label)?;
    for (i, opt) in options.iter().enumerate() {
        let mark = if checked(i) { "[x]" } else { "[ ]" };
        emit(writer, &format!("  {}) {mark} {opt}", i + 1))?;
    }
    let prompt = format!(
        "Enter choices (comma-separated), Enter to keep [{}], 0 for none: ",
        default_summary(options.len(), initial)
    );
    loop {
        let line = prompt_line(reader, writer, &prompt)?;
        match parse_multi(&line, options.len(), initial) {
            Some(picks) => {
                emit(
                    writer,
                    &format!("  selected: {}", summarize(options, &picks)),
                )?;
                return Ok(picks);
            }
            None => emit(
                writer,
                &format!(
                    "  please enter numbers between 1 and {} (comma-separated, or 0 for none)",
                    options.len()
                ),
            )?,
        }
    }
}

/// Parse a comma/space-separated list of 1-based option numbers into
/// sorted, de-duplicated 0-based indices. Empty input falls back to the
/// `initial` checked set; a lone `0` or `none` selects nothing. Returns
/// `None` on any unparseable or out-of-range token so the caller
/// re-prompts.
fn parse_multi(input: &str, len: usize, initial: &[bool]) -> Option<Vec<usize>> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Some(
            (0..len)
                .filter(|&i| initial.get(i).copied().unwrap_or(false))
                .collect(),
        );
    }
    if trimmed == "0" || trimmed.eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let mut picks: Vec<usize> = Vec::new();
    for tok in trimmed.split(|c: char| c == ',' || c.is_whitespace()) {
        if tok.is_empty() {
            continue;
        }
        let n: usize = tok.parse().ok()?;
        if !(1..=len).contains(&n) {
            return None;
        }
        let idx = n - 1;
        if !picks.contains(&idx) {
            picks.push(idx);
        }
    }
    picks.sort_unstable();
    Some(picks)
}

/// `1, 2` for the pre-checked rows, or `none` when nothing is checked —
/// shown as the Enter-default in the multi-select prompt.
fn default_summary(len: usize, initial: &[bool]) -> String {
    let nums: Vec<String> = (0..len)
        .filter(|&i| initial.get(i).copied().unwrap_or(false))
        .map(|i| (i + 1).to_string())
        .collect();
    if nums.is_empty() {
        "none".to_string()
    } else {
        nums.join(", ")
    }
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

    fn run_select(input: &str, options: &[&str]) -> (Result<usize>, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let res = select_one_from(&mut reader, &mut out, "Pick one:", options);
        (res, String::from_utf8_lossy(&out).into_owned())
    }

    fn run_multi(input: &str, options: &[&str], initial: &[bool]) -> (Result<Vec<usize>>, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        let res = select_many_from(&mut reader, &mut out, "Pick many:", options, initial);
        (res, String::from_utf8_lossy(&out).into_owned())
    }

    #[test]
    fn parse_choice_maps_one_based_to_zero_based() {
        assert_eq!(parse_choice("1", 3), Some(0));
        assert_eq!(parse_choice(" 3 ", 3), Some(2));
        assert_eq!(parse_choice("0", 3), None);
        assert_eq!(parse_choice("4", 3), None);
        assert_eq!(parse_choice("", 3), None);
        assert_eq!(parse_choice("x", 3), None);
    }

    #[test]
    fn select_picks_the_numbered_option() {
        let (res, out) = run_select("2\n", &["red", "green", "blue"]);
        assert_eq!(res.unwrap(), 1);
        // Menu is numbered 1-based and the choice is echoed back.
        assert!(out.contains("1) red"));
        assert!(out.contains("selected: green"));
    }

    #[test]
    fn select_reprompts_until_a_valid_number() {
        let (res, out) = run_select("9\nx\n1\n", &["red", "green"]);
        assert_eq!(res.unwrap(), 0);
        assert!(out.contains("please enter a number between 1 and 2"));
        assert!(out.contains("selected: red"));
    }

    #[test]
    fn select_errors_when_stdin_closes() {
        let (res, _out) = run_select("", &["a", "b"]);
        assert!(matches!(res, Err(SetupError::Prompt(_))));
    }

    #[test]
    fn parse_multi_empty_keeps_initial() {
        assert_eq!(parse_multi("", 3, &[true, false, true]), Some(vec![0, 2]));
    }

    #[test]
    fn parse_multi_explicit_list_replaces_initial() {
        assert_eq!(parse_multi("2", 3, &[true, false, true]), Some(vec![1]));
        assert_eq!(
            parse_multi("1, 3", 3, &[false, false, false]),
            Some(vec![0, 2])
        );
        assert_eq!(parse_multi("3 1", 3, &[]), Some(vec![0, 2]));
        assert_eq!(parse_multi("2,2", 3, &[]), Some(vec![1]));
    }

    #[test]
    fn parse_multi_zero_or_none_selects_nothing() {
        assert_eq!(parse_multi("0", 3, &[true, true, true]), Some(vec![]));
        assert_eq!(parse_multi("none", 3, &[true, true, true]), Some(vec![]));
    }

    #[test]
    fn parse_multi_rejects_out_of_range_or_garbage() {
        assert_eq!(parse_multi("4", 3, &[]), None);
        assert_eq!(parse_multi("1,x", 3, &[]), None);
        assert_eq!(parse_multi("0,1", 3, &[]), None); // 0 only means "none" alone
    }

    #[test]
    fn multi_select_reprompts_then_confirms() {
        let (res, out) = run_multi("5\n1,3\n", &["a", "b", "c"], &[false, true, false]);
        assert_eq!(res.unwrap(), vec![0, 2]);
        assert!(out.contains("2) [x] b"), "initial check rendered:\n{out}");
        assert!(out.contains("please enter numbers between 1 and 3"));
        assert!(out.contains("selected: a, c"));
    }

    #[test]
    fn multi_select_empty_keeps_default_and_echoes_none_when_empty() {
        let (res, out) = run_multi("0\n", &["a", "b"], &[true, false]);
        assert_eq!(res.unwrap(), Vec::<usize>::new());
        assert!(out.contains("Enter to keep [1]"), "default summary:\n{out}");
        assert!(out.contains("selected: (none)"));
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
}
