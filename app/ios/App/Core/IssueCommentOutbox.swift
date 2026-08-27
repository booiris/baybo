import Foundation

enum IssueCommentSendState: String, Codable {
    case sending
    case failed
}

/// The part of an uploaded attachment a comment retry needs. The blob already
/// lives on the gateway; retaining its metadata also lets the optimistic row
/// draw the same attachment card before the POST answers.
struct IssueCommentAttachment: Codable, Equatable {
    let blobId: String
    let mimeType: String
    let size: UInt32
    let filename: String?

    init(_ ref: AttachmentRef) {
        blobId = ref.blobId
        mimeType = ref.mimeType
        size = ref.size
        filename = ref.filename
    }

    var request: IssueAttachmentInput {
        IssueAttachmentInput(blobId: blobId, filename: filename)
    }
}

/// One locally-owned comment until the gateway returns the durable timeline
/// row carrying the same `clientMsgId`.
struct PendingIssueComment: Codable, Equatable, Identifiable {
    let clientMsgId: String
    let text: String
    let attachments: [IssueCommentAttachment]
    let createdAtMs: Int64
    let unblockAfterSend: Bool
    var state: IssueCommentSendState

    var id: String { clientMsgId }
}

/// A persisted outbox per card. Unlike board moves, a comment is an append
/// with a durable UUID idempotency key, so replaying it after a process death
/// is both meaningful and safe.
@MainActor
final class IssueCommentOutbox {
    private let file: URL
    private var items: [String: PendingIssueComment]

    init(projectId: String, number: Int64, supportDirectory: URL) {
        let directory = Self.directory(in: supportDirectory)
        file = Self.fileURL(projectId: projectId, number: number, in: directory)
        items = Self.read(from: file)
    }

    func entries() -> [PendingIssueComment] {
        items.values.sorted {
            if $0.createdAtMs != $1.createdAtMs { return $0.createdAtMs < $1.createdAtMs }
            return $0.clientMsgId < $1.clientMsgId
        }
    }

    func entry(_ clientMsgId: String) -> PendingIssueComment? {
        items[clientMsgId]
    }

    func begin(
        clientMsgId: String,
        text: String,
        attachments: [AttachmentRef],
        unblockAfterSend: Bool
    ) {
        items[clientMsgId] = PendingIssueComment(
            clientMsgId: clientMsgId,
            text: text,
            attachments: attachments.map(IssueCommentAttachment.init),
            createdAtMs: Int64((Date().timeIntervalSince1970 * 1_000).rounded()),
            unblockAfterSend: unblockAfterSend,
            state: .sending)
        persist()
    }

    func markFailed(_ clientMsgId: String) {
        mutate(clientMsgId) { $0.state = .failed }
    }

    func resetForRetry(_ clientMsgId: String) {
        mutate(clientMsgId) { $0.state = .sending }
    }

    @discardableResult
    func confirm(_ clientMsgId: String) -> PendingIssueComment? {
        guard let confirmed = items.removeValue(forKey: clientMsgId) else { return nil }
        persist()
        return confirmed
    }

    private func mutate(_ clientMsgId: String, _ change: (inout PendingIssueComment) -> Void) {
        guard var entry = items[clientMsgId] else { return }
        change(&entry)
        items[clientMsgId] = entry
        persist()
    }

    private func persist() {
        if items.isEmpty {
            try? FileManager.default.removeItem(at: file)
            return
        }
        guard let data = try? JSONEncoder().encode(entries()) else { return }
        try? data.write(to: file, options: .atomic)
    }

    private nonisolated static func directory(in supportDirectory: URL) -> URL {
        let directory = supportDirectory.appendingPathComponent(
            "issue-comment-outbox", isDirectory: true)
        try? FileManager.default.createDirectory(
            at: directory, withIntermediateDirectories: true)
        return directory
    }

    private nonisolated static func fileURL(
        projectId: String,
        number: Int64,
        in directory: URL
    ) -> URL {
        let project = projectId.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("\(project)-\(number).json")
    }

    private nonisolated static func read(from file: URL) -> [String: PendingIssueComment] {
        guard let data = try? Data(contentsOf: file),
            let entries = try? JSONDecoder().decode([PendingIssueComment].self, from: data)
        else { return [:] }
        return Dictionary(
            entries.map { ($0.clientMsgId, $0) },
            uniquingKeysWith: { _, newest in newest })
    }

    nonisolated static func deleteAll(in supportDirectory: URL) {
        try? FileManager.default.removeItem(at: directory(in: supportDirectory))
    }
}
