import UIKit

/// The focused text input, found over the responder chain.
///
/// Keyed on the `UITextInput` PROTOCOL, never a concrete `UITextView`/`UITextField`
/// class, so it survives SwiftUI's private field backing across iOS versions.
///
/// Extracted from `ComposerView`, which needs it to clear a live CJK composition
/// on send; `SearchScreen` needs the same window for the opposite reason — to
/// avoid searching for uncommitted pinyin. One probe, two readers.
enum FocusedTextInput {
    /// The current first responder, if it takes text.
    ///
    /// `sendAction(to: nil)` targets the first responder and nothing else, which
    /// is what makes this a probe rather than a search.
    static var current: UITextInput? {
        FirstResponderCapture.found = nil
        UIApplication.shared.sendAction(
            #selector(UIResponder.baybo_captureFirstResponder), to: nil, from: nil, for: nil)
        return FirstResponderCapture.found as? UITextInput
    }

    /// Whether an input method has an OPEN composition — the underlined,
    /// uncommitted syllables a CJK keyboard shows before a candidate is chosen.
    ///
    /// That text lives in the input session's marked range and is mirrored into
    /// SwiftUI's binding, so a naive `onChange` reacts to `shuju` on the way to
    /// 数据: a wasted round trip per word, and a "no matches" flash against
    /// pinyin that was never the query.
    static var isComposing: Bool {
        current?.markedTextRange != nil
    }
}

/// One-shot sink for the responder-chain probe above.
private enum FirstResponderCapture {
    static weak var found: UIResponder?
}

extension UIResponder {
    /// Action target for `sendAction(to: nil)`: only the current first responder
    /// receives it, so it records itself for `FocusedTextInput.current`.
    @objc fileprivate func baybo_captureFirstResponder() {
        FirstResponderCapture.found = self
    }
}
