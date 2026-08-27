import Testing
import UIKit

@testable import Baybo

/// Serialized because the first responder and key window are process-global.
@Suite(.serialized) @MainActor
struct FocusedTextInputTests {
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

    private func close(_ window: UIWindow, _ field: UITextView) {
        _ = field.resignFirstResponder()
        window.isHidden = true
        (UIApplication.shared.connectedScenes.first as? UIWindowScene)?
            .windows.first { $0 !== window }?.makeKeyAndVisible()
    }

    @Test func theCaretIsReadAsAUtf16Offset() {
        let (window, field) = focused("🙂 @de", caret: 6)
        defer { close(window, field) }

        #expect(FocusedTextInput.caretOffset == 6)
    }

    @Test func aSelectionReadsAsItsRightEdge() {
        let (window, field) = focused("@dev-1 and more", caret: 0)
        defer { close(window, field) }
        field.selectedRange = NSRange(location: 1, length: 5)

        #expect(FocusedTextInput.caretOffset == 6)
    }

    @Test func replacingLeavesTheTailAndTheCaretBehindTheInsert() {
        let (window, field) = focused("@de tail", caret: 3)
        defer { close(window, field) }

        #expect(FocusedTextInput.replace(0..<4, with: "@dev-1 "))
        #expect(field.text == "@dev-1 tail")
        #expect(FocusedTextInput.caretOffset == 7)
    }

    @Test func aLiveCompositionIsCommittedBeforeTheWrite() {
        let (window, field) = focused("@", caret: 1)
        defer { close(window, field) }
        field.setMarkedText("d", selectedRange: NSRange(location: 1, length: 0))
        #expect(field.markedTextRange != nil, "the harness failed to open a composition")

        #expect(FocusedTextInput.replace(0..<2, with: "@dev-1 "))
        #expect(field.text == "@dev-1 ")
        #expect(field.markedTextRange == nil, "a composition left open re-commits after the write")
    }

    @Test func nothingFocusedIsSaidRatherThanGuessed() {
        #expect(FocusedTextInput.caretOffset == nil)
        #expect(FocusedTextInput.replace(0..<1, with: "x") == false)
    }
}
