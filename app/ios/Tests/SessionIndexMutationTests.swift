import Foundation
import Testing

@testable import Baybo

/// Optimistic archive / pin / delete, including the durable archive/hide queue.
///
/// The load-bearing case is the CHAINED failure: archive, then undo inside the
/// toast window, both requests dead offline. The latest desired value must stay
/// visible and survive a restart until the gateway acknowledges it.
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

    @Test func failedArchiveThenUndoKeepsTheLatestIntentQueued() {
        #expect(index.rows.first?.archived == false)
        index.beginArchive(Self.sessionId, archived: true)
        index.beginArchive(Self.sessionId, archived: false)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.first?.archived == false)
        #expect(reloaded.pendingMutation(for: Self.sessionId) == .archived(false))
    }

    @Test func failedPinThenUndoRollsBackToTheServerAcknowledgedValue() {
        index.beginPin(Self.sessionId, pinned: true)
        index.beginPin(Self.sessionId, pinned: false)
        index.rollBackPin(Self.sessionId)
        #expect(index.rows.first?.pinned == false)
    }

    @Test func hideRemovesTheRowAndKeepsTheIntentQueued() {
        index.beginHide(Self.sessionId)
        #expect(index.rows.isEmpty)
        #expect(index.pendingMutation(for: Self.sessionId) == .hidden)
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

    /// A cron group's delete queues every named fire in one local frame.
    @Test func batchHideRemovesAndQueuesEveryMember() {
        index.recordUserSend(sessionId: "s-2", text: "fire two")
        index.recordUserSend(sessionId: "s-3", text: "fire three")
        let members = [Self.sessionId, "s-2"]

        index.beginHideMany(members)
        #expect(index.rows.map(\.id) == ["s-3"])
        for id in members {
            #expect(index.pendingMutation(for: id) == .hidden)
        }

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.map(\.id) == ["s-3"])
        for id in members {
            #expect(reloaded.pendingMutation(for: id) == .hidden)
        }
    }

    /// The batch is the same staged intent as a single hide, so `merge` keeps
    /// suppressing every member's remote row until the POST resolves — and
    /// `finishHideMany` is what hands them all back to remote truth.
    @Test func finishingABatchHideClearsEveryStagedIntent() {
        index.recordUserSend(sessionId: "s-2", text: "fire two")
        let members = [Self.sessionId, "s-2"]
        index.beginHideMany(members)
        index.finishHideMany(members)
        for id in members {
            #expect(index.pendingMutation(for: id) == nil)
        }
        #expect(index.rows.isEmpty)
    }

    /// The dialog snapshots the members, so an id can go stale between prompt and
    /// tap (deleted from another client while the confirm was up). The batch must
    /// still stage the rest rather than trip over the row that is already gone.
    @Test func batchHideToleratesAnIdThatNoLongerHasARow() {
        index.beginHideMany([Self.sessionId, "s-never-existed"])
        #expect(index.rows.isEmpty)
        #expect(index.pendingMutation(for: "s-never-existed") == .hidden)
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

    @Test func pendingArchiveSurvivesAReloadAndKeepsItsOptimisticFlag() {
        index.beginArchive(Self.sessionId, archived: true)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.first?.archived == true)
        #expect(reloaded.pendingMutation(for: Self.sessionId) == .archived(true))
        #expect(reloaded.durableMutationSessionIds == [Self.sessionId])
    }

    @Test func pendingHideSurvivesAReloadAndKeepsTheRowRemoved() {
        index.beginHide(Self.sessionId)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.isEmpty)
        #expect(reloaded.pendingMutation(for: Self.sessionId) == .hidden)
        #expect(reloaded.durableMutationSessionIds == [Self.sessionId])
    }

    @Test func acknowledgedMutationDoesNotReturnAfterReload() {
        index.beginArchive(Self.sessionId, archived: true)
        index.finishMutation(Self.sessionId)

        let reloaded = temp.makeIndex()
        #expect(reloaded.rows.first?.archived == true)
        #expect(reloaded.pendingMutation(for: Self.sessionId) == nil)
        #expect(reloaded.durableMutationSessionIds.isEmpty)
    }

    /// The list's order: the pinned block first, then most recently active.
    @Test func sortedPutsThePinnedBlockFirst() {
        index.recordUserSend(sessionId: "newer", text: "second")
        index.setPinnedFlag(Self.sessionId, pinned: true)
        #expect(index.sorted.map(\.id) == [Self.sessionId, "newer"])
    }
}
