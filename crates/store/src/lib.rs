//! Store ports: the persistence-trait contracts (`*Store`) and the shared
//! [`StorageError`], decoupled from any concrete backend.
//!
//! Domain crates and other consumers depend on this crate to *call* a
//! store; `baybo-storage` is the libsql adapter that *implements* the
//! traits. Keeping the contracts here — a leaf over `baybo-model` — lets
//! low-level crates depend on a store interface without pulling the heavy
//! libsql adapter, and keeps the dependency graph acyclic.

pub mod blob;
pub mod channel_bot;
pub mod channel_pairing;
pub mod channel_session;
pub mod cost;
pub mod cron;
pub mod device;
pub mod device_pairing;
pub mod error;
pub mod job;
pub mod secret;
pub mod session;
pub mod session_folder;
pub mod session_summary;
pub mod skill_risk;
pub mod task;
pub mod trace;

pub use blob::{BlobMeta, BlobReader, BlobStore, ByteStream, SHA256_PREFIX};
pub use channel_bot::{ChannelBotRow, ChannelBotStore};
pub use channel_pairing::{ChannelPairingRow, ChannelPairingStore, PairingStatus};
pub use channel_session::ChannelSessionStore;
pub use cost::CostStore;
pub use cron::CronStore;
pub use device::{DeviceRow, DeviceStatus, DeviceStore};
pub use device_pairing::DevicePairingSlot;
pub use error::StorageError;
pub use job::{JobRow, JobStore, JobTransitionRow};
pub use secret::SecretStore;
pub use session::{SessionStore, StoredMessage};
pub use session_folder::{SessionFolderRow, SessionFolderStore};
pub use session_summary::{SessionSummaryRow, SessionSummaryStore};
pub use skill_risk::{AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore};
pub use task::{TaskPatch, TaskStore};
pub use trace::{SpanEventRow, SpanRow, StepRow, TraceStore};
