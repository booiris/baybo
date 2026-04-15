# Reintroduce MCP Client Support

## Problem

MCP (Model Context Protocol) client support was removed from `aura-tools` and
`aura-config` as an interim cleanup. The `rmcp`-backed `McpTool`,
`McpToolProvider`, transport types, the `tools.mcp_servers[]` config section,
and their validators/tests are all gone. MCP has to be re-added once the Tool
system design is finalized — the existing abstraction was premature and needs
to land on top of the new tool governance model, not alongside a legacy one.

## What Was Removed (so it can be put back)

- `crates/tools/src/mcp.rs` — `McpTransport`, `McpServerConfig`, `McpTool`,
  `McpError` (transport/protocol/connection variants).
- `crates/tools/src/mcp_provider.rs` — `McpToolProvider` with
  stdio/streamable-HTTP connect, tool discovery (`list_all_tools`), and
  `disconnect` cleanup.
- `crates/tools/src/registry.rs` — `mcp_tools` field, `register_mcp_tool`,
  `remove_mcp_tools_for_server`, plus MCP branches in `get`, `get_manifest`,
  and `tool_definitions`.
- `crates/tools/src/error.rs` — `ToolError::Mcp(String)`.
- `crates/tools/Cargo.toml` and workspace `Cargo.toml` — `rmcp` dependency
  entries.
- `crates/config/src/tools.rs` — `McpServerEntry`, `McpTransportConfig`,
  `SecretRequirementConfig`, `SecretAccessConfig`, `CapabilityConfig`, and the
  `mcp_servers` field on `ToolsConfig`.
- `crates/config/src/validate.rs` — `mcp_servers[]` per-item validation and
  the `validate_mcp_hosts_against_network` / `validate_trust_capability_matrix`
  cross-section checks (and their URL-parsing helpers).
- `crates/config/tests/config.rs` & `crates/config/tests/mirror_contract.rs`
  — MCP-specific test cases.
- `aura.example.json` — `tools.mcp_servers` field.

## Proposed Direction

Re-add MCP once the tool system (trait, routing, manifest/governance) is
finalized. At that point:

1. Reintroduce `rmcp` in the workspace and `aura-tools` dep lists.
2. Put `McpTool` and `McpToolProvider` back behind the finalized `Tool` trait.
3. Restore the config surface (`tools.mcp_servers[]`) and its mirrors/validators.
4. Replay the prior integration tests (name uniqueness, URL scheme,
   host/allowlist, loopback gate, trust/capability matrix) against the
   up-to-date shapes.
5. Wire MCP registration into bootstrap (still tracked in
   `docs/todo/config-wire-remaining-sections.md`, MCP row).

## Related

- `docs/modules/tools.md` — needs the MCP sections restored.
- `docs/modules/README.md` — needs the `rmcp` reference back on the `tools`
  dependency line.
- `docs/modules/cli.md` — `mcp serve` remains deferred.
- `docs/todo/config-wire-remaining-sections.md` — MCP wiring will come back
  alongside the re-added config surface.
