import XCTest

final class QuestionTextUITests: BayboUITestCase {
    private let prompt = """
        You have one afternoon between two train journeys. The group wants a quiet place for lunch, a short walk near the water, and enough time to collect its luggage before the last connection.

        If the first train arrives late and it starts raining, which part of the plan would you keep?
        """
    private let longLabel = "Reserve the small covered platform near the visitor centre so everyone can sit together, check the timetable and still watch the boats come in."
    private let routeDescription = """
        Start at the north entrance, take the lift down to the river and follow the covered walkway.
        Leave enough time to cross the footbridge and return before the ticket office closes.
        """
    private let routeCode = "Route" + String(repeating: "HarbourConnection", count: 9) + "Terminus"
    private var destinations: String {
        let locale = Locale(identifier: "zh_CN")
        let countries = ["NZ", "IS", "CH", "JP", "SG", "TH", "IT", "NO", "GB", "BR"]
        let names = countries.compactMap { locale.localizedString(forRegionCode: $0) }.joined()
        return String(repeating: names, count: 3) + "🚆"
    }

    private func launchQuestions() throws -> XCUIApplication {
        let shortOptions = [
            ["label": "Lunch", "description": "Eat nearby."],
            ["label": "Walk", "description": "Walk by the river."],
        ]
        let questions: [[String: Any]] = [
            ["header": "First choice", "question": prompt, "multi_select": false, "options": shortOptions],
            ["header": "Route detail", "question": "Which route details should we keep?", "multi_select": true,
             "options": [
                 ["label": longLabel, "description": routeDescription],
                 ["label": routeCode, "description": "Keep this reference for the return journey."],
                 ["label": destinations, "description": "Keep the original destination names."],
                 ["label": "Finish early", "description": "Take the first train back."],
             ]],
            ["header": "Alternatives", "question": "Which backup plan works?", "multi_select": false, "options": shortOptions],
            ["header": "Final review", "question": "Which stop should we confirm?", "multi_select": false, "options": shortOptions],
        ]
        let data = try JSONSerialization.data(withJSONObject: questions)
        return launch(["-baybo-open-chat", "-baybo-demo-question"],
                      environment: ["BAYBO_DEMO_QUESTIONS": String(decoding: data, as: UTF8.self)])
    }

    private func option(_ label: String, in list: XCUIElement) -> XCUIElement {
        list.descendants(matching: .any).matching(NSPredicate(format: "label == %@", label)).firstMatch
    }

    private func scrollToBottom(_ list: XCUIElement) {
        let other = list.buttons["Write your own answer"]
        for _ in 0..<6 {
            if other.isHittable && list.frame.contains(other.frame) { return }
            list.swipeUp()
        }
        XCTAssertTrue(list.frame.contains(other.frame))
    }

    func testLongQuestionAndOptionsWrapAndStaySelectableAcrossFourTabs() throws {
        let app = try launchQuestions()
        let list = app.webViews.firstMatch
        let title = list.descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH %@", String(prompt.prefix(40)))).firstMatch
        XCTAssertTrue(title.waitForExistence(timeout: 15))
        XCTAssertTrue(list.frame.contains(title.frame))
        XCTAssertGreaterThan(title.frame.height, 65)
        XCTAssertLessThanOrEqual(app.buttons["Next question"].frame.height, 45)
        attachScreenshot(app, name: "long-question")

        let lunch = option("Lunch", in: list)
        let shortRowHeight = lunch.frame.height
        lunch.tap()
        let long = option(longLabel, in: list)
        XCTAssertTrue(long.waitForExistence(timeout: 5))
        XCTAssertTrue(list.frame.contains(long.frame))
        XCTAssertGreaterThan(long.frame.height, shortRowHeight + 45)
        long.tap()
        XCTAssertEqual(long.value as? String, "1")
        attachScreenshot(app, name: "long-option")

        let code = option(routeCode, in: list)
        let names = option(destinations, in: list)
        for row in [code, names] {
            XCTAssertGreaterThan(row.frame.height, shortRowHeight + 15)
            XCTAssertGreaterThanOrEqual(row.frame.minX, list.frame.minX)
            XCTAssertLessThanOrEqual(row.frame.maxX, list.frame.maxX)
        }
        scrollToBottom(list)
        names.tap()
        let last = option("Finish early", in: list)
        last.tap()
        XCTAssertEqual(names.value as? String, "1")
        XCTAssertEqual(last.value as? String, "1")
        attachScreenshot(app, name: "unbroken-options")

        app.buttons["Next question"].tap()
        lunch.tap()
        let final = option("Which stop should we confirm?", in: list)
        XCTAssertTrue(final.waitForExistence(timeout: 5))
        XCTAssertTrue(list.frame.contains(final.frame))
        XCTAssertTrue(list.buttons["Final review"].isHittable)
        XCTAssertTrue(app.buttons["Submit answers"].isHittable)
    }

    func testMultilineOtherKeepsItsLastLineAndRestoresTheDraft() throws {
        let app = try launchQuestions()
        let list = app.webViews.firstMatch
        let lunch = option("Lunch", in: list)
        XCTAssertTrue(lunch.waitForExistence(timeout: 15))
        lunch.tap()
        XCTAssertTrue(option(longLabel, in: list).waitForExistence(timeout: 5))
        scrollToBottom(list)
        list.buttons["Write your own answer"].tap()
        let editor = app.descendants(matching: .any)["question.other.field"].firstMatch
        let done = app.buttons["question.other.done"]
        XCTAssertTrue(done.waitForExistence(timeout: 5))
        let answer = """
            Pick up the tickets.
            Take the covered route.
            Stop for lunch.
            Walk along the river.
            Meet at the last platform.
            """
        editor.typeText(answer)
        XCTAssertEqual(editor.value as? String, answer)
        XCTAssertTrue(app.keyboards.firstMatch.exists)
        attachScreenshot(app, name: "multiline-other")
        XCTAssertTrue(list.frame.contains(editor.frame), "Editor \(editor.frame) must fit inside \(list.frame)")
        XCTAssertTrue(list.frame.contains(done.frame), "Done \(done.frame) must fit inside \(list.frame)")
        done.tap()
        XCTAssertFalse(editor.exists)
        XCTAssertTrue(app.descendants(matching: .any)["composer.field"].firstMatch.isHittable)
        app.buttons["Next question"].tap()
        list.buttons["Route detail"].tap()
        let written = list.buttons.matching(NSPredicate(format: "label CONTAINS %@", "Pick up the tickets.")).firstMatch
        written.tap()
        XCTAssertTrue(editor.waitForExistence(timeout: 5))
        XCTAssertEqual(editor.value as? String, answer)
    }
}
