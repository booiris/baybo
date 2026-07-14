import Foundation
import Testing

@testable import Baybo

/// The transcript mirror's retention — the cache that decides whether opening a
/// chat paints instantly or sits blank until the network answers.
///
/// This pins a regression the app shipped: `save()` used to end by pruning the
/// mirrors down to the ten most recently active rows. Every ingredient of that
/// was hostile. `save()` runs on essentially everything (a list merge — including
/// the one the pop back from a chat fires — an activity ping, a badge clear);
/// `rows` after a merge is the gateway's WHOLE session list ranked by the
/// SERVER's `lastActive`, so rows this device has never opened (and every
/// individual cron fire) spent keep-slots on mirrors that do not exist; and
/// reading a chat bumps nothing (`touch` deliberately does not move `lastActive`
/// — "ordering means message activity, not visits"). So a conversation outside
/// the ten most recently MESSAGED ones had the mirror it wrote on exit deleted
/// seconds later, and opened blank on every single re-entry, forever.
///
/// Mirrors are now kept for every session this device has rendered. The only two
/// removals are user-triggered: deleting the conversation, and unbinding.
@Suite @MainActor
struct SessionIndexMirrorTests {
    private nonisolated static let sessionId = "s-mirror"

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
    }

    private func writeMirror(_ sessionId: String, _ json: String = #"{"messages":[]}"#) {
        TranscriptStore.write(sessionId: sessionId, stateJson: json, in: temp.url)
    }

    private func mirror(_ sessionId: String) -> String? {
        TranscriptStore.read(sessionId: sessionId, in: temp.url)
    }

    private func summary(
        id: String, lastActive: String, lastMessageText: String? = "hi"
    ) -> ChatSessionSummary {
        ChatSessionSummary(
            sessionId: id,
            createdAt: "2026-07-01T00:00:00Z",
            lastActive: lastActive,
            lastUserText: nil,
            lastMessageText: lastMessageText,
            title: nil,
            pinned: false,
            archived: false,
            unreadCount: 0,
            cronJobId: nil,
            cronJobTitle: nil)
    }

    /// The headline. A gateway with far more than ten sessions, ours the OLDEST
    /// of them by server recency — exactly the row the old top-10 prune deleted
    /// first. Its mirror must survive the merge that ranks it last.
    @Test func aMirrorSurvivesAMergeThatRanksItFarOutsideTheOldTopTen() {
        writeMirror(Self.sessionId, #"{"messages":[{"id":"m1"}],"lastOrdinal":7}"#)
        index.recordUserSend(sessionId: Self.sessionId, text: "mine")

        // Ours is dated 2020; thirty other sessions are all newer.
        var remote = [summary(id: Self.sessionId, lastActive: "2020-01-01T00:00:00Z")]
        for i in 0..<30 {
            remote.append(summary(id: "other-\(i)", lastActive: "2026-07-1\(i % 10)T00:00:00Z"))
        }
        index.merge(remote: remote, fetchEpoch: index.mutationEpoch)

        #expect(index.rows.count == 31)
        #expect(mirror(Self.sessionId) == #"{"messages":[{"id":"m1"}],"lastOrdinal":7}"#)
    }

    /// The pop back from a chat refreshes the list, and every activity ping saves
    /// too. None of that may touch the cache of a session we are not deleting —
    /// and it must hold with our row buried under a busier crowd than the old
    /// prune's ten keep-slots, which is precisely when it used to disappear.
    @Test func repeatedSavesFromOtherSessionsActivityLeaveTheMirrorAlone() {
        writeMirror(Self.sessionId)
        index.recordUserSend(sessionId: Self.sessionId, text: "mine")
        var remote = [summary(id: Self.sessionId, lastActive: "2020-01-01T00:00:00Z")]
        for i in 0..<12 {
            remote.append(summary(id: "loud-\(i)", lastActive: "2026-07-14T0\(i % 10):00:00Z"))
        }
        index.merge(remote: remote, fetchEpoch: index.mutationEpoch)

        for i in 0..<50 {
            index.noteActivity(
                sessionId: "loud-\(i % 12)", source: "assistant",
                atMillis: 1_784_000_000_000 + Int64(i) * 1000)
        }
        index.clearUnread("loud-0")

        #expect(mirror(Self.sessionId) != nil)
    }

    /// A session the gateway reports but this device has never opened holds no
    /// mirror — it must not cost anyone else theirs (the old prune spent a
    /// keep-slot on every such row, including every cron fire).
    @Test func rowsWithNoMirrorDoNotEvictRowsThatHaveOne() {
        writeMirror(Self.sessionId)
        var remote = [summary(id: Self.sessionId, lastActive: "2020-01-01T00:00:00Z")]
        for i in 0..<20 {
            remote.append(summary(id: "cron-fire-\(i)", lastActive: "2026-07-14T0\(i % 10):00:00Z"))
        }
        index.merge(remote: remote, fetchEpoch: index.mutationEpoch)

        #expect(mirror(Self.sessionId) != nil)
        for i in 0..<20 {
            #expect(mirror("cron-fire-\(i)") == nil)
        }
    }

    /// Deleting the conversation is one of the two ways a mirror is ever removed.
    /// It used to fall out of `save()`'s prune; it is now explicit, and the row
    /// leaving the list must still take its cache with it.
    @Test func deletingTheConversationDropsItsMirror() {
        writeMirror(Self.sessionId)
        index.recordUserSend(sessionId: Self.sessionId, text: "mine")
        #expect(mirror(Self.sessionId) != nil)

        index.beginHide(Self.sessionId)

        #expect(index.rows.isEmpty)
        #expect(mirror(Self.sessionId) == nil)
    }

    /// Deleted on ANOTHER client: the server stops listing it, so `merge` drops
    /// the row — and `beginHide` never runs. Without this the conversation the
    /// user deleted on the web would keep its whole transcript on this phone,
    /// unreachable and undeletable. The row leaving the list takes its cache.
    @Test func aRowTheServerNoLongerListsLosesItsMirrorToo() {
        writeMirror(Self.sessionId)
        writeMirror("s-kept")
        index.merge(
            remote: [
                summary(id: Self.sessionId, lastActive: "2026-07-14T00:00:00Z"),
                summary(id: "s-kept", lastActive: "2026-07-14T00:00:00Z"),
            ],
            fetchEpoch: index.mutationEpoch)
        #expect(mirror(Self.sessionId) != nil)

        // The next pull no longer carries it — deleted elsewhere.
        index.merge(
            remote: [summary(id: "s-kept", lastActive: "2026-07-14T00:00:00Z")],
            fetchEpoch: index.mutationEpoch)

        #expect(index.rows.map(\.id) == ["s-kept"])
        #expect(mirror(Self.sessionId) == nil)
        #expect(mirror("s-kept") != nil)
    }

    /// ...but a row hidden by an IN-FLIGHT local delete is `beginHide`'s to own.
    /// `merge` suppresses it (`pendingMutations`), and must not treat that
    /// suppression as "the server dropped it" — a rollback still has a row to
    /// restore, and the mirror was already dropped by `beginHide` itself.
    @Test func mergeLeavesPendingMutationRowsToBeginHide() {
        writeMirror("s-pending")
        index.recordUserSend(sessionId: "s-pending", text: "mine")
        index.beginHide("s-pending")
        #expect(mirror("s-pending") == nil)

        // The server still lists it (the DELETE is in flight); merge suppresses
        // the row rather than resurrecting it, and touches no other mirror.
        writeMirror(Self.sessionId)
        index.recordUserSend(sessionId: Self.sessionId, text: "keep me")
        index.merge(
            remote: [
                summary(id: "s-pending", lastActive: "2026-07-14T00:00:00Z"),
                summary(id: Self.sessionId, lastActive: "2026-07-14T00:00:00Z"),
            ],
            fetchEpoch: index.mutationEpoch)

        #expect(!index.rows.contains { $0.id == "s-pending" })
        #expect(mirror(Self.sessionId) != nil)
    }

    /// The delete must land even when a racing merge already took the row out
    /// from under it (deleted on another client while the confirm dialog was up):
    /// `beginHide` drops the file ahead of its row-existence guard.
    @Test func deletingAConversationWhoseRowARaceAlreadyRemovedStillDropsItsMirror() {
        writeMirror(Self.sessionId)
        // No row for it at all — the merge that would have created one never ran.
        #expect(!index.contains(sessionId: Self.sessionId))

        index.beginHide(Self.sessionId)

        #expect(mirror(Self.sessionId) == nil)
    }

    /// A failed DELETE puts the row back but NOT the mirror — the same contract
    /// the prune-era code had (`rollBackHide`: "its mirror stays gone"). The row
    /// re-entry just refetches, which is exactly the no-cache path.
    @Test func aRolledBackDeleteRestoresTheRowButNotTheMirror() {
        writeMirror(Self.sessionId)
        index.recordUserSend(sessionId: Self.sessionId, text: "mine")

        index.beginHide(Self.sessionId)
        index.rollBackHide(Self.sessionId)

        #expect(index.rows.contains { $0.id == Self.sessionId })
        #expect(mirror(Self.sessionId) == nil)
    }

    /// The other user-triggered wipe: the rows (and their transcripts) belong to
    /// the gateway we just unbound from.
    @Test func unbindingWipesEveryMirror() {
        writeMirror(Self.sessionId)
        writeMirror("s-other")
        index.recordUserSend(sessionId: Self.sessionId, text: "mine")

        index.removeAll()

        #expect(mirror(Self.sessionId) == nil)
        #expect(mirror("s-other") == nil)
    }
}
