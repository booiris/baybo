import Foundation
import Testing

@testable import Baybo

/// The client half of the 2026-08-16 cold-start send black hole. `.connected`
/// has exactly one exit (the FFI's `on_disconnected`), so the store must (a)
/// treat the registry's stale-leg verdict on a send as a redial, not a
/// deferral, and (b) never let a dial's stale success overwrite a death report
/// that landed while the success was still unwinding — the overwrite erased
/// the armed reconnect and wedged the store until app relaunch.
@Suite @MainActor
struct ChatStoreStaleLegTests {
    private static let sessionId = "s-stale"

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        index.touch(sessionId: Self.sessionId)
        outbox = temp.makeOutbox(sessionId: Self.sessionId)
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox,
            supportDirectory: temp.url)
    }

    private func connect() async -> Bool {
        store.connect()
        return await waitUntil { store.connState == .connected }
    }

    /// `chatSend` throwing `.NotConnected` on a believed-connected store is the
    /// registry saying the leg no longer carries this session (dead pump, or a
    /// fresh leg the session never re-subscribed on). The same dispatch must
    /// fall through to the dial-and-send path — which re-subscribes and carries
    /// this very message — instead of parking the send for a sweep that would
    /// blind-resend into the same verdict.
    @Test func aStaleConnectedSendRedialsAndCarriesTheMessage() async {
        #expect(await connect())
        client.failPlainSend(with: BayboError.NotConnected)

        store.send(text: "hello", attachments: [])

        #expect(await waitUntil { client.sentAfterConnect.count == 1 })
        #expect(client.sentAfterConnect.first?.text == "hello")
        #expect(await waitUntil { store.connState == .connected })
        #expect(outbox.entries().allSatisfy { $0.state != .failed })
    }

    /// A `/stop` dying on a stale leg must still exit `.connected` — the stop
    /// button is useless until a redial, and `.connected` blocks every redial.
    @Test func aStopOnAStaleLegExitsConnectedAndRedials() async {
        #expect(await connect())
        client.failPlainSend(with: BayboError.NotConnected)

        store.stopAgent()

        #expect(await waitUntil { store.connState != .connected })
        // The armed retry heals once the leg answers again.
        client.succeedConnect()
        #expect(await waitUntil(timeout: .seconds(4)) { store.connState == .connected })
    }

    /// The overwrite race: the leg dies milliseconds after the subscribe ack,
    /// so `on_disconnected` (`.connecting` + armed retry) lands BEFORE the
    /// dial's success finishes unwinding. The success must yield — claiming
    /// `.connected` would erase the heal, and with the callback already
    /// consumed nothing would ever exit `.connected` again.
    @Test func aDeathReportOutlivesTheDialsStaleSuccess() async {
        client.stallConnect(ms: 400)
        store.connect()
        #expect(await waitUntil { client.connectedSessions.count == 1 })

        client.dropLeg(of: Self.sessionId)
        // One main-queue hop lands the death report well inside the 400ms
        // stall — the ordering the test exists to pin.
        try? await Task.sleep(for: .milliseconds(50))

        // Let the stalled dial's success land; it must not claim `.connected`.
        try? await Task.sleep(for: .milliseconds(600))
        #expect(store.connState == .connecting)

        // The retry the death report armed redials and heals for real.
        #expect(await waitUntil(timeout: .seconds(4)) { store.connState == .connected })
        #expect(client.connectedSessions.count >= 2)
    }

    /// The same fence on the send slow path: `chatSendAfterConnect`'s success
    /// continuation must also yield to a death report that landed while it was
    /// in flight, and the armed retry must heal and redeliver the message.
    @Test func aDeathReportOutlivesTheSendSlowPathsStaleSuccess() async {
        client.stallSendAfterConnect(ms: 400)

        // Not connected, so the send takes the dial-and-send slow path.
        store.send(text: "hello", attachments: [])
        #expect(await waitUntil { client.sentAfterConnect.count == 1 })

        client.dropLeg(of: Self.sessionId)
        try? await Task.sleep(for: .milliseconds(50))

        try? await Task.sleep(for: .milliseconds(600))
        #expect(store.connState == .connecting)

        // The armed retry dials for real; its reconcile owns redelivery of the
        // still-unconfirmed entry.
        #expect(await waitUntil(timeout: .seconds(4)) { store.connState == .connected })
        #expect(outbox.entries().allSatisfy { $0.state != .failed })
    }
}
