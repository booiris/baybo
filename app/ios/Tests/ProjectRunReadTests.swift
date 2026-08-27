import Foundation
import Testing

@testable import Baybo

@MainActor
struct ProjectRunReadTests {
    @Test func onlyAnUnsettledRunKeepsThePageLive() {
        #expect(ProjectRunReadStore.isLive(.queued))
        #expect(ProjectRunReadStore.isLive(.held))
        #expect(ProjectRunReadStore.isLive(.running))
        #expect(!ProjectRunReadStore.isLive(.done))
        #expect(!ProjectRunReadStore.isLive(.failed))
        #expect(!ProjectRunReadStore.isLive(.cancelled))
        #expect(!ProjectRunReadStore.isLive(.unknown))
    }

    /// Initial sync and backward history unwind different web guards; returning
    /// the wrong frame kind leaves the transcript permanently loading.
    @Test func theFirstPageTakesTheBaselineDoorAndAScrollUpTheHistoryOne() async {
        let fake = FakeBayboClient()
        fake.stubRunBaselineJson = #"{"kind":"sync_page","rows":[]}"#
        fake.stubRunHistoryJson = #"{"kind":"history_page","rows":[]}"#
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .done, client: fake)

        store.requestSync(sinceOrdinal: nil, limit: 80)
        await settle { fake.runBaselineAsks == 1 }
        #expect(fake.runHistoryAsks.isEmpty, "the first page must not come off the history door")

        store.fetchHistory(beforeOrdinal: 4, limit: 80)
        await settle { fake.runHistoryAsks == [4] }
        #expect(fake.runBaselineAsks == 1, "a scroll-up must not re-read the newest page")
    }

    @Test func aScrollUpWithNoCursorIsStillAScrollUp() async {
        let fake = FakeBayboClient()
        fake.stubRunHistoryJson = #"{"kind":"history_page","rows":[]}"#
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .done, client: fake)

        store.fetchHistory(beforeOrdinal: nil, limit: 80)
        await settle { fake.runHistoryAsks == [nil] }
        #expect(fake.runBaselineAsks == 0)
    }

    private func settle(
        until done: () -> Bool, file: StaticString = #filePath, line: UInt = #line
    ) async {
        for _ in 0..<200 {
            if done() { return }
            try? await Task.sleep(for: .milliseconds(5))
        }
        Issue.record("the store never made its call")
    }

    @Test func aRunIsNeverMirroredAndNeverListed() {
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .running,
            client: FakeBayboClient())
        #expect(!store.mirrored)
        #expect(!store.listed)
        #expect(store.expandsUnansweredTail)
        #expect(store.connEpoch == 0)
        #expect(store.sessionId == "s")
    }
}

@MainActor
struct ProjectInvalidationTests {
    @Test func everyObserverHearsAPublish() {
        let bus = ProjectInvalidations.shared
        var heard: [String] = []
        let a = bus.observe { heard.append("a:\($0.scope)") }
        let b = bus.observe { heard.append("b:\($0.scope)") }
        bus.publish(projectId: "p1", scope: "board", issueNumber: nil)
        #expect(heard.sorted() == ["a:board", "b:board"])
        _ = a
        _ = b
    }

    @Test func droppingTheTokenStopsTheDelivery() {
        let bus = ProjectInvalidations.shared
        var count = 0
        do {
            let token = bus.observe { _ in count += 1 }
            bus.publish(projectId: "p1", scope: "run", issueNumber: 1)
            _ = token
        }
        bus.publish(projectId: "p1", scope: "run", issueNumber: 1)
        #expect(count == 1, "the second publish should reach nobody")
    }

    @Test func staleNamesNoBoard() {
        let bus = ProjectInvalidations.shared
        var seen: ProjectInvalidations.Change?
        let token = bus.observe { seen = $0 }
        bus.publishStale()
        #expect(seen?.scope == "stale")
        #expect(seen?.projectId == "")
        #expect(seen?.issueNumber == nil)
        _ = token
    }
}
