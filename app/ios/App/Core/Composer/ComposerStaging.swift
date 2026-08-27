import ImageIO
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// Session-owned unsent state. Picker and upload work may outlive the view that
/// started it, so navigation checkpoints the draft instead of discarding it.
@MainActor
final class ComposerStaging: ObservableObject {
    @Published var text: String = "" {
        didSet {
            guard text != oldValue else { return }
            scheduleDraftSave()
        }
    }
    @Published private(set) var staged: [StagedAttachment] = []

    nonisolated static let maxAttachmentBytes = 100 * 1024 * 1024
    static let maxStagedAttachments = 10
    static let maxConcurrentUploads = 2

    static let draftSaveDebounce: Duration = .milliseconds(400)

    private weak var host: (any ComposerHost)?
    /// The Rust core. Injected like `ChatStore`'s, so the staging machine can
    /// be driven with no gateway behind it.
    private let client: any BayboClientProtocol
    private let pasteboard: any PasteboardReading
    private var stripNotice: (owner: UUID?, text: String)?
    private var uploadsInFlight = 0
    private let draftKey: DraftKey
    private let supportDirectory: URL
    /// The pending debounced write, or nil.
    private var draftSaveTask: Task<Void, Never>?
    private var draftDirty = false
    private var draftSavesSuspended = false
    /// Upload tasks may retain an old machine; retirement prevents it from
    /// overwriting or resurrecting the replacement machine's draft.
    private var retired = false

    init(
        host: any ComposerHost,
        client: any BayboClientProtocol = Baybo.client,
        pasteboard: any PasteboardReading = Pasteboards.launch(),
        supportDirectory: URL = SessionIndex.supportDirectory()
    ) {
        self.host = host
        draftKey = host.draftKey
        self.client = client
        self.pasteboard = pasteboard
        self.supportDirectory = supportDirectory
        restoreDraft()
        #if DEBUG
            staged.append(contentsOf: StagedAttachment.demoStagedIfRequested())
        #endif
    }

    // MARK: - The draft on disk

    func flushDraft() {
        guard draftDirty, !retired else { return }
        draftSaveTask?.cancel()
        draftSaveTask = nil
        writeDraft()
    }

    func retire() {
        flushDraft()
        retired = true
        draftSaveTask?.cancel()
        draftSaveTask = nil
        for item in staged {
            item.work?.cancel()
        }
    }

    func leaveConversation() {
        retractStripNotice()
        flushDraft()
    }

    private func scheduleDraftSave() {
        guard !draftSavesSuspended, !retired else { return }
        draftDirty = true
        draftSaveTask?.cancel()
        draftSaveTask = Task { [weak self] in
            try? await Task.sleep(for: Self.draftSaveDebounce)
            guard !Task.isCancelled, let self else { return }
            self.draftSaveTask = nil
            self.writeDraft()
        }
    }

    private func writeDraft() {
        guard !retired else { return }
        draftDirty = false
        DraftStore.write(snapshotDraft(), key: draftKey, in: supportDirectory)
    }

    private func withoutDraftSaves(_ body: () -> Void) {
        let previous = draftSavesSuspended
        draftSavesSuspended = true
        body()
        draftSavesSuspended = previous
    }

    /// The draft as it stands, having first kept on disk whatever each pick
    /// still needs to be sent from a LATER process.
    private func snapshotDraft() -> Draft {
        guard !staged.isEmpty else { return Draft(text: text, attachments: []) }
        let directory = DraftStore.prepareDirectory(for: draftKey, in: supportDirectory)
        return Draft(text: text, attachments: staged.compactMap { retain($0, in: directory) })
    }

