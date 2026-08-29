import Foundation
import Testing

@testable import Baybo

/// `merge(remote:fetchEpoch:)` — the REST list folded over the device-local
/// registry. The headline guard here is a regression this app demonstrably
/// shipped: the old merge kept the local row when `mine.lastActive > remote
/// lastActive`, comparing the DEVICE clock (stamped by `recordUserSend`) against
/// the SERVER clock. A phone running a few seconds fast therefore pinned its
/// just-sent question as the preview forever — the list showed the prompt while
/// the transcript already held the reply.
@Suite @MainActor
struct SessionIndexMergeTests {
    private nonisolated static let sessionId = "s-1"

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
    }

    private func summary(
        id: String = SessionIndexMergeTests.sessionId,
        lastActive: String = "2026-07-10T12:00:00Z",
        lastUserText: String? = nil,
        lastMessageText: String? = nil,
        title: String? = nil,
        pinned: Bool = false,
        archived: Bool = false,
        unreadCount: Int64 = 0,
        cronJobId: String? = nil,
        cronJobTitle: String? = nil,
        cronGroupPinned: Bool = false,
        approvalPending: Bool = false
    ) -> ChatSessionSummary {
        ChatSessionSummary(
            sessionId: id,
            createdAt: "2026-07-01T00:00:00Z",
            lastActive: lastActive,
            lastUserText: lastUserText,
            lastMessageText: lastMessageText,
            title: title,
            pinned: pinned,
            archived: archived,
            unreadCount: unreadCount,
            approvalPending: approvalPending,
            cronJobId: cronJobId,
            cronJobTitle: cronJobTitle,
            cronGroupPinned: cronGroupPinned)
    }

    /// The device clock must never beat the server's. `recordUserSend` stamps
    /// `lastActive = Date()`; a phone running ahead of the gateway used to win
    /// that comparison and keep its stale preview.
    @Test func serverPreviewWinsOverAJustSentLocalOneEvenWithAFastDeviceClock() {
        index.recordUserSend(sessionId: Self.sessionId, text: "what is the answer")
        #expect(index.rows.first?.preview == "what is the answer")
        // The local row's lastActive is `now`; the server's snapshot is dated a
        // full day ago and STILL wins.
        index.merge(
            remote: [
                summary(
                    lastActive: "2020-01-01T00:00:00Z",
                    lastUserText: "what is the answer",
                    lastMessageText: "the answer is 42")
            ],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.preview == "the answer is 42")
        #expect(index.rows.first?.userText == "what is the answer")
    }

    /// A list fetch that STARTED before a mutation resolved is a stale snapshot
    /// even once the pending entry has cleared — it could rewind the flip or
    /// resurrect a just-deleted row.
    @Test func staleFetchEpochDropsTheWholeSnapshot() {
        index.recordUserSend(sessionId: Self.sessionId, text: "local")
        let staleEpoch = index.mutationEpoch - 1
        index.merge(remote: [summary(lastMessageText: "remote")], fetchEpoch: staleEpoch)
        #expect(index.rows.first?.preview == "local")
    }

    /// A DELETE in flight must not be undone by a refresh that raced it: the
    /// row is gone locally, and the remote snapshot still carries it.
    @Test func hiddenRowIsNotResurrectedByARacingMerge() {
        index.recordUserSend(sessionId: Self.sessionId, text: "local")
        index.beginHide(Self.sessionId)
        #expect(index.rows.isEmpty)
        index.merge(remote: [summary(lastMessageText: "still there")], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.isEmpty)
    }

    /// A remote row with no user-authored text this device never listed is a
    /// draft (compose opened elsewhere, a cron session): it stays out of the
    /// list until it has something to show.
    @Test func emptyUnknownRemoteRowIsSuppressedAsADraft() {
        index.merge(remote: [summary()], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.isEmpty)
    }

    @Test func emptyRemoteRowSurvivesWhenPinned() {
        index.merge(remote: [summary(pinned: true)], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.map(\.id) == [Self.sessionId])
    }

    @Test func emptyRemoteRowSurvivesWhenThisDeviceAlreadyListedIt() {
        index.touch(sessionId: Self.sessionId)
        index.merge(remote: [summary()], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.map(\.id) == [Self.sessionId])
    }

    /// A live `SessionUpdated` patch can land ahead of an older snapshot; a
    /// title-less snapshot must not blank the title the user is looking at.
    @Test func titlelessSnapshotKeepsTheLiveTitle() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.applyTitle(sessionId: Self.sessionId, title: "Live title")
        index.merge(remote: [summary(lastUserText: "hello")], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.title == "Live title")
    }

    @Test func remoteTitleIsAuthoritative() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.applyTitle(sessionId: Self.sessionId, title: "Old")
        index.merge(
            remote: [summary(lastUserText: "hello", title: "Server")],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.title == "Server")
    }

    /// An older gateway has no `last_message_text`; the row must still render a
    /// preview rather than going blank.
    @Test func previewFallsBackToTheUserTextOnAnOlderGateway() {
        index.merge(
            remote: [summary(lastUserText: "only the question")],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.preview == "only the question")
    }

    @Test func pendingArchiveFlipBeatsTheFetchedSnapshot() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.beginArchive(Self.sessionId, archived: true)
        index.merge(
            remote: [summary(lastUserText: "hello", archived: false)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.archived == true)
    }

    @Test func matchingSnapshotAcknowledgesAPendingArchive() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.beginArchive(Self.sessionId, archived: true)

        index.merge(
            remote: [summary(lastUserText: "hello", archived: true)],
            fetchEpoch: index.mutationEpoch)

        #expect(index.rows.first?.archived == true)
        #expect(index.pendingMutation(for: Self.sessionId) == nil)
    }

    @Test func mismatchingSnapshotKeepsAPendingArchive() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.beginArchive(Self.sessionId, archived: true)

        index.merge(
            remote: [summary(lastUserText: "hello", archived: false)],
            fetchEpoch: index.mutationEpoch)

        #expect(index.rows.first?.archived == true)
        #expect(index.pendingMutation(for: Self.sessionId) == .archived(true))
    }

    @Test func missingRowAcknowledgesAPendingHide() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.beginHide(Self.sessionId)

        index.merge(remote: [], fetchEpoch: index.mutationEpoch)

        #expect(index.rows.isEmpty)
        #expect(index.pendingMutation(for: Self.sessionId) == nil)
    }

    @Test func pendingPinFlipBeatsTheFetchedSnapshot() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.beginPin(Self.sessionId, pinned: true)
        index.merge(
            remote: [summary(lastUserText: "hello", pinned: false)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.pinned == true)
    }

    /// The badge is server-computed, so the pull reconciles it — accurate even
    /// on a device that missed every live ping.
    @Test func unreadCountIsAdoptedFromTheServer() {
        index.merge(
            remote: [summary(lastMessageText: "reply", unreadCount: 7)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.unread == 7)
    }

    @Test func fractionalGatewayDatesAreParsed() {
        index.merge(
            remote: [
                summary(
                    lastActive: "2026-07-10T12:00:00.123456789Z",
                    lastMessageText: "reply")
            ],
            fetchEpoch: index.mutationEpoch)

        let parsed = index.rows.first?.lastActive.timeIntervalSince1970 ?? 0
        #expect(abs(parsed - 1_783_684_800.123_456_7) < 0.000_001)
    }

    /// Existence is remote-authoritative: a row hidden from another client is
    /// gone from the snapshot, so it must leave the local list too.
    @Test func aRowMissingFromTheSnapshotIsDropped() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.recordUserSend(sessionId: "s-2", text: "second")
        index.merge(
            remote: [summary(id: "s-2", lastMessageText: "second")],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.map(\.id) == ["s-2"])
    }

    @Test func anIdenticalSnapshotDoesNotRepublishTheList() {
        let remote = [summary(lastMessageText: "answer")]
        index.merge(remote: remote, fetchEpoch: index.mutationEpoch)
        var publishes = 0
        let subscription = index.objectWillChange.sink { _ in publishes += 1 }

        index.merge(remote: remote, fetchEpoch: index.mutationEpoch)

        #expect(publishes == 0)
        withExtendedLifetime(subscription) {}
    }
}
