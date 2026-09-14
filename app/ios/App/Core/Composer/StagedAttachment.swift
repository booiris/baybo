import ImageIO
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// One pick sitting in the composer's staged strip, from selection to blob.
struct StagedAttachment: Identifiable {
    enum Preview {
        case image(UIImage?)
        case file(name: String, mime: String)
    }

    enum Source {
        /// The composer's own temp spool (a photo pick) — ours, and unlinked
        /// when the last holder of the `SpoolFile` lets go.
        case spooled(SpoolFile)
        /// A Files pick read in place under its security scope — the user's
        /// own document, never ours to delete.
        case scoped(URL)

        var url: URL {
            switch self {
            case .spooled(let file): return file.url
            case .scoped(let url): return url
            }
        }

        var isScoped: Bool {
            if case .scoped = self { return true }
            return false
        }
    }

    enum State {
        case queued
        case uploading(sent: UInt64)
        case ready(blobId: String)
        case error

        var isQueued: Bool {
            if case .queued = self { return true }
            return false
        }

        var isUploading: Bool {
            if case .uploading = self { return true }
            return false
        }

        /// Not on the gateway yet, and not failed either.
        var isPending: Bool { isQueued || isUploading }

        var isError: Bool {
            if case .error = self { return true }
            return false
        }

        var sentBytes: UInt64 {
            if case .uploading(let sent) = self { return sent }
            return 0
        }
    }

    /// Why the staged strip can't ride a send yet, or `nil` when every pick has
    /// its blob.
    enum Blocker: Equatable {
        case waiting
        case failed
    }

    /// Persisted with the draft so a restored pick keeps the same identity.
    var id = UUID()
    var preview: Preview
    /// `nil` only on a photo tile PhotosUI has not delivered yet: the pick
    /// takes its slot in the strip before its bytes exist.
    var source: Source?
    var mime: String
    let filename: String?
    /// Validated at staging (`wireSize`), so the wire field is never a
    /// saturating conversion of a size the card's progress denominator reads.
    var byteCount: UInt32
    var bookmark: Data?
    var state: State = .queued
    var work: Task<Void, Never>?

    /// The mime a file whose type nothing could name gets. The kind derivation
    /// reads it as a plain file, which is the honest answer.
    static let fallbackMime = "application/octet-stream"
    static let plainTextMime = "text/plain"

    static func pendingPhoto() -> StagedAttachment {
        StagedAttachment(
            preview: .image(nil), source: nil, mime: fallbackMime, filename: nil, byteCount: 0)
    }

    static func blocker(_ staged: [StagedAttachment]) -> Blocker? {
        if staged.contains(where: { $0.state.isPending }) { return .waiting }
        if staged.contains(where: { $0.state.isError }) { return .failed }
        return nil
    }

    mutating func claimRetry() -> Bool {
        guard state.isError else { return false }
        state = .queued
        return true
    }

    /// The wire reference for a landed pick; `nil` while it has no blob.
    var attachmentRef: AttachmentRef? {
        guard let blobId else { return nil }
        return AttachmentRef(
            kind: Self.kind(forMime: mime), blobId: blobId, mimeType: mime, size: byteCount,
            filename: filename)
    }

    var blobId: String? {
        guard case .ready(let blobId) = state else { return nil }
        return blobId
    }

    static func kind(forMime mime: String) -> AttachmentKind {
        if mime.hasPrefix("image/") { return .image }
        if mime.hasPrefix("audio/") { return .audio }
        return .file
    }

    private static let plainTextExtensions: Set<String> = [
        "rs", "toml", "yml", "yaml", "log", "env", "ini", "cfg", "conf",
        "sql", "kt", "go", "tsx", "jsx",
    ]

    static func mimeType(forExtension ext: String) -> String {
        let declared = UTType(filenameExtension: ext)
        if let mime = declared?.preferredMIMEType { return mime }
        if declared?.conforms(to: .text) == true { return plainTextMime }
        return plainTextExtensions.contains(ext.lowercased()) ? plainTextMime : fallbackMime
    }

    static func photoMime(declared: String?, data: Data) -> String {
        let sniffed = sniffMime(data)
        if sniffed != fallbackMime { return sniffed }
        return declared ?? fallbackMime
    }

