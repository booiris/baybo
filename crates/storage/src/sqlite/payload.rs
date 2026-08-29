use std::io::Cursor;

use rusqlite::types::{FromSqlError, ValueRef};
use sha2::{Digest, Sha256};

/// Small JSON rows are cheaper to leave in place: a second b-tree lookup and
/// a zstd frame cost more than they save. The rows driving database growth are
/// well above this boundary (tool results and trace payloads are commonly
/// several KiB to hundreds of KiB).
const MIN_PAYLOAD_BYTES: usize = 1024;

const ZSTD_LEVEL: i32 = 3;
const PAYLOAD_PREFIX: &str = "sha256:";
const OWNER_REFERENCES: &[(&str, &str)] = &[
    ("session_messages", "content_payload_hash"),
    ("turns", "data_payload_hash"),
    ("steps", "data_payload_hash"),
    ("spans", "data_payload_hash"),
    ("span_events", "data_payload_hash"),
    ("llm_tool_sets", "data_payload_hash"),
];

/// A text value prepared for an owning row plus the shared payload table.
/// `payload` is present only when zstd won the size comparison.
pub(super) struct EncodedText {
    pub(super) inline: String,
    pub(super) hash: Option<String>,
    payload: Option<Vec<u8>>,
    original_size: usize,
}

impl EncodedText {
    pub(super) fn prepare(text: String, compressed_projection: String) -> anyhow::Result<Self> {
        Self::prepare_with(text, |_| Ok(compressed_projection))
    }

    pub(super) fn prepare_json_fields(text: String, fields: &[&str]) -> anyhow::Result<Self> {
        Self::prepare_with(text, |text| json_projection(text, fields))
    }

    fn prepare_with(
        text: String,
        compressed_projection: impl FnOnce(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<Self> {
        if text.len() < MIN_PAYLOAD_BYTES {
            return Ok(Self::inline(text));
        }

        let compressed = zstd::bulk::compress(text.as_bytes(), ZSTD_LEVEL)
            .map_err(|e| anyhow::anyhow!("compress content payload: {e}"))?;
        if compressed.len() >= text.len() {
            return Ok(Self::inline(text));
        }
        let compressed_projection = compressed_projection(&text)?;

        let mut digest = Sha256::new();
        digest.update(text.as_bytes());
        let hash = format!("{PAYLOAD_PREFIX}{}", hex::encode(digest.finalize()));
        Ok(Self {
            inline: compressed_projection,
            hash: Some(hash),
            payload: Some(compressed),
            original_size: text.len(),
        })
    }

    fn inline(text: String) -> Self {
        Self {
            inline: text,
            hash: None,
            payload: None,
            original_size: 0,
        }
    }

    /// Insert the immutable payload before its owning row references it.
    /// Callers put both writes in one transaction.
    pub(super) fn persist(&self, conn: &rusqlite::Connection) -> anyhow::Result<()> {
        let (Some(hash), Some(payload)) = (&self.hash, &self.payload) else {
            return Ok(());
        };
        conn.execute(
            "INSERT OR IGNORE INTO content_payloads \
             (hash, codec, original_size, data) VALUES (?1, 'zstd', ?2, ?3)",
            rusqlite::params![hash, self.original_size as i64, payload],
        )?;
        Ok(())
    }
}

/// Read a value projected by one of the `*_read` views. Inline rows are TEXT;
/// payload-backed rows are the compressed BLOB selected by the view.
pub(super) fn read_text(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<String> {
    match row.get_ref(index)? {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).map(str::to_owned).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                bytes.len(),
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        }),
        ValueRef::Blob(bytes) => {
            let decoded = zstd::stream::decode_all(Cursor::new(bytes)).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    bytes.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(e),
                )
            })?;
            String::from_utf8(decoded).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    bytes.len(),
                    rusqlite::types::Type::Blob,
                    Box::new(e),
                )
            })
        }
        ValueRef::Null => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Null,
            Box::new(FromSqlError::InvalidType),
        )),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            other.data_type(),
            Box::new(FromSqlError::InvalidType),
        )),
    }
}

