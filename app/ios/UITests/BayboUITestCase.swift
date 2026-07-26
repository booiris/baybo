import XCTest

/// Shared launch + wait plumbing for the headless `-baybo-*` fixture smokes.
///
/// Every subclass launches through `launch(_:)`, which pins `-baybo.lang en`.
/// That pin is load-bearing, not tidiness: XCUITest matches SwiftUI buttons by
/// their LABEL, and the labels are localized. A CI runner's simulator is
/// English while a developer's may not be, so a test that omits the pin passes
/// on one machine and burns its timeout on the other (exactly what happened to
/// `AttachmentShareUITests`, which matched a Chinese label and could never have
/// passed on a runner).
///
/// These smokes log out and mutate the session registry, so running them
/// against a PAIRED simulator DESTROYS that pairing. CI is always a fresh sim;
/// locally, expect to re-pair.
class BayboUITestCase: XCTestCase {
    /// The demo download drive (`-baybo-demo-download`) walks the first file
    /// card and the video tile to `ready` TOGETHER — one loop, one terminal
    /// push (`DemoFrames.driveDemoDownloadIfRequested`). The video tile's
    /// accessibility label flips to this on `ready`, so it is the only
    /// observable "the drive finished" signal either card exposes: the file
    /// card's label is identical at `idle` and at `ready` (both read
    /// `PDF · 2.3 MB`), which is why the tests here used to sleep out the
    /// drive's wall-clock instead.
    static let videoReadyLabel = "Play video"

    /// Ceiling for anything the transcript WEBVIEW renders. Deliberately far
    /// looser than the 3-5s the native chrome gets: a WKWebView content process
    /// spins up cold, and a hosted runner does it several times slower than a
    /// dev Mac (the same four attachment cases take ~60s locally and ~270s
    /// there). Keep the NATIVE waits tight — a slow button is a bug — and spend
    /// the patience only where the wait is a web runtime booting.
    static let webviewTimeout: TimeInterval = 60

    override func setUp() {
        super.setUp()
        continueAfterFailure = false
    }

    /// Terminate the app we launched, rather than leaving it for the NEXT test's
    /// `launch()` to kill implicitly. A hosted CI runner is several times slower
    /// than a dev Mac, and there the implicit kill raced its own launch:
    /// `Failed to terminate com.baybo.app`, surfaced as a failure of whichever
    /// test happened to run next. Terminating on the way out gives the shutdown
    /// its own window.
    override func tearDown() {
        launched?.terminate()
        launched = nil
        super.tearDown()
    }

    private var launched: XCUIApplication?

    /// Launch with the fixture flags, language pinned — BOTH languages.
    ///
    /// `-baybo.lang` only pins OUR strings (it is the app's own UserDefaults
    /// key). System-framework chrome inside the app — AVKit's fullscreen
    /// player, the share sheet — follows the SIMULATOR's locale, which differs
    /// between a developer's machine and a CI runner. `-AppleLanguages` pins
    /// that too, so a label like AVKit's "Done" means the same thing
    /// everywhere. Without it, a test matching system chrome by label passes on
    /// an English sim and fails on any other.
    /// Every launch also wipes the device-local stores. The demo fixtures use
    /// FIXED session ids, so without this each launch appends its canned turn to
    /// the same persisted transcript mirror — and because one simulator is
    /// shared across a suite's cases, the fixture grows with every case. The
    /// attachment demo reached SIX video tiles that way, and a by-label query
    /// that is unambiguous on a fresh install started matching six elements.
    @discardableResult
    func launch(_ arguments: [String]) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments =
            arguments + [
                "-baybo-reset-store",
                "-baybo.lang", "en",
                "-AppleLanguages", "(en)",
                "-AppleLocale", "en_US",
            ]
        app.launch()
        launched = app
        return app
    }

    /// The `ConfirmDialog`'s commit button, told apart from the swipe action that
    /// raised it. A tapped swipe action stays parked on screen, so its `Delete`
    /// and the dialog's are BOTH live and a by-label query matches two. The
    /// dialog's is the one sharing the Cancel row, so vertical alignment picks it
    /// out — and tapping the wrong one would just re-arm the dialog, which reads
    /// as "the confirm did nothing".
    func dialogCommit(_ app: XCUIApplication, _ label: String) throws -> XCUIElement {
        let cancel = app.buttons["Cancel"]
        XCTAssertTrue(cancel.waitForExistence(timeout: 3), "the confirm dialog did not present")
        let cancelMidY = cancel.frame.midY
        return try XCTUnwrap(
            app.buttons.matching(NSPredicate(format: "label == %@", label))
                .allElementsBoundByIndex
                .first { abs($0.frame.midY - cancelMidY) < 4 },
            "no dialog \(label) button beside Cancel")
    }

    /// Block until `-baybo-demo-download` has walked both cards to `ready`.
    ///
    /// Replaces a hard sleep, and the generous ceiling is the point: the drive
    /// itself is ~6s, but it only starts once the app has launched and the
    /// transcript webview has booted, and a hosted runner does that several
    /// times slower than a dev Mac (the same four cases take ~60s locally and
    /// ~270s there). A wait costs nothing when the signal arrives early — a
    /// ceiling tuned to a fast machine is just a scheduled flake.
    func waitForDemoDownload(_ app: XCUIApplication, timeout: TimeInterval = 90) {
        let ready = app.buttons[Self.videoReadyLabel]
        XCTAssertTrue(
            ready.waitForExistence(timeout: timeout),
            "the demo download drive never reached ready")
    }

    /// Keep a shot of transient chrome — a presented sheet, a menu level — in
    /// the xcresult, which is the only way to see it from outside the runner
    /// (`xcrun xcresulttool export attachments`).
    func attachScreenshot(_ app: XCUIApplication, name: String) {
        let shot = XCTAttachment(screenshot: app.screenshot())
        shot.name = name
        shot.lifetime = .keepAlways
        add(shot)
    }
}
