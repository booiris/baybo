import Foundation
import Testing

@testable import Baybo

/// Completing an `@handle` in a comment.
///
/// The grammar under test is not this file's invention: it is
/// `crates/project/src/mentions.rs`, mirrored so the strip offers a handle
/// exactly where the gateway will read one. Every case below that looks like a
/// curiosity — `me@dev-1`, `@Dev`, `(@lead` — is one of that suite's, kept in
/// step deliberately.
struct IssueMentionTests {
    private func member(_ id: String, _ handle: String) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: handle, name: handle, description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    private var team: [TeamMemberInfo] {
        [
            member("a-lead", "lead"), member("a-dev", "dev-1"), member("a-dev2", "dev-2"),
            member("a-doc", "docs-1"), member("a-qa", "qa-1"),
        ]
    }

    /// The caret is the whole question — a mention is open only while it is
    /// being typed, so every case here is "text plus where the cursor is".
    private func query(_ text: String, caret: Int? = nil) -> IssueMentionQuery? {
        IssueMention.query(in: text, caret: caret ?? text.utf16.count)
    }

    @Test func anAtOpensAMentionWhereTheServerWouldReadOne() {
        #expect(query("@dev")?.prefix == "dev")
        #expect(query("please @dev")?.prefix == "dev")
        #expect(query("(@lea")?.prefix == "lea")
        #expect(query("a line\n@dev")?.prefix == "dev")
    }

    /// The mention that pays for the grammar: an address is not a handle, and
    /// offering one here promises a delivery the gateway will not make.
    @Test func anAddressIsNotAMention() {
        #expect(query("mail me@dev") == nil)
        #expect(query("docs/x@lea") == nil)
    }

    /// Anything outside `[a-z0-9-]` between the `@` and the caret closes it —
    /// including the space that ends a finished handle, which is what stops
    /// the strip hanging over a comment that has moved on.
    @Test func somethingOutsideTheGrammarClosesIt() {
        #expect(query("@dev ") == nil)
        #expect(query("@Dev") == nil)
        #expect(query("@dev.") == nil)
        #expect(query("nothing typed yet") == nil)
    }

    /// A bare `@` is a request for the roster rather than a failed match.
    @Test func aBareAtAsksForTheWholeRoster() {
        let open = query("look: @")
        #expect(open?.prefix == "")
        #expect(IssueMention.candidates(in: team, prefix: "", assignee: nil).count == team.count)
    }

    /// Which mention is open is decided by the caret, not by the last one in
    /// the text — the operator can go back and finish an earlier one.
    @Test func theCaretDecidesWhichMentionIsOpen() {
        let text = "@de and @qa-1 look"
        let open = query(text, caret: 3)

        #expect(open?.start == 0)
        #expect(open?.prefix == "de")
    }

    /// **Offsets are UTF-16**, because they cross into `UITextInput`, which
    /// counts its positions the same way. Counted in Characters, an emoji
    /// earlier in the draft would shift the insert one place left per emoji.
    @Test func anEmojiDoesNotShiftWhereTheHandleLands() {
        let text = "🙂 @de"
        let open = query(text)

        #expect(open?.start == 3, "the emoji is two UTF-16 units, not one character")
        #expect(complete(text, "dev-1") == "🙂 @dev-1 ")
    }

    @Test func onlyTheHandlesThatCouldStillMatchAreOffered() {
        let offered = IssueMention.candidates(in: team, prefix: "d", assignee: nil)
        #expect(offered.map(\.handle) == ["dev-1", "dev-2", "docs-1"])
        #expect(IssueMention.candidates(in: team, prefix: "zz", assignee: nil).isEmpty)
    }

    /// The strip shows about three chips before it scrolls, so its order is
    /// what most operators will ever see — and the agent on the card is who a
    /// comment is most often addressed to.
    @Test func theCardsAssigneeLeadsTheStrip() {
        let offered = IssueMention.candidates(in: team, prefix: "", assignee: "a-doc")

        #expect(offered.first?.handle == "docs-1")
        #expect(offered.count == team.count, "leading is a reorder, never a filter")
        #expect(
            IssueMention.candidates(in: team, prefix: "", assignee: "a-gone").map(\.handle)
                == team.map(\.handle),
            "an assignee off this board leaves the roster alone")
    }

    /// The completion carries its own space: a handle run up against the next
    /// word is not a mention any more.
    @Test func completingLeavesExactlyOneSpaceBehindTheHandle() {
        #expect(complete("@de", "dev-1") == "@dev-1 ")
        #expect(complete("please @de", "dev-1") == "please @dev-1 ")
        #expect(complete("@de ", "dev-1", caret: 3) == "@dev-1 ", "the space already there is kept")
    }

    /// Only what was typed of the handle is replaced. The tail is somebody's
    /// half-written sentence.
    @Test func theRestOfTheDraftIsLeftAlone() {
        #expect(complete("@de look at this", "dev-1", caret: 3) == "@dev-1 look at this")
        #expect(complete("hi @qa, and @de", "dev-2") == "hi @qa, and @dev-2 ")
    }

    private func complete(_ text: String, _ handle: String, caret: Int? = nil) -> String? {
        guard let open = query(text, caret: caret) else { return nil }
        return IssueMention.applying(
            IssueMention.edit(for: open, handle: handle, in: text), to: text)
    }
}
