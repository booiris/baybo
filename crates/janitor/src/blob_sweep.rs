use std::sync::Arc;
use std::time::Duration;

use aura_storage::BlobStore;

use crate::error::JanitorError;

pub(crate) async fn purge_old_blobs(
    store: &Arc<dyn BlobStore>,
    ttl: Duration,
) -> Result<u64, JanitorError> {
    let cutoff = chrono::Utc::now().timestamp() - ttl.as_secs() as i64;
    Ok(store.purge_older_than(cutoff).await?)
}
