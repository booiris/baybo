import Foundation
import Testing

@testable import Baybo

/// The two ways the shared transcript webview used to strand a conversation
/// blank: a crash-reload cap that never re-armed, and a reconnect claimed while
/// no page was attached, which a same-session re-entry (no remount, no `init`)
/// never heard about.
@Suite @MainActor
struct TranscriptRecoveryTests {
    private static let sessionId = "s-recovery"
    private static let connEpochCall = "window.baybo.setConnEpoch(1)"

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        let index = temp.makeIndex()
        index.touch(sessionId: Self.sessionId)
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index,
            outbox: temp.makeOutbox(sessionId: Self.sessionId),
            supportDirectory: temp.url)
    }

    private func delivered(_ bridge: TranscriptBridge, _ call: String) -> Bool {
        bridge.pending.contains { $0.contains(call) }
    }

    /// Past the cap the page parks: with no live document, calls are dropped
    /// rather than queued for a `ready` that is never coming.
    @Test func aParkedPageDropsCallsUntilRevived() {
        let bridge = TranscriptBridge(store: store)
        for _ in 0...CrashReloadBudget.maxConsecutiveDeaths {
            bridge.contentProcessDied()
        }

        bridge.setConnEpoch(7)
        #expect(!delivered(bridge, "setConnEpoch(7)"))

        bridge.reviveIfParked()
        bridge.setConnEpoch(8)
        #expect(delivered(bridge, "setConnEpoch(8)"))
    }

    /// Opening a conversation is a revive edge.
    @Test func retargetingRevivesAParkedPage() {
        let bridge = TranscriptBridge(store: store)
        for _ in 0...CrashReloadBudget.maxConsecutiveDeaths {
            bridge.contentProcessDied()
        }

        bridge.retarget(to: store)
        bridge.setConnEpoch(9)

        #expect(delivered(bridge, "setConnEpoch(9)"))
    }

    @Test func aReconnectWhileDetachedReachesTheNextAttach() async {
        let bridge = TranscriptBridge(store: store)
        store.detachBridge(bridge)

        store.connect()
        #expect(await waitUntil { store.connState == .connected })
        #expect(!delivered(bridge, Self.connEpochCall))

        store.attachBridge(bridge)
        #expect(delivered(bridge, Self.connEpochCall))
    }

    @Test func aReconnectWhileAttachedIsNotRedeliveredOnReattach() async {
        let bridge = TranscriptBridge(store: store)
        store.connect()
        #expect(await waitUntil { store.connState == .connected })
        let calls = bridge.pending.filter { $0.contains(Self.connEpochCall) }.count
        #expect(calls == 1)

        store.detachBridge(bridge)
        store.attachBridge(bridge)

        #expect(bridge.pending.filter { $0.contains(Self.connEpochCall) }.count == calls)
    }

    /// The position crosses from one page to its replacement as JSON. Only an
    /// object may reach the call, re-encoded — never the received text spliced
    /// into script verbatim.
    @Test func aReadingPositionIsForwardedOnlyAsAReEncodedObject() {
        let bridge = TranscriptBridge(store: store)

        bridge.restoreReadingPosition(#"{"rowId":"m7","ordinal":7,"offset":-12}"#)
        bridge.restoreReadingPosition(#"1);alert(1);("#)
        bridge.restoreReadingPosition(#"["m7"]"#)

        let calls = bridge.pending.filter { $0.contains("restoreReadingPosition") }
        #expect(calls.count == 1)
        #expect(calls.first?.contains(#""rowId":"m7""#) == true)
    }

    @Test func aPageThatIsNotLiveHasNoPositionToGive() async {
        let bridge = TranscriptBridge(store: store)
        let position = await withCheckedContinuation { done in
            bridge.captureReadingPosition { done.resume(returning: $0) }
        }
        #expect(position == nil)
    }
}
