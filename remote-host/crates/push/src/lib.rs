//! **push** — provider-neutral encrypted push dispatch.
//!
//! It relays operator-encrypted lock-screen-preview blobs to configured push providers
//! gateway, staying **blind**: the gateway (A) encrypts the preview with the
//! device's push key, this component copies the opaque `enc`/`n`/`bid`
//! into a provider payload and signs provider requests — it never decrypts.
//! Built as its own crate/binary so provider credentials stay isolatable from
//! the high-exposure relay surface.

pub mod apns;
pub mod apns_http;
pub mod delegation;
pub mod error;
pub mod http;
pub mod jwt;
pub mod notify;
pub mod provider;
pub mod ratelimit;
pub mod serve;
pub mod store;
pub mod traffic;

pub use apns::{ApnsEnvironment, ApnsOutcome, ApnsProvider, ApnsRequest, ApnsSender};
pub use apns_http::HttpApnsSender;
pub use error::PushError;
pub use http::{PushState, router};
pub use jwt::ApnsProviderToken;
pub use notify::{NotifyOutcome, NotifyRequest, NotifyService, RegisterOutcome, RegisterRequest};
pub use provider::{
    EncryptedPush, ProviderDelivery, ProviderOutcome, ProviderSender, PushProviders,
};
pub use ratelimit::NotifyRateLimiter;
pub use serve::{PushConfig, PushLimits, build_router};
pub use store::{DeviceRegistration, DeviceSummary, DeviceTokenStore, InMemoryDeviceTokenStore};
pub use traffic::{PushCounts, PushTrafficDelta, PushTrafficRegistry};
