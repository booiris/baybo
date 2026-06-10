//! Real-terminal smoke + scenario test for the inline-viewport chat TUI
//! (`crates/tui`).
//!
//! The chat UI needs a gateway to render, so the `chat_smoke` probe stands
//! up an in-process stub that speaks `aura_channels::wire` and drives the
//! *real* `TuiAdapter` + `WsTransport`. This test launches that probe in a
//! tmux pane and asserts on the captured frames in two complementary
//! styles:
//!
//! - **Golden snapshots** (`tests/snapshots/*.snap`) for the clean, stable
//!   frames (initial banner, a plain reply) — normalized to mask the
//!   version string and the volatile working-indicator timer, so they
//!   catch *unanticipated* visual drift. Regenerate after an intentional
//!   UI change with `UPDATE_CHAT_SNAPSHOT=1 cargo test -p aura-tui --test
//!   chat_render`.
//! - **Structural assertions** for the dynamic scenarios (tool call,
//!   subagent-as-tool, approval modal, dropped task list), where a golden
//!   would be flaky or where the contract is "this must NOT render".
//!
//! ## The retry wrapper
//!
//! Driving the chat UI exposes a known, accepted race ([the resize-reflow
//! notes]): a non-keyboard viewport rebuild queries cursor position
//! (`ESC[6n`), and because dropping crossterm's `EventStream` doesn't
//! synchronously stop its reader thread, a lingering blocking `stdin` read
//! can steal the reply — the query then times out and the TUI process
//! exits mid-turn. That race is orthogonal to what these tests check
//! (rendering correctness), so [`run_chat`] retries a scenario on a fresh
//! process when it *dies*, while a genuine render mismatch (process alive,
//! output wrong/absent) still fails fast.
//!
//! Each test self-skips when tmux is absent so CI without tmux stays green.

use std::path::PathBuf;
use std::time::Duration;

use aura_term_harness::{HarnessError, Key, LaunchSpec, TmuxSession, tmux_available};
use aura_tui::smoke_contract::*;
use parking_lot::Mutex;

/// Serializes the scenarios below. Each spins a full TUI + WS probe in a tmux
/// pane; libtest runs the 10 test fns on its thread pool, and 10 heavy probes
/// at once starved each other's renders enough to blow the 15s capture timeout
/// (a flaky CI failure). They're I/O/timing-bound, so running them one at a
/// time costs little wall-clock and makes each render deterministic. `parking_lot`
/// doesn't poison, so a panicking (failing) scenario still frees it for the next.
static SERIAL: Mutex<()> = Mutex::new(());

/// Cargo builds the `chat_smoke` bin (its `test-support` required-feature
/// is on during `cargo test`) and hands us its path — no nested cargo
/// build at runtime.
const SMOKE_BIN: &str = env!("CARGO_BIN_EXE_chat_smoke");
/// Heavier than the picker probe (links the full TUI graph and dials a WS),
/// so allow extra startup slack before declaring a hang.
const WAIT: Duration = Duration::from_secs(15);
/// Fixed pane size so the golden snapshots are deterministic.
const COLS: u16 = 90;
const ROWS: u16 = 24;
/// Fresh-process attempts before giving up — see the module note on the
/// cursor-position race. Each death is *fast* (the probe exits early the moment
/// the race steals the `ESC[6n` reply), so retries cost little wall-clock; 8
/// keeps the all-attempts-die odds negligible even under heavy CI load (4 lost
/// every attempt on a contended runner once). Genuine render failures don't
/// consume these (they panic immediately).
const ATTEMPTS: usize = 8;

/// Launch the probe, wait for the first frame, run `body`, and retry on a
/// process death from the known cursor-position race. Self-skips without
/// tmux. A render mismatch inside `body` panics and is *not* retried.
fn run_chat<F>(name: &str, body: F)
where
    F: Fn(&TmuxSession) -> Result<(), String>,
{
    if !tmux_available() {
        eprintln!("skipping chat_render::{name}: tmux not on PATH");
        return;
    }
    // Held for the whole scenario (all retry attempts) so probes run one at a
    // time — see `SERIAL`.
    let _serial = SERIAL.lock();
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        let session =
            TmuxSession::launch(LaunchSpec::new(SMOKE_BIN, COLS, ROWS)).expect("launch chat_smoke");
        let result = wait_render(&session, "chat banner + input box", |c| {
            c.contains("Aura TUI") && c.contains("input")
        })
        .and_then(|_| body(&session));
        match result {
            Ok(()) => return,
            Err(e) => {
                last = e;
                eprintln!("chat_render::{name}: attempt {attempt}/{ATTEMPTS} died: {last}");
            }
        }
    }
    panic!("chat_render::{name}: failed after {ATTEMPTS} attempts: {last}");
}

