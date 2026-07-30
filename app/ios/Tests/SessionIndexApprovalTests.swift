import Foundation
import Testing

@testable import Baybo

/// The chat-list "waiting for your approval" mark.
///
/// A tool call that trips the gateway's approval gate blocks the turn and
/// **denies itself after five minutes**. The conversation it is blocked in may
/// be one this device has never opened, and the prompt frame itself only
/// reaches connections subscribed to that session — so the list mark is the
/// only thing that can tell a user parked on the conversation list which chat
/// needs them before the window closes.
@Suite @MainActor
struct SessionIndexApprovalTests {
    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
    }

    private func summary(
        id: String,
        lastMessageText: String? = "hi",
        pinned: Bool = false,
        approvalPending: Bool = false
    ) -> ChatSessionSummary {
        ChatSessionSummary(
            sessionId: id,
            createdAt: "2026-07-01T00:00:00Z",
            lastActive: "2026-07-20T12:00:00Z",
            lastUserText: nil,
            lastMessageText: lastMessageText,
            title: nil,
            pinned: pinned,
            archived: false,
            unreadCount: 0,
            approvalPending: approvalPending,
            cronJobId: nil,
            cronJobTitle: nil,
            cronGroupPinned: false)
    }

    @Test func liveEdgeRaisesAndClearsAKnownRow() {
        index.merge(remote: [summary(id: "s1")], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.approvalPending == false)

        index.noteApprovalPending(sessionId: "s1", pending: true)
        #expect(index.rows.first?.approvalPending == true)

        // The clear is the ONLY signal a timed-out gate emits — it self-denies
        // and broadcasts no resolution at all.
        index.noteApprovalPending(sessionId: "s1", pending: false)
        #expect(index.rows.first?.approvalPending == false)
    }

    @Test func aRaiseForAnUnknownSessionNudgesARefetch() {
        // The gateway is describing a conversation this device has no row for —
        // started on the web, or a cron fire — and the one thing that
        // conversation needs is the user. The row has to appear.
        let before = index.listStaleEpoch
        index.noteApprovalPending(sessionId: "never-seen", pending: true)
        #expect(index.listStaleEpoch > before)
    }

    @Test func aClearForAnUnknownSessionIsInert() {
        let before = index.listStaleEpoch
        index.noteApprovalPending(sessionId: "never-seen", pending: false)
        #expect(index.listStaleEpoch == before)
        #expect(index.rows.isEmpty)
    }

    @Test func mergeAdoptsServerTruthInBothDirections() {
        index.merge(
            remote: [summary(id: "s1", approvalPending: true)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.approvalPending == true)

        // Unlike pin / archive / hide this is never a local intent, so the
        // server wins outright — no `pendingMutations` shielding.
        index.merge(
            remote: [summary(id: "s1", approvalPending: false)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.approvalPending == false)
    }

    @Test func aBlockedSessionWithNothingToPreviewIsStillAdmitted() {
        // An agent-started conversation can park on its very first tool call,
        // before any displayable turn exists. The old admission test would drop
        // it as an empty draft — losing precisely the row the user must open.
        index.merge(
            remote: [summary(id: "s1", lastMessageText: nil, approvalPending: true)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.count == 1)
        #expect(index.rows.first?.approvalPending == true)
    }

    @Test func anEmptyUnblockedSessionIsStillFilteredOut() {
        index.merge(
            remote: [summary(id: "s1", lastMessageText: nil)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.isEmpty)
    }

    @Test func theMarkDoesNotSurviveARestart() {
        // A parked gate lives in the GATEWAY's memory: it expires in five
        // minutes and a gateway restart drops every one. A mark restored from
        // disk could only ever describe a prompt that no longer exists, so an
        // offline cold start must show nothing rather than an unanswerable
        // "waiting for you".
        index.merge(
            remote: [summary(id: "s1", approvalPending: true)],
            fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.approvalPending == true)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.count == 1, "the row itself survives")
        #expect(reloaded.rows.first?.approvalPending == false)
    }

    @Test func cronGroupSurfacesAChildsBlockedFire() {
        // A group row is the only thing on screen for its fires, so a fire
        // waiting on the user has to show through it. An OR, not a count: the
        // decision is answerable one tap deeper, never from the list.
        let quiet = SessionRow(
            id: "fire-1", createdAt: .init(timeIntervalSince1970: 0),
            lastActive: .init(timeIntervalSince1970: 100), preview: "a", pinned: false,
            cronJobId: "cj-1", cronJobTitle: "Morning brief")
        let blocked = SessionRow(
            id: "fire-2", createdAt: .init(timeIntervalSince1970: 0),
            lastActive: .init(timeIntervalSince1970: 200), preview: "b", pinned: false,
            cronJobId: "cj-1", cronJobTitle: "Morning brief", approvalPending: true)

        let items = ChatListBuckets.items(from: [quiet, blocked])
        let groups = items.compactMap { item -> CronGroup? in
            if case .cronGroup(let g) = item { return g }
            return nil
        }
        #expect(groups.count == 1)
        #expect(groups.first?.approvalPending == true)

        let calm = ChatListBuckets.items(from: [quiet]).compactMap { item -> CronGroup? in
            if case .cronGroup(let g) = item { return g }
            return nil
        }
        #expect(calm.first?.approvalPending == false)
    }
}

/// The app-icon badge count.
@Suite
struct BadgeCenterTests {
    @Test func totalSumsUnreadAcrossTheMainList() {
        let rows = [
            SessionRow(
                id: "a", createdAt: .init(timeIntervalSince1970: 0),
                lastActive: .init(timeIntervalSince1970: 0), preview: nil, pinned: false,
                unread: 3),
            SessionRow(
                id: "b", createdAt: .init(timeIntervalSince1970: 0),
                lastActive: .init(timeIntervalSince1970: 0), preview: nil, pinned: false,
                unread: 2),
        ]
        #expect(BadgeCenter.total(rows) == 5)
        #expect(BadgeCenter.total([]) == 0)
    }

    @Test func archivedRowsAreExcluded() {
        // They live on their own screen, so counting them would put a number on
        // the icon that nothing the user opens can account for.
        let rows = [
            SessionRow(
                id: "a", createdAt: .init(timeIntervalSince1970: 0),
                lastActive: .init(timeIntervalSince1970: 0), preview: nil, pinned: false,
                unread: 3),
            SessionRow(
                id: "b", createdAt: .init(timeIntervalSince1970: 0),
                lastActive: .init(timeIntervalSince1970: 0), preview: nil, pinned: false,
                archived: true, unread: 40),
        ]
        #expect(BadgeCenter.total(rows) == 3)
    }
}

@Suite(.serialized) @MainActor
struct BadgeCenterDeliveryTests {
    @Test func foregroundReconciliationRewritesAValueTheNSEMayHaveChanged() async {
        BadgeCenter.resetForTesting()
        var writes: [Int] = []
        BadgeCenter.setWriterForTesting { count, completion in
            writes.append(count)
            completion(nil)
        }

        BadgeCenter.apply(0)
        await Task.yield()
        BadgeCenter.apply(0)
        #expect(writes == [0])

        BadgeCenter.apply(0, force: true)
        #expect(writes == [0, 0])
        BadgeCenter.resetForTesting()
    }

    @Test func aFailedSystemWriteDoesNotPoisonTheCoalescingMemo() async {
        BadgeCenter.resetForTesting()
        var writes: [Int] = []
        BadgeCenter.setWriterForTesting { count, completion in
            writes.append(count)
            let error =
                writes.count == 1
                ? NSError(domain: "BadgeCenterTests", code: 1)
                : nil
            completion(error)
        }

        BadgeCenter.apply(4)
        await Task.yield()
        BadgeCenter.apply(4)
        #expect(writes == [4, 4])
        BadgeCenter.resetForTesting()
    }

    @Test func theLatestCountSupersedesAnOppositeWriteStillInFlight() async {
        BadgeCenter.resetForTesting()
        var writes: [Int] = []
        BadgeCenter.setWriterForTesting { count, completion in
            writes.append(count)
            if writes.count == 1 {
                completion(nil)
            }
        }

        BadgeCenter.apply(0)
        await Task.yield()
        BadgeCenter.apply(3)
        BadgeCenter.apply(0)

        #expect(writes == [0, 3, 0])
        BadgeCenter.resetForTesting()
    }
}
