import XCTest

final class InlineQuestionUITests: BayboUITestCase {
    private func choice(_ title: String, in thread: XCUIElement) -> XCUIElement {
        thread.descendants(matching: .any).matching(NSPredicate(format: "label == %@", title)).firstMatch
    }

    func testQuestionsFollowTheMessageAndKeepSelectionsAcrossTabs() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-question"])
        let thread = app.webViews.firstMatch
        let train = choice("Train", in: thread)
        XCTAssertTrue(thread.buttons["Next question"].waitForExistence(timeout: Self.webviewTimeout))
        XCTAssertTrue(train.exists, thread.debugDescription)
        let message = thread.staticTexts["Plan a weekend trip"].firstMatch
        let composer = app.descendants(matching: .any)["composer.field"].firstMatch
        let next = thread.buttons["Next question"]
        let cancel = thread.buttons["Cancel question"]
        XCTAssertEqual(cancel.frame.width, 32, accuracy: 1)
        XCTAssertEqual(cancel.frame.height, 32, accuracy: 1)
        XCTAssertLessThan(message.frame.maxY, train.frame.minY)
        XCTAssertLessThan(next.frame.maxY, composer.frame.minY)
        XCTAssertFalse(next.isEnabled)
        for title in ["Train", "Ferry", "Bicycle", "Walk", "Write your own answer"] {
            XCTAssertTrue(choice(title, in: thread).isHittable)
        }
        attachScreenshot(app, name: "inline-question-on-entry")
        train.tap()
        let art = choice("Art", in: thread)
        XCTAssertTrue(art.waitForExistence(timeout: 5))
        art.tap()
        let food = choice("Food", in: thread)
        food.tap()
        XCTAssertEqual(art.value as? String, "1")
        XCTAssertEqual(food.value as? String, "1")
        attachScreenshot(app, name: "inline-multi-select")
        thread.buttons["Travel"].tap()
        XCTAssertEqual(train.value as? String, "1")
        let walk = choice("Walk", in: thread)
        walk.tap()
        XCTAssertEqual(walk.value as? String, "1")
        XCTAssertEqual(train.value as? String, "0")
        XCTAssertTrue(next.isEnabled)
        next.tap()
        XCTAssertTrue(art.waitForExistence(timeout: 5))
    }

    func testLastTabKeepsSubmitDisabledAfterSkippingAnEarlierQuestion() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-question"])
        let thread = app.webViews.firstMatch
        let interests = thread.buttons["Interests"]
        XCTAssertTrue(interests.waitForExistence(timeout: Self.webviewTimeout))
        interests.tap()
        choice("Art", in: thread).tap()
        XCTAssertFalse(thread.buttons["Submit answers"].isEnabled)
        XCTAssertFalse(thread.buttons["Next question"].exists)
        thread.buttons["Travel"].tap()
        XCTAssertFalse(thread.buttons["Next question"].isEnabled)
    }

    func testOtherReplacesComposerAndReturnsBothDrafts() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-question"])
        let thread = app.webViews.firstMatch
        let other = thread.buttons["Write your own answer"]
        XCTAssertTrue(other.waitForExistence(timeout: Self.webviewTimeout))
        let composer = app.descendants(matching: .any)["composer.field"].firstMatch
        composer.tap()
        composer.typeText("A separate message")
        other.tap()
        let editor = app.descendants(matching: .any)["question.other.field"].firstMatch
        let done = app.buttons["question.other.done"]
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        XCTAssertFalse(composer.exists)
        editor.typeText("Bike along the river")
        XCTAssertEqual(editor.value as? String, "Bike along the river")
        let keyboard = app.keyboards.firstMatch
        XCTAssertTrue(keyboard.exists)
        XCTAssertTrue(done.isHittable)
        XCTAssertGreaterThanOrEqual(keyboard.frame.minY - done.frame.maxY, 15.5)
        XCTAssertGreaterThanOrEqual(done.frame.minY - thread.buttons["Next question"].frame.maxY, 23.5)
        attachScreenshot(app, name: "inline-other-keyboard")
        done.tap()
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "A separate message")
        XCTAssertTrue(keyboard.waitForNonExistence(timeout: 5))
        let row = thread.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Bike along the river")).firstMatch
        row.tap()
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        editor.tap()
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        XCTAssertEqual(editor.value as? String, "Bike along the river")
        thread.buttons["Next question"].tap()
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        XCTAssertTrue(choice("Art", in: thread).exists)
    }
}
