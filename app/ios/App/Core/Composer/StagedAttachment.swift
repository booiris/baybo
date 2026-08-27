import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// One pick sitting in the composer's staged strip, from selection to blob.
struct StagedAttachment: Identifiable {
    /// What the strip renders. An enum, not an `isImage` flag: a file tile and
    /// an image thumbnail carry different payloads, and only one of them can
    /// ever be right for a given pick. The thumbnail is `nil` until the pick's
    /// bytes have been delivered and decoded.
    enum Preview {
        case image(UIImage?)
        case file(name: String, mime: String)
    }

    /// Where a (re)upload reads the bytes from — always a PATH, so the encoded
    /// pick never crosses the FFI and a retry re-reads the file instead of
    /// anything having retained it.
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
        /// Admitted to the strip with nothing on the wire yet: a photo whose
        /// bytes PhotosUI has not delivered, or a pick waiting for an upload
        /// slot. The send gate treats it exactly like `uploading` — either way
        /// it is a pick the message is still waiting on.
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

    /// Assigned once and never changed, but a `var` so a pick restored from a
    /// draft keeps the id its persisted files are named after — a fresh one
    /// would orphan the thumbnail and the retained bytes on the next write.
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
    /// A Files pick's security-scoped bookmark, minted ONCE when the pick is
    /// admitted (or resolved back out of a draft). Not re-derived per draft
    /// write: `url.bookmarkData()` fails for a document that has since become
    /// unreachable, and a `retain` that re-minted would then silently drop from
    /// the record a pick whose tile is still on the strip.
    var bookmark: Data?
    var state: State = .queued
    /// Whatever is currently reading this pick — the PhotosUI delivery, or its
    /// upload. Removing the tile cancels it rather than deleting the file out
    /// from under it.
    var work: Task<Void, Never>?

    /// The mime a file whose type nothing could name gets. The kind derivation
    /// reads it as a plain file, which is the honest answer.
    static let fallbackMime = "application/octet-stream"
    static let plainTextMime = "text/plain"

    /// A photo pick's tile the instant it is admitted, before PhotosUI has
    /// delivered a byte. It carries no thumbnail, mime or size because none of
    /// that is knowable yet; what it does carry is a SLOT in the strip, which
    /// is what `blocker` reads.
    static func pendingPhoto() -> StagedAttachment {
        StagedAttachment(
            preview: .image(nil), source: nil, mime: fallbackMime, filename: nil, byteCount: 0)
    }

    /// A pick still on its way to the gateway blocks the send — including one
    /// whose bytes PhotosUI has not delivered yet. Gating on "uploading" alone
    /// could not see a pick mid-`loadTransferable`: the message shipped with
    /// whichever refs were ready and the rest of the batch became ghost tiles
    /// on the next one.
    static func blocker(_ staged: [StagedAttachment]) -> Blocker? {
        if staged.contains(where: { $0.state.isPending }) { return .waiting }
        if staged.contains(where: { $0.state.isError }) { return .failed }
        return nil
    }

    /// Claim a failed tile for a retry, reporting whether THIS call is the one
    /// that owns it. Two taps in one frame both see `.error` in the render
    /// snapshot, so the flip off `.error` is what makes the second a no-op.
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

    /// The blob this pick became, once it is one.
    ///
    /// The card's comment wire takes ids and filenames and nothing else — the
    /// gateway reads mime and size off the blob itself — so it uses this
    /// rather than `attachmentRef`, whose extra fields belong to the chat's
    /// frame.
    var blobId: String? {
        guard case .ready(let blobId) = state else { return nil }
        return blobId
    }

    /// The wire kind is derived from the MIME, never from which picker the file
    /// came through: an image picked in Files is still an image, and video has
    /// no kind of its own (it rides `file` + a `video/*` mime).
    static func kind(forMime mime: String) -> AttachmentKind {
        if mime.hasPrefix("image/") { return .image }
        if mime.hasPrefix("audio/") { return .audio }
        return .file
    }

    /// Extensions the OS types as nothing at all (or as a UTI carrying no mime
    /// of its own) that are UTF-8 text by definition. Without them a `.rs` or a
    /// `docker-compose.yml` uploads as `application/octet-stream`, and the LLM
    /// layer only inlines a text-like mime — the user attaches a source file
    /// and the agent is handed a placeholder. Deliberately short: an extension
    /// that MIGHT be binary keeps the fallback, which degrades to that same
    /// placeholder rather than feeding the model garbage.
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

    /// A photo's mime, sniffed from the BYTES first. `supportedContentTypes`
    /// lists what the item CAN be delivered as and `loadTransferable` promises
    /// nothing about returning the first entry — when PhotosUI transcodes for
    /// compatibility the declared type and the actual bytes disagree, and the
    /// mime is what the gateway stores and what the provider is handed. The
    /// declared type fills in only for bytes the sniff doesn't recognise.
    static func photoMime(declared: String?, data: Data) -> String {
        let sniffed = sniffMime(data)
        if sniffed != fallbackMime { return sniffed }
        return declared ?? fallbackMime
    }

