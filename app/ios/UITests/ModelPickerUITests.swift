import XCTest

/// The chat header's model panel (`ModelMenuPanel` — hand-rolled, not a
/// system `Menu`), driven through the real hit-test path: the pill rests on
/// the default entry's default MODEL, level 1 lists ENTRIES by name (trailing
/// checkmark, no chevron), an entry's level lists its models (`model` +
/// `model_candidates`) plus the Thinking row with the current level as its
/// subtitle, and a pick flips the pill optimistically. The demo chat is a
/// DRAFT, so a pick exercises the stash-for-creation path — no gateway
/// involved, which is what makes it drivable headlessly
/// (`-baybo-demo-models` seeds the catalog; the `gpt` entry carries
/// `model_candidates` so the entry→models level is exercisable).
final class ModelPickerUITests: BayboUITestCase {
    private static let chatArguments = [
        "-baybo-open-chat", "-baybo-demo-models",
    ]

    func testPickingAModelUnderAnEntryPinsIt() throws {
        let app = launch(Self.chatArguments)

        let pill = app.buttons["Model"]
        XCTAssertTrue(pill.waitForExistence(timeout: 5), "the model pill did not render")
        XCTAssertEqual(
            pill.value as? String, "claude-sonnet-5",
            "an unpinned session's pill must rest on the default entry's default model")

        pill.tap()
        let gpt = app.buttons["gpt"]
        XCTAssertTrue(gpt.waitForExistence(timeout: 3), "the entry level did not present")
        XCTAssertTrue(app.buttons["claude"].exists, "an entry row is missing")
        attachScreenshot(app, name: "entries")
        gpt.tap()

        let o3 = app.buttons["o3"]
        XCTAssertTrue(o3.waitForExistence(timeout: 3), "the entry's model level did not present")
        XCTAssertTrue(app.buttons["gpt-5.5"].exists, "the entry's default model row is missing")
        XCTAssertTrue(app.buttons["gpt-5.5-mini"].exists, "a candidate model row is missing")
        let thinking = app.buttons["Thinking"]
        XCTAssertTrue(thinking.exists, "the thinking row is missing")
        XCTAssertEqual(
            thinking.value as? String, "Ultra high",
            "the thinking row must subtitle the entry's current level (gpt is xhigh)")
        attachScreenshot(app, name: "models")
        o3.tap()

        XCTAssertTrue(
            waitUntilValue(pill, equals: "o3"),
            "the pick did not flip the pill (value: \(String(describing: pill.value)))")
    }

    func testWalkingBackToAnotherEntry() throws {
        let app = launch(Self.chatArguments)

        let pill = app.buttons["Model"]
        XCTAssertTrue(pill.waitForExistence(timeout: 5))

        pill.tap()
        app.buttons["gpt"].tap()
        let mini = app.buttons["gpt-5.5-mini"]
        XCTAssertTrue(mini.waitForExistence(timeout: 3))
        mini.tap()
        XCTAssertTrue(waitUntilValue(pill, equals: "gpt-5.5-mini"))

        pill.tap()
        app.buttons["claude"].tap()
        let opus = app.buttons["claude-opus-4-8"]
        XCTAssertTrue(opus.waitForExistence(timeout: 3), "the entry's candidate model is missing")
        opus.tap()

        XCTAssertTrue(
            waitUntilValue(pill, equals: "claude-opus-4-8"),
            "walking back to another entry's model did not flip the pill")
    }

    /// The thinking level: its rows render with the current level checkmarked
    /// (panel-drawn, trailing) and picking one under a non-effective entry
    /// also pins that entry+model — the pill flips. The level PUT itself is
    /// unit-tested; only the pin side is observable here (no gateway).
    func testPickingAThinkingLevelAlsoPinsTheModel() throws {
        let app = launch(Self.chatArguments)

        let pill = app.buttons["Model"]
        XCTAssertTrue(pill.waitForExistence(timeout: 5))

        pill.tap()
        app.buttons["gpt"].tap()
        let thinking = app.buttons["Thinking"]
        XCTAssertTrue(thinking.waitForExistence(timeout: 3))
        thinking.tap()

        let high = app.buttons["High"]
        XCTAssertTrue(high.waitForExistence(timeout: 3), "the level rows did not present")
        XCTAssertTrue(app.buttons["Ultra high"].exists, "the xhigh row is missing")
        attachScreenshot(app, name: "thinking-levels")
        high.tap()

        XCTAssertTrue(
            waitUntilValue(pill, equals: "gpt-5.5"),
            "picking a level must pin the entry it was picked under (its default model)")
    }

    /// The pill publishes its selection as the element VALUE (the a11y label is
    /// the constant "Model"), and XCUIElement.value has no waitFor — poll it.
    private func waitUntilValue(
        _ element: XCUIElement, equals expected: String, timeout: TimeInterval = 3
    ) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if element.value as? String == expected { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
        }
        return element.value as? String == expected
    }
}
