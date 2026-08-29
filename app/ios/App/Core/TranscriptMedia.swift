import AVFoundation
import Foundation
import UIKit
import UniformTypeIdentifiers

/// Everything a transcript does with an attachment: fetch the bytes, keep the
/// card's state honest, materialise the file under its real name, and hand the
/// result to whoever is on screen.
///
/// It is a SERVICE, not a view model — it publishes nothing. Each owner keeps
/// its own presentation state and receives the result through the `on…` hooks,
/// which is what lets one implementation serve both a live `ChatStore` and a
/// read-only `SubagentReadStore` without either observing the other.
///
/// Sharing it is not gratuitous. An attachment surface that behaved one way in
/// a chat and another in a subagent's transcript would be a bug in whichever
/// was written second, and every part of this is load-bearing in both: the
/// digest-keyed preview directory, the in-flight materialisation dedup, the
/// poster cache, and above all the detach-window buffers (a download whose
/// terminal `ready` lands while nothing is attached used to wedge its card at
/// `loading` forever).
@MainActor
final class TranscriptMedia {
    private let client: any BayboClientProtocol
    /// Doubles as the ON-SCREEN token: three handlers refuse to present after
    /// it goes away, because arming a sheet for a screen the user already left
    /// hands it to whatever mounts next.
    private weak var bridge: (any WebMediaSink)?

    var onPreview: ((FilePreview) -> Void)?
    var onShare: ((FilePreview) -> Void)?
    var onViewImage: ((ViewedImage) -> Void)?
    var onPlayVideo: ((VideoPlayback) -> Void)?

    init(client: any BayboClientProtocol) {
        self.client = client
    }

    /// Answers that landed while nothing was attached flush here — see
    /// `pendingFileStates`.
    func attach(_ bridge: any WebMediaSink) {
        self.bridge = bridge
        flushPendingAnswers(to: bridge)
    }

    func detach(_ bridge: any WebMediaSink) {
        guard self.bridge === bridge else { return }
        self.bridge = nil
    }

    func requestBlob(id: Int, blobId: String) {
        #if DEBUG
            // `-baybo-demo-images` serves its own bytes — an image is the one
            // attachment kind that can't be faked from a frame alone.
            if serveDemoImageIfRequested(id: id, blobId: blobId) { return }
        #endif
        Task {
            do {
                let bytes: Data
                if let cached = await client.blobReadCached(blobId: blobId) {
                    bytes = cached
                } else {
                    // A thumbnail fetch: nobody is watching the byte count, and the
                    // core skips the tick machinery entirely for a nil observer.
                    bytes = try await client.blobDownloadBytes(
                        blobId: blobId, progress: nil)
                }
                // Encode off the main actor: base64 of a large blob (up to
                // 100 MiB) would stall every tap for seconds.
                let (encoded, mime) = await Task.detached(priority: .userInitiated) {
                    (bytes.base64EncodedString(), Self.sniffBlobMimeType(bytes))
                }.value
                bridge?.blobResult(id: id, dataBase64: encoded, mimeType: mime, error: nil)
            } catch {
                bridge?.blobResult(
                    id: id, dataBase64: nil, mimeType: "", error: bayboErrorText(error))
            }
        }
    }

    // MARK: - File attachments (download → preview)

    /// Blobs with a download task in flight, so a second tap joins rather than
    /// racing a duplicate stream through the core's per-blob cache lock.
    private var fileDownloads: Set<String> = []

    private struct PendingFileState {
        let state: String
        let loaded: UInt64?
        let total: UInt64?
        let error: String?
    }

    /// Bridge ANSWERS that landed while no webview was attached. Wire frames
    /// buffer (`bufferedFrames`); these used to just drop — and a download
    /// whose terminal `ready` fell in the detach window wedged its card at
    /// `loading` forever, because a same-session re-attach remounts nothing and
    /// so re-queries nothing. Last-write-wins per blob; posters keep every
    /// reply (ids are one-shot promises). Flushed on `attachBridge`; a reply
    /// whose session switched away settles nothing web-side (`init` cleared the
    /// pending map) and is ignored there.
    private var pendingFileStates: [String: PendingFileState] = [:]
    private var pendingPosterReplies: [PendingPosterReply] = []

