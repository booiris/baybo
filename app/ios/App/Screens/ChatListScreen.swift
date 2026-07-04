import SwiftUI

/// Home: the chat list. Local-first — rows render from the device's
/// `SessionIndex` instantly (both legs); a direct binding refreshes it from
/// REST on appear, on foreground, and by pull. Rows push a `ChatScreen` onto
/// the NavigationStack; compose mints a session and enters it.
struct ChatListScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var index = SessionIndex.shared
    @ObservedObject private var lang = Lang.shared
    @Environment(\.scenePhase) private var scenePhase
    @State private var confirmLogout = false
    /// Transient compose/refresh failure line (localized, already resolved).
    @State private var notice: String?

    /// Clearance for the overlaid header: bar height + a breath.
    private static let topContentMargin: CGFloat = 58

    var body: some View {
        ZStack(alignment: .top) {
            if index.sorted.isEmpty {
                emptyState
            } else {
                sessionList
            }

            ListHeaderView(
                notice: notice,
                onLogout: { confirmLogout = true },
                onCompose: compose
            )
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
        .confirmationDialog(
            Text(verbatim: Lang.shared.t("connected.logoutConfirm")),
            isPresented: $confirmLogout, titleVisibility: .visible
        ) {
            Button(role: .destructive) {
                Task { await appStore.logout() }
            } label: {
                Text(verbatim: Lang.shared.t("connected.logout"))
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
        if now.timeIntervalSince(date) < Self.justNowThreshold {
            return justNow
        }
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = Locale(identifier: locale)
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: now)
    }
}

/// The list's paper-veil header, mirroring the chat header's grammar: centered
/// wordmark, flanking glass circles — logout left (it moved here from the chat
/// screen; leaving the account lives one level up now), compose right.
struct ListHeaderView: View {
    let notice: String?
    let onLogout: () -> Void
    let onCompose: () -> Void

    private static let barHeight: CGFloat = 46

    var body: some View {
        VStack(spacing: 6) {
            ZStack {
                // The landing wordmark, header-scaled.
                Text(verbatim: "Baybo")
                    .font(Theme.mono(17))
                    .textCase(.uppercase)
                    .kerning(5)
                    .padding(.leading, 5)
                    .foregroundStyle(Theme.ink)

                HStack {
                    Button(action: onLogout) {
                        Image(systemName: "rectangle.portrait.and.arrow.right")
                            .font(.system(size: 16, weight: .medium))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassEffect(.regular.interactive(), in: .circle)
                    .accessibilityLabel(Text(verbatim: Lang.shared.t("connected.logout")))

                    Spacer()

                    Button(action: onCompose) {
                        Image(systemName: "square.and.pencil")
                            .font(.system(size: 16, weight: .medium))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassEffect(.regular.interactive(), in: .circle)
                    .accessibilityLabel(Text(verbatim: Lang.shared.t("list.newChat")))
                }
            }
            .padding(.horizontal, 24)
            .frame(height: Self.barHeight)

            if let notice {
                Text(verbatim: notice)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.err)
                    .padding(.horizontal, 24)
            }
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    /// The chat header's veil, reused: solid through the status bar, easing to
    /// clear at the bar's bottom so rows ghost past underneath.
    private var veil: some View {
        LinearGradient(
            stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom
        )
        .ignoresSafeArea(edges: .top)
        .allowsHitTesting(false)
    }
}
