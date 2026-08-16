import Foundation

@testable import Baybo

/// A recording stand-in for the shared transcript webview — `TranscriptBridge`
/// is the real conformer, and every method on `TranscriptSurface` is ultimately
/// a JS evaluation into a `WKWebView`. Standing one up is what a unit test
/// cannot do, and the re-seed's payload, ORDER and failed state are exactly what
/// has to be pinned, so the seam is recorded here instead.
@MainActor
final class FakeTranscriptSurface: TranscriptSurface {
    struct Seeded: Equatable {
        let msgId: String
        let text: String
        let attachments: [AttachmentRef]
    }

    struct Confirmed: Equatable {
        let msgId: String
        let ordinal: Int64?
    }

    private(set) var seeded: [Seeded] = []
    private(set) var failed: [String] = []
    /// Sends the store told the transcript it no longer owes — the return leg of
    /// `userSent`, and the only thing that lets a REPLACE stop overlaying a
    /// bubble. The ordinal is the durable row's, when the proof carried one.
    private(set) var confirmed: [Confirmed] = []
    /// Sessions the store asked to have rebuilt. Whether an ask becomes a page
    /// reload is `TranscriptBridge`'s call (it alone knows which conversation is
    /// mounted), so this records the ask, not the outcome.
    private(set) var rebuildAsks: [String] = []

    func userSent(msgId: String, text: String, attachments: [AttachmentRef]) {
        seeded.append(Seeded(msgId: msgId, text: text, attachments: attachments))
    }

    func sendFailed(_ msgId: String) {
        failed.append(msgId)
    }

    func sendConfirmed(_ msgId: String, ordinal: Int64?) {
        confirmed.append(Confirmed(msgId: msgId, ordinal: ordinal))
    }

    func rebuildIfShowing(_ sessionId: String) {
        rebuildAsks.append(sessionId)
    }
}
