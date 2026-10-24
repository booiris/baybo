import SwiftUI

/// The paper veil behind a screen's header. Under a status bar it is the
/// translucent wash that holds solid through the bar and fades out across the
/// header (`ChatHeaderView.veilStops`). With no status bar along the top (the
/// Duo's displays, a Split View pane) that wash shrank to the 12pt lift: its
/// solid zone ended above the header's middle, and text scrolling under the
/// title read through. There the veil is opaque behind the whole header and
/// fades out just below it instead.
struct HeaderVeil: View {
    @Environment(\.sideRailLines) private var lines

    /// How far below the header the bare-top veil fades to clear.
    static let bareFade: CGFloat = 20

    var body: some View {
        Group {
            if lines.bareTop {
                GeometryReader { proxy in
                    LinearGradient(
                        stops: Self.bareStops(height: proxy.size.height),
                        startPoint: .top, endPoint: .bottom)
                }
                .padding(.bottom, -Self.bareFade)
            } else {
                LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            }
        }
        .ignoresSafeArea(edges: .top)
        .allowsHitTesting(false)
    }

    /// Opaque down to the header's bottom edge, then the veil's smoothstep to
    /// clear across `bareFade`.
    static func bareStops(height: CGFloat) -> [Gradient.Stop] {
        guard height > bareFade else { return [.init(color: Theme.paper, location: 0)] }
        let solid = (height - bareFade) / height
        let ramp = ChatHeaderView.rampAlphas
        return [.init(color: Theme.paper, location: 0)]
            + ramp.enumerated().map { idx, alpha in
                .init(
                    color: Theme.paper.opacity(alpha),
                    location: solid + (1 - solid) * CGFloat(idx) / CGFloat(ramp.count - 1))
            }
    }
}
