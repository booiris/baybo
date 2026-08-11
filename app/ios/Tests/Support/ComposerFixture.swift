import Foundation
import Testing
import UIKit

@testable import Baybo

/// One staging machine over a fake core, plus the store whose notice line it
/// writes and the support root the two hang off (deleted with the fixture).
@MainActor
final class ComposerFixture {
    let temp = TempSupportDir()
    let client = FakeBayboClient()
    let pasteboard: FakePasteboard
    let store: ChatStore
    let staging: ComposerStaging

    init(pasteboard: FakePasteboard = FakePasteboard()) {
        self.pasteboard = pasteboard
        let sessionId = "s-compose"
        store = ChatStore(
            sessionId: sessionId, client: client, index: temp.makeIndex(),
            outbox: temp.makeOutbox(sessionId: sessionId), pasteboard: pasteboard)
        // The SESSION's strip, not one built beside it: which object the
        // composer renders — and what its lifetime is tied to — is half of
        // what these tests are about.
        staging = store.staging
    }

    /// A staged pick's spool path, read through a CALL so no copy of the item —
    /// and so no second reference to its `SpoolFile` — survives in the test's
    /// own scope to keep the file alive past what is being asserted.
    func spoolPath(_ index: Int = 0) -> String? {
        staging.staged[index].source?.url.path
    }

    func work(_ index: Int = 0) -> Task<Void, Never>? {
        staging.staged[index].work
    }
}
