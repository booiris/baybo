import Foundation
import Testing
import UIKit

@testable import Baybo

/// The composer's unsent draft: what survives leaving a conversation, what
/// survives the PROCESS, and what a send takes away.
///
/// The relaunch half is the point of most of these, and it is simulated the way
/// `OutboxStoreTests` does it — a second `ChatStore` over the same support root,
/// which is exactly what the next launch builds. Anything asserted only against
/// the live object would pass with nothing on disk at all.
@Suite @MainActor
struct ComposerDraftTests {
    private let temp = TempSupportDir()

    /// A store + staging machine over the shared support root, as a fresh launch
    /// would build it. Each call is a new "process".
    private func relaunch(
        sessionId: String = "s-draft", client: FakeBayboClient = FakeBayboClient()
    ) -> ComposerStaging {
        store(sessionId: sessionId, client: client).staging
    }

    private func store(
        sessionId: String = "s-draft", client: FakeBayboClient = FakeBayboClient(),
        index: SessionIndex? = nil
    ) -> ChatStore {
        ChatStore(
            sessionId: sessionId, client: client, index: index ?? temp.makeIndex(),
            outbox: temp.makeOutbox(sessionId: sessionId), pasteboard: FakePasteboard(),
            supportDirectory: temp.url)
    }

    /// 8×8 PNG bytes — the spool thumbnail goes through ImageIO, so the fixture
    /// has to be a real image (`ComposerStagingTests` makes the same one).
    private static func smallPNG() -> Data {
        UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8)).pngData { context in
            UIColor.systemTeal.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
        }
    }

    // MARK: - the typed text

    /// The field is the conversation's, not the frame's. Before this it was
    /// `ComposerView`'s `@State`, so backing out to the list threw away whatever
    /// was half-written.
    @Test func whatWasTypedComesBackAfterLeaving() {
        let staging = relaunch()
        staging.text = "half a thought"

        staging.leaveConversation()

        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url)?.text == "half a thought")
    }

    @Test func whatWasTypedComesBackAfterARelaunch() {
        let first = relaunch()
        first.text = "半句话"
        first.flushDraft()

        #expect(relaunch().text == "半句话")
    }

    /// Whitespace is preserved verbatim while the draft lives — re-opening must
    /// give back exactly what was typed — but it does not COUNT as a draft, or a
    /// stray space would keep a dead new-chat session reachable forever.
    @Test func aWhitespaceOnlyFieldIsNotADraft() {
        let staging = relaunch()
        staging.text = "   "
        staging.flushDraft()

        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) == nil)
    }

    /// Clearing the field retires the draft, rather than leaving an empty record
    /// that `startNewChat` would keep resuming.
    @Test func emptyingTheFieldRetiresTheDraft() {
        let staging = relaunch()
        staging.text = "typed"
        staging.flushDraft()
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) != nil)

        staging.text = ""
        staging.flushDraft()

        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) == nil)
    }

    /// The send is the one path that discards.
    @Test func sendingDeletesTheDraft() async throws {
        let staging = relaunch()
        staging.text = "shipped"
        let id = try #require(staging.admitPhoto())
        await staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        staging.flushDraft()
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) != nil)

        staging.discardDraft()

        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) == nil)
        #expect(
            !FileManager.default.fileExists(
                atPath: DraftStore.directory(for: .chat("s-draft"), in: temp.url).path),
            "and every byte it was keeping")
    }

    // MARK: - staged picks

    /// A pick that reached its blob needs no local bytes to be sent from a later
    /// process: `AttachmentRef` is the whole message. What it does need is its
    /// tile, which is why the thumbnail is kept.
    @Test func aLandedPickComesBackReadyToSend() async throws {
        let client = FakeBayboClient()
        let first = relaunch(client: client)
        let id = try #require(first.admitPhoto())
        await first.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { StagedAttachment.blocker(first.staged) == nil })
        first.flushDraft()

        let second = relaunch()

        #expect(second.staged.count == 1)
        let ref = try #require(second.staged.first?.attachmentRef)
        #expect(ref.blobId == FakeBayboClient.uploadedBlobId)
        #expect(ref.kind == .image)
        #expect(ref.mimeType == "image/png")
        if case .image(let thumbnail) = try #require(second.staged.first?.preview) {
            #expect(thumbnail != nil, "the tile has to draw something")
        } else {
            Issue.record("a photo pick must restore as an image tile")
        }
    }

    /// The realistic reason a pick has no blob is being OFFLINE: every upload
    /// fails. Those picks are exactly the ones whose bytes nobody else holds, so
    /// the draft keeps them — and re-queues the upload on the next open, which is
    /// where the network usually is again.
    @Test func aPickThatNeverUploadedKeepsItsBytesAndRetriesOnTheNextOpen() async throws {
        let client = FakeBayboClient()
        client.holdBlobUploads()
        let first = relaunch(client: client)
        let id = try #require(first.admitPhoto())
        await first.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { client.blobUploadCalls.count == 1 })
        let work = try #require(first.staged.first?.work)
        client.releaseBlobUploads(.failure(BayboError.Other(message: "offline")))
        await work.value
        #expect(first.staged.first?.state.isError == true)
        first.flushDraft()

        let next = FakeBayboClient()
        let second = relaunch(client: next)

        #expect(second.staged.count == 1)
        #expect(await waitUntil { next.blobUploadCalls.count == 1 }, "the pick re-queues itself")
        #expect(await waitUntil { StagedAttachment.blocker(second.staged) == nil })
        #expect(second.staged.first?.attachmentRef?.blobId == FakeBayboClient.uploadedBlobId)
    }

    /// The retained bytes are a HARD LINK, not a second copy — the whole reason
    /// keeping a 100 MiB pick is affordable — and the restore links them back
    /// into this run's spool directory, so nothing on the strip ever holds a path
    /// the draft prunes.
    @Test func retainedBytesAreLinkedNotCopiedAndTheRestoreLinksThemBack() async throws {
        let client = FakeBayboClient()
        client.holdBlobUploads()
        let first = relaunch(client: client)
        let id = try #require(first.admitPhoto())
        await first.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { client.blobUploadCalls.count == 1 })
        first.flushDraft()

        let retained = DraftStore.sourceURL(
            pickId: id.uuidString, in: DraftStore.directory(for: .chat("s-draft"), in: temp.url))
        let spool = try #require(first.staged.first?.source?.url)
        #expect(FileManager.default.fileExists(atPath: retained.path))
        #expect(
            try FileManager.default.attributesOfItem(atPath: retained.path)[.systemFileNumber]
                as? Int
                == FileManager.default.attributesOfItem(atPath: spool.path)[.systemFileNumber]
                as? Int,
            "one inode, two names")

        let second = relaunch()
        let restored = try #require(second.staged.first?.source?.url)
        #expect(
            restored.path.hasPrefix(StagedAttachment.spoolDirectory.path),
            "a restored pick reads its own spool, never a path under drafts/")
    }

    /// A pick the user took back gives its retained bytes straight back too —
    /// the prune runs on every write, and can, because nothing on the strip is
    /// reading a draft path.
    @Test func removingATileReclaimsWhatTheDraftWasKeepingForIt() async throws {
        let client = FakeBayboClient()
        client.holdBlobUploads()
        let staging = relaunch(client: client)
        let id = try #require(staging.admitPhoto())
        await staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { client.blobUploadCalls.count == 1 })
        staging.text = "keeps the draft alive"
        staging.flushDraft()
        let retained = DraftStore.sourceURL(
            pickId: id.uuidString, in: DraftStore.directory(for: .chat("s-draft"), in: temp.url))
        #expect(FileManager.default.fileExists(atPath: retained.path))

        staging.remove(id)
        staging.flushDraft()

        #expect(!FileManager.default.fileExists(atPath: retained.path))
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url)?.attachments.isEmpty == true)
    }

    /// A landed pick gives its retained bytes back as soon as the upload lands:
    /// from then on the blob is the message and the copy is dead weight.
    @Test func aLandedPickStopsCostingItsBytes() async throws {
        let client = FakeBayboClient()
        client.holdBlobUploads()
        let staging = relaunch(client: client)
        let id = try #require(staging.admitPhoto())
        await staging.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { client.blobUploadCalls.count == 1 })
        staging.flushDraft()
        let retained = DraftStore.sourceURL(
            pickId: id.uuidString, in: DraftStore.directory(for: .chat("s-draft"), in: temp.url))
        #expect(FileManager.default.fileExists(atPath: retained.path))

        let work = try #require(staging.staged.first?.work)
        client.releaseBlobUploads()
        await work.value
        staging.flushDraft()

        #expect(!FileManager.default.fileExists(atPath: retained.path))
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url)?.attachments.count == 1)
    }

    /// A photo admitted but not yet delivered by PhotosUI holds neither a blob
    /// nor a file, and no later process can ask the picker for it again. It stays
    /// on the strip — only the draft cannot carry it.
    @Test func aPickWithNoBytesYetStaysOnTheStripButNotInTheDraft() throws {
        let staging = relaunch()
        _ = try #require(staging.admitPhoto())
        staging.text = "with a photo coming"
        staging.flushDraft()

        #expect(staging.staged.count == 1)
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url)?.attachments.isEmpty == true)
    }

    /// A pick the draft can no longer restore — a Files document the user moved
    /// or deleted between visits — is dropped, said out loud, AND pruned from the
    /// record. The pruning is what stops the dead entry from re-raising an
    /// unactionable line on every future visit; and when it was the draft's ONLY
    /// content, from leaving behind an empty record that no longer deletes itself
    /// and that `startNewChat` would resume forever.
    @Test func aPickThatCannotBeRestoredIsDroppedAndPrunedFromTheRecord() async {
        DraftStore.write(
            Draft(
                text: "",
                attachments: [
                    DraftAttachment(
                        id: UUID().uuidString, isImage: false, mime: "application/pdf",
                        filename: "gone.pdf", byteCount: 12, blobId: nil,
                        bookmark: Data("not a bookmark".utf8))
                ]),
            key: .chat("s-draft"), in: temp.url)

        let staging = relaunch()

        #expect(staging.staged.isEmpty)
        #expect(
            DraftStore.read(key: .chat("s-draft"), in: temp.url) == nil,
            "an all-lost draft must retire itself rather than come back every open")
        #expect(await waitUntil { staging.text.isEmpty })
    }

    // MARK: - the record on disk

    /// Deleting the conversation deletes the half-written message in it: there
    /// would be no row left to reach it from.
    @Test func deletingTheConversationDeletesItsDraft() {
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: "s-doomed", text: "hi")
        DraftStore.write(
            Draft(text: "unsent", attachments: []), key: .chat("s-doomed"), in: temp.url)

        index.beginHide("s-doomed")

        #expect(DraftStore.read(key: .chat("s-doomed"), in: temp.url) == nil)
    }

    /// Deleting the FILE is only half of it. A resident store holds the draft in
    /// memory and writes it back on its next flush — and the resurrected file,
    /// having no row, is exactly what `startNewChat` resumes, so compose would
    /// land in the conversation the user just deleted.
    @Test func deletingTheConversationAlsoDropsAResidentStoresCopy() {
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: "s-doomed", text: "hi")
        let store = store(sessionId: "s-doomed", index: index)
        var discarded: Set<String> = []
        index.onSessionsRemoved = { ids in
            discarded.formUnion(ids)
            for id in ids where id == store.sessionId { store.discardDraft() }
        }
        store.staging.text = "still typing when it went"
        store.flushDraft()

        index.beginHide("s-doomed")
        store.flushDraft()

        #expect(discarded == ["s-doomed"], "the registry has to name what it removed")
        #expect(store.staging.text.isEmpty)
        #expect(DraftStore.read(key: .chat("s-doomed"), in: temp.url) == nil)
    }

    /// The same hole one path over: a conversation deleted from ANOTHER client
    /// arrives as a plain list merge, with no local delete to hang the discard on.
    @Test func aMergeDrivenRemovalNamesTheSessionsItDropped() {
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: "s-elsewhere", text: "hi")
        DraftStore.write(
            Draft(text: "unsent", attachments: []), key: .chat("s-elsewhere"), in: temp.url)
        var dropped: Set<String> = []
        index.onSessionsRemoved = { dropped.formUnion($0) }

        index.merge(remote: [], fetchEpoch: index.mutationEpoch)

        #expect(dropped == ["s-elsewhere"])
        #expect(DraftStore.read(key: .chat("s-elsewhere"), in: temp.url) == nil)
    }

    /// A retired machine — its store evicted, but the object kept alive by an
    /// upload still on the wire — must never touch the draft again. Two live
    /// machines over one directory is how a draft the user already sent comes
    /// back from the dead.
    @Test func aRetiredMachineStopsWritingItsDraft() {
        let staging = relaunch()
        staging.text = "checkpointed"
        staging.retire()
        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url)?.text == "checkpointed")

        DraftStore.delete(key: .chat("s-draft"), in: temp.url)
        staging.text = "written by a zombie"
        staging.flushDraft()

        #expect(DraftStore.read(key: .chat("s-draft"), in: temp.url) == nil)
    }

    /// Two `ComposerStaging`s of one session can be alive at once (an eviction
    /// mid-upload), and a restored pick keeps its persisted id — so the spool it
    /// links back into this run's directory must NOT be named after that id, or
    /// the retired machine's `SpoolFile` unlinks the live one's bytes.
    @Test func twoMachinesRestoringOnePickDoNotShareASpoolPath() async throws {
        let client = FakeBayboClient()
        client.holdBlobUploads()
        let first = relaunch(client: client)
        let id = try #require(first.admitPhoto())
        await first.acceptPhoto(id: id, data: Self.smallPNG(), declaredMime: "image/png")
        #expect(await waitUntil { client.blobUploadCalls.count == 1 })
        first.flushDraft()

        let second = relaunch()
        let third = relaunch()

        let a = try #require(second.staged.first?.source?.url)
        let b = try #require(third.staged.first?.source?.url)
        #expect(a != b, "two restores of one pick must not claim the same spool path")
        #expect(FileManager.default.fileExists(atPath: a.path))
        #expect(FileManager.default.fileExists(atPath: b.path))
    }

    /// Logout / rebind: the drafts belong to the departing gateway.
    @Test func unbindingWipesEveryDraft() {
        let index = temp.makeIndex()
        for id in ["s-1", "s-2"] {
            DraftStore.write(Draft(text: "unsent", attachments: []), key: .chat(id), in: temp.url)
        }

        index.removeAll()

        #expect(DraftStore.sessionIds(in: temp.url).isEmpty)
    }

    // MARK: - resuming the new-chat draft

    /// A draft session has no registry row and no gateway row, so its uuid is
    /// the only handle that exists: compose has to return to it, or what the user
    /// typed is stranded in a conversation nothing can ever name again.
    @Test func composeResumesTheUnsentDraftSession() {
        DraftStore.write(Draft(text: "unsent", attachments: []), key: .chat("s-new"), in: temp.url)

        #expect(
            AppStore.unsentDraftSessionId(in: temp.url, isListed: { _ in false }) == "s-new")
    }

    /// A LISTED session's draft is not the new-chat draft: that conversation has
    /// a row, so it is reachable from the list — where its own draft comes back
    /// on the tap — and compose must still open something new.
    @Test func aSentConversationsDraftIsNotWhatComposeResumes() {
        DraftStore.write(Draft(text: "unsent", attachments: []), key: .chat("s-old"), in: temp.url)

        #expect(AppStore.unsentDraftSessionId(in: temp.url, isListed: { $0 == "s-old" }) == nil)
    }

    @Test func composeMintsFreshWhenNoDraftIsWaiting() {
        #expect(AppStore.unsentDraftSessionId(in: temp.url, isListed: { _ in false }) == nil)
    }

    /// The row is written only after `ensureRemoteSession()` succeeds, so a first
    /// send made OFFLINE leaves the conversation row-less with its message queued.
    /// Typing one more line into it must not make compose re-open the
    /// conversation the user just sent to.
    @Test func aRowlessConversationWithAQueuedSendIsNotANewChatDraft() {
        let outbox = temp.makeOutbox(sessionId: "s-offline")
        outbox.beginSend(platformMsgId: "m-1", text: "sent while offline", attachments: [])
        DraftStore.write(
            Draft(text: "and then some more", attachments: []), key: .chat("s-offline"),
            in: temp.url)

        #expect(AppStore.unsentDraftSessionId(in: temp.url, isListed: { _ in false }) == nil)
    }
}
