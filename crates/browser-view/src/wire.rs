//! Wire types shared by the gateway, the browser sidecar (`LinkUp` /
//! `LinkDown`, JSON over the link socket) and the web viewer (`ViewerUp` /
//! `ViewerDown`, JSON text over the WS). Numbers are `u32` / `u8` / `f64`
//! only so the generated TypeScript never needs `bigint`.
//!
//! TS emitters must send every field: an `Option` field is generated as a
//! required `T | null`, so `None` goes on the wire as `null`, never as an
//! omitted key.
//!
//! Deliberate departures from the plan's §4.4 sketch:
//! - `StreamStatus` has no `device_scale_factor`. `FrameHeader` carries the
//!   page scale and scroll offsets, which is all a viewer needs to map frame
//!   pixels to CSS px.
//! - `StreamState::Idle` is added for the lazy-launch window before the
//!   first browser exists (spike finding (a)).

use std::fmt;

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::limits::{
    BROWSER_LINK_PROTOCOL_VERSION, MAX_LINK_JSON_BYTES_U32, MAX_LINK_TARGETS, MAX_LINK_TEXT_CHARS,
    MAX_SCREENCAST_FRAME_BYTES_U32, MAX_TARGET_ID_CHARS, MAX_TARGET_URL_CHARS,
    VIEWER_PING_INTERVAL_MS,
};

/// How the sidecar reaches Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum BrowserMode {
    Host,
    Docker,
    CdpUrl,
}

/// A docker bring-up step, mirroring `DockerPhase` in
/// `sidecars/tool/browser/src/docker.ts` minus its `"ready"`: the sidecar maps
/// docker `"ready"` to [`BrowserPhase::Ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum DockerPhase {
    DockerChecking,
    DockerBuildingImage,
    DockerStartingContainer,
    DockerWaitingForCdp,
}

/// Lifecycle phase of the sidecar's browser, mirroring `InstallPhase` in
/// `sidecars/tool/browser/src/server.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum BrowserPhase {
    Idle,
    Installing {
        percent: u8,
    },
    Docker {
        phase: DockerPhase,
    },
    Ready,
    Recovering,
    /// `error` is clamped by the sidecar to `LinkLimits::max_text_chars`.
    Failed {
        error: String,
    },
}

/// CDP target id of a page.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct TargetId(String);

impl TargetId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Random id a sidecar process picks once at startup; a new value means the
/// sidecar restarted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct BootId(String);

impl BootId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One-time secret the gateway hands the sidecar through its environment and
/// expects back in `Hello`. `Debug` is redacted, equality is constant-time
/// and an empty secret never matches. Production builds can only
/// deserialize it, so no trace path can serialize the secret back out.
#[derive(Clone, Deserialize)]
#[cfg_attr(any(test, feature = "test-support"), derive(Serialize))]
#[serde(transparent)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct LinkSecret(String);

impl LinkSecret {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The raw value, for handing to the sidecar's environment only.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn ct_eq(&self, other: &Self) -> bool {
        let equal: bool = self.0.as_bytes().ct_eq(other.0.as_bytes()).into();
        equal && !self.0.is_empty()
    }
}

impl PartialEq for LinkSecret {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other)
    }
}

impl Eq for LinkSecret {}

impl fmt::Debug for LinkSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkSecret(<redacted>)")
    }
}

/// One tab. The sidecar drops a target whose id exceeds
/// `LinkLimits::max_target_id_chars` and clamps `url` / `title` to
/// `max_target_url_chars` / `max_text_chars`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct TargetInfo {
    pub target_id: TargetId,
    pub url: String,
    pub title: String,
}

/// `browser_gen` increments every time the sidecar's `Browser` is replaced
/// (docker heal, host relaunch) and restarts at 0 with the sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct BrowserStatus {
    pub mode: BrowserMode,
    pub phase: BrowserPhase,
    pub browser_gen: u32,
}

