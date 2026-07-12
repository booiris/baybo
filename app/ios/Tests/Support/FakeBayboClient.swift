import Foundation

@testable import Baybo

/// A recording stand-in for the Rust core. UniFFI already generates
/// `BayboClientProtocol` (every method the app calls) and `BayboClient` conforms
/// to it, so `ChatStore` can be driven with no gateway, no tokio runtime and no
/// keychain — and every call it makes is inspectable.
///
/// The methods are `nonisolated` (the protocol is), so the recorded state is
/// behind a lock rather than an actor: `ChatStore` starts these calls from the
/// main actor but the async bodies run wherever.
final class FakeBayboClient: BayboClientProtocol, @unchecked Sendable {
    struct SendCall: Equatable {
        let sessionId: String
        let text: String
        let msgId: String
        let attachments: [AttachmentRef]
    }

    struct LookupCall: Equatable {
        let sessionId: String
        let platformMsgId: String
    }

    struct ApprovalCall: Equatable {
        let callId: String
        let decision: ApprovalDecision
    }

    /// Everything the app never calls under test. Distinct prose so an
    /// accidental reliance on it is obvious in the failure.
    private static let unsupported = BayboError.Other(
        message: "FakeBayboClient: unsupported call")

    private let lock = NSLock()

    private var sinks: [String: FrameSink] = [:]
    private var sends: [SendCall] = []
    private var sendAfterConnects: [SendCall] = []
    private var connects: [String] = []
    private var createdSessions: [String] = []
    private var lookups: [LookupCall] = []
    private var approvals: [ApprovalCall] = []
    private var marksRead: [Int64] = []

    private var connectError: Error?
    private var sendError: Error?
    private var createSessionError: Error?
    private var syncOutcome: Result<String, Error> = .success(FakeBayboClient.emptySyncFrame)
    private var lookupResults: [String: MessageLookup] = [:]
    private var lookupError: Error?
    private var approvalError: Error?

