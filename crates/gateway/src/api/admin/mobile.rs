//! `GET /v1/mobile/links`: the link table, what each paired device's live
//! legs ride on and how its last direct offer went. `baybo device status`
//! reads it from the running gateway.

use axum::Json;
use axum::extract::State;
use carrier::kind::CarrierKind;
use chrono::{DateTime, Utc};
use remote_host_protocol::relay::LegClass;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::ErrorBody;
use crate::channel::carrier::offer::{ACCEPTED, Decline};
use crate::channel::links::{DeviceRecord, LegRecord, OfferRecord};
use crate::server::AdminState;

/// Where the admin listener serves [`MobileLinks`].
pub const MOBILE_LINKS_PATH: &str = "/v1/mobile/links";

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(mobile_links))
}

/// Response of `GET /v1/mobile/links`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MobileLinks {
    /// Every device with a live leg or a recorded offer, by `device_id`.
    pub devices: Vec<DeviceLink>,
}

/// One device's live legs and last direct offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DeviceLink {
    pub device_id: String,
    /// The device's authenticated legs, oldest first.
    pub legs: Vec<LiveLeg>,
    /// The latest offer this process's carrier runtime answered for the
    /// device; `null` when none has been.
    pub last_offer: Option<LastOffer>,
}

/// A leg that has authenticated its device and not yet ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LiveLeg {
    pub class: LinkClass,
    /// What the leg rides on; `null` for a QUIC carrier whose address pair
    /// the gateway could not classify.
    pub carrier: Option<LinkCarrier>,
    /// When Noise authenticated the device on this leg.
    pub started_at: DateTime<Utc>,
}

/// The time and outcome of a device's latest direct offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LastOffer {
    pub at: DateTime<Utc>,
    pub outcome: OfferOutcome,
}

/// A leg's class, as the leg announced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkClass {
    Chat,
    Api,
    Blob,
}

/// What a leg rides on: C's relay splice or one of the direct carriers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinkCarrier {
    Relay,
    Lan,
    Ipv6,
    Ipv4,
    Ipv4Punched,
    Tcp,
}

/// How the carrier runtime answered an offer, spelled as the `direct_offer`
/// log line's `outcome` field spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum OfferOutcome {
    #[serde(rename = "accepted")]
    Accepted,
    #[serde(rename = "declined:auth")]
    DeclinedAuth,
    #[serde(rename = "declined:stale")]
    DeclinedStale,
    #[serde(rename = "declined:replayed")]
    DeclinedReplayed,
    #[serde(rename = "declined:over_cap")]
    DeclinedOverCap,
    #[serde(rename = "declined:unbound")]
    DeclinedUnbound,
}

impl LinkClass {
    pub fn as_str(self) -> &'static str {
        self.domain().as_str()
    }

    fn domain(self) -> LegClass {
        match self {
            Self::Chat => LegClass::Chat,
            Self::Api => LegClass::Api,
            Self::Blob => LegClass::Blob,
        }
    }
}

impl From<LegClass> for LinkClass {
    fn from(class: LegClass) -> Self {
        match class {
            LegClass::Chat => Self::Chat,
            LegClass::Api => Self::Api,
            LegClass::Blob => Self::Blob,
        }
    }
}

impl LinkCarrier {
    pub fn as_str(self) -> &'static str {
        self.domain().as_str()
    }

    fn domain(self) -> CarrierKind {
        match self {
            Self::Relay => CarrierKind::Relay,
            Self::Lan => CarrierKind::Lan,
            Self::Ipv6 => CarrierKind::Ipv6,
            Self::Ipv4 => CarrierKind::Ipv4,
            Self::Ipv4Punched => CarrierKind::Ipv4Punched,
            Self::Tcp => CarrierKind::Tcp,
        }
    }
}

impl From<CarrierKind> for LinkCarrier {
    fn from(kind: CarrierKind) -> Self {
        match kind {
            CarrierKind::Relay => Self::Relay,
            CarrierKind::Lan => Self::Lan,
            CarrierKind::Ipv6 => Self::Ipv6,
            CarrierKind::Ipv4 => Self::Ipv4,
            CarrierKind::Ipv4Punched => Self::Ipv4Punched,
            CarrierKind::Tcp => Self::Tcp,
        }
    }
}

impl OfferOutcome {
    pub fn as_str(self) -> &'static str {
        match self.domain() {
            Ok(()) => ACCEPTED,
            Err(decline) => decline.outcome(),
        }
    }

    fn domain(self) -> Result<(), Decline> {
        match self {
            Self::Accepted => Ok(()),
            Self::DeclinedAuth => Err(Decline::Auth),
            Self::DeclinedStale => Err(Decline::Stale),
            Self::DeclinedReplayed => Err(Decline::Replayed),
            Self::DeclinedOverCap => Err(Decline::OverCap),
            Self::DeclinedUnbound => Err(Decline::Unbound),
        }
    }
}

impl From<Result<(), Decline>> for OfferOutcome {
    fn from(outcome: Result<(), Decline>) -> Self {
        match outcome {
            Ok(()) => Self::Accepted,
            Err(Decline::Auth) => Self::DeclinedAuth,
            Err(Decline::Stale) => Self::DeclinedStale,
            Err(Decline::Replayed) => Self::DeclinedReplayed,
            Err(Decline::OverCap) => Self::DeclinedOverCap,
            Err(Decline::Unbound) => Self::DeclinedUnbound,
        }
    }
}

