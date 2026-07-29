import Foundation
import Testing
import UIKit

@testable import Baybo

/// The composer's outbound staging rules — everything a pick has to survive
/// between the picker and `AttachmentRef`.
///
/// The wire-field assertions replace a hardcode the composer shipped with while
/// it could only attach images: `kind: .image`, `filename: nil`, and a
/// `UInt32(clamping:)` size that saturated silently. The strip now stages
/// anything the Files browser offers, and the wire fields are DERIVED — from
/// the mime, never from which picker the file came through.
///
/// The rest pin the invariants a multi-select batch broke: a pick holds its
/// slot from ADMISSION (not from the moment its bytes finish loading), its
/// bytes live on disk rather than inside the tile, and a retry is claimed
/// exactly once.
@Suite @MainActor
struct ComposerStagingTests {
    private static let pdfName = "baybo-architecture-review-2026-Q3-final.pdf"
    /// A path every presentation binding will take and nothing will read.
    private static let devNull = URL(fileURLWithPath: "/dev/null")
    /// A Files pick that isn't there: the size read fails, so it never becomes
    /// a tile.
    private static let missingFile = URL(
        fileURLWithPath: NSTemporaryDirectory() + "baybo-no-such-pick.pdf")

    private func ready(
        mime: String, filename: String?, byteCount: UInt32 = 2048,
        source: URL = URL(fileURLWithPath: "/dev/null")
    ) -> StagedAttachment {
        StagedAttachment(
            preview: .file(name: filename ?? "", mime: mime),
            source: .scoped(source),
            mime: mime, filename: filename, byteCount: byteCount,
            state: .ready(blobId: "sha256:abc.tok"))
    }

    /// A 12-byte ISO-BMFF head: box size, `ftyp`, major brand.
    private static func ftyp(_ brand: String) -> Data {
        Data([0, 0, 0, 0x18]) + Data("ftyp".utf8) + Data(brand.utf8)
    }

    private static let heicBytes = ftyp("heic")
    private static let jpegBytes = Data([0xFF, 0xD8, 0xFF, 0xE0]) + Data(repeating: 0, count: 8)
    private static let pngBytes = Data([0x89, 0x50, 0x4E, 0x47]) + Data(repeating: 0, count: 8)
    private static let webpBytes =
        Data("RIFF".utf8) + Data([0, 0, 0, 0]) + Data("WEBP".utf8)