    private func retain(_ item: StagedAttachment, in directory: URL) -> DraftAttachment? {
        var isImage = false
        if case .image(let thumbnail) = item.preview {
            isImage = true
            if let thumbnail {
                Self.writeThumbnail(
                    thumbnail,
                    to: DraftStore.thumbURL(pickId: item.id.uuidString, in: directory))
            }
        }

        var blobId: String?
        if case .ready(let id) = item.state { blobId = id }

        var bookmark: Data?
        if blobId == nil {
            switch item.source {
            case .spooled(let file):
                guard
                    Self.retainSource(
                        file.url,
                        at: DraftStore.sourceURL(pickId: item.id.uuidString, in: directory))
                else { return nil }
            case .scoped:
                bookmark = item.bookmark
                guard bookmark != nil else { return nil }
            case nil:
                return nil
            }
        }

        return DraftAttachment(
            id: item.id.uuidString, isImage: isImage, mime: item.mime,
            filename: item.filename, byteCount: item.byteCount, blobId: blobId,
            bookmark: bookmark)
    }

    private static func writeThumbnail(_ image: UIImage, to url: URL) {
        guard !FileManager.default.fileExists(atPath: url.path),
            let data = image.jpegData(compressionQuality: 0.8)
        else { return }
        try? data.write(to: url, options: .atomic)
    }

    private static func retainSource(_ spool: URL, at destination: URL) -> Bool {
        let manager = FileManager.default
        if manager.fileExists(atPath: destination.path) { return true }
        if (try? manager.linkItem(at: spool, to: destination)) != nil { return true }
        return (try? manager.copyItem(at: spool, to: destination)) != nil
    }

    private func restoreDraft() {
        var lost = 0
        withoutDraftSaves {
            guard let draft = DraftStore.read(key: draftKey, in: supportDirectory)
            else { return }
            text = draft.text
            let directory = DraftStore.directory(for: draftKey, in: supportDirectory)
            for record in draft.attachments {
                guard let item = StagedAttachment.restored(record, from: directory) else {
                    lost += 1
                    continue
                }
                staged.append(item)
            }
            pumpUploads()
        }
        guard lost > 0 else { return }
        draftDirty = true
        flushDraft()
        Task { @MainActor [weak self] in
            self?.publishUnownedNotice(Lang.shared.t("attach.draftAttachmentsLost"))
        }
    }

    // MARK: - Admission

    func stage(photos picks: [PhotosPickerItem]) {
        admitThenLoad(picks) { id, pick in await self.loadPhoto(id: id, pick: pick) }
    }

    private func admitThenLoad<Pick>(
        _ picks: [Pick], load: @escaping @MainActor (UUID, Pick) async -> Void
    ) {
        // Reserve every visible slot before loading so send sees pending picks;
        // load serially to avoid retaining several full-size Data values.
        var admitted: [(id: UUID, pick: Pick)] = []
        for pick in picks {
            guard let id = admitPhoto() else { break }
            admitted.append((id, pick))
        }
        guard !admitted.isEmpty else { return }
        Task {
            for entry in admitted {
                let work = Task { await load(entry.id, entry.pick) }
                self.update(entry.id) { $0.work = work }
                await work.value
            }
        }
    }

    func stage(files urls: [URL]) {
        for url in urls {
            guard admitSlot() else { break }
            guard let item = makeFileItem(url) else { continue }
            staged.append(item)
        }
        scheduleDraftSave()
        pumpUploads()
    }

    var pasteReady: Bool { !pasteboard.imageItemIndices().isEmpty }

    func stagePasteboard(authorized: Bool = false) {
        let indices = pasteboard.imageItemIndices()
        guard !indices.isEmpty else {
            publishUnownedNotice(Lang.shared.t("chat.pasteNoImage"))
            return
        }
        admitThenLoad(indices) { id, index in
            await self.loadPasted(id: id, index: index, authorized: authorized)
        }
    }

    private func loadPasted(id: UUID, index: Int, authorized: Bool) async {
        guard holds(id) else { return }
        let reader = pasteboard
        // The permission prompt may block; keep an unauthorised read off the main actor.
        let pasted =
            authorized
            ? reader.image(at: index)
            : await Task.detached(priority: .userInitiated) {
                reader.image(at: index)
            }.value
        guard let pasted else {
            drop(id, notice: Lang.shared.t("attach.attachFailed"))
            return
        }
        await acceptPhoto(id: id, data: pasted.data, declaredMime: pasted.mime)
    }

