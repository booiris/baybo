//! Real-sidecar smoke test for the browser tool family.
//!
//! Gated `#[ignore]` because it requires:
//!   - `pnpm install` populating `tool-src/browser/node_modules`
//!   - `pnpm --filter @aura/tool-browser bundle` producing a real
//!     `bundle.mjs` (cargo build does this automatically once
//!     pnpm has hydrated the workspace)
//!   - `bun` on PATH
//!   - For navigation success cases (not asserted here), Playwright
//!     must also have installed Chromium:
//!     `pnpm --filter @aura/tool-browser exec playwright install chromium`
//!
//! Run manually:
//!   cargo test -p aura-gateway --test tool_ws_smoke -- --ignored
//!
//! What this test does:
//!   1. Spawn the real bundled sidecar via `bun bundle.mjs`.
//!   2. Verify it connects and registers over the live `/v1/tool-ws`.
//!   3. Drive a `navigate` call against a literal-private-IP URL and
//!      assert the in-sidecar SSRF guard rejects it BEFORE Chromium
//!      is launched (`assertSafeUrl` runs before `manager.acquire`).
//!
//! That last assertion is the one that matters: it proves the
//! Node-side network_policy port actually executes on the wire,
//! catches what the syntactic Rust pre-check might miss, and
//! surfaces a structured error back through the WS layer.
//!
//! The success-path navigation (to a real loopback HTTP server with
//! `AURA_BROWSER_ALLOW_LOOPBACK=1`) is intentionally out of scope
//! here — that's a manual integration step the operator runs after
//! `playwright install chromium`.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use aura_gateway::channel_listener::ChannelServer;
use aura_gateway::sidecar::SidecarRuntime;
use aura_gateway::test_support::build_test_deps;
use aura_gateway::tool_ws::WsBrowserSidecarClient;
use aura_gateway::{ChannelTokenTable, ClientIdentity};
use aura_tools::builtin::browser::{BrowserRpcError, BrowserSidecarClient};
use serde_json::json;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const TOOL_BROWSER_LABEL: &str = "tool/browser";

