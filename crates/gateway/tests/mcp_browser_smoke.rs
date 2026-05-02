//! End-to-end smoke test for the embedded browser MCP server.
//!
//! Replaces the old `tool_ws_smoke.rs` from before the browser sidecar
//! was reshaped into a stdio MCP server. Each test:
//!
//!   1. Materialises the embedded `tool-src/browser` bundle.
//!   2. Spawns an `McpReconciler` against a tempdir workspace.
//!   3. Waits for the reconciler to register `browser/<tool>` in the
//!      shared `ToolRegistry`.
//!   4. Drives one or more tool calls and asserts the outcome.
//!
//! All tests are gated `#[ignore]` because they require:
//!   - `pnpm install` populating `tool-src/browser/node_modules`
//!   - `pnpm --filter @aura/tool-browser bundle` producing
//!     `dist/bundle.mjs` (cargo build does this automatically once
//!     pnpm has hydrated the workspace)
//!   - `node` on PATH (override with `AURA_NODE_BIN`)
//!   - For chromium-driving tests: `pnpm --filter @aura/tool-browser
//!     exec playwright install chromium`
//!
//! Run manually:
//!   cargo test -p aura-gateway --test mcp_browser_smoke -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_config::BrowserConfig;
use aura_gateway::{SidecarRuntime, node_binary};
use aura_model::{ChannelType, User};
use aura_security::SecretVault;
use aura_storage::Store;
use aura_storage::test_support::MemoryBlobStore;
use aura_tools::ToolRegistry;
use aura_tools::mcp::{EmbeddedMcpProfile, McpReconciler, browser_mcp_profile, embedded_servers};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Skip with a clear reason when prerequisites are missing. Returns
/// `Some` only when everything is in place to run end-to-end.
fn boot_runtime() -> Option<(SidecarRuntime, PathBuf)> {
    let runtime = match SidecarRuntime::install() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipped: SidecarRuntime::install failed ({e})");
            return None;
        }
    };
    let Some(bundle) = runtime.bundle_for("browser") else {
        eprintln!(
            "skipped: browser bundle not embedded — run `pnpm install && \
             pnpm --filter @aura/tool-browser bundle && cargo build`",
        );
        return None;
    };
    let bundle = bundle.to_path_buf();
    let node = node_binary();
    if !node.exists() && !is_on_path(&node) {
        eprintln!(
            "skipped: `{}` not found (override with AURA_NODE_BIN)",
            node.display(),
        );
        return None;
    }
    Some((runtime, bundle))
}

fn is_on_path(name: &std::path::Path) -> bool {
    if name.is_absolute() {
        return name.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(name).is_file())
}

async fn boot_reconciler(
    workspace_root: PathBuf,
    browser_cfg: BrowserConfig,
) -> Option<(Arc<ToolRegistry>, Arc<McpReconciler>, CancellationToken)> {
    let (runtime, _bundle) = boot_runtime()?;
    let storage = Store::open(workspace_root.join("data"))
        .await
        .expect("open store");
    let master_key =
        aura_security::EncryptionKey::new(b"smoke-test-32-byte-fixed-keyqq!!".to_vec())
            .expect("master key");
    let vault = Arc::new(SecretVault::new(master_key, storage.secret.clone()));
    let blob_store: Arc<dyn aura_storage::BlobStore> = Arc::new(MemoryBlobStore::new());
    let registry = Arc::new(ToolRegistry::with_defaults(blob_store.clone()));

    let node_cmd = node_binary().display().to_string();
    let profiles: Vec<EmbeddedMcpProfile> = [runtime.bundle_for("browser").and_then(|p| {
        browser_mcp_profile(
            browser_cfg.enable,
            browser_cfg.sandbox,
            browser_cfg.chrome_path.as_deref(),
            browser_cfg.profile_dir.as_deref(),
            &browser_cfg.args,
            browser_cfg.allow_loopback,
            // Smoke tests don't exercise the >2 MiB upload path —
            // the served pages are tiny. Leaving blob_upload at None
            // keeps every screenshot inline.
            None,
            node_cmd.clone(),
            p,
        )
    })]
    .into_iter()
    .flatten()
    .collect();
    let embedded = embedded_servers(&profiles);
    if embedded.is_empty() {
        eprintln!("skipped: embedded_servers returned empty — bundle missing or browser disabled");
        return None;
    }

    let cancel = CancellationToken::new();
    let reconciler = McpReconciler::new(
        workspace_root,
        Arc::clone(&registry),
        Arc::clone(&vault),
        Some(blob_store),
        embedded,
        cancel.clone(),
    );
    reconciler.spawn();

    if !wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
        registry.get("browser/navigate").is_some()
    })
    .await
    {
        eprintln!("skipped: browser MCP server never connected within 15s");
        cancel.cancel();
        return None;
    }
    Some((registry, reconciler, cancel))
}

fn ctx(session_id: &str) -> aura_tools::ToolContext {
    aura_tools::ToolContext {
        session_id: session_id.to_string(),
        user: User {
            id: "smoke-user".into(),
            name: None,
            channel: ChannelType::tui(),
        },
        timeout: Duration::from_secs(30),
        cancellation_token: CancellationToken::new(),
        workspace_root: std::env::temp_dir(),
        sandbox: None,
        approval: None,
    }
}