    private static let heifBrands = ["heic", "heix", "hevc", "mif1", "msf1"]

    static func sniffMime(_ data: Data) -> String {
        let head = [UInt8](data.prefix(12))
        if head.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if head.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if head.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        guard head.count == 12 else { return fallbackMime }
        let box = String(decoding: head[4...7], as: UTF8.self)
        let brand = String(decoding: head[8...11], as: UTF8.self)
        if brand == "WEBP" { return "image/webp" }
        // HEIC is an ISO-BMFF brand, and a picked photo is very often one: a
        // mime the kind derivation can't read as an image would ship the user's
        // photo as a plain file card.
        if box == "ftyp", heifBrands.contains(brand) { return "image/heic" }
        return fallbackMime
    }

    static func glyph(forMime mime: String) -> String {
        if mime.hasPrefix("image/") { return "photo" }
        if mime.hasPrefix("video/") { return "film" }
        if mime.hasPrefix("audio/") { return "waveform" }
        if mime.hasPrefix("text/") { return "doc.text" }
        return "doc"
    }

    /// The size as the wire carries it, or `nil` for a pick that must be
    /// rejected up front — over the gateway's blob cap, or (unreachably, since
    /// the cap is well under 4 GiB) too large for the field.
    static func wireSize(_ bytes: Int) -> UInt32? {
        guard bytes >= 0, bytes <= ComposerStaging.maxAttachmentBytes else { return nil }
        return UInt32(exactly: bytes)
    }

    static func byteText(_ bytes: UInt64) -> String {
        byteFormatter.string(fromByteCount: Int64(bytes))
    }

    /// `<tmp>/baybo-compose/<run id>/<pick id>.<ext>`: a photo's bytes land
    /// here and the upload streams off the PATH. Nothing retains the encoded
    /// pick — ten ProRAW picks held as `Data` for their tiles' lifetime is most
    /// of a gigabyte, and a foreground jetsam. The pick id keeps two picks
    /// apart; the extension only makes the path legible.
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

    /// Put a pick a `Draft` was holding back on the strip.
    ///
    /// A retained source is HARD-LINKED back into THIS run's spool directory and
    /// handed over as an ordinary `SpoolFile`, so a restored pick is
    /// indistinguishable from a fresh one — same ownership, same reclamation,
    /// same upload path — and, crucially, nothing on the strip ever holds a path
    /// under `drafts/`. That is what lets `DraftStore` prune whenever it likes:
    /// the upload streaming a restored pick is reading its own spool, and
    /// unlinking one name of a hard-linked inode takes no byte from the other.
    ///
    /// `nil` when the pick can no longer be sent at all: a bookmark whose
    /// document the user moved or deleted, or retained bytes that went missing
    /// under the app. The caller counts those and says so.
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

    /// The spool name is a FRESH uuid, not the pick's own. A restored pick keeps
    /// its id (its persisted files are named after it), so deriving the spool
    /// path from it would put two `ComposerStaging`s of one session — the live
    /// one and a retired one an in-flight upload is keeping alive — on the same
    /// path with two independent `SpoolFile`s, and the first `deinit` would
    /// unlink the file the other is streaming. A spool name only has to be
    /// unique.
    private static func relink(_ retained: URL, mime: String) -> SpoolFile? {
        let manager = FileManager.default
        guard manager.fileExists(atPath: retained.path),
            (try? manager.createDirectory(at: spoolDirectory, withIntermediateDirectories: true))
                != nil
        else { return nil }
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
    /// THIS run's spools, and nothing else's — which is what lets the sweep
    /// below reclaim by directory instead of having to know which individual
    /// files are still being read.
    static let spoolDirectory = spoolRoot.appendingPathComponent(runId, isDirectory: true)

    /// Reclaim the spools of runs that are OVER. A `SpoolFile` unlinks its own
    /// file when the last holder lets go, which a kill, a crash or a jetsam
    /// never gets to do: those bytes are then unreachable forever, so an
    /// abandoned ten-pick strip would sit in the temp dir at up to a gigabyte
    /// until iOS itself reclaims it. Run at LAUNCH, and structurally unable to
    /// touch a pick this run is still uploading — everything it spools is under
    /// `spoolDirectory`, the one child left alone.
    static func sweepAbandonedSpools() async {
        let manager = FileManager.default
        guard
            let runs = try? manager.contentsOfDirectory(
                at: spoolRoot, includingPropertiesForKeys: nil)
        else { return }
        for run in runs where run.lastPathComponent != spoolDirectory.lastPathComponent {
            try? manager.removeItem(at: run)
        }
    }

    /// A small decoded thumbnail off the spooled file. ImageIO downsamples
    /// DURING the decode, so the tile never holds a full-resolution backing
    /// store — and, unlike `UIImage(data:)`, never keeps the encoded pick alive
    /// behind it. `nil` for bytes no decoder recognises.
    static func thumbnail(at url: URL) -> UIImage? {
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
