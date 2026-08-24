import SwiftUI

/// One board, pushed over the cards root.
///
/// The stage wall, the Waiting-on-you strip and the card rows land in P4 — see
/// `docs/projects.md` §3.1. What is here is the shell those hang off: the
/// pushed-screen header, the pop gesture, and the board fetch, so the
/// navigation is real from the cards root down.
struct ProjectBoardScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    let projectId: String

    private var project: ProjectInfo? {
        appStore.projectsStore.projects.first { $0.id == projectId }
    }

    var body: some View {
        ZStack(alignment: .top) {
            body(for: appStore.projectsStore.boards[projectId])
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            header
        }
        .background(Theme.paper)
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task { await appStore.projectsStore.refreshBoard(projectId) }
    }

    @ViewBuilder private func body(for board: ProjectsStore.Board?) -> some View {
        VStack(spacing: 10) {
            Spacer()
            Text(verbatim: lang.t("board.comingSoon"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
            if let board {
                Text(verbatim: lang.t("board.cardCount", "\(BoardOrder.liveCount(board.issues))"))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .accessibilityIdentifier("board-card-count")
            }
            Spacer()
        }
        .padding(.horizontal, 40)
    }

    private var header: some View {
        ZStack {
            Text(verbatim: project?.name ?? lang.t("home.tab.projects"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                .padding(.horizontal, 66)

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
