import Testing
import UIKit

@testable import Baybo

/// The two reaches into UIKit that the card's mention strip stands on: where
/// the caret is, and writing over part of the document without touching the
/// rest.
///
/// Worth a tier of their own because **the surface above them cannot tell
/// whether they worked.** A completion typed at the end of a draft looks
/// identical whether it went through the document or through SwiftUI's
/// binding, so `IssueDockUITests` would stay green on a `replace` that never
/// found a responder and a `caretOffset` that always answered nil. What breaks
/// then is only the case a UI test cannot reach — a completion landing in the
/// middle of a sentence, where the binding path throws the caret to the end.
///
/// `.serialized`, and the window is torn down on the way out: the first
/// responder is process-global, and swift-testing runs suites in parallel.
@Suite(.serialized) @MainActor
struct FocusedTextInputTests {
    /// A real field, focused, in a key window — the probe walks the responder
    /// chain, so nothing less will do.
    ///
    /// The window is built ON THE HOST'S SCENE: a `UIWindow` with no
    /// `windowScene` is in no hierarchy, cannot become key, and its field's
    /// `becomeFirstResponder` fails — which reads exactly like the probe being
    /// broken.
    private func focused(_ text: String, caret: Int) -> (UIWindow, UITextView) {
        let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene
        let window = scene.map { UIWindow(windowScene: $0) } ?? UIWindow()
        window.frame = CGRect(x: 0, y: 0, width: 320, height: 120)
        let field = UITextView(frame: window.bounds)
        window.addSubview(field)
        window.makeKeyAndVisible()
        #expect(field.becomeFirstResponder(), "the test's own field never took focus")
        field.text = text
        field.selectedRange = NSRange(location: caret, length: 0)
        return (window, field)
    }

    /// Hand the host its window back: a suite that leaves a key window over
    /// the app changes what every later suite's probe finds.
    private func close(_ window: UIWindow, _ field: UITextView) {
        _ = field.resignFirstResponder()
        window.isHidden = true
        (UIApplication.shared.connectedScenes.first as? UIWindowScene)?
            .windows.first { $0 !== window }?.makeKeyAndVisible()
    }

    /// In UTF-16 units, which is what `IssueMention` scans in — the emoji is
    /// two of them and one Character, and the difference is where a completion
    /// would land.
    @Test func theCaretIsReadAsAUtf16Offset() {
        let (window, field) = focused("🙂 @de", caret: 6)
        defer { close(window, field) }

        #expect(FocusedTextInput.caretOffset == 6)
    }

    /// A selection reads as its right edge: that is where the next keystroke
    /// goes, so that is the mention being typed.
    @Test func aSelectionReadsAsItsRightEdge() {
        let (window, field) = focused("@dev-1 and more", caret: 0)
        defer { close(window, field) }
        field.selectedRange = NSRange(location: 1, length: 5)

        #expect(FocusedTextInput.caretOffset == 6)
    }

    /// **The whole reason the completion goes through the document.** The
    /// draft keeps its tail and the caret ends up behind what was written,
    /// where the operator was typing — a binding write would replace the
    /// string and put the caret at the end of `tail`.
    @Test func replacingLeavesTheTailAndTheCaretBehindTheInsert() {
        let (window, field) = focused("@de tail", caret: 3)
        defer { close(window, field) }

        #expect(FocusedTextInput.replace(0..<4, with: "@dev-1 "))
        #expect(field.text == "@dev-1 tail")
        #expect(FocusedTextInput.caretOffset == 7)
    }

    /// Nothing focused is an answer, not a crash: the dock falls back to its
    /// binding on a `false`.
    @Test func nothingFocusedIsSaidRatherThanGuessed() {
        #expect(FocusedTextInput.caretOffset == nil)
        #expect(FocusedTextInput.replace(0..<1, with: "x") == false)
    }
}
