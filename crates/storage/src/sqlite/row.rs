use baybo_store::StorageError;
use rusqlite::{Error, Row, Rows, types::ValueRef};

/// Decode a domain record while the SQLite row is borrowed. Only value
/// failures belong to a record. A broken projection or failed query must
/// still reach the caller, including on a list read.
pub(super) struct RowDecoder<T> {
    pub table: &'static str,
    pub key: &'static str,
    pub decode: fn(&Row<'_>) -> anyhow::Result<T>,
}

impl<T> RowDecoder<T> {
    fn read(&self, row: &Row<'_>) -> anyhow::Result<T> {
        match (self.decode)(row) {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(sql) = error.downcast_ref::<Error>()
                    && !matches!(
                        sql,
                        Error::InvalidColumnType(..)
                            | Error::FromSqlConversionFailure(..)
                            | Error::IntegralValueOutOfRange(..)
                            | Error::Utf8Error(..)
                    )
                {
                    return Err(error);
                }
                let key = match row.get_ref(self.key)? {
                    ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                    ValueRef::Integer(value) => value.to_string(),
                    _ => "<unreadable>".to_owned(),
                };
                Err(StorageError::Corrupt {
                    table: self.table,
                    key,
                    reason: error.to_string(),
                }
                .into())
            }
        }
    }

    pub(super) fn complete(&self, rows: Rows<'_>) -> anyhow::Result<Vec<T>> {
        rows.and_then(|row| self.read(row)).collect()
    }

    pub(super) fn required(&self, rows: Rows<'_>) -> anyhow::Result<T> {
        self.optional(rows)?
            .ok_or_else(|| Error::QueryReturnedNoRows.into())
    }

    pub(super) fn optional(&self, mut rows: Rows<'_>) -> anyhow::Result<Option<T>> {
        rows.next()?.map(|row| self.read(row)).transpose()
    }

    pub(super) fn list(&self, mut rows: Rows<'_>) -> anyhow::Result<Vec<T>> {
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            match self.read(row) {
                Ok(value) => values.push(value),
                Err(error) => match error.downcast_ref() {
                    Some(StorageError::Corrupt { table, key, .. }) => {
                        tracing::warn!(table, key, "skipping unreadable SQLite row");
                    }
                    _ => return Err(error),
                },
            }
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTEGER: RowDecoder<i64> = RowDecoder {
        table: "items",
        key: "id",
        decode: |row| Ok(row.get("value")?),
    };

    #[test]
    fn a_broken_projection_or_sql_error_always_fails_a_list() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for sql in [
            "SELECT 1 AS id, 2 AS wrong_column",
            "SELECT 1 AS id, 2 AS value UNION ALL SELECT 2, json_extract('invalid', '$')",
        ] {
            let mut stmt = conn.prepare(sql).unwrap();
            let error = INTEGER.list(stmt.query([]).unwrap()).unwrap_err();
            assert!(error.is::<rusqlite::Error>());
        }
    }
}
