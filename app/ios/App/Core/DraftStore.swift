import Foundation

/// One staged pick as an unsent draft carries it — enough to put the tile back
/// on the strip and enough to finish the send, and nothing else.
struct DraftAttachment: Codable, Equatable {
    /// The pick's `StagedAttachment.id`, and the stem of every file this record
    /// owns inside the draft's directory.
    let id: String
    /// Whether the strip draws an image thumbnail or a file pill. NOT derivable
    /// from the mime: a PNG picked through Files is a file pill, and a `.ready`
    /// pick has no local bytes left to sniff either way.
    let isImage: Bool
    let mime: String
    let filename: String?
    let byteCount: UInt32
    /// Set once the pick reached the gateway — from then on the send needs no
    /// local bytes at all, because `AttachmentRef` IS the whole message.
    let blobId: String?
    /// A Files pick with no blob yet: the security-scoped bookmark that re-opens
    /// the user's own document in a later process. Never a copy — that document
    /// can be 100 MiB and was never ours.
    let bookmark: Data?
}

/// What the composer is holding for one conversation and has not sent.
struct Draft: Codable, Equatable {
    var text: String
    var attachments: [DraftAttachment]

    /// Nothing worth keeping. Whitespace alone is not a draft — otherwise a
    /// stray space would keep a dead new-chat session reachable forever
    /// (`AppStore.startNewChat`) — but it IS preserved verbatim while the draft
    /// lives, because re-opening a conversation must give back exactly what was
    /// typed.
    var isEmpty: Bool {
        attachments.isEmpty && text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

/// Per-conversation **unsent composer state**: what the user typed and what they
/// staged, kept across leaving the chat, across an LRU eviction of the
/// `ChatStore`, and across a relaunch. A sibling of the transcript mirror
/// (`TranscriptStore`) and the send outbox (`OutboxStore`), under the same
/// support root.
///
/// One DIRECTORY per session, so a draft is one `removeItem` and one prune:
///
///     drafts/<session id>/draft.json      the record above
///     drafts/<session id>/<pick id>.thumb the staged tile's thumbnail
///     drafts/<session id>/<pick id>.src   the pick's bytes, while it has no blob
///
/// **What is kept per pick follows from what a SEND needs.** A pick that reached
/// its blob keeps only a thumbnail, for the tile. A pick still queued, uploading
/// or failed holds bytes nobody else has, so those are kept too — a photo or a
/// paste by HARD LINK off the composer's temp spool (O(1), and no second copy of
/// a 100 MiB pick), a Files pick by bookmark. The realistic case for the second
/// kind is being offline: every upload fails, and without this the user's picks
/// vanish from a draft that still shows their text.
///
/// **Nothing in the strip ever opens a path under `drafts/`.** The restore
/// hard-links `<pick id>.src` back into THIS run's spool directory and hands the
/// strip an ordinary `SpoolFile`, so a restored pick is indistinguishable from a
/// fresh one — same ownership, same reclamation. That is what makes `prune`
/// safe at any moment: an upload streaming a restored pick is reading its own
/// spool, and unlinking one name of a hard-linked inode takes no byte away from
/// the other. (Deleting a file an upload is reading is not hypothetical — the
/// Rust side opens it TWICE, once to hash and once for the body; see
/// `ComposerStaging.discard`.)
enum DraftStore {
    private static let rootName = "drafts"
    private static let recordName = "draft.json"
    private static let thumbSuffix = "thumb"
    private static let sourceSuffix = "src"

    // MARK: - Reading

    static func read(
        sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) -> Draft? {
        let url = directory(for: sessionId, in: supportDirectory)
            .appendingPathComponent(recordName)
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(Draft.self, from: data)
    }

    /// Sessions holding a draft. `AppStore.startNewChat` reads this to find the
    /// unsent new-chat draft the user walked away from: a draft session has no
    /// registry row and no gateway row, so nothing else on the device can name
    /// it, and minting a fresh id would strand it forever.
    static func sessionIds(in supportDirectory: URL = SessionIndex.supportDirectory()) -> [String] {
        let contents =
            (try? FileManager.default.contentsOfDirectory(
                at: root(in: supportDirectory), includingPropertiesForKeys: nil)) ?? []
        return contents.filter { url in
            FileManager.default.fileExists(
                atPath: url.appendingPathComponent(recordName).path)
        }.map(\.lastPathComponent)
    }

    /// When a session's draft was last written — the tiebreak when more than one
    /// unsent draft session somehow exists.
    static func modified(
        sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) -> Date {
        let url = directory(for: sessionId, in: supportDirectory)
            .appendingPathComponent(recordName)
        return (try? url.resourceValues(forKeys: [.contentModificationDateKey]))?
            .contentModificationDate ?? .distantPast
    }

    // MARK: - Writing

    /// Persist `draft` and reclaim everything it no longer references. An empty
    /// draft deletes the whole directory rather than leaving a husk behind — it
    /// is also how a send, a cleared field and a stray write from a zombie
    /// upload all end up meaning the same thing.
    static func write(
        _ draft: Draft, sessionId: String,
        in supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        guard !draft.isEmpty else {
            delete(sessionId: sessionId, in: supportDirectory)
            return
        }
        let dir = prepareDirectory(for: sessionId, in: supportDirectory)
        guard let data = try? JSONEncoder().encode(draft) else { return }
        try? data.write(to: dir.appendingPathComponent(recordName), options: .atomic)
        prune(dir, keeping: keepNames(draft))
    }

    static func delete(
        sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        try? FileManager.default.removeItem(at: directory(for: sessionId, in: supportDirectory))
    }

    /// Logout / rebind: every draft belonged to the departing gateway.
    static func deleteAll(in supportDirectory: URL) {
        try? FileManager.default.removeItem(at: root(in: supportDirectory))
    }

    // MARK: - Layout

    /// A draft's own directory. Reading one must not CREATE anything — a
    /// restore that missed would otherwise leave an empty directory behind for
    /// every conversation ever opened, and `sessionIds` reads that tree.
    static func directory(
        for sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) -> URL {
        root(in: supportDirectory).appendingPathComponent(sanitize(sessionId), isDirectory: true)
    }

    /// The same directory, created — with the backup exclusion applied to the
    /// root on the way. The staging machine calls it before writing a thumbnail
    /// or linking a source, so those never land in an unmarked tree.
    @discardableResult
    static func prepareDirectory(
        for sessionId: String, in supportDirectory: URL = SessionIndex.supportDirectory()
    ) -> URL {
        var root = self.root(in: supportDirectory)
        if (try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true))
            != nil
        {
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try? root.setResourceValues(values)
        }
        let dir = root.appendingPathComponent(sanitize(sessionId), isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    static func thumbURL(pickId: String, in directory: URL) -> URL {
        directory.appendingPathComponent("\(pickId).\(thumbSuffix)")
    }

    static func sourceURL(pickId: String, in directory: URL) -> URL {
        directory.appendingPathComponent("\(pickId).\(sourceSuffix)")
    }

    /// `Application Support/baybo/drafts`. `prepareDirectory` is what creates
    /// it, and it excludes the tree from backup: a draft's retained bytes run to
    /// 100 MiB a pick and are unsent scratch by definition, so they have no
    /// business in iCloud (the call the blob cache makes — `Baybo.blobCacheDir`).
    private static func root(in supportDirectory: URL) -> URL {
        supportDirectory.appendingPathComponent(rootName, isDirectory: true)
    }

    /// Session ids are gateway-assigned or UUIDs, but never trust one as a raw
    /// path component (`TranscriptStore` makes the same move).
    private static func sanitize(_ sessionId: String) -> String {
        sessionId.replacingOccurrences(of: "/", with: "_")
    }

    /// The files `draft` still refers to. A `.src` survives only for a pick that
    /// has neither a blob nor a bookmark — the one shape that still needs local
    /// bytes — so a pick whose upload landed since the last write gives its
    /// retained copy straight back.
    private static func keepNames(_ draft: Draft) -> Set<String> {
        var names: Set<String> = [recordName]
        for attachment in draft.attachments {
            if attachment.isImage {
                names.insert("\(attachment.id).\(thumbSuffix)")
            }
            if attachment.blobId == nil, attachment.bookmark == nil {
                names.insert("\(attachment.id).\(sourceSuffix)")
            }
        }
        return names
    }

    private static func prune(_ directory: URL, keeping names: Set<String>) {
        let contents =
            (try? FileManager.default.contentsOfDirectory(
                at: directory, includingPropertiesForKeys: nil)) ?? []
        for url in contents where !names.contains(url.lastPathComponent) {
            try? FileManager.default.removeItem(at: url)
        }
    }
}
