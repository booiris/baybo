// Desktop entry point. The iOS/Android build enters through the
// `mobile_entry_point` in `lib.rs`, so this just forwards to `run`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    baybo_mobile_app_lib::run()
}
