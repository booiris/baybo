//! Store ports: the persistence-trait contracts (`*Store`) and the shared
//! [`StorageError`], decoupled from any concrete backend.
//!
//! Domain crates and other consumers depend on this crate to *call* a
//! store; `baybo-storage` is the sqlite adapter that *implements* the
//! traits. Keeping the contracts here — a leaf over `baybo-model` — lets
//! low-level crates depend on a store interface without pulling the heavy
//! sqlite adapter, and keeps the dependency graph acyclic.

pub mod agent_profile;

pub mod blob;
pub mod channel_bot;
pub mod channel_pairing;
pub mod channel_session;
pub mod cost;
pub mod cron;
pub mod deck;
pub mod device;
pub mod error;
pub mod message_search;
pub mod secret;
pub mod session;
pub mod session_folder;
pub mod skill_risk;
pub mod task;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod trace;
pub mod turn;

pub use agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate};
pub use blob::{BlobMeta, BlobReader, BlobStore, ByteStream, MAX_BLOB_BYTES, SHA256_PREFIX};
pub use channel_bot::{ChannelBotRow, ChannelBotStore};
pub use channel_pairing::{ChannelPairingRow, ChannelPairingStore, PairingStatus};
pub use channel_session::ChannelSessionStore;
pub use cost::{CostGroupBucket, CostGroupKey, CostStore};
pub use cron::{CronFire, CronStore, ExecutionCompletion};
pub use deck::{DeckCardRow, DeckCardStore, DeckLayoutEntry, DeckSize, DeckSnapshotRow};
pub use device::{DeviceRow, DeviceStatus, DeviceStore};
pub use error::StorageError;
pub use message_search::{MessageSearchStore, SearchHit, SearchScope};
pub use secret::{SecretStore, StoreIdentity};
pub use session::{DreamCandidate, SessionMessageAppendOutcome, SessionStore, StoredMessage};
pub use session_folder::{SessionFolderRow, SessionFolderStore};
pub use skill_risk::{AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore};
pub use task::{TaskPatch, TaskStore};
pub use trace::{SpanEventRow, SpanRow, StepRow, ToolSetRow, TraceStore};
pub use turn::{SessionTurnBounds, SessionTurnStats, TurnRow, TurnStore};
