import ImageIO
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// The composer's unsent DRAFT for one conversation: what the user typed, the
/// strip of picks between the picker and `AttachmentRef`, and every piece of
/// work still reading one.
///
/// Owned by the SESSION (`ChatStore.staging`) rather than by the composer's
/// frame: the work outlives the frame that started it — a photo's delivery, an
/// upload, a ✕ that lands in the middle of either — and that lifetime is what
/// the whole file is about. Four invariants carry it:
/// * **the strip is the truth.** Work that finishes writes state or raises a
///   notice only while its tile is still in `staged`; there is no parallel
///   bookkeeping that can drift from what the user can see. The one exception
///   is the in-flight upload count, which is what is on the WIRE — a tile the
///   user removed is off the strip and still on it.
/// * **a spool is owned, never deleted by hand.** The temp file belongs to a
///   `SpoolFile`, unlinked when the last holder lets go — so removing a tile
///   cannot pull the file out from under the upload that is reading it, and
///   the conversation going away cannot leave the bytes behind.
/// * **a draft outlives the frame AND the process.** `ChatScreen` docks the
///   composer in a `.safeAreaInset`, and every `fullScreenCover` over the chat
///   — the image viewer, the video player — tears that inset down and puts it
///   straight back; backing out to the list drops the frame for good. None of
///   that is the user discarding what they wrote, so none of it takes it away:
///   the text and the strip persist through `DraftStore` and come back on the
///   next visit, whether that is a second later or after a relaunch.
/// * **only the send discards.** `discardDraft` is the one path that empties
///   the strip, empties the field and deletes the draft off disk; leaving the
///   conversation merely flushes (`leaveConversation`). Everything else — an
///   LRU eviction, a jetsam, backgrounding — is a checkpoint, not a discard.
@MainActor
final class ComposerStaging: ObservableObject {
    /// What the user has typed and not sent. `ComposerView`'s field binds
    /// straight to it: a mirrored `@State` copy would have to be reconciled on
    /// every mount, and the mount edges are exactly where the draft is at risk.
    @Published var text: String = "" {
        didSet {
            guard text != oldValue else { return }
            scheduleDraftSave()
        }
    }
    @Published private(set) var staged: [StagedAttachment] = []

    /// Matches the gateway's 100 MiB blob cap (`MAX_BLOB_BYTES`) so an
    /// over-size pick is rejected up front instead of failing after upload.
    ///
    /// The three caps live HERE, on the machine that enforces them, and not on
    /// any one surface's store. They were `ChatStore`'s until 2026-08-25, and
    /// the cost of that home was already visible: `DeckStore` reached across
    /// for `ComposerStaging.maxAttachmentBytes` to size a share it has nothing to do
    /// with the chat about.
    nonisolated static let maxAttachmentBytes = 100 * 1024 * 1024
    /// How many picks the composer will stage on ONE message. A UI limit, not
    /// a second copy of a wire cap: multi-select makes an accidental 200-file
    /// pick one gesture away, and every staged item holds an upload, a
    /// thumbnail and a strip tile. The gateway enforces its own per-message
    /// attachment cap independently.
    static let maxStagedAttachments = 10
    /// How many staged picks upload at once; the rest queue. A ten-pick batch
    /// fired off in parallel is ten sockets on one uplink AND ten 100ms
    /// progress tickers hopping to the main actor — about a hundred composer
    /// re-evaluations a second, which defeats the coalescing the tick interval
    /// exists to provide.
    static let maxConcurrentUploads = 2

    /// How long typing settles before the draft reaches disk. Long enough that
    /// a burst of keystrokes is one write; short enough that an unannounced
    /// death costs a word. Every exit the app can SEE coming — leaving the
    /// conversation, backgrounding, an eviction, a send — flushes instead of
    /// waiting it out.
    static let draftSaveDebounce: Duration = .milliseconds(400)

