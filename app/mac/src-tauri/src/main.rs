// Hide the extra console window on non-debug Windows builds; harmless on macOS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    aura_mac::run()
}
