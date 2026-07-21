/// One structured log record a sidecar writes to stdout/stderr as
/// NDJSON (`{"level": "...", "msg": "...", "target"?: "..."}`), matching
/// the channel SDK's default logger (`sidecars/sdk/channel-ts/src/logger.ts`).
///
/// Two independent drains try-parse each child line as this shape to
/// recover the sidecar's declared level, falling back to plain text on
/// any parse failure: the channel-sidecar supervisor's per-channel pipe
/// pump (`baybo_gateway::sidecar::supervisor`) and the MCP stdio stderr
/// drain (`crate::mcp::transport::drain_mcp_stderr`). It lives here so
/// both consumers share one definition instead of each carrying a
/// divergent copy of the wire shape.
#[derive(serde::Deserialize)]
pub struct SidecarLogLine {
    pub level: String,
    pub msg: String,
    #[serde(default)]
    pub target: Option<String>,
}
