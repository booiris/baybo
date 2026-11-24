import SwiftUI

/// A native point in the same moving layout as the input field.
struct TranscriptComposerAnchor: UIViewRepresentable {
    let bridge: TranscriptBridge
    let store: ChatStore

    func makeUIView(context: Context) -> UIView {
        let view = UIView()
        view.isUserInteractionEnabled = false
        return view
    }

    func updateUIView(_ view: UIView, context: Context) {
        bridge.setComposerAnchor(view, for: store)
    }
}

@MainActor
final class TranscriptKeyboardMotion: NSObject {
    private weak var scroll: UIScrollView?
    private weak var anchor: UIView?
    private var offsets: NSKeyValueObservation?
    private var updates: UIUpdateLink?
    private var dockAnimationKeys: [String] = []
    private static let correctionKey = "baybo.composerScrollCorrection"
    private var correction: CGFloat = 0
    private var startTop: CGFloat = 0
    private var targetTop: CGFloat = 0
    private var startBottom: CGFloat?
    private var readingOffset: CGFloat?
    private var deadline: CFTimeInterval = 0

    init(scroll: UIScrollView) {
        self.scroll = scroll
        super.init()
        scroll.panGestureRecognizer.addTarget(self, action: #selector(panChanged))
        offsets = scroll.observe(\.contentOffset, options: [.old, .new]) { [weak self] _, change in
            guard change.oldValue?.y != change.newValue?.y else { return }
            MainActor.assumeIsolated { self?.synchronize() }
        }
    }

    func move(from old: CGFloat, to new: CGFloat, with anchor: UIView) {
        guard old != new, let scroll, let window = scroll.window, anchor.window === window,
              !scroll.isDragging, !scroll.isDecelerating, !UIAccessibility.isReduceMotionEnabled else { return }
        if updates == nil || self.anchor !== anchor {
            reset()
            let bottom = max(0, scroll.contentSize.height - scroll.bounds.height)
            let offset = scroll.contentOffset.y
            let atBottom = abs(bottom - offset) <= 1
            // Dismissing the keyboard can shrink the document past a reader
            // near its end. Hold their place until the moving edge reaches it.
            let newBottom = max(0, bottom + old - new)
            guard atBottom || (offset > newBottom && offset <= bottom) else { return }
            self.anchor = anchor
            startTop = old
            readingOffset = atBottom ? nil : offset
            // A document shorter than the screen only starts moving when the
            // composer actually reaches it. Its new scroll range tells us where.
            startBottom = bottom > 0 ? bottom : nil
            let link = UIUpdateLink(view: scroll)
            link.addAction(to: .beforeCATransactionCommit) { [weak self] _, _ in self?.synchronize() }
            link.requiresContinuousUpdates = true
            updates = link
            link.isEnabled = true
        }
        removeDockAnimations()
        targetTop = new
        // A detached/reloading WebContent process may never commit the inset.
        deadline = CACurrentMediaTime() + 1
        synchronize()
    }

    private func synchronize() {
        guard updates != nil else { return }
        guard let scroll, let window = scroll.window, let anchor, anchor.window === window,
              !scroll.isDragging, !scroll.isDecelerating, CACurrentMediaTime() < deadline else { reset(); return }
        let layer = anchor.layer.presentation() ?? anchor.layer
        let windowLayer = window.layer.presentation() ?? window.layer
        let visibleTop = layer.convert(layer.bounds.origin, to: windowLayer).y
        let bottom = max(0, scroll.contentSize.height - scroll.bounds.height)
        let restingOffset = startBottom.map { $0 + startTop - targetTop } ?? bottom
        if dockAnimationKeys.isEmpty, readingOffset == nil, startBottom != nil, restingOffset > 0,
           abs(anchor.convert(anchor.bounds.origin, to: window).y - targetTop) < 1 {
            mirrorDockAnimations(from: anchor.layer, onto: scroll.layer, remaining: visibleTop - targetTop)
        }
        let projected = max(0, restingOffset + targetTop - visibleTop)
        let visibleBottom = startBottom == nil ? min(bottom, projected) : projected
        let visibleOffset = min(readingOffset ?? visibleBottom, visibleBottom)
        if abs(visibleTop - targetTop) < 0.5 && abs(scroll.contentOffset.y - visibleOffset) < 0.5 &&
            abs(scroll.contentOffset.y - bottom) < 1 {
            reset()
        } else {
            translate(scroll.contentOffset.y - (dockAnimationKeys.isEmpty ? visibleOffset : restingOffset))
        }
    }

    // UIKit can commit the dock's spring before WebKit commits its new scroll
    // offset. The two layers must run on the same render-server timeline.
    private func mirrorDockAnimations(from anchor: CALayer, onto destination: CALayer, remaining: CGFloat) {
        var mirrors: [CABasicAnimation] = []
        var travel: CGFloat = 0
        for layer in sequence(first: anchor, next: { $0.superlayer }) {
            for key in layer.animationKeys() ?? [] {
                guard let animation = layer.animation(forKey: key) as? CABasicAnimation, animation.isAdditive,
                      let dy = topEdgeTravel(animation, on: layer), dy != 0,
                      let copy = animation.copy() as? CABasicAnimation else { continue }
                copy.delegate = nil
                copy.keyPath = "bounds.origin.y"
                copy.fromValue = -dy
                copy.toValue = 0
                if animation.beginTime != 0 {
                    copy.beginTime = destination.convertTime(animation.beginTime, from: layer)
                }
                mirrors.append(copy)
                travel += dy
            }
        }
        // SwiftUI can commit nested layers separately. Wait until the
        // animations account for the whole remaining displacement.
        guard abs(travel) + 1 >= abs(remaining), travel * remaining >= 0 else { return }
        for (index, mirror) in mirrors.enumerated() {
            let key = "baybo.composerPosition.\(index)"
            destination.add(mirror, forKey: key)
            dockAnimationKeys.append(key)
        }
    }

    private func topEdgeTravel(_ animation: CABasicAnimation, on layer: CALayer) -> CGFloat? {
        switch (animation.keyPath, animation.fromValue, animation.toValue) {
        case ("position", let from as CGPoint, let to as CGPoint) where to == .zero:
            return from.y
        case ("bounds.size", let from as CGSize, let to as CGSize) where to == .zero:
            // A layer resizes about its anchor. Its center can move while its top stays put.
            return -layer.anchorPoint.y * from.height
        default:
            return nil
        }
    }

    private func removeDockAnimations() {
        for key in dockAnimationKeys { scroll?.layer.removeAnimation(forKey: key) }
        dockAnimationKeys.removeAll()
    }

    private func translate(_ y: CGFloat) {
        guard let layer = scroll?.layer, correction != y else { return }
        correction = y
        guard y != 0 else { layer.removeAnimation(forKey: Self.correctionKey); return }
        let hold = CABasicAnimation(keyPath: "bounds.origin.y")
        hold.fromValue = -y
        hold.toValue = -y
        hold.isAdditive = true
        hold.duration = 0
        hold.fillMode = .both
        hold.isRemovedOnCompletion = false
        layer.add(hold, forKey: Self.correctionKey)
    }

    func reset() {
        updates?.isEnabled = false
        updates = nil
        removeDockAnimations()
        anchor = nil
        translate(0)
    }

    @objc private func panChanged(_ pan: UIPanGestureRecognizer) {
        guard pan.state == .began, updates != nil, let scroll else { return }
        let visible = scroll.layer.presentation()?.bounds.minY
        reset()
        if let visible {
            let top = -scroll.adjustedContentInset.top
            let bottom = max(top, scroll.contentSize.height - scroll.bounds.height + scroll.adjustedContentInset.bottom)
            scroll.setContentOffset(CGPoint(x: scroll.contentOffset.x, y: min(max(top, visible), bottom)), animated: false)
        }
    }
}
