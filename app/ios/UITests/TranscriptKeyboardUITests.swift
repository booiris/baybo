import XCTest

final class TranscriptKeyboardUITests: BayboUITestCase {
    func testReopeningConversationsFromTheListShowsTheirNewestRows() {
        let app = launch(["-baybo-open-home", "-baybo-test-list-transcripts"])
        for (number, count) in [(3, 8), (5, 65), (3, 8), (3, 8)] {
            app.staticTexts["Demo conversation number \(number)"].tap()
            let row = app.webViews.staticTexts.matching(
                NSPredicate(format: "label CONTAINS %@", "demo-\(number) row \(count)")).firstMatch
            XCTAssertTrue(row.waitForExistence(timeout: Self.webviewTimeout))
            XCTAssertTrue(row.isHittable)
            app.buttons["Back to conversations"].tap()
        }
    }

    func testSlightScrollUpBeforeOpeningKeyboardKeepsTheSameRowInPlace() throws {
        try checkNearBottomPosition(startWithKeyboard: false)
    }

    func testSlightScrollUpBeforeDismissingKeyboardKeepsTheSameRowInPlace() throws {
        try checkNearBottomPosition(startWithKeyboard: true)
    }

    private func checkNearBottomPosition(startWithKeyboard: Bool) throws {
        let app = launch(["-baybo-open-chat", "-baybo-demo-index"])
        let web = app.webViews.firstMatch
        let field = app.descendants(matching: .any)["composer.field"].firstMatch
        let newest = web.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH %@", "Done — three paragraphs")).firstMatch
        XCTAssertTrue(newest.waitForExistence(timeout: Self.webviewTimeout))
        let keyboard = app.keyboards.firstMatch
        if startWithKeyboard {
            field.tap()
            XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        }

        let text = try XCTUnwrap(web.staticTexts.allElementsBoundByIndex.first {
            $0.label.count > 12 && $0.frame.minY > 125 && $0.frame.midY < web.frame.height * 0.42
        })
        let row = web.staticTexts.matching(NSPredicate(format: "label == %@", text.label)).firstMatch
        let pinned = row.frame.midY
        let start = web.coordinate(withNormalizedOffset: CGVector(dx: 0.45, dy: 0.3))
        start.press(forDuration: 0.15, thenDragTo: start.withOffset(CGVector(dx: 0, dy: 40)),
                    withVelocity: .slow, thenHoldForDuration: 0.4)
        let parked = row.frame.midY
        XCTAssertGreaterThan(parked - pinned, 2)
        XCTAssertLessThan(parked - pinned, 96)
        XCTAssertEqual(keyboard.exists, startWithKeyboard)

        for open in [!startWithKeyboard, startWithKeyboard] {
            if open {
                field.tap()
                XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
            } else {
                row.tap()
                XCTAssertTrue(keyboard.waitForNonExistence(timeout: 5))
            }
            XCTAssertEqual(row.frame.midY, parked, accuracy: 2)
        }
        let towardNewest = web.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.44))
        towardNewest.press(forDuration: 0.1, thenDragTo: towardNewest.withOffset(CGVector(dx: 0, dy: -120)),
                           withVelocity: .slow, thenHoldForDuration: 0.3)
        let returned = newest.frame.midY
        if startWithKeyboard {
            newest.tap()
            XCTAssertTrue(keyboard.waitForNonExistence(timeout: 5))
        } else {
            field.tap()
            XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        }
        let follows = NSPredicate { _, _ in
            let delta = newest.frame.midY - returned
            return startWithKeyboard ? delta > 100 : delta < -100
        }
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: follows, object: nil)],
                                      timeout: 4), .completed)
    }

    func testOpeningKeyboardKeepsAReaderWhoDraggedIntoHistoryParked() throws {
        let app = launch(["-baybo-open-chat", "-baybo-demo-index"])
        let web = app.webViews.firstMatch
        let field = app.descendants(matching: .any)["composer.field"].firstMatch
        let newest = web.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH %@", "Done — three paragraphs")).firstMatch
        XCTAssertTrue(newest.waitForExistence(timeout: Self.webviewTimeout))
        field.tap()
        let keyboard = app.keyboards.firstMatch
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        let start = web.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.25))
        let end = web.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.82))
        start.press(forDuration: 0.1, thenDragTo: end)
        XCTAssertTrue(keyboard.waitForNonExistence(timeout: 5))
        web.swipeDown(velocity: .slow)
        let jump = app.buttons["chat-jump"]
        XCTAssertTrue(jump.waitForExistence(timeout: 5))
        let parked = try XCTUnwrap(web.staticTexts.allElementsBoundByIndex.first {
            $0.label.count > 12 && $0.frame.minY > 120 && $0.frame.midY < web.frame.midY
        })
        let row = web.staticTexts.matching(NSPredicate(format: "label == %@", parked.label)).firstMatch
        let parkedY = row.frame.midY
        field.tap()
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        XCTAssertTrue(jump.exists)
        XCTAssertEqual(row.frame.midY, parkedY, accuracy: 8)
    }

    func testRefocusingAfterTappingAMessageKeepsTheNewestRowAboveTheField() {
        let app = launch(["-baybo-open-chat", "-baybo-demo-index"])
        let field = app.descendants(matching: .any)["composer.field"].firstMatch
        let newest = app.webViews.staticTexts.matching(
            NSPredicate(format: "label BEGINSWITH %@", "Done — three paragraphs")).firstMatch
        XCTAssertTrue(newest.waitForExistence(timeout: Self.webviewTimeout))
        let closedY = newest.frame.midY

        field.tap()
        let keyboard = app.keyboards.firstMatch
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        let openY = newest.frame.midY
        XCTAssertLessThan(openY, closedY - 100)
        newest.tap()
        XCTAssertTrue(keyboard.waitForNonExistence(timeout: 5))
        field.tap()
        XCTAssertTrue(keyboard.waitForExistence(timeout: 5))
        XCTAssertEqual(newest.frame.midY, openY, accuracy: 8)
        XCTAssertLessThan(newest.frame.maxY, field.frame.minY)
        XCTAssertFalse(app.buttons["chat-jump"].exists)
    }
}
