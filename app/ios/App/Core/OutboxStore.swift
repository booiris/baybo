import Foundation

/// Persisted send outbox (sync-v2). One entry per in-flight send, keyed by its
/// `platform_msg_id`, persisted to `Application Support/baybo/outbox/<id>.json`
/// (a sibling of the transcript mirror) so a relaunch keeps un-durable sends
/// alive.
///
/// Confirmation is two-stage (docs/sync-protocol.md "Send path"): the server's
/// Echo (ordinal-less user message with the same key) proves TRANSPORT
/// (`sending` → `sent`, in-connection retries stop); an ordinal-stamped row
/// with the same key (from sync / backfill) proves DURABILITY and releases the
/// entry. The retry machine: no echo within 10 s → one blind resend (safe
/// inside the gateway's live dedup window), hard-capped at 3 transmissions
/// total, then `failed` + the manual retry affordance. A rebased sync hides the
/// floor, so an unconfirmed entry goes `unknown` and resolves via the per-key
/// point lookup instead of blind-resending.
///
/// The outbox rides out a BRIEF outage silently (a spinner, not a failure — the
/// message is queued, and reconnect drains it). But a SUSTAINED one is a real
/// failure the user must see: an entry still `sending` past `queuedTimeout`
/// (wall-clock, no wire in reach) flips to `failed` too, so an offline send
/// surfaces the red dot rather than spinning forever. The reconnect gate then
/// leaves it alone — it never silently auto-sends after being declared failed.
enum OutboxEntryState: String, Codable {
    case sending
    case sent
    case failed
    case unknown
}

/// Blob-ref metadata for an outbox entry's attachments — a Codable mirror of
/// the FFI `AttachmentRef` so a retry/resend replays the SAME image/file
/// message, not a text-only one. The blob bytes stay server-side
/// (content-addressed), so the ref is all a resend needs.
struct OutboxAttachment: Codable {
    /// `"image"` / `"audio"` / `"file"` (the `AttachmentKind` discriminant).
    var kind: String
    var blobId: String
    var mimeType: String
    var size: UInt32
    var filename: String?
}

struct OutboxEntry: Codable {
    let platformMsgId: String
    var text: String
    var attachments: [OutboxAttachment]
    var state: OutboxEntryState
    /// Automatic transmissions so far (the initial send counts as 1).
    var transmissions: Int
    /// Epoch seconds of the entry's creation and its most recent transmission.
    var createdAt: Double
    var lastSentAt: Double
}

@MainActor
final class OutboxStore {
    /// No Echo within this window (state still `sending`, connection alive)
    /// triggers the one blind in-connection resend. Durability lag alone never
    /// does — a mid-turn persist legitimately takes minutes.
    static let echoTimeout: TimeInterval = 10
    /// Hard cap on automatic transmissions per message (initial + retries); past
    /// it the entry goes `failed` and only a manual retry resets it.
    static let maxAutoTransmissions = 3
    /// Retry-sweep cadence — fine-grained enough to fire within a few seconds of
    /// the 10 s no-echo deadline without busy-spinning.
    static let retryTick: Duration = .seconds(3)
    /// Wall-clock lifetime cap for a still-unsent (`sending`) entry on a DOWN
    /// leg. Past it a queued message is declared `failed` rather than left
    /// spinning — a sustained outage IS a send failure. Sized at (not under) the
    /// live-leg genuine-failure horizon (≈ `maxAutoTransmissions × echoTimeout`)
    /// so it targets the offline case and never preempts the connected retry.
    static let queuedTimeout: TimeInterval = 30

    private let sessionId: String
    private let directory: URL
    /// The retry machine's clock. Injected so the 10 s echo window is testable
    /// without sleeping through it.
    private let now: () -> Date
    private var items: [String: OutboxEntry]

    convenience init(sessionId: String) {
        self.init(
            sessionId: sessionId,
            directory: Self.directory(in: SessionIndex.supportDirectory()),
            now: Date.init)
    }

    init(sessionId: String, directory: URL, now: @escaping () -> Date) {
        self.sessionId = sessionId
        self.directory = directory
        self.now = now
        items = Self.read(sessionId: sessionId, in: directory)
    }

    // MARK: - Reads

    /// Every live entry (any state — a `failed` row stays with its retry
    /// affordance until released).
    func entries() -> [OutboxEntry] {
        Array(items.values)
    }

    /// Whether a `sending` entry's echo window has lapsed and it is still under
    /// the automatic-transmission cap — i.e. a blind resend is due.
    func dueForBlindResend(_ entry: OutboxEntry) -> Bool {
        entry.state == .sending
            && entry.transmissions < Self.maxAutoTransmissions
            && echoWindowLapsed(entry)
    }

    /// Whether a `sending` entry exhausted its cap without an echo — it must flip
    /// to `failed`.
    func resendExhausted(_ entry: OutboxEntry) -> Bool {
        entry.state == .sending
            && entry.transmissions >= Self.maxAutoTransmissions
            && echoWindowLapsed(entry)
    }

    private func echoWindowLapsed(_ entry: OutboxEntry) -> Bool {
        now().timeIntervalSince1970 - entry.lastSentAt >= Self.echoTimeout
    }

    /// Whether a still-unsent entry has outlived the queued-lifetime cap — a
    /// sustained down leg that should now surface as a failure. Only a `sending`
    /// entry qualifies: an echoed (`sent`) entry already proved transport and is
    /// merely awaiting durability, so a stalled mid-turn persist never fails it.
    func queuedTooLong(_ entry: OutboxEntry) -> Bool {
        entry.state == .sending
            && now().timeIntervalSince1970 - entry.createdAt >= Self.queuedTimeout
    }

