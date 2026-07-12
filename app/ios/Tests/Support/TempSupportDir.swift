import Foundation

@testable import Baybo

/// One test's private Application Support root + `UserDefaults` suite.
///
/// `SessionIndex`, `TranscriptStore` and `OutboxStore` all hang off one support
/// root, and swift-testing runs suites in PARALLEL — without this every suite
/// would write the same `sessions.json` and one test's rows would surface as
/// another's logic bug.
final class TempSupportDir {
    let url: URL
    let defaults: UserDefaults
    private let suiteName: String

    init() {
        let id = UUID().uuidString
        suiteName = "baybo.tests.\(id)"
        url = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-tests", isDirectory: true)
            .appendingPathComponent(id, isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        defaults = UserDefaults(suiteName: suiteName)!
    }

    deinit {
        UserDefaults.standard.removePersistentDomain(forName: suiteName)
        try? FileManager.default.removeItem(at: url)
    }

    @MainActor
    func makeIndex() -> SessionIndex {
        SessionIndex(supportDirectory: url, defaults: defaults)
    }

    /// Pass a `TestClock`'s reader as `now` to move the retry machine's only
    /// time input by hand, rather than sleeping through the 10 s echo window.
    @MainActor
    func makeOutbox(sessionId: String, now: @escaping () -> Date = Date.init) -> OutboxStore {
        OutboxStore(
            sessionId: sessionId,
            directory: OutboxStore.directory(in: url),
            now: now)
    }
}

/// A settable clock for the outbox's echo window.
final class TestClock {
    private var current: Date

    init(_ start: Date = Date(timeIntervalSince1970: 1_000_000)) {
        current = start
    }

    var now: Date { current }

    func advance(_ seconds: TimeInterval) {
        current = current.addingTimeInterval(seconds)
    }
}

/// Poll `condition` until it holds or the budget runs out. The store's send /
/// sync / reconcile paths all detach a `Task`, so a test has to let the main
/// actor drain rather than assert on the next line.
@MainActor
func waitUntil(
    timeout: Duration = .seconds(2),
    _ condition: () -> Bool
) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: timeout)
    while ContinuousClock.now < deadline {
        if condition() { return true }
        try? await Task.sleep(for: .milliseconds(2))
    }
    return condition()
}