    private struct PendingPosterReply {
        let id: Int
        let dataBase64: String?
        let width: Int
        let height: Int
        let durationMs: Int
        let error: String?
    }

    private func pushFileState(
        blobId: String, state: String, loaded: UInt64? = nil, total: UInt64? = nil,
        error: String? = nil
    ) {
        if let bridge {
            bridge.fileState(
                blobId: blobId, state: state, loaded: loaded, total: total, error: error)
        } else {
            pendingFileStates[blobId] = PendingFileState(
                state: state, loaded: loaded, total: total, error: error)
        }
    }

    private func pushVideoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int,
        error: String? = nil
    ) {
        if let bridge {
            bridge.videoPoster(
                id: id, dataBase64: dataBase64, width: width, height: height,
                durationMs: durationMs, error: error)
        } else {
            pendingPosterReplies.append(
                PendingPosterReply(
                    id: id, dataBase64: dataBase64, width: width, height: height,
                    durationMs: durationMs, error: error))
        }
    }

    private func flushPendingAnswers(to bridge: any WebMediaSink) {
        let states = pendingFileStates
        pendingFileStates.removeAll()
        for (blobId, s) in states {
            bridge.fileState(
                blobId: blobId, state: s.state, loaded: s.loaded, total: s.total, error: s.error)
        }
        let posters = pendingPosterReplies
        pendingPosterReplies.removeAll()
        for p in posters {
            bridge.videoPoster(
                id: p.id, dataBase64: p.dataBase64, width: p.width, height: p.height,
                durationMs: p.durationMs, error: p.error)
        }
    }

    /// Answer the card's mount-time probe. The blob cache lives in the OS temp
    /// dir, so this is asked every mount rather than remembered.
    func queryFileState(blobId: String) {
        if fileDownloads.contains(blobId) {
            pushFileState(blobId: blobId, state: "loading")
            return
        }
        Task {
            let cached = await client.blobIsCached(blobId: blobId)
            pushFileState(blobId: blobId, state: cached ? "ready" : "idle")
        }
    }

    func downloadFile(blobId: String) {
        guard fileDownloads.insert(blobId).inserted else { return }
        pushFileState(blobId: blobId, state: "loading", loaded: 0)
        Task {
            defer { fileDownloads.remove(blobId) }
            do {
                _ = try await client.blobDownloadBytes(
                    blobId: blobId,
                    progress: BlobProgressForwarder { [weak self] loaded, total in
                        self?.pushFileState(
                            blobId: blobId, state: "loading", loaded: loaded, total: total)
                    })
                pushFileState(blobId: blobId, state: "ready")
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// Materialise the blob under its real name — QuickLook and the share sheet
    /// both pick the handler from the extension, and the core's cache names its
    /// files by digest — then hand it to the screen.
    func previewFile(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // Backed out mid-materialise: don't arm a stale sheet for the
                // next entry (same on-screen token as playVideo/shareFile).
                guard bridge != nil else { return }
                onPreview?(FilePreview(url: url))
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// A card long-press: hand the blob to the system share sheet under its
    /// real name, so Files / AirDrop / Save-to-Photos keep the original bytes
    /// and name — the same materialisation the previewer and players use.
    /// (Images share from inside their viewer instead.)
    func shareFile(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // Backed out mid-materialise: don't arm a stale sheet for the
                // next entry (same on-screen token as playVideo).
                guard bridge != nil else { return }
                onShare?(FilePreview(url: url))
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// Open a tapped image full-screen. The blob is device-cached (the thumbnail
    /// fetch wrote it), so this decodes near-instantly; a blob that is neither a
    /// decodable raster nor a vector simply doesn't present. Its own viewer
    /// rather than QuickLook so pinch-zoom, double-tap-to-restore, and the black
    /// chat-image field are guaranteed.
    ///
    /// The mime is what elects the medium, and it has to: iOS decodes no SVG at
    /// all, so `UIImage(data:)` alone made every tap on an agent's diagram a
    /// no-op (see `ViewedImage.Content`).
    func viewImage(blobId: String, filename: String, mimeType: String) {
        Task {
            guard let bytes = await imageBytes(blobId: blobId),
                let content = ViewedImage.Content(bytes: bytes, mimeType: mimeType)
            else { return }
            // The share sheet hands over the FILE, not the decoded image, so the
            // original encoding and name reach Photos / Files / AirDrop. A write
            // failure only costs the share button, never the viewer.
            let url = try? Self.writePreviewFile(
                bytes: bytes, blobId: blobId, filename: filename, mimeType: mimeType)
            onViewImage?(ViewedImage(id: blobId, content: content, url: url))
        }
    }

    /// The bytes behind a tapped image. A demo run has no leg to download over,
    /// and its images are served locally the same way the transcript's own
    /// `requestBlob` gets them (`-baybo-demo-images`) — without this the viewer
    /// is the one attachment surface no fixture can reach.
    private func imageBytes(blobId: String) async -> Data? {
        #if DEBUG
            if let demo = ChatStore.demoImageBytes(blobId: blobId) { return demo }
        #endif
        return try? await client.blobDownloadBytes(blobId: blobId, progress: nil)
    }

    // MARK: - Audio + video attachments

    /// Play/pause an audio card. The card only posts this once the blob is on
    /// disk, so the byte read is a cache hit; materialising under the real name
    /// gives AVPlayer an extension to sniff the container by.
    func audioToggle(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                AudioPlayerCenter.shared.toggle(
                    blobId: blobId, url: url, title: filename, bridge: bridge)
            } catch {
                // "failed" over "ready" on purpose: the worst failure mode here
                // is the cache getting purged between the probe and the tap
                // with the refetch failing — the blob is genuinely gone, and
                // failed's tap-to-redownload is the honest affordance.
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    func audioSeek(blobId: String, position: Double) {
        AudioPlayerCenter.shared.seek(blobId: blobId, position: position, bridge: bridge)
    }

    func queryAudioState(blobId: String) {
        AudioPlayerCenter.shared.queryState(blobId: blobId, bridge: bridge)
    }

    /// Hand a downloaded video to the native full-screen player. Chat audio
    /// yields first — two engines over one AVAudioSession just fight.
    func playVideo(blobId: String, filename: String, mimeType: String) {
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                // The user backed out while the file materialised: presenting
                // now would arm a stale fullScreenCover for the NEXT entry and
                // kill audio they started elsewhere. The bridge doubles as the
                // on-screen token (detached in ChatScreen.onDisappear).
                guard bridge != nil else { return }
                AudioPlayerCenter.shared.stop()
                onPlayVideo?(VideoPlayback(id: blobId, url: url))
            } catch {
                pushFileState(
                    blobId: blobId, state: "failed", error: bayboErrorText(error))
            }
        }
    }

    /// A video card asking for its poster: first frame + natural size +
    /// duration, generated off the materialised file (AVAssetImageGenerator
    /// needs a pathed asset, and the extension picks the demuxer).
    func requestVideoPoster(id: Int, blobId: String, filename: String, mimeType: String) {
        #if DEBUG
            if serveDemoVideoPosterIfRequested(id: id, blobId: blobId) { return }
        #endif
        Task {
            do {
                let url = try await materializePreviewFile(
                    blobId: blobId, filename: filename, mimeType: mimeType)
                let poster = try await Self.loadOrGeneratePoster(for: url)
                pushVideoPoster(
                    id: id,
                    dataBase64: poster.jpeg.base64EncodedString(),
                    width: poster.width,
                    height: poster.height,
                    durationMs: poster.durationMs)
            } catch {
                pushVideoPoster(
                    id: id, dataBase64: nil, width: 0, height: 0, durationMs: 0,
                    error: bayboErrorText(error))
            }
        }
    }

    private struct GeneratedPoster {
        let jpeg: Data
        let width: Int
        let height: Int
        let durationMs: Int
    }

    private struct PosterMeta: Codable {
        let width: Int
        let height: Int
        let durationMs: Int
    }

    private struct PosterEncodeError: Error, LocalizedError {
        var errorDescription: String? { "poster frame could not be encoded" }
    }

    /// Poster cache beside the materialised file (`poster.jpg` + `poster.json`
    /// in the digest dir): the tile re-requests its poster on EVERY remount
    /// (session switch, relaunch), and AVAssetImageGenerator + JPEG encode are
    /// too heavy to re-run each time. tmp-resident like the preview file — a
    /// purge just regenerates.
    private nonisolated static func loadOrGeneratePoster(for url: URL) async throws
        -> GeneratedPoster
    {
        let dir = url.deletingLastPathComponent()
        let jpegURL = dir.appendingPathComponent("poster.jpg")
        let metaURL = dir.appendingPathComponent("poster.json")
        if let jpeg = try? Data(contentsOf: jpegURL),
            let metaData = try? Data(contentsOf: metaURL),
            let meta = try? JSONDecoder().decode(PosterMeta.self, from: metaData)
        {
            return GeneratedPoster(
                jpeg: jpeg, width: meta.width, height: meta.height,
                durationMs: meta.durationMs)
        }
        let poster = try await generateVideoPoster(url: url)
        let meta = PosterMeta(
            width: poster.width, height: poster.height, durationMs: poster.durationMs)
        try? poster.jpeg.write(to: jpegURL, options: .atomic)
        try? JSONEncoder().encode(meta).write(to: metaURL, options: .atomic)
        return poster
    }

    /// First frame, downscaled — the tile is ~19rem wide; a full 4K frame
    /// base64'd over the bridge would be pure waste.
    private nonisolated static func generateVideoPoster(url: URL) async throws -> GeneratedPoster {
        let asset = AVURLAsset(url: url)
        let generator = AVAssetImageGenerator(asset: asset)
        generator.appliesPreferredTrackTransform = true
        generator.maximumSize = CGSize(width: 1024, height: 1024)
        let duration = try await asset.load(.duration)
        let (cgImage, _) = try await generator.image(at: CMTime(value: 0, timescale: 600))
        guard let jpeg = UIImage(cgImage: cgImage).jpegData(compressionQuality: 0.72) else {
            throw PosterEncodeError()
        }
        return GeneratedPoster(
            jpeg: jpeg,
            width: cgImage.width,
            height: cgImage.height,
            durationMs: duration.isNumeric ? Int(duration.seconds * 1000) : 0)
    }

    /// In-flight materialisations by target path: a poster request and a play
    /// tap for the same blob share one byte round-trip instead of holding the
    /// whole video in memory twice.
    private var previewMaterializations: [URL: Task<URL, Error>] = [:]

    /// The preview-file path without re-reading the blob when the named file is
    /// already on disk — poster generation and playback ask repeatedly for the
    /// same materialisation.
    private func materializePreviewFile(
        blobId: String, filename: String, mimeType: String
    ) async throws -> URL {
        let url = try Self.previewFileURL(blobId: blobId, filename: filename, mimeType: mimeType)
        if FileManager.default.fileExists(atPath: url.path) { return url }
        #if DEBUG
            // Demo blobs have no gateway to fetch from; a locally-served stand-in
            // lets share/preview present headlessly (see DemoFrames).
            if let demo = ChatStore.demoMaterializeBytes(blobId: blobId) {
                try demo.write(to: url, options: .atomic)
                return url
            }
        #endif
        if let inFlight = previewMaterializations[url] {
            return try await inFlight.value
        }
        let task = Task {
            let bytes = try await client.blobDownloadBytes(blobId: blobId, progress: nil)
            try bytes.write(to: url, options: .atomic)
            return url
        }
        previewMaterializations[url] = task
        defer { previewMaterializations[url] = nil }
        return try await task.value
    }

    /// `<tmp>/baybo-preview/<blob digest>/<filename>` — the digest directory
    /// keeps two blobs that share a filename apart, and lets a re-open reuse the
    /// file already written.
    private static func writePreviewFile(
        bytes: Data, blobId: String, filename: String, mimeType: String
    ) throws -> URL {
        let url = try previewFileURL(blobId: blobId, filename: filename, mimeType: mimeType)
        if !FileManager.default.fileExists(atPath: url.path) {
            try bytes.write(to: url, options: .atomic)
        }
        return url
    }

    private static func previewFileURL(
        blobId: String, filename: String, mimeType: String
    ) throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-preview", isDirectory: true)
            .appendingPathComponent(previewDirComponent(for: blobId), isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent(previewFilename(filename, mimeType: mimeType))
    }

    /// The digest half of `sha256:<hex>.<read-token>`; the token is a capability
    /// and rotates per upload, so it must never key a directory.
    private static func previewDirComponent(for blobId: String) -> String {
        let hex = blobId.drop(while: { $0 != ":" }).dropFirst().prefix(while: { $0 != "." })
        return hex.isEmpty ? "blob" : String(hex)
    }

    /// A nameless blob still needs an extension or QuickLook can't pick a
    /// previewer; derive one from the mime.
    private static func previewFilename(_ filename: String, mimeType: String) -> String {
        let trimmed = filename.replacingOccurrences(of: "/", with: "_")
        if !trimmed.isEmpty, trimmed.contains(".") { return trimmed }
        let ext = UTType(mimeType: mimeType)?.preferredFilenameExtension
        let base = trimmed.isEmpty ? "attachment" : trimmed
        return ext.map { "\(base).\($0)" } ?? base
    }

    /// Cheap magic-byte sniff so the webview can build a typed Blob; the exact
    /// subtype only matters for the object URL, so `image/*` fallbacks are fine.
    private nonisolated static func sniffBlobMimeType(_ data: Data) -> String {
        if data.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if data.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if data.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        if data.count > 11, data[8...11] == Data([0x57, 0x45, 0x42, 0x50]) {
            return "image/webp"
        }
        return ""
    }
}

#if DEBUG
    extension TranscriptMedia {
        /// Serve a demo image's bytes with no gateway and no blob leg: a flat
        /// PNG or a flat SVG at the declared size, behind a delay long enough
        /// to screenshot the layout BEFORE the bytes land — which is exactly
        /// the frame the reserved box has to already be right in. The fixture
        /// catalogue stays with the rest of them on `ChatStore`; only the reply
        /// lives here, because only this owns the bridge that carries it.
        func serveDemoImageIfRequested(id: Int, blobId: String) -> Bool {
            guard let mimeType = ChatStore.demoImageMime(blobId: blobId) else { return false }
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(2))
                guard let bytes = ChatStore.demoImageBytes(blobId: blobId) else { return }
                // The web side asked and native is answering, so this reply
                // cannot outrun the bridge that carried the request.
                bridge?.blobResult(
                    id: id, dataBase64: bytes.base64EncodedString(),
                    mimeType: mimeType, error: nil)
            }
            return true
        }

        /// Serve the demo video's poster with no gateway: a flat 1280×720 PNG
        /// plus a fake duration, so the downloaded tile (poster + play disc +
        /// duration chip) is screenshot-verifiable headlessly.
        func serveDemoVideoPosterIfRequested(id: Int, blobId: String) -> Bool {
            guard blobId == ChatStore.demoVideoAttachment["blob_id"] as? String else {
                return false
            }
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(600))
                guard let png = ChatStore.demoImagePng(width: 1280, height: 720) else { return }
                pushVideoPoster(
                    id: id, dataBase64: png.base64EncodedString(),
                    width: 1280, height: 720, durationMs: 83_000)
            }
            return true
        }
        /// Demo pushes go through the SAME detach-window buffer production
        /// uses. A demo drive runs on a timer from launch while the webview
        /// boots on its own schedule — several times slower on a hosted runner
        /// — and an optional-chained push at a bridge that has not attached yet
        /// is silently DROPPED, which read as the product failing to download.
        func pushDemoFileState(
            blobId: String, state: String, loaded: UInt64? = nil, total: UInt64? = nil
        ) {
            pushFileState(blobId: blobId, state: state, loaded: loaded, total: total)
        }
    }
#endif
