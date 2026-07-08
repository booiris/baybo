import Foundation

/// One chat-list row. The device-local truth for sessions this device opened;
/// on a direct binding the REST list merges over it (see `merge(remote:)`).
struct SessionRow: Codable, Identifiable, Equatable {
    /// The session id.
    let id: String
    var createdAt: Date
    var lastActive: Date
    /// Auto-generated conversation title (server-side, from the first user
    /// question); nil until the title pass has run. The list's bold first line;
    /// live-updated by a `SessionUpdated` patch (`SessionIndex.applyTitle`).
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

    init(
        id: String, createdAt: Date, lastActive: Date, title: String? = nil,
        preview: String?, userText: String? = nil, pinned: Bool, archived: Bool = false,
        unread: Int = 0
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
    }
}

/// The device-local session registry backing the chat list on BOTH legs. Remote
/// refreshes from direct REST or the relay API tunnel merge into this same
/// rendering source. Rows persist as one small JSON file in Application Support;
/// transcript mirrors live next to it (`TranscriptStore`).
@MainActor
final class SessionIndex: ObservableObject {
    static let shared = SessionIndex()

    /// Mirrors the gateway's `PREVIEW_MAX_CHARS` so a locally-captured preview
    /// and a REST-fetched one truncate identically.
    static let previewMaxChars = 200
    /// Transcript mirrors kept on disk — only the most recently active
    /// sessions; older mirrors are pruned (the gateway replays history on
    /// re-entry, so a pruned mirror only costs a fetch).
    static let maxMirroredTranscripts = 10

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

    private let fileURL: URL
    /// The session whose `ChatScreen` is on top. A `SessionActivity` ping for it
    /// is not counted as unread (the user is looking at it); `nil` on the list.
    private var foregroundSessionId: String?
    /// In-memory only: a kill mid-flight loses the intent and the next merge
    /// restores server truth, which is the honest fallback.
    private var pendingMutations: [String: PendingMutation] = [:]
    /// Rows removed by `beginHide`, kept for the failure rollback (the mirror
    /// is prune-deleted and stays gone — the hydration matrix's mirror-less
    /// listed path refetches history).
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

