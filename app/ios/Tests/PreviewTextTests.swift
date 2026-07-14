import Foundation
import Testing

@testable import Baybo

/// `SessionIndex.previewText` is the Swift mirror of the gateway's
/// `truncate_preview`. A locally-captured preview (`recordUserSend` /
/// `recordAgentReply`) must equal the server's `last_message_text` byte for
/// byte, or the next `merge` swaps the string and the list's second line
/// visibly jumps.
///
/// Two things drift silently, and both are pinned here: the CAP (this shipped as
/// 200 — push's cap — while the chat list's is 120) and the COUNTING UNIT (Rust
/// `chars()` is unicode scalars; `String.count` is grapheme clusters, and one
/// emoji is enough to desync the two sides).
@Suite @MainActor
struct PreviewTextTests {
    @Test func capMatchesTheGatewaysChatListPreview() {
        #expect(SessionIndex.previewMaxChars == 120)
    }

    @Test func whitespaceIsCollapsed() {
        #expect(
            SessionIndex.previewText("hello\n\n  world\tagain \n") == "hello world again")
    }

    @Test func shortTextPassesThrough() {
        #expect(SessionIndex.previewText("what is the answer") == "what is the answer")
    }

    /// Exactly at the cap: no ellipsis. One past it: clipped to the cap plus the
    /// ellipsis.
    @Test func clipsAtTheCapWithAnEllipsis() {
        let atCap = String(repeating: "a", count: 120)
        #expect(SessionIndex.previewText(atCap) == atCap)

        let overCap = String(repeating: "a", count: 121)
        let clipped = SessionIndex.previewText(overCap)
        #expect(clipped == String(repeating: "a", count: 120) + "…")
    }

    /// 24 family emoji: 24 grapheme clusters but 120 unicode scalars (each is
    /// `man ZWJ woman ZWJ girl`). Rust counts the scalars, so this is exactly at
    /// the cap and must pass through untouched — while `String.count` would see
    /// 24 and agree only by accident.
    @Test func countsUnicodeScalarsLikeRustChars() {
        let family = "👨‍👩‍👧"
        #expect(family.unicodeScalars.count == 5)
        #expect(family.count == 1)

        let atCap = String(repeating: family, count: 24)
        #expect(atCap.unicodeScalars.count == 120)
        #expect(SessionIndex.previewText(atCap) == atCap)

        // 25 of them: 125 scalars — over the cap, even though `String.count` is
        // a comfortable 25. Clipping on graphemes would leave this untouched and
        // hand the server a preview it never agreed to.
        let overCap = String(repeating: family, count: 25)
        #expect(overCap.count == 25)
        let clipped = SessionIndex.previewText(overCap)
        #expect(clipped != overCap)
        #expect(clipped.unicodeScalars.count == 121, "120 scalars + the ellipsis")
    }

    /// An attachment-only send has no text; the caller keys off the empty string
    /// to KEEP the previous preview rather than blanking the row.
    @Test func emptyAndWhitespaceOnlyTextCollapseToEmpty() {
        #expect(SessionIndex.previewText("").isEmpty)
        #expect(SessionIndex.previewText("   \n\t ").isEmpty)
    }
}
