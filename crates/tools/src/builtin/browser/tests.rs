//! Cross-cutting unit tests for the browser tool family. Pure
//! schema / approval-policy / call-label coverage — no network,
//! no real Playwright. Round-trip behaviour through the WS hop
//! is covered by `aura-gateway`'s `tests/tool_ws.rs`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::builtin::browser::client::{BrowserRpcError, BrowserSidecarClient};
use crate::builtin::browser::{
    BrowserBackTool, BrowserCdpTool, BrowserClickTool, BrowserConsoleTool, BrowserDialogTool,
    BrowserGetImagesTool, BrowserNavigateTool, BrowserPressTool, BrowserScreenshotTool,
    BrowserScrollTool, BrowserSnapshotTool, BrowserTypeTool,
};
use crate::{ResourceAccess, Tool};

/// Stub client that fails every RPC. Lets us instantiate tools so
/// we can poke their `parameters_schema` / `accessed_resources` /
/// `call_label` without any real WS plumbing.
struct DeadClient;

#[async_trait]
impl BrowserSidecarClient for DeadClient {
    async fn call(
        &self,
        _method: &str,
        _params: Value,
        _context_id: &str,
        _timeout: Duration,
        _cancellation_token: CancellationToken,
    ) -> Result<Value, BrowserRpcError> {
        Err(BrowserRpcError::Disconnected)
    }
}

fn dead() -> Arc<dyn BrowserSidecarClient> {
    Arc::new(DeadClient)
}

fn blob_store() -> Arc<dyn aura_storage::BlobStore> {
    Arc::new(aura_storage::test_support::MemoryBlobStore::new())
}

// ---------- schema coverage ----------

#[test]
fn every_tool_has_a_well_formed_schema() {
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(BrowserNavigateTool::new(dead())),
        Box::new(BrowserSnapshotTool::new(dead())),
        Box::new(BrowserClickTool::new(dead())),
        Box::new(BrowserTypeTool::new(dead())),
        Box::new(BrowserScrollTool::new(dead())),
        Box::new(BrowserBackTool::new(dead())),
        Box::new(BrowserPressTool::new(dead())),
        Box::new(BrowserGetImagesTool::new(dead())),
        Box::new(BrowserScreenshotTool::new(dead(), blob_store())),
        Box::new(BrowserConsoleTool::new(dead())),
        Box::new(BrowserDialogTool::new(dead())),
        Box::new(BrowserCdpTool::new(dead())),
    ];
    for t in &tools {
        let schema = t.parameters_schema();
        assert_eq!(
            schema["type"],
            "object",
            "{}: schema.type must be 'object'",
            t.name()
        );
        assert!(
            schema["properties"].is_object(),
            "{}: schema.properties must be an object, got {schema}",
            t.name()
        );
        assert!(
            schema["required"].is_array(),
            "{}: schema.required must be an array, got {schema}",
            t.name()
        );
    }
}

#[test]
fn every_tool_name_starts_with_browser_prefix() {
    let names = [
        BrowserNavigateTool::new(dead()).name().to_string(),
        BrowserSnapshotTool::new(dead()).name().to_string(),
        BrowserClickTool::new(dead()).name().to_string(),
        BrowserTypeTool::new(dead()).name().to_string(),
        BrowserScrollTool::new(dead()).name().to_string(),
        BrowserBackTool::new(dead()).name().to_string(),
        BrowserPressTool::new(dead()).name().to_string(),
        BrowserGetImagesTool::new(dead()).name().to_string(),
        BrowserScreenshotTool::new(dead(), blob_store())
            .name()
            .to_string(),
        BrowserConsoleTool::new(dead()).name().to_string(),
        BrowserDialogTool::new(dead()).name().to_string(),
        BrowserCdpTool::new(dead()).name().to_string(),
    ];
    for n in &names {
        assert!(n.starts_with("browser_"), "tool name '{n}' missing prefix");
    }
    // `tools()` factory returns 12 entries — sanity check we listed
    // every one above.
    assert_eq!(
        names.len(),
        crate::builtin::browser::tools(dead(), blob_store()).len()
    );
}

// ---------- navigate: SSRF + approval ----------

#[test]
fn navigate_hostname_does_not_prompt() {
    let tool = BrowserNavigateTool::new(dead());
    let acc = tool.accessed_resources(&json!({ "url": "https://example.com/" }));
    assert!(
        acc.is_empty(),
        "hostname URL must not declare access (resolver guards SSRF), got {acc:?}"
    );
}

#[test]
fn navigate_public_ip_prompts_for_http() {
    let tool = BrowserNavigateTool::new(dead());
    let acc = tool.accessed_resources(&json!({ "url": "https://1.2.3.4/" }));
    assert!(
        matches!(acc.first(), Some(ResourceAccess::Http { host }) if host == "1.2.3.4"),
        "literal public IP must declare Http access, got {acc:?}"
    );
}

