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

    override func setUp() {
        super.setUp()
        continueAfterFailure = false
    }

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
        return app
    }

    /// Block until `-baybo-demo-download` has walked both cards to `ready`.
    /// Replaces a hard sleep: the drive takes ~5.7s, but a loaded CI runner can
    /// take longer, and a sleep that is long enough there is pure waste here.
    func waitForDemoDownload(_ app: XCUIApplication, timeout: TimeInterval = 20) {
        let ready = app.buttons[Self.videoReadyLabel]
        XCTAssertTrue(
            ready.waitForExistence(timeout: timeout),
            "the demo download drive never reached ready")
    }
}