/// Wait for `pred`. The harness tells death (`ProcessDied`, the known race
/// → `Err` so the caller retries) apart from a real render failure
/// (`Timeout` — process alive but output never appeared → panic, fail fast).
fn wait_render(
    session: &TmuxSession,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> Result<String, String> {
    match session.wait_until(WAIT, what, &pred) {
        Ok(frame) => Ok(frame),
        Err(HarnessError::ProcessDied { .. }) => Err(format!("process died waiting for {what}")),
        Err(e) => panic!("render failure waiting for {what}: {e}"),
    }
}

/// `wait_stable` with the same death-vs-real-failure split as [`wait_render`].
fn settle(session: &TmuxSession) -> Result<String, String> {
    match session.wait_stable(WAIT) {
        Ok(frame) => Ok(frame),
        Err(HarnessError::ProcessDied { .. }) => {
            Err("process died while the screen settled".to_string())
        }
        Err(e) => panic!("screen never settled: {e}"),
    }
}

/// Type a line and submit it.
fn say(session: &TmuxSession, text: &str) {
    session.send_text(text).expect("type message");
    session.send_key(Key::Enter).expect("submit");
}

// ---- golden snapshot plumbing ----

/// Mask the two volatile bits so a captured frame is byte-stable: the
/// banner version (`v0.1.0` -> `vX.Y.Z`) and the working-indicator elapsed
/// timer line (`● cooked for 0s`), then trim trailing blank rows.
fn normalize(capture: &str) -> String {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut lines: Vec<String> = capture
        .lines()
        .filter(|l| !l.contains("cooked for"))
        .map(|l| l.replace(&version, "vX.Y.Z").trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("{name}.snap"))
}

/// Compare `normalized` against the committed `.snap`, or rewrite it when
/// `UPDATE_CHAT_SNAPSHOT=1` is set.
fn assert_snapshot(name: &str, normalized: &str) {
    let path = snapshot_path(name);
    if std::env::var_os("UPDATE_CHAT_SNAPSHOT").is_some() {
        std::fs::create_dir_all(path.parent().expect("snapshot dir")).expect("create snapshot dir");
        std::fs::write(&path, normalized).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing snapshot `{name}`; regenerate with UPDATE_CHAT_SNAPSHOT=1")
    });
    assert_eq!(
        normalized,
        expected.trim_end_matches('\n'),
        "snapshot `{name}` mismatch — if this UI change is intentional, regenerate with \
         UPDATE_CHAT_SNAPSHOT=1\n--- got ---\n{normalized}\n--- end ---"
    );
}

// ---- golden snapshot tests (clean, stable frames) ----

#[test]
fn snapshot_initial_frame() {
    run_chat("snapshot_initial_frame", |s| {
        let frame = settle(s)?;
        assert_snapshot("chat_initial", &normalize(&frame));
        Ok(())
    });
}

#[test]
fn snapshot_echo_reply_frame() {
    run_chat("snapshot_echo_reply_frame", |s| {
        let reply = format!("{REPLY_PREFIX}hello there");
        say(s, "hello there");
        // Require the input box back too, not just the reply: the inline
        // viewport redraws it a tick after the reply scrolls in, and a capture
        // in that gap settles on a transient frame missing the bottom panel
        // (an intermittent CI snapshot mismatch). `run_chat` gates the banner
        // on the same `input` marker.
        wait_render(s, "reply + input box", |c| {
            c.contains(&reply) && c.contains("input")
        })?;
        let frame = settle(s)?;
        assert_snapshot("chat_echo_reply", &normalize(&frame));
        Ok(())
    });
}

// ---- structural smoke tests ----

#[test]
fn chat_ui_renders_banner_and_input_box() {
    run_chat("chat_ui_renders_banner_and_input_box", |s| {
        let frame = s.capture().map_err(|e| e.to_string())?;
        assert!(frame.contains("Aura TUI"), "banner header:\n{frame}");
        assert!(
            frame.contains("session: smoke-session"),
            "banner shows the pinned session:\n{frame}"
        );
        assert!(frame.contains("input"), "input box title:\n{frame}");
        Ok(())
    });
}

