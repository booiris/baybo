//! Conversions between the rich trace domain types (`Step` / `Span` /
//! `SpanEvent`) and their persistence rows in `baybo-store`.
//!
//! The `TraceStore` trait lives in `baybo-store` and trades in rows, so
//! the lifecycle recorder's logic stays in this crate while the store
//! contract sits alongside every other one. Callers convert at the
//! boundary via these helpers.

use baybo_model::ToolSetHash;
use sha2::{Digest, Sha256};

use crate::{LlmToolSet, Result, Span, SpanEvent, Step, TraceError};
use baybo_store::{SpanEventRow, SpanRow, StepRow, ToolSetRow};

impl Step {
    pub fn to_row(&self) -> Result<StepRow> {
        Ok(StepRow {
            id: self.id,
            data: serde_json::to_string(self)
                .map_err(|e| TraceError::Storage(format!("serialize step: {e}")))?,
        })
    }

    pub fn from_row(row: StepRow) -> Result<Step> {
        serde_json::from_str(&row.data)
            .map_err(|e| TraceError::Storage(format!("deserialize step: {e}")))
    }
}

impl Span {
    pub fn to_row(&self) -> Result<SpanRow> {
        Ok(SpanRow {
            id: self.id,
            data: serde_json::to_string(self)
                .map_err(|e| TraceError::Storage(format!("serialize span: {e}")))?,
        })
    }

    pub fn from_row(row: SpanRow) -> Result<Span> {
        serde_json::from_str(&row.data)
            .map_err(|e| TraceError::Storage(format!("deserialize span: {e}")))
    }
}

impl SpanEvent {
    pub fn to_row(&self) -> Result<SpanEventRow> {
        Ok(SpanEventRow {
            span_id: self.span_id,
            seq: self.seq,
            data: serde_json::to_string(self)
                .map_err(|e| TraceError::Storage(format!("serialize span_event: {e}")))?,
        })
    }

    pub fn from_row(row: SpanEventRow) -> Result<SpanEvent> {
        serde_json::from_str(&row.data)
            .map_err(|e| TraceError::Storage(format!("deserialize span_event: {e}")))
    }
}

impl LlmToolSet {
    /// Serialize once and key the row by the digest of those very bytes,
    /// so the hash can never name a body that was serialized differently.
    ///
    /// The digest covers the serialized JSON — `serde_json` emits struct
    /// fields in declaration order and object keys in the order the
    /// schema `Value` holds them, which is deterministic for a given tool
    /// registry. That is all the dedup needs; the hash is a storage key,
    /// not a security boundary.
    pub fn to_row(&self) -> Result<ToolSetRow> {
        let data = serde_json::to_string(self)
            .map_err(|e| TraceError::Storage(format!("serialize tool set: {e}")))?;
        let digest: [u8; 32] = Sha256::digest(data.as_bytes()).into();
        Ok(ToolSetRow {
            hash: ToolSetHash::from_digest(&digest),
            data,
        })
    }

    pub fn from_row(row: ToolSetRow) -> Result<LlmToolSet> {
        serde_json::from_str(&row.data)
            .map_err(|e| TraceError::Storage(format!("deserialize tool set: {e}")))
    }
}

impl From<baybo_store::StorageError> for TraceError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::NotFound(s) => TraceError::NotFound(s),
            baybo_store::StorageError::Internal(e) => TraceError::Internal(e),
            other => TraceError::Storage(other.to_string()),
        }
    }
}
