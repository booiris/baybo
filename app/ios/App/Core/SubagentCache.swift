import Foundation

/// The last listing fetched for a conversation, so its sheet opens on content
/// instead of on a spinner.
///
/// `ChatScreen` already asks the gateway for this on every open — that is the
/// backstop that lights the header entry for a spawn which scrolled out of the
/// loaded window — and used to keep only `!items.isEmpty` from the answer. The
/// rows were right there and were thrown away, so opening the sheet paid a
/// second round trip to learn what the app already knew.
///
/// Deliberately NOT a persistent store: it is a within-session read-through
/// cache whose only job is the first paint. Everything it holds is re-fetched
/// by the sheet's own refresh a moment later, so a stale entry costs one frame,
/// never a wrong answer.
@MainActor
final class SubagentCache {
    static let shared = SubagentCache()

    private struct Entry {
        let items: [ChatSubagentSummary]
        let hasMoreOlder: Bool
    }

    /// Small on purpose. A reader visits a handful of conversations in a
    /// sitting, and every entry is a snapshot that the next open refreshes
    /// anyway — holding more would buy nothing and keep dead rows alive.
    private static let capacity = 8

    private var entries: [String: Entry] = [:]
    private var order: [String] = []

    func put(sessionId: String, items: [ChatSubagentSummary], hasMoreOlder: Bool) {
        entries[sessionId] = Entry(items: items, hasMoreOlder: hasMoreOlder)
        order.removeAll { $0 == sessionId }
        order.append(sessionId)
        while order.count > Self.capacity, let oldest = order.first {
            order.removeFirst()
            entries[oldest] = nil
        }
    }

    func seed(for sessionId: String) -> (items: [ChatSubagentSummary], hasMoreOlder: Bool)? {
        guard let entry = entries[sessionId] else { return nil }
        return (entry.items, entry.hasMoreOlder)
    }

    /// Logout / rebind: the next binding's sessions are a different world.
    func clear() {
        entries.removeAll()
        order.removeAll()
    }
}
