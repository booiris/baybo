import Foundation
import Testing

@testable import Baybo

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

    @Test func openingABoardAgainMovesItToTheFront() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("a", at: stamp(100))
        recency.record("b", at: stamp(200))
        recency.record("a", at: stamp(300))
        #expect(recency.ordered([project("a"), project("b")]).map(\.id) == ["a", "b"])
    }

    @Test func neverOpenedBoardsKeepTheServersOrderAndFollowTheOpenedOnes() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("b", at: stamp(100))
        let ordered = recency.ordered(
            [project("x"), project("y"), project("b"), project("z")])
        #expect(ordered.map(\.id) == ["b", "x", "y", "z"])
    }

    @Test func withNoStampsAtAllTheServersOrderSurvivesUntouched() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        let input = [project("p1"), project("p2"), project("p3")]
        #expect(recency.ordered(input).map(\.id) == ["p1", "p2", "p3"])
    }

    @Test func theOrderSurvivesARelaunch() {
        let dir = TempSupportDir()
        let first = ProjectRecency(directory: dir.url)
        first.record("a", at: stamp(100))
        first.record("b", at: stamp(300))

        let second = ProjectRecency(directory: dir.url)
        #expect(second.ordered([project("a"), project("b")]).map(\.id) == ["b", "a"])
        #expect(second.lastOpened("b") == 300)
    }

    @Test func removingTheMirrorTakesTheOrderWithIt() {
        let dir = TempSupportDir()
        let first = ProjectRecency(directory: dir.url)
        first.record("a", at: stamp(100))

        ProjectsStore.removeMirror(in: dir.url)

        let second = ProjectRecency(directory: dir.url)
        #expect(second.lastOpened("a") == nil)
    }

    @Test func aCorruptFileLeavesTheListIntactAndUnordered() throws {
        let dir = TempSupportDir()
        try Data("not json at all".utf8).write(
            to: dir.url.appendingPathComponent(ProjectRecency.filename))
        let recency = ProjectRecency(directory: dir.url)
        #expect(recency.ordered([project("p1"), project("p2")]).map(\.id) == ["p1", "p2"])
    }

    @Test func theStoreAppliesTheSameOrderItRecords() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        store.recordOpened("b")
        #expect(store.inRecencyOrder([project("a"), project("b")]).map(\.id) == ["b", "a"])
    }

    @Test func anEmptyIdIsNeverStamped() {
        let dir = TempSupportDir()
        let recency = ProjectRecency(directory: dir.url)
        recency.record("")
        #expect(recency.lastOpened("") == nil)
    }
}
