//! Real-terminal rendering tests for the setup picker
//! (`crates/setup/src/tty.rs`).
//!
//! The picker draws raw crossterm escape sequences into the alternate
//! screen and windows/​re-anchors itself on resize — behaviour that unit
//! tests over [`Viewport`] math can't see. These drive the actual binary
//! inside a tmux pane (via `aura-term-harness`) and assert on the real
//! captured frames, the same ground truth that ad-hoc probes used to catch
//! the short-window clamp garble and the resize regression.
//!
//! Each test self-skips when tmux is absent, so CI without tmux stays
//! green (mirroring the repo's docker/bwrap-backed tests).

use std::time::Duration;

use aura_term_harness::{Key, LaunchSpec, TmuxSession, tmux_available};

/// Cargo builds the `picker_probe` bin (its `test-support` required-feature
/// is on during `cargo test`) and hands us its path here — no nested cargo
/// build, so launch is instant and lock-free.
const PROBE_BIN: &str = env!("CARGO_BIN_EXE_picker_probe");
/// Generous ceiling: real renders settle in tens of ms; this only trips
/// on a genuine hang.
const WAIT: Duration = Duration::from_secs(10);

/// Launch the probe at `width`x`height` with `args`, or return `None` when
/// tmux is unavailable so the caller can skip.
fn session(width: u16, height: u16, args: &[&str]) -> Option<TmuxSession> {
    if !tmux_available() {
        eprintln!("skipping picker_render: tmux not on PATH");
        return None;
    }
    let spec = LaunchSpec::new(PROBE_BIN, width, height).args(args.iter().copied());
    Some(TmuxSession::launch(spec).expect("launch picker_probe"))
}

#[test]
fn select_renders_label_options_and_cursor() {
    let Some(s) = session(
        80,
        24,
        &["select", "Choose a color", "red", "green", "blue"],
    ) else {
        return;
    };
    let frame = s
        .wait_until(WAIT, "initial picker frame", |c| {
            c.contains("Choose a color") && c.contains("blue")
        })
        .expect("picker should render");
    assert!(
        frame.contains("> red"),
        "cursor should mark the first option:\n{frame}"
    );
    assert!(frame.contains("green") && frame.contains("blue"));
    assert!(
        !frame.contains("> green") && !frame.contains("> blue"),
        "exactly one option should carry the cursor:\n{frame}"
    );
}

#[test]
fn select_arrow_keys_move_and_wrap_cursor() {
    let Some(s) = session(80, 24, &["select", "Pick", "red", "green", "blue"]) else {
        return;
    };
    s.wait_until(WAIT, "initial", |c| c.contains("> red"))
        .expect("initial frame");

    s.send_key(Key::Down).expect("send Down");
    let frame = s
        .wait_until(WAIT, "cursor on green", |c| c.contains("> green"))
        .expect("cursor moved down");
    assert!(!frame.contains("> red"), "cursor left red:\n{frame}");

    // green -> red -> (wrap) blue
    s.send_keys(&[Key::Up, Key::Up]).expect("send Up Up");
    let frame = s
        .wait_until(WAIT, "cursor wraps to blue", |c| c.contains("> blue"))
        .expect("cursor wrapped");
    assert!(
        !frame.contains("> red") && !frame.contains("> green"),
        "only blue carries the cursor after wrap:\n{frame}"
    );

    // The picker also takes vim-style j/k; `j` from blue wraps to red.
    s.send_key(Key::Char('j')).expect("send j");
    s.wait_until(WAIT, "j wraps to red", |c| c.contains("> red"))
        .expect("j moves the cursor");
    s.send_key(Key::Char('k')).expect("send k");
    let frame = s
        .wait_until(WAIT, "k wraps back to blue", |c| c.contains("> blue"))
        .expect("k moves the cursor");
    assert!(!frame.contains("> red"), "k left red:\n{frame}");
}

#[test]
fn select_enter_echoes_selection_on_main_screen() {
    let Some(s) = session(80, 24, &["select", "Pick a fruit", "apple", "pear"]) else {
        return;
    };
    s.wait_until(WAIT, "initial", |c| c.contains("> apple"))
        .expect("initial frame");
    s.send_keys(&[Key::Down, Key::Enter])
        .expect("send Down Enter");
    let frame = s
        .wait_until(WAIT, "selection echo", |c| c.contains("selected: pear"))
        .expect("outcome echoed");
    assert!(
        frame.contains("Pick a fruit"),
        "label echoed on the restored main screen:\n{frame}"
    );
}

