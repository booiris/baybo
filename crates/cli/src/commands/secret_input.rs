//! Shared utilities for reading masked secrets from a TTY.
//!
//! The naive approach (`stdin.read_line` with `*` echoed per byte) leaks
//! the literal characters because the kernel's tty driver echoes them
//! before user code can intercept. Every prompt for a credential goes
//! through these helpers so termios `ECHO` + `ICANON` are disabled
//! exactly while bytes are read; the `RawModeGuard` `Drop` impl restores
//! the prior settings even on panic so the user's terminal isn't left
//! in raw mode.

use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;

use crate::error::{CliError, Result};

pub struct RawModeGuard {
    fd: i32,
    original: libc::termios,
}

impl RawModeGuard {
    pub fn new(fd: i32) -> Result<Self> {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
        if rc != 0 {
            return Err(CliError::Io(format!(
                "failed to read terminal mode: {}",
                io::Error::last_os_error()
            )));
        }

        let original = unsafe { termios.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        if rc != 0 {
            return Err(CliError::Io(format!(
                "failed to enable raw terminal mode: {}",
                io::Error::last_os_error()
            )));
        }

        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

/// Read bytes from `reader` and write `*` per character to `writer`.
/// Honours backspace (`0x08` / `0x7f`) by popping the last buffered byte
/// and erasing one star. Terminates on `\n` / `\r`. Returns the entered
/// string. The caller owns the raw-mode lifetime — wrap the call in a
/// [`RawModeGuard`] so terminal `ECHO` is disabled while bytes flow.
pub fn read_masked_secret<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer.write_all(label.as_bytes())?;
    writer.flush()?;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .map_err(|e| CliError::Config(format!("failed to read masked input: {e}")))?;
        if n == 0 {
            writer.write_all(b"\n")?;
            writer.flush()?;
            return Err(CliError::Io(
                "stdin closed while reading masked input".into(),
            ));
        }

        match byte[0] {
            b'\n' | b'\r' => {
                writer.write_all(b"\n")?;
                writer.flush()?;
                break;
            }
            0x08 | 0x7f => {
                if buf.pop().is_some() {
                    writer.write_all(b"\x08 \x08")?;
                    writer.flush()?;
                }
            }
            b => {
                buf.push(b);
                writer.write_all(b"*")?;
                writer.flush()?;
            }
        }
    }

    String::from_utf8(buf).map_err(|e| CliError::Config(format!("input must be valid utf-8: {e}")))
}

/// One-shot helper for command handlers: verifies stdin/stderr are TTYs,
/// flips the tty into no-echo + non-canonical mode, reads a masked
/// secret, and restores tty mode (including on panic).
pub fn read_masked_password(prompt: &str) -> Result<String> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive secret entry requires a terminal (stdin and stderr both)".into(),
        ));
    }
    let tty_fd = stdin.as_raw_fd();
    let _guard = RawModeGuard::new(tty_fd)?;
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    read_masked_secret(&mut reader, &mut writer, prompt)
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
        // Every input char must be replaced with `*`; literal chars must
        // not appear anywhere in what hits the writer.
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains('h'));
        assert!(!rendered.contains('2'));
        assert_eq!(rendered.matches('*').count(), 7);
    }
}
