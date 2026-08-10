import Foundation

/// One chat-list row. The device-local truth for sessions this device opened;
/// on a direct binding the REST list merges over it (see `merge(remote:)`).
struct SessionRow: Codable, Identifiable, Equatable {
    /// The session id.
    let id: String
    var createdAt: Date
    var lastActive: Date
    /// Conversation title: generated server-side from the first user question,
    /// or set by the user renaming it from the web; nil until either has
    /// happened. The list's bold first line; live-updated by a
    /// `SessionUpdated` patch (`SessionIndex.applyTitle`). iOS is
    /// receive-only — there is no rename affordance here yet.
    var title: String?
    /// Second-line preview: the most-recent message regardless of author (user
    /// prompt or agent reply). Captured locally on send, reconciled to server
    /// truth (`last_message_text`) on merge; nil for a session with no
    /// displayable turn yet.
    var preview: String?
    /// The most-recent USER message (server `last_user_text`). Drives the bold
    /// first line's fallback (a short snippet) when there's no title yet — kept
    /// separate from `preview`, which follows the agent's reply too.
    var userText: String?
    var pinned: Bool
    /// Server-side flag (like `pinned`): archived rows live under the Archived
    /// screen instead of the main list, but keep accruing unread/recency.
    var archived: Bool
    /// Local-only unread counter — the server never surfaces it. Bumped by a
    /// `SessionActivity` ping for a backgrounded session, cleared on open.
    var unread: Int
    /// The cron job this row is a fire of — the key the list groups on so one
    /// job's fires collapse into a single row instead of flooding the list (a
    /// **cron group**; see `docs/cron-groups.md`). `nil` for an ordinary chat.
    var cronJobId: String?
    /// That group's label (the job's live title, else the name it had when the
    /// job was deleted). `nil` when the group cannot be named at all — such a
    /// row stays flat rather than joining a nameless group.
    var cronJobTitle: String?
    /// Whether this row's cron GROUP is pinned. Server-owned like `pinned`, but
    /// the bit lives on the JOB (`cron_jobs.pinned`) — a group is a view over
    /// its fires, so the job is the only object that can hold it. Every fire of
    /// the job carries the same value and the list folds it into the one group
    /// row. `false` on an ordinary chat, and on a tombstone group (job deleted).
    var cronGroupPinned: Bool
    /// A tool call in this conversation is parked on the approval gate and the
    /// user's decision is the only thing that will unblock it. Server-computed
    /// (`approvalPending` on the summary) and reconciled on every merge; the
    /// connection-global `SessionUpdated` patch is the between-pulls
    /// accelerator.
    ///
    /// Deliberately NOT restored from disk — see `init(from:)`.
    var approvalPending: Bool

    init(
        id: String, createdAt: Date, lastActive: Date, title: String? = nil,
        preview: String?, userText: String? = nil, pinned: Bool, archived: Bool = false,
        unread: Int = 0, cronJobId: String? = nil, cronJobTitle: String? = nil,
        cronGroupPinned: Bool = false, approvalPending: Bool = false
    ) {
        self.id = id
        self.createdAt = createdAt
        self.lastActive = lastActive
        self.title = title
        self.preview = preview
        self.userText = userText
        self.pinned = pinned
        self.archived = archived
        self.unread = unread
        self.approvalPending = approvalPending
        self.cronJobId = cronJobId
        self.cronJobTitle = cronJobTitle
        self.cronGroupPinned = cronGroupPinned
    }

    /// The pre-Telegram schema stored the preview under `lastUserText`; decode
    /// it as a fallback so an existing `sessions.json` keeps its captured
    /// previews across the upgrade instead of flashing blank until the first
    /// REST merge.
    private enum LegacyKeys: String, CodingKey { case lastUserText }

    /// `unread` / `archived` / `title` post-date earlier schemas and `preview`
    /// renamed `lastUserText` — decode each defensively so an older
    /// `sessions.json` upgrades in place instead of failing the whole decode.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        createdAt = try c.decode(Date.self, forKey: .createdAt)
        lastActive = try c.decode(Date.self, forKey: .lastActive)
        title = try c.decodeIfPresent(String.self, forKey: .title)
        // The pre-Telegram schema stored the user's last message under
        // `lastUserText`; fall back to it for BOTH the preview and the user-text
        // label so an existing sessions.json upgrades in place.
        var legacy: String?
        if let legacyContainer = try? decoder.container(keyedBy: LegacyKeys.self) {
            legacy = (try? legacyContainer.decodeIfPresent(String.self, forKey: .lastUserText)) ?? nil
        }
        preview = try c.decodeIfPresent(String.self, forKey: .preview) ?? legacy
        userText = try c.decodeIfPresent(String.self, forKey: .userText) ?? legacy
        pinned = try c.decode(Bool.self, forKey: .pinned)
        archived = try c.decodeIfPresent(Bool.self, forKey: .archived) ?? false
        unread = try c.decodeIfPresent(Int.self, forKey: .unread) ?? 0
        // Miss these two and the whole cron grouping silently vanishes on a cold
        // start — the rows load, they just forget which job they belong to. Note
        // `PersistedFormatTests` only asserts a key is PRESENT in the encoded
        // JSON, so it cannot catch a forgotten decode; `SessionIndexMergeTests`
        // round-trips through disk for exactly that reason.
        cronJobId = try c.decodeIfPresent(String.self, forKey: .cronJobId)
        cronJobTitle = try c.decodeIfPresent(String.self, forKey: .cronJobTitle)
        cronGroupPinned = try c.decodeIfPresent(Bool.self, forKey: .cronGroupPinned) ?? false
        // NOT decoded, on purpose. A parked approval lives in the GATEWAY's
        // memory: it self-denies after five minutes, and a gateway restart
        // drops every one of them. A mark loaded off disk could therefore only
        // ever be a claim about a prompt that no longer exists — an offline
        // cold start would show "waiting for your approval" on a conversation
        // that stopped waiting hours ago, with no way to act on it. The launch
        // merge is the only thing allowed to raise it.
        //
        // It is still ENCODED (synthesized) so `SessionRow` stays one shape;
        // `PersistedFormatTests` asserts key presence, not round-tripping.
        approvalPending = false
    }
}