#[test]
fn select_escape_cancels() {
    let Some(s) = session(80, 24, &["select", "Pick", "a", "b"]) else {
        return;
    };
    s.wait_until(WAIT, "initial", |c| c.contains("> a"))
        .expect("initial frame");
    s.send_key(Key::Escape).expect("send Escape");
    let frame = s
        .wait_until(WAIT, "cancellation echo", |c| c.contains("cancelled."))
        .expect("cancellation echoed");
    assert!(frame.contains("Pick"), "label echoed on cancel:\n{frame}");
}

#[test]
fn short_window_windows_list_without_garble() {
    let ten = ["o0", "o1", "o2", "o3", "o4", "o5", "o6", "o7", "o8", "o9"];
    let mut args: Vec<&str> = vec!["select", "PickShort"];
    args.extend_from_slice(&ten);
    // 6 rows: 2 reserved (label + footer) leaves 4 visible option rows.
    let Some(s) = session(80, 6, &args) else {
        return;
    };
    let frame = s
        .wait_until(WAIT, "windowed footer", |c| c.contains("1/10"))
        .expect("footer should appear when list overflows");
    assert_eq!(
        frame.matches("PickShort").count(),
        1,
        "the short-window clamp must not duplicate the label:\n{frame}"
    );
    assert!(frame.contains("> o0"), "cursor on first row:\n{frame}");
    assert!(
        !frame.contains("o9"),
        "tail option windowed off-screen:\n{frame}"
    );

    // Walk the cursor past the bottom edge: the window slides and the
    // footer position tracks it.
    s.send_keys(&[Key::Down; 5]).expect("send Down x5");
    let frame = s
        .wait_until(WAIT, "scrolled footer", |c| c.contains("6/10"))
        .expect("window should slide with the cursor");
    assert!(frame.contains("> o5"), "cursor followed down:\n{frame}");
    assert!(
        !frame.contains("> o0"),
        "top of list scrolled away:\n{frame}"
    );
    assert_eq!(
        frame.matches("PickShort").count(),
        1,
        "label still single after scroll:\n{frame}"
    );
}

#[test]
fn resize_rewindows_the_list() {
    let ten = ["o0", "o1", "o2", "o3", "o4", "o5", "o6", "o7", "o8", "o9"];
    let mut args: Vec<&str> = vec!["select", "PickResize"];
    args.extend_from_slice(&ten);
    // Tall enough that all 10 options fit — no footer.
    let Some(s) = session(80, 20, &args) else {
        return;
    };
    let frame = s
        .wait_until(WAIT, "all options visible", |c| c.contains("o9"))
        .expect("full list rendered");
    assert!(
        !frame.contains("/10"),
        "no position footer while the list fits:\n{frame}"
    );

    // Shrink: the list no longer fits, so it windows and grows a footer.
    s.resize(80, 6).expect("shrink");
    let frame = s
        .wait_until(WAIT, "footer after shrink", |c| c.contains("1/10"))
        .expect("shrink should window the list");
    assert!(!frame.contains("o9"), "windowed after shrink:\n{frame}");
    assert_eq!(
        frame.matches("PickResize").count(),
        1,
        "resize redraw must not stack the label:\n{frame}"
    );

    // Grow back: the whole list fits again and the footer disappears.
    s.resize(80, 20).expect("grow");
    let frame = s
        .wait_until(WAIT, "footer gone after grow", |c| {
            c.contains("o9") && !c.contains("/10")
        })
        .expect("grow should un-window the list");
    assert_eq!(
        frame.matches("PickResize").count(),
        1,
        "label single after grow:\n{frame}"
    );
}

#[test]
fn multi_select_space_toggles_and_confirms() {
    let Some(s) = session(80, 24, &["multi", "PickMany", "apple", "banana", "cherry"]) else {
        return;
    };
    s.wait_until(WAIT, "initial unchecked", |c| c.contains("> [ ] apple"))
        .expect("initial checkbox frame");

    s.send_key(Key::Space).expect("send Space");
    let frame = s
        .wait_until(WAIT, "row toggled on", |c| c.contains("> [x] apple"))
        .expect("space toggles the cursor row");
    assert!(
        frame.contains("[ ] banana"),
        "other rows stay unchecked:\n{frame}"
    );

    // Check a second row, then confirm; both land in the echoed summary.
    s.send_keys(&[Key::Down, Key::Space, Key::Enter])
        .expect("send Down Space Enter");
    let frame = s
        .wait_until(WAIT, "confirmation echo", |c| c.contains("selected:"))
        .expect("confirmation echoed");
    assert!(
        frame.contains("apple") && frame.contains("banana"),
        "both checked options summarised:\n{frame}"
    );
}
