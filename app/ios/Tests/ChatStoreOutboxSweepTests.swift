import Foundation
import Testing

@testable import Baybo

/// The retry sweep's leg gate.
///
/// A transmission is only ever spent on the wire, so the sweep must not elect an
/// entry it cannot actually send. It used to: `sweepOutbox` asked only "is a
/// blind resend due?", and `resendOutboxEntry` bailed on its own `connected`
/// guard AFTER that — BEFORE `recordTransmission`. So on a down leg
/// `transmissions` stuck at 1 forever, `dueForBlindResend` stayed true,
/// `resendExhausted` was never even evaluated, the entry could never reach
/// `failed` (no red dot, ever), and the 3 s timer ran for the life of the store.
///
/// The clock is injected, so the 10 s echo window is crossed by hand; the sweep
/// is stepped directly rather than slept through.
@Suite @MainActor
struct ChatStoreOutboxSweepTests {
    private static let sessionId = "s-1"
    private static let msgId = "msg-1"

    private let temp: TempSupportDir
    private let clock = TestClock()
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        index.touch(sessionId: Self.sessionId)
        let clock = self.clock
        outbox = temp.makeOutbox(sessionId: Self.sessionId, now: { clock.now })
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox)
    }

    private func entry() -> OutboxEntry? {
        outbox.entries().first { $0.platformMsgId == Self.msgId }
    }

    /// Enrol a send and let its echo window lapse, so a blind resend is due.
    private func enrolAndLapse() {
        outbox.beginSend(platformMsgId: Self.msgId, text: "hello", attachments: [])
        clock.advance(OutboxStore.echoTimeout + 1)
    }

    private func connect() async -> Bool {
        store.connect()
        return await waitUntil { store.connState == .connected }
    }

    /// A down leg parks the sweep. The entry keeps its budget — it never reached
    /// the wire, so nothing was spent — and stays `sending` for the reconnect to
    /// resolve. The outbox exists precisely to outlive the outage; failing a
    /// message because the network is down would be a lie.
    @Test func aDownLegParksTheSweepAndSpendsNothing() async {
        client.failConnect(with: BayboError.Other(message: "no route to host"))
        store.connect()
        #expect(await waitUntil { store.connState == .offline })
        enrolAndLapse()

        #expect(store.sweepOutbox(), "the sweep stops rather than tick on a guard that can't pass")
        #expect(entry()?.state == .sending)
        #expect(entry()?.transmissions == 1, "a transmission is only ever spent on the wire")
        #expect(client.transmissions.isEmpty)
    }

    /// The same entry on a LIVE leg is the case the sweep exists for: one blind
    /// resend under the same key, and the budget actually moves.
    @Test func aLiveLegResendsAndCountsIt() async {
        #expect(await connect())
        enrolAndLapse()

        #expect(!store.sweepOutbox(), "the outbox is still in flight — keep ticking")
        #expect(await waitUntil { client.transmissions.map(\.msgId) == [Self.msgId] })
        #expect(entry()?.transmissions == 2)
        #expect(entry()?.state == .sending)
    }

    /// And because the budget moves, it can be exhausted — the red dot the user
    /// needs in order to retry by hand. This is what a down leg could never
    /// reach.
    @Test func anExhaustedEntryFailsOnALiveLeg() async {
        #expect(await connect())
        enrolAndLapse()

        for _ in 1..<OutboxStore.maxAutoTransmissions {
            store.sweepOutbox()
            clock.advance(OutboxStore.echoTimeout + 1)
        }
        #expect(entry()?.transmissions == OutboxStore.maxAutoTransmissions)
        #expect(
            await waitUntil {
                client.transmissions.count == OutboxStore.maxAutoTransmissions - 1
            },
            "the initial send was enrolled by hand here — only the retries hit this fake")

        store.sweepOutbox()
        #expect(entry()?.state == .failed)
    }

    /// The echo stops the retries dead: transport is proven, and a resend would
    /// be a duplicate the gateway only dedups inside its live window.
    @Test func anEchoedEntryIsNeverResent() async {
        #expect(await connect())
        enrolAndLapse()
        outbox.markEchoed(platformMsgId: Self.msgId)

        #expect(!store.sweepOutbox(), "still unreleased — durability has not been proven")
        #expect(client.transmissions.isEmpty)
        #expect(entry()?.state == .sent)
    }

    /// An empty outbox stops the timer — the sweep is not a background heartbeat.
    @Test func anIdleOutboxStopsTheSweep() async {
        #expect(await connect())
        #expect(store.sweepOutbox())
    }
}
