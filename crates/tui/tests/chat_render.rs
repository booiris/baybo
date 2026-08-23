//! Real-terminal smoke + scenario test for the inline-viewport chat TUI
//! (`crates/tui`).
//!
//! The chat UI needs a gateway to render, so the `chat_smoke` probe stands
//! up an in-process stub that speaks `baybo_channels::wire` and drives the
//! *real* `TuiAdapter` + `WsTransport`. This test launches that probe in a
//! tmux pane and asserts on the captured frames in two complementary
//! styles:
//!
//! - **Golden snapshots** (`tests/snapshots/*.snap`) for the clean, stable
//!   frames (initial banner, a plain reply) — normalized to mask the
//!   version string and the volatile working-indicator timer, so they
//!   catch *unanticipated* visual drift. Regenerate after an intentional
//!   UI change with `UPDATE_CHAT_SNAPSHOT=1 cargo test -p baybo-tui --test
//!   chat_render`.
//! - **Structural assertions** for the dynamic scenarios (tool call,
//!   subagent-as-tool, approval modal, dropped task list), where a golden
//!   would be flaky or where the contract is "this must NOT render".
//!
//! A probe that *dies* mid-scenario fails the test outright. The TUI emits no
//! terminal queries (see `crate::backend::AnchoredBackend` in the library), so
//! nothing racy remains to absorb: a death here means the event loop took an
//! error path, which is exactly the regression this suite should catch.
//!
//! Each test self-skips when tmux is absent so CI without tmux stays green.

use std::path::PathBuf;
use std::time::Duration;

use baybo_term_harness::{HarnessError, Key, LaunchSpec, TmuxSession, tmux_available};
use baybo_tui::smoke_contract::*;
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
/// No-progress (idle) window for `wait_render`: `wait_until_progress` resets it
/// on every real frame change, so it's "how long the screen may sit frozen
/// before we call it hung," not an absolute deadline. Heavier than the picker
/// probe (full TUI graph + a WS dial), so it needs slack on a loaded box.
const WAIT: Duration = Duration::from_secs(15);
/// Fixed pane size so the golden snapshots are deterministic.
const COLS: u16 = 90;
const ROWS: u16 = 24;
/// Launch the probe, wait for the first frame, then run `body`. Self-skips
/// without tmux. Both a probe death and a render mismatch fail the test.
///
/// `body` returning `Ok` is not on its own proof the probe survived: tmux's
/// `remain-on-exit` freezes the last frame in the pane, so a predicate that was
/// already satisfied still matches against a dead probe. The liveness check
/// after `body` is what turns that into a failure. Scenarios that *expect* the
/// probe to exit use [`run_chat_until_exit`].
fn run_chat<F>(name: &str, body: F)
where
    F: Fn(&TmuxSession) -> Result<(), String>,
{
    run_scenario(name, body, ExitExpectation::StaysAlive);
}

/// [`run_chat`] for a scenario whose whole point is that the probe exits.
fn run_chat_until_exit<F>(name: &str, body: F)
where
    F: Fn(&TmuxSession) -> Result<(), String>,
{
    run_scenario(name, body, ExitExpectation::Exits);
}

enum ExitExpectation {
    StaysAlive,
    Exits,
}

fn run_scenario<F>(name: &str, body: F, expectation: ExitExpectation)
where
    F: Fn(&TmuxSession) -> Result<(), String>,
{
    if !tmux_available() {
        eprintln!("skipping chat_render::{name}: tmux not on PATH");
        return;
    }
    // Held for the whole scenario so probes run one at a time — see `SERIAL`.
    let _serial = SERIAL.lock();
    let session =
        TmuxSession::launch(LaunchSpec::new(SMOKE_BIN, COLS, ROWS)).expect("launch chat_smoke");
    let result = wait_render(&session, "chat banner + input box", |c| {
        c.contains("Baybo TUI") && c.contains("input")
    })
    .and_then(|_| body(&session));
    if let Err(e) = result {
        panic!("chat_render::{name}: {e}");
    }
    if matches!(expectation, ExitExpectation::StaysAlive) {
        assert!(
            !session.is_dead(),
            "chat_render::{name}: the probe died before the scenario ended"
        );
    }
}

