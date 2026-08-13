//! The UniFFI-facing surface: records, callback interfaces, and the flat error
//! enum. Everything Swift sees crosses through these types; the internal modules
//! keep their `Result<_, String>` prose and the transport's `TransportError`,
//! which [`BayboError::from_msg`] folds into the two load-bearing variants +
//! prose at the boundary.

use crate::binding::NOT_BOUND_MSG;
use crate::direct::INVALID_TOKEN_CODE;

/// The FFI error surface. `InvalidToken` and `NotBound` used to be string codes
/// the webview matched on (`invalid_token` / the unbound prose); as enum variants
/// the cross-language contract can't drift with a rewording.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum BayboError {
    /// The app holds neither a relay pairing nor direct credentials.
    #[error("{}", NOT_BOUND_MSG)]
    NotBound,
    /// The gateway rejected the admin Bearer token (HTTP 401).
    #[error("{}", INVALID_TOKEN_CODE)]
    InvalidToken,
    /// Any other failure, carrying the leg's own prose verbatim.
    #[error("{message}")]
    Other { message: String },
}

impl BayboError {
    /// Fold an internal `String` error into the FFI enum: the two stable codes
    /// become their variants, everything else rides as prose.
    pub(crate) fn from_msg(message: String) -> Self {
        match message.as_str() {
            INVALID_TOKEN_CODE => Self::InvalidToken,
            NOT_BOUND_MSG => Self::NotBound,
            _ => Self::Other { message },
        }
    }
}

/// APNs environment of this build. Passed in from Swift (from the Xcode build
/// configuration) instead of `cfg!(debug_assertions)`: the Rust core is usually
/// compiled in release even for a debug app, so a Rust-side cfg would misreport
/// `production` for a sandbox-token build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

impl ApnsEnvironment {
    /// The wire string the gateway / pairing protocol expects.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }

    pub(crate) fn to_pairing(self) -> device_proto::pairing::ApnsEnv {
        match self {
            Self::Sandbox => device_proto::pairing::ApnsEnv::Sandbox,
            Self::Production => device_proto::pairing::ApnsEnv::Production,
        }
    }
}

/// Construction-time configuration for [`crate::BayboClient`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ClientConfig {
    /// Which APNs environment issued this build's tokens (development signing →
    /// `Sandbox`, distribution signing → `Production`).
    pub apns_env: ApnsEnvironment,
    /// Directory for the rotating `baybo.log` (2 MiB × 3 files — the exportable
    /// log bundle). `None` disables file logging (host tests).
    pub log_dir: Option<String>,
    /// Durable home for downloaded blobs, content-addressed by digest. `None`
    /// falls back to the OS temp dir, which iOS purges under storage pressure —
    /// fine for host tests, wrong for a file the user asked to keep. Nothing
    /// evicts from it; the embedder owns retention.
    pub blob_cache_dir: Option<String>,
}

/// Scan-to-pair target parsed from a `baybo://pair` QR payload.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairTarget {
    /// Relay base URL (the QR's `h=`, already defaulted when absent).
    pub endpoint: String,
    /// Public rendezvous id (the QR's `r=`).
    pub rendezvous_id: String,
    /// Hex Noise PSK (the QR's `s=`); decoded + length-checked at `pair_begin`.
    pub secret: String,
    /// Relay admission key (the QR's `k=`), if present.
    pub remote_api_key: Option<String>,
}

/// What the confirm screen renders after `pair_begin`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairChallenge {
    pub device_id: String,
    pub confirm_code: String,
}

impl From<crate::core::PairChallenge> for PairChallenge {
    fn from(c: crate::core::PairChallenge) -> Self {
        Self {
            device_id: c.device_id,
            confirm_code: c.confirm_code,
        }
    }
}

/// What the UI renders after a successful pairing (never the secrets).
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairedSummary {
    pub relay_node_id: String,
    pub rendezvous_id: String,
}

