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
struct PopGestureEnabler: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> PopGestureHostController {
        PopGestureHostController()
    }

    func updateUIViewController(_ controller: PopGestureHostController, context: Context) {}
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
    private weak var nav: UINavigationController?
    private weak var priorDelegate: UIGestureRecognizerDelegate?
    private var priorEnabled = false

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
            priorDelegate = recognizer.delegate
            priorEnabled = recognizer.isEnabled
            nav.interactiveContentPopGestureRecognizer?.require(toFail: recognizer)
        }
        PopVelocityClamp.install(on: recognizer)
        if let contentPop = nav.interactiveContentPopGestureRecognizer {
            PopVelocityClamp.install(on: contentPop)
        }
        recognizer.delegate = self
        recognizer.isEnabled = true
    }

    // viewDidDisappear, not -WillDisappear: the will- callback fires when an
    // interactive pop merely STARTS, and restoring there would disarm the
    // recognizer mid-gesture. self.navigationController is already nil by
    // did-; use the reference captured on appear. Guarded on identity so a
    // delegate installed by someone else is never stomped.
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        guard let recognizer = nav?.interactivePopGestureRecognizer,
              recognizer.delegate === self
        else {
            return
        }
        recognizer.delegate = priorDelegate
        recognizer.isEnabled = priorEnabled
    }

    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard let nav else { return false }
        return nav.viewControllers.count > 1 && nav.transitionCoordinator == nil
    }
}
