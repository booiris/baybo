import Foundation
import Testing

@testable import Baybo

/// The persisted send outbox's retry machine, driven through the injected clock
/// so the 10 s echo window is exercised without sleeping through it.
///
/// The two comparisons below are the whole feature: flip `<` to `<=` in
/// `dueForBlindResend` and a message transmits a fourth time (a retry storm past
/// the cap); flip `>=` to `>` in `resendExhausted` and an exhausted entry never
/// flips to `failed`, so the user never gets the red dot that is their only way
/// to retry.
@Suite @MainActor
struct OutboxStoreTests {
    private static let msgId = "msg-1"

    private let temp: TempSupportDir
    private let clock: TestClock
    private let outbox: OutboxStore

    init() {
        let temp = TempSupportDir()
        let clock = TestClock()
        self.temp = temp
        self.clock = clock
        outbox = temp.makeOutbox(sessionId: "s-1", now: { clock.now })
    }

    private func stored() -> OutboxEntry? {
        outbox.entries().first { $0.platformMsgId == Self.msgId }
    }

    private func send() {
        outbox.beginSend(platformMsgId: Self.msgId, text: "hello", attachments: [])
    }

    @Test func beginSendEnrolsOneCountedTransmission() {
        send()
        #expect(stored()?.state == .sending)
        #expect(stored()?.transmissions == 1)
    }

    /// The blind resend is due only once the echo window has LAPSED — one tick
    /// early and every send doubles under a slow gateway.
    @Test func blindResendIsNotDueBeforeTheEchoWindowLapses() throws {
        send()
        clock.advance(OutboxStore.echoTimeout - 0.001)
        let entry = try #require(stored())
        #expect(outbox.dueForBlindResend(entry) == false)
        #expect(outbox.resendExhausted(entry) == false)
    }

    @Test func blindResendIsDueOnceTheEchoWindowLapses() throws {
        send()
        clock.advance(OutboxStore.echoTimeout)
        let entry = try #require(stored())
        #expect(outbox.dueForBlindResend(entry))
        #expect(outbox.resendExhausted(entry) == false)
    }

    /// At the cap the entry is no longer resendable and IS exhausted — the two
    /// predicates hand over to each other exactly here.
    @Test func theTransmissionCapHandsOverFromResendToFailure() throws {
        send()
        for _ in 1..<OutboxStore.maxAutoTransmissions {
            clock.advance(OutboxStore.echoTimeout)
            outbox.recordTransmission(platformMsgId: Self.msgId)
        }
        #expect(stored()?.transmissions == OutboxStore.maxAutoTransmissions)

        let armed = try #require(stored())
        // The last transmission re-armed the window: neither predicate fires yet.
        #expect(outbox.dueForBlindResend(armed) == false)
        #expect(outbox.resendExhausted(armed) == false)

        clock.advance(OutboxStore.echoTimeout)
        let lapsed = try #require(stored())
        #expect(outbox.dueForBlindResend(lapsed) == false, "the cap is spent")
        #expect(outbox.resendExhausted(lapsed))
    }

    /// The echo proves TRANSPORT, so in-connection retries stop — but the entry
    /// stays until durability confirms.
    @Test func echoStopsTheRetryMachineButKeepsTheEntry() throws {
        send()
        outbox.markEchoed(platformMsgId: Self.msgId)
        clock.advance(OutboxStore.echoTimeout * 10)
        #expect(stored()?.state == .sent)
        let entry = try #require(stored())
        #expect(outbox.dueForBlindResend(entry) == false)
        #expect(outbox.resendExhausted(entry) == false)
    }

    /// A late echo for an entry the sweep already failed must not un-fail it —
    /// the red dot is the user's affordance and it just appeared.
    @Test func echoDoesNotRescueAFailedEntry() {
        send()
        outbox.markFailed(platformMsgId: Self.msgId)
        outbox.markEchoed(platformMsgId: Self.msgId)
        #expect(stored()?.state == .failed)
    }

    @Test func durabilityReleasesTheEntry() {
        send()
        outbox.confirmDurable(platformMsgId: Self.msgId)
        #expect(outbox.entries().isEmpty)
    }

    /// A rebased page hides the floor: the entry parks `unknown` (the retry
    /// machine is off) until the point lookup answers.
    @Test func unknownParksTheEntryOutOfTheRetryMachine() throws {
        send()
        outbox.markUnknown(platformMsgId: Self.msgId)
        clock.advance(OutboxStore.echoTimeout * 10)
        let entry = try #require(stored())
        #expect(entry.state == .unknown)
        #expect(outbox.dueForBlindResend(entry) == false)
        #expect(outbox.resendExhausted(entry) == false)
    }