impl From<crate::core::PairedSummary> for PairedSummary {
    fn from(s: crate::core::PairedSummary) -> Self {
        Self {
            relay_node_id: s.relay_node_id,
            rendezvous_id: s.rendezvous_id,
        }
    }
}

/// One chat-session row for the native chat list, mirroring the gateway's
/// `ChatSessionSummary` (the web sidebar's row shape). Timestamps are RFC 3339
/// strings — Swift parses them for the age label.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSessionSummary {
    pub session_id: String,
    pub created_at: String,
    pub last_active: String,
    /// Preview drawn from the most-recent user-authored message; `None` for a
    /// session without a user turn yet.
    pub last_user_text: Option<String>,
    /// Preview drawn from the most-recent message regardless of author — the
    /// newest user prompt or final assistant answer with text. Drives the
    /// Telegram-style list's second line so the preview follows the
    /// conversation (an agent reply shows once it lands). `None` on an older
    /// gateway that predates the field, or a session with no displayable turn;
    /// the row falls back to `last_user_text` / its title.
    pub last_message_text: Option<String>,
    /// Conversation title — generated from the first user question, or set by
    /// a user rename — and `None` before either has happened. The list renders
    /// it as the row's bold first line; live updates arrive via
    /// [`SessionListSink::on_title`].
    pub title: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    /// Server-computed unread reply count (final assistant replies above this
    /// session's read cursor), capped at 99. Accurate across a cold restart /
    /// a device that missed the live `SessionActivity` pings — unlike a
    /// client-local ping counter. `0` when caught up.
    pub unread_count: i64,
    /// A tool call in this conversation is parked on the approval gate and
    /// cannot proceed until the user answers it. Server-computed from live
    /// gate state — a gateway restart kills every parked turn, so `false`
    /// after a restart is the truth, not a stale read.
    pub approval_pending: bool,
    /// The cron job this row is a fire of — the key the list groups on so a
    /// job's fires collapse into one row instead of flooding the list (a **cron
    /// group**; see `docs/cron-groups.md`). `None` for an ordinary chat.
    pub cron_job_id: Option<String>,
    /// That group's label: the job's live title, or the title snapshotted onto
    /// the fire once the job is deleted. `None` when the group cannot be named
    /// at all (a pre-snapshot fire whose job is gone) — the list leaves such a
    /// row flat rather than inventing a name.
    pub cron_job_title: Option<String>,
    /// Whether that GROUP is pinned to the top of the list. The bit lives on the
    /// cron JOB (`cron_jobs.pinned`) — the group is a view over its fires, and
    /// the job is the only object whose identity matches it — so every fire
    /// carries the same value and the list folds it into the one group row,
    /// exactly as it already does `cron_job_title`.
    ///
    /// NOT the same as `pinned`, which is this SESSION's own pin: pinning a fire
    /// lifts that one conversation out of its group, while pinning the group
    /// keeps the whole recurring stream at the top. A tombstone group (the job
    /// was deleted, its history kept) is always `false` — nothing is left to
    /// hold the bit.
    pub cron_group_pinned: bool,
}

/// One matching message inside a [`ChatSearchGroup`], mirroring the gateway's
/// `ChatSearchHit` (`GET /v1/chat/search`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSearchHit {
    /// Where this message sits in the session's transcript — the jump address.
    ///
    /// This is the ordinal to navigate to even when [`Self::superseded_by`] is
    /// set: the display read filters `compaction_inserted = 0`, so a superseded
    /// ORIGINAL still renders. Note the transcript keys a user row by its
    /// `platform_msg_id`, not by `m<ordinal>`, so a jump must resolve BY ordinal
    /// (`rowCoverageOrdinal`) rather than by building a row id — building one
    /// silently misses every user-authored hit.
    pub ordinal: i64,
    /// `"user"` / `"assistant"` — the gateway's `Role::as_str`, passed through
    /// rather than parsed: this side only labels it.
    pub role: String,
    /// The ORIGINAL prose, never the segmented index text. Highlight by
    /// substring — a phrase of unigrams matches exactly the substring typed, so
    /// a client-side match agrees with what the index matched.
    pub text: String,
    /// RFC 3339, parsed on the Swift side for the excerpt's timestamp.
    pub created_at: String,
    /// Set when compaction stamped this row: the ordinal where that compaction's
    /// re-inserted rows begin. **Not a jump target and not a "hidden" marker** —
    /// those rows carry `compaction_inserted = 1` and never render, while this
    /// hit's own `ordinal` does. Every row active at a compaction points at the
    /// same ordinal, so in a compacted conversation most hits carry it; it says
    /// the model's context was rewritten after this row, nothing about what the
    /// user can see. Carried so a client that wants to reason about compaction
    /// can, not so it can label the hit.
    pub superseded_by: Option<i64>,
}

