import Foundation

/// What the transcript webview needs from whoever owns the conversation it is
/// showing — the forward direction of the bridge (`TranscriptSurface` is the
/// return leg, native→web callbacks about a send).
///
/// It exists so the SAME webview machinery can render a subagent child, which
/// has no send path at all. That is expressed as a TYPE, not as a `readOnly`
/// flag on `ChatStore`: every dial, outbox, draft and unread guard in that
/// class assumes a live, listed session, and the failure mode of getting one
/// of them wrong is silent (see `app/ios/docs/subagents.md` — the
/// `listed || remoteSessionEnsured` gate answers an unlisted session's baseline
/// sync with an EMPTY page that REPLACEs the thread, so the transcript renders
/// blank while looking perfectly healthy).
@MainActor
protocol TranscriptTarget: AnyObject {
    var sessionId: String { get }
    /// Bumped on every fresh subscription; the web side re-runs its sync loop
    /// when it changes. A target that never dials leaves it at zero.
    var connEpoch: Int { get }
    /// Whether this conversation is in the local chat list. Drives the initial
    /// paint gate only — a target that is deliberately absent from the list
    /// (a subagent child) reports `false`.
    var listed: Bool { get }

    func attachBridge(_ bridge: TranscriptBridge)
    func detachBridge(_ bridge: TranscriptBridge)

    func requestSync(sinceOrdinal: Int64?, limit: UInt32)
    func fetchHistory(beforeOrdinal: Int64?, limit: UInt32)

    func requestBlob(id: Int, blobId: String)
    func queryFileState(blobId: String)
    func downloadFile(blobId: String)
    func previewFile(blobId: String, filename: String, mimeType: String)
    func shareFile(blobId: String, filename: String, mimeType: String)
    func viewImage(blobId: String, filename: String, mimeType: String)
    func playVideo(blobId: String, filename: String, mimeType: String)
    func requestVideoPoster(id: Int, blobId: String, filename: String, mimeType: String)
    func audioToggle(blobId: String, filename: String, mimeType: String)
    func audioSeek(blobId: String, position: Double)
    func queryAudioState(blobId: String)

    // MARK: - The send half

    func replayUnconfirmedSends(to transcript: any TranscriptSurface)
    func flushPendingSendConfirms(to transcript: any TranscriptSurface)
    func retrySend(msgId: String, text: String, attachments: [AttachmentRef])
    func markRead(ordinal: Int64)
    func setAgentRunning(_ running: Bool)
}

/// A read-only target implements none of the send half, and a no-op is the
/// CORRECT behaviour rather than a stub: it owns no outbox (so there is
/// nothing to replay and no failed bubble to retry), it is absent from the
/// chat list (so it has no unread cursor to advance), and it has no composer
/// (so nothing renders a send↔stop button). Anything that later needs real
/// behaviour here belongs on the conforming type, where it can be seen.
extension TranscriptTarget {
    func replayUnconfirmedSends(to transcript: any TranscriptSurface) {}
    func flushPendingSendConfirms(to transcript: any TranscriptSurface) {}
    func retrySend(msgId: String, text: String, attachments: [AttachmentRef]) {}
    func markRead(ordinal: Int64) {}
    func setAgentRunning(_ running: Bool) {}
}
