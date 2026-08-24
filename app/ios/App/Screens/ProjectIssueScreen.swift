import SwiftUI

/// One card, pushed over its board.
///
/// The body is an `issue.html` webview and the dock is native — both land in
/// P5, see `docs/projects.md` §3.3. This shell exists so the route is real and
/// the pop chain (card → board → cards) can be walked now.
struct ProjectIssueScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    let projectId: String
    let number: Int64

    var body: some View {
        ZStack(alignment: .top) {
            VStack(spacing: 10) {
                Spacer()
                Text(verbatim: lang.t("board.comingSoon"))
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            header
        }
        .background(Theme.paper)
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
    }

    private var header: some View {
        ZStack {
            Text(verbatim: "#\(number)")
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)

            HStack {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("board.back")))
                Spacer()
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) {
            LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .top)
                .allowsHitTesting(false)
        }
    }
}