/// One conversation's matches, collapsed into a single result card.
///
/// Grouped rather than listed flat because one chatty conversation would
/// otherwise fill the result set — measured on real data, a single conversation
/// took 15 of 30 slots and hid 10 others. The gateway groups over a window much
/// wider than it returns, so this cannot be reproduced client-side.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSearchGroup {
    pub session_id: String,
    /// The gateway's title, `None` before the title pass has run. The card
    /// prefers this device's own list row (same headline rule as the chat list)
    /// so one conversation is not named two different things in two places.
    pub session_title: Option<String>,
    /// Best-matching excerpts, best first, at most 3 (the gateway's cap).
    pub hits: Vec<ChatSearchHit>,
    /// Total matches in this conversation — equal to `hits.len()` at or under
    /// the cap, larger above it, so a card can say "and N more" without a second
    /// call.
    pub total_hits: i64,
}

/// A whole result set for one query (`GET /v1/chat/search`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSearchResults {
    /// Conversations, best match first; a conversation ranks by its best hit.
    pub groups: Vec<ChatSearchGroup>,
    /// The query matched more than the gateway's scan window, so some
    /// conversations are missing and `total_hits` undercounts. There is no
    /// cursor by design — a search box refines the query, it does not page.
    pub truncated: bool,
}

/// How far a subagent child got, mirroring the gateway's `ChatSubagentStatus`.
/// Derived from the child's TURN rows, never its session row: nothing rewrites
/// a child's session after creation, so its `last_active` sits on `created_at`
/// forever.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ChatSubagentStatus {
    /// Spawned, but its actor has not opened a turn yet.
    Pending,
    /// At least one turn is still open — the sheet polls a child in this state.
    Running,
    Completed,
    Failed,
    Cancelled,
    /// A status this build does not name. Both ends carry the arm on purpose: a
    /// gateway that grows a status must cost one row its label, not blank the
    /// whole sheet with a decode error.
    ///
    /// Do NOT read it as "terminal". The gateway only emits its own `Unknown`
    /// after ruling out a live turn, but `#[serde(other)]` also lands here for
    /// any FUTURE status string — including a non-terminal one — so a client
    /// that treats this arm as an end state (the read-only page stops polling
    /// on it) would freeze such a child mid-run until reopened.
    Unknown,
}

/// One direct subagent child of a conversation, mirroring the gateway's
/// `ChatSubagentSummary` (`GET /v1/chat/sessions/{id}/subagents`). Timestamps
/// are RFC 3339 strings, like [`ChatSessionSummary`]'s.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSubagentSummary {
    /// The child's own session id — the argument to
    /// [`crate::BayboClient::chat_fetch_subagent_history`], and to
    /// [`crate::BayboClient::chat_list_subagents`] when drilling further down.
    pub session_id: String,
    /// Profile the parent spawned (`explorer`, `general-purpose`, …).
    pub subagent_type: Option<String>,
    /// `"baybo"` for an in-process child, else the external agent's name
    /// (`"claude"` / `"codex"`).
    pub backend: String,
    /// The errand the parent authored. `None` for a child spawned before the
    /// gateway stamped it onto the child's title — the row falls back to
    /// `subagent_type`.
    pub task: Option<String>,
    pub status: ChatSubagentStatus,
    pub created_at: String,
    /// When its first turn began; `None` while [`ChatSubagentStatus::Pending`].
    pub started_at: Option<String>,
    /// When its last turn ended, `None` while anything is still open. A pair
    /// rather than a precomputed duration, so a running child's clock ticks
    /// without polling for it.
    pub ended_at: Option<String>,
}