#[test]
fn navigate_private_ip_does_not_prompt() {
    // Validator rejects these pre-RPC, so prompting is pure noise.
    let tool = BrowserNavigateTool::new(dead());
    for url in [
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://172.16.0.1/",
        "http://169.254.169.254/",
        "http://127.0.0.1/",
        "http://[::1]/",
    ] {
        let acc = tool.accessed_resources(&json!({ "url": url }));
        assert!(
            acc.is_empty(),
            "{url} resolves into a blocked range; must not prompt, got {acc:?}",
        );
    }
}

#[test]
fn navigate_call_label_is_truncated_url() {
    let tool = BrowserNavigateTool::new(dead());
    let label = tool
        .call_label(&json!({ "url": "https://example.com/path" }))
        .expect("label");
    assert_eq!(label, "https://example.com/path");
    let long = "https://example.com/".to_string() + &"a".repeat(200);
    let lbl = tool.call_label(&json!({ "url": long })).unwrap();
    assert!(lbl.ends_with('…'));
}

// ---------- console: conditional approval ----------

#[test]
fn console_without_expression_does_not_prompt() {
    let tool = BrowserConsoleTool::new(dead());
    assert!(tool.accessed_resources(&json!({})).is_empty());
    assert!(
        tool.accessed_resources(&json!({ "clear": true }))
            .is_empty()
    );
    assert!(
        tool.accessed_resources(&json!({ "expression": "" }))
            .is_empty(),
        "empty expression must not prompt"
    );
}

#[test]
fn console_with_expression_prompts_exec_command() {
    let tool = BrowserConsoleTool::new(dead());
    let acc = tool.accessed_resources(&json!({ "expression": "document.cookie" }));
    assert!(
        matches!(
            acc.first(),
            Some(ResourceAccess::ExecCommand { command }) if command.starts_with("browser_console:") && command.contains("document.cookie")
        ),
        "expression must trigger an ExecCommand prompt, got {acc:?}"
    );
}

#[test]
fn console_long_expression_label_is_truncated() {
    let tool = BrowserConsoleTool::new(dead());
    let long = "x".repeat(500);
    let lbl = tool.call_label(&json!({ "expression": long })).unwrap();
    // Truncated to 80 chars + ellipsis.
    assert!(lbl.ends_with('…'));
    assert!(lbl.chars().count() <= 81);
}

// ---------- cdp: always prompt ----------

#[test]
fn cdp_always_prompts_regardless_of_method() {
    let tool = BrowserCdpTool::new(dead());
    for method in [
        "Network.getAllCookies",
        "Page.reload",
        "Runtime.evaluate",
        "Browser.getVersion",
    ] {
        let acc = tool.accessed_resources(&json!({ "method": method }));
        assert!(
            matches!(
                acc.first(),
                Some(ResourceAccess::ExecCommand { command }) if command.contains(method)
            ),
            "cdp method {method} must always prompt, got {acc:?}",
        );
    }
}

#[test]
fn cdp_missing_method_still_prompts() {
    // We never bypass the gate, even when params are malformed —
    // execute() will reject the bad params, but accessed_resources
    // is consulted before execute() and must fail closed.
    let tool = BrowserCdpTool::new(dead());
    let acc = tool.accessed_resources(&json!({}));
    assert!(
        matches!(acc.first(), Some(ResourceAccess::ExecCommand { .. })),
        "cdp must prompt even when method is absent, got {acc:?}"
    );
}

// ---------- non-prompting tools ----------

#[test]
fn passive_tools_do_not_prompt() {
    // Click / type / scroll / back / press / dialog / snapshot /
    // screenshot / get_images all rely on a prior `navigate` having
    // fired the Http approval. None of them declares access on
    // their own.
    let tools: Vec<Box<dyn Tool>> = vec![
        Box::new(BrowserSnapshotTool::new(dead())),
        Box::new(BrowserClickTool::new(dead())),
        Box::new(BrowserTypeTool::new(dead())),
        Box::new(BrowserScrollTool::new(dead())),
        Box::new(BrowserBackTool::new(dead())),
        Box::new(BrowserPressTool::new(dead())),
        Box::new(BrowserGetImagesTool::new(dead())),
        Box::new(BrowserScreenshotTool::new(dead(), blob_store())),
        Box::new(BrowserDialogTool::new(dead())),
    ];
    for t in &tools {
        let acc = t.accessed_resources(&json!({}));
        assert!(
            acc.is_empty(),
            "{}: passive tool must not declare access, got {acc:?}",
            t.name()
        );
    }
}
