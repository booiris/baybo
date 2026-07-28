import Combine
import Foundation
import Testing

@testable import Baybo

/// The attach panel's two pieces of logic that live below the view: where the
/// panel lands, and when the anchor that decides it is worth publishing.
///
/// Every case works in the DOCK's own coordinate space — the one the `+`
/// reports its frame in and the one the panel is laid out in — so the dock's
/// top edge is `y = 0` and the panel's box, floating above it, is negative. The
/// `+`'s x is what cannot be hardcoded the way the header's can
/// (`ModelMenuPanel.anchorLeading`): the composer pill's horizontal padding
/// animates between at-rest and focused. Its Y is measured too, but only as a
/// witness: the panel's floor is the DOCK's top edge, and these cases are what
/// hold it there while the `+` sinks under a notice line, an approval card and
/// a staged strip.
@Suite @MainActor
struct AttachMenuTests {
    /// The `+` in a BARE dock — the pill alone, so it sits on the dock's own
    /// top edge.
    private static let plus = CGRect(x: 40, y: 0, width: 46, height: 48)
    /// The same `+` under the 4-tile staged strip `-baybo-demo-compose` seeds.
    /// It has not moved on SCREEN — the dock grew upward around it — so in the
    /// dock's own space it is 76pt down from the top edge.
    private static let plusUnderStrip = CGRect(x: 40, y: 76, width: 46, height: 48)
    /// And under a blocked tool call, the tallest thing the dock ever carries.
    private static let plusUnderApproval = CGRect(x: 40, y: 200, width: 46, height: 48)
    /// Two 46pt rows. Written out rather than read from `AttachMenuPanel` so a
    /// row growing is a FAILURE here rather than a silently taller panel: the
    /// panel is positioned by an offset that needs its height up front, so the
    /// constant and the rows must not drift apart.
    private static let panelHeight: CGFloat = 92

    @Test func theLeadingEdgeSitsOnThePlus() {
        let box = AttachMenuPanel.box(anchor: Self.plus)
        #expect(box.minX == Self.plus.minX)
        #expect(box.width == AttachMenuPanel.panelWidth)
    }

    /// The rule the panel exists to satisfy: its bottom edge clears the DOCK's
    /// top edge — `y = 0` here — whatever the dock has grown, and whatever the
    /// panel's own height turns out to be.
    ///
    /// Clearing the `+` instead (`anchor.minY - anchorGap`) is the defect this
    /// replaces: the `+` sinks INTO the dock as the notice line, the approval
    /// card and the staged strip stack above it, and a panel hung off it lands
    /// behind them.
    /// NOTE ON WHAT THIS TIER CANNOT SEE. These cases pin the pure function
    /// only. A version of the panel positioned with `.alignmentGuide` inside an
    /// `.overlay(alignment:)` kept every assertion here green while the VIEW
    /// discarded `box()` entirely — an overlay pins its child to the named
    /// corner and never resolves those guides — and the panel opened inside the
    /// dock at x = 0 in all seven dock configurations. What catches that is the
    /// UI tier asserting the rendered frame (`ComposerAttachUITests`), not this
    /// one.
    @Test func theBottomEdgeClearsTheDock() {
        for anchor in [Self.plus, Self.plusUnderStrip, Self.plusUnderApproval] {
            let box = AttachMenuPanel.box(anchor: anchor)
            #expect(box.maxY == -AttachMenuPanel.anchorGap)
            #expect(box.maxY < 0)
            #expect(box.height == Self.panelHeight)
        }
    }

    /// The panel's height must be exactly the rows it is built from, because
    /// the offset that positions it is computed from the constant rather than
    /// from the laid-out view.
    @Test func theHeightIsTheRowsItIsBuiltFrom() {
        #expect(AttachMenuPanel.panelHeight == Self.panelHeight)
        #expect(
            AttachMenuPanel.panelHeight
                == AttachMenuPanel.rowHeight * CGFloat(AttachSource.allCases.count))
    }

    /// Focusing the field animates the pill's padding from 40 to 14: the panel
    /// follows the `+` sideways, which is the whole reason the anchor is
    /// measured at all.
    @Test func thePanelFollowsThePlusSideways() {
        let resting = AttachMenuPanel.box(anchor: Self.plus)
        let focused = AttachMenuPanel.box(
            anchor: Self.plus.offsetBy(dx: -26, dy: 0))
        #expect(focused.minX == resting.minX - 26)
    }

    /// …and does NOT follow it down. The keyboard, a notice line and a staged
    /// strip all move the `+` within the dock; none of them may pull the panel
    /// into the dock behind them.
    @Test func thePanelDoesNotFollowThePlusDown() {
        let resting = AttachMenuPanel.box(anchor: Self.plus)
        let sunk = AttachMenuPanel.box(
            anchor: Self.plusUnderApproval)
        #expect(sunk.minY == resting.minY)
        #expect(sunk.maxY == resting.maxY)
    }

    /// A `+` at or left of the dock's leading edge (nothing real, but a frame
    /// arriving before layout settles is) must not push the panel off-screen.
    @Test func aDegenerateAnchorClampsToZero() {
        let box = AttachMenuPanel.box(anchor: .zero)
        #expect(box.minX == 0)
        #expect(box.maxY == -AttachMenuPanel.anchorGap)
    }

    /// The `+` reports its frame on every tick of the focus and keyboard
    /// animations. While the panel is down nobody is looking at it, and
    /// republishing would recompute the whole chat screen per tick — the same
    /// bargain `TranscriptBridge.setComposerTop` makes by publishing nothing.
    @Test func theAnchorIsQuietWhileTheMenuIsDown() {
        let menu = AttachMenu()
        var updates = 0
        let subscription = menu.objectWillChange.sink { _ in updates += 1 }

        menu.report(anchor: Self.plus)
        #expect(menu.anchor == Self.plus)
        #expect(updates == 0)

        withExtendedLifetime(subscription) {}
    }

    /// Up, it has to track: the field can still be focused under the panel, and
    /// that moves the `+` the panel is hanging off.
    @Test func anOpenMenuTracksTheAnchor() {
        let menu = AttachMenu()
        menu.report(anchor: Self.plus)
        menu.isOpen = true

        var updates = 0
        let subscription = menu.objectWillChange.sink { _ in updates += 1 }

        menu.report(anchor: Self.plus.offsetBy(dx: -26, dy: 0))
        #expect(updates == 1)
        // An unchanged frame is not a change: the tick fires either way.
        menu.report(anchor: Self.plus.offsetBy(dx: -26, dy: 0))
        #expect(updates == 1)

        withExtendedLifetime(subscription) {}
    }
}