/// A session's direct subagent children (`GET /v1/chat/sessions/{id}/subagents`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatSubagentList {
    /// Ascending by creation — the transcript's own direction, so the newest
    /// (usually the running one) is last.
    pub items: Vec<ChatSubagentSummary>,
    /// Older children exist below `items[0]`. The sheet pages back by handing
    /// that row's `(created_at, session_id)` back as [`SubagentCursor`] — the
    /// fan-out limiter bounds CONCURRENT breadth, not the cumulative count, so
    /// an overnight conversation really can leave hundreds behind.
    pub has_more_older: bool,
}

/// Keyset cursor into a parent's children: return those strictly OLDER than
/// this row. The id rides along because one turn's fan-out mints siblings
/// inside the same microsecond, which a timestamp alone cannot separate.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SubagentCursor {
    /// RFC 3339, verbatim from the row's `created_at`.
    pub created_at: String,
    pub session_id: String,
}

/// One selectable LLM entry for the chat header's model picker — a `baybo.json`
/// entry, narrowed from the gateway's `LlmModelEntry`. `name` is the entry name
/// (what a session pin references); the picker lists it by name and offers the
/// models it can serve (`model` + `model_candidates`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct LlmModelInfo {
    pub name: String,
    pub provider: String,
    /// The entry's default model id.
    pub model: String,
    /// Every model id this entry can serve, flattened from the gateway's
    /// `model_list` (which already includes `model`, so this list normally
    /// contains it — `ModelCatalog.models(of:)` de-dupes). A session pins any
    /// of them; the picker lists them under the entry. Empty only when the
    /// gateway sent no list at all.
    pub model_candidates: Vec<String>,
    /// The entry's reasoning-effort override — a rung on the thinking ladder
    /// (`off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`), or `None` for
    /// the provider's own default.
    pub reasoning_effort: Option<String>,
    /// The rungs THIS entry's provider can be told, cheapest first. The panel
    /// offers exactly these — an empty list means the provider takes no
    /// effort from baybo at all, and the Thinking row is hidden rather than
    /// offering picks that would never reach the wire.
    pub available_efforts: Vec<String>,
}

/// The gateway's configured LLM entries plus the current `default-llm` name
/// (`GET /v1/llm/models`, narrowed). A session with no pin resolves against
/// `default_name`, so the picker shows it as the "default" row's identity.
#[derive(Debug, Clone, uniffi::Record)]
pub struct LlmModelCatalog {
    pub default_name: String,
    pub items: Vec<LlmModelInfo>,
}

/// A session's model pin: the entry name (`last_llm`), the chosen model within
/// it (`last_model`), and the reasoning-effort pick (`last_effort`) — each
/// `None`. Returned by [`crate::BayboClient::chat_session_model`] to seed the
/// header picker.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionModelPin {
    pub llm: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// One RECURRING scheduled job, as the phone's cron list renders it — a
/// read-only mirror of the gateway's `CronJob` DTO, minus what a list has no use
/// for.
///
/// This is the JOB, not the **cron group** its fires collapse into: the group is
/// a view over conversations that already happened (`docs/cron-groups.md`),
/// while this is the schedule that produces them. A job with no fire yet has no
/// group at all, and still belongs on this list — that is most of why the list
/// exists.
///
/// **One-shots (`{"kind":"at"}`) are not here.** They are reminders rather than
/// schedules: they fire once and are over, so they neither recur nor need
/// managing, and the list is about what runs on a repeat. That is why the
/// schedule is a bare `expr` and not a sum type — see `list_cron_jobs`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CronJobSummary {
    pub id: String,
    /// Short human name — **empty** on a row minted before the field existed, in
    /// which case clients fall back to the prompt. The fallback stays on the
    /// client because it is presentation, and both other surfaces already make
    /// the same choice.
    pub title: String,
    pub prompt: String,
    /// The cron expression, evaluated in `timezone`.
    pub expr: String,
    /// IANA zone the expression is read in. Shown beside it: `0 9 * * *` means
    /// nothing without knowing whose 9am.
    pub timezone: String,
    pub status: CronJobStatus,
    /// RFC 3339. `None` when nothing is coming — i.e. the job is paused.
    pub next_trigger_at: Option<String>,
    /// RFC 3339. `None` until the job first fires.
    pub last_triggered_at: Option<String>,
    /// Whether the runtime owns this job (the dream pass). It pauses and
    /// reschedules like any other, but the server refuses to delete it — so
    /// the UI must not offer a delete the user can only be told "no" to.
    pub builtin: bool,
}

