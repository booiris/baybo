import SwiftUI
import UIKit

/// A system column down one side of a screen, where iOS 27.1 lays its vertical
/// bars. iPhone Duo's outer display reports a trailing column (insets top 0,
/// trailing 84) with the clock + camera reserved at its top and the tab bar a
/// rail at its foot; the inner display has the same on the right; a Split View
/// pane gets one on its OUTER edge (the left pane's tab rail runs down its left
/// side). The custom headers put their circle controls there too, on the line
/// the system's own vertical bars use; with nothing left at the top but glass,
/// the chat header drops its paper veil.
///
/// Measured, not keyed to a device: a column is an inset on ONE side with none
/// on the other. A notched phone in landscape has insets on both sides, so it
/// never qualifies.
struct SideRail: Equatable {
    var edge: HorizontalEdge
    var width: CGFloat
    /// The bar line's distance in from the column's inner edge. Not the
    /// column's middle: the system keeps its bars clear of the screen's curve.
    var center: CGFloat
    /// Where a bar may start: under the clock + camera block, if the column
    /// carries one.
    var top: CGFloat

    /// Narrower than a circle control plus breathing room is not a column.
    static let minWidth: CGFloat = 60
    static let spacing: CGFloat = 12
    /// The widest control the rail hosts — the extent the bar line is asked for.
    static let extent: CGFloat = 45

    /// `bounds` is the screen's frame including its insets, in window space;
    /// `bar` answers where the system would lay a bar against a screen edge, in
    /// the same space — nil leaves the column's middle and its top.
    static func resolve(
        bounds: CGRect, insets: UIEdgeInsets, bar: (UIRectEdge) -> CGRect?
    ) -> SideRail? {
        let edge: HorizontalEdge
        let width: CGFloat
        if insets.right >= minWidth, insets.left < 1 {
            edge = .trailing
            width = insets.right
        } else if insets.left >= minWidth, insets.right < 1 {
            edge = .leading
            width = insets.left
        } else {
            return nil
        }
        guard let line = bar(edge == .trailing ? .right : .left) else {
            return SideRail(edge: edge, width: width, center: width / 2, top: 0)
        }
        let inner = edge == .trailing ? bounds.maxX - width : bounds.minX + width
        let center = edge == .trailing ? line.midX - inner : inner - line.midX
        return SideRail(
            edge: edge, width: width, center: min(max(center, 0), width),
            top: max(0, line.minY - bounds.minY))
    }
}

/// Where the system lays a vertical bar against each edge of the window, in
/// window space. Window-level and screen-independent, so measured once at the
/// root; which edge a SCREEN has a column on is its own to say.
struct SideRailLines: Equatable {
    var left: CGRect?
    var right: CGRect?

    func line(_ edge: UIRectEdge) -> CGRect? {
        edge == .left ? left : right
    }
}

extension EnvironmentValues {
    @Entry var sideRailLines = SideRailLines()
    @Entry var sideRail: SideRail? = nil
}

extension View {
    /// Measures the window this view fills (place it full-bleed at the root):
    /// publishes its bar lines, and lifts every screen off a bare top edge.
    func publishesSideRailLines() -> some View {
        modifier(SideRailLinesPublisher())
    }

    /// Resolves the side rail of the SCREEN this view is the root of and
    /// publishes it to the screen's headers. Per screen because the column is
    /// handed out per screen: in a Split View pane the window has no inset at
    /// all, and only the pushed screen's hosting controller carries the 84pt
    /// leading column its navigation container reserves.
    func resolvesSideRail() -> some View {
        modifier(SideRailResolver())
    }

    /// Hosts `items` as a vertical stack on the side rail's bar line, under
    /// its reserved block. Attach to a header whose side edge is the column's
    /// inner edge; the header leaves those controls out of its own row when
    /// `rail` is set.
    func sideRailItems<Items: View>(_ rail: SideRail?, @ViewBuilder _ items: () -> Items) -> some View {
        modifier(SideRailItems(rail: rail, items: items()))
    }
}

private struct SideRailResolver: ViewModifier {
    @Environment(\.sideRailLines) private var lines
    @State private var screen = ScreenSpan()

    /// The screen's frame (inside its safe area) and the insets around it.
    private struct ScreenSpan: Equatable {
        var frame: CGRect = .zero
        var insets = EdgeInsets()
    }