/// At most `LinkLimits::max_targets` tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct TargetsMsg {
    pub browser_gen: u32,
    pub targets: Vec<TargetInfo>,
    pub followed: Option<TargetId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum StreamState {
    /// No browser yet: launch is lazy, so nothing exists before the first
    /// browser tool call.
    Idle,
    Live,
    /// The followed tab is hidden (a human switched tabs in a headful
    /// browser), so Chrome stopped producing frames. CDDM-opened tabs never
    /// enter this state on their own (spike finding (c)).
    Background,
    TargetGone,
    /// The browser is recovering; frames resume on the next `browser_gen`.
    Paused,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct StreamStatus {
    pub state: StreamState,
    pub browser_gen: u32,
    pub target: Option<TargetInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum Capability {
    Screencast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum UnavailableReason {
    TapNotFired,
    CddmVersionMismatch,
    /// Set by the gateway, never the sidecar: `browser.enable` or
    /// `browser.view.enable` is off, so no link will ever come up.
    BrowserDisabled,
}

/// Metadata in front of every screencast JPEG, taken from CDP's
/// `Page.screencastFrame` metadata. Unknown fields are rejected because the
/// gateway forwards the sidecar's header bytes to viewers verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct FrameHeader {
    pub target_id: TargetId,
    pub browser_gen: u32,
    pub seq: u32,
    pub captured_at_ms: f64,
    pub device_width: f64,
    pub device_height: f64,
    pub offset_top: f64,
    pub page_scale_factor: f64,
    pub scroll_offset_x: f64,
    pub scroll_offset_y: f64,
}

/// Sidecar → gateway JSON messages. Production builds only deserialize them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(any(test, feature = "test-support"), derive(Serialize))]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum LinkUp {
    Hello {
        protocol: u32,
        secret: LinkSecret,
        boot_id: BootId,
        pid: u32,
        capabilities: Vec<Capability>,
    },
    Status(BrowserStatus),
    Targets(TargetsMsg),
    Stream(StreamStatus),
    /// Viewer availability as a state: `Some` makes viewing unavailable,
    /// `None` clears an earlier reason (e.g. the tap fired late).
    Availability {
        unavailable: Option<UnavailableReason>,
    },
}

/// Limits the gateway announces in `HelloAck` and the sidecar adopts. Char
/// counts are UTF-16 code units (JS `string.length`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub struct LinkLimits {
    pub max_frame_bytes: u32,
    pub max_json_bytes: u32,
    pub max_targets: u32,
    pub max_target_id_chars: u32,
    pub max_target_url_chars: u32,
    pub max_text_chars: u32,
}

impl LinkLimits {
    pub(crate) const CURRENT: Self = Self {
        max_frame_bytes: MAX_SCREENCAST_FRAME_BYTES_U32,
        max_json_bytes: MAX_LINK_JSON_BYTES_U32,
        max_targets: MAX_LINK_TARGETS,
        max_target_id_chars: MAX_TARGET_ID_CHARS,
        max_target_url_chars: MAX_TARGET_URL_CHARS,
        max_text_chars: MAX_LINK_TEXT_CHARS,
    };
}

/// Why the gateway refused a `Hello`. Every authentication failure collapses
/// into `Unauthorized` so an unauthenticated peer can't tell which check
/// failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum HelloRejectReason {
    ProtocolMismatch,
    Unauthorized,
    AlreadyConnected,
}

/// Gateway → sidecar JSON messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum LinkDown {
    HelloAck { protocol: u32, limits: LinkLimits },
    HelloReject { reason: HelloRejectReason },
    StartScreencast,
    StopScreencast,
}

impl LinkDown {
    /// The `HelloAck` for this build, carrying the limits the sidecar adopts.
    pub(crate) fn hello_ack() -> Self {
        Self::HelloAck {
            protocol: BROWSER_LINK_PROTOCOL_VERSION,
            limits: LinkLimits::CURRENT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum ViewerErrorCode {
    TooManyViewers,
    BadMessage,
    /// The reply to `RequestControl` while takeover is off (always, in M1).
    TakeoverDisabled,
}

/// Gateway → web viewer JSON text messages. Screencast frames travel
/// separately as WS binary messages: `[u32 BE hdr_len][FrameHeader JSON][JPEG]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum ViewerDown {
    /// Generated by the gateway. `link_epoch` increments on every accepted
    /// link. The gateway drops everything the previous link sent (cached
    /// state and the pending frame); a viewer resets its state when the
    /// epoch changes, even when a restarted sidecar reuses a `browser_gen`.
    /// `ping_interval_ms` is how often the viewer must `Ping`.
    Link {
        up: bool,
        link_epoch: u32,
        unavailable: Option<UnavailableReason>,
        ping_interval_ms: u32,
    },
    Status(BrowserStatus),
    Targets(TargetsMsg),
    Stream(StreamStatus),
    Error {
        code: ViewerErrorCode,
    },
    Pong,
}

impl ViewerDown {
    pub fn link(up: bool, link_epoch: u32, unavailable: Option<UnavailableReason>) -> Self {
        Self::Link {
            up,
            link_epoch,
            unavailable,
            ping_interval_ms: VIEWER_PING_INTERVAL_MS,
        }
    }
}

/// Web viewer → gateway JSON text messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/tool/browser/src/generated/")
)]
pub enum ViewerUp {
    Ping,
    RequestControl,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::limits::{MAX_LINK_JSON_BYTES, VIEWER_PING_TIMEOUT};