    /// The lookup proved the key absent: retry resumes IMMEDIATELY (the lookup
    /// itself consumed no transmission, so `lastSentAt` is zeroed).
    @Test func resumeSendingMakesTheEntryDueAtOnce() throws {
        send()
        outbox.markUnknown(platformMsgId: Self.msgId)
        outbox.resumeSending(platformMsgId: Self.msgId)
        let entry = try #require(stored())
        #expect(entry.state == .sending)
        #expect(outbox.dueForBlindResend(entry))
        #expect(entry.transmissions == 1, "the lookup is not a transmission")
    }

    @Test func manualRetryResetsTheAutomaticBudget() throws {
        send()
        clock.advance(OutboxStore.echoTimeout)
        outbox.recordTransmission(platformMsgId: Self.msgId)
        outbox.markFailed(platformMsgId: Self.msgId)

        outbox.resetForManualRetry(platformMsgId: Self.msgId, text: "hello", attachments: [])
        let entry = try #require(stored())
        #expect(entry.state == .sending)
        #expect(entry.transmissions == 0)
        #expect(outbox.dueForBlindResend(entry))
    }

    /// The queued-lifetime cap is the down-leg failure trigger: a still-`sending`
    /// entry is "too long" only once it outlives `queuedTimeout` — one tick early
    /// and a brief outage would fail a message reconnect could still deliver.
    @Test func queuedTooLongFiresOnlyPastTheCap() throws {
        send()
        clock.advance(OutboxStore.queuedTimeout - 1)
        #expect(outbox.queuedTooLong(try #require(stored())) == false)

        clock.advance(2)
        #expect(outbox.queuedTooLong(try #require(stored())))
    }

    /// An echoed (`sent`) entry proved transport and merely awaits durability, so
    /// a slow mid-turn persist never trips the queued-lifetime cap.
    @Test func anEchoedEntryIsNeverQueuedTooLong() throws {
        send()
        outbox.markEchoed(platformMsgId: Self.msgId)
        clock.advance(OutboxStore.queuedTimeout * 10)
        #expect(outbox.queuedTooLong(try #require(stored())) == false)
    }

    /// A hand retry is a fresh attempt: the queued-lifetime clock restarts, so a
    /// retried offline message gets its full window rather than instantly
    /// re-failing off the original enrolment time.
    @Test func manualRetryRestartsTheQueuedLifetimeClock() throws {
        send()
        clock.advance(OutboxStore.queuedTimeout + 1)
        #expect(outbox.queuedTooLong(try #require(stored())), "the original enrolment is past the cap")
        outbox.markFailed(platformMsgId: Self.msgId)

        outbox.resetForManualRetry(platformMsgId: Self.msgId, text: "hello", attachments: [])
        #expect(outbox.queuedTooLong(try #require(stored())) == false, "the clock restarted")
    }

    /// A retried image send must stay that message rather than degrading to
    /// text-only, so the blob refs ride the entry to disk and back.
    @Test func entriesAndTheirAttachmentsSurviveARelaunch() {
        let attachment = OutboxAttachment(
            kind: "image", blobId: "sha256:abc.tok", mimeType: "image/png", size: 12,
            filename: "shot.png")
        outbox.beginSend(platformMsgId: Self.msgId, text: "look", attachments: [attachment])

        let reloaded = temp.makeOutbox(sessionId: "s-1")
        let restored = reloaded.entries().first
        #expect(restored?.platformMsgId == Self.msgId)
        #expect(restored?.text == "look")
        #expect(restored?.state == .sending)
        #expect(restored?.attachments.first?.blobId == "sha256:abc.tok")
        #expect(restored?.attachments.first?.filename == "shot.png")
    }

    /// Releasing the last entry removes the file — a relaunch must not resurrect
    /// a send that is already durable.
    @Test func releasingTheLastEntryClearsThePersistedFile() {
        send()
        outbox.confirmDurable(platformMsgId: Self.msgId)
        #expect(temp.makeOutbox(sessionId: "s-1").entries().isEmpty)
    }

    /// Logout/rebind wipes every session's outbox alongside the mirrors.
    @Test func deleteAllWipesEverySessionsOutbox() {
        send()
        temp.makeOutbox(sessionId: "s-2")
            .beginSend(platformMsgId: "other", text: "hi", attachments: [])

        OutboxStore.deleteAll(in: temp.url)
        #expect(temp.makeOutbox(sessionId: "s-1").entries().isEmpty)
        #expect(temp.makeOutbox(sessionId: "s-2").entries().isEmpty)
    }
}
