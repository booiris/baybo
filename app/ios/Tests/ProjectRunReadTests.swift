import Foundation
import Testing

@testable import Baybo

/// What a run's transcript target decides on its own.
@MainActor
struct ProjectRunReadTests {
    /// The poll's whole stop condition. `unknown` is NOT live, and that is the
    /// deliberate risk the FFI's own doc names — a future non-terminal status
    /// decodes here and freezes such a run's page until it is reopened.
    @Test func onlyAnUnsettledRunKeepsThePageLive() {
        #expect(ProjectRunReadStore.isLive(.queued))
        #expect(ProjectRunReadStore.isLive(.held))
        #expect(ProjectRunReadStore.isLive(.running))
        #expect(!ProjectRunReadStore.isLive(.done))
        #expect(!ProjectRunReadStore.isLive(.failed))
        #expect(!ProjectRunReadStore.isLive(.cancelled))
        #expect(!ProjectRunReadStore.isLive(.unknown))
    }

    /// The initial load and a scroll-up take DIFFERENT doors, and which door
    /// decides the frame kind the web receives.
    ///
    /// This is the whole of the "Loading conversation…" bug. The web arms a
    /// guard when it asks for a sync that only `sync_page` / `sync_failed`
    /// unwinds, and separately drops a `history_page` matching no in-flight
    /// backward-paging request — so answering the initial load off the history
    /// door lost the rows AND left the guard armed, and every run sheet sat on
    /// the loading line with its transcript already fetched. `gateway_api`'s
    /// own test pins what each door emits; this pins which one each caller
    /// takes, and neither half is meaningful alone.
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

    /// A first scroll-up has no cursor yet, so `nil` reaches the history door
    /// too — which is exactly why the door, and not the ordinal, is what tells
    /// the two apart. Reading the ordinal is how the failure frames came to be
    /// chosen wrongly as well.
    @Test func aScrollUpWithNoCursorIsStillAScrollUp() async {
        let fake = FakeBayboClient()
        fake.stubRunHistoryJson = #"{"kind":"history_page","rows":[]}"#
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .done, client: fake)

        store.fetchHistory(beforeOrdinal: nil, limit: 80)
        await settle { fake.runHistoryAsks == [nil] }
        #expect(fake.runBaselineAsks == 0)
    }

    /// Let the store's detached `Task` run. Polls rather than sleeping a fixed
    /// span so a slow machine does not turn into a flake.
    private func settle(
        until done: () -> Bool, file: StaticString = #filePath, line: UInt = #line
    ) async {
        for _ in 0..<200 {
            if done() { return }
            try? await Task.sleep(for: .milliseconds(5))
        }
        Issue.record("the store never made its call")
    }

    /// A run's transcript is a GET away, so it is never mirrored — a mirror is
    /// how a rendering the server no longer produces outlives the fix that
    /// removed it, with a cursor covering the thread so no sync can delete it.
    @Test func aRunIsNeverMirroredAndNeverListed() {
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .running,
            client: FakeBayboClient())
        #expect(!store.mirrored)
        #expect(!store.listed)
        #expect(store.expandsUnansweredTail)
        #expect(store.connEpoch == 0)
        // The React tree is keyed on it, so two attempts never share a tree.
        #expect(store.sessionId == "s")
    }
}

/// The seam a `ProjectChanged` frame travels along.
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

    /// Dropping the token unsubscribes. A screen's lifetime IS its
    /// subscription's, and an unsubscribe somebody has to remember is one that
    /// a `.onDisappear` racing a `deinit` eventually forgets.
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

    /// A dropped broadcast names no board — every observer reads it as
    /// "refetch whatever you are showing".
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
