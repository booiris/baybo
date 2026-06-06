//! Real-terminal test harness: launch a child process inside a detached
//! tmux pane at a forced size, drive it with keystrokes and resizes, and
//! read back the rendered screen with `capture-pane`.
//!
//! Why tmux and not a raw PTY: tmux interprets escape sequences exactly
//! like a real terminal, so the capture is ground truth for crossterm /
//! ratatui output — short-window cursor clamping, alternate-screen
//! redraws, and SIGWINCH/resize reflow all show up here. A raw PTY would
//! hand back the bytes uninterpreted and hide every one of those bugs.
//!
//! Tests built on this self-skip when tmux is missing (see
//! [`tmux_available`]) so a CI runner without tmux stays green, the same
//! way the workspace's docker/bwrap-backed tests self-skip.
//!
//! The harness never touches global/server tmux options — every option it
//! sets is scoped to the session it created, so it is safe to run inside
//! the developer's own tmux server.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

/// How often [`TmuxSession::wait_until`] and friends re-`capture-pane`
/// while polling for a screen condition. Short enough to feel instant on
/// a settled screen, long enough not to spin the tmux server.
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// Counter feeding unique session names so concurrently-running tests
/// (cargo runs test fns on multiple threads) never collide on a name.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("spawn tmux: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("tmux {cmd} failed (status {status}): {stderr}")]
    Tmux {
        cmd: String,
        status: String,
        stderr: String,
    },
    #[error(
        "timed out after {ms}ms waiting for {what}\n--- last capture ---\n{last}\n--- end capture ---"
    )]
    Timeout {
        ms: u128,
        what: String,
        last: String,
    },
}

pub type Result<T> = std::result::Result<T, HarnessError>;

/// Returns `true` if a `tmux` binary is on `PATH` and runnable. Tests
/// gate on this and skip (rather than fail) when it returns `false`.
pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A keystroke to deliver to the pane — the harness's keyboard vocabulary.
/// Named keys map to tmux key tokens (`Down` -> `Down`, `Ctrl('c')` ->
/// `C-c`); [`Key::Char`] is sent as a single literal character. For typing
/// a whole string use [`TmuxSession::send_text`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Escape,
    Tab,
    Backspace,
    /// A control-modified letter, e.g. `Ctrl('c')` -> `C-c`.
    Ctrl(char),
    /// A single literal character (sent with `send-keys -l`).
    Char(char),
}

impl Key {
    /// The tmux key token for a *named* key, or `None` for the literal
    /// variants that must be sent with `send-keys -l`.
    fn token(self) -> Option<String> {
        Some(match self {
            Key::Up => "Up".into(),
            Key::Down => "Down".into(),
            Key::Left => "Left".into(),
            Key::Right => "Right".into(),
            Key::Enter => "Enter".into(),
            Key::Space => "Space".into(),
            Key::Escape => "Escape".into(),
            Key::Tab => "Tab".into(),
            Key::Backspace => "BSpace".into(),
            Key::Ctrl(c) => format!("C-{c}"),
            Key::Char(_) => return None,
        })
    }
}

/// Everything needed to start a pane. Construct the literal and hand it to
/// [`TmuxSession::launch`]; every field is explicit at the call site.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Program to exec inside the pane (an absolute path is best so tmux's
    /// `PATH`/cwd don't matter).
    pub program: PathBuf,
    /// Argument vector for `program`.
    pub args: Vec<String>,
    /// Forced pane width in columns. The child's `terminal::size()` reads
    /// this because the session is detached (no client overrides it).
    pub width: u16,
    /// Forced pane height in rows.
    pub height: u16,
    /// Extra environment variables exported into the pane.
    pub env: Vec<(String, String)>,
}

