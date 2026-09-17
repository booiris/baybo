import Foundation
import Testing

@testable import Baybo

/// The unread walk behind a re-tap of the selected Chats tab
/// (`docs/chat-list.md`): each tap steps to the next row carrying unread, down
/// the rendered order, without opening it and without marking anything read.
///
/// Because nothing is consumed, the sequence cannot advance by badges clearing —
/// the cursor is the whole feature, and these are the cases that decide whether a
/// tap is ever dead or ever repeats. The rule is driven against real
/// `ChatListBuckets.items(from:)` output rather than a hand-built array, so it is
/// tested over the order the list actually renders.
@Suite @MainActor
struct NextUnreadStepTests {
    private func row(
        _ id: String,
        minutesAgo: Int,
        unread: Int = 0,
        pinned: Bool = false,
        archived: Bool = false,
        jobId: String? = nil,
        jobTitle: String? = nil,
        groupPinned: Bool = false
    ) -> SessionRow {
        SessionRow(
            id: id,
            createdAt: Date(timeIntervalSince1970: 0),
            lastActive: Date(timeIntervalSince1970: 1_000_000 - Double(minutesAgo) * 60),
            preview: nil,
            pinned: pinned,
            archived: archived,
            unread: unread,
            cronJobId: jobId,
            cronJobTitle: jobTitle,
            cronGroupPinned: groupPinned)
    }

    /// `taps` steps fed from the previous result, which is exactly how the screen
    /// drives it.
    private func walk(_ items: [ChatListItem], taps: Int) -> [ChatListItem.ID?] {
        var cursor: ChatListItem.ID?
        return (0..<taps).map { _ in
            let next = ChatListBuckets.nextUnread(after: cursor, in: items)
            if let next { cursor = next }
            return next
        }
    }

    @Test func eachTapStepsToTheNextUnreadRowDownTheList() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1, unread: 2),
            row("b", minutesAgo: 2),
            row("c", minutesAgo: 3, unread: 1),
            row("d", minutesAgo: 4),
            row("e", minutesAgo: 5, unread: 7),
        ])

        #expect(walk(items, taps: 3) == ["a", "c", "e"])
    }

    /// The walk cannot end by exhaustion — nothing is consumed — so a terminal tap
    /// would leave an invisible gesture permanently dead.
    @Test func theWalkWrapsBackToTheTopAfterTheLastUnread() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1, unread: 1),
            row("b", minutesAgo: 2),
            row("c", minutesAgo: 3, unread: 1),
        ])

        #expect(walk(items, taps: 4) == ["a", "c", "a", "c"])
    }

    /// Not a dead tap — the honest "still this one". The screen replays the
    /// arrival mark off the tap's epoch so it does not look dropped.
    @Test func aLoneUnreadRowIsReturnedByEveryTap() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1),
            row("b", minutesAgo: 2, unread: 3),
        ])

        #expect(walk(items, taps: 3) == ["b", "b", "b"])
    }

    @Test func nothingUnreadStepsNowhere() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1),
            row("b", minutesAgo: 2),
        ])

        #expect(ChatListBuckets.nextUnread(after: nil, in: items) == nil)
        #expect(ChatListBuckets.nextUnread(after: "a", in: items) == nil)
    }

    @Test func anEmptyListStepsNowhere() {
        #expect(ChatListBuckets.nextUnread(after: nil, in: []) == nil)
    }

    /// The difference between a cursor and "first unread": reading a row must not
    /// send the next tap back to the top of the list.
    @Test func aCursorOnAReadRowStillResumesAfterIt() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1),
            row("b", minutesAgo: 2),
            row("c", minutesAgo: 3, unread: 1),
        ])

        #expect(ChatListBuckets.nextUnread(after: "b", in: items) == "c")
    }

    /// Rows leave the list (archived, deleted, dropped by a wholesale merge) while
    /// a walk is in progress. Restarting from the top is the recovery; guessing a
    /// slot is not.
    @Test func aCursorWhoseRowHasLeftRestartsFromTheTop() {
        let items = ChatListBuckets.items(from: [
            row("a", minutesAgo: 1, unread: 1),
            row("b", minutesAgo: 2, unread: 1),
        ])

        #expect(ChatListBuckets.nextUnread(after: "gone", in: items) == "a")
    }

    /// The pinned block is the prefix of the rendered order, so a pinned unread row
    /// is visited before any unpinned one. That is what pinning means.
    @Test func aPinnedUnreadRowIsVisitedFirstEvenWhenItIsOlder() {
        let items = ChatListBuckets.items(from: [
            row("recent", minutesAgo: 1, unread: 1),
            row("ancient", minutesAgo: 900, unread: 1, pinned: true),
        ])

        #expect(walk(items, taps: 2) == ["ancient", "recent"])
    }

    /// A group is ONE stop, selected on the sum over the fires drawn inside it. The
    /// walk cannot step into a group — those fires are rows of `CronGroupScreen`.
    @Test func aCronGroupIsASingleStopCarryingItsMembersSum() {
        let items = ChatListBuckets.items(from: [
            row("chat", minutesAgo: 1, unread: 1),
            row("fire-1", minutesAgo: 2, unread: 1, jobId: "cj", jobTitle: "Morning brief"),
            row("fire-2", minutesAgo: 3, unread: 1, jobId: "cj", jobTitle: "Morning brief"),
        ])

        #expect(walk(items, taps: 3) == ["chat", "cron:cj", "chat"])
    }

    /// An archived row is not rendered, so it is not a stop — which is also why the
    /// tab badge (`BadgeCenter.total`, same filter) and the walk cannot disagree.
    @Test func archivedUnreadIsNotAStop() {
        let items = ChatListBuckets.items(from: [
            row("visible", minutesAgo: 2, unread: 1),
            row("filed", minutesAgo: 1, unread: 5, archived: true),
        ])

        #expect(walk(items, taps: 2) == ["visible", "visible"])
    }

    /// `items(from:)` must be TOTAL: two rows sharing `(pinned, lastActive)` had no
    /// defined relative order between calls, and a walk that visits each row exactly
    /// once cannot rest on that.
    @Test func rowsSharingATimestampKeepOneStableOrder() {
        let rows = [
            row("z", minutesAgo: 5, unread: 1),
            row("m", minutesAgo: 5, unread: 1),
            row("a", minutesAgo: 5, unread: 1),
        ]

        #expect(walk(ChatListBuckets.items(from: rows), taps: 3) == ["a", "m", "z"])
        #expect(walk(ChatListBuckets.items(from: rows.reversed()), taps: 3) == ["a", "m", "z"])
    }
}
