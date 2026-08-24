import Foundation

/// What an attachment card's native half needs from whichever webview is
/// showing it.
///
/// Two pages now render the same cards — the chat transcript and a project
/// card — because both carry the same `WireAttachment` and both import the
/// same components. `TranscriptMedia` does all the actual work (fetching,
/// caching, poster generation, the preview and player presentations) and had
/// exactly one thing tying it to the transcript: the concrete
/// `TranscriptBridge` it answered through. This is that tie, narrowed to the
/// four replies it actually makes.
///
/// Deliberately NOT "the whole bridge behind a protocol": what the two pages
/// share is the attachment lifecycle, and nothing else about them is alike —
/// one has a sync loop, an outbox and a scroll anchor, the other has a
/// description editor. Sharing the seam, not the surface.
@MainActor
protocol WebMediaSink: AnyObject {
    /// A `requestBlob` answer. `id` is the one-shot promise the page is
    /// holding; `dataBase64` nil means the error field is the answer.
    func blobResult(id: Int, dataBase64: String?, mimeType: String, error: String?)

    /// One file card's lifecycle step. `loaded`/`total` only ride a `loading`
    /// tick; `error` only a `failed` one.
    func fileState(blobId: String, state: String, loaded: UInt64?, total: UInt64?, error: String?)

    /// One audio track's engine state: play/pause flips, position ticks, and
    /// the `stopped` reset on end or usurp.
    func audioState(blobId: String, state: String, position: Double, duration: Double)

    /// A `requestVideoPoster` answer: the frame's JPEG bytes plus its natural
    /// size and the clip's duration, or `dataBase64: nil` and an error.
    func videoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int, error: String?)
}

/// The inbound half of the same seam: the message names an attachment card
/// posts, dispatched once for both pages.
///
/// Returns whether the message was consumed, so each bridge's own `switch`
/// runs only on what is left — a card message reaching a page-specific arm
/// would be a silent no-op, and a page-specific message swallowed here would
/// be worse.
@MainActor
enum WebMediaDispatch {
    /// Every message an attachment card can send. Held as a function rather
    /// than duplicated into two switch statements: the two pages must agree on
    /// the whole set, and a card kind added to one bridge and forgotten in the
    /// other fails only on the page nobody tested.
    static func handle(type: String, body: [String: Any], target: any WebMediaTarget) -> Bool {
        switch type {
        case "requestBlob":
            guard let id = (body["id"] as? NSNumber)?.intValue,
                let blobId = body["blobId"] as? String
            else { return true }
            target.requestBlob(id: id, blobId: blobId)
        case "queryFileState":
            guard let blobId = body["blobId"] as? String else { return true }
            target.queryFileState(blobId: blobId)
        case "downloadFile":
            guard let blobId = body["blobId"] as? String else { return true }
            target.downloadFile(blobId: blobId)
        case "previewFile":
            guard let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            else { return true }
            target.previewFile(blobId: blobId, filename: filename, mimeType: mimeType)
        case "shareFile":
            guard let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            else { return true }
            target.shareFile(blobId: blobId, filename: filename, mimeType: mimeType)
        case "viewImage":
            guard let blobId = body["blobId"] as? String else { return true }
            target.viewImage(
                blobId: blobId,
                filename: body["filename"] as? String ?? "",
                mimeType: body["mimeType"] as? String ?? "")
        case "audioToggle":
            guard let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            else { return true }
            target.audioToggle(blobId: blobId, filename: filename, mimeType: mimeType)
        case "audioSeek":
            guard let blobId = body["blobId"] as? String,
                let position = (body["position"] as? NSNumber)?.doubleValue
            else { return true }
            target.audioSeek(blobId: blobId, position: position)
        case "queryAudioState":
            guard let blobId = body["blobId"] as? String else { return true }
            target.queryAudioState(blobId: blobId)
        case "playVideo":
            guard let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            else { return true }
            target.playVideo(blobId: blobId, filename: filename, mimeType: mimeType)
        case "requestVideoPoster":
            guard let id = (body["id"] as? NSNumber)?.intValue,
                let blobId = body["blobId"] as? String,
                let filename = body["filename"] as? String,
                let mimeType = body["mimeType"] as? String
            else { return true }
            target.requestVideoPoster(id: id, blobId: blobId, filename: filename, mimeType: mimeType)
        default:
            return false
        }
        return true
    }
}

/// The store side of the seam — what `TranscriptMedia` implements and what
/// both pages' stores hand to `WebMediaDispatch`.
@MainActor
protocol WebMediaTarget: AnyObject {
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
}