/// Resolve `node` (preferred for the esbuild-bundled tool sidecar)
/// from `AURA_NODE_BIN` env or fall back to `node` on PATH.
/// Returns `None` so the test skips cleanly when node isn't around.
///
/// We deliberately prefer node over bun for the tool sidecar: the
/// bundle is built with `esbuild --target=node`, and bun's runtime
/// substitutions for `ws` confuse the upgraded WebSocket handshake
/// against the gateway's axum endpoint (`Unexpected server response:
/// 101`). Channel sidecars use `bun --target=bun` and stay on bun;
/// only the tool sidecar bundle is node-flavoured.
fn node_path() -> Option<std::path::PathBuf> {
    if let Some(s) = std::env::var_os("AURA_NODE_BIN") {
        let p = std::path::PathBuf::from(s);
        if p.exists() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("node");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires bundled tool-src/browser sidecar (pnpm install) + bun on PATH"]
async fn browser_smoke_ssrf_guard_fires_via_real_sidecar() {
    // Pre-flight: skip with a clear reason when the bundle or bun
    // aren't around. We don't want this test to fail for missing-
    // dep reasons; that would punish anyone running `cargo test
    // --ignored` without the full TS toolchain.
    //
    // The bundle is fully self-contained (esbuild bundles
    // playwright + ws + msgpackr inline) so we run from the
    // materialised cache path — same shape as production.
    let runtime = match SidecarRuntime::install() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("smoke test skipped: SidecarRuntime::install failed ({e})");
            return;
        }
    };
    let Some(bundle_path) = runtime.bundle_for("browser") else {
        eprintln!(
            "smoke test skipped: browser bundle not embedded — \
             run `pnpm install && pnpm --filter @aura/tool-browser bundle && cargo build`",
        );
        return;
    };
    let Some(node) = node_path() else {
        eprintln!("smoke test skipped: `node` not found on PATH (override with AURA_NODE_BIN)");
        return;
    };

    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let admin_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut tg = build_test_deps(admin_bind).await;
    let ws_client = WsBrowserSidecarClient::new();
    tg.deps.tool_ws_client = Some(ws_client.clone());
    tg.deps.tool_ws_sidecar_label = TOOL_BROWSER_LABEL.to_string();

    let channel_tokens: ChannelTokenTable = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Mint the tool/browser-bound token. Held via `_handle` for the
    // lifetime of the test.
    let _handle = channel_tokens.mint(ClientIdentity {
        pid: 0,
        label: TOOL_BROWSER_LABEL.into(),
        bound_channel_type: None,
    });
    let token = _handle.token().to_string();

    // Use a fresh profile dir under tempdir so the smoke test never
    // touches the user's real cache.
    let profile_dir = tempdir.path().join("browser-profile");
    std::fs::create_dir_all(&profile_dir).expect("mk profile dir");

    let mut cmd = Command::new(&node);
    cmd.arg(bundle_path);
    cmd.env(
        "AURA_TOOL_WS_URL",
        format!("ws://127.0.0.1:{port}/v1/tool-ws"),
    );
    cmd.env("AURA_TOOL_WS_TOKEN", &token);
    cmd.env("AURA_TOOL_SIDECAR_ID", "browser");
    cmd.env("AURA_BROWSER_PROFILE_DIR", &profile_dir);
    // Default: sandbox stays ON. Smoke test does not navigate to a
    // real page, so we never launch chromium and the sandbox
    // setting is moot — but we leave it explicit here so a future
    // extension that opens a real page inherits the safe default.
    cmd.env_remove("AURA_BROWSER_NO_SANDBOX");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn bun sidecar");

    // Pump child stderr to test stderr so unexpected sidecar errors
    // surface in the test log instead of silently dying.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stderr] {line}");
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stdout] {line}");
            }
        });
    }

    // Wait up to 15s for the sidecar to come up + register. Real
    // bundle on a cold cache can take a few seconds; CI loaded
    // hosts can be slower.
    let attached = wait_until(Duration::from_secs(15), Duration::from_millis(50), || {
        ws_client.is_attached()
    })
    .await;
    if !attached {
        let _ = child.start_kill();
        let _ = child.wait().await;
        shutdown.trigger();
        let _ = server_handle.await;
        panic!("sidecar never registered with /v1/tool-ws within 15s");
    }

    // Drive a `navigate` to a literal RFC1918 IP. The Rust-side
    // `validate_url_with` would already reject this before we send
    // — but the assertion that matters here is end-to-end: the
    // ENTIRE chain (Rust tool → WS frame → sidecar handler →
    // assertSafeUrl → BLOCKED_BY_SSRF_POLICY → RpcError) is alive.
    // Skip the Rust pre-check by invoking the WS client directly.
    let cancel = CancellationToken::new();
    let outcome = ws_client
        .call(
            "navigate",
            json!({ "url": "http://10.0.0.1/" }),
            "smoke-session",
            Duration::from_secs(10),
            cancel,
        )
        .await;

    let err = match outcome {
        Ok(v) => panic!("expected SSRF block, sidecar returned ok: {v}"),
        Err(e) => e,
    };
    match err {
        BrowserRpcError::Sidecar { code, message } => {
            assert!(
                code.contains("BLOCKED_BY_SSRF_POLICY") || message.contains("blocked"),
                "expected BLOCKED_BY_SSRF_POLICY, got code={code} message={message}",
            );
        }
        other => panic!("expected Sidecar SSRF error, got {other:?}"),
    }

    // Tear down.
    let _ = child.start_kill();
    let _ = child.wait().await;
    shutdown.trigger();
    let _ = server_handle.await;
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
#[ignore = "requires bundled tool-src/browser sidecar + bun + playwright install chromium"]
async fn browser_smoke_navigate_and_snapshot_through_real_chromium() {
    let runtime = match SidecarRuntime::install() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("smoke test skipped: SidecarRuntime::install failed ({e})");
            return;
        }
    };
    let Some(bundle_path) = runtime.bundle_for("browser") else {
        eprintln!(
            "smoke test skipped: browser bundle not embedded — \
             run `pnpm install && pnpm --filter @aura/tool-browser bundle && cargo build`",
        );
        return;
    };
    let Some(node) = node_path() else {
        eprintln!("smoke test skipped: node not on PATH");
        return;
    };

    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();
    let admin_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut tg = build_test_deps(admin_bind).await;
    let ws_client = WsBrowserSidecarClient::new();
    tg.deps.tool_ws_client = Some(ws_client.clone());
    tg.deps.tool_ws_sidecar_label = TOOL_BROWSER_LABEL.to_string();

    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let _handle = channel_tokens.mint(ClientIdentity {
        pid: 0,
        label: TOOL_BROWSER_LABEL.into(),
        bound_channel_type: None,
    });
    let token = _handle.token().to_string();

    // Bind a tiny HTTP server on 127.0.0.1 serving a single page
    // with one button. The agent will navigate, snapshot, and click
    // — exercising the full ref-injection + locator path.
    let html_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let html_port = html_listener.local_addr().unwrap().port();
    let html_handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = html_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = "<!doctype html><html><head><title>Smoke Page</title></head>\
                            <body><h1>Aura smoke test</h1>\
                            <button id=\"go\" data-aura-ref=\"PAGE_PRE_POLLUTION\">Click me</button>\
                            </body></html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let profile_dir = tempdir.path().join("browser-profile");
    std::fs::create_dir_all(&profile_dir).expect("mk profile dir");

    let mut cmd = Command::new(&node);
    cmd.arg(bundle_path);
    cmd.env(
        "AURA_TOOL_WS_URL",
        format!("ws://127.0.0.1:{port}/v1/tool-ws"),
    );
    cmd.env("AURA_TOOL_WS_TOKEN", &token);
    cmd.env("AURA_TOOL_SIDECAR_ID", "browser");
    cmd.env("AURA_BROWSER_PROFILE_DIR", &profile_dir);
    // Test-only: allow loopback so we can navigate to 127.0.0.1.
    cmd.env("AURA_BROWSER_ALLOW_LOOPBACK", "1");
    // Enable --no-sandbox in test contexts where the chromium
    // sandbox can't initialise (containers without user-namespace
    // privileges, CI runners, etc.). On a vanilla dev machine this
    // is harmless because the test runs as a non-root user and
    // chromium's sandbox initialises normally.
    cmd.env("AURA_BROWSER_NO_SANDBOX", "1");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn bun sidecar");

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stderr] {line}");
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stdout] {line}");
            }
        });
    }

    // Sidecar register can take a few seconds; chromium launch
    // itself is lazy (first navigate), so the register itself is
    // fast.
    if !wait_until(Duration::from_secs(15), Duration::from_millis(50), || {
        ws_client.is_attached()
    })
    .await
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
        shutdown.trigger();
        let _ = server_handle.await;
        html_handle.abort();
        panic!("sidecar never registered within 15s");
    }

    // Navigate. Chromium boot + DNS check + page load can take
    // ~10s on a cold cache.
    let cancel = CancellationToken::new();
    let nav = ws_client
        .call(
            "navigate",
            json!({ "url": format!("http://127.0.0.1:{html_port}/") }),
            "smoke-session",
            Duration::from_secs(60),
            cancel.clone(),
        )
        .await
        .expect("navigate failed");

    let snapshot = nav["snapshot"]
        .as_str()
        .expect("navigate result missing snapshot string");
    eprintln!("\n=== snapshot ===\n{snapshot}\n=== end snapshot ===\n");
    assert!(
        snapshot.contains("Aura smoke test"),
        "snapshot must include the H1 text we served, got:\n{snapshot}",
    );
    assert!(
        snapshot.contains("Click me"),
        "snapshot must include the button label, got:\n{snapshot}",
    );

    // Find the @eN ref the snapshot assigned to the button. The
    // button's accessible name is the label text "Click me".
    let button_ref = extract_ref_for(snapshot, "Click me")
        .unwrap_or_else(|| panic!("could not find ref for button in snapshot:\n{snapshot}"));
    eprintln!("button_ref = {button_ref}");

    // The page pre-set `data-aura-ref="PAGE_PRE_POLLUTION"` on the
    // button; the per-context nonce-suffixed attribute name means
    // that pre-set attribute is irrelevant (different attribute
    // name) and the locator we resolve via @eN must still hit the
    // real button.
    let click = ws_client
        .call(
            "click",
            json!({ "ref": button_ref }),
            "smoke-session",
            Duration::from_secs(10),
            cancel,
        )
        .await
        .expect("click failed");
    assert_eq!(click["ok"], true, "click result: {click}");

    // Tear down.
    let _ = child.start_kill();
    let _ = child.wait().await;
    shutdown.trigger();
    let _ = server_handle.await;
    html_handle.abort();
}