    private static let heifBrands = ["heic", "heix", "hevc", "mif1", "msf1"]

    static func sniffMime(_ data: Data) -> String {
        let head = [UInt8](data.prefix(12))
        if head.starts(with: [0xFF, 0xD8, 0xFF]) { return jpegMime }
        if head.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if head.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        guard head.count == 12 else { return fallbackMime }
        let box = String(decoding: head[4...7], as: UTF8.self)
        let brand = String(decoding: head[8...11], as: UTF8.self)
        if brand == "WEBP" { return "image/webp" }
        if box == "ftyp", heifBrands.contains(brand) { return "image/heic" }
        return fallbackMime
    }

    static let jpegMime = "image/jpeg"

    /// Photos re-encoded before they leave the phone. HEIC is what the camera
    /// roll hands over, and the gateway delivers png / jpeg / webp only
    /// (`is_portable_image_type` in `crates/llm`, because a format some
    /// provider refuses fails the whole request), so a HEIC would reach the
    /// model as a placeholder. It travels as JPEG instead; every other photo
    /// keeps its original bytes.
    private static let reencodedPhotoMimes: Set<String> = ["image/heic", "image/heif"]
    /// Long edges tried in order; the first whose JPEG fits
    /// `providerImageByteCap` is sent. 4096 px keeps a 12 MP photo at full
    /// size and is the edge the gateway's `IMAGE_TOKEN_CEILING` is priced
    /// from, so it reaches every provider; 2048 px is for a scene too
    /// detailed to fit.
    private static let reencodedPhotoEdges = [4096, 2048]
    /// Mirrors `MAX_IMAGE_DOCUMENT_BYTES` in `crates/llm`: past it the model
    /// gets a placeholder instead of the image.
    private static let providerImageByteCap = 5 * 1024 * 1024
    private static let reencodedPhotoQuality = 0.8

    /// The bytes a staged photo uploads as, and their mime — see
    /// `reencodedPhotoMimes`. Bytes ImageIO can't decode come back as they
    /// went in, which only keeps this total: the thumbnail decode that
    /// follows in `spoolPhoto` fails on them too and drops the pick.
    static func uploadablePhoto(_ data: Data, mime: String) -> (data: Data, mime: String) {
        guard reencodedPhotoMimes.contains(mime) else { return (data, mime) }
        var rendition: Data?
        for edge in reencodedPhotoEdges {
            rendition = jpegRendition(of: data, maxEdge: edge)
            guard let jpeg = rendition, jpeg.count > providerImageByteCap else { break }
        }
        guard let rendition else { return (data, mime) }
        return (rendition, jpegMime)
    }