#[test]
fn user_message_streams_an_assistant_reply() {
    run_chat("user_message_streams_an_assistant_reply", |s| {
        let reply = format!("{REPLY_PREFIX}hello there");
        say(s, "hello there");
        let frame = wait_render(s, "assistant reply", |c| c.contains(&reply))?;
        assert!(
            frame.contains("hello there"),
            "user line committed to the transcript:\n{frame}"
        );
        let history = s.capture_with_scrollback(200).map_err(|e| e.to_string())?;
        assert!(
            history.contains(&reply),
            "reply present in scrollback:\n{history}"
        );
        Ok(())
    });
}

#[test]
fn live_region_survives_a_resize() {
    run_chat("live_region_survives_a_resize", |s| {
        let reply = format!("{REPLY_PREFIX}hi");
        say(s, "hi");
        wait_render(s, "reply before resize", |c| c.contains(&reply))?;

        s.resize(70, 16).expect("resize");
        // Poll until the post-resize reflow shows BOTH the rebuilt input box and
        // the surviving transcript — rather than snapshotting one `settle()` frame
        // that can be sampled mid-reflow. The inline-viewport resize momentarily
        // drops the input box (a known cosmetic ghost), so a single stable capture
        // could land on a frame without it; `wait_render` keeps polling past that
        // transient. A genuine miss still times out → fail-fast; a process death
        // (the cursor-position race) is retried by `run_chat`. Smoke-level: assert
        // the live region rebuilt + transcript survived, not a pixel-exact layout.
        wait_render(s, "input box + transcript after resize", |c| {
            c.contains("input") && c.contains(&reply)
        })?;
        Ok(())
    });
}

#[test]
fn ctrl_c_on_empty_prompt_exits() {
    run_chat("ctrl_c_on_empty_prompt_exits", |s| {
        s.send_key(Key::Ctrl('c')).expect("send Ctrl+C");
        s.wait_for_exit(WAIT).map(|_| ()).map_err(|e| e.to_string())
    });
}

// ---- scenario tests (dynamic frames) ----

#[test]
fn tool_call_renders_a_tool_line() {
    run_chat("tool_call_renders_a_tool_line", |s| {
        say(s, SAY_TOOL);
        let frame = wait_render(s, "tool reply", |c| c.contains(TOOL_REPLY))?;
        assert!(
            frame.contains(&format!("{TOOL_NAME}({TOOL_LABEL})")),
            "tool call line `Read(src/lib.rs)`:\n{frame}"
        );
        assert!(
            frame.contains(TOOL_SUMMARY),
            "tool result summary:\n{frame}"
        );
        Ok(())
    });
}

#[test]
fn subagent_spawn_renders_as_a_task_tool() {
    run_chat("subagent_spawn_renders_as_a_task_tool", |s| {
        say(s, SAY_SUBAGENT);
        let frame = wait_render(s, "subagent reply", |c| c.contains(SUBAGENT_REPLY))?;
        assert!(
            frame.contains(&format!("{SUBAGENT_TOOL}({SUBAGENT_LABEL})")),
            "subagent surfaces as a Task tool line:\n{frame}"
        );
        assert!(
            frame.contains(SUBAGENT_SUMMARY),
            "subagent summary:\n{frame}"
        );
        Ok(())
    });
}

#[test]
fn tool_approval_modal_renders_and_resolves() {
    run_chat("tool_approval_modal_renders_and_resolves", |s| {
        say(s, SAY_APPROVAL);
        let modal = wait_render(s, "approval modal", |c| c.contains("wants to run"))?;
        assert!(
            modal.contains(APPROVAL_TOOL),
            "modal names the tool:\n{modal}"
        );
        assert!(
            modal.contains(APPROVAL_COMMAND),
            "modal shows the command:\n{modal}"
        );
        assert!(
            modal.contains("[a] Approve") && modal.contains("[d] Deny"),
            "modal offers approve/deny:\n{modal}"
        );

        // Approve it; the TUI sends ResolveApproval, the stub replies.
        s.send_key(Key::Char('a')).expect("approve");
        let resolved = wait_render(s, "approval resolved", |c| c.contains(APPROVAL_REPLY))?;
        assert!(
            resolved.contains(&format!("approved: {APPROVAL_TOOL}")),
            "resolved line shows the approved tool:\n{resolved}"
        );
        Ok(())
    });
}

#[test]
fn task_list_is_not_rendered_in_the_tui() {
    run_chat("task_list_is_not_rendered_in_the_tui", |s| {
        say(s, SAY_TASK);
        // The reply lands but the TaskList frame is dropped (web-dashboard-only).
        let frame = wait_render(s, "task reply", |c| c.contains(TASK_REPLY))?;
        assert!(
            !frame.contains(TASK_SUBJECT),
            "TaskList must not render in the TUI, but the subject appeared:\n{frame}"
        );
        Ok(())
    });
}