/// A live job's run state, mirroring the gateway's `CronStatus`. Orthogonal to
/// deletion — the list never carries a deleted job, so nothing here says
/// anything about the recycle bin.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum CronJobStatus {
    Enabled,
    /// Paused by hand: it keeps its schedule but has no next trigger.
    Disabled,
    /// A one-shot that has already run. Terminal, and not a failure.
    ///
    /// **Unreachable on this list**, and kept only because it is the wire's third
    /// status: `Executed` is set for one-shots alone (`mark_one_shot_executed`),
    /// one-shots are filtered out, and editing one into a recurring schedule
    /// resets it to `Enabled` (`update_job`). Rendering it truthfully if it ever
    /// did arrive beats dropping the row on the floor.
    Executed,
}

/// One deck card row, mirroring the gateway's `DeckCardDto` (`GET /v1/deck`
/// and `GET /v1/deck/recycle`). Flat primitives + unix-millis timestamps, the
/// [`ChatSessionSummary`] pattern.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeckCardInfo {
    pub card_id: String,
    pub title: String,
    /// Ordered slot in the deck's 2-column flow layout.
    pub position: i64,
    /// Size class: `"small"` (1×1), `"wide"` (2×1), or `"large"` (2×2).
    pub size: String,
    /// The grid sizes the card's `card.html` implements — the ⤢ resize cycle
    /// stays inside this set, and a single entry hides the resize control.
    /// Always contains `size`.
    pub sizes: Vec<String>,
    /// Whether the card declares a maximized (full-screen) layout — the ⛶
    /// affordance the shell renders in the tile's top-right.
    pub maximize: bool,
    pub enabled: bool,
    /// Auto-disabled by the gateway supervisor (crash/timeout budget blown);
    /// the card renders an error face with a Re-enable action.
    pub quarantined: bool,
    /// Unix millis; populated only on recycle-bin rows.
    pub deleted_at_ms: Option<i64>,
    pub spec_hash: String,
    /// The card's persisted push counter — the seed for the client's seq
    /// cursor (accept a [`DeckSink::on_card_data`] push iff its seq is
    /// greater than the cached one).
    pub last_seq: i64,
    pub created_at_ms: i64,
    /// Ops whose mandatory `x-baybo-retryable` declaration is `true`. The
    /// caller passes the per-op verdict into [`deck_call`]'s `retryable` so
    /// the relay leg knows whether a silent-death call may be replayed.
    pub retryable_ops: Vec<String>,
}

/// Latest stored snapshot for one card — the deck's instant paint source.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeckSnapshotInfo {
    pub card_id: String,
    /// Gateway-assigned monotonic emit counter for the card.
    pub seq: i64,
    /// The card's own JSON text, opaque to this layer; empty when `error` is
    /// present.
    pub payload: String,
    pub fetched_at_ms: i64,
    pub error: Option<String>,
}

/// `GET /v1/deck`: live cards (ordered by position) plus the latest snapshot
/// per card. A [`DeckSink::on_deck_changed`] nudge means refetch this whole
/// view and replace cached snapshots + seqs unconditionally.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeckView {
    pub cards: Vec<DeckCardInfo>,
    pub snapshots: Vec<DeckSnapshotInfo>,
}

