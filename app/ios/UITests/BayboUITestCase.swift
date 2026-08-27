import UIKit
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

    /// What a screenshot actually PAINTS, which every other assertion in these
    /// suites is blind to: they read `exists` / `isHittable` / frames, all of
    /// which a laid-out-but-invisible element satisfies perfectly (see the
    /// `.clipped()` attach panel, and the paper band under a full-screen HTML
    /// preview). Reach for it only when the claim really is about pixels.
    func screenPixels() -> ScreenshotPixels? {
        ScreenshotPixels(XCUIScreen.main.screenshot().image)
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

/// One screenshot's pixels, read once.
///
/// Reading a point at a time through a 1x1 `CGContext` is fine for a handful of
/// samples and hopeless for a scan, so a scan crops the column FIRST (free — no
/// rasterisation) and rasterises that alone.
struct ScreenshotPixels {
    private let image: CGImage
    let scale: CGFloat

    init?(_ image: UIImage) {
        guard let cgImage = image.cgImage else { return nil }
        self.image = cgImage
        scale = image.scale
    }

    /// Size in POINTS — screenshots are at the device's scale, and every caller
    /// thinks in the units XCUITest frames are in.
    var size: CGSize {
        CGSize(width: CGFloat(image.width) / scale, height: CGFloat(image.height) / scale)
    }

    /// Rec. 601 luma at one point, 0...1. Nil outside the image.
    func brightness(at point: CGPoint) -> CGFloat? {
        let x = Int(point.x * scale)
        let y = Int(point.y * scale)
        guard x >= 0, y >= 0, x < image.width, y < image.height else { return nil }
        return rows(column: x, from: y, height: 1)?.first
    }

    /// How much of a vertical line through `x` is BRIGHT, in points — the
    /// height of whatever light thing the column crosses. Orientation-free by
    /// construction: it counts rows rather than locating them, so it says
    /// nothing about which way CoreGraphics stacked them.
    func brightHeight(atX x: CGFloat, threshold: CGFloat = 0.5) -> CGFloat? {
        let column = Int(x * scale)
        guard column >= 0, column < image.width,
            let luma = rows(column: column, from: 0, height: image.height)
        else { return nil }
        return CGFloat(luma.filter { $0 > threshold }.count) / scale
    }

    func redCoverage(in rect: CGRect) -> CGFloat {
        let x = Int(rect.minX * scale)
        let y = Int(rect.minY * scale)
        let w = Int(rect.width * scale)
        let h = Int(rect.height * scale)
        guard w > 0, h > 0, x >= 0, y >= 0, x + w <= image.width, y + h <= image.height,
            let crop = image.cropping(to: CGRect(x: x, y: y, width: w, height: h))
        else { return 0 }
        var pixels = [UInt8](repeating: 0, count: w * h * 4)
        guard
            let context = pixels.withUnsafeMutableBytes({ buffer in
                CGContext(
                    data: buffer.baseAddress, width: w, height: h, bitsPerComponent: 8,
                    bytesPerRow: w * 4, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            })
        else { return 0 }
        context.draw(crop, in: CGRect(x: 0, y: 0, width: w, height: h))
        var red = 0
        for i in stride(from: 0, to: pixels.count, by: 4) {
            let r = CGFloat(pixels[i]) / 255
            let g = CGFloat(pixels[i + 1]) / 255
            let b = CGFloat(pixels[i + 2]) / 255
            if r > 0.5, r - max(g, b) > 0.25 { red += 1 }
        }
        return CGFloat(red) / CGFloat(w * h)
    }

    func inkCoverage(in rect: CGRect, threshold: CGFloat = 0.6) -> CGFloat {
        let x = Int(rect.minX * scale)
        let y = Int(rect.minY * scale)
        let w = Int(rect.width * scale)
        let h = Int(rect.height * scale)
        guard w > 0, h > 0, x >= 0, y >= 0, x + w <= image.width, y + h <= image.height,
            let crop = image.cropping(to: CGRect(x: x, y: y, width: w, height: h))
        else { return 0 }
        var pixels = [UInt8](repeating: 0, count: w * h * 4)
        guard
            let context = pixels.withUnsafeMutableBytes({ buffer in
                CGContext(
                    data: buffer.baseAddress, width: w, height: h, bitsPerComponent: 8,
                    bytesPerRow: w * 4, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            })
        else { return 0 }
        context.draw(crop, in: CGRect(x: 0, y: 0, width: w, height: h))
        var dark = 0
        for i in stride(from: 0, to: pixels.count, by: 4) {
            let luma =
                (0.299 * CGFloat(pixels[i]) + 0.587 * CGFloat(pixels[i + 1])
                    + 0.114 * CGFloat(pixels[i + 2])) / 255
            if luma < threshold { dark += 1 }
        }
        return CGFloat(dark) / CGFloat(w * h)
    }

    /// `height` pixels of one column, as luma.
    private func rows(column: Int, from top: Int, height: Int) -> [CGFloat]? {
        guard height > 0, top >= 0, top + height <= image.height,
            let strip = image.cropping(
                to: CGRect(x: column, y: top, width: 1, height: height))
        else { return nil }
        var pixels = [UInt8](repeating: 0, count: height * 4)
        guard
            let context = pixels.withUnsafeMutableBytes({ buffer in
                CGContext(
                    data: buffer.baseAddress, width: 1, height: height, bitsPerComponent: 8,
                    bytesPerRow: 4, space: CGColorSpaceCreateDeviceRGB(),
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
            })
        else { return nil }
        context.draw(strip, in: CGRect(x: 0, y: 0, width: 1, height: height))
        return (0..<height).map { row in
            let i = row * 4
            return (0.299 * CGFloat(pixels[i]) + 0.587 * CGFloat(pixels[i + 1])
                + 0.114 * CGFloat(pixels[i + 2])) / 255
        }
    }
}
