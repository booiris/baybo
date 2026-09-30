//! The link table: for each paired device, the legs live right now, each
//! with its class, what it rides on and when it came up, and the time and
//! outcome of the device's last direct offer. It lives in memory only. Its
//! writers are the relay path and the carrier runtime, through the
//! crate-private `DeviceLinks::tracked` and `DeviceLinks::record_offer`;
//! `GET /v1/mobile/links` serves `DeviceLinks::snapshot`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use carrier::kind::CarrierKind;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use remote_host_protocol::relay::LegClass;

use crate::channel::carrier::offer::Decline;
use crate::channel::device_content::{AuthenticatedDevice, BinarySink, RelaySessionError};

/// The gateway's link table. Cheap to clone: every clone is the same table.
#[derive(Clone, Default)]
pub struct DeviceLinks(Arc<Mutex<LinkTable>>);

#[derive(Default)]
struct LinkTable {
    next_leg: u64,
    devices: HashMap<String, DeviceEntry>,
}

#[derive(Default)]
struct DeviceEntry {
    legs: BTreeMap<u64, LegRecord>,
    last_offer: Option<OfferRecord>,
}

/// One device's row of the table, as [`DeviceLinks::snapshot`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRecord {
    pub(crate) device_id: String,
    /// Oldest first.
    pub(crate) legs: Vec<LegRecord>,
    pub(crate) last_offer: Option<OfferRecord>,
}

/// A leg that has authenticated its device and not yet ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegRecord {
    pub(crate) class: LegClass,
    /// `None` for a QUIC carrier whose address pair has no kind.
    pub(crate) kind: Option<CarrierKind>,
    /// When Noise authenticated the device on this leg.
    pub(crate) started_at: DateTime<Utc>,
}

/// How the carrier runtime answered a device's latest offer, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferRecord {
    pub(crate) at: DateTime<Utc>,
    pub(crate) outcome: Result<(), Decline>,
}

impl DeviceLinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps a leg's sink so the leg is listed for the device Noise
    /// authenticates on it, from that moment until the sink drops. `kind`
    /// is `None` for a QUIC carrier whose address pair has no kind.
    pub(crate) fn tracked<S: BinarySink>(
        &self,
        sink: S,
        class: LegClass,
        kind: Option<CarrierKind>,
    ) -> LinkedSink<S> {
        LinkedSink {
            inner: sink,
            links: self.clone(),
            class,
            kind,
            live: None,
        }
    }

    /// Records how the carrier runtime answered `device_id`'s latest offer.
    pub(crate) fn record_offer(&self, device_id: &str, outcome: Result<(), Decline>) {
        self.0
            .lock()
            .devices
            .entry(device_id.to_owned())
            .or_default()
            .last_offer = Some(OfferRecord {
            at: Utc::now(),
            outcome,
        });
    }

    /// Every device the table holds, by id.
    pub(crate) fn snapshot(&self) -> Vec<DeviceRecord> {
        let table = self.0.lock();
        let mut devices: Vec<DeviceRecord> = table
            .devices
            .iter()
            .map(|(device_id, entry)| DeviceRecord {
                device_id: device_id.clone(),
                legs: entry.legs.values().cloned().collect(),
                last_offer: entry.last_offer.clone(),
            })
            .collect();
        devices.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        devices
    }

    fn open(&self, device_id: &str, class: LegClass, kind: Option<CarrierKind>) -> LegEntry {
        let mut table = self.0.lock();
        let leg = table.next_leg;
        table.next_leg = table.next_leg.wrapping_add(1);
        table
            .devices
            .entry(device_id.to_owned())
            .or_default()
            .legs
            .insert(
                leg,
                LegRecord {
                    class,
                    kind,
                    started_at: Utc::now(),
                },
            );
        LegEntry {
            links: self.clone(),
            device_id: device_id.to_owned(),
            leg,
        }
    }

    fn close(&self, device_id: &str, leg: u64) {
        let mut table = self.0.lock();
        let Some(entry) = table.devices.get_mut(device_id) else {
            return;
        };
        entry.legs.remove(&leg);
        if entry.legs.is_empty() && entry.last_offer.is_none() {
            table.devices.remove(device_id);
        }
    }
}

/// One leg's row in the table, removed on drop.
struct LegEntry {
    links: DeviceLinks,
    device_id: String,
    leg: u64,
}

impl Drop for LegEntry {
    fn drop(&mut self) {
        self.links.close(&self.device_id, self.leg);
    }
}

/// A leg's sink, which lists the leg once Noise has authenticated its
/// device and the wrapped sink has accepted it, and unlists it when the
/// sink drops with the session.
pub(crate) struct LinkedSink<S> {
    inner: S,
    links: DeviceLinks,
    class: LegClass,
    kind: Option<CarrierKind>,
    live: Option<LegEntry>,
}

