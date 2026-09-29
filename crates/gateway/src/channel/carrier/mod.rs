//! The gateway (A) side of a relay binding's direct carriers: see
//! `docs/modules/mobile/direct-carriers.md`.
//!
//! - [`runtime`]: the carrier runtime a binding scope owns, with its direct
//!   token and candidate keys, and the port offers arrive through.
//! - [`offer`]: the freshness and replay rules an opened offer must pass.
//! - [`punches`]: the live punches and their allowed-IP sets, which gate QUIC
//!   admission, and authenticated-punch verification.
//! - [`gather`] and [`interfaces`]: A's host candidates, and the pairs it
//!   punches.
//! - [`probe`]: A's punches and on-demand rendezvous registration, and the
//!   routing of received probe datagrams.
//! - [`udp`]: each family's stable socket, rebound only on a persistent
//!   receive error.
//! - [`quic`]: QUIC admission and the connections and streams it admits.
//! - [`tcp`]: the opt-in TCP listeners, their pre-authentication permits and
//!   the sessions they accept.
//! - [`session`]: the `DirectOpen` gate and the hand-off to the relay leg's
//!   responders.
//! - `phone` (tests only): the phone's end of a carrier, shared by the
//!   runtime's tests and the relay E2E.

pub(crate) mod error;
pub(crate) mod gather;
pub(crate) mod interfaces;
pub(crate) mod offer;
#[cfg(test)]
pub(crate) mod phone;
pub(crate) mod probe;
pub(crate) mod punches;
pub(crate) mod quic;
pub(crate) mod runtime;
pub(crate) mod session;
pub(crate) mod tcp;
pub(crate) mod udp;