    private init() {
        fileURL = Self.supportDirectory().appendingPathComponent("sessions.json")
        rows = Self.load(from: fileURL)
        migrateLegacySingleSession()
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
    /// (offline/failed tunnel). An attachment-only send (empty text) bumps
    /// activity but keeps the previous preview.
    func recordUserSend(sessionId: String, text: String) {
        let preview = String(text.prefix(Self.previewMaxChars))
        let now = Date()
        if let idx = rows.firstIndex(where: { $0.id == sessionId }) {
            if !preview.isEmpty {
                // The just-sent message is both the newest message overall and
                // the newest USER message, so it drives the preview AND the
                // bold-line fallback until REST reconciles.
                rows[idx].preview = preview
                rows[idx].userText = preview
            }
            rows[idx].lastActive = now
        } else {
            rows.append(
                SessionRow(
                    id: sessionId, createdAt: now, lastActive: now,
                    preview: preview.isEmpty ? nil : preview,
                    userText: preview.isEmpty ? nil : preview, pinned: false))
        }
        save()
    }

    // MARK: - Live activity (SessionActivity pings)

    /// A connection-global `SessionActivity` ping (see `SessionActivityHandler`):
    /// bump the row's recency and, unless it's the foreground session, its unread
    /// count. Unknown ids (drafts / cron / sessions created on another device) are
    /// ignored — a later REST merge surfaces them. Both `user` and `assistant`
    /// sources count, matching the web sidebar.
    func noteActivity(sessionId: String, source: String, atMillis: Int64) {
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else { return }
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

    /// A `ChatScreen` came to the foreground: mark it current and clear its badge.
    func enterSession(_ sessionId: String) {
        foregroundSessionId = sessionId
        clearUnread(sessionId)
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

    /// Optimistic delete (soft-hide): remove the row now — `save()`'s prune
    /// drops the transcript mirror — and suppress the row's remote existence
    /// in `merge` until the DELETE resolves.
    func beginHide(_ sessionId: String) {
        pendingMutations[sessionId] = .hidden
        mutationEpoch += 1
        guard let idx = rows.firstIndex(where: { $0.id == sessionId }) else { return }
        hiddenBackups[sessionId] = rows[idx]
        rows.remove(at: idx)
        save()
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
            guard mine != nil || hasRemotePreview || summary.pinned else { continue }
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
            if let mine, mine.lastActive > lastActive {
                // Local row saw activity after this snapshot (a just-sent
                // message racing the refetch) — keep its fresher preview/label.
                merged.append(
                    SessionRow(
                        id: summary.sessionId, createdAt: createdAt,
                        lastActive: mine.lastActive, title: title,
                        preview: mine.preview ?? remotePreview,
                        userText: mine.userText ?? summary.lastUserText,
                        pinned: pinned, archived: archived, unread: unread))
            } else {
                merged.append(
                    SessionRow(
                        id: summary.sessionId, createdAt: createdAt, lastActive: lastActive,
                        title: title, preview: remotePreview, userText: summary.lastUserText,
                        pinned: pinned, archived: archived, unread: unread))
            }
        }
        rows = merged
        save()
    }

    /// Logout / rebind: the rows belong to the old gateway — drop them, their
    /// transcript mirrors, and any staged mutations against it.
    func removeAll() {
        rows = []
        pendingMutations = [:]
        hiddenBackups = [:]
        archiveBaselines = [:]
        pinBaselines = [:]
        mutationEpoch += 1
        save()
        TranscriptStore.deleteAll()
        OutboxStore.deleteAll()
    }

    // MARK: - Persistence

    private func save() {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        guard let data = try? encoder.encode(rows) else { return }
        try? data.write(to: fileURL, options: .atomic)
        TranscriptStore.prune(keeping: mirrorWorthyIds())
    }

    /// The sessions whose transcript mirrors survive pruning: the most recently
    /// active, pinned or not (a pinned-but-dormant session re-fetches on entry).
    private func mirrorWorthyIds() -> Set<String> {
        Set(
            rows.sorted { $0.lastActive > $1.lastActive }
                .prefix(Self.maxMirroredTranscripts)
                .map(\.id))
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
        let defaults = UserDefaults.standard
        guard let sessionId = defaults.string(forKey: ChatDefaults.sessionId) else { return }
        if !rows.contains(where: { $0.id == sessionId }) {
            let now = Date()
            rows.append(
                SessionRow(
                    id: sessionId, createdAt: now, lastActive: now,
                    preview: nil, pinned: false))
        }
        if let blob = defaults.string(forKey: ChatDefaults.transcriptState) {
            TranscriptStore.write(sessionId: sessionId, stateJson: blob)
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
}

/// Per-session transcript mirrors: the JSON state blob the web bundle sends
/// over the bridge's `persist`, one file per session (the single-session
/// UserDefaults key couldn't survive a multi-session list, and a plist that
/// holds every transcript loads whole at launch).
enum TranscriptStore {
    private static func directory() -> URL {
        let dir = SessionIndex.supportDirectory()
            .appendingPathComponent("transcripts", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// Session ids are gateway-assigned or UUIDs, but never trust them as raw
    /// path components. Prune compares filenames, so it sanitizes through the
    /// same function.
    private static func sanitize(_ sessionId: String) -> String {
        sessionId.replacingOccurrences(of: "/", with: "_")
    }

    private static func fileURL(for sessionId: String) -> URL {
        directory().appendingPathComponent("\(sanitize(sessionId)).json")
    }

    static func read(sessionId: String) -> String? {
        guard let data = try? Data(contentsOf: fileURL(for: sessionId)) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func write(sessionId: String, stateJson: String) {
        try? stateJson.data(using: .utf8)?
            .write(to: fileURL(for: sessionId), options: .atomic)
    }

    /// Drop mirrors for sessions outside `keeping` (the registry's most
    /// recently active). A pruned session just re-fetches history on re-entry.
    static func prune(keeping: Set<String>) {
        let fm = FileManager.default
        let keptNames = Set(keeping.map(sanitize))
        guard
            let files = try? fm.contentsOfDirectory(
                at: directory(), includingPropertiesForKeys: nil)
        else { return }
        for file in files where file.pathExtension == "json" {
            if !keptNames.contains(file.deletingPathExtension().lastPathComponent) {
                try? fm.removeItem(at: file)
            }
        }
    }

    static func deleteAll() {
        try? FileManager.default.removeItem(at: directory())
    }
}
