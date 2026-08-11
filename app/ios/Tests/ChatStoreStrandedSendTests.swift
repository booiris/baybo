import Foundation
import Testing

@testable import Baybo

/// Sends that outlive the process.
///
/// `ChatStore.send` enrols the outbox BEFORE it touches the network — that
/// ordering is what makes a kill non-lossy. But it also means a kill in the
/// window between the outbox write and `POST /v1/chat/sessions` leaves entries
/// for a session the gateway was never told about and `SessionIndex` never
/// listed. Nothing drained them: `connect()` is a no-op on a draft, the sweep
/// only fires on a connected leg, and compose mints a FRESH id — so the
/// conversation was unreachable from the list and the message was lost on disk
/// forever, in a file nothing would ever delete either.
///
/// `resumePersistedSends` adopts them; `AppStore.resumeStrandedSends` finds them
/// (every session id with an outbox file) on launch and on every foreground.
@Suite @MainActor
struct ChatStoreStrandedSendTests {
    private static let sessionId = "s-stranded"
    private static let msgId = "msg-1"

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        outbox = temp.makeOutbox(sessionId: Self.sessionId)
        // The previous process got exactly this far: the outbox is on disk, the
        // gateway knows nothing, and the registry has no row — so the store comes
        // up a DRAFT.
        outbox.beginSend(platformMsgId: Self.msgId, text: "hello", attachments: [])
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox,
            supportDirectory: temp.url)
    }

    /// The directory scan is how a session with no registry row is found at all.
    @Test func theOutboxFileIsHowAStrandedSessionIsFound() {
        #expect(!index.contains(sessionId: Self.sessionId))
        #expect(OutboxStore.pendingSessionIds(in: temp.url) == [Self.sessionId])
    }

    /// Adoption: create the row the kill never created, list the conversation so
    /// the user can see it, and dial — which is what finally lets the outbox
    /// drain at all.
    @Test func aDraftWithAPersistedOutboxIsAdopted() async {
        #expect(store.connState == .draft)

        store.resumePersistedSends()

        #expect(await waitUntil { store.connState == .connected })
        #expect(client.createdSessionIds == [Self.sessionId])
        #expect(index.contains(sessionId: Self.sessionId), "and the list can reach it")
        #expect(index.rows.first?.preview == "hello")
    }

    /// The recovery goes through the SAME reconciliation gate as any reconnect: a
    /// send that did land — the kill simply happened before we saw its echo — is
    /// released, not re-run. A blind relaunch resend would double-run the agent
    /// turn, and after a gateway restart the dedup window can't save us.
    @Test func aStrandedSendThatDidLandIsReleasedNotReRun() async {
        client.answerLookup(platformMsgId: Self.msgId, found: true, ordinal: 3)

        store.resumePersistedSends()

        #expect(await waitUntil { outbox.entries().isEmpty })
        #expect(client.transmissions.isEmpty, "a durable send must never be re-run")
    }

    /// Provably absent → redelivered under the ORIGINAL key, so a delivery that
    /// races this one still lands as a single row.
    @Test func aStrandedSendThatNeverLandedIsRedelivered() async {
        client.answerLookup(platformMsgId: Self.msgId, found: false)

        store.resumePersistedSends()

        #expect(await waitUntil { outbox.entries().first?.lastSentAt == 0 })
        store.sweepOutbox()
        #expect(await waitUntil { client.transmissions.map(\.msgId) == [Self.msgId] })
    }

    /// A failed create leaves the store a draft — and, crucially, leaves the
    /// outbox on disk: the next launch or foreground tries again rather than
    /// dropping the message.
    @Test func aFailedRecoveryKeepsTheMessageForTheNextAttempt() async {
        client.failCreateSession(with: BayboError.Other(message: "gateway unreachable"))

        store.resumePersistedSends()

        #expect(await waitUntil { client.createdSessionIds == [Self.sessionId] })
        #expect(store.connState == .draft)
        #expect(outbox.entries().count == 1)
        #expect(!index.contains(sessionId: Self.sessionId), "no ghost row for a session that isn't")
    }
}

