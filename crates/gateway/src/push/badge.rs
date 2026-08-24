//! The app-icon badge number carried on a push.
//!
//! The count rides **inside the sealed preview plaintext**, not `aps.badge`,
//! and the iOS Notification Service Extension applies it locally. That keeps
//! the remote host blind — it already relays the preview without being able to
//! read it, and an absolute unread count is exactly the kind of activity
//! metadata the rest of this module goes out of its way to hide (the
//! collapse-id is hashed, the session id lives inside the AEAD). It also means
//! **no remote-host change and no deploy ordering** for a badge digit: the
//! `/notify` body is byte-identical, so every already-deployed host keeps
//! working.
//!
//! The number is always an absolute *set*, never an increment, so it
//! self-heals: a dropped push, an NSE that ran out of budget, or a stale value
//! is corrected by the next push or the next time the app comes forward.

use baybo_model::SessionId;
use baybo_session::SessionManager;

use crate::api::admin::chat::{UNREAD_COUNT_CAP, is_excluded_from_global_chat};

/// Ceiling on the icon badge. iOS renders large numbers as a wide pill that
/// crowds the icon, and past a point the digit stops being information.
const BADGE_MAX: i64 = 999;

/// Total unread replies across every conversation the user can actually see,
/// or `None` when the count could not be established.
///
/// `None` is meaningful: the caller omits the `badge` key entirely, and APNs
/// reads a missing badge as "leave the icon alone". Sending `0` on a failed
/// count would instead wipe a truthful badge.
///
/// The visible set is [`list_sessions`](crate::api::admin::chat)'s, plus
/// `!archived`: the iOS client buckets archived conversations onto their own
/// screen, so counting them would push the icon past anything the main list
/// can account for. It is written out here rather than shared with
/// `list_sessions`, whose predicate is parameterised by the `include_hidden` /
/// `include_cron` query flags — a flag-taking shared helper would obscure both
/// call sites more than the duplication does.
pub(crate) async fn unread_badge_total(session_manager: &SessionManager) -> Option<i64> {
    let channel = crate::api::admin::chat::chat_list_channel(None);
    let sessions = match session_manager.list_by_channel(&channel).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(error = %e, "push: badge count unavailable (session list failed)");
            return None;
        }
    };
    let ids: Vec<SessionId> = sessions
        .into_iter()
        .filter(|s| !s.hidden && !s.archived && !is_excluded_from_global_chat(s))
        .map(|s| s.id)
        .collect();
    match session_manager.unread_total(&ids, UNREAD_COUNT_CAP).await {
        Ok(total) => Some((total as i64).min(BADGE_MAX)),
        Err(e) => {
            tracing::warn!(error = %e, "push: badge count unavailable (unread scan failed)");
            None
        }
    }
}
