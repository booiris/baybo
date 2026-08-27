import Foundation
import Testing

@testable import Baybo

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

    private func query(_ text: String, caret: Int? = nil) -> IssueMentionQuery? {
        IssueMention.query(in: text, caret: caret ?? text.utf16.count)
    }

    @Test func anAtOpensAMentionWhereTheServerWouldReadOne() {
        #expect(query("@dev")?.prefix == "dev")
        #expect(query("please @dev")?.prefix == "dev")
        #expect(query("(@lea")?.prefix == "lea")
        #expect(query("a line\n@dev")?.prefix == "dev")
    }

    @Test func anAddressIsNotAMention() {
        #expect(query("mail me@dev") == nil)
        #expect(query("docs/x@lea") == nil)
    }

    @Test func somethingOutsideTheGrammarClosesIt() {
        #expect(query("@dev ") == nil)
        #expect(query("@Dev") == nil)
        #expect(query("@dev.") == nil)
        #expect(query("nothing typed yet") == nil)
    }

    @Test func aBareAtAsksForTheWholeRoster() {
        let open = query("look: @")
        #expect(open?.prefix == "")
        #expect(IssueMention.candidates(in: team, prefix: "", assignee: nil).count == team.count)
    }

    @Test func theCaretDecidesWhichMentionIsOpen() {
        let text = "@de and @qa-1 look"
        let open = query(text, caret: 3)

        #expect(open?.start == 0)
        #expect(open?.prefix == "de")
    }

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

    @Test func theCardsAssigneeLeadsTheStrip() {
        let offered = IssueMention.candidates(in: team, prefix: "", assignee: "a-doc")

        #expect(offered.first?.handle == "docs-1")
        #expect(offered.count == team.count, "leading is a reorder, never a filter")
        #expect(
            IssueMention.candidates(in: team, prefix: "", assignee: "a-gone").map(\.handle)
                == team.map(\.handle),
            "an assignee off this board leaves the roster alone")
    }

    @Test func completingLeavesExactlyOneSpaceBehindTheHandle() {
        #expect(complete("@de", "dev-1") == "@dev-1 ")
        #expect(complete("please @de", "dev-1") == "please @dev-1 ")
        #expect(complete("@de ", "dev-1", caret: 3) == "@dev-1 ", "the space already there is kept")
    }

    @Test func theRestOfTheDraftIsLeftAlone() {
        #expect(complete("@de look at this", "dev-1", caret: 3) == "@dev-1 look at this")
        #expect(complete("hi @qa, and @de", "dev-2") == "hi @qa, and @dev-2 ")
    }

    @Test func theEditAndTheDraftAreTheSameEdit() {
        guard let open = query("please @d") else {
            Issue.record("no mention open")
            return
        }
        let done = IssueMention.completion(for: open, handle: "dev-1", in: "please @d")

        #expect(done.draft == "please @dev-1 ")
        #expect(done.range == 7..<9, "the range covers the @ and what was typed of the handle")
        #expect(done.text == "@dev-1 ")
        let byHand = "please @d".prefix(7) + done.text + "please @d".dropFirst(9)
        #expect(String(byHand) == done.draft)
    }

    private func complete(_ text: String, _ handle: String, caret: Int? = nil) -> String? {
        guard let open = query(text, caret: caret) else { return nil }
        return IssueMention.completion(for: open, handle: handle, in: text).draft
    }
}
