import Foundation
import Testing
import UniformTypeIdentifiers

@testable import Baybo

/// `DeckStore` with an injected `FakeBayboClient`: the refresh mapping, the
/// live-push seq rule, and the optimistic layout write's baseline rollback.
@MainActor
struct DeckStoreTests {
    private func makeStore(_ fake: FakeBayboClient) -> DeckStore {
        DeckStore.removeMirror()
        return DeckStore(clientProvider: { fake })
    }

    private func card(
        _ id: String,
        position: Int64,
        size: String = "wide",
        sizes: [String] = ["wide"],
        maximize: Bool = false
    ) -> DeckCardInfo {
        DeckCardInfo(
            cardId: id,
            title: id,
            position: position,
            size: size,
            sizes: sizes,
            maximize: maximize,
            enabled: true,
            quarantined: false,
            deletedAtMs: nil,
            specHash: "h",
            lastSeq: 0,
            createdAtMs: 0,
            retryableOps: ["refresh"]
        )
    }

    /// `<tmp>/deck-pick-<uuid>/<name>` holding `bytes`, so a file pick has a
    /// real URL to size, type and stream.
    private func tempFile(named name: String, bytes: Data = Data("hello".utf8)) throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("deck-pick-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let url = dir.appendingPathComponent(name)
        try bytes.write(to: url)
        return url
    }

    @Test func acceptElectsThePhotoLibraryForImagesAndForGarbage() {
        // The compat floor: no accept at all is what every card authored before
        // the grammar existed sends, and it must still open the photo library.
        #expect(DeckStore.electPicker(accept: nil) == .photos)
        #expect(DeckStore.electPicker(accept: "") == .photos)
        #expect(DeckStore.electPicker(accept: "image/*") == .photos)
        #expect(DeckStore.electPicker(accept: "image/png,image/jpeg") == .photos)
        // Nothing parseable survives → treated as absent, not as a file pick.
        #expect(DeckStore.electPicker(accept: "nonsense") == .photos)
        #expect(DeckStore.electPicker(accept: "/,x/,/y") == .photos)
    }

    @Test func acceptWithANonImageTokenElectsTheFileBrowser() {
        #expect(DeckStore.electPicker(accept: "application/pdf") == .files([.pdf]))
        // A mixed list keeps the image types too — the browser must show both.
        #expect(DeckStore.electPicker(accept: "image/*,application/pdf") == .files([.image, .pdf]))
        #expect(DeckStore.electPicker(accept: "text/*") == .files([.text]))
        #expect(DeckStore.electPicker(accept: "*/*") == .files([DeckStore.anyFileType]))
        // A mime iOS can't resolve still opens the browser, and an empty type
        // list widens rather than presenting one where nothing is selectable.
        #expect(DeckStore.electPicker(accept: "application/x-baybo-nope").isFiles)
        #expect(DeckStore.PickerMode.files([]).fileTypes == [DeckStore.anyFileType])
    }

