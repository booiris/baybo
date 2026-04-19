//! Generate a 32-byte PSK into `$OUT_DIR/tui_psk.bin` on every build.
//!
//! The bytes are drawn from the OS RNG, so cloning the open-source repo
//! gives an attacker nothing — each build tree has its own PSK embedded
//! in the compiled binary via `include_bytes!`.
//!
//! The build script intentionally has no `cargo:rerun-if-*` hints other
//! than the default (script contents). That makes the PSK stable across
//! an incremental `cargo build`, but any workspace change that forces a
//! rebuild of this crate will roll a fresh PSK.

use std::env;
use std::fs;
use std::path::PathBuf;

use rand::Rng;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let psk_path = out_dir.join("tui_psk.bin");

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);

    fs::write(&psk_path, bytes).expect("write tui_psk.bin");

    println!("cargo:rerun-if-changed=build.rs");
}
