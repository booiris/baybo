use std::sync::Arc;
use std::time::Duration;

use aura_store::BlobStore;

use crate::error::JanitorError;

pub(crate) async fn purge_old_blobs(
    store: &Arc<dyn BlobStore>,
    ttl: Duration,
) -> Result<u64, JanitorError> {
    let ttl_us = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
    let cutoff_us = chrono::Utc::now().timestamp_micros().saturating_sub(ttl_us);
    Ok(store.purge_older_than(cutoff_us).await?)
}
