use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel send error: {0}")]
    Send(String),

    #[error("channel configuration error: {0}")]
    Config(String),

    /// Couldn't reach the channel transport at all (e.g. no gateway
    /// process listening on the UDS). Distinguished from [`Config`] so
    /// callers can tell "nothing on the other end" from "handshake
    /// failed on a live endpoint" — the auto-spawn gateway path in the
    /// TUI only fires on this variant.
    #[error("channel endpoint not reachable: {0}")]
    NotReachable(String),

    /// The endpoint answered and refused our credential (HTTP 401/403 on
    /// the upgrade). Split from [`NotReachable`] (nothing listening) and
    /// [`Config`] (live endpoint, malformed handshake) so the CLI can say
    /// "running but rejected our token" instead of "no gateway" — and so
    /// the TUI's auto-spawn path, which is gated on [`NotReachable`],
    /// never fires because a live gateway turned us away.
    #[error("channel endpoint rejected our credential: {0}")]
    Unauthorized(String),

    #[error("channel {0} already installed")]
    DuplicateChannel(String),
}

/// Dedicated, narrow error returned by
/// [`crate::SubscribedView::subscribe`]. The previous broader
/// [`ChannelError`] return type had three structurally-unreachable
/// variants in this context (`WrongKind` was killed alongside the
/// view migration; `Send` / `Config` / `NotReachable` / `Duplicate`
/// never originate from `subscribe`'s body). Surfacing only the one
/// real failure mode keeps `match` arms honest at the call site.
#[derive(Debug, Error)]
#[error("connection {0} not found on channel")]
pub struct ConnectionNotFoundError(pub String);
