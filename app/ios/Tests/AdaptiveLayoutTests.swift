import CoreGraphics
import SwiftUI
import UIKit
import Testing

@testable import Baybo

/// The measured-layout rules a resizable window leans on — a foldable's outer
/// display (~466×678pt, landscape honoured) and inner one (~951×669pt), a Split
/// View half, a phone in landscape. Each is a pure function of a measurement, so
/// the numbers are pinned here instead of at the simulator tier, where only the
/// device sizes someone remembered to boot ever get looked at.
@Suite @MainActor
struct ReadingWidthTests {
    /// A phone never reaches the band, so nothing about its layout moves.
    @Test(arguments: [CGFloat(0), 402, 466, 720])
    func narrowerThanTheBandHasNoMargin(width: CGFloat) {
        #expect(Theme.readingMargin(in: width) == 0)
    }

    /// The inner display centres a 720pt band in what is left.
    @Test func aWideWindowCentresTheBand() {
        let margin = Theme.readingMargin(in: 951)
        #expect(margin == CGFloat(951 - 720) / 2)
        #expect(951 - 2 * margin == Theme.readingWidth)
    }
}

@Suite @MainActor
struct ApprovalDetailCapTests {
    /// Unmeasured — the first frame, before the host reports a height — keeps
    /// the box a phone has always had.
    @Test func unmeasuredIsTheCeiling() {
        #expect(ApprovalCardView.detailCap(available: 0) == ApprovalCardView.detailCeiling)
    }

    /// A phone in portrait, keyboard up or down, is tall enough to keep it too.
    @Test(arguments: [CGFloat(480), 669, 800])
    func aTallWindowKeepsTheCeiling(available: CGFloat) {
        #expect(ApprovalCardView.detailCap(available: available) == ApprovalCardView.detailCeiling)
    }

    /// A short window gives the detail a share of what is left, so the card
    /// can't shove the pill off the bottom.
    @Test func aShortWindowTakesItsShare() {
        let cap = ApprovalCardView.detailCap(available: 300)
        #expect(cap == 300 * ApprovalCardView.detailShare)
        #expect(cap < ApprovalCardView.detailCeiling)
    }

    /// …but never below the floor: a scroller a line tall reads as nothing.
    @Test(arguments: [CGFloat(1), 100, 150])
    func aTinyWindowHoldsTheFloor(available: CGFloat) {
        #expect(ApprovalCardView.detailCap(available: available) == ApprovalCardView.detailFloor)
    }
}

@Suite @MainActor
struct ModelMenuLayoutTests {
    /// Rows that fit are left alone — no scroller, no fade, no clipped height.
    @Test func rowsThatFitDoNotScroll() {
        #expect(ModelMenuPanel.scrollHeight(rows: 300, available: 500) == nil)
        #expect(ModelMenuPanel.scrollHeight(rows: 500, available: 500) == nil)
    }

    /// Unmeasured never scrolls: the first frame must not flash a scroller.
    @Test func unmeasuredDoesNotScroll() {
        #expect(ModelMenuPanel.scrollHeight(rows: 900, available: 0) == nil)
        #expect(ModelMenuPanel.scrollHeight(rows: 900, available: -40) == nil)
    }

    /// A long list on a short window scrolls inside exactly the room there is.
    @Test func overflowingRowsScrollInTheRoomLeft() {
        #expect(ModelMenuPanel.scrollHeight(rows: 600, available: 220) == 220)
    }

    /// …and keeps at least one row's worth, however little is left.
    @Test func aScrollerKeepsOneRow() {
        #expect(ModelMenuPanel.scrollHeight(rows: 600, available: 10) == ModelMenuPanel.minScrollHeight)
    }

    /// The panel sits on the PILL's leading edge, read in the panel layer's own
    /// space — the layer may itself be inset (a landscape notch, a Split View
    /// half), so the pill's raw x is not the answer.
    @Test func thePanelHangsOffThePill() {
        let bounds = CGRect(x: 59, y: 0, width: 600, height: 400)
        let pill = CGRect(x: 59 + 78, y: 2, width: 120, height: 42)
        #expect(ModelMenuPanel.leadingInset(anchor: pill, bounds: bounds, panelWidth: 232) == 78)
    }

