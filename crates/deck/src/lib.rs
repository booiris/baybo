//! Deck — agent-authored live cards (docs/modules/deck.md).
//!
//! A card is a plain-file bundle at `workspace/deck/<uuid>/` whose
//! backend (`service.js`) runs as an always-resident supervised bun
//! process in the OS sandbox with **no network** — every effect is a
//! stdio RPC into this crate (host-mediated fetch behind the SSRF floor
//! with placeholder reveal at egress, sandboxed exec, policed emits) —
//! and whose frontend (`card.html`) renders in a sandboxed iframe on the
//! phone. The gateway consumes [`DeckManager`]; the agent consumes the
//! [`tools`].

pub mod bundle;
pub mod error;
pub mod host;
pub mod manager;
mod repo;
pub mod service;
pub mod spec;
pub mod supervisor;
pub mod tools;

pub use bundle::{DeckManifest, RefreshSpec, SDK_VERSION};
pub use error::{DeckError, Result};
pub use manager::{
    BundleFiles, CardView, DeckEvents, DeckManager, DeckManagerConfig, DeckMutationResult,
    DeckView, NoopDeckEvents,
};
