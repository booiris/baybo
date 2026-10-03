import Foundation
import Testing

@testable import Baybo

/// The list badge versus the server's `unreadCount`. The server is the truth only
/// once the read cursor has moved there, and a read PUT is fire-and-forget — so
/// every list pull that lands while the chat is open, or before the PUT does,
/// is counting replies the user has already read. That stale count is what kept
/// the badge up until the pull after the user left the chat.
@Suite @MainActor
struct SessionIndexReadTests {
    private nonisolated static let sessionId = "s-1"

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
    }

    private func summary(unreadCount: Int64) -> ChatSessionSummary {
        ChatSessionSummary(
            sessionId: Self.sessionId,
            createdAt: "2026-07-01T00:00:00Z",
            lastActive: "2026-07-10T12:00:00Z",
            lastUserText: nil,
            lastMessageText: "reply",
            title: nil,
            pinned: false,
            archived: false,
            unreadCount: unreadCount,
            approvalPending: false,
            cronJobId: nil,
            cronJobTitle: nil,
            cronGroupPinned: false)
    }

    private func pull(unreadCount: Int64, fetch: ListFetch? = nil) {
        index.merge(remote: [summary(unreadCount: unreadCount)], fetch: fetch ?? index.beginListFetch())
    }

    private var badge: Int? { index.rows.first?.unread }

    @Test func theOpenChatIgnoresTheServersCount() {
        pull(unreadCount: 3)
        index.enterSession(Self.sessionId)

        pull(unreadCount: 3)
        #expect(badge == 0)

        index.leaveSession(Self.sessionId)
        pull(unreadCount: 3)
        #expect(badge == 3, "off screen, the server is the truth again")
    }

    @Test func aReadInFlightHoldsTheBadgeAtZero() async throws {
        pull(unreadCount: 3)
        let (gate, open) = AsyncStream<Void>.makeStream()
        let read = Task {
            try await index.markingRead([Self.sessionId]) {
                for await _ in gate { break }
            }
        }
        #expect(await waitUntil { badge == 0 })

        pull(unreadCount: 3)
        #expect(badge == 0)
        index.noteActivity(sessionId: Self.sessionId, source: "assistant", atMillis: 0)
        #expect(badge == 0)

        open.yield()
        try await read.value
    }

    /// The race that outlives the PUT: a pull the server answered before the
    /// cursor moved, delivered after the PUT returned.
    @Test func aSnapshotRequestedBeforeTheReadLandedIsNotTrusted() async throws {
        pull(unreadCount: 3)
        let stale = index.beginListFetch()

        try await index.markingRead([Self.sessionId]) {}
        pull(unreadCount: 3, fetch: stale)
        #expect(badge == 0)

        pull(unreadCount: 2)
        #expect(badge == 2, "a pull requested after the read landed is trusted")
    }

    @Test func aFailedReadStopsShielding() async {
        pull(unreadCount: 3)
        struct Offline: Error {}

        await #expect(throws: Offline.self) {
            try await index.markingRead([Self.sessionId]) { throw Offline() }
        }
        pull(unreadCount: 3)
        #expect(badge == 3)
    }
}
