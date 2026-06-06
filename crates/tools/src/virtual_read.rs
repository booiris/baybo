use std::path::Path;

use async_trait::async_trait;
use aura_model::{SessionId, User};

/// Identity of the caller for a virtual read, so a [`VirtualReadResolver`] can
/// enforce access control. The resolver keys its content off this principal
/// (e.g. "serve only the caller's own session transcript"), never off an id
/// parsed from the requested path — so a fabricated path can't cross a
/// confidentiality boundary.
pub struct VirtualReadAccess<'a> {
    pub session_id: &'a SessionId,
    pub user: &'a User,
}

/// Resolves a `Read` of a path to **virtual** content — data with no on-disk
/// backing, materialised on demand (e.g. the session transcript served from
/// the store). [`crate::builtin::read::ReadTool`] consults the optional
/// resolver before it touches the filesystem:
///
/// - `None` → not a virtual path; fall through to the real filesystem read.
/// - `Some(Ok(text))` → the virtual content (the caller paginates it like a
///   real file read).
/// - `Some(Err(reason))` → the path is virtual but the read is refused (access
///   denied, or a load failure). The resolver audits denials itself.
///
/// A trait, injected via [`crate::ToolContext`], so `ReadTool` stays generic
/// and free of any domain/store dependency.
#[async_trait]
pub trait VirtualReadResolver: Send + Sync {
    async fn resolve(
        &self,
        path: &Path,
        access: &VirtualReadAccess<'_>,
    ) -> Option<Result<String, String>>;
}
