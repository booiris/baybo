import Foundation
import Testing

@testable import Baybo

/// The live unread badge (`SessionActivity` pings → `noteActivity`). Both
/// phantom-badge sources are pinned here: a ping for the session the user is
/// LOOKING AT, and a ping for a session this device has never listed.
@Suite @MainActor
struct SessionIndexUnreadTests {
    private static let sessionId = "s-1"
    /// A ping stamped a minute ahead of the `recordUserSend` wall clock, so it
    /// advances recency as well as the badge. (A fixed epoch would go stale the
    /// day the suite outlives it.)
    private static var laterMillis: Int64 {
        Int64(Date().addingTimeInterval(60).timeIntervalSince1970 * 1000)
    }

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
    }

    @Test func activityBumpsUnreadAndRecency() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        let before = index.rows.first?.lastActive ?? .distantFuture
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        #expect(index.rows.first?.unread == 1)
        #expect((index.rows.first?.lastActive ?? .distantPast) > before)
    }

    /// The user is looking at this conversation — a ping for it is not unread.
    @Test func foregroundSessionNeverBadges() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.enterSession(Self.sessionId)
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        #expect(index.rows.first?.unread == 0)
    }

    /// Pings are connection-global (every session, subscribed or not). An id
    /// this device has never listed must not conjure a row — a later merge
    /// surfaces it.
    @Test func unknownSessionIsIgnored() {
        index.noteActivity(sessionId: "never-seen", source: "user", atMillis: Self.laterMillis)
        #expect(index.rows.isEmpty)
    }

    /// A ping older than the row's recency must not rewind the list order —
    /// but it is still an unseen reply.
    @Test func staleActivityBadgesWithoutRewindingRecency() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        let before = index.rows.first?.lastActive ?? .distantFuture
        index.noteActivity(sessionId: Self.sessionId, source: "assistant", atMillis: 0)
        #expect(index.rows.first?.lastActive == before)
        #expect(index.rows.first?.unread == 1)
    }

    @Test func enteringASessionClearsItsBadge() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        index.enterSession(Self.sessionId)
        #expect(index.rows.first?.unread == 0)
    }

    /// Leaving hands the foreground marker back, so the next ping counts again.
    /// A fast switch already moved the marker, so `leaveSession` only clears its
    /// own id.
    @Test func leavingRestoresBadgingForThatSession() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.enterSession(Self.sessionId)
        index.leaveSession("some-other-session")
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        #expect(index.rows.first?.unread == 0, "the marker still points at s-1")

        index.leaveSession(Self.sessionId)
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        #expect(index.rows.first?.unread == 1)
    }

    @Test func clearUnreadIsIdempotentAndOnlyTouchesItsRow() {
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
        index.recordUserSend(sessionId: "s-2", text: "second")
        index.noteActivity(
            sessionId: Self.sessionId, source: "assistant", atMillis: Self.laterMillis)
        index.noteActivity(sessionId: "s-2", source: "assistant", atMillis: Self.laterMillis)
        index.clearUnread(Self.sessionId)
        index.clearUnread(Self.sessionId)
        #expect(index.rows.first(where: { $0.id == Self.sessionId })?.unread == 0)
        #expect(index.rows.first(where: { $0.id == "s-2" })?.unread == 1)
    }
}
