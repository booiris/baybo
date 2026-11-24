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

    @Test func mountingANewComposerDoesNotTranslateTheTranscript() {
        let outgoing = makeStore()
        let incoming = makeStore(sessionId: "incoming")
        let host = makeHost(store: outgoing)
        defer { host.teardown() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        host.webView.frame = window.bounds
        window.addSubview(host.webView)
        let scroll = host.webView.scrollView
        scroll.contentSize = CGSize(width: 400, height: 1800)
        scroll.contentOffset.y = 1000

        host.bridge.setComposerTop(360, for: incoming)
        host.bridge.retarget(to: incoming)
        let dock = UIView(frame: CGRect(x: 0, y: 720, width: 400, height: 60))
        window.addSubview(dock)
        host.bridge.setComposerAnchor(dock, for: incoming)
        host.bridge.setComposerTop(dock.frame.minY, for: incoming)
        #expect(!host.bridge.contentVisible)
        #expect(scroll.layer.animationKeys()?.isEmpty != false)
        #expect(insets(host).last == "window.baybo && window.baybo.setBottomInset(80,\"incoming\");")
    }

    @Test func dismissingTheKeyboardLeavesAShortThreadInPlace() {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        let scroll = UIScrollView(frame: window.bounds)
        scroll.contentSize = window.bounds.size
        let dock = UIView(frame: CGRect(x: 0, y: 400, width: 400, height: 60))
        window.addSubview(scroll)
        window.addSubview(dock)
        let motion = TranscriptKeyboardMotion(scroll: scroll)
        defer { motion.reset() }
        motion.move(from: 400, to: 700, with: dock)
        #expect(scroll.contentOffset.y == 0)
        #expect(scroll.layer.animationKeys()?.isEmpty != false)
    }

    @Test func dismissalMovesAParkedReaderOnlyWhenTheShrinkingScrollRangeReachesThem() {
        guard !UIAccessibility.isReduceMotionEnabled else { return }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        let scroll = UIScrollView(frame: window.bounds)
        let dock = UIView(frame: CGRect(x: 0, y: 400, width: 400, height: 60))
        window.addSubview(scroll)
        window.addSubview(dock)
        scroll.contentSize = CGSize(width: 400, height: 1400)
        scroll.contentOffset.y = 440
        let motion = TranscriptKeyboardMotion(scroll: scroll)
        defer { motion.reset() }
        motion.move(from: 400, to: 700, with: dock)
        scroll.contentSize.height -= 300
        scroll.contentOffset.y = 300

        for (dockY, visibleOffset): (CGFloat, CGFloat) in [(430, 440), (600, 400), (700, 300)] {
            dock.frame.origin.y = dockY
            motion.move(from: 400, to: 700, with: dock)
            let correction = scroll.layer.animationKeys()?.compactMap { key -> CGFloat? in
                guard let animation = scroll.layer.animation(forKey: key) as? CABasicAnimation,
                      animation.keyPath == "bounds.origin.y" else { return nil }
                return animation.fromValue as? CGFloat
            }.first ?? 0
            #expect(scroll.contentOffset.y + correction == visibleOffset)
        }
    }

    @Test func repeatedKeyboardChangesUseTheDockAnimationAndPreserveTheWebScrollOffset() throws {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 400, height: 800))
        let scroll = UIScrollView(frame: window.bounds)
        let dock = UIView(frame: CGRect(x: 0, y: 700, width: 400, height: 60))
        window.addSubview(scroll)
        window.addSubview(dock)
        let anchor = UIView(frame: dock.bounds)
        dock.addSubview(anchor)
        scroll.contentSize = CGSize(width: scroll.bounds.width, height: scroll.bounds.height + 500)
        scroll.contentOffset.y = 500
        let motion = TranscriptKeyboardMotion(scroll: scroll)
        defer { motion.reset() }

        for newY: CGFloat in [400, 700, 400] {
            let oldY = dock.frame.minY
            let delta = oldY - newY
            dock.frame.origin.y = newY
            let spring = CASpringAnimation(keyPath: "position")
            spring.isAdditive = true
            spring.fromValue = CGPoint(x: 0, y: delta)
            spring.toValue = CGPoint.zero
            spring.duration = 0.4
            spring.beginTime = CACurrentMediaTime()
            dock.layer.add(spring, forKey: "position")
            // The composer gains 12 pt of clearance above the keyboard. UIKit
            // animates both its size and its center to keep its top on the dock.
            let oldHeightMinusNew: CGFloat = delta > 0 ? -12 : 12
            let size = try #require(spring.copy() as? CASpringAnimation)
            size.keyPath = "bounds.size"
            size.fromValue = CGSize(width: 0, height: oldHeightMinusNew)
            size.toValue = CGSize.zero
            anchor.layer.add(size, forKey: "bounds.size")
            let center = try #require(spring.copy() as? CASpringAnimation)
            center.fromValue = CGPoint(x: 0, y: oldHeightMinusNew * anchor.layer.anchorPoint.y)
            anchor.layer.add(center, forKey: "position")
            let originalOffset = scroll.contentOffset.y
            #expect(originalOffset == scroll.contentSize.height - scroll.bounds.height)
            motion.move(from: oldY, to: newY, with: anchor)
            #expect(scroll.contentOffset.y == originalOffset)
            if !UIAccessibility.isReduceMotionEnabled {
                let mirrors = try #require(scroll.layer.animationKeys()?.compactMap {
                    scroll.layer.animation(forKey: $0) as? CASpringAnimation
                }.filter { $0.keyPath == "bounds.origin.y" })
                #expect(!mirrors.isEmpty && mirrors.allSatisfy { $0.beginTime == spring.beginTime })
                let displacement = try mirrors.reduce(0.0) { total, animation in
                    let from = try #require((animation.fromValue as? NSNumber)?.doubleValue)
                    return total + from
                }
                #expect(abs(displacement + Double(delta)) < 0.01)
            }
            scroll.contentSize.height += delta
            scroll.contentOffset.y = scroll.contentSize.height - scroll.bounds.height
            motion.reset()
            #expect(scroll.layer.animationKeys()?.contains { scroll.layer.animation(forKey: $0) is CASpringAnimation } != true)
        }
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