/// One entry of the full ordered layout write (`PUT /v1/deck/layout`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeckLayoutEntryInput {
    pub card_id: String,
    pub position: i64,
    /// `"small"` | `"wide"` | `"large"` — the gateway rejects anything else.
    pub size: String,
}

/// Result of the per-send durability point lookup
/// (`GET /v1/chat/sessions/{id}/messages?platform_msg_id=…`), consumed by the
/// native outbox: `found: false` is a provable absence (the key was never
/// persisted for this session) that lets the retry machine resume; `found:
/// true` confirms durability without consuming a retry transmission.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageLookup {
    pub found: bool,
    /// Ordinal of the newest persisted row carrying the key, when found.
    pub ordinal: Option<i64>,
}

/// A content-addressed attachment reference on an outbound message (already
/// uploaded via `blob_upload_bytes`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentRef {
    pub kind: AttachmentKind,
    pub blob_id: String,
    pub mime_type: String,
    pub size: u32,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AttachmentKind {
    Image,
    Audio,
    File,
}

/// A user's answer to a pending tool-approval prompt, echoed back to the
/// gateway as `Frame::ResolveApproval`.
///
/// Deliberately NARROWER than `baybo_model::ApprovalDecision`, which also has
/// an `ApproveAlways` (a standing, session-wide grant for every resource the
/// call touches). Other clients offer it; the phone does not — a mis-tap is
/// likeliest here, and a standing grant is the one decision you can't undo by
/// paying more attention next time. Leaving the variant out of the FFI means
/// the app cannot send it even by accident. Decisions made ELSEWHERE still
/// arrive and render — this type is the send side only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

/// Outcome of [`crate::BayboClient::blob_bytes_for_display`] — the iOS scheme
/// handler maps each case to a `WKURLSchemeTask` response for deck imagery or
/// an HTML preview. Collapses the shape check, cache-first read, leg-bound
/// download, and display-cap decisions into one call so Swift stays thin.
#[derive(Debug, uniffi::Enum)]
pub enum BlobServeOutcome {
    /// The blob's bytes, within the display cap.
    Bytes { data: Vec<u8> },
    /// Present but larger than the caller's display cap — the `<img>` should
    /// fail to render rather than pull it into memory.
    OverCap,
    /// Not cached and undownloadable right now (no active leg, or gone).
    NotFound,
    /// Not a well-formed `sha256:<digest>.<token>` capability id.
    BadId,
}

impl From<ApprovalDecision> for crate::core::WireApprovalDecision {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Approve => Self::Approve,
            ApprovalDecision::Deny => Self::Deny,
        }
    }
}

impl From<AttachmentRef> for crate::core::WireAttachment {
    fn from(a: AttachmentRef) -> Self {
        let kind = match a.kind {
            AttachmentKind::Image => crate::core::AttachmentKind::Image,
            AttachmentKind::Audio => crate::core::AttachmentKind::Audio,
            AttachmentKind::File => crate::core::AttachmentKind::File,
        };
        Self {
            kind,
            blob_id: a.blob_id,
            mime_type: a.mime_type,
            size: a.size,
            filename: a.filename,
            // Outbound user sends: the composer can't attach audio, so there
            // is never a duration to carry.
            duration_ms: None,
        }
    }
}

/// Byte progress for one `blob_download_bytes` call. Calls arrive on the core's
/// tokio workers and are rate-limited by the core, not per network chunk — a
/// 100 MiB download would otherwise cross the FFI thousands of times.
#[uniffi::export(with_foreign)]
pub trait BlobProgress: Send + Sync {
    /// `downloaded` counts the bytes already on disk for this blob, so a
    /// **resumed** download opens above zero rather than snapping back. `total`
    /// is the blob's full length when the server declared one; the caller
    /// usually knows it already from the attachment's `size`.
    fn on_progress(&self, downloaded: u64, total: Option<u64>);
}

