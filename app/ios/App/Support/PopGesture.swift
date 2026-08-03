import SwiftUI
import UIKit

/// Re-enables UINavigationController's interactive pop (the edge-swipe back
/// gesture) while the system navigation bar is hidden — UIKit silently
/// disables the recognizer without a visible back button. Attached to the
/// pushed ChatScreen. Four invariants keep the hack safe:
/// - begin only past the stack root AND with no transition in flight;
/// - hand the recognizer back (delegate + enabled state) when this screen
///   leaves — a popped screen must not strand the list with an armed,
///   delegate-less recognizer (the delegate reference is weak and dies with
///   this host);
/// - order iOS 26's content-area pop recognizer behind the edge one via a
///   failure requirement (the use Apple documents for it), so one recognizer
///   drives any given swipe;
/// - clamp the gesture velocity the pop completion inherits
///   (`PopVelocityClamp`) — iOS 26's fluid pop seeds its settle spring from
///   `velocityInView:`, and a fast flick otherwise overshoots the revealed
///   list ~40pt past its resting edge and rubber-bands back (measured
///   +112px @3x at 4000pt/s; stock Settings shows the same, but on our
///   flat-paper screens it reads as a glitch).
///
/// A screen may also hand the edge swipe to something else for a while
/// (`EdgeSwipeOverride`) — see that type.
struct PopGestureEnabler: UIViewControllerRepresentable {
    /// What the left-edge swipe means while something is covering the
    /// conversation. `nil` on every screen that has nothing to cover it.
    var edgeOverride: EdgeSwipeOverride?

    init(edgeOverride: EdgeSwipeOverride? = nil) {
        self.edgeOverride = edgeOverride
    }

    func makeUIViewController(context: Context) -> PopGestureHostController {
        PopGestureHostController()
    }

    func updateUIViewController(_ controller: PopGestureHostController, context: Context) {
        controller.edgeOverride = edgeOverride
    }
}

/// Borrows the left edge from the interactive pop. While `active`, the pop is
/// held off and the swipe drives this instead — the chat's full-screen HTML
/// preview, whose own dismissal has to be what the gesture reaches first (a
/// swipe out of a full-screen sheet must leave the sheet, not the conversation
/// behind it).
///
/// Native decides the release, not the web side: `end(true)` means the drag
/// passed the distance or flick threshold. `move` carries the distance from the
/// edge, clamped at zero — the overlay follows the finger, so the render half
/// only ever applies the number.
struct EdgeSwipeOverride {
    let active: Bool
    let begin: () -> Void
    let move: (CGFloat) -> Void
    let end: (Bool) -> Void
}

/// Caps the x-velocity UIKit's pop-completion spring inherits from the
/// gesture: a fast flick otherwise overshoots the revealed list ~40pt past
/// its resting edge and rubber-bands back. The recognizer's concrete class is
/// private (`_UIParallaxTransitionPanGestureRecognizer` on iOS 26), so a
/// compile-time subclass can't cover it; instead a dynamic subclass of
/// whatever class the instance already has is registered at runtime with a
/// clamped `velocityInView:` override, and the instance is isa-swizzled onto
/// it. Adds no ivars, so the instance layout is untouched.
enum PopVelocityClamp {
    static let maxPopVelocity: CGFloat = 500
    private static let velocitySel = NSSelectorFromString("velocityInView:")

    static func install(on recognizer: UIGestureRecognizer) {
        guard let base = object_getClass(recognizer) else { return }
        let baseName = String(cString: class_getName(base))
        guard !baseName.hasPrefix("BayboClamped_") else { return }
        let name = "BayboClamped_" + baseName
        if let existing = NSClassFromString(name) {
            object_setClass(recognizer, existing)
            return
        }
        guard let method = class_getInstanceMethod(base, velocitySel),
              let sub = objc_allocateClassPair(base, name, 0)
        else {
            return
        }
        typealias VelocityFn = @convention(c) (AnyObject, Selector, UIView?) -> CGPoint
        let baseImp = unsafeBitCast(method_getImplementation(method), to: VelocityFn.self)
        let clamped: @convention(block) (AnyObject, UIView?) -> CGPoint = { object, view in
            var v = baseImp(object, velocitySel, view)
            v.x = min(v.x, maxPopVelocity)
            return v
        }
        class_addMethod(
            sub, velocitySel,
            imp_implementationWithBlock(clamped),
            method_getTypeEncoding(method)
        )
        objc_registerClassPair(sub)
        object_setClass(recognizer, sub)
    }
}

