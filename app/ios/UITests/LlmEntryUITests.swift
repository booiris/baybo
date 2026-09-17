import XCTest

/// The Settings → Models → entry editor path, driven headlessly off
/// `-baybo-demo-models`.
///
/// **What this can and cannot prove.** The demo catalog is seeded client-side,
/// so every READ on this path is real: the rows, their values, and the
/// inherited-vs-pinned distinction all come from the same code the gateway
/// feeds in production. A WRITE has no gateway behind it, so it fails at the
/// transport — which is still worth driving, because the failure has to arrive
/// on the outcome strip rather than as a silent no-op.
///
/// The demo entries are deliberately opposite (`ModelCatalog.seedDemoIfRequested`):
/// `claude` inherits everything, `gpt` pins every override and carries an
/// `apiKeyEnv` shadow.
final class LlmEntryUITests: BayboUITestCase {
    private func openModels() -> XCUIApplication {
        let app = launch(["-baybo-open-home", "-baybo-home-tab", "settings", "-baybo-demo-models"])
        let row = app.buttons["settings-models"]
        XCTAssertTrue(row.waitForExistence(timeout: 5), "the Settings Models row never appeared")
        // The trailing value is the default entry, and it is the row's
        // accessibility VALUE rather than part of its label — so it can change
        // under a by-label query without breaking it.
        XCTAssertEqual(row.value as? String, "claude")
        row.tap()
        return app
    }

    /// The list marks the default and says which entries have no resolvable
    /// key — the two facts that decide whether the operator needs to act.
    func testTheEntriesListMarksTheDefaultAndTheKeylessEntry() {
        let app = openModels()

        let claude = app.buttons["llm-entry-claude"]
        XCTAssertTrue(claude.waitForExistence(timeout: 5), "the entries list never appeared")
        XCTAssertEqual(claude.value as? String, "Default")

        let gpt = app.buttons["llm-entry-gpt"]
        XCTAssertTrue(gpt.exists)
        XCTAssertNotEqual(gpt.value as? String, "Default")
        XCTAssertTrue(gpt.label.contains("gpt"))
        attachScreenshot(app, name: "llm-entries")
    }

    /// The editor's whole state language is inherited vs pinned, and it is
    /// carried to VoiceOver in words because a colour step alone is not
    /// self-explanatory. `claude` inherits its context window; `gpt` pins one.
    func testTheEditorSaysWhichValuesArePinnedAndWhichAreInherited() {
        let app = openModels()

        app.buttons["llm-entry-claude"].tap()
        let context = app.buttons["llm-field-context"]
        XCTAssertTrue(context.waitForExistence(timeout: 5), "the entry editor never appeared")
        XCTAssertEqual(context.value as? String, "200000, inherited from the provider")
        XCTAssertEqual(
            app.buttons["llm-field-vision"].value as? String, "On, inherited from the provider")
        attachScreenshot(app, name: "llm-entry-inherited")

        app.buttons["llm-entry-back"].tap()
        app.buttons["llm-entry-gpt"].tap()
        let pinned = app.buttons["llm-field-context"]
        XCTAssertTrue(pinned.waitForExistence(timeout: 5))
        XCTAssertEqual(pinned.value as? String, "400000, set on this entry")
        attachScreenshot(app, name: "llm-entry-pinned")
    }

    /// Provider is the one row with no editor: the valid set lives only in the
    /// gateway's Rust registry and is on no HTTP route, so a free-text field
    /// here could silently drop the entry from the pool while the API kept
    /// listing it.
    func testProviderIsNotEditable() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        XCTAssertTrue(
            app.buttons["llm-field-model"].waitForExistence(timeout: 5),
            "the entry editor never appeared")