/// Where a subscribed chat session's frames land. The binding owns one global
/// chat leg; each `chat_connect` registers/replaces the sink for that
/// `session_id`. Calls arrive on the core's tokio workers, so the Swift
/// implementation must be thread-safe (hop to the main actor before touching UI).
#[uniffi::export(with_foreign)]
pub trait FrameSink: Send + Sync {
    /// One inbound `wire::Frame`, serialized as JSON (the same shape the web
    /// transcript already consumes). `Ping` is answered inside the pump and never
    /// surfaces here.
    fn on_frame(&self, frame_json: String);

    /// The global chat leg ended ON ITS OWN (peer closed, liveness watchdog,
    /// Noise desync). Deliberate teardown — `chat_disconnect`, `logout` —
    /// aborts the pump before this fires, so it signals only unsolicited death;
    /// every subscribed owner reconnects with backoff.
    fn on_disconnected(&self, session_id: String);
}

/// Where connection-global session-activity pings land. The gateway broadcasts a
/// throttled `Frame::SessionActivity` for ANY session on the binding's leg —
/// subscribed or not — so the chat list can bump unread + recency without
/// subscribing every session. One sink per client, registered once via
/// [`BayboClient::set_session_list_sink`]; both legs share it (only one is live
/// at a time). Calls arrive on the core's tokio workers, so the Swift impl must
/// be thread-safe (hop to the main actor before touching UI).
#[uniffi::export(with_foreign)]
pub trait SessionListSink: Send + Sync {
    /// `source` is the lowercase `ActivityKind` (`"user"` / `"assistant"`);
    /// `at_millis` is the activity's unix-epoch milliseconds.
    fn on_activity(&self, session_id: String, source: String, at_millis: i64);
    /// A `SessionUpdated` patch carried a freshly-generated conversation
    /// `title` for `session_id`. Connection-global like `on_activity` — it
    /// fires for ANY session on the leg (subscribed or not), so the list can
    /// swap a row's bold first line the moment the title pass lands without a
    /// REST refetch. Only the title patch is forwarded; pin / archive / hide
    /// stay on the optimistic + REST-merge path the list already owns.
    fn on_title(&self, session_id: String, title: String);
    /// A `SessionUpdated` patch flipped `session_id`'s approval-gate bit: a
    /// tool call there is now waiting on the user (`true`), or its last prompt
    /// was answered, cancelled, or timed out (`false`).
    ///
    /// Connection-global like `on_title`, and that is the entire point. The
    /// prompt frame itself (`Frame::ApprovalRequested`) only reaches
    /// connections subscribed to that session, so a device parked on the chat
    /// list — the one that needs to know which conversation to open — would
    /// never see it. `false` is also the ONLY signal a timed-out gate emits:
    /// it self-denies after five minutes and broadcasts no resolution.
    fn on_approval_pending(&self, session_id: String, pending: bool);
    /// The device's session-list mirror may be behind the gateway's: refetch it.
    ///
    /// Fires on `Frame::Gap { session_id: None }` — the gateway's "I dropped a
    /// session-less broadcast" nudge, which is exactly how a `SessionActivity`
    /// for a brand-new session goes missing. That frame carries no session id,
    /// so without this hop it would be fanned to the per-session frame sinks
    /// and reach *nobody* while the user is parked on the chat list with
    /// nothing subscribed — precisely the moment the list is on screen and a
    /// missed row matters. See `docs/sync-protocol.md` ("refetch the session
    /// list") and `docs/cron-groups.md` (a cron fire is the row this loses).
    fn on_list_stale(&self);
}