/// The device-local session registry backing the chat list on BOTH legs. Remote
/// refreshes from direct REST or the relay API tunnel merge into this same
/// rendering source. Rows persist as one small JSON file in Application Support;
/// transcript mirrors live next to it (`TranscriptStore`).
@MainActor
final class SessionIndex: ObservableObject {
    static let shared = SessionIndex(
        supportDirectory: supportDirectory(), defaults: .standard, ownsAppBadge: true)

    private static let indexFileName = "sessions.json"

    /// Mirrors the gateway CHAT-LIST `PREVIEW_MAX_CHARS` (`api/admin/chat.rs`
    /// — 120, NOT push's 200) so a locally-captured preview and a REST-fetched
    /// one truncate identically; run every capture through `previewText`.
    static let previewMaxChars = 120

    /// Collapse whitespace and clip to `previewMaxChars` with a trailing
    /// ellipsis — the Swift mirror of the gateway's `truncate_preview`. A local
    /// capture run through this equals the server's `last_message_text` /
    /// `last_user_text`, so reconciling it on the next `merge` changes nothing
    /// (no visible jump). Unicode-scalar counting matches Rust's `chars()`.
    static func previewText(_ text: String) -> String {
        let collapsed = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        let scalars = collapsed.unicodeScalars
        if scalars.count <= previewMaxChars { return collapsed }
        return String(String.UnicodeScalarView(scalars.prefix(previewMaxChars))) + "…"
    }
    /// The latest user-intended archive/hide state for a session whose request
    /// hasn't resolved yet. `merge(remote:)` consults it so a refresh racing an
    /// in-flight mutation can't rewind the optimistic flip (or re-insert a row
    /// an in-flight DELETE just removed).
    enum PendingMutation: Equatable {
        case archived(Bool)
        case pinned(Bool)
        case hidden
    }

    @Published private(set) var rows: [SessionRow] = []

    /// Bumped whenever the gateway tells us our list mirror is behind — see
    /// [`noteListStale`]. `ChatListScreen` observes it and refetches. Not
    /// persisted: staleness is a fact about *this* connection, not about disk.
    @Published private(set) var listStaleEpoch: UInt64 = 0

    /// The Application Support root this registry — and the transcript mirrors
    /// and outboxes it wipes — lives under. Injected rather than resolved
    /// from the static path so a test drives an isolated tree: the suites run in
    /// PARALLEL, and a shared `sessions.json` turns one suite's writes into
    /// another's "logic bug".
    private let supportDirectory: URL
    private let defaults: UserDefaults
    private let fileURL: URL
    /// The session whose `ChatScreen` is on top. A `SessionActivity` ping for it
    /// is not counted as unread (the user is looking at it); `nil` on the list.
    private var foregroundSessionId: String?
    /// In-memory only: a kill mid-flight loses the intent and the next merge
    /// restores server truth, which is the honest fallback.
    private var pendingMutations: [String: PendingMutation] = [:]
    /// In-flight **cron group** pin intents, keyed by JOB id — a different id
    /// space from `pendingMutations`' session ids, which is exactly why it is a
    /// separate map rather than another `PendingMutation` case.
    private var pendingCronPins: [String: Bool] = [:]
    /// The group-pin value last acknowledged by the server, staged when a job's
    /// first pending pin lands, so a chained failure rolls back to truth rather
    /// than to the negation of a flip that never happened.
    private var pinnedCronBaselines: [String: Bool] = [:]
    /// Rows removed by `beginHide`, kept for the failure rollback (the mirror is
    /// deleted with the row and stays gone — a rolled-back delete refetches its
    /// history on re-entry).
    private var hiddenBackups: [String: SessionRow] = [:]
    /// The archived value last acknowledged by the server, staged when a
    /// session's FIRST pending mutation lands. A chained failure (archive →
    /// undo, both dead offline) must roll back here — negating the failed
    /// intent would re-archive a row the server never archived.
    private var archiveBaselines: [String: Bool] = [:]
    /// Same idea as `archiveBaselines`, for the pin toggle.
    private var pinBaselines: [String: Bool] = [:]
    /// Bumped on every mutation stage/resolve. A list fetch that STARTED
    /// before a mutation resolved is a stale snapshot even after the pending
    /// entry is gone — `merge` compares epochs and drops it.
    private(set) var mutationEpoch = 0