    /// `data` as a JPEG at most `maxEdge` pixels on its long side, with its
    /// orientation drawn into the pixels. The metadata stays behind — the
    /// location a camera-roll photo carries would otherwise ride along to the
    /// provider. `nil` when ImageIO can't decode it.
    static func jpegRendition(of data: Data, maxEdge: Int) -> Data? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: maxEdge,
        ]
        guard
            let image = CGImageSourceCreateThumbnailAtIndex(
                source, CGImageSourceGetPrimaryImageIndex(source), options as CFDictionary)
        else { return nil }
        let output = NSMutableData()
        guard
            let destination = CGImageDestinationCreateWithData(
                output as CFMutableData, UTType.jpeg.identifier as CFString, 1, nil)
        else { return nil }
        let properties: [CFString: Any] = [
            kCGImageDestinationLossyCompressionQuality: reencodedPhotoQuality
        ]
        CGImageDestinationAddImage(destination, image, properties as CFDictionary)
        guard CGImageDestinationFinalize(destination) else { return nil }
        return output as Data
    }

    static func glyph(forMime mime: String) -> String {
        if mime.hasPrefix("image/") { return "photo" }
        if mime.hasPrefix("video/") { return "film" }
        if mime.hasPrefix("audio/") { return "waveform" }
        if mime.hasPrefix("text/") { return "doc.text" }
        return "doc"
    }

    static func wireSize(_ bytes: Int) -> UInt32? {
        guard bytes >= 0, bytes <= ComposerStaging.maxAttachmentBytes else { return nil }
        return UInt32(exactly: bytes)
    }

    static func byteText(_ bytes: UInt64) -> String {
        byteFormatter.string(fromByteCount: Int64(bytes))
    }

    static func spool(_ data: Data, id: UUID, mime: String) throws -> SpoolFile {
        try FileManager.default.createDirectory(
            at: spoolDirectory, withIntermediateDirectories: true)
        let url = spoolDirectory.appendingPathComponent(spoolName(id: id, mime: mime))
        try data.write(to: url, options: .atomic)
        return SpoolFile(url: url)
    }

    private static func spoolName(id: UUID, mime: String) -> String {
        let ext = UTType(mimeType: mime)?.preferredFilenameExtension
        return ext.map { "\(id.uuidString).\($0)" } ?? id.uuidString
    }

    static func restored(_ record: DraftAttachment, from directory: URL) -> StagedAttachment? {
        guard let id = UUID(uuidString: record.id) else { return nil }
        let preview: Preview =
            record.isImage
            ? .image(
                UIImage(contentsOfFile: DraftStore.thumbURL(pickId: record.id, in: directory).path))
            : .file(name: record.filename ?? "", mime: record.mime)

        var source: Source?
        var state: State = .queued
        if let blobId = record.blobId {
            // On the gateway already: the ref is the whole message, so there is
            // nothing local left to hold and nothing to re-upload.
            state = .ready(blobId: blobId)
        } else if let bookmark = record.bookmark {
            guard let url = resolveBookmark(bookmark) else { return nil }
            source = .scoped(url)
        } else {
            guard
                let spool = relink(
                    DraftStore.sourceURL(pickId: record.id, in: directory), mime: record.mime)
            else { return nil }
            source = .spooled(spool)
        }

        return StagedAttachment(
            id: id, preview: preview, source: source, mime: record.mime,
            filename: record.filename, byteCount: record.byteCount, bookmark: record.bookmark,
            state: state)
    }

    private static func relink(_ retained: URL, mime: String) -> SpoolFile? {
        let manager = FileManager.default
        guard manager.fileExists(atPath: retained.path),
            (try? manager.createDirectory(at: spoolDirectory, withIntermediateDirectories: true))
                != nil
        else { return nil }
        // A fresh path prevents live and retired machines from sharing a spool
        // that either one's final holder is allowed to unlink.
        let url = spoolDirectory.appendingPathComponent(spoolName(id: UUID(), mime: mime))
        guard
            (try? manager.linkItem(at: retained, to: url)) != nil
                || (try? manager.copyItem(at: retained, to: url)) != nil
        else { return nil }
        return SpoolFile(url: url)
    }

    /// A stale bookmark still resolves; iOS only wants it rewritten, which the
    /// next draft write does anyway (it re-bookmarks from the resolved URL).
    private static func resolveBookmark(_ data: Data) -> URL? {
        var stale = false
        return try? URL(resolvingBookmarkData: data, bookmarkDataIsStale: &stale)
    }

    static let spoolRoot = FileManager.default.temporaryDirectory
        .appendingPathComponent(spoolRootName, isDirectory: true)
    static let spoolDirectory = spoolRoot.appendingPathComponent(runId, isDirectory: true)

    static func sweepAbandonedSpools() async {
        // Per-launch directories make crash/jetsam cleanup safe: prior runs can
        // be removed without touching uploads owned by this process.
        let manager = FileManager.default
        guard
            let runs = try? manager.contentsOfDirectory(
                at: spoolRoot, includingPropertiesForKeys: nil)
        else { return }
        for run in runs where run.lastPathComponent != spoolDirectory.lastPathComponent {
            try? manager.removeItem(at: run)
        }
    }

    static func thumbnail(at url: URL) -> UIImage? {
        // ImageIO downsamples during decode, avoiding a full-resolution backing
        // store for a 64-point preview.
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: thumbnailMaxPixels,
        ]
        guard
            let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
        else { return nil }
        return UIImage(cgImage: image)
    }

    private static let spoolRootName = "baybo-compose"
    private static let runId = UUID().uuidString
    /// The tile draws at 64pt; 256px covers it at every screen scale.
    private static let thumbnailMaxPixels = 256

    private static let byteFormatter: ByteCountFormatter = {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        formatter.allowsNonnumericFormatting = false
        return formatter
    }()
}