impl LaunchSpec {
    /// Minimal spec: a program with no args at `width`x`height`.
    pub fn new(program: impl Into<PathBuf>, width: u16, height: u16) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            width,
            height,
            env: Vec::new(),
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// A live detached tmux session running one child. Dropping it kills the
/// session, so a panicking test never leaks a pane.
pub struct TmuxSession {
    name: String,
}

impl TmuxSession {
    /// Create a detached session at the spec's forced size and start the
    /// child. `remain-on-exit` is enabled on the window (scoped to this
    /// session only) so the child's final frame survives its exit and can
    /// still be captured.
    pub fn launch(spec: LaunchSpec) -> Result<Self> {
        let name = format!(
            "auraterm_{}_{}",
            std::process::id(),
            SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
        );

        let mut args: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.clone(),
            "-x".into(),
            spec.width.to_string(),
            "-y".into(),
            spec.height.to_string(),
        ];
        for (k, v) in &spec.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        // Trailing args are the command's argv — tmux execs it directly
        // (no intermediate shell) when given as separate arguments.
        args.push(spec.program.to_string_lossy().into_owned());
        args.extend(spec.args.iter().cloned());

        run_tmux(&args)?;
        let session = Self { name };

        // Window-scoped, never global: keep the last frame after the
        // child exits. The child is blocked on its first input read at
        // this point, so there is no exit race to lose.
        session.set_window_option("remain-on-exit", "on")?;
        Ok(session)
    }

