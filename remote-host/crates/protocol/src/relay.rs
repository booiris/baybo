//! Relay (WebSocket) wire surface: the pairing + content rendezvous and the
//! gateway control channel.

use serde::{Deserialize, Serialize};

// Keep the original public path source-compatible after the header became a
// relay + push contract at the protocol root.
#[doc(hidden)]
pub use crate::REMOTE_API_KEY_HEADER;

/// Header on a phone's [`CONTENT_JOIN`] dial declaring the leg's traffic class
/// ([`LegClass`]). The phone owns the join, so the phone authors the class; the
/// relay copies it (it never authors it) onto the matching gateway host leg so
/// both spliced halves meter the same way. Absent or unparseable ⇒ [`LegClass::Chat`].
pub const RELAY_LEG_CLASS_HEADER: &str = "x-relay-leg-class";

/// The traffic class of a content leg, declared by the phone on its
/// [`CONTENT_JOIN`] dial ([`RELAY_LEG_CLASS_HEADER`]) and relayed to the gateway
/// in [`ControlSignal::OpenDataLeg`]. `Chat` runs the live frame loop, `Api` runs
/// an interactive API tunnel, and `Blob` runs the same tunnel on background
/// bandwidth for bulk transfer. Defaults to `Chat` for backward-compatible decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegClass {
    /// Interactive chat traffic; the full `wire::Frame` loop.
    #[default]
    Chat,
    /// Small API tunnel traffic; interactive bandwidth.
    Api,
    /// Bulk blob traffic; API tunnel over background bandwidth.
    Blob,
}

impl LegClass {
    /// Parse the [`RELAY_LEG_CLASS_HEADER`] value; absent or unknown values are
    /// `Chat` — the relay must never fail a join on a bad class hint, and an
    /// unknown value is the safe interactive default.
    pub fn from_header_value(value: &str) -> Self {
        match value {
            "api" => LegClass::Api,
            "blob" => LegClass::Blob,
            _ => LegClass::Chat,
        }
    }

    /// The header/wire string for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            LegClass::Chat => "chat",
            LegClass::Api => "api",
            LegClass::Blob => "blob",
        }
    }
}

/// Route templates (axum-style `{param}` tokens). The server registers these
/// directly; clients build a concrete URL via the helpers below.
///
/// The pairing routes key on the **public `rendezvous_id`** (a UUID) — the only
/// pairing value C ever sees. It routes nothing secret: the QR secret (the Noise
/// PSK) never reaches C, so C cannot complete the handshake with either side.
pub const PAIR_HOST: &str = "/pair/host/{rendezvous_id}";
pub const PAIR_JOIN: &str = "/pair/join/{rendezvous_id}";
pub const CONTROL: &str = "/control";
pub const CONTENT_JOIN: &str = "/content/join/{relay_node_id}";
pub const CONTENT_HOST: &str = "/content/host/{relay_key}";

/// First binary-JSON frame on [`CONTROL`] (gateway → C): the gateway names
/// itself by `relay_node_id`. Admission rides the `x-remote-api-key` header on the
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
    /// Tell the gateway to open a content data leg under `relay_key`. `class`
    /// (defaulted for a pre-class sender) tells the gateway whether to run the
    /// chat frame loop ([`LegClass::Chat`]) or the API tunnel
    /// ([`LegClass::Api`] / [`LegClass::Blob`]) over the leg, and to meter it
    /// accordingly.
    OpenDataLeg {
        relay_key: String,
        #[serde(default)]
        class: LegClass,
    },
}

/// `{base}/pair/host/{rendezvous_id}` — the gateway's pairing host leg.
pub fn pair_host_url(base: &str, rendezvous_id: &str) -> String {
    crate::join(base, &PAIR_HOST.replace("{rendezvous_id}", rendezvous_id))
}
/// `{base}/pair/join/{rendezvous_id}` — the app's pairing join leg.
pub fn pair_join_url(base: &str, rendezvous_id: &str) -> String {
    crate::join(base, &PAIR_JOIN.replace("{rendezvous_id}", rendezvous_id))
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
