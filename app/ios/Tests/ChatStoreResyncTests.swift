import Foundation
import Testing

@testable import Baybo

/// The per-session resync — the user's escape hatch when a transcript renders
/// wrong on this device but right on a fresh install. It exists because it
/// throws local state away and re-runs the COLD-OPEN path, so what these cases
/// pin is what it throws away and, far more importantly, what it must not:
///
/// * the transcript mirror goes (there is nothing left for `deliverInit` to
///   restore, which is the whole hatch);
/// * the session row, every OTHER session's mirror, the send outbox and the
///   pending approvals stay. A resync that eats the user's unsent message — or
///   the card that is the only way to answer a gate that self-denies in five
///   minutes — is worse than the bug it recovers from.
///
/// It is reached from the CHAT LIST's long-press menu, so the conversation it
/// rebuilds usually has no page in the webview and no store in memory. That
/// splits the hatch in two, and both halves are here: the drop is immediate,
/// while the re-seed of the outbox's bubbles is DEFERRED to that session's next
/// mount — where it runs unconditionally, because the webview, not native,
/// decides whether a bubble is already there (`holdsUserSend`).
///
/// The page reload itself is not reachable from a unit test — it needs a live
/// `WKWebView` — so the seams around it are: the store always ASKS
/// (`rebuildIfShowing`), and `TranscriptBridge` answers from the session it
/// remembers mounting.
@Suite @MainActor
struct ChatStoreResyncTests {
    /// `nonisolated` so the helpers below can take them as default arguments.
    private nonisolated static let sessionId = "s-1"
    private nonisolated static let otherSessionId = "s-2"
    /// Long enough for a detached `Task` to land, short enough that the cases
    /// asserting something NEVER happens don't stall the suite.
    private static let negativeTimeout: Duration = .milliseconds(250)
    private static let mirrorJson = #"{"rows":[{"id":"r-1"}],"lastOrdinal":42}"#

