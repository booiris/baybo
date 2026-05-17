//! Bake the React dashboard and the in-tree channel sidecars into the
//! gateway binary.
//!
//! Three pipelines share this file. The first prepares the TS toolchain;
//! the other two consume it:
//!
//! * **pnpm install** — fingerprints `pnpm-lock.yaml`, the root
//!   `package.json`/`pnpm-workspace.yaml`, and every workspace
//!   `package.json` under `web/`, `sdks/*`, `channel-src/*`, and
//!   `tool-src/*`. When the fingerprint changes or
//!   `node_modules/.modules.yaml` is missing, we run
//!   `pnpm install` (non-interactive) so the downstream pipelines
//!   never trip over a stale workspace.
//! * **WebUI** — watches the relevant `web/` sources, reruns
//!   `pnpm --filter aura-web build` when those inputs change, then
//!   zstd-compresses each emitted `web/dist` asset and writes
//!   `$OUT_DIR/webui_assets.rs` with one compressed `include_bytes!`
//!   arm per asset. The runtime lazily decompresses the table on first
//!   request.
//! * **Sidecars** — runs `pnpm --filter @aura/channel-<name> bundle`
//!   (and the analogous tool-sidecar bundle scripts) to produce a
//!   single self-contained ESM file at `dist/bundle.mjs`. Channel
//!   sidecars in `channel-src/*` use `bun build`; tool sidecars in
//!   `tool-src/*` (e.g. the browser sidecar) use `esbuild` + `node`.
//!   Each bundle plus any aux assets declared in `package.json` is
//!   zstd-compressed and emitted into `$OUT_DIR/sidecar_assets.rs`
//!   for the gateway's `sidecar` module to consume. At runtime
//!   channel sidecars are spawned with the host's `bun` binary and
//!   tool sidecars with the host's `node` binary (each resolved from
//!   `PATH`) — no JS runtime is shipped inside the gateway binary.
//!
//! The sidecar pipeline degrades gracefully: if `pnpm` is missing,
//! `node_modules` aren't populated, or the bundler chokes on a
//! bundle, we emit an assets file with empty slices and print
//! `cargo:warning=…`.
//! The gateway supervisor then skips spawning sidecars and logs a
//! one-liner — `cargo build` itself never fails because of sidecar
//! packaging trouble.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::Digest;

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo"),
    );
    let ws_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("gateway crate lives under <workspace>/crates/gateway")
        .to_path_buf();

    if let Err(e) = ensure_pnpm_install(&ws_root) {
        println!(
            "cargo:warning=pnpm install failed ({e}); webui/sidecar packaging may fall back to placeholders"
        );
    }

    embed_webui(&ws_root);
    embed_sidecars(&ws_root);
}

// ---------------------------------------------------------------------
// pnpm install (TS workspace deps)
// ---------------------------------------------------------------------