impl From<DeviceRecord> for DeviceLink {
    fn from(record: DeviceRecord) -> Self {
        Self {
            device_id: record.device_id,
            legs: record.legs.into_iter().map(LiveLeg::from).collect(),
            last_offer: record.last_offer.map(LastOffer::from),
        }
    }
}

impl From<LegRecord> for LiveLeg {
    fn from(record: LegRecord) -> Self {
        Self {
            class: record.class.into(),
            carrier: record.kind.map(LinkCarrier::from),
            started_at: record.started_at,
        }
    }
}

impl From<OfferRecord> for LastOffer {
    fn from(record: OfferRecord) -> Self {
        Self {
            at: record.at,
            outcome: record.outcome.into(),
        }
    }
}

#[utoipa::path(
    get,
    path = "/mobile/links",
    tag = "mobile",
    responses(
        (status = 200, description = "Each paired device's live legs, by class and carrier, and its last direct offer", body = MobileLinks),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn mobile_links(State(state): State<AdminState>) -> Json<MobileLinks> {
    Json(MobileLinks {
        devices: state
            .device_links
            .snapshot()
            .into_iter()
            .map(DeviceLink::from)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::channel::device_content::{AuthenticatedDevice, BinarySink};
    use crate::server::build_admin_router_for_tests;
    use crate::test_support::{TEST_ADMIN_TOKEN, build_test_deps};

    const DEVICE: &str = "device-a";
    const BODY_LIMIT: usize = 64 * 1024;

    struct NullSink;

    #[async_trait::async_trait]
    impl BinarySink for NullSink {
        async fn send_bytes(&mut self, _bytes: Vec<u8>) -> Result<(), ()> {
            Ok(())
        }

        async fn close(&mut self) {}
    }

    fn get(bearer: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().method("GET").uri(MOBILE_LINKS_PATH);
        if let Some(bearer) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        request.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn the_route_needs_the_admin_bearer() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let router = build_admin_router_for_tests(&tg.deps);
        for bearer in [None, Some("not-the-admin-token")] {
            let response = router.clone().oneshot(get(bearer)).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{bearer:?}");
        }
    }

    #[tokio::test]
    async fn the_route_serves_what_the_writers_recorded() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let router = build_admin_router_for_tests(&tg.deps);
        let links = &tg.deps.device_links;
        let mut leg = links.tracked(NullSink, LegClass::Chat, Some(CarrierKind::Ipv4Punched));
        leg.authenticated(&AuthenticatedDevice {
            device_id: DEVICE.to_owned(),
        })
        .unwrap();
        links.record_offer(DEVICE, Err(Decline::Replayed));

        let response = router.oneshot(get(Some(TEST_ADMIN_TOKEN))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), BODY_LIMIT).await.unwrap())
                .unwrap();
        let device = &body["devices"][0];
        assert_eq!(device["device_id"], DEVICE);
        assert_eq!(device["legs"][0]["class"], "chat");
        assert_eq!(device["legs"][0]["carrier"], "ipv4_punched");
        assert!(device["legs"][0]["started_at"].is_string());
        assert_eq!(device["last_offer"]["outcome"], "declined:replayed");

        let links: MobileLinks = serde_json::from_value(body).unwrap();
        let [link] = links.devices.as_slice() else {
            panic!("one device: {links:?}");
        };
        assert_eq!(link.legs[0].class, LinkClass::Chat);
        assert_eq!(link.legs[0].carrier, Some(LinkCarrier::Ipv4Punched));
        assert_eq!(
            link.last_offer.as_ref().map(|offer| offer.outcome),
            Some(OfferOutcome::DeclinedReplayed)
        );
        drop(leg);
    }

    #[test]
    fn the_wire_labels_are_the_domain_and_log_labels() {
        for kind in [
            CarrierKind::Relay,
            CarrierKind::Lan,
            CarrierKind::Ipv6,
            CarrierKind::Ipv4,
            CarrierKind::Ipv4Punched,
            CarrierKind::Tcp,
        ] {
            let carrier = LinkCarrier::from(kind);
            assert_eq!(carrier.as_str(), kind.as_str());
            assert_eq!(serde_json::to_value(carrier).unwrap(), kind.as_str());
        }
        for class in [LegClass::Chat, LegClass::Api, LegClass::Blob] {
            let link = LinkClass::from(class);
            assert_eq!(link.as_str(), class.as_str());
            assert_eq!(serde_json::to_value(link).unwrap(), class.as_str());
        }
        for outcome in [
            Ok(()),
            Err(Decline::Auth),
            Err(Decline::Stale),
            Err(Decline::Replayed),
            Err(Decline::OverCap),
            Err(Decline::Unbound),
        ] {
            let wire = OfferOutcome::from(outcome);
            let logged = outcome.map_or_else(Decline::outcome, |()| ACCEPTED);
            assert_eq!(wire.as_str(), logged);
            assert_eq!(serde_json::to_value(wire).unwrap(), logged);
        }
    }
}