/// The recovery must not fire on a conversation that has nothing pending — it
/// dials, and dialling every session on the device at launch is not the deal.
@Suite @MainActor
struct ChatStoreQuietOutboxTests {
    @Test func anEmptyOutboxResumesNothing() async {
        let temp = TempSupportDir()
        let client = FakeBayboClient()
        let index = temp.makeIndex()
        let store = ChatStore(
            sessionId: "s-quiet", client: client, index: index,
            outbox: temp.makeOutbox(sessionId: "s-quiet"),
            supportDirectory: temp.url)

        store.resumePersistedSends()

        _ = await waitUntil(timeout: .milliseconds(250)) { !client.createdSessionIds.isEmpty }
        #expect(client.createdSessionIds.isEmpty)
        #expect(client.connectedSessions.isEmpty)
        #expect(store.connState == .draft)
    }

    /// A `failed` entry is the red dot the user owns — the relaunch recovery must
    /// not silently re-send behind their back.
    @Test func aFailedEntryIsNotResumed() async {
        let temp = TempSupportDir()
        let client = FakeBayboClient()
        let index = temp.makeIndex()
        let outbox = temp.makeOutbox(sessionId: "s-dead")
        outbox.beginSend(platformMsgId: "msg-1", text: "hello", attachments: [])
        outbox.markFailed(platformMsgId: "msg-1")
        let store = ChatStore(
            sessionId: "s-dead", client: client, index: index, outbox: outbox,
            supportDirectory: temp.url)

        store.resumePersistedSends()

        _ = await waitUntil(timeout: .milliseconds(250)) { !client.createdSessionIds.isEmpty }
        #expect(client.createdSessionIds.isEmpty)
        #expect(client.transmissions.isEmpty)
    }
}

/// A brand-new chat (a DRAFT) whose FIRST send happens while offline. The send
/// can't create the remote session, so `dispatchSend` fails at
/// `ensureRemoteSession` before it ever dials — and a draft has no reconnect
/// loop to re-drive it (the sweep needs a live leg, `connect` no-ops on a
/// draft). The message must NOT be flagged failed: it is queued, not lost. It
/// stays a `sending` spinner, and the in-session recovery adopts it once the
/// network returns, without waiting for a background→foreground cycle.
@Suite @MainActor
struct ChatStoreDraftRecoveryTests {
    private static let sessionId = "s-draft"

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        outbox = temp.makeOutbox(sessionId: Self.sessionId)
        // No registry row and no gateway session — the store comes up a DRAFT.
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox,
            supportDirectory: temp.url)
    }

    /// `send` mints its own idempotency key, so the sole entry is read
    /// positionally rather than by a fixed id.
    private func entry() -> OutboxEntry? {
        outbox.entries().first
    }

    /// The core of the fix: an offline first-send leaves the entry `sending` (a
    /// spinner, never the red `failed` dot the old catch flashed) and the store a
    /// draft. Nothing reached the wire, and nothing was dropped.
    @Test func anOfflineDraftSendStaysQueuedNotFailed() async {
        client.failCreateSession(with: BayboError.Other(message: "offline"))
        store.send(text: "hello", attachments: [])

        #expect(await waitUntil { client.createdSessionIds == [Self.sessionId] })
        #expect(store.connState == .draft)
        #expect(entry()?.state == .sending)
        #expect(client.transmissions.isEmpty)
    }

    /// The regression this guards: with the network back, the in-session recovery
    /// — not a foreground cycle, not a manual retry — adopts the stranded draft,
    /// creating the session, listing the conversation, and dialling so the queued
    /// message finally has a live leg to drain on. `stepSendRecovery` is exactly
    /// what the 2 s recovery loop runs on its next tick.
    @Test func theInSessionRecoveryDrainsTheDraftWhenTheNetworkReturns() async {
        client.failCreateSession(with: BayboError.Other(message: "offline"))
        store.send(text: "hello", attachments: [])
        #expect(await waitUntil { store.connState == .draft && entry()?.state == .sending })

        client.succeedCreateSession()
        store.stepSendRecovery()

        #expect(await waitUntil { store.connState == .connected })
        #expect(index.contains(sessionId: Self.sessionId), "and the list can now reach it")
        #expect(index.rows.first?.preview == "hello")
    }

    /// The loop stands down the moment there is nothing left to recover, rather
    /// than dialling a draft forever — the quiet-outbox guarantee, but for the
    /// send-driven recovery path.
    @Test func recoveryStopsOnceTheOutboxIsEmpty() async {
        #expect(store.stepSendRecovery(), "an empty draft has nothing to recover")
        #expect(client.createdSessionIds.isEmpty)
        #expect(store.connState == .draft)
    }
}
