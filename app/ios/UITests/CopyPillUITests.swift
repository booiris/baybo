import XCTest

/// Long-press-to-copy's confirmation pill, sampled as PIXELS.
///
/// The pill is the transcript's one piece of chrome that deliberately paints
/// OUTSIDE its own row — it hangs in the gap above the bubble. That makes it the
/// one thing a row-level containment change can erase with nothing noticing:
/// `content-visibility: auto` on `.msg-group` carries paint containment even
/// while the row is on screen, and it cut the pill to zero visible pixels on
/// every text-only send. The node was in the DOM, its rect was right, and every
/// assertion an existence or hittability check can make still passed. Only the
/// raster was gone. So this reads ink out of the band the pill occupies, before
/// and after the press — the "before" reading is what stops a neighbour's ink
/// from passing the test on the pill's behalf.
final class CopyPillUITests: BayboUITestCase {
    private static let chatArguments = ["-baybo-open-chat", "-baybo-demo-index"]
    /// The fixture's LAST user send (`DemoFrames.demoIndexTurns`). The thread
    /// opens pinned to its newest edge, so this bubble sits far from the header
    /// — where the pill takes its DEFAULT placement, above the bubble.
    private static let lastSend = "Draft the PR body for it"
    /// The pill's band, offset from the frame WebKit publishes for the send —
    /// which is the text box, ~12pt inside the bubble's border box, so the pill
    /// (~19pt tall, riding ~6pt above that border box) lands around -38...-24.
    /// The window is widened at both ends and still clears the previous row,
    /// which ends a full `--chat-row-gap` (24pt) above the bubble.
    private static let bandTopOffset: CGFloat = -40
    private static let bandBottomOffset: CGFloat = -20
    /// Sampled about the send's horizontal centre, which is where the pill is
    /// centred — narrow enough to stay well inside its ~72pt width.
    private static let bandWidth: CGFloat = 40
    /// The pill is solid ink under a white label, so a band holding it reads far
    /// darker than this and an empty band far lighter. The GAP between the two
    /// readings is the assertion; the thresholds only have to sit inside it.
    private static let emptyMaxInk: CGFloat = 0.05
    private static let pillMinInk: CGFloat = 0.3

    func testLongPressPaintsTheCopiedPill() throws {
        let app = launch(Self.chatArguments)

        let bubble = app.staticTexts[Self.lastSend]
        XCTAssertTrue(
            bubble.waitForExistence(timeout: Self.webviewTimeout),
            "the demo thread's last user send never rendered")

        // Read the frame ONCE. Mounting the pill re-renders the row, and the
        // webview's accessibility tree is rebuilt under it — a re-query after
        // the press can miss the element outright.
        let frame = bubble.frame
        let band = CGRect(
            x: frame.midX - Self.bandWidth / 2,
            y: frame.minY + Self.bandTopOffset,
            width: Self.bandWidth,
            height: Self.bandBottomOffset - Self.bandTopOffset)

        let before = try XCTUnwrap(screenPixels(), "no screenshot").inkCoverage(in: band)
        XCTAssertLessThan(
            before, Self.emptyMaxInk,
            "the gap above the bubble is not empty paper — the sample band is aimed wrong, so "
                + "a reading after the press would prove nothing (ink: \(before))")

        // Past `LONG_PRESS_MS` (450) in gestures.ts, and short enough that the
        // 1300ms pill is still up when the screenshot is taken.
        bubble.press(forDuration: 0.7)

        let after = try XCTUnwrap(screenPixels(), "no screenshot").inkCoverage(in: band)
        attachScreenshot(app, name: "copy-pill")
        XCTAssertGreaterThan(
            after, Self.pillMinInk,
            "the copied pill painted nothing above the bubble — its row is clipping the mark "
                + "the pill deliberately hangs outside it (ink: \(after))")
    }
}
