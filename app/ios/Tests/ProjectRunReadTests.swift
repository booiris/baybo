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

    /// A run's transcript is a GET away, so it is never mirrored — a mirror is
    /// how a rendering the server no longer produces outlives the fix that
    /// removed it, with a cursor covering the thread so no sync can delete it.
    @Test func aRunIsNeverMirroredAndNeverListed() {
        let store = ProjectRunReadStore(
            projectId: "p1", number: 3, attempt: 2, sessionId: "s", status: .running,
            client: FakeBayboClient())
        #expect(!store.mirrored)
        #expect(!store.listed)
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
