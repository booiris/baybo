import Foundation

/// Narrow shared attachment seam for transcript and issue pages; their other
/// lifecycles remain separate.
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

@MainActor
enum WebMediaDispatch {
    /// Returns whether the shared handler consumed the message so a page
    /// bridge neither swallows nor double-handles it.
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
