//! Conversions between the rich trace domain types (`Step` / `Span` /
//! `SpanEvent`) and their persistence rows in `aura-store`.
//!
//! The `TraceStore` trait lives in `aura-store` and trades in rows, so
//! the lifecycle recorder's logic stays in this crate while the store
//! contract sits alongside every other one. Callers convert at the
//! boundary via these helpers.

use crate::{Result, Span, SpanEvent, Step, TraceError};
use aura_store::{SpanEventRow, SpanRow, StepRow};

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

impl From<aura_store::StorageError> for TraceError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::NotFound(s) => TraceError::NotFound(s),
            aura_store::StorageError::Internal(e) => TraceError::Internal(e),
            other => TraceError::Storage(other.to_string()),
        }
    }
}
