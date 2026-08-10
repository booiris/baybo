import Foundation
import Testing

@testable import Baybo

/// The optimistic rename and everything that must not undo it while its PUT is
/// still flying.
///
/// A title is the one row field with THREE writers — the REST merge, the
/// connection-global `SessionUpdated` patch, and now the user — and the other
/// two are both "the server said so". The staged intent is what keeps a local
/// rename from being quietly overwritten by a snapshot or a patch that was
/// composed before it.
@Suite @MainActor
struct SessionIndexRenameTests {
    private nonisolated static let sessionId = "s-1"

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        index.recordUserSend(sessionId: Self.sessionId, text: "what is the answer")
    }

    private func summary(title: String?) -> ChatSessionSummary {
        ChatSessionSummary(
            sessionId: Self.sessionId,
            createdAt: "2026-07-01T00:00:00Z",
            lastActive: "2026-07-10T12:00:00Z",
            lastUserText: "what is the answer",
            lastMessageText: "the answer is 42",
            title: title,
            pinned: false,
            archived: false,
            unreadCount: 0,
            approvalPending: false,
            cronJobId: nil,
            cronJobTitle: nil,
            cronGroupPinned: false)
    }

    @Test func renameShowsAtOnceAndStagesTheIntent() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        #expect(index.rows.first?.title == "Trip planning")
        #expect(index.pendingTitle(for: Self.sessionId) == "Trip planning")
    }

    /// The whole reason the intent is staged: a list refresh that left before the
    /// rename carries the OLD title, and adopting it would flip the row back
    /// under the user seconds after they renamed it.
    @Test func aMergeCarryingTheOldTitleLosesToAStagedRename() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.merge(remote: [summary(title: "Weekend logistics")], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.title == "Trip planning")
    }

    /// …and once the PUT is acknowledged the server is authoritative again.
    @Test func aMergeAfterTheAckAdoptsServerTruth() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.finishRename(Self.sessionId)
        index.merge(remote: [summary(title: "Weekend logistics")], fetchEpoch: index.mutationEpoch)
        #expect(index.rows.first?.title == "Weekend logistics")
    }

    /// The live patch is the other writer. The only one that can race a rename is
    /// an OLDER auto-title (the titler writes solely into a session that has
    /// none), and the user's PUT is about to overwrite it server-side — so
    /// letting it land would leave this device showing a name the gateway no
    /// longer holds.
    @Test func aLiveTitlePatchLosesToAStagedRename() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.applyTitle(sessionId: Self.sessionId, title: "Answering a question")
        #expect(index.rows.first?.title == "Trip planning")

        index.finishRename(Self.sessionId)
        index.applyTitle(sessionId: Self.sessionId, title: "Answering a question")
        #expect(index.rows.first?.title == "Answering a question")
    }

    /// A failed rename on a row that never had a title must rewind to NO title —
    /// not to the seeded preview, and not to an empty string. Leaving a title
    /// behind would also settle the row against the auto-titler, which writes
    /// only where there is none, on the strength of a rename that never landed.
    @Test func aFailedRenameOfAnUntitledRowRewindsToNoTitle() {
        #expect(index.rows.first?.title == nil)
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.rollBackRename(Self.sessionId)
        #expect(index.rows.first?.title == nil)
        #expect(index.pendingTitle(for: Self.sessionId) == nil)
    }

    /// Two renames, both dead offline: the baseline is what the SERVER last
    /// acknowledged, so the rollback lands there rather than on the first
    /// attempt — which the server never saw either.
    @Test func chainedFailedRenamesRewindToTheServerAcknowledgedTitle() {
        index.applyTitle(sessionId: Self.sessionId, title: "Server title")
        index.beginRename(Self.sessionId, title: "First try")
        index.beginRename(Self.sessionId, title: "Second try")
        #expect(index.rows.first?.title == "Second try")
        index.rollBackRename(Self.sessionId)
        #expect(index.rows.first?.title == "Server title")
    }

    /// An acknowledged rename drops its baseline, so a stray late rollback (a
    /// superseded request answering after the newer one) cannot rewind it.
    @Test func finishedRenameDropsTheBaseline() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.finishRename(Self.sessionId)
        index.rollBackRename(Self.sessionId)
        #expect(index.rows.first?.title == "Trip planning")
    }

    /// Renaming a conversation whose delete is already in flight would stage an
    /// intent against a row that is gone — the same guard archive and pin carry.
    @Test func renameNoOpsWhileADeleteIsInFlight() {
        index.beginHide(Self.sessionId)
        index.beginRename(Self.sessionId, title: "Trip planning")
        #expect(index.pendingTitle(for: Self.sessionId) == nil)
    }

    @Test func renameNoOpsForARowThisDeviceHasNeverListed() {
        index.beginRename("s-unknown", title: "Trip planning")
        #expect(index.pendingTitle(for: "s-unknown") == nil)
        #expect(index.rows.count == 1)
    }

    /// The row is what the list renders from, and it is persisted — a rename must
    /// survive the process, not just the session.
    @Test func aRenameSurvivesAReload() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.finishRename(Self.sessionId)
        #expect(temp.makeIndex().rows.first?.title == "Trip planning")
    }

    /// Unbinding drops the staged intents with the rows: they belong to the old
    /// gateway, and a leftover pending title would shield a row on the NEXT one.
    @Test func unbindingClearsStagedRenames() {
        index.beginRename(Self.sessionId, title: "Trip planning")
        index.removeAll()
        #expect(index.pendingTitle(for: Self.sessionId) == nil)
    }
}
