//! Test probe for the real-terminal setup picker. Built only with the
//! `test-support` feature (see the `required-features` gate in
//! `Cargo.toml`) so it never ships in a release build, and launched inside
//! a tmux pane by `crates/setup/tests/picker_render.rs` (via
//! `aura-term-harness`) so the crossterm picker can be driven and its
//! rendered frames captured.
//!
//! Usage: `picker_probe <select|multi> <label> [option...]`
//!   - `select`: single-choice picker over the options.
//!   - `multi`:  checkbox picker; `PROBE_INITIAL` (comma-separated indices)
//!     pre-checks rows.
//!
//! After the picker returns it echoes its outcome (handled inside the
//! picker), then this probe BLOCKS on stdin so the final frame stays on
//! screen for the harness to capture — the capture-while-alive pattern.
//! The harness kills the pane when the test ends; nothing ever feeds
//! stdin, so the read never returns.

use std::io::{BufRead, Write};

use aura_setup::{Prompter, TtyPrompter};

fn main() {
    let mut args = std::env::args().skip(1);
    let kind = args.next().unwrap_or_else(|| "select".to_string());
    let label = args.next().unwrap_or_else(|| "Pick one".to_string());
    let options: Vec<String> = args.collect();
    let options = if options.is_empty() {
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    } else {
        options
    };
    let opt_refs: Vec<&str> = options.iter().map(String::as_str).collect();

    // A failed `TtyPrompter::new` (not a TTY) is the one case worth a loud
    // stderr line — every other outcome is rendered by the picker itself.
    match TtyPrompter::new() {
        Ok(mut prompter) => match kind.as_str() {
            "multi" => {
                let initial = parse_initial(options.len());
                let _ = prompter.multi_select(&label, &opt_refs, &initial);
            }
            _ => {
                let _ = prompter.select(&label, &opt_refs);
            }
        },
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "PROBE_NOT_A_TTY: {e}");
        }
    }

    block_forever();
}

/// `PROBE_INITIAL="0,2"` -> a checked mask over `len` rows.
fn parse_initial(len: usize) -> Vec<bool> {
    let mut mask = vec![false; len];
    if let Ok(raw) = std::env::var("PROBE_INITIAL") {
        for tok in raw.split(',') {
            if let Ok(idx) = tok.trim().parse::<usize>()
                && idx < len
            {
                mask[idx] = true;
            }
        }
    }
    mask
}

/// Park the process so its last rendered frame stays on screen until the
/// harness tears the pane down.
fn block_forever() -> ! {
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => std::thread::sleep(std::time::Duration::from_secs(3600)),
            Ok(_) => {}
        }
    }
}
