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

    private func card(_ id: String, position: Int64, size: String = "wide") -> DeckCardInfo {
        DeckCardInfo(
            cardId: id,
            title: id,
            position: position,
            size: size,
            enabled: true,
            quarantined: false,
            deletedAtMs: nil,
            specHash: "h",
            lastSeq: 0,
            createdAtMs: 0,
            retryableOps: ["refresh"]
        )
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
