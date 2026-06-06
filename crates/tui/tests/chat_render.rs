//! Real-terminal smoke test for the inline-viewport chat TUI
//! (`crates/tui`).
//!
//! Unlike the picker, the chat UI needs a gateway to render anything, so
//! the `chat_smoke` probe stands up an in-process stub that speaks
//! `aura_channels::wire` and drives the *real* `TuiAdapter` + `WsTransport`
//! against it. This test launches that probe in a tmux pane and asserts on
//! the captured frames: the banner + input box render, a typed message
//! draws its user line and the scripted assistant reply, the live region
//! survives a resize, and Ctrl+C exits the process.
//!
//! Assertions stay structural (substring presence, process liveness)
//! rather than pixel-exact — the inline-viewport resize path has a known,
//! accepted cosmetic ghost frame, so a golden snapshot would be flaky by
//! design. Each test self-skips when tmux is absent.

use std::time::Duration;

use aura_term_harness::{Key, LaunchSpec, TmuxSession, tmux_available};

/// Cargo builds the `chat_smoke` bin (its `test-support` required-feature
/// is on during `cargo test`) and hands us its path — no nested cargo
/// build at runtime.
const SMOKE_BIN: &str = env!("CARGO_BIN_EXE_chat_smoke");
/// Heavier than the picker probe (links the full TUI graph and dials a WS),
/// so allow extra startup slack before declaring a hang.
const WAIT: Duration = Duration::from_secs(15);

/// Launch the chat smoke at a roomy size and wait for the first rendered
/// frame, or return `None` when tmux is unavailable so the caller skips.
fn launched() -> Option<TmuxSession> {
    if !tmux_available() {
        eprintln!("skipping chat_render: tmux not on PATH");
        return None;
    }
    let session =
        TmuxSession::launch(LaunchSpec::new(SMOKE_BIN, 90, 24)).expect("launch chat_smoke");
    session
        .wait_until(WAIT, "chat banner + input box", |c| {
            c.contains("Aura TUI") && c.contains("input")
        })
        .expect("chat UI should render its first frame");
    Some(session)
}

#[test]
fn chat_ui_renders_banner_and_input_box() {
    let Some(session) = launched() else {
        return;
    };
    let frame = session.capture().expect("capture");
    assert!(frame.contains("Aura TUI"), "banner header:\n{frame}");
    assert!(
        frame.contains("session: smoke-session"),
        "banner shows the pinned session:\n{frame}"
    );
    assert!(frame.contains("input"), "input box title:\n{frame}");
}

#[test]
fn user_message_streams_an_assistant_reply() {
    let Some(session) = launched() else {
        return;
    };
    session.send_text("hello there").expect("type message");
    session
        .wait_until(WAIT, "typed input", |c| c.contains("hello there"))
        .expect("input box echoes typing");

    session.send_key(Key::Enter).expect("submit");
    let frame = session
        .wait_until(WAIT, "assistant reply", |c| {
            c.contains("stub-reply for: hello there")
        })
        .expect("scripted reply should render");
    assert!(
        frame.contains("hello there"),
        "user line committed to the transcript:\n{frame}"
    );

    // The transcript commits into the terminal's own scrollback
    // (insert_before), so a scrollback capture must recover it too.
    let history = session
        .capture_with_scrollback(200)
        .expect("scrollback capture");
    assert!(
        history.contains("stub-reply for: hello there"),
        "reply present in scrollback:\n{history}"
    );
}

#[test]
fn live_region_survives_a_resize() {
    let Some(session) = launched() else {
        return;
    };
    session.send_text("hi").expect("type");
    session.send_key(Key::Enter).expect("submit");
    session
        .wait_until(WAIT, "reply before resize", |c| {
            c.contains("stub-reply for: hi")
        })
        .expect("reply rendered");

    session.resize(70, 16).expect("resize");
    // Smoke-level: the inline-viewport resize may leave a known cosmetic
    // ghost above the input, so assert only that the live region rebuilt
    // and the transcript is still readable — not a pixel-exact layout.
    let frame = session
        .wait_stable(WAIT)
        .expect("screen settles after resize");
    assert!(
        frame.contains("input"),
        "input box re-rendered after resize:\n{frame}"
    );
    assert!(
        frame.contains("stub-reply for: hi"),
        "transcript survives the resize:\n{frame}"
    );
}

#[test]
fn ctrl_c_on_empty_prompt_exits() {
    let Some(session) = launched() else {
        return;
    };
    session.send_key(Key::Ctrl('c')).expect("send Ctrl+C");
    session
        .wait_for_exit(WAIT)
        .expect("Ctrl+C on an empty prompt should exit the process");
}