    /// A photo pick's slot in the strip, before PhotosUI has delivered a byte
    /// of it; `nil` once the strip is full.
    func admitPhoto() -> UUID? {
        guard admitSlot() else { return nil }
        let item = StagedAttachment.pendingPhoto()
        staged.append(item)
        scheduleDraftSave()
        return item.id
    }

    private func admitSlot() -> Bool {
        guard staged.count < ComposerStaging.maxStagedAttachments else {
            publishUnownedNotice(
                String(
                    format: Lang.shared.t("attach.tooManyAttachments"),
                    ComposerStaging.maxStagedAttachments))
            return false
        }
        return true
    }

    private func makeFileItem(_ url: URL) -> StagedAttachment? {
        // Inspect and bookmark the Files URL while its security scope is active;
        // the upload later reopens that scope without materializing the file.
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let bytes = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize else {
            publishUnownedNotice(Lang.shared.t("attach.attachFailed"))
            return nil
        }
        guard let byteCount = StagedAttachment.wireSize(bytes) else {
            publishUnownedNotice(Lang.shared.t("attach.tooLarge"))
            return nil
        }
        let name = url.lastPathComponent
        let mime = StagedAttachment.mimeType(forExtension: url.pathExtension)
        return StagedAttachment(
            preview: .file(name: name, mime: mime), source: .scoped(url), mime: mime,
            filename: name, byteCount: byteCount,
            // Minted here, inside the access bracket the pick arrives under, and
            // carried for the tile's whole life — see `StagedAttachment.bookmark`.
            bookmark: try? url.bookmarkData())
    }

    // MARK: - A photo's bytes

