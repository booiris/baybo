import XCTest

/// The draft as the user meets it: type, walk away, come back, it is still
/// there — and the same for a NEW chat, whose session id exists nowhere but on
/// this device.
///
/// This tier is the only one that can make that claim. `ComposerDraftTests`
/// drives `ComposerStaging` directly and proves the record round-trips; what it
/// cannot see is the field itself, which is the whole feature — the text used to
/// be `ComposerView`'s `@State`, so it died with a frame the chat tears down for
/// reasons that have nothing to do with the user.
///
/// **Run this on a FRESH simulator** (`docs/testing.md`), and not only for the
/// reason the rest of the suite says. `-baybo-open-chat` opens an unlisted
/// session on whatever binding the device HAS, so on a paired simulator the list
/// merge behind the compose tap pulls in the owner's real conversations — and
/// `debug-session` among them the moment anything has ever sent from it, at
/// which point it is a listed row and compose correctly refuses to resume it.
/// Nothing here sends, deliberately: the send half of the draft's lifecycle is
/// `ComposerDraftTests.sendingDeletesTheDraft`, at a tier with no gateway in
/// reach. A UI test must not put a message in someone's real assistant.
final class ComposerDraftUITests: BayboUITestCase {
    private static let backLabel = "Back to conversations"
    private static let composeLabel = "New chat"
    private static let draft = "half a thought"
    /// What a SwiftUI text field reports as its `value` when it is empty.
    private static let placeholder = "Message…"

    /// Back out of a conversation with something typed, then return to it.
    ///
    /// `-baybo-open-chat` opens an UNLISTED session, so backing out lands on an
    /// empty list and the compose button is the way back in — which is exactly
    /// the second half of the feature: an unsent draft session has no registry
    /// row and no gateway row, so compose has to RESUME it rather than mint a
    /// fresh uuid and strand what was typed somewhere unreachable.
    func testTypedTextSurvivesLeavingAndComposeReturnsToIt() {
        let app = launch(["-baybo-open-chat"])
        let field = composerField(app)
        field.tap()
        field.typeText(Self.draft)
        XCTAssertEqual(restoredValue(app), Self.draft, "the field must carry what was typed")

        app.buttons[Self.backLabel].tap()
        let compose = app.buttons[Self.composeLabel].firstMatch
        XCTAssertTrue(compose.waitForExistence(timeout: 10), "the chat list must come up")
        compose.tap()

        XCTAssertEqual(
            restoredValue(app), Self.draft,
            "compose minted a new session instead of resuming the unsent draft")
        attachScreenshot(app, name: "composer-draft-restored")
    }

    /// The mirror image: typing and then CLEARING the field retires the draft,
    /// so the next compose opens a clean chat instead of resuming a husk. It is
    /// what keeps a dead new-chat session from being reachable forever.
    ///
    /// Cleared by backspacing rather than Select All: the system edit menu's
    /// selection is what made `RenameMenuUITests` type its input onto the END of
    /// the seed instead of over it.
    func testClearingTheFieldRetiresTheDraft() {
        let app = launch(["-baybo-open-chat"])
        let field = composerField(app)
        field.tap()
        field.typeText(Self.draft)
        XCTAssertEqual(restoredValue(app), Self.draft)

        field.typeText(
            String(repeating: XCUIKeyboardKey.delete.rawValue, count: Self.draft.count))
        XCTAssertTrue(isEmpty(restoredValue(app)), "the field must be empty before we leave")

        app.buttons[Self.backLabel].tap()
        let compose = app.buttons[Self.composeLabel].firstMatch
        XCTAssertTrue(compose.waitForExistence(timeout: 10), "the chat list must come up")
        compose.tap()

        XCTAssertTrue(
            isEmpty(restoredValue(app)),
            "a cleared composer must not leave a draft for the next compose to resume")
    }

    private func restoredValue(_ app: XCUIApplication) -> String? {
        composerField(app).value as? String
    }

    /// An empty SwiftUI field reports `""` before it has ever been edited and
    /// its PLACEHOLDER afterwards — both mean "nothing typed", and which one
    /// shows up is not something to assert on.
    private func isEmpty(_ value: String?) -> Bool {
        (value ?? "").isEmpty || value == Self.placeholder
    }

    /// By IDENTIFIER, not by type or value. The element type SwiftUI backs a
    /// vertical `TextField` with is not contractual (`ComposerAttachUITests`
    /// takes whichever of the two is there), and the value is the very thing
    /// these cases assert about — so `ComposerView.fieldIdentifier` is the only
    /// stable handle. Mirrors that constant; the app module is not importable
    /// here.
    private func composerField(_ app: XCUIApplication) -> XCUIElement {
        let field = app.descendants(matching: .any)["composer.field"].firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 15), "the composer field must exist")
        return field
    }

}
