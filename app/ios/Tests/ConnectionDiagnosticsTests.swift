import Foundation
import Testing

@testable import Baybo

@Suite @MainActor
struct ConnectionDiagnosticsTests {
    private final class Client: ConnectionDiagnosticsClient {
        var enabled: [Bool] = []
        var pending: [ConnectionLogEntry] = []
        func setConnectionDiagnostics(enabled: Bool) {
            self.enabled.append(enabled)
            pending.removeAll()
        }
        func drainConnectionDiagnostics() -> [ConnectionLogEntry] {
            defer { pending.removeAll() }
            return pending
        }
    }

    private func entry(_ sequence: UInt64) -> ConnectionLogEntry {
        ConnectionLogEntry(sequence: sequence, timestampMs: 0,
                           stage: .probe, message: "probe \(sequence)")
    }

    @Test func captureIsOptInAndLeavingStopsAndClearsIt() {
        let client = Client()
        let store = ConnectionDiagnosticsStore(client: client)
        store.poll()
        #expect(client.enabled.isEmpty)
        store.setEnabled(true)
        client.pending = [entry(1)]
        store.poll()
        #expect(store.entries.count == 1)
        store.stop()
        #expect(client.enabled == [true, false])
        #expect(!store.enabled)
        #expect(store.entries.isEmpty)
    }

    @Test func restartingAppendsUntilExplicitClear() {
        let client = Client()
        let store = ConnectionDiagnosticsStore(client: client)
        store.setEnabled(true)
        client.pending = [entry(1)]
        store.poll()
        client.pending = [entry(2)]
        store.setEnabled(false)
        #expect(store.text.contains("[probe] probe 1"))
        #expect(store.text.contains("[probe] probe 2"))
        store.setEnabled(true)
        client.pending = [entry(3)]
        store.poll()
        #expect(store.entries.map(\.sequence) == [1, 2, 3])
        client.pending = [entry(4)]
        store.clear()
        store.poll()
        #expect(store.entries.isEmpty)
    }

    @Test func consoleKeepsOnlyTheLatestEntriesWithoutDuplicates() {
        let client = Client()
        let store = ConnectionDiagnosticsStore(client: client)
        store.setEnabled(true)
        let limit = ConnectionDiagnosticsStore.maxEntries
        client.pending = (0..<(limit + 7)).map { entry(UInt64($0)) }
        store.poll()
        store.poll()
        #expect(store.entries.count == limit)
        #expect(store.entries.first?.sequence == 7)
        #expect(store.entries.last?.sequence == UInt64(limit + 6))
    }
}