    /// Real encoded bytes — the spool's thumbnail goes through ImageIO, which
    /// a magic-byte fixture can't satisfy.
    private static func smallPNG() -> Data {
        let format = UIGraphicsImageRendererFormat.default()
        format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8), format: format)
            .pngData { ctx in
                UIColor(white: 0.5, alpha: 1).setFill()
                ctx.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
            }
    }

    // MARK: - kind derivation

    @Test func imageMimesCarryTheImageKind() {
        #expect(StagedAttachment.kind(forMime: "image/png") == .image)
        #expect(StagedAttachment.kind(forMime: "image/jpeg") == .image)
        // An image picked through FILES is still an image.
        #expect(StagedAttachment.kind(forMime: "image/heic") == .image)
        #expect(StagedAttachment.kind(forMime: "image/svg+xml") == .image)
    }

    @Test func audioMimesCarryTheAudioKind() {
        #expect(StagedAttachment.kind(forMime: "audio/mpeg") == .audio)
        #expect(StagedAttachment.kind(forMime: "audio/mp4") == .audio)
    }

    /// Video has no wire kind of its own — it rides `file` plus a `video/*`
    /// mime, and the transcript elects its tile from that mime.
    @Test func videoAndDocumentsCarryTheFileKind() {
        #expect(StagedAttachment.kind(forMime: "video/mp4") == .file)
        #expect(StagedAttachment.kind(forMime: "video/quicktime") == .file)
        #expect(StagedAttachment.kind(forMime: "application/pdf") == .file)
        #expect(StagedAttachment.kind(forMime: "text/markdown") == .file)
        #expect(StagedAttachment.kind(forMime: StagedAttachment.fallbackMime) == .file)
    }

    /// The extension is what a mime is derived from, which is why the tile
    /// middle-truncates the name rather than clipping its tail.
    @Test func mimeComesFromTheExtension() {
        #expect(StagedAttachment.mimeType(forExtension: "pdf") == "application/pdf")
        #expect(StagedAttachment.mimeType(forExtension: "mp4") == "video/mp4")
        #expect(StagedAttachment.mimeType(forExtension: "png") == "image/png")
    }

    /// `UTType` has no answer for a `.rs`, a `.toml` or a `docker-compose.yml`,
    /// and the LLM layer only inlines a text-like mime — so without this map
    /// the user attaches a source file and the agent is handed a placeholder.
    @Test func plainTextExtensionsReachTheModelAsText() {
        for ext in ["rs", "toml", "env", "RS"] {
            #expect(
                StagedAttachment.mimeType(forExtension: ext) == StagedAttachment.plainTextMime,
                "\(ext) must upload as text")
        }
        // Whatever this OS names these, none of them may reach the model as
        // opaque bytes.
        let widened = [
            "log", "yml", "yaml", "ini", "cfg", "conf", "sql", "kt", "go", "tsx", "jsx",
        ]
        for ext in widened {
            #expect(
                StagedAttachment.mimeType(forExtension: ext) != StagedAttachment.fallbackMime,
                "\(ext) must not upload as octet-stream")
        }
    }

    /// The map stays short on purpose: an extension nobody can vouch for as
    /// UTF-8 keeps `application/octet-stream`, which degrades to that same
    /// placeholder rather than feeding the model decoded garbage.
    @Test func unnameableExtensionsKeepTheOctetStreamFallback() {
        #expect(
            StagedAttachment.mimeType(forExtension: "notarealextension")
                == StagedAttachment.fallbackMime)
        #expect(StagedAttachment.mimeType(forExtension: "") == StagedAttachment.fallbackMime)
    }

    // MARK: - a photo's mime

    /// The BYTES outrank the declared type. `supportedContentTypes` lists what
    /// the item CAN be delivered as and `loadTransferable` promises nothing
    /// about returning the first entry — when PhotosUI transcodes for
    /// compatibility the two disagree, and the mime is what the gateway stores
    /// and what the provider is handed.
    @Test func theByteSniffOutranksTheDeclaredType() {
        #expect(
            StagedAttachment.photoMime(declared: "image/heic", data: Self.jpegBytes)
                == "image/jpeg")
        #expect(
            StagedAttachment.photoMime(declared: "image/jpeg", data: Self.heicBytes)
                == "image/heic")
    }

    /// Only where the sniff fires. Unrecognisable bytes leave the picker's own
    /// answer as the best one available, and an unidentifiable blob stays a
    /// plain file rather than being guessed at.
    @Test func theDeclaredTypeFillsInForBytesTheSniffCannotName() {
        let opaque = Data([0x00, 0x01, 0x02, 0x03])
        #expect(StagedAttachment.photoMime(declared: "image/heic", data: opaque) == "image/heic")
        #expect(
            StagedAttachment.photoMime(declared: nil, data: opaque)
                == StagedAttachment.fallbackMime)
        #expect(
            StagedAttachment.photoMime(declared: nil, data: Data())
                == StagedAttachment.fallbackMime)
    }

    /// The sniff has to name a HEIC: it is what a picked iPhone photo usually
    /// is, and a mime the kind derivation can't read as an image would ship
    /// the user's photo as a plain file card.
    @Test func theSniffNamesEveryFormatThePickerCanHandOver() {
        let heic = StagedAttachment.sniffMime(Self.heicBytes)
        #expect(heic == "image/heic")
        #expect(StagedAttachment.kind(forMime: heic) == .image)
        #expect(StagedAttachment.sniffMime(Self.ftyp("mif1")) == "image/heic")
        #expect(StagedAttachment.sniffMime(Self.jpegBytes) == "image/jpeg")
        #expect(StagedAttachment.sniffMime(Self.pngBytes) == "image/png")
        #expect(StagedAttachment.sniffMime(Self.webpBytes) == "image/webp")
        #expect(StagedAttachment.sniffMime(Self.ftyp("qt  ")) == StagedAttachment.fallbackMime)
    }

    // MARK: - the send gate

    /// A pick takes its slot in the strip the moment it is ADMITTED, not when
    /// its bytes finish loading. Staging appended a photo only after an
    /// `await` on `loadTransferable`, so a send fired mid-batch saw neither an
    /// uploading nor an errored item: it shipped with whichever refs happened
    /// to be ready, cleared the strip, and the rest of the batch landed as
    /// ghost tiles on the NEXT message.
    @Test func aPickWhoseBytesAreStillLoadingBlocksTheSend() {
        let pending = StagedAttachment.pendingPhoto()
        #expect(pending.state.isPending)
        #expect(pending.attachmentRef == nil)
        #expect(StagedAttachment.blocker([pending]) == .waiting)

        let batch = [ready(mime: "image/jpeg", filename: nil), pending]
        #expect(StagedAttachment.blocker(batch) == .waiting)
        #expect(batch.compactMap(\.attachmentRef).count == 1)
    }

    /// The three verdicts, in the order the composer applies them: a pick
    /// still working outranks a failed one (the failure may yet be retried
    /// into a blob), and a strip where everything landed doesn't block at all.
    @Test func theGateRanksWaitingOverFailedAndClearsWhenEveryPickLanded() {
        var failed = ready(mime: "application/pdf", filename: Self.pdfName)
        failed.state = .error
        var uploading = ready(mime: "image/png", filename: nil)
        uploading.state = .uploading(sent: 900)

        #expect(StagedAttachment.blocker([failed]) == .failed)
        #expect(StagedAttachment.blocker([failed, uploading]) == .waiting)
        #expect(StagedAttachment.blocker([]) == nil)
        #expect(StagedAttachment.blocker([ready(mime: "image/png", filename: nil)]) == nil)
    }

    // MARK: - the spool

    /// The encoded pick lands on DISK, and the staged item keeps only a path:
    /// ten ProRAW picks retained as `Data` for their tiles' lifetime is most of
    /// a gigabyte of foreground memory, on top of the decoded thumbnails. It
    /// lands under THIS RUN's directory, which is what lets the launch sweep
    /// reclaim by directory rather than having to know which individual files
    /// are still being read.
    @Test func aPhotoSpoolsToDiskUnderThisRunsDirectory() throws {
        let id = UUID()
        let spool = try StagedAttachment.spool(Self.smallPNG(), id: id, mime: "image/png")
        #expect(FileManager.default.fileExists(atPath: spool.url.path))
        #expect(spool.url.lastPathComponent.hasPrefix(id.uuidString))
        #expect(StagedAttachment.thumbnail(at: spool.url) != nil)
        #expect(
            spool.url.deletingLastPathComponent().lastPathComponent
                == StagedAttachment.spoolDirectory.lastPathComponent)
    }

    /// The spool is OWNED, never unlinked by hand: the tile and the upload
    /// reading it hold the same file, and it goes when the LAST of them does.
    /// Deleting it on removal instead landed either between the two opens the
    /// Rust side makes (the hash pass, then the body reader) — failing the
    /// upload with a read error the user never asked for — or after both,
    /// letting a pick the user had removed silently mint its blob anyway.
    @Test func theSpoolOutlivesTheTileButNotTheReadStillOnIt() throws {
        var staged: [StagedAttachment] = []
        // Nested so the only strong references that survive it are the two
        // under test: the strip's tile and the upload's frame.
        func stageOne() throws -> URL {
            let spool = try StagedAttachment.spool(Self.smallPNG(), id: UUID(), mime: "image/png")
            var item = StagedAttachment.pendingPhoto()
            item.source = .spooled(spool)
            staged.append(item)
            return spool.url
        }
        let url = try stageOne()
        #expect(FileManager.default.fileExists(atPath: url.path))

        // Scoped, not just a `var … = nil` later: a value is guaranteed dead at
        // the end of its scope, but ARC is free to release it any time after
        // its last use, so `withExtendedLifetime` is what pins the first half.
        do {
            let reader = staged[0].source  // what `upload` holds on its frame
            staged.removeAll()
            #expect(
                FileManager.default.fileExists(atPath: url.path),
                "an upload still reading the spool must keep it")
            withExtendedLifetime(reader) {}
        }
        #expect(!FileManager.default.fileExists(atPath: url.path))
    }

    /// A kill, a crash or a jetsam runs no `deinit`, so the spools of the strip
    /// it took with it stay — and their names are UUIDs nothing will ever ask
    /// for again (unlike the digest-keyed `baybo-preview` cache, which a
    /// re-open hits). Launch reclaims them by RUN directory, which is also what
    /// makes the sweep structurally unable to touch a pick this run is still
    /// uploading.
    @Test func theSweepReclaimsPreviousRunsAndSparesThisOne() async throws {
        let stray = StagedAttachment.spoolRoot
            .appendingPathComponent("run-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: stray, withIntermediateDirectories: true)
        let abandoned = stray.appendingPathComponent("abandoned.jpg")
        try Data("dead weight".utf8).write(to: abandoned)
        let live = try StagedAttachment.spool(Self.smallPNG(), id: UUID(), mime: "image/png")

        await StagedAttachment.sweepAbandonedSpools()

        #expect(!FileManager.default.fileExists(atPath: abandoned.path))
        #expect(!FileManager.default.fileExists(atPath: stray.path))
        #expect(
            FileManager.default.fileExists(atPath: live.url.path),
            "a pick this run is still holding is inside the directory the sweep skips")
    }

    /// A Files pick's URL is the user's own document, security-scoped and read
    /// in place: it carries no `SpoolFile`, so nothing unlinks it when the tile
    /// goes.
    @Test func aFilesPickIsNeverDeleted() throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-staging-\(UUID().uuidString).txt")
        try Data("hi".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        var staged: [StagedAttachment] = []
        staged.append(ready(mime: "text/plain", filename: url.lastPathComponent, source: url))
        staged.removeAll()
        #expect(FileManager.default.fileExists(atPath: url.path))
    }

    // MARK: - the strip's lifetime

    /// Leaving a conversation with picks still in it — the back button on a
    /// strip that was never sent. Nothing swept this before the strip had an
    /// owner: it was view state that died with the view, and its spools (up to
    /// 100 MiB each, ten to a strip) stayed on disk for good.
    @Test func everySpoolTheStripWasHoldingGoesWhenTheChatDoes() async throws {
        let fixture = Fixture()
        var paths: [String] = []
        for _ in 0..<2 {
            let id = try #require(fixture.staging.admitPhoto())
            await fixture.staging.acceptPhoto(
                id: id, data: Self.smallPNG(), declaredMime: "image/png")
            paths.append(try #require(fixture.spoolPath(fixture.staging.staged.count - 1)))
        }
        #expect(await waitUntil { StagedAttachment.blocker(fixture.staging.staged) == nil })
        #expect(paths.allSatisfy { FileManager.default.fileExists(atPath: $0) })

        fixture.store.leaveChat()

        #expect(fixture.staging.staged.isEmpty)
        #expect(paths.allSatisfy { !FileManager.default.fileExists(atPath: $0) })
    }

    /// A `fullScreenCover` over the chat — the image viewer, the video player —
    /// is NOT the user leaving it, and neither is a sheet (QuickLook, the share
    /// sheet, the message index). The composer is docked in `ChatScreen`'s
    /// `.safeAreaInset`, which a cover tears down and puts straight back, so
    /// reclaiming on the composer's own teardown cancelled every staged upload
    /// and emptied the strip because the user tapped an image in the transcript
    /// — the message then shipped MINUS its attachments, which is exactly what
    /// the failed-upload blocker exists to prevent. Only leaving the
    /// conversation reclaims (`AppStore.chatPath` → `ChatStore.leaveChat`).
    @Test func aPresentationOverTheChatIsNotLeavingIt() async throws {
        let fixture = Fixture()
        fixture.client.holdBlobUploads()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let path = try #require(fixture.spoolPath())
        let work = try #require(fixture.work())

        // Every presentation `ChatScreen` mounts, opened and dismissed.
        fixture.store.viewedImage = ViewedImage(id: "b1", image: UIImage(), url: nil)
        fixture.store.viewedImage = nil
        fixture.store.videoPlayback = VideoPlayback(id: "b2", url: Self.devNull)
        fixture.store.videoPlayback = nil
        fixture.store.filePreview = FilePreview(url: Self.devNull)
        fixture.store.filePreview = nil
        fixture.store.fileShare = FilePreview(url: Self.devNull)
        fixture.store.fileShare = nil

        #expect(fixture.staging.staged.count == 1, "a cover must not empty the strip")
        #expect(!work.isCancelled, "nor cancel the upload it is holding")
        #expect(FileManager.default.fileExists(atPath: path))

        // Backing out to the list is the one that means it.
        fixture.store.leaveChat()

        #expect(fixture.staging.staged.isEmpty)
        #expect(work.isCancelled)
        fixture.client.releaseBlobUploads()
        await work.value
        #expect(!FileManager.default.fileExists(atPath: path), "and the spool goes back")
        #expect(fixture.store.notice == nil)
    }

    /// The composer frame is rebuilt constantly (every cover, every keyboard
    /// beat); the strip is not. One conversation, one strip.
    @Test func theStripIsOwnedByTheSessionNotTheComposerFrame() {
        let fixture = Fixture()
        #expect(fixture.store.staging === fixture.staging)
        #expect(fixture.store.staging === fixture.store.staging)
    }

    /// What `AppStore.chatPath`'s didSet reads. A presentation over a chat
    /// changes no route, so it can never name a conversation as left; a pop, a
    /// swap and a logout all can.
    @Test func onlyANavigationChangeCanNameAConversationAsLeft() {
        let inChat: [AppStore.ChatRoute] = [.session("s1")]
        #expect(AppStore.sessionIds(inChat) == ["s1"])
        // The cover case: same path in, same path out.
        #expect(AppStore.sessionIds(inChat).subtracting(AppStore.sessionIds(inChat)).isEmpty)
        // Back to the list, and the logout that empties the stack outright.
        #expect(AppStore.sessionIds(inChat).subtracting(AppStore.sessionIds([])) == ["s1"])
        // A push-tap swapping one conversation for another leaves the first.
        #expect(
            AppStore.sessionIds(inChat).subtracting(AppStore.sessionIds([.session("s2")]))
                == ["s1"])
        // Deeper stacks: the chat pops off the archived screen, which stays.
        #expect(
            AppStore.sessionIds([.archived, .session("s1")])
                .subtracting(AppStore.sessionIds([.archived])) == ["s1"])
        #expect(AppStore.sessionIds([.archived, .cronJobs]).isEmpty)
    }

    /// Leaving mid-upload stops the work rather than letting it finish into a
    /// strip nobody can see.
    @Test func leavingCancelsWorkStillReadingTheStrip() async throws {
        let fixture = Fixture()
        fixture.client.holdBlobUploads()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let work = try #require(fixture.work())

        fixture.store.leaveChat()
        #expect(work.isCancelled)

        fixture.client.releaseBlobUploads()
        await work.value
        #expect(fixture.store.notice == nil)
    }

    // MARK: - a pick the user took back

    /// A pick removed while its bytes were loading takes its failure WITH it.
    /// Guarding the state writes was not enough: the notice beside them still
    /// fired, so seconds after a ✕ the dock announced "Send failed: …" for an
    /// attachment that was no longer on screen.
    @Test func aPickRemovedWhileItsBytesLoadPublishesNoNotice() async throws {
        let fixture = Fixture()
        let id = try #require(fixture.staging.admitPhoto())
        fixture.staging.remove(id)

        // The delivery lands afterwards, and turns out to be bytes no decoder
        // can read.
        await fixture.staging.acceptPhoto(id: id, data: Data([0, 1, 2, 3]), declaredMime: nil)

        #expect(fixture.store.notice == nil)
        #expect(fixture.staging.staged.isEmpty)
    }

    /// The control for the two tests above it: the same failure on a tile that
    /// IS still in the strip speaks up and frees its slot. The discriminator is
    /// the removal, not a swallowed error path.
    @Test func aPickStillInTheStripReportsItsLoadFailure() async throws {
        let fixture = Fixture()
        let id = try #require(fixture.staging.admitPhoto())

        await fixture.staging.acceptPhoto(id: id, data: Data([0, 1, 2, 3]), declaredMime: nil)

        #expect(fixture.store.notice != nil)
        #expect(fixture.staging.staged.isEmpty)
    }

    /// The ✕ on a spinning tile: the upload it abandons must keep reading the
    /// file it was handed, and must then reach neither the strip nor the notice
    /// line. Unlinking the spool from `remove` instead failed that upload with
    /// a read error — which the unguarded notice then showed the user for a
    /// tile they had just dismissed.
    @Test func removingATileMidUploadKeepsItsSpoolAndItsSilence() async throws {
        let fixture = Fixture()
        fixture.client.holdBlobUploads()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let path = try #require(fixture.spoolPath())
        let work = try #require(fixture.work())

        fixture.staging.remove(id)

        #expect(fixture.staging.staged.isEmpty)
        #expect(work.isCancelled)
        #expect(
            FileManager.default.fileExists(atPath: path),
            "the upload is still reading it — both opens have to see the file")

        // The flaky link finally gives up.
        fixture.client.releaseBlobUploads(.failure(BayboError.Other(message: "request timed out")))
        await work.value

        #expect(fixture.store.notice == nil, "a dismissed pick's failure is not the user's problem")
        #expect(!FileManager.default.fileExists(atPath: path), "and its spool is reclaimed")
    }

    /// The control: an upload that fails while its tile is still in the strip
    /// marks it `.error` — retryable on tap, blocking the send — and says so.
    @Test func aFailedUploadOnALiveTileReportsAndStaysRetryable() async throws {
        let fixture = Fixture()
        fixture.client.holdBlobUploads()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let work = try #require(fixture.work())

        fixture.client.releaseBlobUploads(.failure(BayboError.Other(message: "request timed out")))
        await work.value

        #expect(fixture.store.notice != nil)
        #expect(fixture.staging.staged.first?.state.isError == true)
        #expect(StagedAttachment.blocker(fixture.staging.staged) == .failed)
    }

    // MARK: - the line the strip put on the dock

    /// One staged photo whose upload has failed: the state that puts a red line
    /// in the dock, offers the retry, and blocks the send.
    private func stageAFailedUpload(_ fixture: Fixture) async throws -> UUID {
        fixture.client.holdBlobUploads()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let work = try #require(fixture.work())
        fixture.client.releaseBlobUploads(.failure(BayboError.Other(message: "request timed out")))
        await work.value
        return id
    }

    /// The ✕ is the action the failure notice OFFERS. Taking it has to retract
    /// the notice: retry and send both clear the line, and a remove that did
    /// not left "Send failed: request timed out" in red over an empty strip
    /// until the user dismissed it by hand.
    @Test func theXOnAFailedTileTakesItsNoticeWithIt() async throws {
        let fixture = Fixture()
        let id = try await stageAFailedUpload(fixture)
        #expect(fixture.store.notice != nil)

        fixture.staging.remove(id)

        #expect(fixture.staging.staged.isEmpty)
        #expect(fixture.store.notice == nil, "the line named a tile that is gone")
    }

    /// Same for the send gate's own line — it names the tile holding the
    /// message up, so removing that tile clears the block and the line at once.
    @Test func theXOnTheBlockingTileClearsTheSendGatesNotice() async throws {
        let fixture = Fixture()
        let id = try await stageAFailedUpload(fixture)
        let blocker = try #require(StagedAttachment.blocker(fixture.staging.staged))
        fixture.staging.noteBlocked(blocker)
        #expect(fixture.store.notice == Lang.shared.t("chat.removeFailedAttachment"))

        fixture.staging.remove(id)

        #expect(fixture.store.notice == nil)
    }

    /// But the dock's line belongs to the whole screen — a model failure and a
    /// failed approval land on it too — so a ✕ may only take back what the
    /// STRIP put there.
    @Test func aNoticeRaisedSinceSurvivesTheX() async throws {
        let fixture = Fixture()
        let id = try await stageAFailedUpload(fixture)
        fixture.store.notice = Lang.shared.t("chat.modelFailed")

        fixture.staging.remove(id)

        #expect(fixture.store.notice == Lang.shared.t("chat.modelFailed"))
    }

    /// Leaving the conversation empties the strip, so it takes the strip's line
    /// too: coming back to a red "Send failed" over a dock with nothing in it
    /// is the same dead end one tile down.
    @Test func leavingTheChatTakesTheStripsNoticeWithIt() async throws {
        let fixture = Fixture()
        _ = try await stageAFailedUpload(fixture)
        #expect(fixture.store.notice != nil)

        fixture.store.leaveChat()

        #expect(fixture.store.notice == nil)
    }

    /// And the harder half: a pick REJECTED before it ever took a slot leaves a
    /// line with NO tile to own it. Every rejection wrote the dock directly, so
    /// the strip held no record of the line and `leaveChat` walked past it —
    /// the next visit opened on "you can attach at most 10 files" in red over
    /// an EMPTY strip, naming a tile that never existed.
    @Test func leavingTakesTheLineAnOverCapPickLeftBehind() throws {
        let fixture = Fixture()
        for _ in 0..<ChatStore.maxStagedAttachments {
            _ = try #require(fixture.staging.admitPhoto())
        }
        #expect(fixture.staging.admitPhoto() == nil, "the eleventh pick has no slot")
        #expect(
            fixture.store.notice
                == String(
                    format: Lang.shared.t("chat.tooManyAttachments"),
                    ChatStore.maxStagedAttachments))

        fixture.store.leaveChat()

        #expect(fixture.staging.staged.isEmpty)
        #expect(fixture.store.notice == nil, "no tile to name, so nothing to come back to")
    }

    /// Same for a Files pick whose size can't even be read: it never becomes a
    /// tile, and the line saying so must not outlive the strip it never joined.
    @Test func leavingTakesTheLineAnUnreadableFilesPickLeftBehind() {
        let fixture = Fixture()
        fixture.staging.stage(files: [Self.missingFile])
        #expect(fixture.staging.staged.isEmpty)
        #expect(fixture.store.notice == Lang.shared.t("chat.attachFailed"))

        fixture.store.leaveChat()

        #expect(fixture.store.notice == nil)
    }

    /// And when the Files browser hands back a failure instead of URLs there is
    /// no pick at all behind the line — still the strip's to take back.
    @Test func leavingTakesThePickersOwnFailureWithIt() {
        let fixture = Fixture()
        fixture.staging.notePickerFailed()
        #expect(fixture.store.notice == Lang.shared.t("chat.attachFailed"))

        fixture.store.leaveChat()

        #expect(fixture.store.notice == nil)
    }

    // MARK: - the concurrency budget

    /// The budget counts what is on the WIRE, not what is on screen. A removed
    /// tile's upload runs to completion regardless — the generated UniFFI async
    /// binding has no cancellation hook — so deriving it from
    /// `staged.filter(isUploading)` let the first completion release BOTH
    /// queued picks while the second abandoned upload was still streaming:
    /// three at once on a cap of two, and the zombie's 100ms progress ticker
    /// still paying a main-actor hop per tick.
    @Test func aRemovedTilesUploadKeepsItsSlotUntilItLands() async throws {
        let fixture = Fixture()
        fixture.client.holdBlobUploads()
        var ids: [UUID] = []
        for _ in 0..<4 {
            let id = try #require(fixture.staging.admitPhoto())
            await fixture.staging.acceptPhoto(
                id: id, data: Self.smallPNG(), declaredMime: "image/png")
            ids.append(id)
        }
        #expect(
            await waitUntil {
                fixture.client.parkedBlobUploads == ChatStore.maxConcurrentUploads
            }, "the cap holds the last two back")
        #expect(fixture.client.blobUploadCalls.count == ChatStore.maxConcurrentUploads)

        // The user takes back both picks that are mid-flight, then the first of
        // the two abandoned uploads lands.
        fixture.staging.remove(ids[0])
        fixture.staging.remove(ids[1])
        fixture.client.releaseOneBlobUpload()

        #expect(
            await waitUntil { fixture.client.blobUploadCalls.count == 3 },
            "the freed slot admits exactly one queued pick")
        let overran = await waitUntil(timeout: .milliseconds(200)) {
            fixture.client.blobUploadCalls.count > 3
        }
        #expect(!overran, "the second abandoned upload still holds its slot")
        #expect(fixture.staging.staged.map(\.state.isUploading) == [true, false])

        fixture.client.releaseBlobUploads()
        #expect(await waitUntil { fixture.staging.staged.allSatisfy { !$0.state.isPending } })
    }

    /// Every staged pick uploads off a PATH — the encoded bytes never cross the
    /// FFI, so a 100 MiB pick is never held whole in memory.
    @Test func aStagedPickUploadsOffItsPath() async throws {
        let fixture = Fixture()
        let id = try #require(fixture.staging.admitPhoto())
        await fixture.staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })

        let call = try #require(fixture.client.blobUploadCalls.first)
        #expect(call.mimeType == "image/png")
        #expect(
            call.path.hasPrefix(StagedAttachment.spoolDirectory.path),
            "a photo streams off this run's spool")
        #expect(
            await waitUntil {
                fixture.staging.staged.first?.attachmentRef?.blobId
                    == FakeBayboClient.uploadedBlobId
            })
    }

    // MARK: - retry

    /// A double-tap on a failed tile: both taps read the same render snapshot
    /// and both see `.error`, so the CLAIM is what has to be idempotent. Two
    /// uploads would mint two blobs on the gateway and race over which one the
    /// message ends up referencing.
    @Test func aFailedTileRetriesExactlyOncePerFailure() {
        var item = ready(mime: "application/pdf", filename: Self.pdfName)
        item.state = .error

        // `claimRetry` is mutating, and `#expect` expands its argument into a
        // closure that captures the receiver immutably — the claim has to land
        // in a local first.
        let firstTap = item.claimRetry()
        #expect(firstTap)
        #expect(item.state.isPending)
        let secondTap = item.claimRetry()
        #expect(!secondTap)

        // Nothing else is retryable: a landed pick has its blob, and one
        // already on the wire would be uploaded twice.
        item.state = .ready(blobId: "sha256:abc.tok")
        let onReady = item.claimRetry()
        #expect(!onReady)
        item.state = .uploading(sent: 10)
        let onUploading = item.claimRetry()
        #expect(!onUploading)
        item.state = .queued
        let onQueued = item.claimRetry()
        #expect(!onQueued)
    }

    // MARK: - the wire reference

    @Test func theRealFilenameRidesTheSend() {
        let ref = ready(mime: "application/pdf", filename: Self.pdfName).attachmentRef
        #expect(ref?.filename == Self.pdfName)
        #expect(ref?.kind == .file)
        #expect(ref?.mimeType == "application/pdf")
        #expect(ref?.size == 2048)
        #expect(ref?.blobId == "sha256:abc.tok")
    }

    /// A photo carries no name of its own; the gateway names the block from
    /// its mime rather than storing an empty string.
    @Test func aNamelessPickShipsANilFilename() {
        #expect(ready(mime: "image/jpeg", filename: nil).attachmentRef?.filename == nil)
    }

    /// A pick with no blob can't ride the send — `send()` blocks on it rather
    /// than shipping the message minus the attachment.
    @Test func anUnfinishedPickHasNoReference() {
        var item = ready(mime: "application/pdf", filename: Self.pdfName)
        item.state = .uploading(sent: 12)
        #expect(item.attachmentRef == nil)
        item.state = .error
        #expect(item.attachmentRef == nil)
    }

    // MARK: - size

    /// The wire field is a `u32` and the card's progress denominator reads it,
    /// so an over-cap pick is REJECTED rather than reported at a saturated
    /// size.
    @Test func overCapPicksAreRejectedNotClamped() {
        #expect(StagedAttachment.wireSize(0) == 0)
        #expect(StagedAttachment.wireSize(1_048_576) == 1_048_576)
        #expect(StagedAttachment.wireSize(ChatStore.maxAttachmentBytes) != nil)
        #expect(StagedAttachment.wireSize(ChatStore.maxAttachmentBytes + 1) == nil)
        #expect(StagedAttachment.wireSize(Int(UInt32.max) + 1) == nil)
        #expect(StagedAttachment.wireSize(-1) == nil)
    }

    /// One cap, not two: the UI limit is a count, and the byte cap stays the
    /// gateway's `MAX_BLOB_BYTES` mirror. A full strip must QUEUE rather than
    /// upload all at once — ten parallel uploads is ten progress tickers
    /// hopping to the main actor.
    @Test func theStagedCapIsACountAndTheByteCapIsTheGateways() {
        #expect(ChatStore.maxStagedAttachments == 10)
        #expect(ChatStore.maxAttachmentBytes == 100 * 1024 * 1024)
        #expect(ChatStore.maxConcurrentUploads > 0)
        #expect(ChatStore.maxConcurrentUploads < ChatStore.maxStagedAttachments)
    }

    // MARK: - chat-list preview

    /// An attachment-only send used to leave the row showing the PREVIOUS
    /// message: the gateway's own preview is `None` for a media-only turn, and
    /// `merge` keeps a local preview over a nil remote one, so this string is
    /// that row's only second line.
    @Test func attachmentOnlySendsDescribeTheirAttachments() {
        let ref = ready(mime: "application/pdf", filename: Self.pdfName).attachmentRef
        let preview = SessionIndex.attachmentPreview([ref].compactMap { $0 })
        #expect(preview.contains(Self.pdfName))
        #expect(SessionIndex.attachmentPreview([]).isEmpty)
    }

    /// A photo-only batch has no name to show, so it falls back to the count
    /// rather than rendering a bare label.
    @Test func aNamelessBatchFallsBackToItsCount() {
        let refs = [
            ready(mime: "image/jpeg", filename: nil).attachmentRef,
            ready(mime: "image/png", filename: "  ").attachmentRef,
        ].compactMap { $0 }
        #expect(refs.count == 2)
        #expect(SessionIndex.attachmentPreview(refs).contains("2"))
    }

    /// Same truncation as every other capture, so the live row and the row a
    /// later merge writes agree.
    @Test func theAttachmentPreviewTruncatesLikeTheGateways() {
        let long = String(repeating: "a", count: 400) + ".pdf"
        let preview = SessionIndex.attachmentPreview(
            [ready(mime: "application/pdf", filename: long).attachmentRef].compactMap { $0 })
        #expect(preview.unicodeScalars.count == SessionIndex.previewMaxChars + 1)
        #expect(preview.hasSuffix("…"))
    }

    /// The attachment description drives the second line only. `userText` is
    /// the bold-line fallback until the title pass runs, and a filename names
    /// no conversation.
    @Test func anAttachmentOnlySendKeepsTheUserTextLabel() {
        let temp = TempSupportDir()
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: "s1", text: "what changed in the release?")
        let ref = ready(mime: "application/pdf", filename: Self.pdfName).attachmentRef
        index.recordUserSend(sessionId: "s1", text: "", attachments: [ref].compactMap { $0 })

        let row = index.rows.first { $0.id == "s1" }
        #expect(row?.preview?.contains(Self.pdfName) == true)
        #expect(row?.userText == "what changed in the release?")
    }
}

/// One staging machine over a fake core, plus the store whose notice line it
/// writes and the support root the two hang off (deleted with the fixture).
@MainActor
private final class Fixture {
    let temp = TempSupportDir()
    let client = FakeBayboClient()
    let store: ChatStore
    let staging: ComposerStaging

    init() {
        let sessionId = "s-compose"
        store = ChatStore(
            sessionId: sessionId, client: client, index: temp.makeIndex(),
            outbox: temp.makeOutbox(sessionId: sessionId))
        // The SESSION's strip, not one built beside it: which object the
        // composer renders — and what its lifetime is tied to — is half of
        // what these tests are about.
        staging = store.staging
    }

    /// A staged pick's spool path, read through a CALL so no copy of the item —
    /// and so no second reference to its `SpoolFile` — survives in the test's
    /// own scope to keep the file alive past what is being asserted.
    func spoolPath(_ index: Int = 0) -> String? {
        staging.staged[index].source?.url.path
    }

    func work(_ index: Int = 0) -> Task<Void, Never>? {
        staging.staged[index].work
    }
}
