import SwiftUI

/// The Chats section: the chat list, the root of the Chats tab's
/// NavigationStack. Local-first — rows render from the device's `SessionIndex`
/// instantly (both legs); a direct binding refreshes it from REST on appear, on
/// foreground, and by pull. Rows push a `ChatScreen`; the header's compose
/// button (top-right) mints a session and enters it.
struct ChatListScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var index = SessionIndex.shared
    @ObservedObject private var lang = Lang.shared
    @Environment(\.scenePhase) private var scenePhase
    /// Transient compose-failure line (localized, already resolved).
    @State private var notice: String?

    /// Clearance for the overlaid header: bar height + a breath. (The native tab
    /// bar's bottom inset is handled by the system, so no bottom margin here.)
    private static let topContentMargin: CGFloat = 58

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if index.sorted.isEmpty {
                    emptyState
                } else {
                    sessionList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            HomeHeaderView(notice: notice, onCompose: compose)
        }
        .background(Theme.paper)
        .task {
            await refresh()
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active {
                Task { await refresh() }
            }
        }
    }

    private var sessionList: some View {
        List {
            ForEach(index.sorted) { row in
                Button {
                    appStore.openSession(row.id)
                } label: {
                    SessionRowView(
                        row: row,
                        langCode: lang.current.lproj,
                        justNow: lang.t("list.justNow")
                    )
                }
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(EdgeInsets(top: 0, leading: 24, bottom: 0, trailing: 24))
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, Self.topContentMargin, for: .scrollContent)
        .refreshable {
            await refresh()
        }
    }

    private var emptyState: some View {
        VStack(spacing: 24) {
            Spacer()
            Text(verbatim: lang.t("list.empty"))
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
            Button {
                compose()
            } label: {
                Text(verbatim: lang.t("list.newChat"))
            }
            .buttonStyle(InkPillButtonStyle())
            .frame(maxWidth: 260)
            .disabled(appStore.busy)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private func compose() {
        guard !appStore.busy else { return }
        Haptics.tap()
        notice = nil
        Task {
            notice = await appStore.startNewChat()
        }
    }

    /// Merge the gateway's list over the local registry — direct binding only
    /// (relay has no listing capability; its local rows are already the truth).
    /// Failures stay quiet: the local rows keep rendering, which is the whole
    /// point of local-first.
    private func refresh() async {
        guard appStore.directBound else { return }
        do {
            let items = try await Baybo.client.chatListSessions()
            SessionIndex.shared.merge(remote: items)
        } catch {
            NSLog("baybo: session list refresh: %@", bayboErrorText(error))
        }
    }
}

/// One list row: the last-user-text preview over a pin + relative-age line.
struct SessionRowView: View {
    let row: SessionRow
    /// The app language's locale identifier (drives the age formatter, so it
    /// can't diverge from the chrome language).
    let langCode: String
    let justNow: String

    private static let justNowThreshold: TimeInterval = 5
    private static let absoluteTimeThreshold: TimeInterval = 24 * 60 * 60

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(verbatim: row.lastUserText ?? Lang.shared.t("list.previewPlaceholder"))
                .font(Theme.mono(15))
                .foregroundStyle(row.lastUserText == nil ? Theme.inkSoft : Theme.ink)
                .lineLimit(2)
                .multilineTextAlignment(.leading)
            HStack(spacing: 6) {
                if row.pinned {
                    Image(systemName: "pin.fill")
                        .font(.system(size: 9, weight: .medium))
                        .foregroundStyle(Theme.inkSoft)
                }
                Text(verbatim: Self.age(of: row.lastActive, locale: langCode, justNow: justNow))
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 14)
        .contentShape(Rectangle())
    }

    private static func age(
        of date: Date,
        locale: String,
        justNow: String,
        relativeTo now: Date = Date()
    ) -> String {
        let elapsed = now.timeIntervalSince(date)
        if elapsed < Self.justNowThreshold {
            return justNow
        }
        if elapsed >= Self.absoluteTimeThreshold {
            let formatter = DateFormatter()
            formatter.locale = Locale(identifier: locale)
            let sameYear = Calendar.current.isDate(date, equalTo: now, toGranularity: .year)
            formatter.setLocalizedDateFormatFromTemplate(sameYear ? "Mdjm" : "yMdjm")
            return formatter.string(from: date)
        }
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = Locale(identifier: locale)
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: now)
    }
}
