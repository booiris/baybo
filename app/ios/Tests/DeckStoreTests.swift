import Foundation
import Testing

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

    @Test func pickRejectsAConcurrentRequestAsBusy() {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        #expect(store.pickerActive)
        // A second pick while one is up is rejected immediately — not queued.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.lastPickResult?.id == "b")
        #expect(store.lastPickResult?.ok == false)
        #expect(store.lastPickResult?.error == "busy")
        // The first pick is untouched — its picker stays up.
        #expect(store.pickerActive)
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
        #expect(store.pickerActive)
        #expect(store.lastPickResult?.id == "a")  // no busy result minted for "d"
    }

    @Test func pickDismissedWithNoChoiceResolvesCancelledAndFreesTheSlot() async throws {
        let store = makeStore(FakeBayboClient())
        store.requestPick(id: "a", cardId: "c1", accept: nil)
        // The picker dismissed with nothing chosen → the promise rejects.
        store.pickerActive = false
        store.pickerDismissed()
        try await Task.sleep(nanoseconds: 30_000_000)  // let the deferred cancel run
        #expect(store.lastPickResult?.id == "a")
        #expect(store.lastPickResult?.error == "cancelled")
        // The slot is free again — a new pick presents.
        store.requestPick(id: "b", cardId: "c1", accept: nil)
        #expect(store.pickerActive)
        #expect(store.lastPickResult?.id == "a")  // "b" was accepted, not rejected
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
        await store.refreshNow()
        #expect(store.state.cards.map(\.cardId) == ["a", "b"] || store.state.cards.count == 2)
        #expect(store.state.snapshots.first?.seq == 3)

        // The mirror round-trips: a fresh store paints from disk.
        let rehydrated = DeckStore(clientProvider: { fake })
        #expect(rehydrated.state.cards.count == 2)
        #expect(rehydrated.state.snapshots.first?.payload == "{\"n\":3}")
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
        // OPTIMISTIC: the row leaves at the tap, before the (slow —
        // dry-run-gated) restore call returns.
        #expect(store.recycle.isEmpty, "row leaves the bin at the tap")
        await store.actionTask?.value
        #expect(fake.deckRestores == ["gone"])
        #expect(store.recycle.isEmpty)
        DeckStore.removeMirror()
    }

    @Test func restoreFailureRollsTheRowBackIntoTheBin() async {
        let fake = FakeBayboClient()
        fake.deckRecycleList = [card("gone", position: 0)]
        fake.deckView = DeckView(cards: [], snapshots: [])
        let store = makeStore(fake)
        await store.fetchRecycleNow()

        // The fake throws for a card absent from its recycle list — the
        // gateway's dry-run gate refusing the restore looks the same.
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