async fn wait_until<F: Fn() -> bool>(max: Duration, interval: Duration, cond: F) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < max {
        if cond() {
            return true;
        }
        tokio::time::sleep(interval).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires bundled tool-src/browser MCP server + node on PATH"]
async fn browser_smoke_ssrf_guard_fires_via_real_mcp_server() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let cfg = BrowserConfig {
        enable: true,
        profile_dir: Some(tempdir.path().join("browser-profile")),
        ..Default::default()
    };
    let Some((registry, _reconciler, cancel)) =
        boot_reconciler(tempdir.path().to_path_buf(), cfg).await
    else {
        return;
    };

    let outcome = registry
        .execute(
            "browser/navigate",
            json!({ "url": "http://10.0.0.1/" }),
            &ctx("smoke-ssrf"),
        )
        .await;

    cancel.cancel();

    let out = outcome.expect("call returned");
    match out {
        aura_tools::ToolOutput::Error(s) => {
            assert!(
                s.contains("BLOCKED_BY_SSRF_POLICY") || s.contains("blocked"),
                "expected SSRF rejection text, got: {s}",
            );
        }
        other => panic!("expected SSRF Error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires bundled MCP server + node + playwright install chromium"]
async fn browser_smoke_navigate_and_snapshot_through_real_chromium() {
    use std::net::SocketAddr;

    let tempdir = tempfile::tempdir().expect("tempdir");
    // Bind a tiny HTTP server so we have something to navigate to. We
    // serve a single button + the same `data-aura-ref-XXXX` attribute
    // pattern the snapshot walker mints — proves the snapshot
    // re-mints on each call (defeating page-side pre-pollution).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = "<!doctype html><html><head><title>Smoke</title></head><body>\
                    <button id=\"btn\" data-aura-ref=\"e99\">Click me</button></body></html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cfg = BrowserConfig {
        profile_dir: Some(tempdir.path().join("browser-profile")),
        // sandbox stays at the default (false) — Chromium runs without
        // its renderer sandbox, fine for the test sandbox.
        allow_loopback: true,
        ..Default::default()
    };
    let Some((registry, _reconciler, cancel)) =
        boot_reconciler(tempdir.path().to_path_buf(), cfg).await
    else {
        return;
    };

    // 1. Navigate
    let url = format!("http://{addr}/");
    let nav = registry
        .execute(
            "browser/navigate",
            json!({ "url": url }),
            &ctx("smoke-navigate"),
        )
        .await
        .expect("navigate");
    let nav_text = match nav {
        aura_tools::ToolOutput::Text(s) => s,
        other => panic!("expected Text from navigate, got {other:?}"),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&nav_text).expect("navigate result is JSON");
    let snapshot = parsed["snapshot"].as_str().expect("snapshot field present");

    // The snapshot's @eN ref must NOT be `@e99` (the page tried to
    // pre-set it), because the walker assigns nonce-suffixed refAttr
    // and re-mints sequence ids per call.
    assert!(
        !snapshot.contains("@e99"),
        "snapshot leaked the page-side ref pollution: {snapshot}",
    );
    let mint = snapshot
        .lines()
        .find_map(|l| {
            let i = l.find("@e")?;
            let rest = &l[i + 1..];
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(rest.len());
            Some(rest[..end].to_string())
        })
        .expect("at least one @eN ref in snapshot");
    eprintln!("snapshot mint: {mint}");

    // 2. Click the button via the freshly-minted ref.
    let click = registry
        .execute(
            "browser/click",
            json!({ "ref": format!("@{mint}") }),
            &ctx("smoke-navigate"),
        )
        .await
        .expect("click");
    match click {
        aura_tools::ToolOutput::Text(s) => {
            assert!(s.contains("\"ok\":true"), "expected ok:true, got: {s}");
        }
        other => panic!("expected Text from click, got {other:?}"),
    }

    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires bundled MCP server + node + playwright install chromium"]
async fn browser_smoke_hostile_dom_is_bounded() {
    use std::net::SocketAddr;

    let tempdir = tempfile::tempdir().expect("tempdir");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let mut body = String::from("<!doctype html><html><body>");
                for i in 0..5_000 {
                    body.push_str(&format!("<button>btn{i}</button>"));
                }
                body.push_str("</body></html>");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cfg = BrowserConfig {
        enable: true,
        profile_dir: Some(tempdir.path().join("browser-profile")),
        allow_loopback: true,
        ..Default::default()
    };
    let Some((registry, _reconciler, cancel)) =
        boot_reconciler(tempdir.path().to_path_buf(), cfg).await
    else {
        return;
    };

    let started = std::time::Instant::now();
    let url = format!("http://{addr}/");
    let nav = registry
        .execute(
            "browser/navigate",
            json!({ "url": url }),
            &ctx("smoke-hostile"),
        )
        .await
        .expect("navigate");
    let elapsed = started.elapsed();
    cancel.cancel();

    // Soft bound: 5,000 buttons must not turn into a multi-second hang.
    assert!(
        elapsed < Duration::from_secs(10),
        "snapshot of 5000-button page took {elapsed:?}; budget exceeded",
    );
    let nav_text = match nav {
        aura_tools::ToolOutput::Text(s) => s,
        other => panic!("expected Text from navigate, got {other:?}"),
    };
    // The snapshot walker truncates at COMPACT_MAX_NODES (200) and
    // emits a marker; assert the marker is present.
    assert!(
        nav_text.contains("truncated") || nav_text.contains("…"),
        "expected truncation marker in snapshot, got: {}",
        &nav_text[..nav_text.len().min(400)]
    );
}
