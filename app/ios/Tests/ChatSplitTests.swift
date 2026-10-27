import SwiftUI
import Testing

@testable import Baybo

/// When the Chats tab turns into a split view, and how the navigation path is
/// shared between the docked conversation and the stack.
@Suite @MainActor
struct ChatSplitTests {
    /// Only a window that is wide AND tall: iPhone Duo's inner display.
    @Test func aWideTallWindowSplits() {
        #expect(ChatSplit.engages(horizontal: .regular, vertical: .regular))
    }

    /// A phone in portrait, a Pro Max in landscape (regular width, compact
    /// height), the Duo's outer display and a Split View half all keep the push.
    @Test(arguments: [
        (UserInterfaceSizeClass.compact, UserInterfaceSizeClass.regular),
        (.regular, .compact),
        (.compact, .compact),
    ])
    func everythingElsePushes(horizontal: UserInterfaceSizeClass, vertical: UserInterfaceSizeClass) {
        #expect(!ChatSplit.engages(horizontal: horizontal, vertical: vertical))
    }

    @Test func unresolvedSizeClassesPush() {
        #expect(!ChatSplit.engages(horizontal: nil, vertical: .regular))
        #expect(!ChatSplit.engages(horizontal: .regular, vertical: nil))
    }

    /// The leading conversation docks, and the stack only sees what is pushed
    /// over it.
    @Test func theLeadingConversationDocks() {
        let path: [AppStore.ChatRoute] = [.session("a"), .connection]
        let docked = AppStore.dockedSession(in: path, splitView: true)
        #expect(docked == "a")
        #expect(AppStore.stackPath(path, docked: docked) == [.connection])
    }

    /// Without the split nothing docks: the stack gets the whole path, exactly
    /// as on a phone.
    @Test func withoutTheSplitTheStackHasEverything() {
        let path: [AppStore.ChatRoute] = [.session("a")]
        let docked = AppStore.dockedSession(in: path, splitView: false)
        #expect(docked == nil)
        #expect(AppStore.stackPath(path, docked: docked) == path)
    }

    /// A conversation pushed over another screen (unfolding over the archived
    /// list's open chat) stays pushed: only a LEADING one docks.
    @Test func aConversationOverAnotherScreenStaysPushed() {
        let path: [AppStore.ChatRoute] = [.archived, .session("a")]
        let docked = AppStore.dockedSession(in: path, splitView: true)
        #expect(docked == nil)
        #expect(AppStore.stackPath(path, docked: docked) == path)
    }

    private func row(
        _ id: String, minutesAgo: Int, unread: Int = 0, pinned: Bool = false,
        jobId: String? = nil, groupPinned: Bool = false
    ) -> SessionRow {
        SessionRow(
            id: id,
            createdAt: Date(timeIntervalSince1970: 0),
            lastActive: Date(timeIntervalSince1970: 1_000_000 - Double(minutesAgo) * 60),
            preview: nil,
            pinned: pinned,
            archived: false,
            unread: unread,
            cronJobId: jobId,
            cronJobTitle: jobId.map { "Job \($0)" },
            cronGroupPinned: groupPinned)
    }

    private func sidebar(_ rows: [SessionRow], expanded: Set<String> = []) -> [String] {
        ChatListBuckets.sidebar(from: ChatListBuckets.items(from: rows), expanded: expanded)
            .map(\.id)
    }

    /// The web sidebar's order — Pinned, the folders, everything else — with a
    /// cron folder closed until opened, and a pinned group at the folders' head
    /// rather than in the Pinned block.
    @Test func blocksFollowTheWebOrder() {
        let rows = [
            row("chat", minutesAgo: 1),
            row("fire-a", minutesAgo: 2, jobId: "a"),
            row("fire-b", minutesAgo: 30, jobId: "b", groupPinned: true),
            row("pin", minutesAgo: 60, pinned: true),
        ]
        #expect(
            sidebar(rows) == [
                "section:pinned", "pin",
                "section:scheduled", "cron:b", "cron:a",
                "section:chats", "chat",
            ])
    }

    /// An open folder lists its fires straight under it, newest first.
    @Test func anOpenFolderListsItsFires() {
        let rows = [
            row("old", minutesAgo: 60, jobId: "a"),
            row("new", minutesAgo: 1, jobId: "a"),
        ]
        #expect(sidebar(rows, expanded: ["a"]) == ["section:scheduled", "cron:a", "new", "old"])
    }

    /// With nothing above them, the conversations need no label.
    @Test func aListOfOnlyChatsHasNoLabels() {
        #expect(sidebar([row("a", minutesAgo: 1), row("b", minutesAgo: 2)]) == ["a", "b"])
    }

    /// The walk counts a closed folder once, and an open one through its fires —
    /// never both, or two taps would land on the same unread.
    @Test func theWalkSeesAFolderOrItsFiresNeverBoth() {
        let rows = [row("fire", minutesAgo: 1, unread: 2, jobId: "a")]
        let items = ChatListBuckets.items(from: rows)
        let closed = ChatListBuckets.sidebar(from: items, expanded: []).compactMap(\.walkItem)
        let open = ChatListBuckets.sidebar(from: items, expanded: ["a"]).compactMap(\.walkItem)
        #expect(closed.map(\.id) == ["cron:a"])
        #expect(open.map(\.id) == ["fire"])
    }

    /// A docked fire names the folder to open; a pinned fire has escaped its
    /// group, so it names none.
    @Test func aDockedFireNamesItsFolder() {
        let items = ChatListBuckets.items(from: [
            row("fire", minutesAgo: 1, jobId: "a"),
            row("pinned-fire", minutesAgo: 2, pinned: true, jobId: "a"),
        ])
        #expect(ChatListBuckets.group(drawing: "fire", in: items) == "a")
        #expect(ChatListBuckets.group(drawing: "pinned-fire", in: items) == nil)
    }
}
