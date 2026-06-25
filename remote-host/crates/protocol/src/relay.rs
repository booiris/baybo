//! Relay (WebSocket) wire surface: the pairing + content rendezvous and the
//! gateway control channel.

use serde::{Deserialize, Serialize};

/// Admission header the gateway presents on the host-side WS upgrades
/// ([`PAIR_HOST`], [`CONTENT_HOST`]); its value must be an admitted instance key.
/// The app/phone-side routes carry no credential.
pub const INSTANCE_KEY_HEADER: &str = "x-instance-key";

/// Route templates (axum-style `{param}` tokens). The server registers these
/// directly; clients build a concrete URL via the helpers below.
pub const PAIR_HOST: &str = "/pair/host/{code}";
pub const PAIR_JOIN: &str = "/pair/join/{code}";
pub const CONTROL: &str = "/control";
pub const CONTENT_JOIN: &str = "/content/join/{relay_node_id}";
pub const CONTENT_HOST: &str = "/content/host/{relay_key}";

/// First binary-JSON frame on [`CONTROL`] (gateway → C): the gateway names
/// itself by `relay_node_id`. Admission rides the `x-instance-key` header on the
/// dial (the shared pre-layer), like every other route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHello {
    pub relay_node_id: String,
}

/// C → gateway signal, pushed over [`CONTROL`] as binary-JSON
/// (`{"t":"open_data_leg","relay_key":"…"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlSignal {
    /// Tell the gateway to open a content data leg under `relay_key`.
    OpenDataLeg { relay_key: String },
}

/// `{base}/pair/host/{code}` — the gateway's SPAKE2 host leg.
pub fn pair_host_url(base: &str, code: &str) -> String {
    crate::join(base, &PAIR_HOST.replace("{code}", code))
}
/// `{base}/pair/join/{code}` — the app's SPAKE2 join leg.
pub fn pair_join_url(base: &str, code: &str) -> String {
    crate::join(base, &PAIR_JOIN.replace("{code}", code))
}
/// `{base}/control` — the gateway's persistent control connection.
pub fn control_url(base: &str) -> String {
    crate::join(base, CONTROL)
}
/// `{base}/content/join/{relay_node_id}` — the app's content join leg.
pub fn content_join_url(base: &str, relay_node_id: &str) -> String {
    crate::join(
        base,
        &CONTENT_JOIN.replace("{relay_node_id}", relay_node_id),
    )
}
/// `{base}/content/host/{relay_key}` — the gateway's content data leg.
pub fn content_host_url(base: &str, relay_key: &str) -> String {
    crate::join(base, &CONTENT_HOST.replace("{relay_key}", relay_key))
}