    private func loadPhoto(id: UUID, pick: PhotosPickerItem) async {
        guard holds(id) else { return }
        let data: Data
        do {
            guard let loaded = try await pick.loadTransferable(type: Data.self) else {
                drop(id, notice: Lang.shared.t("attach.attachFailed"))
                return
            }
            data = loaded
        } catch {
            drop(
                id,
                notice: String(format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error)))
            return
        }
        await acceptPhoto(
            id: id, data: data,
            declaredMime: pick.supportedContentTypes.compactMap(\.preferredMIMEType).first)
    }

    func acceptPhoto(id: UUID, data: Data, declaredMime: String?) async {
        guard let byteCount = StagedAttachment.wireSize(data.count) else {
            drop(id, notice: Lang.shared.t("attach.tooLarge"))
            return
        }
        let mime = StagedAttachment.photoMime(declared: declaredMime, data: data)
        guard holds(id) else { return }
        guard let spool = await Self.spoolPhoto(data, id: id, mime: mime) else {
            drop(id, notice: Lang.shared.t("attach.attachFailed"))
            return
        }
        // Removed while its bytes were being written: the file goes with the
        // local reference, which is the only one that ever held it.
        guard holds(id) else { return }
        update(id) {
            $0.preview = .image(spool.image)
            $0.source = .spooled(spool.file)
            $0.mime = mime
            $0.byteCount = byteCount
        }
        scheduleDraftSave()
        pumpUploads()
    }

    private static func spoolPhoto(
        _ data: Data, id: UUID, mime: String
    ) async -> (file: SpoolFile, image: UIImage)? {
        await Task.detached(priority: .userInitiated) {
            guard let file = try? StagedAttachment.spool(data, id: id, mime: mime) else {
                return nil
            }
            guard let image = StagedAttachment.thumbnail(at: file.url) else { return nil }
            return (file, image)
        }.value
    }

    // MARK: - Leaving the strip

    private func drop(_ id: UUID, notice text: String) {
        publishNotice(text, for: id)
        discard(id)
    }

    func remove(_ id: UUID) {
        retractNotice(for: id)
        discard(id)
    }

    private func discard(_ id: UUID) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        let removed = staged.remove(at: idx)
        // Do not unlink here: a non-cancellable Rust upload may still hold the
        // SpoolFile even after its Swift task is cancelled.
        removed.work?.cancel()
        scheduleDraftSave()
    }

    func discardDraft() {
        // Only send and explicit deletion use this path; ordinary navigation
        // calls leaveConversation() and preserves the draft.
        withoutDraftSaves {
            for item in staged {
                item.work?.cancel()
            }
            staged.removeAll()
            text = ""
        }
        retractStripNotice()
        draftSaveTask?.cancel()
        draftSaveTask = nil
        draftDirty = false
        DraftStore.delete(key: draftKey, in: supportDirectory)
    }

    private func holds(_ id: UUID) -> Bool {
        staged.contains { $0.id == id }
    }

    private func publishNotice(_ text: String, for id: UUID) {
        guard holds(id) else { return }
        stripNotice = (id, text)
        host?.notice = text
    }

    private func publishUnownedNotice(_ text: String) {
        stripNotice = (nil, text)
        host?.notice = text
    }

    /// The Files browser handed back a failure instead of URLs: no pick, so no
    /// tile, so the line is the strip's own.
    func notePickerFailed() {
        publishUnownedNotice(Lang.shared.t("attach.attachFailed"))
    }

    func claimSend() -> ComposerPayload? {
        // A pending or failed tile blocks the whole send so attachments are
        // never silently omitted from the message.
        if let blocker = StagedAttachment.blocker(staged) {
            noteBlocked(blocker)
            return nil
        }
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty || !staged.isEmpty else { return nil }
        return ComposerPayload(text: body, picks: staged)
    }

    func noteBlocked(_ blocker: StagedAttachment.Blocker) {
        let subject: StagedAttachment?
        let text: String
        switch blocker {
        case .waiting:
            subject = staged.first { $0.state.isPending }
            text = Lang.shared.t("attach.waitingUpload")
        case .failed:
            subject = staged.first { $0.state.isError }
            text = Lang.shared.t("attach.removeFailedAttachment")
        }
        guard let subject else { return }
        publishNotice(text, for: subject.id)
    }

    private func retractNotice(for id: UUID) {
        guard let current = stripNotice, current.owner == id else { return }
        retractStripNotice()
    }

    private func retractStripNotice() {
        guard let current = stripNotice else { return }
        stripNotice = nil
        guard host?.notice == current.text else { return }
        host?.notice = nil
    }

    // MARK: - Uploads

    private func pumpUploads() {
        // Count wire work independently of the strip: removing a tile cancels
        // its UI task, but the underlying UniFFI upload can still be running.
        while uploadsInFlight < ComposerStaging.maxConcurrentUploads,
            let idx = staged.firstIndex(where: { $0.state.isQueued && $0.source != nil })
        {
            staged[idx].state = .uploading(sent: 0)
            let id = staged[idx].id
            uploadsInFlight += 1
            let task = Task {
                await self.upload(id)
                self.uploadsInFlight -= 1
                self.pumpUploads()
            }
            update(id) { $0.work = task }
        }
    }

    private func upload(_ id: UUID) async {
        guard let item = staged.first(where: { $0.id == id }), let source = item.source else {
            return
        }
        let url = source.url
        let scoped = source.isScoped && url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        // The owner keeps a spool or security scope alive while Rust reads the path.
        defer { withExtendedLifetime(source) {} }
        // UniFFI async calls are not cancellable; cancellation only prevents a
        // removed tile from receiving the eventual result.
        do {
            let blobId = try await client.blobUploadFile(
                path: url.path, mimeType: item.mime,
                progress: BlobProgressForwarder { [weak self] sent, _ in
                    self?.update(id) {
                        guard $0.state.isUploading else { return }
                        $0.state = .uploading(sent: sent)
                    }
                })
            update(id) { $0.state = .ready(blobId: blobId) }
        } catch {
            update(id) { $0.state = .error }
            publishNotice(
                String(format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error)), for: id)
        }
        if holds(id) { scheduleDraftSave() }
    }

    func retry(_ id: UUID) {
        var claimed = false
        update(id) { claimed = $0.claimRetry() }
        guard claimed else { return }
        retractNotice(for: id)
        scheduleDraftSave()
        pumpUploads()
    }

    private func update(_ id: UUID, _ mutate: (inout StagedAttachment) -> Void) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        mutate(&staged[idx])
    }
}

struct ComposerPayload {
    let text: String
    let picks: [StagedAttachment]
}
