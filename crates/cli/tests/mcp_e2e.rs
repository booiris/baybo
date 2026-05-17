//! End-to-end tests for `aura mcp {add, list, get, remove}` against a
//! tmpdir workspace and an in-memory SecretVault. The full happy path
//! is exercised by stdio entries (HTTP entries hit the network in `add`
//! via the auth pre-check, so the network-free invariants are best
//! covered here with stdio + the no-probe variant of `list`/`get`).

use std::path::PathBuf;
use std::sync::Arc;

use aura_cli::cli::{Commands, McpCmd, McpTransportArg, TrustLevelArg};
use aura_cli::{ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;
use aura_security::test_support::MemorySecretStore;
use aura_security::{EncryptionKey, SecretVault};
use aura_tools::mcp::{McpFile, McpTransportConfig, vault_keys};

fn make_ctx(tmpdir: &tempfile::TempDir) -> aura_cli::CommandContext {
    let mut config = AuraConfig::default();
    config.workspace.path = tmpdir.path().to_string_lossy().into_owned();
    let vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
        Arc::new(MemorySecretStore::new()),
    ));
    ContextBuilder::new(Arc::new(config))
        .secret_vault(vault)
        .workspace(Arc::new(aura_workspace::WorkspaceManager::new(
            tmpdir.path().to_path_buf(),
        )))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
        .with_confirmed(false)
}

#[allow(clippy::too_many_arguments)]
fn add_cmd(
    name: &str,
    transport: McpTransportArg,
    command_or_url: &str,
    args: Vec<String>,
    envs: Vec<String>,
    headers: Vec<String>,
    trust: TrustLevelArg,
) -> Commands {
    Commands::Mcp {
        cmd: McpCmd::Add {
            transport,
            envs,
            headers,
            client_id: None,
            client_secret: false,
            callback_port: None,
            trust_level: trust,
            name: name.to_string(),
            command_or_url: command_or_url.to_string(),
            args,
        },
    }
}

#[tokio::test]
async fn stdio_roundtrip_persists_and_scrubs() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);

    // 1. add stdio entry
    let out = dispatch::run(
        &ctx,
        add_cmd(
            "demo",
            McpTransportArg::Stdio,
            "/bin/true",
            vec!["--flag".into(), "value".into()],
            vec!["KEY=secret-value".into()],
            vec![],
            TrustLevelArg::Trusted,
        ),
    )
    .await
    .expect("add should succeed");
    assert!(out.human.contains("Registered MCP server 'demo' (stdio)"));

    // .mcp.json should now contain the entry
    let file = McpFile::load(tmpdir.path()).await.unwrap();
    assert_eq!(file.servers.len(), 1);
    let entry = &file.servers[0];
    assert_eq!(entry.name, "demo");
    match &entry.transport {
        McpTransportConfig::Stdio { command, args } => {
            assert_eq!(command, "/bin/true");
            assert_eq!(args, &vec!["--flag".to_string(), "value".to_string()]);
        }
        _ => panic!("expected stdio transport"),
    }

    // env bag must be in the vault as JSON, not plaintext on disk
    let vault = ctx.secret_vault.as_ref().unwrap();
    let env_bag = vault
        .get_secret(&vault_keys::env_bag("demo"))
        .await
        .unwrap()
        .expect("env bag should be present");
    let env: serde_json::Value = serde_json::from_slice(env_bag.as_bytes()).unwrap();
    assert_eq!(env["KEY"], "secret-value");
    let json = std::fs::read_to_string(tmpdir.path().join("config").join(".mcp.json")).unwrap();
    assert!(
        !json.contains("secret-value"),
        ".mcp.json must not leak secrets"
    );

    // 2. list (no probe so we don't spawn `/bin/true`)
    let out = dispatch::run(
        &ctx,
        Commands::Mcp {
            cmd: McpCmd::List { no_probe: true },
        },
    )
    .await
    .unwrap();
    assert!(out.human.contains("demo"));
    assert!(out.human.contains("(probe skipped)"));

    // 3. get (no probe; secrets must be redacted in human output)
    let out = dispatch::run(
        &ctx,
        Commands::Mcp {
            cmd: McpCmd::Get {
                name: "demo".into(),
                no_probe: true,
            },
        },
    )
    .await
    .unwrap();
    assert!(out.human.contains("name: demo"));
    assert!(out.human.contains("transport: stdio"));
    assert!(out.human.contains("******** (encrypted)"));
    assert!(!out.human.contains("secret-value"));

    // 4. remove — drops the entry and scrubs every vault key
    let out = dispatch::run(
        &ctx,
        Commands::Mcp {
            cmd: McpCmd::Remove {
                name: "demo".into(),
                yes: true,
            },
        },
    )
    .await
    .unwrap();
    assert!(out.human.contains("Removed MCP server 'demo'"));

    let file = McpFile::load(tmpdir.path()).await.unwrap();
    assert!(file.servers.is_empty());

    for key in vault_keys::all_keys_for("demo") {
        let v = vault.get_secret(&key).await.unwrap();
        assert!(v.is_none(), "vault key {key} should be scrubbed");
    }
}