/// Wait for `pred`, reporting a probe death (`ProcessDied`) and a real render
/// failure (`Timeout` — process alive but output never appeared) alike; both
/// end the scenario.
fn wait_render(
    session: &TmuxSession,
    what: &str,
    pred: impl Fn(&str) -> bool,
) -> Result<String, String> {
    // `normalize` is the progress key: it strips the volatile working-indicator
    // timer (`● cooked for Ns`) and version, so a turn that's slow under load
    // still counts as progressing (real content changing) — only a genuinely
    // stuck render trips the idle window.
    match session.wait_until_progress(WAIT, what, &pred, normalize) {
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
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
fn snapshot_initial_frame() {
    run_chat("snapshot_initial_frame", |s| {
        let frame = settle(s)?;
        assert_snapshot("chat_initial", &normalize(&frame));
        Ok(())
    });
}

#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
fn chat_ui_renders_banner_and_input_box() {
    run_chat("chat_ui_renders_banner_and_input_box", |s| {
        let frame = s.capture().map_err(|e| e.to_string())?;
        assert!(frame.contains("Baybo TUI"), "banner header:\n{frame}");
        assert!(
            frame.contains("session: smoke-session"),
            "banner shows the pinned session:\n{frame}"
        );
        assert!(frame.contains("input"), "input box title:\n{frame}");
        Ok(())
    });
}

#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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

/// Markdown blocks reach a real terminal as rendered layout, not as source.
/// `capture-pane -p` carries no styling, so this pins the *glyphs and layout*
/// markdown produces (bullets, code gutter, table rules) and — the part that
/// matters most — that no markup punctuation is left on screen.
#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
fn markdown_blocks_render_as_layout_not_source() {
    run_chat("markdown_blocks_render_as_layout_not_source", |s| {
        say(s, SAY_MARKDOWN);
        wait_render(s, "markdown answer", |c| c.contains(MARKDOWN_TAIL))?;
        let history = s.capture_with_scrollback(400).map_err(|e| e.to_string())?;

        for (label, needle) in [
            ("heading text", MARKDOWN_HEADING),
            ("emphasised word", MARKDOWN_EMPHASISED),
            ("bullet glyph", "•"),
            ("ordered marker", "1. step one"),
            ("code gutter", "│ fn main() {}"),
            ("code language label", "┌ rust"),
            ("table rule", "├"),
            ("quote gutter", "▌"),
            ("han paragraph", MARKDOWN_TAIL),
        ] {
            assert!(
                history.contains(needle),
                "{label} ({needle:?}) missing from:\n{history}"
            );
        }

        for (label, marker) in [
            ("bold markers", "**"),
            ("fence markers", "```"),
            ("table pipes", "| lang |"),
            ("list dashes", "- first point"),
        ] {
            assert!(
                !history.contains(marker),
                "{label} ({marker:?}) left as literal source in:\n{history}"
            );
        }
        Ok(())
    });
}

/// Answer text held back by the markdown block scanner must reach scrollback
/// *above* a tool block that arrives while it is still buffered. `insert_before`
/// cannot write beneath a committed row, so getting this wrong inverts the turn's
/// reading order permanently.
#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
fn buffered_answer_text_commits_above_a_tool_block() {
    run_chat("buffered_answer_text_commits_above_a_tool_block", |s| {
        say(s, SAY_MARKDOWN_TOOL);
        wait_render(s, "markdown + tool turn", |c| c.contains(MDTOOL_TAIL))?;
        let history = s.capture_with_scrollback(400).map_err(|e| e.to_string())?;

        let held = history
            .find(MDTOOL_HELD)
            .ok_or_else(|| format!("held-back prose missing:\n{history}"))?;
        let tool = history
            .find(TOOL_SUMMARY)
            .ok_or_else(|| format!("tool result missing:\n{history}"))?;
        let tail = history
            .find(MDTOOL_TAIL)
            .ok_or_else(|| format!("post-tool prose missing:\n{history}"))?;
        assert!(
            held < tool,
            "buffered prose landed BELOW its tool block:\n{history}"
        );
        assert!(
            tool < tail,
            "post-tool prose landed above the tool block:\n{history}"
        );
        Ok(())
    });
}

#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
        // transient. A genuine miss still times out → fail-fast. Smoke-level:
        // assert the live region rebuilt + transcript survived, not a
        // pixel-exact layout.
        wait_render(s, "input box + transcript after resize", |c| {
            c.contains("input") && c.contains(&reply)
        })?;
        Ok(())
    });
}

#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
fn ctrl_c_on_empty_prompt_exits() {
    run_chat_until_exit("ctrl_c_on_empty_prompt_exits", |s| {
        s.send_key(Key::Ctrl('c')).expect("send Ctrl+C");
        s.wait_for_exit(WAIT).map(|_| ()).map_err(|e| e.to_string())
    });
}

// ---- scenario tests (dynamic frames) ----

#[test]
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
#[ignore = "tmux render test; flaky under load — run in CI with --ignored"]
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
