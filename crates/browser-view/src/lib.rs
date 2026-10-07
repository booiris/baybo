//! Live browser viewer: the wire contract between the gateway, the browser
//! sidecar and the web viewer, the bounded link codec, the gateway's link
//! listener and the hub that fans the sidecar's screencast out to viewers.

mod clamp;
pub mod codec;
pub mod error;
pub mod hub;
pub mod limits;
mod listener;
pub mod params;
pub mod wire;