        XCTAssertFalse(
            app.buttons["llm-field-provider"].exists,
            "Provider must not be a tappable row")
        XCTAssertTrue(app.staticTexts["anthropic"].exists, "Provider must still be READABLE")
    }

    /// The seeding trap. The Context field is seeded with the EFFECTIVE window,
    /// so committing it untouched would manufacture an override out of an
    /// inherited value — pinning the model to a number that used to track the
    /// provider's snapshot. Save stays disabled until the digits actually change.
    func testSavingAnUntouchedContextWindowIsRefused() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        let context = app.buttons["llm-field-context"]
        XCTAssertTrue(context.waitForExistence(timeout: 5))
        context.tap()

        let save = app.buttons["llm-commit-context"]
        XCTAssertTrue(save.waitForExistence(timeout: 5), "the context editor never appeared")
        XCTAssertFalse(save.isEnabled, "an untouched effective value must not be committable")

        let field = app.textFields["llm-input-context"]
        XCTAssertTrue(field.waitForExistence(timeout: 3))
        field.tap()
        field.typeText("0")
        XCTAssertTrue(save.isEnabled, "a changed value must be committable")
        attachScreenshot(app, name: "llm-context-editor")
    }

    /// An env var outranks the vault, so a key stored while one is set is
    /// accepted, reported saved, and never used. The warning has to be on
    /// screen BEFORE the tap — and there must be no clear affordance at all,
    /// because clearing a stored key is impossible over HTTP.
    func testTheKeyScreenWarnsAboutTheEnvShadowAndOffersNoClear() {
        let app = openModels()
        app.buttons["llm-entry-gpt"].tap()
        let keyRow = app.buttons["llm-field-api-key"]
        XCTAssertTrue(keyRow.waitForExistence(timeout: 5))
        // Plain "Not set", NOT the pinned/inherited phrasing the other rows
        // carry: that language is about an override, and a write-only
        // credential has none.
        XCTAssertEqual(keyRow.value as? String, "Not set")
        keyRow.tap()

        let store = app.buttons["llm-commit-api-key"]
        XCTAssertTrue(store.waitForExistence(timeout: 5), "the key editor never appeared")
        XCTAssertFalse(store.isEnabled, "a blank key must not be committable")

        XCTAssertTrue(
            app.staticTexts.containing(
                NSPredicate(format: "label CONTAINS %@", "OPENAI_KEY")).firstMatch.exists,
            "the env-var shadow must be stated before the tap")
        XCTAssertTrue(
            app.buttons["llm-clear-env"].exists,
            "clearing the shadowing env var is the documented escape hatch")
        // `gpt`'s key comes from the environment, not the vault. Removal
        // deletes only what the vault holds, so offering it here would be a
        // button that reports success and changes nothing.
        XCTAssertFalse(
            app.buttons["llm-remove-key"].exists,
            "an env-provided key must not offer removal")
        attachScreenshot(app, name: "llm-key-editor")
    }

    /// The mirror image: `claude` has a key in the vault, so removal is real
    /// and offered. This is the affordance that could not exist while an empty
    /// `api_key` was a silent no-op.
    func testAStoredKeyOffersRemoval() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        let keyRow = app.buttons["llm-field-api-key"]
        XCTAssertTrue(keyRow.waitForExistence(timeout: 5))
        XCTAssertEqual(keyRow.value as? String, "Configured")
        keyRow.tap()

        let remove = app.buttons["llm-remove-key"]
        XCTAssertTrue(remove.waitForExistence(timeout: 5), "a vault-stored key must be removable")
        attachScreenshot(app, name: "llm-key-stored")

        // Irreversible and unreadable-back, so it confirms rather than firing
        // on the tap that reveals it.
        remove.tap()
        XCTAssertTrue(
            app.buttons["Remove"].waitForExistence(timeout: 5),
            "removing a key must confirm first")
        XCTAssertTrue(app.buttons["Cancel"].exists)
        attachScreenshot(app, name: "llm-key-remove-confirm")
        app.buttons["Cancel"].tap()
    }

    /// The model list is now managed from the phone: every served model gets a
    /// remove affordance except the default (the entry prepends it to its own
    /// list, so removing it would not remove it), and "Add a model" opens the
    /// provider's live catalog rather than a text field.
    func testTheModelListOffersRemovalExceptForTheDefault() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        let model = app.buttons["llm-field-model"]
        XCTAssertTrue(model.waitForExistence(timeout: 5))
        model.tap()

        let candidate = app.buttons["llm-option-claude-opus-4-8"]
        XCTAssertTrue(candidate.waitForExistence(timeout: 5), "the model list never appeared")
        XCTAssertTrue(
            app.buttons["llm-option-claude-opus-4-8-remove"].exists,
            "a non-default model must be removable")
        XCTAssertFalse(
            app.buttons["llm-option-claude-sonnet-5-remove"].exists,
            "the default model must NOT offer removal")
        XCTAssertTrue(app.buttons["llm-add-model"].exists)
        attachScreenshot(app, name: "llm-model-list")
    }

    /// Adding is a PICK from what the provider currently offers, never free
    /// text — nothing gateway-side checks an id against the vendor, so a typo
    /// would survive every validation and fail at the first real completion.
    /// With no gateway behind the demo catalog the fetch fails, and the level
    /// has to say so rather than showing an empty list.
    func testAddingAModelOpensTheProviderCatalog() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        app.buttons["llm-field-model"].tap()
        let add = app.buttons["llm-add-model"]
        XCTAssertTrue(add.waitForExistence(timeout: 5))
        add.tap()

        // Either the catalog rendered or it said why it could not. Asserting on
        // an identifier rather than the prose: the failure text is the
        // provider's or the transport's, and this smoke has no business
        // pinning either one's wording.
        let failure = app.staticTexts["llm-catalog-failure"]
        let anyRow = app.buttons.matching(
            NSPredicate(format: "identifier BEGINSWITH %@", "llm-catalog-")).firstMatch
        let deadline = Date().addingTimeInterval(30)
        while Date() < deadline && !failure.exists && !anyRow.exists {
            _ = failure.waitForExistence(timeout: 1)
        }
        XCTAssertTrue(
            failure.exists || anyRow.exists,
            "the catalog level must resolve to a list or a stated failure, not stay blank")
        XCTAssertFalse(
            app.textFields["llm-input-add-model"].exists,
            "adding a model must never be free text")
        attachScreenshot(app, name: "llm-add-model")
    }

    /// A pick commits immediately — there is no Save button on the fields
    /// level — and with no gateway behind the demo catalog the write fails.
    /// What matters is that the failure SURFACES: a silent no-op here would
    /// read as "the tap did nothing".
    func testAFailedWriteReachesTheOutcomeStrip() {
        let app = openModels()
        app.buttons["llm-entry-claude"].tap()
        let model = app.buttons["llm-field-model"]
        XCTAssertTrue(model.waitForExistence(timeout: 5))
        model.tap()

        let option = app.buttons["llm-option-claude-opus-4-8"]
        XCTAssertTrue(option.waitForExistence(timeout: 5), "the model picker never appeared")
        option.tap()

        let outcome = app.staticTexts["llm-outcome"]
        XCTAssertTrue(
            outcome.waitForExistence(timeout: 10),
            "a failed write must say so instead of doing nothing visible")
        XCTAssertTrue(outcome.label.contains("Model"), "the strip names the field it tried")
        attachScreenshot(app, name: "llm-write-failed")
    }
}
