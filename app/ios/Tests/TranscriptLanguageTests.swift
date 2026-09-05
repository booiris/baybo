import Foundation
import Testing

@testable import Baybo

/// The transcript is a webview, so every word inside it — the copy-confirm pill,
/// the work-block labels, the stopped indicator — is rendered by the web bundle's
/// own i18next, not by `Lang`. Native tells it which language to speak exactly
/// twice: in `deliverInit`, and through `setLanguage`.
///
/// `setLanguage` had NO callers, so the second channel did not exist, and the
/// first one is not enough: `retarget` skips `deliverInit` when the store is
/// unchanged, which is precisely what re-entering the SAME conversation does. So
/// a toggle re-rendered every native string on the screen and left the
/// transcript speaking the old language until the reader opened a different
/// chat. The bridge now watches `Lang` itself rather than each screen wiring it,
/// because three screens own a `TranscriptBridge` and only one of them would
/// have been remembered.
@Suite
@MainActor
struct TranscriptLanguageTests {
    private let temp = TempSupportDir()

    private func makeBridge(_ sessionId: String) -> TranscriptBridge {
        let index = temp.makeIndex()
        index.touch(sessionId: sessionId)
        let store = ChatStore(
            sessionId: sessionId, client: FakeBayboClient(), index: index,
            outbox: temp.makeOutbox(sessionId: sessionId),
            supportDirectory: temp.url)
        return TranscriptBridge(store: store)
    }

    /// Nothing is mounted yet, so the push lands in the pending buffer — which
    /// is the same thing the webview would receive a moment later.
    /// `Lang` is an app-wide singleton that persists to UserDefaults, and these
    /// cases move it — so every one puts it back. Cycling rather than toggling
    /// ONCE: `toggle()` is the only setter and it walks `Lang.supported`, so a
    /// single toggle only returns home while exactly two languages exist. The
    /// bound is what keeps a mistake here a failed test rather than a hung one.
    private func restoreLanguage(to original: String) {
        for _ in 0..<Lang.supported.count where Lang.shared.code != original {
            Lang.shared.toggle()
        }
    }

    private func languagePushes(_ bridge: TranscriptBridge) -> [String] {
        bridge.pending.filter { $0.contains("setLanguage") }
    }

    @Test func togglingTheLanguagePushesItIntoTheTranscript() {
        let bridge = makeBridge("lang-test")
        let original = Lang.shared.code
        defer { restoreLanguage(to: original) }

        #expect(languagePushes(bridge).isEmpty, "construction alone must push nothing")

        Lang.shared.toggle()
        let pushed = languagePushes(bridge)

        #expect(pushed.count == 1)
        #expect(pushed.first?.contains("\"\(Lang.shared.code)\"") == true)
    }

    /// The current value is what `deliverInit` already carries, so replaying it
    /// on subscribe would be a duplicate — and one that can arrive before the
    /// page exists. Only CHANGES are news.
    @Test func merelyBuildingABridgeSendsNoLanguage() {
        let bridge = makeBridge("lang-test-2")

        #expect(languagePushes(bridge).isEmpty)
    }

    /// The deck shell is the app's other long-lived webview and had the same
    /// hole from the other end: its push lived in `DeckContent.body`, which has
    /// not necessarily ever run — the shell is PREWARMED at home, so toggling
    /// before opening the Deck tab left it on the prewarmed language. Same rule,
    /// so the watch belongs in the same place: on the bridge.
    @Test func theDeckShellFollowsTheToggleWithoutItsScreenEverAppearing() {
        let bridge = DeckBridge()
        let original = Lang.shared.code
        defer { restoreLanguage(to: original) }

        #expect(bridge.pending.filter { $0.contains("setLanguage") }.isEmpty)

        Lang.shared.toggle()

        let pushed = bridge.pending.filter { $0.contains("setLanguage") }
        #expect(pushed.count == 1)
        #expect(pushed.first?.contains("\"\(Lang.shared.code)\"") == true)
    }
}