    /// Whether this instance drives the app-icon badge. True only for
    /// [`shared`]: the suites run in parallel and construct their own indexes
    /// against temp directories, and every one of those writing the host app's
    /// real badge would make the number a race between unrelated tests.
    private let ownsAppBadge: Bool

    init(supportDirectory: URL, defaults: UserDefaults, ownsAppBadge: Bool = false) {
        self.supportDirectory = supportDirectory
        self.defaults = defaults
        self.ownsAppBadge = ownsAppBadge
        fileURL = supportDirectory.appendingPathComponent(Self.indexFileName)
        rows = Self.load(from: fileURL)
        migrateLegacySingleSession()
        // A cold launch must correct a badge left over from pushes that landed
        // while the app was dead — including down to zero, which no later
        // mutation would necessarily produce.
        publishBadge()
    }

    /// Pinned block first, then most recently active — the web sidebar's order.
    var sorted: [SessionRow] {
        rows.sorted {
            if $0.pinned != $1.pinned { return $0.pinned }
            return $0.lastActive > $1.lastActive
        }
    }

    func contains(sessionId: String) -> Bool {
        rows.contains { $0.id == sessionId }
    }

    /// Ensure a row exists for a session being opened. Does NOT bump
    /// `lastActive` on an existing row — ordering means message activity, not
    /// visits.
    func touch(sessionId: String) {
        guard !rows.contains(where: { $0.id == sessionId }) else { return }
        let now = Date()
        rows.append(
            SessionRow(
                id: sessionId, createdAt: now, lastActive: now,
                preview: nil, pinned: false))
        save()
    }

    /// A user message left this device: capture the preview + activity locally
    /// so the list is correct even where remote refresh is unavailable
    /// (offline/failed tunnel). An attachment-only send (empty text) describes
    /// its attachments instead of leaving the row showing the PREVIOUS message
    /// — the gateway's own `last_message_preview` is `None` for a media-only
    /// turn and `merge` keeps a local preview over a nil remote one, so this is
    /// that row's only second line. `userText` stays the user's last real
    /// question: it is the title fallback, and "Attachment: x.pdf" names
    /// nothing.
    func recordUserSend(sessionId: String, text: String, attachments: [AttachmentRef] = []) {
        let typed = Self.previewText(text)
        let preview = typed.isEmpty ? Self.attachmentPreview(attachments) : typed
        let now = Date()
        if let idx = rows.firstIndex(where: { $0.id == sessionId }) {
            // The just-sent message is both the newest message overall and
            // the newest USER message, so it drives the preview AND the
            // bold-line fallback until REST reconciles.
            if !preview.isEmpty {
                rows[idx].preview = preview
            }
            if !typed.isEmpty {
                rows[idx].userText = typed
            }
            rows[idx].lastActive = now
        } else {
            rows.append(
                SessionRow(
                    id: sessionId, createdAt: now, lastActive: now,
                    preview: preview.isEmpty ? nil : preview,
                    userText: typed.isEmpty ? nil : typed, pinned: false))
        }
        save()
    }

    /// The chat-list second line for an attachment-only send: the picks' names,
    /// marked as attachments. Run through `previewText` like every other
    /// capture, so a locally-shown preview truncates exactly the way a
    /// server-sent one would. A nameless batch (a photo carries no filename)
    /// falls back to the count.
    static func attachmentPreview(_ attachments: [AttachmentRef]) -> String {
        guard !attachments.isEmpty else { return "" }
        let names = attachments.compactMap { ref -> String? in
            let name = (ref.filename ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            return name.isEmpty ? nil : name
        }
        guard !names.isEmpty else {
            return previewText(
                String(format: Lang.shared.t("chat.previewAttachments"), attachments.count))
        }
        return previewText(
            String(
                format: Lang.shared.t("chat.previewAttachment"),
                names.joined(separator: ", ")))
    }

    /// The agent's final reply landed on this (foreground or still-resident)
    /// session's leg — capture it as the row's second-line preview locally, the
    /// assistant-side mirror of `recordUserSend`. Without it the list keeps
    /// showing the user's last message until the return-to-list REST merge swaps
    /// in `last_message_text`, a visible jump; recency and title already update
    /// live (the `SessionActivity` ping / `SessionUpdated` patch), so the preview
    /// was the one row field with no live path. Mirrors the gateway's tool-free
    /// final-answer rule; an empty reply (attachment-only) keeps the prior
    /// preview, matching the gateway's media-only skip. `lastActive` / `userText`
    /// are untouched — recency rides the ping and `userText` stays the user-only
    /// label. Unknown ids are ignored; a later merge surfaces them.
    func recordAgentReply(sessionId: String, text: String) {
        let preview = Self.previewText(text)
        guard !preview.isEmpty,
            let idx = rows.firstIndex(where: { $0.id == sessionId }),
            rows[idx].preview != preview
        else { return }
        rows[idx].preview = preview
        save()
    }

    // MARK: - Live activity (SessionActivity pings)

    /// A connection-global `SessionActivity` ping (see `SessionActivityHandler`):
    /// bump the row's recency and, unless it's the foreground session, its unread
    /// count. Both `user` and `assistant` sources count, matching the web sidebar.
    ///
    /// An id we've never listed is the gateway telling us about a session that
    /// does not exist on this device yet — a cron fire, or a chat started on
    /// another client. That is not a no-op: it is the ONLY live signal such a row
    /// exists, so it nudges a list refetch (`listStaleEpoch`). Returning silently
    /// here — as this used to — meant a user parked on the chat list watched a
    /// scheduled job fire and saw *nothing*, until they backgrounded the app or
    /// pulled to refresh. (The web sidebar has always refetched on an unknown id.)
    func noteActivity(sessionId: String, source: String, atMillis: Int64) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else {
            noteListStale()
            return
        }
        let at = Date(timeIntervalSince1970: Double(atMillis) / 1000)
        var changed = false
        if at > rows[idx].lastActive {
            rows[idx].lastActive = at
            changed = true
        }
        if sessionId != foregroundSessionId {
            rows[idx].unread += 1
            changed = true
        }
        if changed { save() }
    }