/// Walk the snapshot text looking for a line whose name field
/// contains `needle`; return the `eN` ref. Loose parsing — if the
/// snapshot format ever changes, the test fails clearly with the
/// actual snapshot dumped above.
fn extract_ref_for(snapshot: &str, needle: &str) -> Option<String> {
    for line in snapshot.lines() {
        if !line.contains(needle) {
            continue;
        }
        // Lines look like `  [button @e3] "Click me"`.
        let at = line.find('@')?;
        let rest = &line[at + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .unwrap_or(rest.len());
        return Some(rest[..end].to_string());
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires bundled tool-src/browser sidecar + node + playwright install chromium"]
async fn browser_smoke_hostile_dom_is_bounded() {
    // Adversarial regression: a page with a huge DOM must not hang
    // the sidecar. The walker now self-bounds at COMPACT_MAX_NODES
    // (200 today) so we expect the snapshot to come back fast and
    // truncated. Without the fix this test would either time out
    // (innerText layout cost) or return a multi-megabyte snapshot.
    let runtime = match SidecarRuntime::install() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("smoke test skipped: {e}");
            return;
        }
    };
    let Some(bundle_path) = runtime.bundle_for("browser") else {
        eprintln!("smoke test skipped: bundle missing");
        return;
    };
    let Some(node) = node_path() else {
        eprintln!("smoke test skipped: node not on PATH");
        return;
    };

    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();
    let admin_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut tg = build_test_deps(admin_bind).await;
    let ws_client = WsBrowserSidecarClient::new();
    tg.deps.tool_ws_client = Some(ws_client.clone());
    tg.deps.tool_ws_sidecar_label = TOOL_BROWSER_LABEL.to_string();

    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let _handle = channel_tokens.mint(ClientIdentity {
        pid: 0,
        label: TOOL_BROWSER_LABEL.into(),
        bound_channel_type: None,
    });
    let token = _handle.token().to_string();

    // Synthesize a page with 5_000 buttons. Each one is a real
    // interactive element, so the walker would emit a node per
    // button if unbounded. With the bound at 200 the snapshot
    // should mention the truncation marker and skip the long tail.
    let html_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let html_port = html_listener.local_addr().unwrap().port();
    let html_handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = html_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let mut body = String::from(
                    "<!doctype html><html><head><title>Hostile DOM</title></head><body>",
                );
                for i in 0..5_000 {
                    body.push_str(&format!("<button>btn-{i}</button>"));
                }
                body.push_str("</body></html>");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let profile_dir = tempdir.path().join("browser-profile");
    std::fs::create_dir_all(&profile_dir).expect("mk profile dir");

    let mut cmd = Command::new(&node);
    cmd.arg(bundle_path);
    cmd.env(
        "AURA_TOOL_WS_URL",
        format!("ws://127.0.0.1:{port}/v1/tool-ws"),
    );
    cmd.env("AURA_TOOL_WS_TOKEN", &token);
    cmd.env("AURA_TOOL_SIDECAR_ID", "browser");
    cmd.env("AURA_BROWSER_PROFILE_DIR", &profile_dir);
    cmd.env("AURA_BROWSER_ALLOW_LOOPBACK", "1");
    cmd.env("AURA_BROWSER_NO_SANDBOX", "1");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn node sidecar");
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stderr] {line}");
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[sidecar/stdout] {line}");
            }
        });
    }

    if !wait_until(Duration::from_secs(15), Duration::from_millis(50), || {
        ws_client.is_attached()
    })
    .await
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
        shutdown.trigger();
        let _ = server_handle.await;
        html_handle.abort();
        panic!("sidecar never registered within 15s");
    }

    // 30s budget. Real Chromium boot is ~5–10s on a cold profile,
    // walking 200 nodes is cheap. If this exceeds 30s the bound
    // isn't actually working and we have a regression.
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let nav = ws_client
        .call(
            "navigate",
            json!({ "url": format!("http://127.0.0.1:{html_port}/") }),
            "hostile-session",
            Duration::from_secs(30),
            cancel,
        )
        .await
        .expect("navigate failed");
    let elapsed = started.elapsed();
    eprintln!("hostile-DOM navigate took {:?}", elapsed);

    let snapshot = nav["snapshot"]
        .as_str()
        .expect("navigate result missing snapshot string");
    let line_count = snapshot.lines().count();
    eprintln!(
        "snapshot: {} bytes, {} lines, first 400 chars:\n{}",
        snapshot.len(),
        line_count,
        &snapshot[..snapshot.len().min(400)],
    );

    // Truncation marker MUST appear — that's the whole point of
    // the bound.
    assert!(
        snapshot.contains("[... truncated"),
        "expected truncation marker in snapshot of 5k-node page; got {} bytes",
        snapshot.len(),
    );
    // The snapshot shouldn't include all 5k button labels.
    // COMPACT_MAX_NODES is 200 so we cap closer to that. Allow
    // generous margin to absorb the URL/TITLE/header lines and
    // structural nodes.
    assert!(
        line_count < 500,
        "snapshot has {line_count} lines — bound seems broken",
    );

    let _ = child.start_kill();
    let _ = child.wait().await;
    shutdown.trigger();
    let _ = server_handle.await;
    html_handle.abort();
}
