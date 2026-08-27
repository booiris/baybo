import SwiftUI

/// Shared layout constants for the Projects tab.
///
/// Deliberately its own values rather than reaching into `ChatListScreen`'s:
/// they happen to agree today because both are content gutters under the same
/// header, but the chat list's are that list's, and one screen widening
/// another's `fileprivate` is how two unrelated surfaces end up unable to move
/// independently.
enum ProjectsLayout {
    /// Content gutter, matching the chat list's row inset so the two tabs line
    /// up when the tab bar switches between them.
    static let gutter: CGFloat = 24
    /// Where content starts under the wordmark header's veil.
    static let topInset: CGFloat = 58
}
