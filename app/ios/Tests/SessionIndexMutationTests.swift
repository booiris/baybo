import Foundation
import Testing

@testable import Baybo

/// Optimistic archive / pin / delete and their rollbacks.
///
/// The load-bearing case is the CHAINED failure: archive, then undo inside the
/// toast window, both requests dead offline. Rolling back to the negation of the
/// failed intent would re-archive a row the server never archived — and the row
/// is then gone from the main list for good, with no undo left to tap. Rollback
/// therefore targets the last SERVER-ACKNOWLEDGED value.
@Suite @MainActor
struct SessionIndexMutationTests {
    private static let sessionId = "s-1"

    private let temp: TempSupportDir
    private let index: SessionIndex

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        index.recordUserSend(sessionId: Self.sessionId, text: "hello")
    }

    @Test func archiveFlipsOptimisticallyAndStagesTheIntent() {
        let epoch = index.mutationEpoch
        index.beginArchive(Self.sessionId, archived: true)
        #expect(index.rows.first?.archived == true)
        #expect(index.pendingMutation(for: Self.sessionId) == .archived(true))
        #expect(index.mutationEpoch > epoch)
    }

    /// archive → undo → BOTH fail. The baseline (`false`, what the server last
    /// acknowledged) wins; the negation of the failed `.archived(false)` intent
    /// would be `true` and would strand the row in the archive.
    @Test func failedArchiveThenUndoRollsBackToTheServerAcknowledgedValue() {
        #expect(index.rows.first?.archived == false)
        index.beginArchive(Self.sessionId, archived: true)
        index.beginArchive(Self.sessionId, archived: false)
        index.rollBackArchive(Self.sessionId)
        #expect(index.rows.first?.archived == false)
        #expect(index.pendingMutation(for: Self.sessionId) == nil)
    }

    /// The same chain against a row the server DID archive: the baseline is
    /// `true`, so an unarchive that fails rewinds into the archive, not out of it.
    @Test func rollbackRewindsToAnArchivedBaseline() {
        index.setArchivedFlag(Self.sessionId, archived: true)
        index.beginArchive(Self.sessionId, archived: false)
        index.rollBackArchive(Self.sessionId)
        #expect(index.rows.first?.archived == true)
    }

    @Test func failedPinThenUndoRollsBackToTheServerAcknowledgedValue() {
        index.beginPin(Self.sessionId, pinned: true)
        index.beginPin(Self.sessionId, pinned: false)
        index.rollBackPin(Self.sessionId)
        #expect(index.rows.first?.pinned == false)
    }

    /// `finishMutation` hands the row back to remote truth — including dropping
    /// the baseline, so a later stray rollback can't rewind an acknowledged flip.
    @Test func finishedMutationDropsTheBaseline() {
        index.beginArchive(Self.sessionId, archived: true)
        index.finishMutation(Self.sessionId)
        index.rollBackArchive(Self.sessionId)
        #expect(index.rows.first?.archived == true)
    }

    @Test func hideRemovesTheRowAndRollbackRestoresIt() {
        index.beginHide(Self.sessionId)
        #expect(index.rows.isEmpty)
        #expect(index.pendingMutation(for: Self.sessionId) == .hidden)
        index.rollBackHide(Self.sessionId)
        #expect(index.rows.map(\.id) == [Self.sessionId])
        #expect(index.rows.first?.preview == "hello")
    }

    /// A stale undo toast must not overwrite a delete already in flight — the
    /// row is gone, and an archive intent would only confuse the pump.
    @Test func archiveAndPinNoOpWhileADeleteIsInFlight() {
        index.beginHide(Self.sessionId)
        index.beginArchive(Self.sessionId, archived: true)
        index.beginPin(Self.sessionId, pinned: true)
        #expect(index.pendingMutation(for: Self.sessionId) == .hidden)
        #expect(index.rows.isEmpty)
    }

    /// Rolling a hide back twice (a resolved-then-retried DELETE) must not
    /// duplicate the row.
    @Test func rollingBackAHideTwiceDoesNotDuplicateTheRow() {
        index.beginHide(Self.sessionId)
        index.rollBackHide(Self.sessionId)
        index.rollBackHide(Self.sessionId)
        #expect(index.rows.count == 1)
    }

    /// Every stage and every resolve moves the epoch — that is what lets `merge`
    /// spot a snapshot older than the mutation and drop it.
    @Test func everyStageAndResolveMovesTheEpoch() {
        let start = index.mutationEpoch
        index.beginArchive(Self.sessionId, archived: true)
        index.finishMutation(Self.sessionId)
        index.beginPin(Self.sessionId, pinned: true)
        index.rollBackPin(Self.sessionId)
        #expect(index.mutationEpoch == start + 4)
    }

    /// Rows and their flags survive the process: a second index over the same
    /// support dir reads what the first wrote.
    @Test func rowsPersistAcrossAReload() {
        index.setPinnedFlag(Self.sessionId, pinned: true)
        index.setArchivedFlag(Self.sessionId, archived: true)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.count == 1)
        #expect(reloaded.rows.first?.pinned == true)
        #expect(reloaded.rows.first?.archived == true)
        #expect(reloaded.rows.first?.preview == "hello")
    }

    /// The list's order: the pinned block first, then most recently active.
    @Test func sortedPutsThePinnedBlockFirst() {
        index.recordUserSend(sessionId: "newer", text: "second")
        index.setPinnedFlag(Self.sessionId, pinned: true)
        #expect(index.sorted.map(\.id) == [Self.sessionId, "newer"])
    }
}