#[async_trait::async_trait]
impl<S: BinarySink> BinarySink for LinkedSink<S> {
    const CONFIRMS_HANDSHAKE: bool = S::CONFIRMS_HANDSHAKE;

    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        self.inner.send_bytes(bytes).await
    }

    async fn close(&mut self) {
        self.inner.close().await;
    }

    fn authenticated(&mut self, device: &AuthenticatedDevice) -> Result<(), RelaySessionError> {
        self.inner.authenticated(device)?;
        self.live = Some(self.links.open(&device.device_id, self.class, self.kind));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "device-a";
    const OTHER: &str = "device-b";

    #[derive(Default)]
    struct NullSink {
        refuse: bool,
    }

    #[async_trait::async_trait]
    impl BinarySink for NullSink {
        const CONFIRMS_HANDSHAKE: bool = true;

        async fn send_bytes(&mut self, _bytes: Vec<u8>) -> Result<(), ()> {
            Ok(())
        }

        async fn close(&mut self) {}

        fn authenticated(
            &mut self,
            _device: &AuthenticatedDevice,
        ) -> Result<(), RelaySessionError> {
            if self.refuse {
                return Err(RelaySessionError::Ended("refused".to_owned()));
            }
            Ok(())
        }
    }

    fn device(device_id: &str) -> AuthenticatedDevice {
        AuthenticatedDevice {
            device_id: device_id.to_owned(),
        }
    }

    fn legs(links: &DeviceLinks, device_id: &str) -> Vec<(LegClass, Option<CarrierKind>)> {
        links
            .snapshot()
            .into_iter()
            .find(|record| record.device_id == device_id)
            .map(|record| {
                record
                    .legs
                    .into_iter()
                    .map(|leg| (leg.class, leg.kind))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_leg_is_listed_from_authentication_until_its_sink_drops() {
        let links = DeviceLinks::new();
        let mut relay = links.tracked(
            NullSink::default(),
            LegClass::Chat,
            Some(CarrierKind::Relay),
        );
        assert!(links.snapshot().is_empty(), "not yet authenticated");

        relay.authenticated(&device(DEVICE)).unwrap();
        let mut quic = links.tracked(
            NullSink::default(),
            LegClass::Api,
            Some(CarrierKind::Ipv4Punched),
        );
        quic.authenticated(&device(DEVICE)).unwrap();
        let mut unclassified = links.tracked(NullSink::default(), LegClass::Blob, None);
        unclassified.authenticated(&device(DEVICE)).unwrap();
        assert_eq!(
            legs(&links, DEVICE),
            [
                (LegClass::Chat, Some(CarrierKind::Relay)),
                (LegClass::Api, Some(CarrierKind::Ipv4Punched)),
                (LegClass::Blob, None),
            ]
        );

        drop(relay);
        assert_eq!(
            legs(&links, DEVICE),
            [
                (LegClass::Api, Some(CarrierKind::Ipv4Punched)),
                (LegClass::Blob, None),
            ]
        );
        drop((quic, unclassified));
        assert!(
            links.snapshot().is_empty(),
            "a device with no leg and no offer leaves the table"
        );
    }

    #[test]
    fn a_leg_whose_sink_refuses_the_device_is_never_listed() {
        let links = DeviceLinks::new();
        let mut refused = links.tracked(
            NullSink { refuse: true },
            LegClass::Chat,
            Some(CarrierKind::Ipv4),
        );
        assert!(refused.authenticated(&device(DEVICE)).is_err());
        assert!(links.snapshot().is_empty());
    }

    #[test]
    fn legs_are_listed_under_the_device_noise_authenticated() {
        let links = DeviceLinks::new();
        let mut first = links.tracked(NullSink::default(), LegClass::Chat, Some(CarrierKind::Lan));
        let mut second =
            links.tracked(NullSink::default(), LegClass::Chat, Some(CarrierKind::Ipv4));
        first.authenticated(&device(OTHER)).unwrap();
        second.authenticated(&device(DEVICE)).unwrap();
        let ids: Vec<String> = links
            .snapshot()
            .into_iter()
            .map(|record| record.device_id)
            .collect();
        assert_eq!(ids, [DEVICE, OTHER], "devices are listed by id");
        assert_eq!(
            legs(&links, OTHER),
            [(LegClass::Chat, Some(CarrierKind::Lan))]
        );
        assert_eq!(
            legs(&links, DEVICE),
            [(LegClass::Chat, Some(CarrierKind::Ipv4))]
        );
    }

    #[test]
    fn the_last_offer_outcome_replaces_the_one_before_and_outlives_the_legs() {
        let links = DeviceLinks::new();
        links.record_offer(DEVICE, Ok(()));
        links.record_offer(DEVICE, Err(Decline::Stale));
        let mut leg = links.tracked(NullSink::default(), LegClass::Chat, Some(CarrierKind::Ipv6));
        leg.authenticated(&device(DEVICE)).unwrap();
        drop(leg);

        let snapshot = links.snapshot();
        let [record] = snapshot.as_slice() else {
            panic!("one device: {snapshot:?}");
        };
        assert!(record.legs.is_empty());
        assert_eq!(
            record.last_offer.as_ref().map(|offer| offer.outcome),
            Some(Err(Decline::Stale))
        );
    }

    #[test]
    fn a_linked_sink_keeps_the_handshake_rule_of_the_sink_it_wraps() {
        fn confirms<S: BinarySink>(_: &S) -> bool {
            S::CONFIRMS_HANDSHAKE
        }
        let links = DeviceLinks::new();
        let sink = links.tracked(NullSink::default(), LegClass::Chat, None);
        assert!(confirms(&sink));
    }
}