    func body(content: Content) -> some View {
        let insets = UIEdgeInsets(
            top: screen.insets.top, left: screen.insets.leading,
            bottom: screen.insets.bottom, right: screen.insets.trailing)
        let bounds = screen.frame.inset(by: UIEdgeInsets(
            top: -insets.top, left: -insets.left, bottom: -insets.bottom, right: -insets.right))
        content
            .environment(
                \.sideRail,
                screen.frame == .zero
                    ? nil : SideRail.resolve(bounds: bounds, insets: insets, bar: lines.line))
            .onGeometryChange(for: ScreenSpan.self) { proxy in
                ScreenSpan(frame: proxy.frame(in: .global), insets: proxy.safeAreaInsets)
            } action: { screen = $0 }
    }
}

private struct SideRailItems<Items: View>: ViewModifier {
    let rail: SideRail?
    let items: Items
    /// The host's top in window space — `rail.top` is measured there, and a
    /// header sits wherever its screen's safe area put it.
    @State private var hostTop: CGFloat = 0

    func body(content: Content) -> some View {
        content
            .onGeometryChange(for: CGFloat.self) { proxy in
                proxy.frame(in: .global).minY
            } action: { top in
                hostTop = top
            }
            .overlay(alignment: rail?.edge == .leading ? .topLeading : .topTrailing) {
                if let rail {
                    // Centred in a box twice the line's inset, then slid one box
                    // width outward: the centre lands on the line.
                    VStack(spacing: SideRail.spacing) { items }
                        .frame(width: 2 * rail.center)
                        .padding(.top, max(0, rail.top + SideRail.spacing - hostTop))
                        .offset(x: rail.edge == .trailing ? 2 * rail.center : -2 * rail.center)
                }
            }
    }
}

private struct SideRailLinesPublisher: ViewModifier {
    @State private var lines = SideRailLines()

    func body(content: Content) -> some View {
        content
            .environment(\.sideRailLines, lines)
            .background {
                SideRailProbe { new in
                    if new != lines { lines = new }
                }
                .ignoresSafeArea()
                .allowsHitTesting(false)
            }
    }
}

private struct SideRailProbe: UIViewRepresentable {
    let onChange: (SideRailLines) -> Void

    func makeUIView(context: Context) -> ProbeView {
        let view = ProbeView()
        view.onChange = onChange
        return view
    }

    func updateUIView(_ view: ProbeView, context: Context) {
        view.onChange = onChange
    }

    final class ProbeView: UIView {
        var onChange: ((SideRailLines) -> Void)?

        override func layoutSubviews() {
            super.layoutSubviews()
            report()
        }

        override func safeAreaInsetsDidChange() {
            super.safeAreaInsetsDidChange()
            report()
        }

        /// Runs from `layoutSubviews`, which UIKit re-runs when anything it
        /// read changes — the bar region settles a pass after the insets.
        private func report() {
            guard let window else { return }
            let lines = SideRailLines(
                left: Self.barLine(in: window, edge: .left),
                right: Self.barLine(in: window, edge: .right))
            // A window with no top inset on a runtime that lays bars down the
            // side: the bar steps down off the bare top edge, as the system's
            // own bars do. Through the ROOT controller, the way a status bar
            // inset arrives: every pushed screen, tab and webview (its
            // `safe-area-inset-top`) inherits it, so each header and every
            // clearance measured from one move together. SwiftUI's
            // `safeAreaPadding` stops at the navigation and tab containers.
            if let root = window.rootViewController {
                let lift = window.safeAreaInsets.top < 1 && lines.right != nil ? SideRail.spacing : 0
                if root.additionalSafeAreaInsets.top != lift {
                    root.additionalSafeAreaInsets.top = lift
                }
            }
            DispatchQueue.main.async { [onChange] in onChange?(lines) }
        }

        /// The query is 27.1-SDK-only; CI builds with Xcode 26.5 (Swift 6.3),
        /// and no earlier runtime has a column to measure.
        private static func barLine(in window: UIWindow, edge: UIRectEdge) -> CGRect? {
            #if compiler(>=6.4)
                if #available(iOS 27.1, *) {
                    let insets = window.edgeInsets(for: .bar(onEdge: edge, extent: SideRail.extent))
                    return window.bounds.inset(by: insets)
                }
            #endif
            return nil
        }
    }
}