final class PopGestureHostController: UIViewController, UIGestureRecognizerDelegate {
    /// How far across the screen the drag must land, and how hard a flick
    /// counts regardless of distance, for a release to dismiss. Matched to the
    /// system pop so the two swipes commit at the same point under a finger.
    private static let dismissFraction: CGFloat = 0.32
    private static let dismissVelocity: CGFloat = 700

    private weak var nav: UINavigationController?
    private weak var priorDelegate: UIGestureRecognizerDelegate?
    private var priorEnabled = false
    /// The iOS 26 content-area recognizer's own enabled state, captured before
    /// an override ever touches it (it is ordered behind the edge one by a
    /// failure requirement, so refusing the edge swipe would otherwise HAND the
    /// pop to it — the exact outcome the override exists to prevent).
    private var priorContentEnabled = true
    /// Whether we actually switched it off. Restoring blind would mean every
    /// screen that never borrows the edge still writes `isEnabled` back on the
    /// way out, overruling whatever UIKit had decided since.
    private var contentPopSuppressed = false
    private var overrideRecognizerAttached = false

    var edgeOverride: EdgeSwipeOverride? {
        didSet { applyOverrideState() }
    }

    /// Our own edge recognizer rather than a repurposed pop: the pop recognizer
    /// is a transition driver whose whole output is "how far along is the
    /// pop" — there is no way to point it at something that isn't one.
    private lazy var overrideRecognizer: UIScreenEdgePanGestureRecognizer = {
        let recognizer = UIScreenEdgePanGestureRecognizer(
            target: self, action: #selector(handleOverrideSwipe))
        recognizer.edges = .left
        recognizer.delegate = self
        recognizer.isEnabled = false
        return recognizer
    }()

    override func viewDidLoad() {
        super.viewDidLoad()
        // Presence-only host: never intercept the touches it exists to enable.
        view.isUserInteractionEnabled = false
        view.backgroundColor = .clear
    }

    // viewWillAppear (not didMove): the parent chain reliably reaches the
    // navigation controller only once presentation starts. Also re-runs after
    // a cancelled pop rewinds, re-arming without re-capturing.
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        guard let nav = navigationController,
              let recognizer = nav.interactivePopGestureRecognizer
        else {
            return
        }
        self.nav = nav
        if recognizer.delegate !== self {
            // Two hosts can stack (archived list → chat). Re-appearing under a
            // peer must inherit the SYSTEM prior the first host captured — not
            // capture the peer itself, whose weak host dies with the popped
            // screen and would strand the recognizer armed and delegate-less
            // at the stack root. Peer inheritance also survives a popToRoot
            // double-pop, where the top host restores straight to the system
            // delegate.
            var inheritedContentEnabled: Bool?
            if let peer = recognizer.delegate as? PopGestureHostController {
                priorDelegate = peer.priorDelegate
                priorEnabled = peer.priorEnabled
                // The content recognizer follows the SAME rule, for the same
                // reason. A peer that currently has the edge borrowed has
                // already switched it off, and appearance order hands it over
                // in exactly that state — the incoming screen's
                // `viewWillAppear` runs before the outgoing one's
                // `viewDidDisappear` on a same-depth `chatPath` replace (a
                // push tap). Reading it live there would adopt the peer's
                // mutation as the SYSTEM value, and writing that back on the
                // way out leaves the content pop dead for the whole nav
                // stack's life.
                inheritedContentEnabled = peer.priorContentEnabled
            } else {
                priorDelegate = recognizer.delegate
                priorEnabled = recognizer.isEnabled
            }
            // iOS 26's content-area pop recognizer doesn't exist earlier —
            // there the edge recognizer alone carries the whole feature.
            if #available(iOS 26.0, *) {
                nav.interactiveContentPopGestureRecognizer?.require(toFail: recognizer)
                priorContentEnabled =
                    inheritedContentEnabled
                    ?? (nav.interactiveContentPopGestureRecognizer?.isEnabled ?? true)
            }
        }
        // On the NAVIGATION controller's view, where the system's own edge
        // recognizer lives: a recognizer sees every touch delivered anywhere in
        // its view's subtree, which is what lets it read a swipe that lands on
        // web content. This host's own view is zero-sized and takes no touches.
        if !overrideRecognizerAttached {
            nav.view.addGestureRecognizer(overrideRecognizer)
            overrideRecognizerAttached = true
        }
        // The clamp exists for iOS 26's fluid pop, whose settle spring seeds
        // from `velocityInView:` and overshoots on fast flicks. Earlier pops
        // don't overshoot — and UIKit's own finish/completion math reads the
        // same velocity — so pre-26 the system value stays untouched.
        if #available(iOS 26.0, *) {
            PopVelocityClamp.install(on: recognizer)
            if let contentPop = nav.interactiveContentPopGestureRecognizer {
                PopVelocityClamp.install(on: contentPop)
            }
        }
        recognizer.delegate = self
        recognizer.isEnabled = true
        // Last: a re-appear (a cancelled pop rewinding) must not re-arm the pop
        // under an overlay that had borrowed the edge.
        applyOverrideState()
    }

    // viewDidDisappear, not -WillDisappear: the will- callback fires when an
    // interactive pop merely STARTS, and restoring there would disarm the
    // recognizer mid-gesture. self.navigationController is already nil by
    // did-; use the reference captured on appear. Guarded on identity so a
    // delegate installed by someone else is never stomped.
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        // Unconditionally ours, unlike the shared pop recognizer below: it was
        // added by this host and a leftover would keep answering swipes on a
        // screen that is gone.
        if overrideRecognizerAttached {
            nav?.view.removeGestureRecognizer(overrideRecognizer)
            overrideRecognizerAttached = false
        }
        // Ahead of the delegate guard: this is our own mutation to undo, and a
        // screen torn down while the edge was still borrowed must not leave the
        // content pop switched off behind it.
        restoreContentPop()
        guard let recognizer = nav?.interactivePopGestureRecognizer,
              recognizer.delegate === self
        else {
            return
        }
        recognizer.delegate = priorDelegate
        recognizer.isEnabled = priorEnabled
    }

    /// Arm exactly one of the two readings of a left-edge swipe. Both pop
    /// recognizers go down together: on iOS 26 the content-area one is ordered
    /// behind the edge one by a failure requirement, so leaving it armed would
    /// simply pass the pop along once the edge recognizer stepped aside.
    private func applyOverrideState() {
        let borrowed = edgeOverride?.active == true
        overrideRecognizer.isEnabled = borrowed
        guard let nav, let recognizer = nav.interactivePopGestureRecognizer,
              recognizer.delegate === self
        else {
            return
        }
        recognizer.isEnabled = !borrowed
        guard borrowed else {
            restoreContentPop()
            return
        }
        if #available(iOS 26.0, *) {
            nav.interactiveContentPopGestureRecognizer?.isEnabled = false
            contentPopSuppressed = true
        }
    }

    private func restoreContentPop() {
        guard contentPopSuppressed else { return }
        contentPopSuppressed = false
        if #available(iOS 26.0, *) {
            nav?.interactiveContentPopGestureRecognizer?.isEnabled = priorContentEnabled
        }
    }

    @objc private func handleOverrideSwipe(_ recognizer: UIScreenEdgePanGestureRecognizer) {
        guard let edgeOverride, edgeOverride.active else { return }
        let view = recognizer.view
        // Rightward only. A finger that pulls back past the edge would otherwise
        // push the overlay off its own leading side.
        let travelled = max(0, recognizer.translation(in: view).x)
        switch recognizer.state {
        case .began:
            edgeOverride.begin()
        case .changed:
            edgeOverride.move(travelled)
        case .ended:
            let width = view?.bounds.width ?? 0
            let flicked = recognizer.velocity(in: view).x > Self.dismissVelocity
            edgeOverride.end(flicked || travelled > width * Self.dismissFraction)
        case .cancelled, .failed:
            edgeOverride.end(false)
        default:
            break
        }
    }

    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        if gestureRecognizer === overrideRecognizer {
            return edgeOverride?.active == true
        }
        guard let nav else { return false }
        return nav.viewControllers.count > 1 && nav.transitionCoordinator == nil
    }
}
