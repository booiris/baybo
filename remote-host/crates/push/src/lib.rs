//! **push** — the only `remote-host` component that holds the APNs `.p8`.
//!
//! It relays operator-encrypted lock-screen-preview blobs to APNs for any Baybo
//! gateway, staying **blind**: the gateway (A) encrypts the preview with the
//! device's push key, this component copies the opaque `enc`/`n`/`bid`
//! into the APNs payload and signs the provider token — it never decrypts.
//! Built as its own crate/binary so the crown-jewel key stays isolatable from
//! the high-exposure relay surface.

pub mod apns;
pub mod apns_http;
pub mod delegation;
pub mod error;
pub mod http;
pub mod jwt;
pub mod notify;
pub mod ratelimit;
pub mod serve;
pub mod store;

pub use apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};
pub use apns_http::HttpApnsSender;
pub use error::PushError;
pub use http::{PushState, router};
pub use jwt::ApnsProviderToken;
pub use notify::{NotifyOutcome, NotifyRequest, NotifyService, RegisterOutcome, RegisterRequest};
pub use ratelimit::NotifyRateLimiter;
pub use serve::{PushConfig, build_router};
pub use store::{
    Admission, DeviceRegistration, DeviceTokenStore, InMemoryAdmission, InMemoryDeviceTokenStore,
};