    /// Deliver a batch of named/literal keystrokes in order.
    pub fn send_keys(&self, keys: &[Key]) -> Result<()> {
        for key in keys {
            match key.token() {
                Some(token) => {
                    run_tmux(&["send-keys".into(), "-t".into(), self.name.clone(), token])?;
                }
                None => {
                    if let Key::Char(c) = key {
                        self.send_text(&c.to_string())?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Convenience for a single key.
    pub fn send_key(&self, key: Key) -> Result<()> {
        self.send_keys(&[key])
    }

    /// Type a literal string into the pane (no key-name interpretation).
    pub fn send_text(&self, text: &str) -> Result<()> {
        run_tmux(&[
            "send-keys".into(),
            "-t".into(),
            self.name.clone(),
            "-l".into(),
            // `--` stops option parsing so text starting with `-` is
            // still treated as literal keys.
            "--".into(),
            text.into(),
        ])
        .map(|_| ())
    }

    /// Resize the pane, exercising the child's SIGWINCH / resize path.
    /// `resize-window` flips the session to manual sizing, so the new
    /// dimensions stick for subsequent captures.
    pub fn resize(&self, width: u16, height: u16) -> Result<()> {
        run_tmux(&[
            "resize-window".into(),
            "-t".into(),
            self.name.clone(),
            "-x".into(),
            width.to_string(),
            "-y".into(),
            height.to_string(),
        ])
        .map(|_| ())
    }

    /// Capture the visible pane as plain text (trailing spaces trimmed by
    /// tmux). For a child in the alternate screen this is the alt-screen
    /// content; otherwise it is the main screen.
    pub fn capture(&self) -> Result<String> {
        run_tmux(&[
            "capture-pane".into(),
            "-p".into(),
            "-t".into(),
            self.name.clone(),
        ])
    }

    /// Capture the visible pane plus `extra` lines of scrollback above it.
    /// Reveals frames that scrolled off the top (stacking / garble that a
    /// single visible capture would miss).
    pub fn capture_with_scrollback(&self, extra: u32) -> Result<String> {
        run_tmux(&[
            "capture-pane".into(),
            "-p".into(),
            "-t".into(),
            self.name.clone(),
            "-S".into(),
            format!("-{extra}"),
        ])
    }

    /// `true` once the child process has exited (the pane is in its
    /// `remain-on-exit` dead state).
    pub fn is_dead(&self) -> bool {
        run_tmux(&[
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            self.name.clone(),
            "-F".into(),
            "#{pane_dead}".into(),
        ])
        .map(|s| s.trim() == "1")
        .unwrap_or(true)
    }

    /// Poll the pane until `pred` holds against a fresh capture, then
    /// return that capture. Errors with [`HarnessError::Timeout`] (last
    /// capture attached) if `timeout` elapses first.
    pub fn wait_until<F>(&self, timeout: Duration, what: &str, pred: F) -> Result<String>
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        loop {
            last = self.capture().unwrap_or(last);
            if pred(&last) {
                return Ok(last);
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout {
                    ms: timeout.as_millis(),
                    what: what.to_string(),
                    last,
                });
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Poll until two consecutive captures (one `POLL_INTERVAL` apart) are
    /// identical, i.e. the screen has stopped changing, then return it.
    /// Use after a resize/keystroke burst before asserting, so a redraw
    /// in flight isn't sampled mid-frame.
    pub fn wait_stable(&self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut prev = self.capture()?;
        loop {
            std::thread::sleep(POLL_INTERVAL);
            let cur = self.capture()?;
            if cur == prev {
                return Ok(cur);
            }
            prev = cur;
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout {
                    ms: timeout.as_millis(),
                    what: "screen to settle".to_string(),
                    last: prev,
                });
            }
        }
    }

    /// Poll until the child exits. Returns the final captured frame —
    /// which, because the pane is kept alive with `remain-on-exit`,
    /// includes tmux's "Pane is dead" footer and has scrolled up one row,
    /// so treat it as exit *evidence*, not a pristine last frame. To
    /// assert on a program's final output, keep the program alive (block
    /// it) and [`capture`](Self::capture) while it still runs.
    pub fn wait_for_exit(&self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_dead() {
                return self.capture();
            }
            if Instant::now() >= deadline {
                return Err(HarnessError::Timeout {
                    ms: timeout.as_millis(),
                    what: "child to exit".to_string(),
                    last: self.capture().unwrap_or_default(),
                });
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn set_window_option(&self, option: &str, value: &str) -> Result<()> {
        run_tmux(&[
            "set-option".into(),
            "-w".into(),
            "-t".into(),
            self.name.clone(),
            option.into(),
            value.into(),
        ])
        .map(|_| ())
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

fn run_tmux(args: &[String]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(HarnessError::Spawn)?;
    if !output.status.success() {
        return Err(HarnessError::Tmux {
            cmd: args.first().cloned().unwrap_or_default(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_tmux_tokens() {
        assert_eq!(Key::Down.token().as_deref(), Some("Down"));
        assert_eq!(Key::Enter.token().as_deref(), Some("Enter"));
        assert_eq!(Key::Backspace.token().as_deref(), Some("BSpace"));
        assert_eq!(Key::Ctrl('c').token().as_deref(), Some("C-c"));
        assert_eq!(Key::Ctrl('d').token().as_deref(), Some("C-d"));
    }

    #[test]
    fn literal_char_has_no_named_token() {
        assert_eq!(Key::Char('j').token(), None);
    }

    #[test]
    fn launch_spec_builder_collects_args_and_env() {
        let spec = LaunchSpec::new("/bin/echo", 80, 24)
            .arg("hello")
            .args(["a", "b"])
            .env("FOO", "bar");
        assert_eq!(spec.args, vec!["hello", "a", "b"]);
        assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
        assert_eq!((spec.width, spec.height), (80, 24));
    }

    // Real-terminal smoke test: only runs where tmux is installed. The
    // program prints then blocks (the capture-while-alive pattern every
    // probe follows), so the frame is pristine when we read it.
    #[test]
    fn captures_a_program_at_forced_size() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }
        let spec = LaunchSpec::new("/bin/sh", 40, 10)
            .arg("-c")
            .arg("printf 'HARNESS_OK\\n'; exec sleep 30");
        let session = TmuxSession::launch(spec).expect("launch");
        let screen = session
            .wait_until(Duration::from_secs(5), "printf output", |s| {
                s.contains("HARNESS_OK")
            })
            .expect("capture");
        assert!(screen.contains("HARNESS_OK"), "screen was:\n{screen}");
    }
}
