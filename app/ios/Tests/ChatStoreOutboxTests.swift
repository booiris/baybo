import Foundation
import Testing

@testable import Baybo

/// The two-stage send confirmation, driven end to end through `ChatStore` with a
/// fake core: the Echo proves TRANSPORT (`sending` → `sent`), an ordinal-stamped
/// row proves DURABILITY (release).
///
/// The load-bearing test is `reconnectLooksUpEveryInFlightEntryBeforeAnyResend`.
/// A gateway restart evicts the in-memory dedup window, so a blind resend on the
/// reconnect edge would DOUBLE-RUN a send that is already durable — a duplicate
/// agent turn, the worst user-visible failure in the app. The point lookup (from
/// the persisted column, which survives the restart) is the gate, and deleting
/// it is a one-line change that no compiler and no UI test catches.
@Suite @MainActor
struct ChatStoreOutboxTests {
    private static let sessionId = "s-1"
    private nonisolated static let msgId = "msg-1"
    /// Long enough for a detached `Task` to land, short enough that the tests
    /// asserting a call NEVER happens don't stall the suite.
    private static let negativeTimeout: Duration = .milliseconds(250)

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        // A LISTED session: it exists on the gateway, so the store connects
        // rather than sitting as a draft.
        index.touch(sessionId: Self.sessionId)
        outbox = temp.makeOutbox(sessionId: Self.sessionId)
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox,
            supportDirectory: temp.url)
    }

    private func entry() -> OutboxEntry? {
        outbox.entries().first { $0.platformMsgId == Self.msgId }
    }

    private func enrol(_ msgId: String = ChatStoreOutboxTests.msgId) {
        outbox.beginSend(platformMsgId: msgId, text: "hello", attachments: [])
    }

    /// The server's Echo: a user message carrying the idempotency key and NO
    /// ordinal — it is not a durable row yet.
    private func echoFrame() -> String {
        #"{"kind":"message","role":"user","platform_msg_id":"\#(Self.msgId)","content":"hello"}"#
    }

    /// A sync page whose row carries the key AND an ordinal — durability.
    private func syncPage() -> String {
        """
        {"kind":"sync_page","rows":[{"id":"r-1","kind":"message","role":"user",
         "platform_msg_id":"\(Self.msgId)","ordinal":42,"content":"hello"}],
         "since_ordinal":null,"next_cursor":42,"rebased":false,
         "oldest_ordinal":1,"has_more_older":false}
        """
    }

    /// A REBASED page: the floor moved above the entry, so its absence from the
    /// rows proves nothing.
    private func rebasedEmptyPage() -> String {
        """
        {"kind":"sync_page","rows":[],"since_ordinal":null,"next_cursor":null,
         "rebased":true,"oldest_ordinal":99,"has_more_older":true}
        """
    }

    private func connect() async -> Bool {
        store.connect()
        return await waitUntil { store.connState == .connected }
    }

    @Test func sendEnrolsTheOutboxAndReachesTheWire() async {
        store.send(text: "hello", attachments: [])
        #expect(await waitUntil { !client.transmissions.isEmpty })

        let sent = client.transmissions.first
        #expect(client.transmissions.count == 1)
        #expect(sent?.sessionId == Self.sessionId)
        #expect(sent?.text == "hello")
        #expect(client.createdSessionIds == [Self.sessionId])

        // The optimistic bubble's key IS the outbox key IS the wire's msg_id.
        #expect(outbox.entries().count == 1)
        #expect(outbox.entries().first?.platformMsgId == sent?.msgId)
        #expect(outbox.entries().first?.transmissions == 1)
    }

    /// The Echo proves transport only: the entry stops retrying but is NOT
    /// released — a gateway that echoed and then crashed before persisting is
    /// exactly the window the outbox exists for.
    @Test func echoConfirmsTransportButDoesNotRelease() async {
        #expect(await connect())
        enrol()

        client.deliver(echoFrame(), to: Self.sessionId)
        #expect(await waitUntil { entry()?.state == .sent })
        #expect(entry() != nil, "transport is not durability")
    }

    /// An ORDINAL-stamped user frame is a durable row replayed live, not an
    /// echo. The ordinal-less shape is the whole discriminator — matching on the
    /// key alone would confirm transport off a row that has been durable for
    /// hours.
    @Test func anOrdinalStampedUserFrameIsNotAnEcho() async {
        #expect(await connect())
        enrol()

        client.deliver(
            #"{"kind":"message","role":"user","platform_msg_id":"\#(Self.msgId)","ordinal":7}"#,
            to: Self.sessionId)
        _ = await waitUntil(timeout: Self.negativeTimeout) { entry()?.state == .sent }
        #expect(entry()?.state == .sending)
    }

    /// An ordinal-stamped row with the key, seen on a sync page, is DURABILITY —
    /// reconciled natively before the frame ever reaches the webview.
    @Test func syncPageRowReleasesTheEntry() async {
        enrol()
        client.answerSync(with: syncPage())

        store.requestSync(sinceOrdinal: nil, limit: 50)
        #expect(await waitUntil { outbox.entries().isEmpty })
        #expect(client.transmissions.isEmpty)
    }

    @Test func aLoadedSyncPageIsMarkedReadImmediately() async {
        client.answerSync(with: syncPage())

        store.requestSync(sinceOrdinal: nil, limit: 50)

        #expect(await waitUntil { client.readOrdinals == [42] })
        store.markRead(ordinal: 42)
        _ = await waitUntil(timeout: Self.negativeTimeout) { client.readOrdinals.count > 1 }
        #expect(client.readOrdinals == [42], "the transcript callback must be deduplicated")
    }

    /// A rebased page hides the floor, so each unconfirmed entry resolves by
    /// point lookup instead — found → released, no transmission consumed.
    @Test func rebasedSyncResolvesEachEntryByPointLookup() async {
        enrol()
        client.answerSync(with: rebasedEmptyPage())
        client.answerLookup(platformMsgId: Self.msgId, found: true, ordinal: 7)

        store.requestSync(sinceOrdinal: nil, limit: 50)
        #expect(await waitUntil { outbox.entries().isEmpty })
        #expect(client.lookupCalls.map(\.platformMsgId) == [Self.msgId])
        #expect(client.transmissions.isEmpty, "a lookup is not a transmission")
    }

    /// The transcript half of an OFFSCREEN release: with no ready bridge, the
    /// confirm — durable ordinal included — parks in the store queue and the
    /// next mount edge delivers it exactly once. The optional-chained bridge
    /// call this replaces dropped it silently, stranding the webview's
    /// REPLACE-overlay membership on a same-session re-entry.
    @Test func anOffscreenReleaseQueuesTheConfirmForTheNextMountEdge() async {
        enrol()
        client.answerSync(with: syncPage())

        store.requestSync(sinceOrdinal: nil, limit: 50)
        #expect(await waitUntil { outbox.entries().isEmpty })

        let surface = FakeTranscriptSurface()
        store.flushPendingSendConfirms(to: surface)
        #expect(
            surface.confirmed == [
                FakeTranscriptSurface.Confirmed(msgId: Self.msgId, ordinal: 42)
            ])

        // Drained: a second mount edge must not double-deliver.
        store.flushPendingSendConfirms(to: surface)
        #expect(surface.confirmed.count == 1)
    }

    /// The point-lookup release carries the looked-up ordinal. Without it the
    /// released bubble has NO keep predicate — the echo never stamps one, and
    /// this release ships no page — so the next REPLACE whose page lacks the
    /// row would delete the very bubble the lookup just proved durable.
    @Test func aPointLookupReleaseCarriesTheOrdinal() async {
        enrol()
        client.answerSync(with: rebasedEmptyPage())
        client.answerLookup(platformMsgId: Self.msgId, found: true, ordinal: 7)

        store.requestSync(sinceOrdinal: nil, limit: 50)
        #expect(await waitUntil { outbox.entries().isEmpty })

        let surface = FakeTranscriptSurface()
        store.flushPendingSendConfirms(to: surface)
        #expect(
            surface.confirmed == [
                FakeTranscriptSurface.Confirmed(msgId: Self.msgId, ordinal: 7)
            ])
    }

    /// The lookup proved the key absent: the retry machine resumes at once
    /// (`lastSentAt` zeroed) and the lookup consumed no transmission.
    @Test func aProvablyAbsentEntryResumesSending() async {
        enrol()
        client.answerSync(with: rebasedEmptyPage())
        client.answerLookup(platformMsgId: Self.msgId, found: false)

        store.requestSync(sinceOrdinal: nil, limit: 50)
        #expect(await waitUntil { entry()?.state == .sending && entry()?.lastSentAt == 0 })
        #expect(entry()?.transmissions == 1)
    }

    /// THE GATE. Every unconfirmed entry — `sending` AND `sent` — is
    /// point-looked-up BEFORE any resend, because a gateway restart evicts the
    /// dedup window and a blind resend of a durable send double-runs the agent.
    @Test func reconnectLooksUpEveryInFlightEntryBeforeAnyResend() async {
        enrol()
        enrol("msg-2")
        outbox.markEchoed(platformMsgId: "msg-2")
        client.answerLookup(platformMsgId: Self.msgId, found: true, ordinal: 7)
        client.answerLookup(platformMsgId: "msg-2", found: true, ordinal: 8)

        #expect(await connect())
        #expect(
            await waitUntil {
                Set(client.lookupCalls.map(\.platformMsgId)) == [Self.msgId, "msg-2"]
            })
        #expect(client.transmissions.isEmpty, "a durable send must never be resent")
        #expect(await waitUntil { outbox.entries().isEmpty })
    }

    /// The gate's ONE exemption: the send that drove this very dial. Its row is
    /// microseconds old, so the lookup races the gateway's own persist, answers
    /// `found: false`, and `resumeSending` re-arms it for a blind resend at the
    /// next 3 s tick — a second transmission of a message still in flight, well
    /// inside the 10 s the echo needs, spent for nothing.
    @Test func theDialsOwnSendIsExemptFromTheGate() async {
        store.send(text: "hello", attachments: [])
        #expect(await waitUntil { store.connState == .connected })

        _ = await waitUntil(timeout: Self.negativeTimeout) { !client.lookupCalls.isEmpty }
        #expect(client.lookupCalls.isEmpty, "a lookup would race the gateway's own persist")
        #expect(
            outbox.entries().first?.lastSentAt != 0,
            "and its `found: false` would re-arm the entry for an immediate resend")
        #expect(outbox.entries().first?.transmissions == 1)
    }

    /// Only that one entry is exempt. An entry from an EARLIER leg rode a
    /// connection that is gone; it is exactly what the gate exists for, and the
    /// dial it happens to share with a fresh send must still resolve it.
    @Test func anEarlierEntryIsStillGatedOnTheSameDial() async {
        enrol("msg-old")
        client.answerLookup(platformMsgId: "msg-old", found: true, ordinal: 5)

        store.send(text: "hello", attachments: [])
        #expect(await waitUntil { client.lookupCalls.map(\.platformMsgId) == ["msg-old"] })
        #expect(await waitUntil { !outbox.entries().contains { $0.platformMsgId == "msg-old" } })
    }

    /// A `failed` entry is the user's red dot; the reconnect gate leaves it
    /// alone — only a manual retry revives it.
    @Test func reconnectIgnoresAFailedEntry() async {
        enrol()
        outbox.markFailed(platformMsgId: Self.msgId)

        #expect(await connect())
        _ = await waitUntil(timeout: Self.negativeTimeout) { !client.lookupCalls.isEmpty }
        #expect(client.lookupCalls.isEmpty)
        #expect(entry()?.state == .failed)
    }

    /// A lookup that ERRORS defers to the next reconnect. It must never fall
    /// back to a blind resend: the entry's durability is still unknown, and
    /// that is precisely when a resend is unsafe.
    @Test func aLookupFailureNeverFallsBackToABlindResend() async {
        enrol()
        client.failLookup(with: BayboError.Other(message: "gateway unreachable"))

        #expect(await connect())
        #expect(await waitUntil { !client.lookupCalls.isEmpty })
        _ = await waitUntil(timeout: Self.negativeTimeout) { !client.transmissions.isEmpty }
        #expect(client.transmissions.isEmpty)
        #expect(entry()?.state == .unknown)
    }

    /// The manual retry (the red dot) reuses the ORIGINAL key, so a resend that
    /// races a late first delivery still lands as one row — and it resets the
    /// automatic-transmission budget.
    @Test func manualRetryReusesTheKeyAndResetsTheBudget() async {
        #expect(await connect())
        store.retrySend(msgId: Self.msgId, text: "hello", attachments: [])

        #expect(await waitUntil { !client.sentMessages.isEmpty })
        #expect(client.sentMessages.map(\.msgId) == [Self.msgId])
        #expect(entry()?.state == .sending)
        #expect(entry()?.transmissions == 0)
    }

    /// A send that never reaches the wire keeps its entry enrolled (the retry
    /// machine owns it from here) and drops the leg to `offline` — a failed dial
    /// is the one and only trigger for that state.
    @Test func aFailedDialLeavesTheEntryEnrolledAndTheLegOffline() async {
        client.failSend(with: BayboError.Other(message: "no route to host"))
        store.send(text: "hello", attachments: [])

        #expect(await waitUntil { store.connState == .offline })
        #expect(client.transmissions.isEmpty)
        #expect(outbox.entries().first?.state == .sending)
    }

    /// The list's second line follows the conversation: the user's send lands
    /// immediately, and the turn's terminal assistant message replaces it in
    /// place — rather than jumping from question to answer on the next
    /// return-to-list refresh.
    @Test func theChatListPreviewFollowsBothSides() async {
        store.send(text: "what is the answer", attachments: [])
        #expect(await waitUntil { index.rows.first?.preview == "what is the answer" })

        client.deliver(
            #"{"kind":"message","role":"assistant","content":"the answer is 42"}"#,
            to: Self.sessionId)
        #expect(await waitUntil { index.rows.first?.preview == "the answer is 42" })
        #expect(
            index.rows.first?.userText == "what is the answer",
            "the bold-line fallback stays the user's own last message")
    }
}