    /// The surface this draft belongs to — a conversation, a project card.
    /// Only ever written to (`showNotice`), and weakly: an upload can outlive
    /// the screen that started it, and the host belongs to its own registry.
    private weak var host: (any ComposerHost)?
    /// The Rust core. Injected like `ChatStore`'s, so the staging machine can
    /// be driven with no gateway behind it.
    private let client: any BayboClientProtocol
    /// The system clipboard. Injected for the same reason as the client, only
    /// more so: `UIPasteboard.general` is process-global, and swift-testing runs
    /// suites in PARALLEL — one suite's paste written to the real board would
    /// surface as another's logic bug.
    private let pasteboard: any PasteboardReading
    /// The dock's notice line while the STRIP is what put it there, with the
    /// tile it names when there is one — so it can be taken back when that tile
    /// leaves, and in either case when the conversation does. A pick REJECTED
    /// before it ever took a slot (strip full, over the byte cap, unreadable)
    /// has no tile to name: nothing on the strip can retract its line, so only
    /// leaving does.
    private var stripNotice: (owner: UUID?, text: String)?
    /// Uploads actually on the wire. Deliberately not derived from the strip:
    /// a removed tile's upload runs to completion regardless (the generated
    /// UniFFI async binding has no cancellation hook), and counting only what
    /// is on SCREEN let a completing upload start two more against a slot the
    /// zombie still held — three at once on a cap of two.
    private var uploadsInFlight = 0
    /// Which draft this is, and where drafts live. Held rather than read off
    /// `host`, which is weak and may already be gone when a late upload
    /// finishes and writes the draft back.
    private let draftKey: DraftKey
    private let supportDirectory: URL
    /// The pending debounced write, or nil.
    private var draftSaveTask: Task<Void, Never>?
    /// Whether anything has changed since the last write. Without it every
    /// navigation away from every conversation would touch the disk to
    /// re-persist nothing.
    private var draftDirty = false
    /// Set while this machine is writing to its OWN published state — seeding
    /// from disk, emptying after a send — so the edit doesn't schedule a write
    /// of what was just read, or of what is about to be deleted.
    private var draftSavesSuspended = false
    /// Set by `retire()`: this machine no longer speaks for the conversation and
    /// must never touch its draft again.
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
            // `-baybo-demo-compose`: seed the staged strip (see `DemoFrames`).
            // Here rather than on the composer's appear, so it is one-shot per
            // conversation: a re-appear would otherwise refill a strip whose
            // emptiness is exactly what the smoke is checking. It only ever
            // ADDS to a restored strip — an empty seed must not erase a draft.
            staged.append(contentsOf: StagedAttachment.demoStagedIfRequested())
        #endif
    }

    // MARK: - The draft on disk

    /// Write the draft NOW rather than at the end of the debounce. Called
    /// wherever the app can see the composer going away — leaving the
    /// conversation, backgrounding, this store being evicted — so what reaches
    /// disk is never a keystroke behind what is on screen.
    func flushDraft() {
        guard draftDirty, !retired else { return }
        draftSaveTask?.cancel()
        draftSaveTask = nil
        writeDraft()
    }

    /// This machine is being dropped while its conversation lives on — an LRU
    /// eviction, a memory warning. Write what it has, then go INERT: cancel the
    /// work still reading the strip and refuse every later write.
    ///
    /// Going inert is the load-bearing half. An upload Task captures this object
    /// strongly and `blob_upload_file` has no cancellation hook, so the machine
    /// outlives the `ChatStore` that owned it for as long as anything is still on
    /// the wire — while re-opening the conversation builds a SECOND machine over
    /// the same `drafts/<session id>/`. The zombie's terminal write
    /// (`upload`'s `if holds(id)`) passes, because the tile is still on ITS
    /// strip, and would put a draft the live machine has since sent or cleared
    /// straight back on disk.
    func retire() {
        flushDraft()
        retired = true
        draftSaveTask?.cancel()
        draftSaveTask = nil
        for item in staged {
            item.work?.cancel()
        }
    }

    /// The user left the conversation. Nothing is taken away — walking away is
    /// not discarding, which is the whole feature — but two things do end with
    /// the visit: the strip's own notice line (it names a pick the next visit
    /// shows again, and the failure it announced is stale by then; `noteBlocked`
    /// re-raises it on the next send attempt) and the pending write.
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

    /// The one choke point, so the `retired` refusal cannot be routed around —
    /// the debounce's Task resumes as its own main-actor job and would otherwise
    /// reach here without passing `flushDraft`'s guard.
    private func writeDraft() {
        guard !retired else { return }
        draftDirty = false
        DraftStore.write(snapshotDraft(), key: draftKey, in: supportDirectory)
    }

    /// Suppress draft writes for the duration of an edit this machine is making
    /// to its own state — a seed read off disk, or the emptying a send does
    /// right before deleting the file.
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

    /// Keep what this pick needs to come back in a later process, and describe
    /// it.
    ///
    /// A pick that reached its blob needs only its thumbnail: `AttachmentRef` is
    /// the whole message from there on, and its bytes are the gateway's. One
    /// that has NOT needs the bytes as well, because nothing else holds them —
    /// which is the offline case, where every upload fails and the picks would
    /// otherwise vanish from a draft that still shows the text that went with
    /// them.
    ///
    /// `nil` for a pick there is nothing to keep — it stays on the strip, and
    /// only the DRAFT cannot carry it. Three ways, all of them narrow: a photo
    /// whose bytes PhotosUI has not delivered yet holds neither a blob nor a
    /// file and no later process can ask the picker again (the window is one
    /// `loadTransferable` wide); a spool that could be neither linked nor
    /// copied; and a Files pick whose bookmark could not be minted when it was
    /// admitted, inside the access bracket it arrived under.
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

    /// The tile image, at the size it is already downsampled to (256px through
    /// ImageIO). It is the only copy of a landed pick the draft keeps, and it
    /// costs tens of KB against the 100 MiB the pick itself can weigh.
    private static func writeThumbnail(_ image: UIImage, to url: URL) {
        guard !FileManager.default.fileExists(atPath: url.path),
            let data = image.jpegData(compressionQuality: 0.8)
        else { return }
        try? data.write(to: url, options: .atomic)
    }

    /// Give the draft its own name for the pick's spool. A HARD LINK, not a
    /// copy: it is O(1), it adds no second copy of a 100 MiB pick, and it means
    /// the bytes survive both `SpoolFile.deinit` and the launch sweep that
    /// reclaims a dead run's temp directory. Already linked is the common case
    /// — the record is rewritten on every change to the strip.
    private static func retainSource(_ spool: URL, at destination: URL) -> Bool {
        let manager = FileManager.default
        if manager.fileExists(atPath: destination.path) { return true }
        if (try? manager.linkItem(at: spool, to: destination)) != nil { return true }
        return (try? manager.copyItem(at: spool, to: destination)) != nil
    }

    /// Seed this conversation's composer from the draft on disk. Runs in `init`
    /// — before the first frame, so the field is never briefly empty — and
    /// re-queues every restored pick that still owes an upload.
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
        // A pick whose document the user moved or deleted between visits.
        //
        // Both halves are outside the suppression above, and neither is
        // optional. The RECORD still names the dead pick, and a seed dirties
        // nothing — so without a write the stale entry survives every visit,
        // re-raising an unactionable line forever; worse, a draft whose picks
        // ALL died is now an empty record that no longer deletes itself, which
        // is exactly what `startNewChat` resumes, so compose would land on that
        // dead session every time. And the LINE has to be deferred a turn twice
        // over: this machine is built lazily off `ChatStore.staging`, whose
        // first toucher is `ComposerView.init` — inside a view update, on an
        // object SwiftUI is already rendering — and `ChatScreen.onAppear`'s
        // `connectIfNeeded()` clears `notice` on its way out, which would wipe
        // a line raised any earlier.
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

    /// The drive every source whose bytes arrive LATER shares (a photo pick, a
    /// paste): take all the slots first, then fill them in one at a time. Both
    /// halves are load-bearing, and neither is the loader's business — which is
    /// why they live here and not once per source.
    ///
    /// **Every admitted pick takes its slot in the strip NOW, before a single
    /// byte is loaded.** `send()` reads the staged array to decide whether the
    /// message is ready, so a pick still inside its load would be invisible to
    /// it: the message shipped with whichever refs happened to be ready, the
    /// array was cleared, and the rest of the batch landed as ghost tiles on the
    /// NEXT message.
    ///
    /// **Sequential, and the handle lands before the body runs.** A load
    /// materialises the whole encoded pick, and ten at once is ten full-size
    /// `Data`s alive together. A task started on the main actor doesn't run until
    /// this turn ends, so `work` is on the tile before its own body can touch
    /// anything — a ✕ tapped mid-load always has something to cancel.
    private func admitThenLoad<Pick>(
        _ picks: [Pick], load: @escaping @MainActor (UUID, Pick) async -> Void
    ) {
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

    /// Is there an image on the clipboard? Read on the `+`'s tap to decide
    /// whether the panel offers a Paste row at all — a presence probe over
    /// `types(forItemSet:)`, which is documented not to notify the user, so
    /// asking costs nothing and no "Allow Paste?" alert can come of it.
    var pasteReady: Bool { !pasteboard.imageItemIndices().isEmpty }

    /// The Paste row. The clipboard's shape is known up front — which items hold
    /// an image, without pulling a byte — so `admitThenLoad` can take the slots
    /// before any bytes are read, exactly as a photo batch does; then each image
    /// is pulled and handed to `acceptPhoto`, which is where a pasted image and a
    /// picked one become the same thing.
    ///
    /// Nothing dismisses this row (there is no picker behind it), so the caller
    /// clears `AttachMenu.pick` itself.
    ///
    /// `authorized` says the system has ALREADY attributed this read to the user
    /// — it came from iOS's own Paste command (`ComposerPasteTarget`) rather
    /// than from a button of ours — which is the whole difference between a
    /// silent read and an "Allow Paste?" alert, and it changes how the bytes are
    /// pulled below.
    func stagePasteboard(authorized: Bool = false) {
        let indices = pasteboard.imageItemIndices()
        guard !indices.isEmpty else {
            // The board can empty (or turn into plain text) between the panel
            // opening and the row being tapped. Nothing was admitted, so no tile
            // can own the line.
            publishUnownedNotice(Lang.shared.t("chat.pasteNoImage"))
            return
        }
        admitThenLoad(indices) { id, index in
            await self.loadPasted(id: id, index: index, authorized: authorized)
        }
    }

    /// Pull ONE clipboard item's bytes and fill its tile in.
    ///
    /// The two ways to read, and why there are two. An UNAUTHORISED read — our
    /// own Paste row, which iOS does not count as user intent — can raise the
    /// system "Allow Paste?" alert, and the read BLOCKS the thread it is on
    /// until that is answered; on the main actor that is the whole composer,
    /// frozen mid-paste, so it goes to a detached task. An AUTHORISED one (iOS's
    /// own Paste command) is silent, and stays on the main actor deliberately:
    /// the permission belongs to the interaction, and hopping threads to read it
    /// later is how you land outside the window it granted.
    private func loadPasted(id: UUID, index: Int, authorized: Bool) async {
        guard holds(id) else { return }
        let reader = pasteboard
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

    /// One free slot in the strip, else the over-cap notice. Checked against
    /// the live array between appends, so a multi-select batch admits exactly
    /// up to the cap.
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

    /// A `fileImporter` URL is security-scoped: without the access bracket an
    /// iCloud / Files-provider document reads as silently EMPTY. The size comes
    /// from the URL's resource values, before any byte is read — the payload is
    /// never materialised just to find out it is over the cap.
    private func makeFileItem(_ url: URL) -> StagedAttachment? {
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

    /// Fill an admitted photo tile in once PhotosUI delivers its bytes. The
    /// tile is `pending` throughout, so the send gate blocks on it from
    /// admission to blob.
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

    /// What a DELIVERED photo goes through: the cap check, the mime sniff, the
    /// spool its upload will stream off, the thumbnail, then the upload queue.
    /// Split from the PhotosUI half above because every way this can end —
    /// including all the ways a pick the user has meanwhile removed has to end
    /// SILENTLY — lives on this side of the picker.
    func acceptPhoto(id: UUID, data: Data, declaredMime: String?) async {
        guard let byteCount = StagedAttachment.wireSize(data.count) else {
            drop(id, notice: Lang.shared.t("attach.tooLarge"))
            return
        }
        let mime = StagedAttachment.photoMime(declared: declaredMime, data: data)
        // Nothing to spool for a tile that is already gone — the ✕ during a
        // big pick's load is the common case, and it must not cost a 100 MiB
        // write.
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

    /// The staging work a photo needs done OFF the main actor: spool the
    /// encoded bytes and decode the small thumbnail off the file. A ProRAW or
    /// panorama pick runs to tens of MiB, and neither the write nor the decode
    /// belongs inside a frame. `nil` means nothing could read the bytes as an
    /// image — the spool is unlinked as the returned-nothing frame drops it.
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

    /// A pick that never made it out of staging: its tile goes — freeing the
    /// slot for the rest of the batch — and the notice says why. The notice is
    /// raised BEFORE the removal on purpose: `publishNotice` reads the strip to
    /// decide whether the user can still see what it is about.
    private func drop(_ id: UUID, notice text: String) {
        publishNotice(text, for: id)
        discard(id)
    }

    /// The ✕ on a tile. Unlike `drop`, this is the USER taking a pick back, so
    /// the notice that named it goes with it: the red line is what offered the
    /// ✕ in the first place, and leaving it over a strip that no longer shows
    /// the failed pick is a dead end the user can only dismiss by hand.
    func remove(_ id: UUID) {
        retractNotice(for: id)
        discard(id)
    }

    /// Take a tile out of the strip and STOP whatever is still reading it.
    /// Nothing is unlinked here: a photo's spool belongs to a `SpoolFile` the
    /// running upload holds as well, so the file lives exactly as long as
    /// something is still reading it. Deleting it here instead landed either
    /// between the two opens the Rust side makes (the hash pass, then the body
    /// reader) — failing the upload with a read error the user never asked for
    /// — or after both, letting it silently succeed. A Files pick's URL is the
    /// user's own document and was never ours to delete.
    private func discard(_ id: UUID) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        let removed = staged.remove(at: idx)
        removed.work?.cancel()
        scheduleDraftSave()
    }

    /// The draft is over: the field empties, the strip empties, and the record
    /// and every byte it was keeping leave the disk. Work still in flight is
    /// cancelled (no message will reference its blob now) and each spool is
    /// unlinked as its last holder lets go, so a 100 MiB photo never outlives
    /// the strip it was staged in. Whatever line the strip put on the dock goes
    /// too: it named a tile and there are no tiles left, or it named a pick that
    /// never became one and there is no longer anything for it to explain.
    ///
    /// Exactly two things end a draft, and this is both of them: **the message
    /// was sent**, and **the conversation was deleted**. The in-memory half
    /// matters as much as the file in the second case — `AppStore.requestDelete`
    /// evicts the store, and an eviction FLUSHES, so a machine still holding the
    /// draft would write it straight back after `SessionIndex.beginHide`
    /// deleted it. The resurrected file has no row to reach it from, which also
    /// makes it look exactly like an unsent new-chat draft to `startNewChat`.
    ///
    /// Leaving the conversation is emphatically NOT this — see
    /// `leaveConversation`.
    func discardDraft() {
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

    /// Is this pick still on screen? Every terminal write and every failure
    /// notice is conditional on it. A tile the user removed mid-load or
    /// mid-upload has taken its slot back, and the failure that arrives
    /// afterwards — a timeout, a photo that came back over-cap — names nothing
    /// they can act on: "Send failed: …" over a strip that no longer shows it
    /// is a dead end.
    private func holds(_ id: UUID) -> Bool {
        staged.contains { $0.id == id }
    }

    private func publishNotice(_ text: String, for id: UUID) {
        guard holds(id) else { return }
        stripNotice = (id, text)
        host?.notice = text
    }

    /// A line about a pick that never became a tile — still worth saying (the
    /// pick simply not appearing is the part the user can't explain), but it
    /// names nothing they can act on, so `clear()` is the only thing that can
    /// take it back. It goes through here rather than straight to
    /// the host's slot so the strip HAS it to take back: written directly, it
    /// outlived `leaveChat` and stood in red over the next visit's empty dock.
    private func publishUnownedNotice(_ text: String) {
        stripNotice = (nil, text)
        host?.notice = text
    }

    /// The Files browser handed back a failure instead of URLs: no pick, so no
    /// tile, so the line is the strip's own.
    func notePickerFailed() {
        publishUnownedNotice(Lang.shared.t("attach.attachFailed"))
    }

    /// What a send would carry, or `nil` because it must not go yet.
    ///
    /// **The one door out of the strip.** The gate used to live in the chat
    /// composer's `send()`, which was fine while there was one composer: a
    /// second surface reading `staged.compactMap(\.blobId)` for itself would
    /// ship the comment MINUS every pick still uploading or failed, silently,
    /// which is the exact failure the gate was written for. So the check, the
    /// line it raises and the trim all live here, and a caller cannot reach
    /// the picks without passing through them.
    ///
    /// Answers `nil` for an empty draft too — nothing typed, nothing staged.
    func claimSend() -> ComposerPayload? {
        // A pick whose upload failed has no blob to reference, so it cannot
        // ride the send — and dropping it silently ships the message MINUS the
        // file the user attached, with nothing to say so. Block on it instead:
        // the tile retries on tap, and its ✕ is right there.
        if let blocker = StagedAttachment.blocker(staged) {
            noteBlocked(blocker)
            return nil
        }
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty || !staged.isEmpty else { return nil }
        return ComposerPayload(text: body, picks: staged)
    }

    /// Why the send didn't go, named on the tile that is holding it up — so the
    /// line dies with that tile the same way an upload failure's does. The gate
    /// itself is `StagedAttachment.blocker`; this is only how it speaks.
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

    /// Take back the line this strip published about `id`, if it is still the
    /// one on the dock. The text check is what keeps a model or approval
    /// failure raised since then — the notice belongs to the whole dock, not to
    /// the composer — from being cleared by a ✕ it has nothing to do with.
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

    /// Start as many queued uploads as there are free slots, oldest first.
    /// Queue order and whether a pick still exists come from the strip; the
    /// budget does NOT. `staged.filter(isUploading)` counted tiles, and a
    /// removed tile's upload is off the strip while still on the wire: with two
    /// of four picks removed mid-flight, the first completion saw an empty
    /// strip, released BOTH queued picks, and ran three uploads against a cap
    /// of two. The counter is decremented by the same task that incremented it,
    /// after the call returns — cancellation doesn't skip it, because the call
    /// is not cancellable in the first place.
    private func pumpUploads() {
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

    /// Every staged pick uploads off a PATH — a Files pick in place, a photo
    /// off the spool written at staging — so the encoded bytes never cross the
    /// FFI, a 100 MiB pick is never held whole in memory, and a retry re-reads
    /// the file instead of anything having retained it. `source` stays on this
    /// frame for the whole call: that reference is what keeps a spool on disk
    /// across both opens even if its tile is removed mid-flight.
    ///
    /// Removing a tile cancels this task, but the call itself still runs to
    /// completion — the generated UniFFI async binding has no cancellation
    /// hook — so the bytes of a removed pick DO become a blob on the gateway
    /// that no message will ever reference. Nothing sweeps chat blobs, so that
    /// one is permanent; what cancellation buys is that its result reaches
    /// neither the strip nor the notice line.
    private func upload(_ id: UUID) async {
        guard let item = staged.first(where: { $0.id == id }), let source = item.source else {
            return
        }
        let url = source.url
        let scoped = source.isScoped && url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        // ARC may release a local right after its LAST use, and `source`'s is
        // `url.path` below — before the call it has to outlive. `url` is a
        // plain value and retains nothing. Without this the strip's copy going
        // away mid-upload can leave this frame holding the only reference,
        // drop it early, and unlink the file the upload is still reading.
        defer { withExtendedLifetime(source) {} }
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
        // Either outcome changes what the draft has to keep: a landed pick gives
        // its retained bytes back, a failed one still owes them. Guarded on the
        // tile, so a zombie upload finishing after the send that cleared the
        // draft can't write one back.
        if holds(id) { scheduleDraftSave() }
    }

    /// A tap on a failed tile. The claim happens on the CURRENT array element
    /// and BEFORE any await, so a double-tap — both taps reading the same
    /// render snapshot, both seeing `.error` — starts exactly one upload:
    /// two would mint two blobs on the gateway and race over which one the
    /// message ends up referencing.
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

/// What one send carries: the trimmed text and the picks behind it.
///
/// Deliberately the staged items rather than a wire type — the two surfaces
/// need different ones off them (`AttachmentRef` for a chat frame, blob id plus
/// filename for a card comment), and choosing between those is the caller's
/// business, not the strip's.
struct ComposerPayload {
    let text: String
    let picks: [StagedAttachment]
}