#[tokio::test]
async fn add_rejects_stdio_without_trusted_level() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);

    // Default trust level is `installed`; a stdio entry must be rejected
    // because connecting it spawns a subprocess (ExecCommand) which only
    // `trusted` may carry. This guards against Codex's finding that
    // `installed` stdio entries silently dropped `ExecCommand` from the
    // manifest while still spawning the process at reconcile time.
    let err = dispatch::run(
        &ctx,
        add_cmd(
            "rogue",
            McpTransportArg::Stdio,
            "/bin/true",
            vec![],
            vec![],
            vec![],
            TrustLevelArg::Installed,
        ),
    )
    .await
    .expect_err("installed stdio must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("trust_level=trusted"),
        "expected trust hint in {msg:?}"
    );

    // Confirm nothing was persisted.
    assert!(!tmpdir.path().join("config").join(".mcp.json").exists());
}

#[tokio::test]
async fn add_rejects_duplicate_name() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);

    dispatch::run(
        &ctx,
        add_cmd(
            "demo",
            McpTransportArg::Stdio,
            "/bin/true",
            vec![],
            vec![],
            vec![],
            TrustLevelArg::Trusted,
        ),
    )
    .await
    .expect("first add should succeed");

    let err = dispatch::run(
        &ctx,
        add_cmd(
            "demo",
            McpTransportArg::Stdio,
            "/bin/false",
            vec![],
            vec![],
            vec![],
            TrustLevelArg::Trusted,
        ),
    )
    .await
    .expect_err("duplicate add must fail");
    assert!(err.to_string().contains("already exists"));

    let file = McpFile::load(tmpdir.path()).await.unwrap();
    assert_eq!(file.servers.len(), 1);
    match &file.servers[0].transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "/bin/true", "first entry must be untouched");
        }
        _ => panic!("expected stdio transport"),
    }
}

#[tokio::test]
async fn http_with_static_header_persists_without_network_probe() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);

    // Passing --header skips both the auth pre-check (network) and the OAuth
    // flow, so this exercises the static-bearer happy path without leaving
    // the test process.
    dispatch::run(
        &ctx,
        add_cmd(
            "static-srv",
            McpTransportArg::Http,
            "https://mcp.example.com/mcp",
            vec![],
            vec![],
            vec!["Authorization: Bearer abc123".into()],
            TrustLevelArg::Trusted,
        ),
    )
    .await
    .expect("static-bearer add should not hit the network");

    let file = McpFile::load(tmpdir.path()).await.unwrap();
    assert_eq!(file.servers.len(), 1);
    assert!(file.servers[0].oauth.is_none());

    let vault = ctx.secret_vault.as_ref().unwrap();
    let header_bag = vault
        .get_secret(&vault_keys::header_bag("static-srv"))
        .await
        .unwrap()
        .expect("header bag must be persisted");
    let bag: serde_json::Value = serde_json::from_slice(header_bag.as_bytes()).unwrap();
    assert_eq!(bag["Authorization"], "Bearer abc123");

    // Bearer token must never appear in plaintext on disk.
    let on_disk = std::fs::read_to_string(tmpdir.path().join("config").join(".mcp.json")).unwrap();
    assert!(!on_disk.contains("abc123"));
}

#[tokio::test]
async fn remove_unknown_server_errors_cleanly() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);
    let err = dispatch::run(
        &ctx,
        Commands::Mcp {
            cmd: McpCmd::Remove {
                name: "ghost".into(),
                yes: true,
            },
        },
    )
    .await
    .expect_err("removing a missing server must fail");
    assert!(err.to_string().to_lowercase().contains("not found"));
}

#[tokio::test]
async fn get_unknown_server_errors() {
    let tmpdir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(&tmpdir);
    let err = dispatch::run(
        &ctx,
        Commands::Mcp {
            cmd: McpCmd::Get {
                name: "ghost".into(),
                no_probe: true,
            },
        },
    )
    .await
    .expect_err("get on a missing server must fail");
    assert!(err.to_string().to_lowercase().contains("not found"));
}

// silence the unused import in some paths
#[allow(dead_code)]
fn _silence_unused() -> Option<PathBuf> {
    None
}
