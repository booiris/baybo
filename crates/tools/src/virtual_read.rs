use std::path::Path;

use async_trait::async_trait;
use baybo_model::{SessionId, User};

/// Identity of the caller for a virtual read, so a [`VirtualReadResolver`] can
/// enforce access control. The resolver keys its content off this principal
/// (e.g. "serve only the caller's own session transcript"), never off an id
/// parsed from the requested path — so a fabricated path can't cross a
/// confidentiality boundary.
pub struct VirtualReadAccess<'a> {
    pub session_id: &'a SessionId,
    pub user: &'a User,
}

/// The `Read` call's requested page, passed through to the resolver so it can
/// materialise only what the page needs instead of rendering the whole virtual
/// file per page. Same semantics as the tool params: 1-based `offset` line,
/// `limit` lines, both optional.
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtualReadWindow {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Resolves a `Read` of a path to **virtual** content — data with no on-disk
/// backing, materialised on demand (e.g. the session transcript served from
/// the store). [`crate::builtin::read::ReadTool`] consults the optional
/// resolver before it touches the filesystem:
///
/// - `None` → not a virtual path; fall through to the real filesystem read.
/// - `Some(Ok(text))` → the finished `Read`-style output for `window` — the
///   resolver paginates + line-numbers itself (via
///   [`crate::paginate_numbered`]) so it can stop materialising content at the
///   window's end.
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
        window: &VirtualReadWindow,
    ) -> Option<Result<String, String>>;
}
