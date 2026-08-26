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

    /// Empty the focused input's document, finalising any live IME composition
    /// FIRST.
    ///
    /// The composing syllables (underlined marked text / inline candidates)
    /// live in the input session's marked range — NOT in the SwiftUI binding —
    /// so a plain `text = ""` (sync or deferred) leaves them to re-commit on
    /// the next input turn and re-materialise after send: the intermittent
    /// "字没消失", worst under pinyin. `unmarkText()` commits, so the ordering
    /// matters; the document is then emptied imperatively so the reset cannot
    /// lose a race with the field's own edit up-sync. No responder is
    /// resigned — the keyboard stays up.
    ///
    /// Both docks call it, and the CALLER still empties its own binding after:
    /// this reaches the UIKit half only.
    static func clearDocument() {
        guard let input = current else { return }
        input.unmarkText()
        guard
            let range = input.textRange(from: input.beginningOfDocument, to: input.endOfDocument)
        else { return }
        input.replace(range, withText: "")
    }

    /// Where the caret is, as an offset from the start of the document.
    ///
    /// In UTF-16 code units: `UITextPosition` is backed by the field's text
    /// storage, which is an `NSString`. That is the same unit `IssueMention`
    /// scans in, and the reason it does.
    ///
    /// The END of the selection, so a caret is a caret and a selection reads
    /// as its right edge — which is where the next keystroke lands.
    static var caretOffset: Int? {
        guard let input = current, let selection = input.selectedTextRange else { return nil }
        return input.offset(from: input.beginningOfDocument, to: selection.end)
    }

    /// Replace a UTF-16 range of the focused input's document, leaving the
    /// caret behind what was written.
    ///
    /// Through the document rather than through SwiftUI's binding: a binding
    /// write replaces the whole string, and a `TextField` handed a new string
    /// puts the caret at the END of it — which moves the operator's cursor
    /// every time a completion lands mid-draft. Answers whether it reached a
    /// responder, so the caller can fall back to the binding.
    ///
    /// **Any live composition is committed first**, `clearDocument`'s scar
    /// applied to a write that is not a reset: uncommitted syllables live in
    /// the input session's marked range, and a range replaced around them
    /// leaves them to re-commit afterwards — the replacement AND the text it
    /// was meant to replace, both in the document. `unmarkText` commits, so
    /// the marked text lands where it already reads in the binding, which is
    /// what the caller measured its range against.
    @discardableResult
    static func replace(_ range: Range<Int>, with text: String) -> Bool {
        guard let input = current else { return false }
        input.unmarkText()
        guard
            let from = input.position(from: input.beginningOfDocument, offset: range.lowerBound),
            let to = input.position(from: input.beginningOfDocument, offset: range.upperBound),
            let target = input.textRange(from: from, to: to)
        else { return false }
        input.replace(target, withText: text)
        return true
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
