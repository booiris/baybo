use std::path::Path;

fn main() {
    // `tauri::generate_context!` is a compile-time proc macro that reads the
    // frontendDist (`../dist`). On a fresh checkout — where `dist/` is
    // gitignored and the Vite frontend hasn't been built yet — write a
    // placeholder so `cargo build`/`cargo clippy --all` work without a prior
    // `pnpm build`. A real `vite build` overwrites this.
    let dist = Path::new("../dist");
    let index = dist.join("index.html");
    if !index.exists() {
        let _ = std::fs::create_dir_all(dist);
        let _ = std::fs::write(
            &index,
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Aura</title></head><body><div id=\"root\"></div></body></html>\n",
        );
    }
    tauri_build::build()
}