    private let temp: TempSupportDir
    /// The outbox's only time input — moved by hand so two enrolments get
    /// distinct `createdAt`s and the replay's ordering is actually asserted.
    private let clock = TestClock()
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let outbox: OutboxStore
    private let store: ChatStore
    /// Stands in for the one shared transcript webview.
    private let transcript: FakeTranscriptSurface

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        transcript = FakeTranscriptSurface()
        let clock = self.clock
        index = temp.makeIndex()
        // A LISTED session: it exists on the gateway, so a sync actually pulls.
        index.touch(sessionId: Self.sessionId)
        outbox = temp.makeOutbox(sessionId: Self.sessionId, now: { clock.now })
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index, outbox: outbox)
    }

    private func writeMirror(_ sessionId: String = ChatStoreResyncTests.sessionId) {
        TranscriptStore.write(sessionId: sessionId, stateJson: Self.mirrorJson, in: temp.url)
    }

    private func mirror(_ sessionId: String = ChatStoreResyncTests.sessionId) -> String? {
        TranscriptStore.read(sessionId: sessionId, in: temp.url)
    }

    // MARK: - What it throws away

    @Test func resyncDeletesThisSessionsMirror() {
        writeMirror()
        #expect(mirror() != nil)

        store.resync(transcript: transcript)

        #expect(mirror() == nil, "deliverInit must have nothing left to restore")
    }

    /// A targeted delete, not a sweep: the hatch is per-session, and every other
    /// conversation's cold open still has to paint with zero network.
    @Test func resyncLeavesEveryOtherSessionsMirrorAlone() {
        writeMirror()
        writeMirror(Self.otherSessionId)

        store.resync(transcript: transcript)

        #expect(mirror(Self.otherSessionId) == Self.mirrorJson)
    }

    /// The conversation is not being deleted — only its local rendering. Losing
    /// the row would take the chat off the list with no way back to it.
    @Test func resyncKeepsTheSessionRow() {
        writeMirror()

        store.resync(transcript: transcript)

        #expect(index.contains(sessionId: Self.sessionId))
    }

    // MARK: - The rebuild ask

    /// The store never decides whether to reload the page — it says which
    /// conversation was discarded and the webview answers, because it alone
    /// knows which one it is currently mounting.
    @Test func resyncAsksTheTranscriptToRebuildThisSession() {
        store.resync(transcript: transcript)

        #expect(transcript.rebuildAsks == [Self.sessionId])
    }

    /// And `TranscriptBridge` answers from a session id it holds ITSELF, not
    /// from its (weak) store: the LRU can evict a session's store while its
    /// transcript is still the mounted page, and re-entering the same id does
    /// not remount the React tree — so a bridge that had forgotten would leave
    /// the discarded rows on screen.
    @Test func theTranscriptRemembersWhichChatItIsShowing() {
        let bridge = TranscriptBridge(store: store)

        #expect(bridge.shownSessionId == Self.sessionId)
    }

    /// The list is reachable before the transcript webview has ever booted (a
    /// launch straight into the chat list, no conversation opened yet). Nothing
    /// to reload then, and the drop still has to land.
    @Test func resyncWorksWithNoTranscriptAtAll() {
        writeMirror()

        store.resync(transcript: nil)

        #expect(mirror() == nil)
    }

    // MARK: - The outbox survives

    /// Queued sends live in `OutboxStore`, a sibling file of the mirror, so
    /// clearing the mirror cannot touch them. Asserted through a SECOND store
    /// over the same directory: the in-memory copy surviving proves nothing
    /// about the bytes on disk, and the bytes are what a relaunch reads.
    @Test func resyncKeepsTheSendOutboxOnDisk() {
        outbox.beginSend(platformMsgId: "m-1", text: "queued", attachments: [])
        outbox.beginSend(platformMsgId: "m-2", text: "dead", attachments: [])
        outbox.markFailed(platformMsgId: "m-2")
        writeMirror()

        store.resync(transcript: transcript)

        let reread = temp.makeOutbox(sessionId: Self.sessionId)
        #expect(Set(reread.entries().map(\.platformMsgId)) == ["m-1", "m-2"])
        #expect(reread.entries().first { $0.platformMsgId == "m-1" }?.text == "queued")
        #expect(reread.entries().first { $0.platformMsgId == "m-2" }?.state == .failed)
    }

    /// Nothing is re-seeded at resync time. Reached from the list, this
    /// conversation has no page to seed — the bubbles come back on its next
    /// mount, and pushing them at a transcript standing on some OTHER chat would
    /// put this session's unsent messages in that one's thread.
    @Test func resyncSeedsNoBubblesOfItsOwn() {
        outbox.beginSend(platformMsgId: "m-1", text: "queued", attachments: [])

        store.resync(transcript: transcript)

        #expect(transcript.seeded.isEmpty)
        #expect(transcript.failed.isEmpty)
    }

    /// The deferred half: whatever mounts this session next re-seeds the outbox
    /// onto it — oldest first, with the failed entry still failed. Order is the
    /// point (`OutboxStore` is a dictionary, so `entries()` has none), and the
    /// red dot is the only affordance that can retry a failed send.
    @Test func theNextMountReSeedsTheBubblesInOrderWithTheRedDot() {
        outbox.beginSend(platformMsgId: "m-old", text: "first", attachments: [])
        clock.advance(60)
        outbox.beginSend(platformMsgId: "m-new", text: "second", attachments: [])
        outbox.markFailed(platformMsgId: "m-new")
        store.resync(transcript: transcript)

        store.replayUnconfirmedSends(to: transcript)

        #expect(transcript.seeded.map(\.msgId) == ["m-old", "m-new"])
        #expect(transcript.seeded.map(\.text) == ["first", "second"])
        #expect(
            transcript.failed == ["m-new"],
            "the red dot is the only retry affordance, and it lives in the web row")
    }

    /// The re-seed is NOT gated on a resync having happened — that latch is what
    /// made every other way of losing a bubble (a jetsam before the mirror's
    /// debounced write, a session whose mirror was never written) permanent. It
    /// runs on every mount and the webview drops what it already holds.
    @Test func theReSeedRunsWithNoResyncInvolved() {
        outbox.beginSend(platformMsgId: "m-1", text: "queued", attachments: [])

        store.replayUnconfirmedSends(to: transcript)

        #expect(transcript.seeded.map(\.msgId) == ["m-1"])
    }

    /// The bubbles the rebuilt page is re-seeded with, oldest first. Order is
    /// the point: `OutboxStore` is a dictionary, so `entries()` has none, and
    /// unordered re-seeding would shuffle the user's own messages.
    @Test func theReplayIsOldestFirstAndKeepsAFailedSend() {
        outbox.beginSend(platformMsgId: "m-old", text: "first", attachments: [])
        clock.advance(60)
        outbox.beginSend(platformMsgId: "m-new", text: "second", attachments: [])
        outbox.markFailed(platformMsgId: "m-new")

        let replay = store.unconfirmedSends()

        #expect(replay.map(\.platformMsgId) == ["m-old", "m-new"])
        #expect(
            replay.last?.state == .failed,
            "the red dot is the only retry affordance, and it lives in the web row")
    }

    /// An attachment send must come back as that message, not as text — the
    /// blob refs ride the entry, exactly as they do for a resend.
    @Test func theReplayCarriesAttachments() {
        outbox.beginSend(
            platformMsgId: "m-1", text: "look",
            attachments: [
                OutboxAttachment(
                    kind: "image", blobId: "sha256:abc.tok", mimeType: "image/png", size: 12,
                    filename: "shot.png")
            ])

        #expect(store.unconfirmedSends().first?.attachments.first?.blobId == "sha256:abc.tok")

        store.replayUnconfirmedSends(to: transcript)

        #expect(transcript.seeded.first?.attachments.first?.blobId == "sha256:abc.tok")
    }

    // MARK: - Pending approvals

    /// The approval card is NATIVE and derived from the frame stream, and a
    /// baseline sync carries rows, not pending prompts — so a resync that
    /// dropped it would leave a gate nobody can answer until it self-denies.
    @Test func resyncKeepsPendingApprovals() async {
        store.connect()
        #expect(await waitUntil { store.connState == .connected })
        client.deliver(
            """
            {"kind":"approval_requested","session_id":"s-1","call_id":"prompt-1",
             "tool_call_id":"toolcall-1","tool":"Bash","params_preview":"{}","accesses":[]}
            """,
            to: Self.sessionId)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        store.resync(transcript: transcript)

        #expect(store.pendingApprovals.map(\.callId) == ["prompt-1"])
    }

    // MARK: - The banner

    /// The transcript blanking and refilling is ambiguous on a slow link, so the
    /// hatch says what it did on the dock's notice line.
    @Test func resyncRaisesTheBanner() {
        store.resync(transcript: transcript)

        #expect(store.notice != nil)
    }

    /// The banner announces a rebuild, so the rebuild's own rows take it back —
    /// it is never left to a timer, and never left standing over a finished job.
    @Test func theRebuildsSyncAnswerRetractsTheBanner() async {
        store.resync(transcript: transcript)
        #expect(store.notice != nil)

        store.requestSync(sinceOrdinal: nil, limit: 50)

        #expect(await waitUntil { store.notice == nil })
    }

    /// A sync that FAILED rebuilt nothing — leave the line up rather than
    /// claiming a recovery that did not happen.
    @Test func aFailedSyncLeavesTheBannerUp() async {
        client.failSync(with: BayboError.Other(message: "gateway unreachable"))
        store.resync(transcript: transcript)

        store.requestSync(sinceOrdinal: nil, limit: 50)

        _ = await waitUntil(timeout: Self.negativeTimeout) { store.notice == nil }
        #expect(store.notice != nil)
    }

    /// Whoever writes the line next OWNS it: a later failure's message must not
    /// be swept away by the rebuild the user asked for before it.
    @Test func aLaterWritersLineIsNotRetractedByTheRebuild() async {
        store.resync(transcript: transcript)
        store.notice = "boom"

        store.requestSync(sinceOrdinal: nil, limit: 50)

        _ = await waitUntil(timeout: Self.negativeTimeout) { store.notice == nil }
        #expect(store.notice == "boom")
    }

    /// And the line dies with the VISIT like every other one — a notice must be
    /// owned or retracted on leave, never stranded over the next visit's dock.
    @Test func leavingTheChatRetractsTheBanner() {
        store.resync(transcript: transcript)
        #expect(store.notice != nil)

        store.leaveChat()

        #expect(store.notice == nil)
    }
}
