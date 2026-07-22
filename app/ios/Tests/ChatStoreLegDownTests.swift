import Foundation
import Testing

@testable import Baybo

/// The sustained-disconnect signal behind the header's offline icon. Raw
/// `connState` oscillates `connecting ↔ offline` through the retry loop on a
/// real outage (offline is only the gap between failed dials), so the icon
/// keys off this debounced signal instead — these pin its three edges: raise
/// after the delay, clear on reconnect, and never fire on a healthy dial's
/// transient `connecting`.
@Suite @MainActor
struct ChatStoreLegDownTests {
    private static let sessionId = "s-leg"

    private let temp = TempSupportDir()
    private let client = FakeBayboClient()

    private func makeStore() -> ChatStore {
        let index = temp.makeIndex()
        index.touch(sessionId: Self.sessionId)
        let store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index,
            outbox: temp.makeOutbox(sessionId: Self.sessionId))
        store.legDownDelay = .milliseconds(30)
        return store
    }

    @Test func aSustainedOutageRaisesTheSignal() async {
        client.failConnect(with: BayboError.Other(message: "down"))
        let store = makeStore()

        store.connect()

        #expect(await waitUntil { store.legDown })
    }

    @Test func reconnectingClearsIt() async {
        client.failConnect(with: BayboError.Other(message: "down"))
        let store = makeStore()
        store.connect()
        #expect(await waitUntil { store.legDown })

        client.succeedConnect()
        store.connect()

        #expect(await waitUntil { !store.legDown })
        #expect(store.connState == .connected)
    }

    /// Every healthy open passes through `.connecting` — the debounce exists
    /// so that transient never flashes the icon.
    @Test func aHealthyDialNeverRaisesIt() async {
        let store = makeStore()

        store.connect()
        #expect(await waitUntil { store.connState == .connected })

        try? await Task.sleep(for: .milliseconds(80))
        #expect(!store.legDown)
    }

    /// A draft has no leg to be down — composing offline must not badge.
    @Test func aDraftNeverRaisesIt() async {
        client.failConnect(with: BayboError.Other(message: "down"))
        let index = temp.makeIndex()
        let store = ChatStore(
            sessionId: "s-draft", client: client, index: index,
            outbox: temp.makeOutbox(sessionId: "s-draft"))
        store.legDownDelay = .milliseconds(30)

        store.connectIfNeeded()

        try? await Task.sleep(for: .milliseconds(80))
        #expect(!store.legDown)
        #expect(store.connState == .draft)
    }
}
