import Testing
import UIKit
import WebKit

@testable import Baybo

@Suite @MainActor
struct TranscriptInsetTests {
    private let temp = TempSupportDir()

    private func makeHost() -> TranscriptHost {
        let sessionId = "inset-test"
        let index = temp.makeIndex()
        index.touch(sessionId: sessionId)
        let store = ChatStore(
            sessionId: sessionId, client: FakeBayboClient(), index: index,
            outbox: temp.makeOutbox(sessionId: sessionId), supportDirectory: temp.url)
        let host = TranscriptHost(store: store)
        host.webView.stopLoading()
        return host
    }

    private func insets(_ host: TranscriptHost) -> [String] {
        host.bridge.pending.filter { $0.contains("setBottomInset") }
    }

    @Test func composerMeasuredBeforeWindowAttachmentIsReplayedOnAttachment() {
        let host = makeHost()
        defer { host.teardown() }
        host.bridge.setComposerTop(700)
        #expect(insets(host).isEmpty)

        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)

        #expect(insets(host) == ["window.baybo && window.baybo.setBottomInset(100);"])
    }

    @Test func composerMeasuredAfterWindowAttachmentUpdatesAndDeduplicates() {
        let host = makeHost()
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        #expect(insets(host).isEmpty)

        host.bridge.setComposerTop(700)
        host.webView.setNeedsLayout()
        host.webView.layoutIfNeeded()
        host.bridge.setComposerTop(700)
        #expect(insets(host) == ["window.baybo && window.baybo.setBottomInset(100);"])

        host.bridge.setComposerTop(450)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(350);")
    }

    @Test func reattachmentUsesTheCurrentWindowHeightWithoutAnotherMeasurement() {
        let host = makeHost()
        defer { host.teardown() }
        let first = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        first.addSubview(host.webView)
        host.bridge.setComposerTop(700)
        host.webView.removeFromSuperview()

        let second = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 900))
        second.addSubview(host.webView)
        #expect(insets(host) == [
            "window.baybo && window.baybo.setBottomInset(100);",
            "window.baybo && window.baybo.setBottomInset(200);",
        ])
    }

    @Test func layoutRetriesWhenWindowHeightChanges() {
        let host = makeHost()
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        host.bridge.setComposerTop(700)

        window.bounds.size.height = 900
        host.webView.setNeedsLayout()
        host.webView.layoutIfNeeded()
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(200);")
    }
}

@Suite @MainActor
struct IssueInsetTests {
    @Test func composerMeasuredBeforeWindowAttachmentIsReplayedOnAttachment() {
        let host = IssueHost()
        defer { host.teardown() }
        host.webView.stopLoading()
        host.bridge.setComposerTop(700)
        #expect(!host.bridge.pending.contains { $0.contains("setBottomInset") })

        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)

        #expect(host.bridge.pending.filter { $0.contains("setBottomInset") }
            == ["window.issuePage.setBottomInset(100);"])
    }
}