    /// The baseline answer to a sync: no rows, no cursor. Enough to unwind the
    /// webview's in-flight guard, and it confirms nothing in the outbox.
    static let emptySyncFrame = #"""
        {"kind":"sync_page","rows":[],"since_ordinal":null,"next_cursor":null,
         "rebased":false,"oldest_ordinal":null,"has_more_older":false}
        """#

    // MARK: - Recorded calls

    var sentMessages: [SendCall] { lock.withLock { sends } }
    var sentAfterConnect: [SendCall] { lock.withLock { sendAfterConnects } }
    var connectedSessions: [String] { lock.withLock { connects } }
    var createdSessionIds: [String] { lock.withLock { createdSessions } }
    var lookupCalls: [LookupCall] { lock.withLock { lookups } }
    var approvalCalls: [ApprovalCall] { lock.withLock { approvals } }
    var readOrdinals: [Int64] { lock.withLock { marksRead } }

    /// Every transmission of a message, however it reached the wire — the
    /// initial `chatSendAfterConnect` on a cold leg and every later `chatSend`.
    /// A duplicate durable send (the failure the reconcile gate exists to
    /// prevent) shows up here as two entries with the same `msgId`.
    var transmissions: [SendCall] {
        lock.withLock { sendAfterConnects + sends }
    }

    // MARK: - Canned behaviour

    func failConnect(with error: Error) { lock.withLock { connectError = error } }
    func failSend(with error: Error) { lock.withLock { sendError = error } }
    func failCreateSession(with error: Error) { lock.withLock { createSessionError = error } }
    func failApproval(with error: Error) { lock.withLock { approvalError = error } }

    func answerSync(with frameJson: String) {
        lock.withLock { syncOutcome = .success(frameJson) }
    }

    func failSync(with error: Error) { lock.withLock { syncOutcome = .failure(error) } }

    /// The durability point lookup's answer for one key. An unstubbed key
    /// answers `found: false` — provably absent, which is what resumes the retry
    /// machine.
    func answerLookup(platformMsgId: String, found: Bool, ordinal: Int64? = nil) {
        lock.withLock {
            lookupResults[platformMsgId] = MessageLookup(found: found, ordinal: ordinal)
        }
    }

    func failLookup(with error: Error) { lock.withLock { lookupError = error } }

    /// Push a frame into the session's live sink, exactly as the core's pump
    /// does. The sink hops to the main queue, so the caller must let the actor
    /// drain (`waitUntil`).
    func deliver(_ frameJson: String, to sessionId: String) {
        lock.withLock { sinks[sessionId] }?.onFrame(frameJson: frameJson)
    }

    // MARK: - BayboClientProtocol

    func chatConnect(sessionId: String, sink: FrameSink) async throws {
        let error: Error? = lock.withLock {
            connects.append(sessionId)
            if connectError == nil { sinks[sessionId] = sink }
            return connectError
        }
        if let error { throw error }
    }

    func chatSendAfterConnect(
        sessionId: String, sink: FrameSink, text: String, msgId: String,
        attachments: [AttachmentRef]
    ) async throws {
        let error: Error? = lock.withLock {
            connects.append(sessionId)
            if let sendError { return sendError }
            sinks[sessionId] = sink
            sendAfterConnects.append(
                SendCall(
                    sessionId: sessionId, text: text, msgId: msgId, attachments: attachments))
            return nil
        }
        if let error { throw error }
    }

    func chatSend(
        sessionId: String, text: String, msgId: String, attachments: [AttachmentRef]
    ) async throws {
        let error: Error? = lock.withLock {
            if let sendError { return sendError }
            sends.append(
                SendCall(
                    sessionId: sessionId, text: text, msgId: msgId, attachments: attachments))
            return nil
        }
        if let error { throw error }
    }

    func chatCreateSession(sessionId: String) async throws -> String {
        let error: Error? = lock.withLock {
            createdSessions.append(sessionId)
            return createSessionError
        }
        if let error { throw error }
        return sessionId
    }

    func chatFetchSync(sessionId: String, sinceOrdinal: Int64?, limit: UInt32) async throws
        -> String
    {
        try lock.withLock { syncOutcome }.get()
    }

    func chatLookupMessage(sessionId: String, platformMsgId: String) async throws -> MessageLookup {
        let outcome: Result<MessageLookup, Error> = lock.withLock {
            lookups.append(LookupCall(sessionId: sessionId, platformMsgId: platformMsgId))
            if let lookupError { return .failure(lookupError) }
            return .success(lookupResults[platformMsgId] ?? MessageLookup(found: false, ordinal: nil))
        }
        return try outcome.get()
    }

    func chatResolveApproval(callId: String, decision: ApprovalDecision) async throws {
        let error: Error? = lock.withLock {
            approvals.append(ApprovalCall(callId: callId, decision: decision))
            return approvalError
        }
        if let error { throw error }
    }

    func chatMarkRead(sessionId: String, ordinal: Int64) async throws {
        lock.withLock { marksRead.append(ordinal) }
    }

    func chatUnsubscribe(sessionId: String) async {
        lock.withLock { sinks.removeValue(forKey: sessionId) }
    }

    func chatDisconnect() async {
        lock.withLock { sinks.removeAll() }
    }

    func chatFetchHistory(sessionId: String, beforeOrdinal: Int64?, limit: UInt32?) async throws
        -> String
    {
        throw Self.unsupported
    }

    func chatHideSession(sessionId: String) async throws { throw Self.unsupported }
    func chatListSessions() async throws -> [ChatSessionSummary] { throw Self.unsupported }
    func chatSetArchived(sessionId: String, archived: Bool) async throws { throw Self.unsupported }
    func chatSetPinned(sessionId: String, pinned: Bool) async throws { throw Self.unsupported }

    func blobDownloadBytes(blobId: String, progress: BlobProgress?) async throws -> Data {
        throw Self.unsupported
    }

    func blobIsCached(blobId: String) async -> Bool { false }

    func blobUploadBytes(bytes: Data, mimeType: String) async throws -> String {
        throw Self.unsupported
    }

    func directLogin(baseUrl: String, token: String) async throws -> String {
        throw Self.unsupported
    }

    func directPreconnect() async throws { throw Self.unsupported }
    func directStatus() throws -> String? { nil }
    func logout() async throws { throw Self.unsupported }

    func pairBegin(target: PairTarget, onAbort: PairAbortListener) async throws -> PairChallenge {
        throw Self.unsupported
    }

    func pairConfirm(deviceId: String, accepted: Bool) async throws -> PairedSummary {
        throw Self.unsupported
    }

    func pairedDevice() -> String? { nil }
    func registerPush() async throws -> String? { throw Self.unsupported }
    func relayPreconnect() async throws { throw Self.unsupported }
    func setApnsToken(tokenHex: String) {}
    func setSessionListSink(sink: SessionListSink) {}
}