    /// Session ids with a persisted outbox. A relaunch reads this to find sends
    /// the previous process enrolled — including for a session that never
    /// reached `SessionIndex`, which nothing else can reach
    /// (`AppStore.resumeStrandedSends`). Session ids are UUIDs, so the file
    /// name's `/` sanitisation never fires and the basename IS the id.
    nonisolated static func pendingSessionIds(in supportDirectory: URL) -> [String] {
        let contents =
            (try? FileManager.default.contentsOfDirectory(
                at: directory(in: supportDirectory), includingPropertiesForKeys: nil)) ?? []
        return
            contents
            .filter { $0.pathExtension == "json" }
            .map { $0.deletingPathExtension().lastPathComponent }
    }

    // MARK: - Mutations

    /// Record a fresh send: state `sending`, the transmission already counted
    /// (only sends that actually hit the wire are enrolled).
    func beginSend(platformMsgId: String, text: String, attachments: [OutboxAttachment]) {
        let at = now().timeIntervalSince1970
        items[platformMsgId] = OutboxEntry(
            platformMsgId: platformMsgId, text: text, attachments: attachments,
            state: .sending, transmissions: 1, createdAt: at, lastSentAt: at)
        persist()
    }

    /// Echo observed: transport confirmed, in-connection retries stop. The entry
    /// is retained until durability confirms.
    func markEchoed(platformMsgId: String) {
        mutate(platformMsgId) { entry in
            if entry.state == .sending || entry.state == .unknown { entry.state = .sent }
        }
    }

    /// An ordinal-stamped row with this key was observed (sync / backfill / point
    /// lookup): durability confirmed, release the entry.
    func confirmDurable(platformMsgId: String) {
        guard items.removeValue(forKey: platformMsgId) != nil else { return }
        persist()
    }

    /// Count one (re)transmission: state back to `sending`, echo timer re-armed.
    func recordTransmission(platformMsgId: String) {
        let at = now().timeIntervalSince1970
        mutate(platformMsgId) { entry in
            entry.state = .sending
            entry.transmissions += 1
            entry.lastSentAt = at
        }
    }

    func markFailed(platformMsgId: String) {
        mutate(platformMsgId) { entry in entry.state = .failed }
    }

    /// A rebased page hid the floor — this entry's absence is unknowable from the
    /// page alone. Park it `unknown` pending the point lookup.
    func markUnknown(platformMsgId: String) {
        mutate(platformMsgId) { entry in
            if entry.state != .failed { entry.state = .unknown }
        }
    }

    /// Point lookup proved the key absent: the normal retry machine resumes.
    /// `lastSentAt` is zeroed so the next sweep treats the entry as due
    /// immediately; the lookup itself consumes no transmission.
    func resumeSending(platformMsgId: String) {
        mutate(platformMsgId) { entry in
            entry.state = .sending
            entry.lastSentAt = 0
        }
    }

    /// Manual retry from the failed affordance: same key, the automatic-
    /// transmission budget AND the queued-lifetime clock reset (a hand retry is a
    /// fresh attempt — `createdAt` moves so the `queuedTimeout` cap starts over,
    /// not instantly re-fires off the original enrolment). Re-creates the entry
    /// if it was released.
    func resetForManualRetry(platformMsgId: String, text: String, attachments: [OutboxAttachment]) {
        let at = now().timeIntervalSince1970
        if var entry = items[platformMsgId] {
            entry.state = .sending
            entry.transmissions = 0
            entry.createdAt = at
            entry.lastSentAt = 0
            items[platformMsgId] = entry
        } else {
            items[platformMsgId] = OutboxEntry(
                platformMsgId: platformMsgId, text: text, attachments: attachments,
                state: .sending, transmissions: 0,
                createdAt: at, lastSentAt: 0)
        }
        persist()
    }

    private func mutate(_ platformMsgId: String, _ body: (inout OutboxEntry) -> Void) {
        guard var entry = items[platformMsgId] else { return }
        body(&entry)
        items[platformMsgId] = entry
        persist()
    }

    // MARK: - Persistence

    nonisolated static func directory(in supportDirectory: URL) -> URL {
        let dir = supportDirectory.appendingPathComponent("outbox", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private nonisolated static func fileURL(for sessionId: String, in directory: URL) -> URL {
        let sanitized = sessionId.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("\(sanitized).json")
    }

    private nonisolated static func read(
        sessionId: String, in directory: URL
    ) -> [String: OutboxEntry] {
        guard let data = try? Data(contentsOf: fileURL(for: sessionId, in: directory)),
            let list = try? JSONDecoder().decode([OutboxEntry].self, from: data)
        else { return [:] }
        return Dictionary(list.map { ($0.platformMsgId, $0) }, uniquingKeysWith: { _, b in b })
    }

    private func persist() {
        let url = Self.fileURL(for: sessionId, in: directory)
        if items.isEmpty {
            try? FileManager.default.removeItem(at: url)
            return
        }
        guard let data = try? JSONEncoder().encode(Array(items.values)) else { return }
        try? data.write(to: url, options: .atomic)
    }

    /// Wipe every session's outbox — called on logout/rebind alongside
    /// `TranscriptStore.deleteAll(in:)`.
    nonisolated static func deleteAll(in supportDirectory: URL) {
        try? FileManager.default.removeItem(at: directory(in: supportDirectory))
    }
}
