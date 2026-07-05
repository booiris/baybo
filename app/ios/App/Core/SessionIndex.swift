import Foundation

/// One chat-list row. The device-local truth for sessions this device opened;
/// on a direct binding the REST list merges over it (see `merge(remote:)`).
struct SessionRow: Codable, Identifiable, Equatable {
    /// The session id.
    let id: String
    var createdAt: Date
    var lastActive: Date
    /// Preview drawn from the most-recent user-authored message; nil for a
    /// session without a user turn yet.
    var lastUserText: String?
    var pinned: Bool
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

    @Published private(set) var rows: [SessionRow] = []

    private let fileURL: URL

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

    /// Ensure a row exists for a session being opened. Does NOT bump
    /// `lastActive` on an existing row — ordering means message activity, not
    /// visits.
    func touch(sessionId: String) {
        guard !rows.contains(where: { $0.id == sessionId }) else { return }
        let now = Date()
        rows.append(
            SessionRow(
                id: sessionId, createdAt: now, lastActive: now,
                lastUserText: nil, pinned: false))
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
                rows[idx].lastUserText = preview
            }
            rows[idx].lastActive = now
        } else {
            rows.append(
                SessionRow(
                    id: sessionId, createdAt: now, lastActive: now,
                    lastUserText: preview.isEmpty ? nil : preview, pinned: false))
        }
        save()
    }

    /// Merge the direct leg's REST list (the full non-hidden truth). Remote wins
    /// for existence — a local row missing remotely was hidden/deleted from
    /// another client — and for row fields, unless the local row saw activity
    /// after the remote snapshot (a just-sent message racing the refetch).
    /// Empty remote rows that this device has never listed are draft sessions:
    /// keep them out of the chat list until a send records local activity, or
    /// the gateway reports user-authored preview text.
    func merge(remote: [ChatSessionSummary]) {
        var merged: [SessionRow] = []
        let local = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0) })
        for summary in remote {
            let mine = local[summary.sessionId]
            let hasRemotePreview = !(summary.lastUserText ?? "").isEmpty
            guard mine != nil || hasRemotePreview || summary.pinned else { continue }

            let createdAt = Self.parseDate(summary.createdAt)
            let lastActive = Self.parseDate(summary.lastActive)
            if let mine, mine.lastActive > lastActive {
                merged.append(
                    SessionRow(
                        id: summary.sessionId, createdAt: createdAt,
                        lastActive: mine.lastActive,
                        lastUserText: mine.lastUserText ?? summary.lastUserText,
                        pinned: summary.pinned))
            } else {
                merged.append(
                    SessionRow(
                        id: summary.sessionId, createdAt: createdAt, lastActive: lastActive,
                        lastUserText: summary.lastUserText, pinned: summary.pinned))
            }
        }
        rows = merged
        save()
    }

    /// Logout / rebind: the rows belong to the old gateway — drop them and
    /// their transcript mirrors.
    func removeAll() {
        rows = []
        save()
        TranscriptStore.deleteAll()
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
                    lastUserText: nil, pinned: false))
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
