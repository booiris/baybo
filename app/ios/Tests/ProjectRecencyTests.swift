import Foundation
import Testing

@testable import Baybo

/// The order the cards root shows boards in.
@MainActor
struct ProjectRecencyTests {
    private func project(_ id: String, archived: Bool = false) -> ProjectInfo {
        ProjectInfo(
            id: id, name: id, description: "", workdir: "/tmp/\(id)", dailyBudgetMicros: nil,
            dailyBudgetTokens: nil, maxParallelIssueRuns: 3, agentsMayMerge: false,
            archivedAtMs: archived ? 1 : nil, createdAtMs: 0, updatedAtMs: 0)
    }

    private func stamp(_ ms: Int64) -> Date {
        Date(timeIntervalSince1970: Double(ms) / 1000)
    }

    @Test func theMostRecentlyOpenedComesFirst() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("a", at: stamp(100))
        recency.record("b", at: stamp(300))
        recency.record("c", at: stamp(200))
        #expect(
            recency.ordered([project("a"), project("b"), project("c")]).map(\.id)
                == ["b", "c", "a"])
    }

    /// Opening one again moves it to the front. This is the whole feature.
    @Test func openingABoardAgainMovesItToTheFront() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("a", at: stamp(100))
        recency.record("b", at: stamp(200))
        recency.record("a", at: stamp(300))
        #expect(recency.ordered([project("a"), project("b")]).map(\.id) == ["a", "b"])
    }

    /// A board never opened HERE keeps the server's order among its peers and
    /// sits after the opened ones — rather than sorting as if it were opened at
    /// the epoch, which would interleave it by an answer nobody gave.
    @Test func neverOpenedBoardsKeepTheServersOrderAndFollowTheOpenedOnes() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("b", at: stamp(100))
        let ordered = recency.ordered(
            [project("x"), project("y"), project("b"), project("z")])
        #expect(ordered.map(\.id) == ["b", "x", "y", "z"])
    }

    /// A fresh install has no stamps at all, and the list must still be the
    /// server's rather than arbitrary.
    @Test func withNoStampsAtAllTheServersOrderSurvivesUntouched() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        let input = [project("p1"), project("p2"), project("p3")]
        #expect(recency.ordered(input).map(\.id) == ["p1", "p2", "p3"])
    }

    /// The stamps survive a relaunch — that is what "记在 ios 本地" means.
    @Test func theOrderSurvivesARelaunch() {
        let dir = TempSupportDir()
        let first = ProjectRecency(directory: dir.url)
        first.record("a", at: stamp(100))
        first.record("b", at: stamp(300))

        let second = ProjectRecency(directory: dir.url)
        #expect(second.ordered([project("a"), project("b")]).map(\.id) == ["b", "a"])
        #expect(second.lastOpened("b") == 300)
    }

    /// Logout takes the stamps with the boards: a project id that meant one
    /// board under this gateway means nothing under the next.
    @Test func removingTheMirrorTakesTheOrderWithIt() {
        let dir = TempSupportDir()
        let first = ProjectRecency(directory: dir.url)
        first.record("a", at: stamp(100))

        ProjectsStore.removeMirror(in: dir.url)

        let second = ProjectRecency(directory: dir.url)
        #expect(second.lastOpened("a") == nil)
    }

    /// A corrupt file costs the ORDER, never the list — this is on-disk JSON,
    /// not a trusted type.
    @Test func aCorruptFileLeavesTheListIntactAndUnordered() throws {
        let dir = TempSupportDir()
        try Data("not json at all".utf8).write(
            to: dir.url.appendingPathComponent(ProjectRecency.filename))
        let recency = ProjectRecency(directory: dir.url)
        #expect(recency.ordered([project("p1"), project("p2")]).map(\.id) == ["p1", "p2"])
    }

    /// The store is the one place that answers "what order", so the screen has
    /// one thing to ask and the live and archived blocks cannot drift.
    @Test func theStoreAppliesTheSameOrderItRecords() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        store.recordOpened("b")
        #expect(store.inRecencyOrder([project("a"), project("b")]).map(\.id) == ["b", "a"])
    }

    /// An empty id is not a board and must not take a slot in the map.
    @Test func anEmptyIdIsNeverStamped() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("")
        #expect(recency.lastOpened("") == nil)
    }
}