/// Sync the workspace's `node_modules` to `pnpm-lock.yaml` before
/// either the WebUI or sidecar pipelines invoke pnpm scripts. The
/// fingerprint covers the lockfile, the root `package.json` (whose
/// `pnpm.overrides` change install output), `pnpm-workspace.yaml`,
/// and every workspace `package.json` under `web/`, `sdks/*`,
/// `channel-src/*`, and `tool-src/*`. Steady-state builds find a
/// matching stamp and skip the pnpm invocation entirely.
fn ensure_pnpm_install(ws_root: &Path) -> Result<(), String> {
    let cache_dir = ws_root.join("target").join("pnpm-install-cache");
    let stamp_path = cache_dir.join("fingerprint.txt");
    // `.modules.yaml` is pnpm's own marker file inside `node_modules`.
    // If the user nukes `node_modules` we force a reinstall even when
    // the fingerprint hasn't moved.
    let modules_marker = ws_root.join("node_modules").join(".modules.yaml");

    let fingerprint = pnpm_install_fingerprint(ws_root)?;
    let cached = fs::read_to_string(&stamp_path).ok();
    if cached.as_deref().map(str::trim) == Some(fingerprint.as_str()) && modules_marker.exists() {
        return Ok(());
    }

    let output = Command::new("pnpm")
        .arg("install")
        .current_dir(ws_root)
        // Suppress pnpm's interactive "modules directories will be
        // removed and reinstalled from scratch" confirmation, which
        // otherwise hangs the build script forever on a stale tree.
        .env("npm_config_confirm_modules_purge", "false")
        // Mute pnpm's update-notifier banner in build-script context.
        .env("CI", "1")
        .output()
        .map_err(|e| format!("spawn pnpm install: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pnpm install exited {}: stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }

    fs::create_dir_all(&cache_dir).map_err(|e| format!("mkdir {}: {e}", cache_dir.display()))?;
    fs::write(&stamp_path, format!("{fingerprint}\n"))
        .map_err(|e| format!("write {}: {e}", stamp_path.display()))?;
    Ok(())
}

fn pnpm_install_fingerprint(ws_root: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let inputs = pnpm_install_inputs(ws_root);
    for path in &inputs {
        emit_rerun_if_changed(path);
    }
    let mut hasher = Sha256::new();
    for path in &inputs {
        hash_path_state(&mut hasher, ws_root, path)?;
    }
    Ok(hex_short(&hasher.finalize(), 64))
}

fn pnpm_install_inputs(ws_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        ws_root.join("pnpm-lock.yaml"),
        ws_root.join("pnpm-workspace.yaml"),
        ws_root.join("package.json"),
        ws_root.join("web/package.json"),
    ];
    for dir_root in ["sdks", "channel-src", "tool-src"] {
        let root = ws_root.join(dir_root);
        let Ok(reader) = fs::read_dir(&root) else {
            continue;
        };
        for entry in reader.flatten() {
            let p = entry.path();
            if p.is_dir() {
                paths.push(p.join("package.json"));
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

// ---------------------------------------------------------------------
// WebUI
// ---------------------------------------------------------------------

fn embed_webui(ws_root: &Path) {
    let web_root = ws_root.join("web");
    let dist = web_root.join("dist");
    let index = dist.join("index.html");
    let generated_schema = web_root.join("src/api/schema.d.ts");
    let webui_cache_dir = ws_root.join("target").join("webui-cache");

    let source_fingerprint = match webui_source_fingerprint(ws_root, &web_root) {
        Ok(fingerprint) => Some(fingerprint),
        Err(e) => {
            println!("cargo:warning=hash webui inputs failed ({e}); rebuilding webui");
            None
        }
    };

    let mut should_build = !index.exists() || !generated_schema.exists();
    if let Some(fingerprint) = source_fingerprint.as_deref() {
        should_build |= webui_build_required(&webui_cache_dir, fingerprint, &index);
    } else {
        should_build = true;
    }

    if should_build {
        match build_webui(ws_root) {
            Ok(()) => {
                if let Some(fingerprint) = source_fingerprint.as_deref()
                    && let Err(e) = write_webui_build_stamp(&webui_cache_dir, fingerprint)
                {
                    println!("cargo:warning=write webui build stamp failed: {e}");
                }
            }
            Err(e) => {
                println!(
                    "cargo:warning=webui build failed ({e}); using existing dist or placeholder"
                );
            }
        }
    }

    if !index.exists() {
        fs::create_dir_all(&dist).expect("create dist directory");
        fs::write(
            &index,
            concat!(
                "<!doctype html><html><head><meta charset=\"utf-8\">",
                "<title>Aura WebUI</title></head><body style=\"font-family:monospace;padding:2rem\">",
                "<h1>Aura WebUI not built</h1>",
                "<p>Run <code>pnpm install &amp;&amp; pnpm --filter aura-web build</code> to embed the dashboard.</p>",
                "</body></html>",
            ),
        )
        .expect("write stub index.html");
    }

    let mut files = Vec::new();
    collect_assets(&dist, &dist, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let generated = out_dir.join("webui_assets.rs");
    let mut out = fs::File::create(&generated).expect("create generated assets file");
    writeln!(out, "// @generated by build.rs — do not edit.").expect("write header");
    writeln!(
        out,
        "pub(super) struct WebAsset {{ \
         pub path: &'static str, \
         pub mime: &'static str, \
         pub content_zst: &'static [u8] \
         }}",
    )
    .expect("write struct");
    writeln!(out, "pub(super) const ASSETS: &[WebAsset] = &[").expect("write array open");
    for (idx, (rel, abs)) in files.iter().enumerate() {
        let mime = mime_for(rel);
        let zst_path = out_dir.join(web_asset_zst_name(idx, rel));
        compress_file(abs, &zst_path).expect("compress web asset");
        writeln!(
            out,
            "    WebAsset {{ path: {rel:?}, mime: {mime:?}, content_zst: include_bytes!({zst_path:?}) }},",
        )
        .expect("write asset");
    }
    writeln!(out, "];").expect("write array close");

    emit_rerun_if_changed(&dist);
}

fn collect_assets(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(reader) = fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_assets(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push((rel.to_string_lossy().replace('\\', "/"), path));
        }
    }
}

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn web_asset_zst_name(index: usize, rel: &str) -> String {
    let mut sanitized = String::with_capacity(rel.len());
    for ch in rel.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    format!("webui-{index:04}-{sanitized}.zst")
}

fn webui_source_fingerprint(ws_root: &Path, web_root: &Path) -> Result<String, String> {
    let input_roots = webui_input_roots(ws_root, web_root);
    for path in &input_roots {
        emit_rerun_if_changed_filtered(path, &|candidate| {
            is_webui_generated_source(web_root, candidate)
        });
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for path in &input_roots {
        hash_path_state_filtered(&mut hasher, ws_root, path, &|candidate| {
            is_webui_generated_source(web_root, candidate)
        })?;
    }
    Ok(hex_short(&hasher.finalize(), 64))
}

fn webui_input_roots(ws_root: &Path, web_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        ws_root.join("pnpm-lock.yaml"),
        ws_root.join("pnpm-workspace.yaml"),
        ws_root.join("docs/openapi.json"),
        web_root.join("index.html"),
        web_root.join("package.json"),
        web_root.join("tsconfig.json"),
        web_root.join("tsconfig.app.json"),
        web_root.join("tsconfig.node.json"),
        web_root.join("vite.config.ts"),
        web_root.join("src"),
        web_root.join("public"),
    ];
    paths.sort();
    paths.dedup();
    paths
}

fn is_webui_generated_source(web_root: &Path, path: &Path) -> bool {
    path == web_root.join("src/api/schema.d.ts")
}

fn webui_build_required(cache_dir: &Path, fingerprint: &str, index: &Path) -> bool {
    if !index.exists() {
        return true;
    }
    let stamp = cache_dir.join("source-fingerprint.txt");
    let Ok(raw) = fs::read_to_string(&stamp) else {
        return true;
    };
    raw.trim() != fingerprint
}

fn write_webui_build_stamp(cache_dir: &Path, fingerprint: &str) -> Result<(), String> {
    fs::create_dir_all(cache_dir).map_err(|e| format!("mkdir {}: {e}", cache_dir.display()))?;
    let stamp = cache_dir.join("source-fingerprint.txt");
    fs::write(&stamp, format!("{fingerprint}\n"))
        .map_err(|e| format!("write {}: {e}", stamp.display()))
}

fn build_webui(ws_root: &Path) -> Result<(), String> {
    let output = Command::new("pnpm")
        .arg("--filter")
        .arg("aura-web")
        .arg("build")
        .current_dir(ws_root)
        .output()
        .map_err(|e| format!("spawn pnpm: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pnpm build exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

fn emit_rerun_if_changed_filtered<F>(path: &Path, skip: &F)
where
    F: Fn(&Path) -> bool,
{
    if !path.exists() || skip(path) {
        return;
    }
    println!("cargo:rerun-if-changed={}", path.display());
    if !path.is_dir() {
        return;
    }
    for entry in list_dir_entries(path) {
        emit_rerun_if_changed_filtered(&entry, skip);
    }
}

fn hash_path_state_filtered<F>(
    hasher: &mut sha2::Sha256,
    ws_root: &Path,
    path: &Path,
    skip: &F,
) -> Result<(), String>
where
    F: Fn(&Path) -> bool,
{
    if skip(path) {
        return Ok(());
    }
    let rel = path
        .strip_prefix(ws_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    hasher.update(rel.as_bytes());
    hasher.update([0]);
    if !path.exists() {
        hasher.update(b"missing");
        hasher.update([0]);
        return Ok(());
    }
    if path.is_dir() {
        hasher.update(b"dir");
        hasher.update([0]);
        for entry in list_dir_entries(path) {
            hash_path_state_filtered(hasher, ws_root, &entry, skip)?;
        }
        return Ok(());
    }
    hasher.update(b"file");
    hasher.update([0]);
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    hasher.update(&bytes);
    hasher.update([0]);
    Ok(())
}

fn list_dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    let Ok(reader) = fs::read_dir(dir) else {
        return entries;
    };
    for entry in reader.flatten() {
        entries.push(entry.path());
    }
    entries.sort();
    entries
}

// ---------------------------------------------------------------------
// Sidecar embedding
// ---------------------------------------------------------------------

/// zstd level. Level 19 is the upper end of the "reasonable" range —
/// ~10% smaller output than 3 at a ~10x build-time cost. Given the
/// output is cached for unchanged sidecar inputs and the binary ships
/// to every user, paying the build-time cost is the right tradeoff.
const ZSTD_LEVEL: i32 = 19;
const SIDECAR_CACHE_SCHEMA_VERSION: &str = "6";

/// The pnpm package that channel sidecars import (`@aura/channel-sdk`).
/// Sidecar `bundle` scripts shell out to `bun build`, which resolves
/// this dependency through the workspace symlink — but bun honors the
/// SDK's `exports` map, which points exclusively at `./dist/*.js`. So
/// the SDK must be compiled before any sidecar can be bundled. The
/// gateway build script runs `pnpm --filter` against this name in the
/// cache-miss path to guarantee `dist/` exists.
const CHANNEL_SDK_PKG: &str = "@aura/channel-sdk";

/// Each sidecar self-declares its **domain** — the business surface
/// it belongs to (`channel` for Telegram/WeChat/TUI, `browser` for
/// the Playwright tool, future domains for code-exec, db, …) — via
/// its `package.json`'s `aura.domain` field. The runtime exposes
/// per-domain iteration so `aura channel list/add` only sees the
/// `channel` domain, the browser supervisor only sees `browser`,
/// and adding a future domain doesn't touch any of those code
/// paths.
///
/// Domain is a free string at the build/runtime layer; constants
/// for the known ones live in `aura_gateway::sidecar::domains`.
struct AuxAsset {
    name: String,
    src: PathBuf,
}

struct BuiltSidecar {
    name: String,
    domain: String,
    bundle_path: PathBuf,
    content_hash: String,
    aux_assets: Vec<AuxAsset>,
}

struct EmittedAuxAsset {
    name: String,
    zst_path: PathBuf,
}

struct EmittedSidecar {
    name: String,
    domain: String,
    bundle_zst: PathBuf,
    content_hash: String,
    aux_assets: Vec<EmittedAuxAsset>,
}

fn embed_sidecars(ws_root: &Path) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let generated = out_dir.join("sidecar_assets.rs");
    let strict = sidecars_required();

    println!("cargo:rerun-if-env-changed={STRICT_SIDECARS_ENV}");

    // `channel-src/` holds channel sidecars, `tool-src/` holds tool
    // sidecars (browser, …). Both are bundled the same way and emit
    // into the same asset table; the `name` field (= directory name)
    // is the unique identifier used by `SidecarRuntime::bundle_for`.
    // Collisions across pools are a build-time error.
    // Discovery is now agnostic about which top-level directory a
    // sidecar lives in. Each `package.json` self-declares its
    // domain via `aura.domain`; the directory layout is just file-
    // system organisation. Adding a third `<x>-src/` is one line
    // here.
    let mut sidecar_dirs: Vec<PathBuf> = Vec::new();
    sidecar_dirs.extend(list_sidecar_dirs(&ws_root.join("channel-src")));
    sidecar_dirs.extend(list_sidecar_dirs(&ws_root.join("tool-src")));
    sidecar_dirs.sort();
    if let Some(dup) = first_duplicate_dir_name(&sidecar_dirs) {
        let msg = format!(
            "duplicate sidecar directory name `{dup}` across sidecar source dirs; \
             names must be unique because they key the embedded asset table"
        );
        sidecar_failure(strict, &msg);
        write_empty_sidecar_assets(&generated);
        return;
    }
    let cache_root = ws_root.join("target").join("sidecar-cache");
    let input_fingerprint = match sidecar_input_fingerprint(ws_root, &sidecar_dirs) {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            println!("cargo:warning=hash sidecar inputs failed ({e}); rebuilding sidecars");
            String::new()
        }
    };
    let cache_dir = cache_root.join(if input_fingerprint.is_empty() {
        "adhoc".to_string()
    } else {
        input_fingerprint.clone()
    });

    if !input_fingerprint.is_empty() {
        match load_sidecar_cache(&cache_dir) {
            Ok(Some(cache)) => {
                emit_sidecar_assets(&generated, &cache);
                return;
            }
            Ok(None) => {}
            Err(e) => {
                println!("cargo:warning=read sidecar cache failed ({e}); rebuilding sidecars");
            }
        }
    }

    if let Err(e) = fs::create_dir_all(&cache_dir) {
        let msg = format!(
            "create sidecar cache dir {} failed: {e}",
            cache_dir.display()
        );
        sidecar_failure(strict, &msg);
        write_empty_sidecar_assets(&generated);
        return;
    }

    // Compile `@aura/channel-sdk` before bundling. Channel sidecars
    // import it, and `bun build` resolves the import through the
    // SDK's `exports` map → `./dist/*.js`; without `dist/` every
    // sidecar bundle fails with "Could not resolve". This runs only
    // on the cache-miss path (the fingerprint above already covers
    // the SDK's `src/` and config), and `tsc` is incremental, so
    // the cost is paid once per SDK change.
    if let Err(e) = build_channel_sdk(ws_root) {
        let msg = format!("building {CHANNEL_SDK_PKG} failed: {e}");
        sidecar_failure(strict, &msg);
        write_empty_sidecar_assets(&generated);
        return;
    }

    let mut entries: Vec<BuiltSidecar> = Vec::new();
    let mut bundling_failures = 0usize;
    for dir in &sidecar_dirs {
        let name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        // Channel sidecars name their entry `src/index.ts`; the browser
        // tool sidecar (an embedded MCP server) uses `src/server.ts`
        // since it has a different lifecycle. Either is fine.
        let entry_index = dir.join("src/index.ts");
        let entry_server = dir.join("src/server.ts");
        if !entry_index.exists() && !entry_server.exists() {
            println!(
                "cargo:warning=sidecar '{name}' has neither src/index.ts nor src/server.ts; skipping",
            );
            continue;
        }
        let (pkg_name, domain) = match read_package_meta(&dir.join("package.json")) {
            Ok(m) => m,
            Err(e) => {
                bundling_failures += 1;
                let msg = format!("read sidecar '{name}' package.json failed: {e}");
                sidecar_failure(strict, &msg);
                continue;
            }
        };
        match bundle_with_pnpm(ws_root, &pkg_name) {
            Ok(()) => {}
            Err(e) => {
                bundling_failures += 1;
                let msg = format!("bundling sidecar '{name}' failed: {e}");
                sidecar_failure(strict, &msg);
                continue;
            }
        }
        let bundle_src = dir.join("dist/bundle.mjs");
        if !bundle_src.exists() {
            bundling_failures += 1;
            let msg = format!(
                "bundling sidecar '{name}' produced no {}",
                bundle_src.display()
            );
            sidecar_failure(strict, &msg);
            continue;
        }
        let aux_assets = match sidecar_aux_assets(dir) {
            Ok(assets) => assets,
            Err(e) => {
                bundling_failures += 1;
                let msg = format!("collect aux assets for sidecar '{name}' failed: {e}");
                sidecar_failure(strict, &msg);
                continue;
            }
        };
        let bundle_path = cache_dir.join(format!("{name}.mjs"));
        if let Err(e) = fs::copy(&bundle_src, &bundle_path) {
            bundling_failures += 1;
            let msg = format!(
                "copy bundle for sidecar '{name}' from {} failed: {e}",
                bundle_src.display()
            );
            sidecar_failure(strict, &msg);
            continue;
        }
        let hash = match sidecar_content_hash(&bundle_path, &aux_assets) {
            Ok(hash) => hash,
            Err(e) => {
                bundling_failures += 1;
                let msg = format!("hash sidecar '{name}' failed: {e}");
                sidecar_failure(strict, &msg);
                continue;
            }
        };
        entries.push(BuiltSidecar {
            name: name.to_string(),
            domain: domain.clone(),
            bundle_path,
            content_hash: hash,
            aux_assets,
        });
    }
    if strict && bundling_failures > 0 {
        // sidecar_failure has already paniced above, but defensively
        // bail before we ship a partial set.
        panic!("{STRICT_SIDECARS_ENV} is set but {bundling_failures} sidecar(s) failed to bundle",);
    }

    let mut emitted: Vec<EmittedSidecar> = Vec::new();
    for entry in &entries {
        let zst_path = cache_dir.join(format!("{}.mjs.zst", entry.name));
        if let Err(e) = compress_file(&entry.bundle_path, &zst_path) {
            let name = entry.name.as_str();
            let msg = format!("compress sidecar '{name}' failed: {e}");
            sidecar_failure(strict, &msg);
            continue;
        }
        let mut aux_assets = Vec::new();
        let mut aux_failed = false;
        for (idx, aux) in entry.aux_assets.iter().enumerate() {
            let aux_zst = cache_dir.join(format!(
                "{}.aux.{}.{}.zst",
                entry.name,
                idx,
                sanitized_filename(&aux.name)
            ));
            if let Err(e) = compress_file(&aux.src, &aux_zst) {
                let msg = format!(
                    "compress aux asset '{}' for sidecar '{}' failed: {e}",
                    aux.name, entry.name
                );
                sidecar_failure(strict, &msg);
                aux_failed = true;
                break;
            }
            aux_assets.push(EmittedAuxAsset {
                name: aux.name.clone(),
                zst_path: aux_zst,
            });
        }
        if aux_failed {
            continue;
        }
        emitted.push(EmittedSidecar {
            name: entry.name.clone(),
            domain: entry.domain.clone(),
            bundle_zst: zst_path,
            content_hash: entry.content_hash.clone(),
            aux_assets,
        });
    }

    // Only cache when *every* sidecar bundled + emitted cleanly. A
    // partial-success cache poisons subsequent builds: the input
    // fingerprint matches but the cached manifest is missing whichever
    // sidecars failed (e.g. when a previous `pnpm` invocation hit a
    // sandbox / network glitch), and the early-return at line ~428
    // hands out the corrupt cache without ever retrying. Better to
    // skip the cache write so the next build re-attempts bundling
    // from scratch.
    let all_bundled = bundling_failures == 0 && emitted.len() == entries.len();
    if all_bundled {
        if let Err(e) = write_sidecar_cache_manifest(&cache_dir, &emitted) {
            println!("cargo:warning=write sidecar cache manifest failed: {e}");
        }
    } else {
        println!(
            "cargo:warning=skipping sidecar cache write \
             ({bundling_failures} bundling failure(s); \
             {} compressed of {} entries) — next build will retry from scratch",
            emitted.len(),
            entries.len(),
        );
    }

    emit_sidecar_assets(&generated, &emitted);
}

/// Opt-in CI/release gate: when set to `1`/`true`, any sidecar
/// packaging failure becomes a hard build error instead of a
/// `cargo:warning` + empty-asset degrade. The gateway binary boots fine
/// without sidecars during local backend hacking, but a release build
/// with no embedded channels is almost always a packaging mistake.
const STRICT_SIDECARS_ENV: &str = "AURA_REQUIRE_SIDECARS";

fn sidecars_required() -> bool {
    matches!(
        std::env::var(STRICT_SIDECARS_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

fn sidecar_failure(strict: bool, msg: &str) {
    if strict {
        panic!("{STRICT_SIDECARS_ENV}=1 and sidecar packaging failed: {msg}");
    }
    println!("cargo:warning={msg}");
}

fn list_sidecar_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(reader) = fs::read_dir(root) else {
        return dirs;
    };
    for entry in reader.flatten() {
        let p = entry.path();
        if p.is_dir() {
            dirs.push(p);
        }
    }
    dirs.sort();
    dirs
}

/// Detect the first directory name that appears in more than one sidecar
/// pool (`channel-src/` + `tool-src/`). Returns `None` when names are
/// unique. The asset table keys on the directory name, so collisions
/// would silently shadow each other at materialise time.
fn first_duplicate_dir_name(dirs: &[PathBuf]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !seen.insert(name.to_string()) {
            return Some(name.to_string());
        }
    }
    None
}

fn sidecar_input_fingerprint(ws_root: &Path, sidecar_dirs: &[PathBuf]) -> Result<String, String> {
    let input_roots = sidecar_input_roots(ws_root, sidecar_dirs);
    println!(
        "cargo:rerun-if-changed={}",
        ws_root.join("channel-src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ws_root.join("tool-src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ws_root.join("sdks/channel-ts").display()
    );
    for dir in sidecar_dirs {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    for path in &input_roots {
        emit_rerun_if_changed(path);
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(SIDECAR_CACHE_SCHEMA_VERSION.as_bytes());
    hasher.update([0]);
    for path in &input_roots {
        hash_path_state(&mut hasher, ws_root, path)?;
    }
    Ok(hex_short(&hasher.finalize(), 64))
}

fn sidecar_input_roots(ws_root: &Path, sidecar_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = vec![
        ws_root.join("pnpm-lock.yaml"),
        ws_root.join("pnpm-workspace.yaml"),
        // Fingerprint the SDK *source*, not the compiled `dist/`. The
        // build script compiles `dist/` on the cache-miss path, so
        // including `dist/` here would mean the fingerprint changes
        // every time we (re)build the SDK, and the cache could never
        // be reused on the next cargo invocation.
        ws_root.join("sdks/channel-ts/src"),
        ws_root.join("sdks/channel-ts/package.json"),
        ws_root.join("sdks/channel-ts/tsconfig.json"),
    ];
    for dir in sidecar_dirs {
        paths.push(dir.join("src"));
        paths.push(dir.join("package.json"));
        paths.push(dir.join("tsconfig.json"));
    }
    paths.sort();
    paths.dedup();
    paths
}

fn emit_rerun_if_changed(path: &Path) {
    if !path.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", path.display());
    if !path.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect_files(path, &mut files);
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(reader) = fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
    out.sort();
}

fn hash_path_state(hasher: &mut sha2::Sha256, ws_root: &Path, path: &Path) -> Result<(), String> {
    let rel = path
        .strip_prefix(ws_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    hasher.update(rel.as_bytes());
    hasher.update([0]);
    if !path.exists() {
        hasher.update(b"missing");
        hasher.update([0]);
        return Ok(());
    }
    if path.is_dir() {
        hasher.update(b"dir");
        hasher.update([0]);
        let mut files = Vec::new();
        collect_files(path, &mut files);
        for file in files {
            hash_path_state(hasher, ws_root, &file)?;
        }
        return Ok(());
    }
    hasher.update(b"file");
    hasher.update([0]);
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    hasher.update(&bytes);
    hasher.update([0]);
    Ok(())
}

/// Read both the npm package name and the Aura `domain` field from
/// a sidecar's `package.json`. Domain is mandatory — a missing
/// `aura.domain` is a build-time error so a new sidecar can't
/// silently default into the wrong family.
fn read_package_meta(package_json: &Path) -> Result<(String, String), String> {
    let raw = fs::read_to_string(package_json)
        .map_err(|e| format!("read {}: {e}", package_json.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", package_json.display()))?;
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{} has no string `name`", package_json.display()))?
        .to_owned();
    let domain = value
        .get("aura")
        .and_then(|v| v.get("domain"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "{} has no string `aura.domain` (sidecars must declare a domain like \
                 `\"aura\": {{ \"domain\": \"channel\" }}` so the runtime can iterate \
                 them per-family)",
                package_json.display()
            )
        })?
        .to_owned();
    if domain.is_empty()
        || !domain
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
    {
        return Err(format!(
            "{} has invalid `aura.domain` `{domain}` — must match [a-z0-9_]+",
            package_json.display()
        ));
    }
    Ok((name, domain))
}

/// Compile `@aura/channel-sdk` (`pnpm --filter <pkg> build`) so its
/// `dist/` exists before sidecar bundlers try to resolve it. `tsc` is
/// incremental — on a no-op the cost is negligible — so it's safe to
/// run this unconditionally on the cache-miss path.
fn build_channel_sdk(ws_root: &Path) -> Result<(), String> {
    let output = Command::new("pnpm")
        .arg("--filter")
        .arg(CHANNEL_SDK_PKG)
        .arg("build")
        .current_dir(ws_root)
        .output()
        .map_err(|e| format!("spawn pnpm: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pnpm build exited {}: stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

/// Run `pnpm --filter <pkg> bundle` inside the workspace. The script
/// is defined in each sidecar's `package.json` and invokes its own
/// bundler — `bun build` for channel sidecars, `esbuild` for tool
/// sidecars — to emit a single self-contained `dist/bundle.mjs` for
/// the host runtime (`bun` or `node` respectively) to consume.
fn bundle_with_pnpm(ws_root: &Path, pkg_name: &str) -> Result<(), String> {
    let output = Command::new("pnpm")
        .arg("--filter")
        .arg(pkg_name)
        .arg("bundle")
        .current_dir(ws_root)
        .output()
        .map_err(|e| format!("spawn pnpm: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pnpm bundle exited {}: stdout={}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

fn compress_file(src: &Path, dst: &Path) -> Result<(), String> {
    let data = fs::read(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    let compressed = zstd::bulk::compress(&data, ZSTD_LEVEL)
        .map_err(|e| format!("zstd compress {}: {e}", src.display()))?;
    fs::write(dst, &compressed).map_err(|e| format!("write {}: {e}", dst.display()))
}

fn sidecar_content_hash(bundle: &Path, aux_assets: &[AuxAsset]) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"bundle");
    hasher.update([0]);
    let bundle_bytes = fs::read(bundle).map_err(|e| format!("read {}: {e}", bundle.display()))?;
    hasher.update(&bundle_bytes);
    hasher.update([0]);
    for aux in aux_assets {
        hasher.update(b"aux");
        hasher.update([0]);
        hasher.update(aux.name.as_bytes());
        hasher.update([0]);
        let bytes = fs::read(&aux.src).map_err(|e| format!("read {}: {e}", aux.src.display()))?;
        hasher.update(&bytes);
        hasher.update([0]);
    }
    Ok(hex_short(&hasher.finalize(), 16))
}

fn sidecar_aux_assets(dir: &Path) -> Result<Vec<AuxAsset>, String> {
    let package_json = dir.join("package.json");
    if !package_json.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&package_json)
        .map_err(|e| format!("read {}: {e}", package_json.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", package_json.display()))?;
    let Some(assets) = value
        .get("aura")
        .and_then(|v| v.get("auxAssets"))
        .and_then(|v| v.as_array())
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for asset in assets {
        let src = asset
            .get("src")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "aura.auxAssets entries require string `src`".to_string())?;
        let name = asset
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "aura.auxAssets entries require string `name`".to_string())?;
        let recursive = asset
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let src_path = dir.join(src);
        if !src_path.exists() {
            return Err(format!(
                "aux asset source '{}' does not exist",
                src_path.display()
            ));
        }
        if recursive {
            // Directory-tree variant: walk every file under `src` and
            // emit one AuxAsset per file named `<name>/<relative path>`.
            // Used by the browser sidecar to ship chrome-devtools-mcp's
            // 280+ pre-bundled JS files without enumerating each one
            // by hand in package.json.
            if !src_path.is_dir() {
                return Err(format!(
                    "aux asset '{name}' has recursive=true but src '{}' is not a directory",
                    src_path.display()
                ));
            }
            collect_recursive_aux(&src_path, &src_path, name, &mut out)?;
        } else {
            validate_aux_name(name)?;
            out.push(AuxAsset {
                name: name.to_string(),
                src: src_path,
            });
        }
    }
    Ok(out)
}

fn collect_recursive_aux(
    root: &Path,
    cur: &Path,
    name_prefix: &str,
    out: &mut Vec<AuxAsset>,
) -> Result<(), String> {
    let entries = fs::read_dir(cur).map_err(|e| format!("read aux dir {}: {e}", cur.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("read aux dir entry under {}: {e}", cur.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("stat aux dir entry {}: {e}", path.display()))?;
        if file_type.is_dir() {
            collect_recursive_aux(root, &path, name_prefix, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("aux dir relative path for {}: {e}", path.display()))?;
            let rel_str = rel
                .to_str()
                .ok_or_else(|| format!("aux dir entry {} has non-UTF-8 path", path.display()))?
                .replace('\\', "/");
            let aux_name = format!("{name_prefix}/{rel_str}");
            validate_aux_name(&aux_name)?;
            out.push(AuxAsset {
                name: aux_name,
                src: path,
            });
        }
    }
    Ok(())
}

fn validate_aux_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("aux asset name must not be empty".to_string());
    }
    if name.contains(['\t', '\n', '\r']) {
        return Err(format!(
            "aux asset name '{name}' contains a control character"
        ));
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(format!("aux asset name '{name}' must be relative"));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "aux asset name '{name}' contains an unsafe component"
                ));
            }
        }
    }
    Ok(())
}

fn sanitized_filename(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    sanitized
}

fn hex_short(bytes: &[u8], len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(len);
    for b in bytes {
        if out.len() >= len {
            break;
        }
        out.push(HEX[(b >> 4) as usize] as char);
        if out.len() >= len {
            break;
        }
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn emit_sidecar_assets(generated: &Path, sidecars: &[EmittedSidecar]) {
    let mut out = fs::File::create(generated).expect("create sidecar_assets.rs");
    writeln!(out, "// @generated by build.rs — do not edit.").unwrap();
    writeln!(
        out,
        "pub(crate) struct SidecarAuxAsset {{ \
         pub name: &'static str, \
         pub content_zst: &'static [u8] \
         }}",
    )
    .unwrap();
    writeln!(
        out,
        "pub(crate) struct SidecarAsset {{ \
         pub name: &'static str, \
         pub domain: &'static str, \
         pub bundle_zst: &'static [u8], \
         pub content_hash: &'static str, \
         pub aux_assets: &'static [SidecarAuxAsset] \
         }}",
    )
    .unwrap();
    writeln!(out, "pub(crate) const SIDECARS: &[SidecarAsset] = &[").unwrap();
    for sidecar in sidecars {
        writeln!(
            out,
            "    SidecarAsset {{ name: {:?}, domain: {:?}, bundle_zst: include_bytes!({:?}), content_hash: {:?}, aux_assets: &[",
            sidecar.name, sidecar.domain, sidecar.bundle_zst, sidecar.content_hash,
        )
        .unwrap();
        for aux in &sidecar.aux_assets {
            writeln!(
                out,
                "        SidecarAuxAsset {{ name: {:?}, content_zst: include_bytes!({:?}) }},",
                aux.name, aux.zst_path,
            )
            .unwrap();
        }
        writeln!(out, "    ] }},").unwrap();
    }
    writeln!(out, "];").unwrap();
}

fn write_empty_sidecar_assets(generated: &Path) {
    emit_sidecar_assets(generated, &[]);
}

fn load_sidecar_cache(cache_dir: &Path) -> Result<Option<Vec<EmittedSidecar>>, String> {
    let manifest = cache_dir.join("manifest.tsv");
    if !manifest.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&manifest).map_err(|e| format!("read {}: {e}", manifest.display()))?;

    let mut sidecars: BTreeMap<String, EmittedSidecar> = BTreeMap::new();
    for line in raw.lines() {
        let mut parts = line.split('\t');
        let kind = parts.next().unwrap_or_default();
        match kind {
            "sidecar" => {
                let name = parts.next().unwrap_or_default();
                let domain = parts.next().unwrap_or_default();
                let hash = parts.next().unwrap_or_default();
                if name.is_empty() || domain.is_empty() || hash.is_empty() {
                    return Err(format!(
                        "invalid sidecar cache manifest entry in {}",
                        manifest.display()
                    ));
                }
                let zst_path = cache_dir.join(format!("{name}.mjs.zst"));
                if !zst_path.exists() {
                    return Ok(None);
                }
                sidecars.insert(
                    name.to_string(),
                    EmittedSidecar {
                        name: name.to_string(),
                        domain: domain.to_string(),
                        bundle_zst: zst_path,
                        content_hash: hash.to_string(),
                        aux_assets: Vec::new(),
                    },
                );
            }
            "aux" => {
                let sidecar_name = parts.next().unwrap_or_default();
                let asset_name = parts.next().unwrap_or_default();
                let zst_name = parts.next().unwrap_or_default();
                if sidecar_name.is_empty() || asset_name.is_empty() || zst_name.is_empty() {
                    return Err(format!(
                        "invalid aux cache manifest entry in {}",
                        manifest.display()
                    ));
                }
                let zst_path = cache_dir.join(zst_name);
                if !zst_path.exists() {
                    return Ok(None);
                }
                let Some(sidecar) = sidecars.get_mut(sidecar_name) else {
                    return Err(format!(
                        "aux cache manifest entry before sidecar in {}",
                        manifest.display()
                    ));
                };
                sidecar.aux_assets.push(EmittedAuxAsset {
                    name: asset_name.to_string(),
                    zst_path,
                });
            }
            _ => {}
        }
    }

    Ok(Some(sidecars.into_values().collect()))
}

fn write_sidecar_cache_manifest(
    cache_dir: &Path,
    sidecars: &[EmittedSidecar],
) -> Result<(), String> {
    fs::create_dir_all(cache_dir).map_err(|e| format!("mkdir {}: {e}", cache_dir.display()))?;
    let manifest = cache_dir.join("manifest.tsv");
    let mut out = String::new();
    for sidecar in sidecars {
        out.push_str(&format!(
            "sidecar\t{}\t{}\t{}\n",
            sidecar.name, sidecar.domain, sidecar.content_hash
        ));
        for aux in &sidecar.aux_assets {
            let zst_name = aux
                .zst_path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("invalid aux zst path {}", aux.zst_path.display()))?;
            out.push_str(&format!(
                "aux\t{}\t{}\t{}\n",
                sidecar.name, aux.name, zst_name
            ));
        }
    }
    fs::write(&manifest, out).map_err(|e| format!("write {}: {e}", manifest.display()))
}