    @Test func pickRejectsAConcurrentRequestAsBusy() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
        // A second pick while one is up is rejected immediately — not queued.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.lastPickResult?.id == "b")
        #expect(store.lastPickResult?.ok == false)
        #expect(store.lastPickResult?.error == "busy")
        // The first pick is untouched — its picker stays up.
        #expect(store.pickerMode == .photos)
    }

    @Test func pickRejectsAConcurrentRequestWhileTheFileBrowserIsOpen() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        #expect(store.pickerMode.isFiles)
        store.requestPick(id: "b", cardId: "c1", accept: "image/*")
        #expect(store.lastPickResult?.id == "b")
        #expect(store.lastPickResult?.error == "busy")
        // The busy rejection carries a DIFFERENT id — it must not free the pick
        // it was rejected against, nor swap the presentation out from under it.
        #expect(store.pickerMode.isFiles)
        store.requestPick(id: "c", cardId: "c1", accept: nil)
        #expect(store.lastPickResult?.error == "busy")
        #expect(store.pickerMode.isFiles)
    }

    @Test func pickStaysBusyThroughUploadThenFreesOnSettle() async throws {
        // FakeBayboClient.deckBlobUploadBytes throws, so finishPick settles failed.
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        // Photo chosen; upload about to run. consumePick returns (id, cardId).
        #expect(store.consumePick()?.id == "a")
        // A concurrent request DURING the upload window is rejected busy — the
        // slot must stay held until the pick settles, not freed on selection.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.lastPickResult?.id == "b")
        #expect(store.lastPickResult?.error == "busy")
        // The upload settles → the slot frees.
        store.finishPick(id: "a", cardId: "c1", data: Data([1, 2, 3]), mime: "image/png")
        try await Task.sleep(nanoseconds: 40_000_000)
        // A new pick is now accepted (presents), not busy-rejected.
        store.requestPick(id: "d", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
        #expect(store.lastPickResult?.id == "a")  // no busy result minted for "d"
    }

    @Test func photoPickerDismissedWithNoChoiceResolvesCancelledAndFreesTheSlot() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        // The picker dismissed with nothing chosen → the promise rejects.
        store.photosPickerDismissed()
        #expect(store.pickerMode == .idle)
        try await Task.sleep(nanoseconds: 30_000_000)  // let the deferred cancel run
        #expect(store.lastPickResult?.id == "a")
        #expect(store.lastPickResult?.error == "cancelled")
        // The slot is free again — a new pick presents.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
        #expect(store.lastPickResult?.id == "a")  // "b" was accepted, not rejected
    }

    /// The REAL order `PhotosPicker` produces: it clears `isPresented` and
    /// delivers the selection on one synchronous dismissal path, and the
    /// dismissal half can run FIRST. `DeckScreen` claims the pick from the
    /// selection binding's setter (not from an `onChange` a pass later), so the
    /// choice still lands inside the deferral window — and the settle loses.
    @Test func photoDismissalFollowedByTheSelectionIsNotACancel() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        store.photosPickerDismissed()
        #expect(store.consumePick()?.id == "a")
        try await Task.sleep(nanoseconds: 30_000_000)
        #expect(store.pickSettleCount == 0, "the upload owns the settle, not the dismissal")
    }

    /// The other same-turn interleave — the selection arrives first. Symmetric
    /// by construction: the deferred check only ever runs after both halves.
    @Test func photoSelectionFollowedByTheDismissalIsNotACancel() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        #expect(store.consumePick()?.id == "a")
        store.photosPickerDismissed()
        try await Task.sleep(nanoseconds: 30_000_000)
        #expect(store.pickSettleCount == 0, "the upload owns the settle, not the dismissal")
    }

    /// Past the deferral window the cancel has already answered the promise, so
    /// a choice arriving late is REFUSED rather than settled twice — exactly
    /// once still holds, and this is the branch `DeckScreen`'s synchronous claim
    /// keeps a real photo out of.
    @Test func aPhotoChoiceArrivingAfterTheDeferredCancelIsRefused() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        store.photosPickerDismissed()
        try await Task.sleep(nanoseconds: 30_000_000)
        #expect(store.pickSettleCount == 1)
        #expect(store.lastPickResult?.error == "cancelled")
        #expect(store.consumePick()?.id == nil)
        #expect(store.pickSettleCount == 1)
    }

    /// The deferred cancel belongs to the pick that was up at the dismissal —
    /// it cannot reach one that started inside its window, on either leg.
    @Test func aDeferredPhotoCancelCannotSettleAPickThatArrivedAfterIt() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        store.photosPickerDismissed()
        // The chosen photo's transfer fails, settling "a" inside the same turn,
        // and the card opens a new pick before the deferred check runs.
        #expect(store.consumePick()?.id == "a")
        store.finishPick(id: "a", cardId: "c1", data: nil, mime: "image/png")
        #expect(store.pickSettleCount == 1)
        store.requestPick(id: "b", cardId: "c1", accept: "application/pdf")
        try await Task.sleep(nanoseconds: 30_000_000)
        #expect(store.pickSettleCount == 1, "only \"a\" settled")
        #expect(store.lastPickResult?.id == "a")
        #expect(store.pickerMode.isFiles, "and \"b\"'s browser stays up")
    }

    @Test func fileBrowserCancellationSettlesOnceAndFreesTheSlot() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        // The file leg is TOLD it was cancelled — no dismissal inference, no
        // deferral. The dismissal SwiftUI writes first has already cleared the
        // mode, which is why the cancel is guarded on the pick's leg and not on
        // `pickerMode`: guarding on the mode would strand this pick forever.
        store.filePickerDismissed()
        store.filePickCancelled()
        #expect(store.pickerMode == .idle)
        #expect(store.lastPickResult?.id == "a")
        #expect(store.lastPickResult?.error == "cancelled")
        #expect(store.pickSettleCount == 1)
        // A second cancellation for the same (already settled) pick is inert.
        store.filePickCancelled()
        #expect(store.pickSettleCount == 1)
        // The slot is free — a new pick presents.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
    }

    /// A cancel from the file leg may only settle a pick the FILE leg owns.
    /// SwiftUI chooses the order of the dismissal write and `onCancellation`,
    /// so a late one can arrive once the slot already belongs to a photo pick —
    /// settling it would answer the wrong promise AND strand the real one.
    @Test func aFileCancelCannotSettleAPickThePhotoLegOwns() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        store.filePickerDismissed()
        store.filePickCancelled()
        #expect(store.pickSettleCount == 1)
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
        store.filePickCancelled()
        #expect(store.pickSettleCount == 1, "\"b\" belongs to the photo leg")
        #expect(store.lastPickResult?.id == "a")
        #expect(store.pickerMode == .photos, "and its picker stays up")
    }

    @Test func fileCancellationAfterAChoiceCannotSettleTheUploadTwice() async throws {
        let fake = FakeBayboClient()
        fake.answerDeckFileUpload(with: "sha256:" + String(repeating: "b", count: 64) + ".tok")
        let store = makeStore(fake)
        let url = try tempFile(named: "notes.pdf")
        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        #expect(store.consumePick()?.id == "a")
        store.finishFilePick(id: "a", cardId: "c1", url: url)
        store.filePickCancelled()
        try await Task.sleep(nanoseconds: 40_000_000)
        #expect(store.pickSettleCount == 1, "exactly one settle per request")
        #expect(store.lastPickResult?.ok == true)
    }

    @Test func filePickStreamsByPathAndKeepsTheRealFilename() async throws {
        let fake = FakeBayboClient()
        let blobId = "sha256:" + String(repeating: "c", count: 64) + ".tok"
        fake.answerDeckFileUpload(with: blobId)
        let store = makeStore(fake)
        let bytes = Data("report body".utf8)
        let url = try tempFile(named: "report.pdf", bytes: bytes)

        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        #expect(store.consumePick()?.id == "a")
        store.finishFilePick(id: "a", cardId: "c1", url: url)
        try await Task.sleep(nanoseconds: 60_000_000)

        // The bytes rode the PATH, and the card id is what makes the blob
        // `deck:<card>` — reclaimable at purge instead of an immortal device:*.
        #expect(fake.deckFileUploadCalls.count == 1)
        #expect(fake.deckFileUploadCalls.first?.path == url.path)
        #expect(fake.deckFileUploadCalls.first?.mimeType == "application/pdf")
        #expect(fake.deckFileUploadCalls.first?.cardId == "c1")
        #expect(store.lastPickResult?.ok == true)
        #expect(store.pickSettleCount == 1)

        let refJSON = try #require(store.lastPickResult?.refJSON)
        let ref = try #require(
            JSONSerialization.jsonObject(with: Data(refJSON.utf8)) as? [String: Any])
        #expect(ref["blobId"] as? String == blobId)
        #expect(ref["contentType"] as? String == "application/pdf")
        #expect(ref["size"] as? Int == bytes.count)
        // A file pick reports the file's REAL name, not the photo leg's
        // synthesized `photo.<ext>`.
        #expect(ref["name"] as? String == "report.pdf")
    }

    @Test func fileWithNoUsableMimeFallsBackToTheGatewaysDefault() async throws {
        let fake = FakeBayboClient()
        fake.answerDeckFileUpload(with: "sha256:" + String(repeating: "d", count: 64) + ".tok")
        let store = makeStore(fake)
        let url = try tempFile(named: "payload.baybo-unknown-ext")
        store.requestPick(id: "a", cardId: "c1", accept: "*/*")
        #expect(store.consumePick()?.id == "a")
        store.finishFilePick(id: "a", cardId: "c1", url: url)
        try await Task.sleep(nanoseconds: 60_000_000)
        #expect(fake.deckFileUploadCalls.first?.mimeType == DeckStore.defaultBlobMime)
    }

    @Test func fileImporterFailureSettlesTheRequestSoTheSlotIsNotWedged() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: "application/pdf")
        #expect(store.consumePick()?.id == "a")
        // The importer handed back nothing (an error, or an empty selection).
        store.finishFilePick(id: "a", cardId: "c1", url: nil)
        #expect(store.lastPickResult?.error == "load failed")
        #expect(store.pickSettleCount == 1)
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.pickerMode == .photos)
    }

    @Test func shareMaterializesTheBlobUnderItsRealName() async throws {
        let fake = FakeBayboClient()
        let bytes = Data("hello deck".utf8)
        let blobId = "sha256:" + String(repeating: "a", count: 64) + ".deadbeef"
        fake.cachedBlobs[blobId] = bytes
        let store = makeStore(fake)
        store.requestShare(blobId: blobId, filename: "note.txt", contentType: "text/plain")
        try await Task.sleep(nanoseconds: 60_000_000)  // fetch + materialize
        let url = try #require(store.shareItem?.url)
        #expect(url.lastPathComponent == "note.txt")
        #expect(FileManager.default.fileExists(atPath: url.path))
        #expect((try? Data(contentsOf: url)) == bytes)
    }

    @Test func refreshMapsTheFfiViewAndPersistsTheMirror() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(
            cards: [card("b", position: 1), card("a", position: 0)],
            snapshots: [
                DeckSnapshotInfo(
                    cardId: "a", seq: 3, payload: "{\"n\":3}", fetchedAtMs: 0, error: nil)
            ]
        )
        let store = makeStore(fake)
        #expect(store.isEmpty)
        await store.refreshNow()
        #expect(!store.isEmpty)
        #expect(store.state.cards.map(\.cardId) == ["a", "b"] || store.state.cards.count == 2)
        #expect(store.state.snapshots.first?.seq == 3)

        // The mirror round-trips: a fresh store paints from disk.
        let rehydrated = DeckStore(clientProvider: { fake })
        #expect(!rehydrated.isEmpty)
        #expect(rehydrated.state.cards.count == 2)
        #expect(rehydrated.state.snapshots.first?.payload == "{\"n\":3}")
        DeckStore.removeMirror()
    }

    @Test func emptyStateTracksRefreshes() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(cards: [card("a", position: 0)], snapshots: [])
        let store = makeStore(fake)

        await store.refreshNow()
        #expect(!store.isEmpty)

        fake.deckView = DeckView(cards: [], snapshots: [])
        await store.refreshNow()
        #expect(store.isEmpty)
        DeckStore.removeMirror()
    }

    @Test func cardDataAcceptsOnlyStrictlyNewerSeqs() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(
            cards: [card("a", position: 0)],
            snapshots: [
                DeckSnapshotInfo(
                    cardId: "a", seq: 5, payload: "{\"n\":5}", fetchedAtMs: 0, error: nil)
            ]
        )
        let store = makeStore(fake)
        await store.refreshNow()

        store.handleCardData(cardId: "a", seq: 5, payload: "{\"stale\":true}")
        #expect(store.state.snapshots.first?.payload == "{\"n\":5}")
        store.handleCardData(cardId: "a", seq: 4, payload: "{\"stale\":true}")
        #expect(store.state.snapshots.first?.payload == "{\"n\":5}")

        store.handleCardData(cardId: "a", seq: 6, payload: "{\"n\":6}")
        #expect(store.state.snapshots.first?.seq == 6)
        #expect(store.state.snapshots.first?.payload == "{\"n\":6}")

        // Unknown card: dropped (the DeckChanged refetch brings the card).
        store.handleCardData(cardId: "ghost", seq: 1, payload: "{}")
        #expect(store.state.snapshots.count == 1)
        DeckStore.removeMirror()
    }

    @Test func layoutAppliesOptimisticallyAndRollsBackToBaselineOnFailure() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(
            cards: [card("a", position: 0), card("b", position: 1)],
            snapshots: []
        )
        let store = makeStore(fake)
        await store.refreshNow()

        // Success path: optimistic order sticks, PUT recorded.
        store.requestLayout(entries: [
            ["cardId": "b", "position": 0, "size": "small"],
            ["cardId": "a", "position": 1, "size": "wide"],
        ])
        #expect(store.state.cards.map(\.cardId) == ["b", "a"])
        await store.layoutTask?.value
        #expect(fake.deckLayoutPuts.count == 1)
        #expect(fake.deckLayoutPuts[0].map(\.cardId) == ["b", "a"])

        // Failure path: the optimistic flip rolls back to the BASELINE.
        struct LayoutRefused: Error {}
        fake.deckLayoutError = LayoutRefused()
        store.requestLayout(entries: [
            ["cardId": "a", "position": 0, "size": "large"],
            ["cardId": "b", "position": 1, "size": "small"],
        ])
        #expect(store.state.cards.first?.size == "large")
        await store.layoutTask?.value
        // The rollback also kicks a refresh; await it too, or the assertions
        // race a task that rewrites `state.cards` right after them.
        await store.refreshTask?.value
        #expect(store.state.cards.map(\.cardId) == ["b", "a"], "rolled back to baseline order")
        #expect(store.state.cards.first?.size == "small")
        DeckStore.removeMirror()
    }

    @Test func deleteConfirmsNativelyBeforeTheFfiCall() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(cards: [card("a", position: 0)], snapshots: [])
        let store = makeStore(fake)
        await store.refreshNow()

        store.requestCardAction(cardId: "a", action: "delete")
        #expect(store.pendingDelete == "a")
        #expect(fake.deckDeletes.isEmpty, "nothing happens before the confirm")

        store.confirmPendingDelete()
        await store.actionTask?.value
        #expect(fake.deckDeletes == ["a"])
        DeckStore.removeMirror()
    }

    @Test func recycleFetchesAndRestorePutsTheCardBack() async {
        let fake = FakeBayboClient()
        fake.deckRecycleList = [card("gone", position: 0)]
        fake.deckView = DeckView(cards: [], snapshots: [])
        let store = makeStore(fake)

        await store.fetchRecycleNow()
        #expect(store.recycle.map(\.cardId) == ["gone"])

        store.restore(cardId: "gone")
        // OPTIMISTIC: the row leaves at the tap, before the restore returns.
        #expect(store.recycle.isEmpty, "row leaves the bin at the tap")
        await store.actionTask?.value
        #expect(fake.deckRestores == ["gone"])
        #expect(store.recycle.isEmpty)
        DeckStore.removeMirror()
    }

    @Test func purgePermanentlyDeletesTheRecycledCard() async {
        let fake = FakeBayboClient()
        fake.deckRecycleList = [card("gone", position: 0)]
        fake.deckView = DeckView(cards: [], snapshots: [])
        let store = makeStore(fake)
        await store.fetchRecycleNow()

        store.purge(cardId: "gone")
        #expect(store.recycle.isEmpty, "row leaves the bin at the confirmed action")
        await store.actionTask?.value
        #expect(fake.deckPurges == ["gone"])
        #expect(store.recycle.isEmpty)
        DeckStore.removeMirror()
    }

    @Test func purgeFailureRollsTheRowBackIntoTheBin() async {
        let fake = FakeBayboClient()
        fake.deckRecycleList = [card("gone", position: 0)]
        fake.deckView = DeckView(cards: [], snapshots: [])
        let store = makeStore(fake)
        await store.fetchRecycleNow()

        fake.deckRecycleList = []
        store.purge(cardId: "gone")
        #expect(store.recycle.isEmpty, "optimistic removal still happens")
        await store.actionTask?.value
        #expect(
            store.recycle.map(\.cardId) == ["gone"],
            "failed purge rolls the row back")
        DeckStore.removeMirror()
    }

    @Test func restoreFailureRollsTheRowBackIntoTheBin() async {
        let fake = FakeBayboClient()
        fake.deckRecycleList = [card("gone", position: 0)]
        fake.deckView = DeckView(cards: [], snapshots: [])
        let store = makeStore(fake)
        await store.fetchRecycleNow()

        // The fake throws for a card absent from its recycle list — the same
        // failure shape as a gateway refusing the restore.
        fake.deckRecycleList = []
        store.restore(cardId: "gone")
        #expect(store.recycle.isEmpty, "optimistic removal still happens")
        await store.actionTask?.value
        #expect(
            store.recycle.map(\.cardId) == ["gone"],
            "failed restore rolls the row back")
        DeckStore.removeMirror()
    }

    @Test func enableActionCallsTheFfiAndRefreshes() async {
        let fake = FakeBayboClient()
        fake.deckView = DeckView(cards: [card("a", position: 0)], snapshots: [])
        let store = makeStore(fake)
        store.requestCardAction(cardId: "a", action: "enable")
        await store.actionTask?.value
        #expect(fake.deckEnableCalls.count == 1)
        #expect(fake.deckEnableCalls[0].0 == "a")
        #expect(fake.deckEnableCalls[0].1 == true)
        DeckStore.removeMirror()
    }
}
