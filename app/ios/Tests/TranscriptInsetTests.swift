import Testing
import UIKit
import WebKit

@testable import Baybo

@Suite @MainActor
struct TranscriptInsetTests {
    private let temp = TempSupportDir()

    private func makeStore(sessionId: String = "inset-test") -> ChatStore {
        let index = temp.makeIndex()
        index.touch(sessionId: sessionId)
        return ChatStore(
            sessionId: sessionId, client: FakeBayboClient(), index: index,
            outbox: temp.makeOutbox(sessionId: sessionId), supportDirectory: temp.url)
    }

    private func makeHost(store: ChatStore, hasComposer: Bool = true) -> TranscriptHost {
        let host = TranscriptHost(store: store, hasComposer: hasComposer)
        host.webView.stopLoading()
        return host
    }

    private func insets(_ host: TranscriptHost) -> [String] {
        host.bridge.pending.filter { $0.contains("setBottomInset") }
    }

    @Test func retargetWaitsForTheIncomingComposersOwnMeasurement() {
        let old = makeStore()
        let next = makeStore(sessionId: "next-session")
        let host = makeHost(store: old)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        host.bridge.setComposerTop(500, for: old)
        let sentBeforeRetarget = insets(host)

        host.bridge.retarget(to: next)
        #expect(host.bridge.pending.last { $0.contains("baybo.init(") }?
            .contains("\"bottomInset\":null") == true)
        #expect(insets(host) == sentBeforeRetarget)

        host.bridge.setComposerTop(680, for: next)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(120,\"next-session\");")
        let sentAfterMeasurement = insets(host)
        host.bridge.setComposerTop(490, for: old)
        host.bridge.detachCurrent(old)
        host.bridge.pushBottomInset()
        #expect(insets(host) == sentAfterMeasurement)
    }

    @Test func anIncomingMeasurementBeforeOnAppearSurvivesRetarget() {
        let old = makeStore()
        let next = makeStore(sessionId: "next-session")
        let host = makeHost(store: old)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        host.bridge.setComposerTop(500, for: old)
        host.bridge.setComposerTop(680, for: next)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(300,\"inset-test\");")

        host.bridge.retarget(to: next)
        #expect(host.bridge.pending.last { $0.contains("baybo.init(") }?
            .contains("\"bottomInset\":120") == true)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(120,\"next-session\");")
    }

    @Test func leavingAConversationDiscardsItsMeasuredGeometry() {
        let old = makeStore()
        let next = makeStore(sessionId: "next-session")
        let host = makeHost(store: old)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        host.bridge.setComposerTop(500, for: old)
        host.bridge.detachCurrent(old)
        host.bridge.retarget(to: next)
        host.bridge.retarget(to: old)
        #expect(host.bridge.pending.last { $0.contains("baybo.init(") }?
            .contains("\"bottomInset\":null") == true)
    }

    @Test func readOnlyHostHasAResolvedZeroInsetBeforeAttachment() {
        let store = makeStore()
        let host = makeHost(store: store, hasComposer: false)
        defer { host.teardown() }
        host.bridge.pushBottomInset()
        #expect(insets(host) == ["window.baybo && window.baybo.setBottomInset(0,\"inset-test\");"])
    }

    @Test func replacementInheritsGeometryBeforeItsComposerIsMounted() {
        let store = makeStore()
        let old = makeHost(store: store)
        let fresh = makeHost(store: store)
        defer {
            old.teardown()
            fresh.teardown()
        }
        old.bridge.setComposerTop(700, for: store)
        fresh.bridge.inheritComposerGeometry(from: old.bridge)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(fresh.webView)
        #expect(insets(fresh) == ["window.baybo && window.baybo.setBottomInset(100,\"inset-test\");"])
    }

    @Test func composerMeasuredBeforeWindowAttachmentIsReplayedOnAttachment() {
        let store = makeStore()
        let host = makeHost(store: store)
        defer { host.teardown() }
        host.bridge.setComposerTop(700, for: store)
        #expect(insets(host).isEmpty)

        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)

        #expect(insets(host) == ["window.baybo && window.baybo.setBottomInset(100,\"inset-test\");"])
    }

    @Test func composerMeasuredAfterWindowAttachmentUpdatesAndDeduplicates() {
        let store = makeStore()
        let host = makeHost(store: store)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        #expect(insets(host).isEmpty)

        host.bridge.setComposerTop(700, for: store)
        host.webView.setNeedsLayout()
        host.webView.layoutIfNeeded()
        host.bridge.setComposerTop(700, for: store)
        #expect(insets(host) == ["window.baybo && window.baybo.setBottomInset(100,\"inset-test\");"])

        host.bridge.setComposerTop(450, for: store)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(350,\"inset-test\");")
    }

    @Test func reattachmentUsesTheCurrentWindowHeightWithoutAnotherMeasurement() {
        let store = makeStore()
        let host = makeHost(store: store)
        defer { host.teardown() }
        let first = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        first.addSubview(host.webView)
        host.bridge.setComposerTop(700, for: store)
        host.webView.removeFromSuperview()

        let second = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 900))
        second.addSubview(host.webView)
        #expect(insets(host) == [
            "window.baybo && window.baybo.setBottomInset(100,\"inset-test\");",
            "window.baybo && window.baybo.setBottomInset(200,\"inset-test\");",
        ])
    }

    @Test func layoutRetriesWhenWindowHeightChanges() {
        let store = makeStore()
        let host = makeHost(store: store)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        window.addSubview(host.webView)
        host.bridge.setComposerTop(700, for: store)

        window.bounds.size.height = 900
        host.webView.setNeedsLayout()
        host.webView.layoutIfNeeded()
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(200,\"inset-test\");")
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