    /// The device's list mirror may be behind the gateway's — refetch it.
    ///
    /// Two callers, one meaning. A `SessionActivity` for a session we have never
    /// listed (above), and `Frame::Gap { session_id: nil }` — the gateway's "I
    /// dropped a session-less broadcast", which is *how* such a ping goes missing
    /// in the first place. Both say the same thing: what you are rendering is not
    /// what the server has.
    ///
    /// A counter rather than a `Bool` flag: two nudges in a row must produce two
    /// refreshes, and a flag that is already `true` would swallow the second.
    /// Observers debounce; this side never decides *when* to refetch.
    func noteListStale() {
        listStaleEpoch &+= 1
    }

    /// A `ChatScreen` came to the foreground: mark it current and clear its badge.
    func enterSession(_ sessionId: String) {
        foregroundSessionId = sessionId
        clearUnread(sessionId)
        reconcileAppBadge()
    }

    /// The foreground `ChatScreen` went away (pop / switch). Only clears the
    /// marker if it still points at `sessionId` (a fast switch may have moved it).
    func leaveSession(_ sessionId: String) {
        if foregroundSessionId == sessionId {
            foregroundSessionId = nil
        }
    }

    func clearUnread(_ sessionId: String) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }), rows[idx].unread != 0
        else { return }
        rows[idx].unread = 0
        save()
    }

    /// Optimistic clear behind a cron group's "mark all read" — one save, one
    /// `@Published` edit, so the badge drops in a single frame rather than N.
    func clearUnread(_ sessionIds: [String]) {
        let targets = Set(sessionIds)
        var changed = false
        for idx in rows.indices where targets.contains(rows[idx].id) && rows[idx].unread != 0 {
            rows[idx].unread = 0
            changed = true
        }
        if changed { save() }
    }

    /// Reassert the absolute system badge after the NSE may have changed it in
    /// its separate process without updating `BadgeCenter`'s local memo.
    func reconcileAppBadge() {
        publishBadge(force: true)
    }

    /// A live `SessionUpdated` title patch reached the connection-global list
    /// sink (`SessionActivityHandler.onTitle`): swap the row's bold first line
    /// in place. Title is server-authoritative and never mutated locally, so it
    /// applies unconditionally for a known row; an unknown id waits for a REST
    /// merge to surface it (the title is also carried on the summary).
    func applyTitle(sessionId: String, title: String) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }), rows[idx].title != title
        else { return }
        rows[idx].title = title
        save()
    }

    /// A connection-global `SessionUpdated{approval_pending}` reached the list
    /// sink: a tool call in `sessionId` is now waiting on the user, or its last
    /// prompt just went away.
    ///
    /// An unknown id means the gateway is telling us about a conversation this
    /// device has no row for — one started on the web, or a cron fire — and the
    /// single thing that conversation needs is the user. Nudge a refetch so the
    /// row appears, exactly as `noteActivity` does. A CLEAR for an unknown id
    /// is nothing to act on.
    func noteApprovalPending(sessionId: String, pending: Bool) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else {
            if pending { noteListStale() }
            return
        }
        guard rows[idx].approvalPending != pending else { return }
        rows[idx].approvalPending = pending
        save()
    }

    // MARK: - Optimistic archive / hide (server mutations in flight)

    /// Optimistic archive flip: the row moves between the main and archived
    /// lists at once; the staged intent shields it from a racing merge until
    /// the PUT resolves. No-ops for a row that is gone or has a delete in
    /// flight — a stale undo toast must not overwrite a hide intent.
    func beginArchive(_ sessionId: String, archived: Bool) {
        guard pendingMutations[sessionId] != .hidden,
            let idx = rows.firstIndex(where: { $0.id == sessionId })
        else { return }
        if pendingMutations[sessionId] == nil {
            archiveBaselines[sessionId] = rows[idx].archived
        }
        pendingMutations[sessionId] = .archived(archived)
        mutationEpoch += 1
        setArchivedFlag(sessionId, archived: archived)
    }

    /// Optimistic **cron group** pin flip. Keyed by the JOB, not a session: the
    /// group is a view over its fires and the bit lives on the job, so the flip
    /// touches every member row (that is what carries it into the bucketing) and
    /// the staged intent is held per job id.
    ///
    /// Deliberately NOT routed through `pendingMutations` / `mutationEpoch`: those
    /// are keyed by session id and a job id is a different id space (aliasing them
    /// would let a job id shadow a session's staged archive). A racing merge
    /// consults `pendingCronPins` instead.
    func beginCronPin(jobId: String, pinned: Bool) {
        if pinnedCronBaselines[jobId] == nil {
            pinnedCronBaselines[jobId] = rows.first { $0.cronJobId == jobId }?.cronGroupPinned
                ?? false
        }
        pendingCronPins[jobId] = pinned
        setCronGroupPinnedFlag(jobId: jobId, pinned: pinned)
    }

    /// The staged group pin reached the server: remote truth takes over again.
    func finishCronPin(jobId: String) {
        pendingCronPins.removeValue(forKey: jobId)
        pinnedCronBaselines.removeValue(forKey: jobId)
    }

    /// The group-pin PUT failed: rewind to the last server-acknowledged value
    /// (not the negation of the failed intent — a failed pin→unpin chain would
    /// otherwise re-pin a group the server never pinned).
    func rollBackCronPin(jobId: String) {
        pendingCronPins.removeValue(forKey: jobId)
        guard let baseline = pinnedCronBaselines.removeValue(forKey: jobId) else { return }
        setCronGroupPinnedFlag(jobId: jobId, pinned: baseline)
    }

    private func setCronGroupPinnedFlag(jobId: String, pinned: Bool) {
        var changed = false
        for idx in rows.indices where rows[idx].cronJobId == jobId {
            if rows[idx].cronGroupPinned != pinned {
                rows[idx].cronGroupPinned = pinned
                changed = true
            }
        }
        if changed { save() }
    }

    /// Optimistic pin flip: the row re-sorts to the top block at once; the
    /// staged intent shields it from a racing merge until the PUT resolves.
    /// No-ops for a gone row or one with a delete in flight.
    func beginPin(_ sessionId: String, pinned: Bool) {
        guard pendingMutations[sessionId] != .hidden,
            let idx = rows.firstIndex(where: { $0.id == sessionId })
        else { return }
        if pendingMutations[sessionId] == nil {
            pinBaselines[sessionId] = rows[idx].pinned
        }
        pendingMutations[sessionId] = .pinned(pinned)
        mutationEpoch += 1
        setPinnedFlag(sessionId, pinned: pinned)
    }

    /// Optimistic delete (soft-hide): remove the row now — and its transcript
    /// mirror with it — and suppress the row's remote existence in `merge` until
    /// the DELETE resolves. The mirror goes FIRST, ahead of the row guard: the
    /// user asked for this conversation to be gone, and a row that a racing merge
    /// already dropped (deleted from another client while the confirm was up)
    /// must still take its transcript with it rather than strand the file.
    func beginHide(_ sessionId: String) {
        pendingMutations[sessionId] = .hidden
        mutationEpoch += 1
        TranscriptStore.delete(sessionId: sessionId, in: supportDirectory)
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else { return }
        hiddenBackups[sessionId] = rows[idx]
        rows.remove(at: idx)
        save()
    }

    /// Optimistic batch hide behind a **cron group's delete** — [`beginHide`]'s
    /// contract per id, under one save and one `@Published` edit, so the group
    /// row leaves in a single frame rather than N (the same reason
    /// `clearUnread(_:[String])` exists). Mirrors go first here too: whether a
    /// racing merge already dropped a row decides nothing about its transcript.
    ///
    /// Deleting a group deletes only its *execution records*. The job is another
    /// object entirely and is not touched — it keeps firing, and the group comes
    /// back with its next fire.
    func beginHideMany(_ sessionIds: [String]) {
        let targets = Set(sessionIds)
        guard !targets.isEmpty else { return }
        for sessionId in targets {
            pendingMutations[sessionId] = .hidden
            TranscriptStore.delete(sessionId: sessionId, in: supportDirectory)
        }
        mutationEpoch += 1
        for row in rows where targets.contains(row.id) {
            hiddenBackups[row.id] = row
        }
        rows.removeAll { targets.contains($0.id) }
        save()
    }

    /// Drop one conversation's transcript mirror while KEEPING its row — the
    /// per-session resync escape hatch (`ChatStore.resync`). Every other mirror
    /// delete here means the conversation itself is gone; this one means the
    /// opposite: the conversation stays and its local rendering is thrown away
    /// so the cold-open path can rebuild it from the gateway.
    func dropTranscriptMirror(sessionId: String) {
        TranscriptStore.delete(sessionId: sessionId, in: supportDirectory)
    }

    func pendingMutation(for sessionId: String) -> PendingMutation? {
        pendingMutations[sessionId]
    }

    /// The staged intent reached the server (or was superseded and re-sent):
    /// remote truth takes over again.
    func finishMutation(_ sessionId: String) {
        pendingMutations.removeValue(forKey: sessionId)
        hiddenBackups.removeValue(forKey: sessionId)
        archiveBaselines.removeValue(forKey: sessionId)
        pinBaselines.removeValue(forKey: sessionId)
        mutationEpoch += 1
    }

    /// Archive PUT failed with the intent still current: rewind to the last
    /// server-acknowledged value (NOT the failed intent's negation — after a
    /// failed archive→undo chain that negation would re-archive a row the
    /// server never archived).
    func rollBackArchive(_ sessionId: String) {
        pendingMutations.removeValue(forKey: sessionId)
        mutationEpoch += 1
        guard let baseline = archiveBaselines.removeValue(forKey: sessionId) else { return }
        setArchivedFlag(sessionId, archived: baseline)
    }

    /// Pin PUT failed: rewind to the last server-acknowledged value.
    func rollBackPin(_ sessionId: String) {
        pendingMutations.removeValue(forKey: sessionId)
        mutationEpoch += 1
        guard let baseline = pinBaselines.removeValue(forKey: sessionId) else { return }
        setPinnedFlag(sessionId, pinned: baseline)
    }

    /// Hide DELETE failed: re-insert the removed row (its mirror stays gone;
    /// re-entry refetches history).
    func rollBackHide(_ sessionId: String) {
        pendingMutations.removeValue(forKey: sessionId)
        archiveBaselines.removeValue(forKey: sessionId)
        mutationEpoch += 1
        guard let row = hiddenBackups.removeValue(forKey: sessionId),
            !rows.contains(where: { $0.id == row.id })
        else { return }
        rows.append(row)
        save()
    }

    /// Batch hide failed: re-insert every row it removed. The user made ONE
    /// gesture, so it rewinds as one — a partial local state would be a list
    /// nobody asked for. The gateway refuses the whole batch if any id is out of
    /// scope, but a failure mid-write can still leave some rows hidden there; the
    /// next merge reconciles those back out, since remote wins for existence.
    func rollBackHideMany(_ sessionIds: [String]) {
        var restored: [SessionRow] = []
        for sessionId in sessionIds {
            pendingMutations.removeValue(forKey: sessionId)
            archiveBaselines.removeValue(forKey: sessionId)
            if let row = hiddenBackups.removeValue(forKey: sessionId) {
                restored.append(row)
            }
        }
        mutationEpoch += 1
        let present = Set(rows.map(\.id))
        let fresh = restored.filter { !present.contains($0.id) }
        guard !fresh.isEmpty else { return }
        rows.append(contentsOf: fresh)
        save()
    }

    /// The batch reached the server: drop every staged intent it held.
    func finishHideMany(_ sessionIds: [String]) {
        for sessionId in sessionIds {
            finishMutation(sessionId)
        }
    }

    #if DEBUG
        /// Stamp a row's cron-group key by hand. The DEBUG demo seed only —
        /// production rows get this from the gateway, which derives it from the
        /// fire's trigger and never lets a client author it. Exists so the group
        /// row is screenshot-verifiable with no gateway and no scheduled job.
        func setCronGroupForDemo(_ sessionId: String, jobId: String, title: String) {
            guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else { return }
            rows[idx].cronJobId = jobId
            rows[idx].cronJobTitle = title
            save()
        }
    #endif

    /// Plain local flip with no staged intent — rollback and the DEBUG demo
    /// seed use it; user-driven flips go through `beginArchive`.
    func setArchivedFlag(_ sessionId: String, archived: Bool) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }),
            rows[idx].archived != archived
        else { return }
        rows[idx].archived = archived
        save()
    }

    /// Plain local pin flip — rollback and the DEBUG demo seed use it;
    /// user-driven flips go through `beginPin`.
    func setPinnedFlag(_ sessionId: String, pinned: Bool) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }),
            rows[idx].pinned != pinned
        else { return }
        rows[idx].pinned = pinned
        save()
    }

    /// Merge the direct leg's REST list (the full non-hidden truth). Remote wins
    /// for existence — a local row missing remotely was hidden/deleted from
    /// another client — and for row fields, unless the local row saw activity
    /// after the remote snapshot (a just-sent message racing the refetch).
    /// Empty remote rows that this device has never listed are draft sessions:
    /// keep them out of the chat list until a send records local activity, or
    /// the gateway reports user-authored preview text. In-flight mutations
    /// (`pendingMutations`) beat the fetched snapshot: a pending archive flip
    /// wins over the remote value, and a pending hide suppresses the remote row
    /// entirely (this rebuild would otherwise re-insert it). Callers capture
    /// `mutationEpoch` BEFORE fetching: a snapshot older than the last mutation
    /// stage/resolve is dropped whole (it could rewind a flip whose pending
    /// entry has already cleared, or resurrect a just-deleted row); the next
    /// refresh re-merges.
    func merge(remote: [ChatSessionSummary], fetchEpoch: Int) {
        guard fetchEpoch == mutationEpoch else { return }
        var merged: [SessionRow] = []
        let local = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0) })
        for summary in remote {
            let pending = pendingMutations[summary.sessionId]
            if pending == .hidden { continue }
            let mine = local[summary.sessionId]
            // Newest-message preview wins; fall back to the user-only label so
            // an older gateway (no `last_message_text`) still renders a preview.
            let remotePreview = summary.lastMessageText ?? summary.lastUserText
            let hasRemotePreview = !(remotePreview ?? "").isEmpty
            // `approvalPending` joins the admission test because an
            // agent-started conversation can block on its very first tool call,
            // before any displayable turn exists — filtering it out as an empty
            // draft would drop precisely the row the user most needs to see.
            guard mine != nil || hasRemotePreview || summary.pinned || summary.approvalPending
            else { continue }
            // Remote title is authoritative, but a live patch may have set it
            // before this (older) snapshot caught up — keep the local one when
            // the snapshot has none rather than blanking a title just shown.
            let title = summary.title ?? mine?.title

            let archived: Bool
            if case .archived(let flag)? = pending {
                archived = flag
            } else {
                archived = summary.archived
            }
            let pinned: Bool
            if case .pinned(let flag)? = pending {
                pinned = flag
            } else {
                pinned = summary.pinned
            }
            let createdAt = Self.parseDate(summary.createdAt)
            let lastActive = Self.parseDate(summary.lastActive)
            // `unread` is now server-computed (`unreadCount`), so the pull
            // reconciles the badge to the truth — accurate across a cold
            // restart / a device that missed the live `SessionActivity` pings.
            // The live ping (`noteActivity`) still bumps it between pulls as a
            // cheap accelerator.
            let unread = Int(summary.unreadCount)
            // Server data is authoritative — adopt the snapshot's preview / user
            // text / recency wholesale. A local row NEVER overrides a server
            // value; it only fills a field the server left nil. (The old
            // "keep local when mine.lastActive > lastActive" guard compared the
            // DEVICE clock `recordUserSend` stamps against the SERVER clock, so a
            // device running ahead pinned a stale just-sent preview forever — the
            // list showed the question while the transcript already held the
            // reply.) The optimistic just-sent preview still shows instantly
            // between refreshes; a merge simply reconciles it to server truth.
            merged.append(
                SessionRow(
                    id: summary.sessionId, createdAt: createdAt, lastActive: lastActive,
                    title: title,
                    preview: remotePreview ?? mine?.preview,
                    userText: summary.lastUserText ?? mine?.userText,
                    pinned: pinned, archived: archived, unread: unread,
                    // Server-owned, like pin/archive: the group a fire belongs to
                    // is a fact about the session, and the label follows the job's
                    // LIVE title, so a rename lands here with no local rewrite.
                    cronJobId: summary.cronJobId, cronJobTitle: summary.cronJobTitle,
                    // The group's pin lives on the JOB, so an in-flight local flip
                    // is staged per JOB (not per session) and beats this snapshot
                    // — the same rule `pendingMutations` enforces for a row's own
                    // pin, applied to the object that actually holds the bit.
                    cronGroupPinned: summary.cronJobId
                        .flatMap { pendingCronPins[$0] } ?? summary.cronGroupPinned,
                    // Adopted wholesale, with no `pendingMutations` shielding:
                    // unlike pin / archive / hide this is never a local intent.
                    // The client cannot decide a conversation is waiting on the
                    // user — only the gateway's gate knows, and it is the
                    // arbiter across every client.
                    approvalPending: summary.approvalPending))
        }
        // A row the server no longer lists was deleted from another client, and
        // this rebuild is where it leaves the list for good. Its transcript goes
        // with it: `beginHide` is the only other per-session mirror delete, and it
        // never runs for a delete performed elsewhere — so without this the
        // conversation the user deleted on the web would keep its full transcript
        // on this phone forever. A targeted set difference, NOT a directory sweep:
        // rows still staged in `pendingMutations` are owned by beginHide /
        // rollBackHide, and a draft (never a row) is never considered here.
        let survivors = Set(merged.map(\.id))
        for row in rows
        where !survivors.contains(row.id) && pendingMutations[row.id] == nil {
            TranscriptStore.delete(sessionId: row.id, in: supportDirectory)
        }
        rows = merged
        save()
    }

    /// Logout / rebind: the rows belong to the old gateway — drop them, their
    /// transcript mirrors, and any staged mutations against it.
    func removeAll() {
        rows = []
        pendingMutations = [:]
        pendingCronPins = [:]
        pinnedCronBaselines = [:]
        hiddenBackups = [:]
        archiveBaselines = [:]
        pinBaselines = [:]
        mutationEpoch += 1
        save()
        TranscriptStore.deleteAll(in: supportDirectory)
        OutboxStore.deleteAll(in: supportDirectory)
    }

    // MARK: - Persistence

    /// Writes the registry — and NOTHING else. It used to end by pruning the
    /// transcript mirrors down to the ten most recently active rows, which made
    /// the mirror useless for exactly the sessions it exists to serve: `save()`
    /// runs on every list merge (including the one the pop back from a chat
    /// fires) and on every activity ping, `rows` is the gateway's whole session
    /// list ranked by the SERVER's `lastActive`, and reading a chat bumps
    /// nothing — so a conversation outside the ten most recently MESSAGED ones
    /// had the mirror it just wrote deleted seconds later, and opened blank on
    /// every re-entry until the network answered. Mirrors are now kept for every
    /// session this device has rendered; they are dropped only when the user
    /// deletes the session (`beginHide`) or unbinds the gateway (`removeAll`).
    private func save() {
        // Every list mutation funnels through here, which makes it the one
        // place the icon can be kept in step with the rows without scattering
        // badge writes across a dozen call sites. `BadgeCenter` coalesces, so
        // the saves that do not move the number cost nothing.
        publishBadge()
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        guard let data = try? encoder.encode(rows) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    private func publishBadge(force: Bool = false) {
        guard ownsAppBadge else { return }
        BadgeCenter.apply(BadgeCenter.total(rows), force: force)
    }

    private static func load(from url: URL) -> [SessionRow] {
        guard let data = try? Data(contentsOf: url) else { return [] }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return (try? decoder.decode([SessionRow].self, from: data)) ?? []
    }

    /// Pre-list installs persisted exactly one session pointer + transcript
    /// blob in UserDefaults. Fold them into the registry once so the existing
    /// conversation shows up instead of orphaning, then retire the keys (the
    /// per-session mirror file takes over).
    private func migrateLegacySingleSession() {
        guard let sessionId = defaults.string(forKey: ChatDefaults.sessionId) else { return }
        if !rows.contains(where: { $0.id == sessionId }) {
            let now = Date()
            rows.append(
                SessionRow(
                    id: sessionId, createdAt: now, lastActive: now,
                    preview: nil, pinned: false))
        }
        if let blob = defaults.string(forKey: ChatDefaults.transcriptState) {
            TranscriptStore.write(sessionId: sessionId, stateJson: blob, in: supportDirectory)
        }
        defaults.removeObject(forKey: ChatDefaults.sessionId)
        defaults.removeObject(forKey: ChatDefaults.transcriptState)
        defaults.removeObject(forKey: ChatDefaults.lastOrdinal)
        save()
    }

    /// RFC 3339 with or without fractional seconds (the gateway emits both
    /// across endpoints). Unparseable → epoch, so a malformed row sinks to the
    /// bottom instead of crashing the list.
    nonisolated static func parseDate(_ rfc3339: String) -> Date {
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = fractional.date(from: rfc3339) { return date }
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        return plain.date(from: rfc3339) ?? Date(timeIntervalSince1970: 0)
    }

    nonisolated static func supportDirectory() -> URL {
        let base = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? FileManager.default.temporaryDirectory
        let dir = base.appendingPathComponent("baybo", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }
}

/// Bridges the core's connection-global `SessionActivity` pings onto the
/// device-local list. Registered once at launch (`AppStore` →
/// `setSessionListSink`). `@unchecked Sendable`: it holds no mutable state and
/// all work hops to the main actor before touching `SessionIndex`. NOT named
/// `SessionListSinkImpl` — UniFFI generates a class by that exact name for the
/// `with_foreign` trait, so a same-named class here collides (cf. `Sink` /
/// `PairAbortHandler`, which dodge the generated `*Impl` names the same way).
final class SessionActivityHandler: SessionListSink, @unchecked Sendable {
    func onActivity(sessionId: String, source: String, atMillis: Int64) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                SessionIndex.shared.noteActivity(
                    sessionId: sessionId, source: source, atMillis: atMillis)
            }
        }
    }

    func onTitle(sessionId: String, title: String) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                SessionIndex.shared.applyTitle(sessionId: sessionId, title: title)
            }
        }
    }

    func onApprovalPending(sessionId: String, pending: Bool) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                SessionIndex.shared.noteApprovalPending(
                    sessionId: sessionId, pending: pending)
            }
        }
    }

    func onListStale() {
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                SessionIndex.shared.noteListStale()
            }
        }
    }
}

