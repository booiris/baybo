//! **push** — the only `aura-remote-host` component that holds the APNs `.p8`.
//!
//! It relays operator-encrypted lock-screen-preview blobs to APNs for any Aura
//! gateway, staying **blind**: the gateway (A) encrypts the preview with the
//! device's push key, this component copies the opaque `enc`/`n`/`kid`/`bid`
//! into the APNs payload and signs the provider token — it never decrypts.
//! Built as its own crate/binary so the crown-jewel key stays isolatable from
//! the high-exposure relay surface.
//!
//! This first slice implements the ES256 provider-token signer ([`jwt`]); the
//! `/notify` ingest, the APNs sender seam (+ mock), and the
//! `device_id → { token, env }` store follow.

pub mod apns;
pub mod error;
pub mod http;
pub mod jwt;
pub mod notify;
pub mod store;

pub use apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};
pub use error::PushError;
pub use http::{PushState, router};
pub use jwt::ApnsProviderToken;
pub use notify::{NotifyOutcome, NotifyRequest, NotifyService};
pub use store::{
    Admission, DeviceRegistration, DeviceTokenStore, InMemoryAdmission, InMemoryDeviceTokenStore,
};
