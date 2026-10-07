import XCTest

final class ConnectionDetailsUITests: BayboUITestCase {
    func testDetailsOwnProbeAndScopedLiveConsole() {
        let app = launch([
            "-baybo-open-home", "-baybo-home-tab", "settings", "-baybo-demo-connection",
        ])
        let connection = app.buttons["settings.connection"]
        XCTAssertTrue(connection.waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts["Last check"].exists)
        connection.tap()
        let toggle = app.switches["connection.diagnostics"]
        XCTAssertTrue(toggle.waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts["Last check"].exists)
        XCTAssertEqual(toggle.value as? String, "0")
        toggle.tap()
        let started = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS %@", "Connection diagnostics started")
        ).firstMatch
        XCTAssertTrue(started.waitForExistence(timeout: 5))
        XCUIDevice.shared.press(.home)
        app.activate()
        let foreground = app.staticTexts.matching(
            NSPredicate(format: "label CONTAINS %@", "App foreground:")
        ).firstMatch
        XCTAssertTrue(foreground.waitForExistence(timeout: 5))
        toggle.tap()
        toggle.tap()
        XCTAssertTrue(foreground.exists)
        for _ in 0..<12 {
            toggle.tap()
            toggle.tap()
        }
        let console = app.scrollViews["connection.logConsole"]
        let initialFrame = console.frame
        console.swipeDown()
        let follow = app.buttons["connection.followLatest"]
        XCTAssertTrue(follow.waitForExistence(timeout: 5))
        XCTAssertTrue(follow.isHittable)
        XCTAssertEqual(console.frame.height, initialFrame.height, accuracy: 1)
        XCTAssertLessThan(follow.frame.maxY, app.frame.maxY)
        let screenshot = XCTAttachment(screenshot: app.screenshot())
        screenshot.name = "Connection details and live console"
        screenshot.lifetime = .keepAlways
        add(screenshot)
        for _ in 0..<8 {
            if !follow.exists { break }
            console.swipeUp()
        }
        XCTAssertFalse(follow.exists)
        console.swipeDown()
        XCTAssertTrue(follow.waitForExistence(timeout: 5))
        follow.tap()
        XCTAssertFalse(follow.exists)

        app.buttons["Copy"].tap()
        XCTAssertTrue(app.buttons["Copied"].exists)
        app.buttons["Clear"].tap()
        XCTAssertFalse(started.exists)
        app.buttons["connection.back"].tap()
        XCTAssertTrue(connection.waitForExistence(timeout: 5))
        connection.tap()
        XCTAssertTrue(toggle.waitForExistence(timeout: 5))
        XCTAssertEqual(toggle.value as? String, "0")
        XCTAssertFalse(started.exists)
    }
}