    fn target() -> TargetInfo {
        TargetInfo {
            target_id: TargetId::new("T1"),
            url: "https://example.com/".into(),
            title: "Example".into(),
        }
    }

    fn status() -> BrowserStatus {
        BrowserStatus {
            mode: BrowserMode::Docker,
            phase: BrowserPhase::Docker {
                phase: DockerPhase::DockerBuildingImage,
            },
            browser_gen: 3,
        }
    }

    fn targets() -> TargetsMsg {
        TargetsMsg {
            browser_gen: 3,
            targets: vec![target()],
            followed: Some(TargetId::new("T1")),
        }
    }

    fn stream() -> StreamStatus {
        StreamStatus {
            state: StreamState::Live,
            browser_gen: 3,
            target: Some(target()),
        }
    }

    fn assert_shape<T>(value: &T, expected: Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let actual = serde_json::to_value(value).unwrap();
        assert_eq!(actual, expected);
        let back: T = serde_json::from_value(expected).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn browser_mode_shape() {
        assert_shape(&BrowserMode::Host, json!("host"));
        assert_shape(&BrowserMode::Docker, json!("docker"));
        assert_shape(&BrowserMode::CdpUrl, json!("cdp_url"));
    }

    #[test]
    fn docker_phase_shape_matches_docker_ts() {
        assert_shape(&DockerPhase::DockerChecking, json!("docker-checking"));
        assert_shape(
            &DockerPhase::DockerBuildingImage,
            json!("docker-building-image"),
        );
        assert_shape(
            &DockerPhase::DockerStartingContainer,
            json!("docker-starting-container"),
        );
        assert_shape(
            &DockerPhase::DockerWaitingForCdp,
            json!("docker-waiting-for-cdp"),
        );
        assert!(serde_json::from_value::<DockerPhase>(json!("ready")).is_err());
    }

    #[test]
    fn browser_phase_shape() {
        assert_shape(&BrowserPhase::Idle, json!({"type": "idle"}));
        assert_shape(
            &BrowserPhase::Installing { percent: 42 },
            json!({"type": "installing", "percent": 42}),
        );
        assert_shape(
            &BrowserPhase::Docker {
                phase: DockerPhase::DockerWaitingForCdp,
            },
            json!({"type": "docker", "phase": "docker-waiting-for-cdp"}),
        );
        assert_shape(&BrowserPhase::Ready, json!({"type": "ready"}));
        assert_shape(&BrowserPhase::Recovering, json!({"type": "recovering"}));
        assert_shape(
            &BrowserPhase::Failed {
                error: "boom".into(),
            },
            json!({"type": "failed", "error": "boom"}),
        );
    }

    #[test]
    fn simple_enum_shapes() {
        assert_shape(&StreamState::Idle, json!("idle"));
        assert_shape(&StreamState::Live, json!("live"));
        assert_shape(&StreamState::Background, json!("background"));
        assert_shape(&StreamState::TargetGone, json!("target_gone"));
        assert_shape(&StreamState::Paused, json!("paused"));
        assert_shape(&StreamState::Unavailable, json!("unavailable"));
        assert_shape(&Capability::Screencast, json!("screencast"));
        assert_shape(&UnavailableReason::TapNotFired, json!("tap_not_fired"));
        assert_shape(
            &UnavailableReason::CddmVersionMismatch,
            json!("cddm_version_mismatch"),
        );
        assert_shape(
            &UnavailableReason::BrowserDisabled,
            json!("browser_disabled"),
        );
        assert_shape(
            &HelloRejectReason::ProtocolMismatch,
            json!("protocol_mismatch"),
        );
        assert_shape(&HelloRejectReason::Unauthorized, json!("unauthorized"));
        assert_shape(
            &HelloRejectReason::AlreadyConnected,
            json!("already_connected"),
        );
        assert_shape(&ViewerErrorCode::TooManyViewers, json!("too_many_viewers"));
        assert_shape(&ViewerErrorCode::BadMessage, json!("bad_message"));
        assert_shape(
            &ViewerErrorCode::TakeoverDisabled,
            json!("takeover_disabled"),
        );
    }

    #[test]
    fn struct_shapes() {
        assert_shape(
            &target(),
            json!({"target_id": "T1", "url": "https://example.com/", "title": "Example"}),
        );
        assert_shape(
            &TargetsMsg {
                browser_gen: 1,
                targets: vec![],
                followed: None,
            },
            json!({"browser_gen": 1, "targets": [], "followed": null}),
        );
        assert_shape(
            &StreamStatus {
                state: StreamState::Idle,
                browser_gen: 0,
                target: None,
            },
            json!({"state": "idle", "browser_gen": 0, "target": null}),
        );
        assert_shape(
            &FrameHeader {
                target_id: TargetId::new("T1"),
                browser_gen: 2,
                seq: 7,
                captured_at_ms: 1_700_000_000_123.5,
                device_width: 1280.0,
                device_height: 800.0,
                offset_top: 0.0,
                page_scale_factor: 1.0,
                scroll_offset_x: 0.0,
                scroll_offset_y: 240.0,
            },
            json!({
                "target_id": "T1",
                "browser_gen": 2,
                "seq": 7,
                "captured_at_ms": 1_700_000_000_123.5,
                "device_width": 1280.0,
                "device_height": 800.0,
                "offset_top": 0.0,
                "page_scale_factor": 1.0,
                "scroll_offset_x": 0.0,
                "scroll_offset_y": 240.0,
            }),
        );
    }

    #[test]
    fn frame_header_rejects_unknown_and_null_fields() {
        let mut header = serde_json::to_value(FrameHeader {
            target_id: TargetId::new("T1"),
            browser_gen: 0,
            seq: 0,
            captured_at_ms: 0.0,
            device_width: 1.0,
            device_height: 1.0,
            offset_top: 0.0,
            page_scale_factor: 1.0,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
        })
        .unwrap();
        let mut extra = header.clone();
        extra["extra"] = json!(1);
        assert!(serde_json::from_value::<FrameHeader>(extra).is_err());
        header["captured_at_ms"] = Value::Null;
        assert!(serde_json::from_value::<FrameHeader>(header).is_err());
    }

    #[test]
    fn tagged_payload_structs_have_no_type_key() {
        let payloads = [
            serde_json::to_value(status()).unwrap(),
            serde_json::to_value(targets()).unwrap(),
            serde_json::to_value(stream()).unwrap(),
        ];
        for payload in payloads {
            let object = payload.as_object().unwrap();
            assert!(!object.contains_key("type"), "{payload}");
        }
    }

    #[test]
    fn link_up_shapes() {
        assert_shape(
            &LinkUp::Hello {
                protocol: 1,
                secret: LinkSecret::new("s3cr3t"),
                boot_id: BootId::new("boot-1"),
                pid: 4242,
                capabilities: vec![Capability::Screencast],
            },
            json!({
                "type": "hello",
                "protocol": 1,
                "secret": "s3cr3t",
                "boot_id": "boot-1",
                "pid": 4242,
                "capabilities": ["screencast"],
            }),
        );
        assert_shape(
            &LinkUp::Status(status()),
            json!({
                "type": "status",
                "mode": "docker",
                "phase": {"type": "docker", "phase": "docker-building-image"},
                "browser_gen": 3,
            }),
        );
        assert_shape(
            &LinkUp::Targets(targets()),
            json!({
                "type": "targets",
                "browser_gen": 3,
                "targets": [{"target_id": "T1", "url": "https://example.com/", "title": "Example"}],
                "followed": "T1",
            }),
        );
        assert_shape(
            &LinkUp::Stream(stream()),
            json!({
                "type": "stream",
                "state": "live",
                "browser_gen": 3,
                "target": {"target_id": "T1", "url": "https://example.com/", "title": "Example"},
            }),
        );
        assert_shape(
            &LinkUp::Availability {
                unavailable: Some(UnavailableReason::TapNotFired),
            },
            json!({"type": "availability", "unavailable": "tap_not_fired"}),
        );
        assert_shape(
            &LinkUp::Availability { unavailable: None },
            json!({"type": "availability", "unavailable": null}),
        );
    }

    #[test]
    fn link_down_shapes() {
        assert_shape(
            &LinkDown::hello_ack(),
            json!({
                "type": "hello_ack",
                "protocol": 1,
                "limits": {
                    "max_frame_bytes": 2_097_152,
                    "max_json_bytes": 262_144,
                    "max_targets": 32,
                    "max_target_id_chars": 64,
                    "max_target_url_chars": 512,
                    "max_text_chars": 256,
                },
            }),
        );
        assert_shape(
            &LinkDown::HelloReject {
                reason: HelloRejectReason::Unauthorized,
            },
            json!({"type": "hello_reject", "reason": "unauthorized"}),
        );
        assert_shape(
            &LinkDown::StartScreencast,
            json!({"type": "start_screencast"}),
        );
        assert_shape(
            &LinkDown::StopScreencast,
            json!({"type": "stop_screencast"}),
        );
    }

    #[test]
    fn viewer_down_shapes() {
        assert_shape(
            &ViewerDown::link(false, 4, Some(UnavailableReason::BrowserDisabled)),
            json!({
                "type": "link",
                "up": false,
                "link_epoch": 4,
                "unavailable": "browser_disabled",
                "ping_interval_ms": 15_000,
            }),
        );
        assert_shape(
            &ViewerDown::link(true, 5, None),
            json!({
                "type": "link",
                "up": true,
                "link_epoch": 5,
                "unavailable": null,
                "ping_interval_ms": 15_000,
            }),
        );
        assert_shape(
            &ViewerDown::Status(BrowserStatus {
                mode: BrowserMode::Host,
                phase: BrowserPhase::Installing { percent: 10 },
                browser_gen: 0,
            }),
            json!({
                "type": "status",
                "mode": "host",
                "phase": {"type": "installing", "percent": 10},
                "browser_gen": 0,
            }),
        );
        assert_shape(
            &ViewerDown::Targets(targets()),
            json!({
                "type": "targets",
                "browser_gen": 3,
                "targets": [{"target_id": "T1", "url": "https://example.com/", "title": "Example"}],
                "followed": "T1",
            }),
        );
        assert_shape(
            &ViewerDown::Stream(StreamStatus {
                state: StreamState::TargetGone,
                browser_gen: 2,
                target: None,
            }),
            json!({"type": "stream", "state": "target_gone", "browser_gen": 2, "target": null}),
        );
        assert_shape(
            &ViewerDown::Error {
                code: ViewerErrorCode::TakeoverDisabled,
            },
            json!({"type": "error", "code": "takeover_disabled"}),
        );
        assert_shape(&ViewerDown::Pong, json!({"type": "pong"}));
    }

    #[test]
    fn viewer_ping_timeout_is_twice_the_announced_interval() {
        let ViewerDown::Link {
            ping_interval_ms, ..
        } = ViewerDown::link(true, 0, None)
        else {
            panic!("expected link");
        };
        assert_eq!(
            VIEWER_PING_TIMEOUT.as_millis(),
            2 * u128::from(ping_interval_ms)
        );
    }

    #[test]
    fn viewer_up_shapes() {
        assert_shape(&ViewerUp::Ping, json!({"type": "ping"}));
        assert_shape(
            &ViewerUp::RequestControl,
            json!({"type": "request_control"}),
        );
    }

    #[test]
    fn largest_clamped_targets_msg_fits() {
        let limits = LinkLimits::CURRENT;
        // A control char is one UTF-16 unit and the worst JSON escape (`\u0001`).
        let worst = |chars: u32| "\u{1}".repeat(chars as usize);
        let tab = TargetInfo {
            target_id: TargetId::new(worst(limits.max_target_id_chars)),
            url: worst(limits.max_target_url_chars),
            title: worst(limits.max_text_chars),
        };
        let msg = LinkUp::Targets(TargetsMsg {
            browser_gen: u32::MAX,
            targets: vec![tab; limits.max_targets as usize],
            followed: Some(TargetId::new(worst(limits.max_target_id_chars))),
        });
        let len = serde_json::to_vec(&msg).unwrap().len();
        assert!(len <= MAX_LINK_JSON_BYTES, "{len} > {MAX_LINK_JSON_BYTES}");
        assert_eq!(limits.max_json_bytes as usize, MAX_LINK_JSON_BYTES);
    }

    #[test]
    fn link_secret_debug_is_redacted() {
        let secret = LinkSecret::new("hunter2-very-secret");
        let hello = LinkUp::Hello {
            protocol: 1,
            secret: secret.clone(),
            boot_id: BootId::new("b"),
            pid: 1,
            capabilities: vec![],
        };
        for rendered in [
            format!("{secret:?}"),
            format!("{hello:?}"),
            format!("{hello:#?}"),
        ] {
            assert!(!rendered.contains("hunter2"), "{rendered}");
        }
        assert!(format!("{secret:?}").contains("redacted"));
    }

    #[test]
    fn link_secret_compares_by_value() {
        let a = LinkSecret::new("abc");
        assert!(a.ct_eq(&LinkSecret::new("abc")));
        assert!(!a.ct_eq(&LinkSecret::new("abd")));
        assert!(!a.ct_eq(&LinkSecret::new("abcd")));
        assert!(!a.ct_eq(&LinkSecret::new("")));
        assert_eq!(a.expose(), "abc");
    }

    #[test]
    fn empty_link_secret_never_matches() {
        assert!(!LinkSecret::new("").ct_eq(&LinkSecret::new("")));
    }
}