/// Where the connection-global deck frames land — the [`SessionListSink`]
/// pattern. `Frame::DeckCardData` / `Frame::DeckChanged` are session-less
/// broadcasts (the deck tab has no session to subscribe), so the transport
/// routes them here and NEVER to a per-session [`FrameSink`]. One sink per
/// client, registered via [`crate::BayboClient::set_deck_sink`]; both legs
/// share it. Calls arrive on the core's tokio workers, so the Swift impl must
/// be thread-safe (hop to the main actor before touching UI).
///
/// The Swift implementor must NOT be named `DeckSinkImpl` — UniFFI generates a
/// class of exactly that name for every `with_foreign` trait, and a same-named
/// class collides (cf. `SessionActivityHandler` in `App/Core/SessionIndex.swift`).
#[uniffi::export(with_foreign)]
pub trait DeckSink: Send + Sync {
    /// A card's service emitted a fresh snapshot. Accept iff `seq` beats the
    /// cached seq for `card_id`; `payload` is the card's own JSON text.
    fn on_card_data(&self, card_id: String, seq: u32, payload: String);
    /// Deck structure changed (install, update, delete, restore, enable,
    /// disable, quarantine, layout). No payload — refetch
    /// [`crate::BayboClient::deck_fetch`].
    fn on_deck_changed(&self);
}

/// Gateway-side cancellation of an in-flight pairing (the operator declined or
/// the link dropped) while the confirm screen is up — dismiss it instead of
/// hanging until the user taps.
#[uniffi::export(with_foreign)]
pub trait PairAbortListener: Send + Sync {
    fn on_abort(&self, reason: String);
}

/// The `invalid_token` signal travels from the leg to Swift as an UNTYPED STRING
/// through three hops — `TransportError::Other(code)` → `.to_string()` →
/// [`BayboError::from_msg`]'s exact match. Every hop is an equality on the same
/// literal, so one well-meaning `format!("ws connect: {e}")` anywhere on the path
/// downgrades `InvalidToken` to `Other { message }` and the login screen silently
/// stops rendering "token rejected". The tests below pin each hop.
#[cfg(test)]
mod tests {
    use super::*;

    use crate::transport::TransportError;

    /// Hop 1 → 2: the leg's error carries the bare code, and stringifying it (what
    /// `transport::connect` does at the FFI boundary) must not decorate it.
    #[test]
    fn stringifying_the_transport_error_preserves_the_bare_code() {
        let err = TransportError::Other(INVALID_TOKEN_CODE.to_string());
        assert_eq!(err.to_string(), INVALID_TOKEN_CODE);
    }

    /// Hop 3: the exact-match fold at the boundary.
    #[test]
    fn the_bare_code_folds_into_the_invalid_token_variant() {
        assert!(matches!(
            BayboError::from_msg(INVALID_TOKEN_CODE.to_string()),
            BayboError::InvalidToken
        ));
        assert!(matches!(
            BayboError::from_msg(NOT_BOUND_MSG.to_string()),
            BayboError::NotBound
        ));
    }

    /// All three hops end to end, exactly as `chat_connect` runs them.
    #[test]
    fn a_401_from_the_leg_reaches_swift_as_invalid_token() {
        let from_leg = TransportError::Other(INVALID_TOKEN_CODE.to_string());
        let over_the_seam = from_leg.to_string();
        assert!(matches!(
            BayboError::from_msg(over_the_seam),
            BayboError::InvalidToken
        ));
    }

    /// The regression this whole chain is fragile to: a decorated message no longer
    /// matches, so it lands in `Other` and the login screen loses its token-rejected
    /// state. If this test ever fails because someone made the match fuzzy, make
    /// sure they did it on purpose.
    #[test]
    fn a_decorated_code_does_not_fold_and_is_therefore_forbidden_on_the_path() {
        let decorated = format!("ws connect: {INVALID_TOKEN_CODE}");
        assert!(matches!(
            BayboError::from_msg(decorated),
            BayboError::Other { .. }
        ));
    }

    /// The variants render back as the same codes they folded from, so an error
    /// crossing the boundary twice (a leg error re-stringified) stays stable.
    #[test]
    fn the_error_variants_render_as_the_codes_they_fold_from() {
        assert_eq!(BayboError::InvalidToken.to_string(), INVALID_TOKEN_CODE);
        assert_eq!(BayboError::NotBound.to_string(), NOT_BOUND_MSG);
        assert!(matches!(
            BayboError::from_msg(BayboError::InvalidToken.to_string()),
            BayboError::InvalidToken
        ));
    }
}