/// Per-session transcript mirrors: the JSON state blob the web bundle sends
/// over the bridge's `persist`, one file per session (the single-session
/// UserDefaults key couldn't survive a multi-session list, and a plist that
/// holds every transcript loads whole at launch).
///
/// A mirror exists for every session this device has rendered, and **nothing
/// evicts it in the background** — it is the chat's whole cold-open story (it is
/// what `deliverInit` paints before a single byte crosses the network), so a
/// sweeper that drops it trades a permanent blank open for a few KB of text.
/// The files carry message text, the sync cursor and image dimensions; never
/// blob bytes (those live in their own cache, which is also never swept). Only a
/// user-triggered delete (`SessionIndex.beginHide`) or unbinding the gateway
/// (`removeAll`) removes one. Bounding this wants a stated retention policy, not
/// a surprise sweep.
enum TranscriptStore {
    /// The support root defaults to the production one; `SessionIndex` passes
    /// its own so an isolated registry deletes/wipes its own tree (see its
    /// `supportDirectory`).
    private static func directory(in supportDirectory: URL) -> URL {
        let dir = supportDirectory.appendingPathComponent("transcripts", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// Session ids are gateway-assigned or UUIDs, but never trust them as raw
    /// path components.
    private static func sanitize(_ sessionId: String) -> String {
        sessionId.replacingOccurrences(of: "/", with: "_")
    }

    private static func fileURL(for sessionId: String, in supportDirectory: URL) -> URL {
        directory(in: supportDirectory).appendingPathComponent("\(sanitize(sessionId)).json")
    }

    static func read(
        sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) -> String? {
        guard let data = try? Data(contentsOf: fileURL(for: sessionId, in: supportDirectory))
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func write(
        sessionId: String, stateJson: String,
        in supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        try? stateJson.data(using: .utf8)?
            .write(to: fileURL(for: sessionId, in: supportDirectory), options: .atomic)
    }

    /// Drop one session's mirror — the user deleted the conversation. The only
    /// per-session removal there is; see the type's note on why nothing sweeps.
    static func delete(sessionId: String, in supportDirectory: URL) {
        try? FileManager.default.removeItem(at: fileURL(for: sessionId, in: supportDirectory))
    }

    static func deleteAll(in supportDirectory: URL) {
        try? FileManager.default.removeItem(at: directory(in: supportDirectory))
    }
}
