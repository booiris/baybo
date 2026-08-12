import Foundation
import Testing

@testable import Baybo

/// The rename rules — the Swift half of a normalizer that exists three times
/// (here, `app/web/src/pages/chat/renameTitle.ts`, and the gateway's
/// `validate_session_title`, which is the authority). Every case below is one
/// the server would answer differently if this drifted: a 400 the user cannot
/// act on, or a title that rewrites itself the moment the endpoint's own
/// broadcast comes back.
@Suite
struct RenameTitleTests {
    /// The cap is the server's, counted the server's way. Swift's `count` is
    /// graphemes, Rust's `chars()` is scalars, and a flag emoji is the case that
    /// tells them apart — 2 scalars, 1 grapheme.
    @Test func capsAtTheServersLengthCountedInScalars() {
        #expect(RenameTitle.cap(String(repeating: "a", count: 90)).count == 80)
        #expect(RenameTitle.cap("短标题") == "短标题")

        let flags = String(repeating: "🇯🇵", count: 50)  // 100 scalars, 50 graphemes
        #expect(RenameTitle.cap(flags).unicodeScalars.count == 80)
    }

    /// A title renders on one line everywhere, so the server collapses interior
    /// whitespace instead of storing what it is handed. Paste a two-line title
    /// into the field and both sides must land on the same string.
    @Test func normalizedCollapsesInteriorWhitespaceAndTrims() {
        #expect(RenameTitle.normalized("  Fix   the\nlogin\tredirect  ") == "Fix the login redirect")
        #expect(RenameTitle.normalized("   ") == "")
        #expect(RenameTitle.normalized("\n\t") == "")
    }

    /// A blank field is not a rename. There is deliberately no "clear it and let
    /// the model re-title": an absent `SessionPatch.title` already means
    /// "unchanged" on the wire, so a cleared title has no representation.
    @Test func aBlankDraftCommitsNothing() {
        #expect(RenameTitle.toCommit(draft: "", seed: "Old") == nil)
        #expect(RenameTitle.toCommit(draft: "   \n ", seed: "Old") == nil)
    }

    /// Opened and closed without typing — including where the only difference is
    /// whitespace the server would have collapsed away anyway.
    @Test func anUntouchedDraftCommitsNothing() {
        #expect(RenameTitle.toCommit(draft: "Old name", seed: "Old name") == nil)
        #expect(RenameTitle.toCommit(draft: "  Old   name ", seed: "Old name") == nil)
    }

    @Test func aChangedDraftCommitsItsNormalizedForm() {
        #expect(RenameTitle.toCommit(draft: "  New   name ", seed: "Old") == "New name")
    }

    /// The seed for an untitled row is the user's last message, and committing it
    /// unchanged would rename the conversation to its own preview — *and* settle
    /// it against the auto-titler, which only ever writes into a session that has
    /// no title. Comparing against the seed rather than the stored title is what
    /// makes an untouched editor a no-op here.
    @Test func anUntitledRowSeedsFromItsLastMessageAndCommitsNothingUntouched() {
        let seed = RenameTitle.seed(title: nil, userText: "what is the answer")
        #expect(seed == "what is the answer")
        #expect(RenameTitle.toCommit(draft: seed, seed: seed) == nil)
    }

    /// A row with neither seeds an empty field, which is the one state where the
    /// commit button has nothing to enable for.
    @Test func aRowWithNothingToShowSeedsEmpty() {
        #expect(RenameTitle.seed(title: nil, userText: nil) == "")
    }

    /// A title minted server-side predates no such cap (a cron fire's is built
    /// from the job's name and the date). Seeding it whole would open the editor
    /// on a draft the server refuses even if the user changes nothing.
    @Test func anOverlongExistingTitleSeedsCapped() {
        let long = String(repeating: "台", count: 120)
        #expect(RenameTitle.seed(title: long, userText: nil).unicodeScalars.count == 80)
    }
}