/// Install the one source of truth for payload ownership. Owner writes and the
/// count update run in the same SQLite transaction; a mutable span/turn can
/// therefore release its previous body without scanning every owner table.
pub(super) fn install_refcount_triggers(conn: &rusqlite::Connection) -> anyhow::Result<()> {
    for (table, column) in OWNER_REFERENCES {
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS payload_ref_{table}_insert \
                 AFTER INSERT ON {table} \
                 WHEN NEW.{column} IS NOT NULL \
             BEGIN \
                 UPDATE content_payloads SET ref_count = ref_count + 1 \
                  WHERE hash = NEW.{column}; \
             END; \
             CREATE TRIGGER IF NOT EXISTS payload_ref_{table}_update \
                 AFTER UPDATE OF {column} ON {table} \
                 WHEN OLD.{column} IS NOT NEW.{column} \
             BEGIN \
                 UPDATE content_payloads SET ref_count = ref_count - 1 \
                  WHERE hash = OLD.{column} AND ref_count > 0; \
                 UPDATE content_payloads SET ref_count = ref_count + 1 \
                  WHERE hash = NEW.{column}; \
                 DELETE FROM content_payloads \
                  WHERE hash = OLD.{column} AND ref_count = 0; \
             END; \
             CREATE TRIGGER IF NOT EXISTS payload_ref_{table}_delete \
                 AFTER DELETE ON {table} \
                 WHEN OLD.{column} IS NOT NULL \
             BEGIN \
                 UPDATE content_payloads SET ref_count = ref_count - 1 \
                  WHERE hash = OLD.{column} AND ref_count > 0; \
                 DELETE FROM content_payloads \
                  WHERE hash = OLD.{column} AND ref_count = 0; \
             END;"
        ))?;
    }
    Ok(())
}

/// Remove a candidate inserted by an idempotent owner write that lost its race.
pub(super) fn delete_unreferenced(
    conn: &rusqlite::Connection,
    hash: &str,
) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "DELETE FROM content_payloads AS payload \
         WHERE payload.hash = ?1 AND payload.ref_count = 0",
        rusqlite::params![hash],
    )?)
}

/// Keep only the fields SQLite-generated columns need while the canonical JSON
/// body lives in `content_payloads`.
fn json_projection(data: &str, fields: &[&str]) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| anyhow::anyhow!("parse payload projection source: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("payload projection source is not a JSON object"))?;
    let mut projected = serde_json::Map::new();
    for field in fields {
        if let Some(value) = object.get(*field) {
            projected.insert((*field).to_string(), value.clone());
        }
    }
    serde_json::to_string(&projected)
        .map_err(|e| anyhow::anyhow!("serialize payload projection: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_stays_inline() {
        let text = "hello".to_string();
        let encoded = EncodedText::prepare(text.clone(), "{}".into()).expect("prepare");
        assert_eq!(encoded.inline, text);
        assert!(encoded.hash.is_none());
    }

    #[test]
    fn compressible_text_gets_a_stable_hash() {
        let text = "tool output\n".repeat(2_000);
        let first = EncodedText::prepare(text.clone(), "[]".into()).expect("prepare");
        let second = EncodedText::prepare(text, "[]".into()).expect("prepare");
        assert_eq!(first.inline, "[]");
        assert_eq!(first.hash, second.hash);
        assert!(
            first
                .hash
                .as_deref()
                .is_some_and(|h| h.starts_with(PAYLOAD_PREFIX))
        );
    }

    #[test]
    fn projection_keeps_only_index_fields() {
        let projected = json_projection(
            r#"{"step_id":"s","started_at":"now","large":"discard"}"#,
            &["step_id", "started_at", "ended_at"],
        )
        .expect("projection");
        let value: serde_json::Value = serde_json::from_str(&projected).expect("json");
        assert_eq!(value["step_id"], "s");
        assert_eq!(value["started_at"], "now");
        assert!(value.get("large").is_none());
    }
}