    /// A pill too far right for the panel to fit after it keeps the panel inside
    /// the layer rather than off its trailing edge.
    @Test func thePanelStaysInsideANarrowLayer() {
        let bounds = CGRect(x: 0, y: 0, width: 300, height: 400)
        let pill = CGRect(x: 200, y: 2, width: 80, height: 42)
        #expect(ModelMenuPanel.leadingInset(anchor: pill, bounds: bounds, panelWidth: 232) == 68)
    }

    /// An unreported pill (`.zero`) parks the panel at the layer's edge, never
    /// at a negative inset.
    @Test func anUnreportedPillIsNotNegative() {
        let bounds = CGRect(x: 40, y: 0, width: 600, height: 400)
        #expect(ModelMenuPanel.leadingInset(anchor: .zero, bounds: bounds, panelWidth: 232) == 0)
    }

    /// A pill in the bar opens the panel under the bar, from its leading edge.
    @Test func aBarPillOpensUnderTheBar() {
        let bounds = CGRect(x: 0, y: 0, width: 402, height: 700)
        let pill = CGRect(x: 78, y: 2, width: 120, height: 42)
        #expect(ModelMenuPanel.topInset(anchor: pill, bounds: bounds) == ModelMenuPanel.panelTop)
        #expect(!ModelMenuPanel.bloomsFromTrailing(anchor: pill, bounds: bounds))
    }

    /// The outer display's rail circle sits past the layer's trailing edge: the
    /// panel opens level with it, flush against the column.
    @Test func aRailCircleOpensThePanelBesideIt() {
        let bounds = CGRect(x: 0, y: 12, width: 382, height: 600)
        let circle = CGRect(x: 397, y: 294, width: 42, height: 42)
        #expect(ModelMenuPanel.topInset(anchor: circle, bounds: bounds) == 282)
        #expect(ModelMenuPanel.leadingInset(anchor: circle, bounds: bounds, panelWidth: 232) == 150)
        #expect(ModelMenuPanel.bloomsFromTrailing(anchor: circle, bounds: bounds))
    }
}

@Suite @MainActor
struct SideRailTests {
    private let outer = CGRect(x: 0, y: 0, width: 466, height: 678)
    private let column = UIEdgeInsets(top: 0, left: 0, bottom: 34, right: 84)
    /// What the 27.1 runtime answers for a 45pt bar on the outer display's
    /// right edge: under the 174pt clock + camera block, centred at x 418.
    private let measuredBar = CGRect(x: 395.5, y: 174, width: 45, height: 470)

    @Test func theOuterDisplayRailSitsOnTheSystemBarLine() {
        let rail = SideRail.resolve(bounds: outer, insets: column) { edge in
            edge == .right ? measuredBar : nil
        }
        #expect(rail == SideRail(edge: .trailing, width: 84, center: 36, top: 174))
    }

    /// Before 27.1 (or before the region settles) the column still exists; it
    /// falls back to its middle and its top.
    @Test func withoutABarLineTheColumnsMiddleIsUsed() {
        let rail = SideRail.resolve(bounds: outer, insets: column) { _ in nil }
        #expect(rail == SideRail(edge: .trailing, width: 84, center: 42, top: 0))
    }

    @Test func aLeadingColumnMeasuresInwardFromTheLeft() {
        let insets = UIEdgeInsets(top: 0, left: 84, bottom: 34, right: 0)
        let rail = SideRail.resolve(bounds: outer, insets: insets) { edge in
            edge == .left ? CGRect(x: 25.5, y: 174, width: 45, height: 470) : nil
        }
        #expect(rail == SideRail(edge: .leading, width: 84, center: 36, top: 174))
    }

    @Test(arguments: [
        UIEdgeInsets(top: 62, left: 0, bottom: 34, right: 0),   // portrait phone
        UIEdgeInsets(top: 0, left: 62, bottom: 21, right: 62),  // notched phone in landscape
        UIEdgeInsets(top: 0, left: 0, bottom: 20, right: 0),    // inner display, no column
        UIEdgeInsets(top: 0, left: 0, bottom: 34, right: 24),   // a sliver is not a column
    ])
    func noRailWithoutAOneSidedColumn(insets: UIEdgeInsets) {
        #expect(SideRail.resolve(bounds: outer, insets: insets) { _ in nil } == nil)
    }
}
