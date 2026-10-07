//! The QUIC direct carrier shared by the gateway (A) and the phone (P) of a
//! relay binding: see `docs/modules/mobile/direct-carriers.md`.
//!
//! - [`socket`]: `DemuxSocket`, the one UDP socket per family that QUIC, the
//!   UDP rendezvous and the punches share.
//! - [`quic`]: the endpoint, transport and TLS configuration, A's per-process
//!   certificate, and P's certificate pin.
//! - [`framing`]: the u32-BE length-prefixed frames of a carrier session and
//!   its `DirectOpen` preface.
//! - [`interfaces`]: the host's interface addresses, from which each side
//!   gathers its host candidates.
//! - [`burst`]: punch pacing.
//! - [`rendezvous`]: one side's UDP registration with C for a punch, and the
//!   latch for the `Peer` C returns.
//! - [`kind`]: `CarrierKind`, what a leg rides on.
//!
//! The crate names no crypto provider: the consumer enables one on quinn and
//! passes the matching rustls `CryptoProvider` in.

pub mod burst;
pub mod error;
pub mod framing;
pub mod interfaces;
pub mod kind;
pub mod quic;
pub mod rendezvous;
pub mod socket;
