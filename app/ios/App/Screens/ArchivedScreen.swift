import SwiftUI

/// The archived chat list, pushed on the outer NavigationStack from the Chats
/// header's ☰ menu — it covers the tab bar and pops with the edge swipe, like
/// `ChatScreen`. Same row rendering as the main list; swiping offers unarchive
/// (edge-most, full-swipe) and delete (same confirm dialog). Tapping a row
/// appends the conversation, so the pop chain runs chat → archived → list.
struct ArchivedScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var index = SessionIndex.shared
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    /// Hidden rows never reach the device list, so archived ∧ ¬hidden needs no
    /// extra filtering. The pinned-first + recency sort carries over — an
    /// archived pinned row keeps its pin.
    private var archivedRows: [SessionRow] {
        index.sorted.filter(\.archived)
    }

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if archivedRows.isEmpty {
                    emptyState
                } else {
                    sessionList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            header
        }
        .background(Theme.paper)
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        .background(PopGestureEnabler().frame(width: 0, height: 0))
    }

    /// Back chevron + centered title over the shared paper veil — the chat
    /// header's grammar without the connection capsule.
    private var header: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(verbatim: lang.t("archived.title"))
                    .font(Theme.mono(16))
                    .foregroundStyle(Theme.ink)

                HStack {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "chevron.left")
                            .font(.system(size: 18, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassEffect(.regular.interactive(), in: .circle)
                    .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

                    Spacer()
                }
            }
            .padding(.horizontal, 24)
            .frame(height: ChatHeaderView.barHeight)

            if let notice = appStore.sessionNotice {
                Text(verbatim: notice)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.err)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
            }
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    private var sessionList: some View {
        List {
            ForEach(archivedRows) { row in
                Button {
                    appStore.openArchivedSession(row.id)
                } label: {
                    SessionRowView(
                        row: row,
                        langCode: lang.current.lproj
                    )
                }
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(EdgeInsets(top: 0, leading: 24, bottom: 0, trailing: 24))
                // First action = edge-most = full-swipe commit: unarchive. No
                // toast — the row leaving the list is the feedback.
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        unarchive(row)
                    } label: {
                        Label(lang.t("list.unarchive"), systemImage: "tray.and.arrow.up")
                    }
                    .tint(Theme.inkSoft)
                    Button(role: .destructive) {
                        appStore.promptDeleteSession(row.id)
                    } label: {
                        Label(lang.t("list.delete"), systemImage: "trash")
                    }
                    .tint(Theme.err)
                }
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, ChatListScreen.topContentMargin, for: .scrollContent)
        .scrollBounceBehavior(.always)
    }

    private var emptyState: some View {
        VStack {
            Spacer()
            Text(verbatim: lang.t("archived.empty"))
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private func unarchive(_ row: SessionRow) {
        Haptics.tap()
        withAnimation(.easeOut(duration: 0.2)) {
            appStore.requestArchive(row.id, archived: false)
        }
    }
}
