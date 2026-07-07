import SwiftUI

/// The Chats section: the chat list, the root of the Chats tab's
/// NavigationStack. Local-first — rows render from the device's `SessionIndex`
/// instantly; the active binding refreshes it from the gateway on appear, on
/// foreground, and by pull. Rows push a `ChatScreen`; the header's compose
/// button (top-right) mints a session and enters it.
struct ChatListScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var index = SessionIndex.shared
    @ObservedObject private var lang = Lang.shared
    @Environment(\.scenePhase) private var scenePhase
    /// Transient compose-failure line (localized, already resolved).
    @State private var notice: String?
    /// The just-archived session the undo toast is offering to restore; nil
    /// hides the toast. Re-set (and the dismiss timer re-armed) per archive.
    @State private var undoSessionId: String?
    @State private var undoDismissTask: Task<Void, Never>?
    /// Self-drawn pull-to-refresh. The native List spinner is suppressed (we
    /// don't use `.refreshable`); the header ring is shown instead, only once the
    /// pull is released and the refresh starts. `pullPeak` is the max top-overscroll
    /// within the current drag and `dragging` gates the on-release threshold check.
    @State private var isRefreshing = false
    @State private var pullPeak: CGFloat = 0
    @State private var dragging = false

    /// Clearance for the overlaid header: bar height + a breath. (The native tab
    /// bar's bottom inset is handled by the system, so no bottom margin here.)
    /// Shared with `ArchivedScreen`, whose header reuses this chrome.
    static let topContentMargin: CGFloat = 58
    /// Top-overscroll (points) that, once released past it, fires a refresh.
    private static let pullThreshold: CGFloat = 72
    /// How long the undo toast lingers after an archive.
    private static let undoWindow: Duration = .seconds(3)
    /// Ground for a pinned row — a touch deeper than paper so the pinned block
    /// stands apart. Drawn INSIDE the row content (not `.listRowBackground`) so a
    /// pin flip never swaps the cell's background configuration mid-move — that
    /// swap is what blanked the incoming row and stalled the reorder. Constant
    /// background ⇒ the reorder is a pure move, which `List` slides cleanly.
    fileprivate static let pinnedRowTint = Color(red: 0.945, green: 0.945, blue: 0.945)
    /// Row content gutter. The pinned tint bleeds back out past it (negative
    /// padding in `SessionRowView`) so it fills edge-to-edge like the old ground.
    fileprivate static let rowHInset: CGFloat = 24
    /// Tint/badge cross-fade as a row (un)pins — its own timeline, riding
    /// alongside the positional glide rather than a hard swap.
    fileprivate static let pinTintFade: Animation = .easeOut(duration: 0.22)

    /// Archived rows live under the ☰ menu's Archived screen, not here.
    private var visibleRows: [SessionRow] {
        index.sorted.filter { !$0.archived }
    }

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if visibleRows.isEmpty {
                    emptyState
                } else {
                    sessionList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            HomeHeaderView(
                notice: notice ?? appStore.sessionNotice,
                onCompose: compose,
                onArchived: appStore.openArchived,
                isRefreshing: isRefreshing
            )
        }
        .overlay(alignment: .bottom) {
            if undoSessionId != nil {
                undoToast
                    .padding(.bottom, 12)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
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
        #if DEBUG
            .task {
                if ProcessInfo.processInfo.arguments.contains("-baybo-demo-refresh") {
                    isRefreshing = true
                }
            }
        #endif
    }

    private var sessionList: some View {
        List {
            ForEach(visibleRows) { row in
                Button {
                    appStore.openSession(row.id)
                } label: {
                    SessionRowView(
                        row: row,
                        langCode: lang.current.lproj,
                        justNow: lang.t("list.justNow")
                    )
                }
                // CONSTANT background — the pinned tint lives in the row content
                // (see SessionRowView), so a pin flip is a pure move, not a
                // background-config swap that would blank the sliding row.
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(
                    EdgeInsets(top: 0, leading: Self.rowHInset, bottom: 0, trailing: Self.rowHInset))
                // Native swipe (real UIKit feel): first action = edge-most =
                // the full-swipe commit (archive). Grey archive, red delete.
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        archive(row)
                    } label: {
                        Label(lang.t("list.archive"), systemImage: "archivebox")
                    }
                    .tint(Theme.inkSoft)
                    Button(role: .destructive) {
                        appStore.promptDeleteSession(row.id)
                    } label: {
                        Label(lang.t("list.delete"), systemImage: "trash")
                    }
                    .tint(Theme.err)
                }
                // Leading (right-)swipe toggles pin: a pinned row offers unpin.
                // No full-swipe — the button must be tapped (pin isn't
                // destructive enough to want an accidental fling committing it).
                .swipeActions(edge: .leading, allowsFullSwipe: false) {
                    Button {
                        togglePin(row)
                    } label: {
                        Label(
                            lang.t(row.pinned ? "list.unpin" : "list.pin"),
                            systemImage: row.pinned ? "pin.slash.fill" : "pin.fill")
                    }
                    .tint(Theme.ink)
                }
            }
        }
        .listStyle(.plain)
        // Glide only pin-driven reorders; a refresh/activity reshuffle keeps its
        // instant snap (the tick changes solely inside `requestPin`).
        .animation(AppStore.pinReorderMotion, value: appStore.pinReorderTick)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, Self.topContentMargin, for: .scrollContent)
        .scrollBounceBehavior(.always)
        .onScrollGeometryChange(for: CGFloat.self) { geo in
            max(0, -(geo.contentOffset.y + geo.contentInsets.top))
        } action: { _, overscroll in
            if dragging && overscroll > pullPeak { pullPeak = overscroll }
        }
        .onScrollPhaseChange { _, phase in
            switch phase {
            case .tracking, .interacting:
                if !isRefreshing {
                    dragging = true
                    pullPeak = 0
                }
            default:
                if dragging {
                    dragging = false
                    if pullPeak >= Self.pullThreshold { triggerRefresh() }
                }
            }
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

    /// The archive undo toast: an ink capsule above the tab bar — 「已归档」+
    /// trailing 「撤销」. Red never appears here; archive is not destructive.
    private var undoToast: some View {
        HStack(spacing: 18) {
            Text(verbatim: lang.t("list.archivedToast"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.paper)
            Button(action: undoArchive) {
                Text(verbatim: lang.t("list.undo"))
                    .font(Theme.mono(13, weight: .bold))
                    .foregroundStyle(Theme.paper)
                    .frame(minHeight: 44)
            }
        }
        .padding(.horizontal, 22)
        .frame(height: 44)
        .background(Theme.ink, in: Capsule())
        .shadow(color: Theme.ink.opacity(0.18), radius: 16, y: 6)
    }

    private func archive(_ row: SessionRow) {
        Haptics.tap()
        withAnimation(.spring(response: 0.32, dampingFraction: 0.85)) {
            appStore.requestArchive(row.id, archived: true)
            undoSessionId = row.id
        }
        armUndoDismiss()
    }

    /// Toggle pin. The row glides to (or down from) the pinned block: the list's
    /// `.animation(_, value: appStore.pinReorderTick)` animates this reorder only
    /// — `requestPin` bumps that tick — while refresh/activity reshuffles stay
    /// instant. The tint/badge cross-fade on their own timeline in `SessionRowView`.
    private func togglePin(_ row: SessionRow) {
        Haptics.tap()
        appStore.requestPin(row.id, pinned: !row.pinned)
    }

    /// (Re-)start the toast's auto-dismiss — consecutive archives keep one
    /// toast alive and restart its clock.
    private func armUndoDismiss() {
        undoDismissTask?.cancel()
        undoDismissTask = Task { @MainActor in
            try? await Task.sleep(for: Self.undoWindow)
            guard !Task.isCancelled else { return }
            withAnimation(.easeIn(duration: 0.18)) { undoSessionId = nil }
        }
    }

    private func undoArchive() {
        guard let sessionId = undoSessionId else { return }
        Haptics.tap()
        undoDismissTask?.cancel()
        withAnimation(.easeIn(duration: 0.18)) {
            undoSessionId = nil
            appStore.requestArchive(sessionId, archived: false)
        }
    }

    private func compose() {
        guard !appStore.busy else { return }
        Haptics.tap()
        notice = nil
        Task {
            notice = await appStore.startNewChat()
        }
    }

    /// Fire a refresh from a completed pull. Spin for at least one full turn even
    /// when the round-trip is near-instant, so the gesture always reads as
    /// feedback rather than a flicker.
    private func triggerRefresh() {
        guard !isRefreshing else { return }
        Haptics.tap()
        isRefreshing = true
        Task {
            async let minimum: Void = Task.sleep(for: .milliseconds(800))
            await refresh()
            try? await minimum
            isRefreshing = false
        }
    }

    /// Merge the gateway's list over the local registry. Failures stay quiet:
    /// the local rows keep rendering, which is the whole point of local-first.
    private func refresh() async {
        let fetchEpoch = SessionIndex.shared.mutationEpoch
        do {
            let items = try await Baybo.client.chatListSessions()
            SessionIndex.shared.merge(remote: items, fetchEpoch: fetchEpoch)
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
        HStack(alignment: .center, spacing: 12) {
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
                            .transition(.scale.combined(with: .opacity))
                    }
                    Text(
                        verbatim: Self.age(
                            of: row.lastActive, locale: langCode, justNow: justNow)
                    )
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
                }
                .animation(ChatListScreen.pinTintFade, value: row.pinned)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if row.unread > 0 {
                unreadBadge
            }
        }
        .padding(.vertical, 14)
        // The pinned ground, drawn in-content so it cross-fades instead of a
        // hard cell-background swap; negative gutter bleeds it edge-to-edge.
        .background {
            ChatListScreen.pinnedRowTint
                .opacity(row.pinned ? 1 : 0)
                .padding(.horizontal, -ChatListScreen.rowHInset)
                .allowsHitTesting(false)
                .animation(ChatListScreen.pinTintFade, value: row.pinned)
        }
        .contentShape(Rectangle())
    }

    /// The unread count for a backgrounded session: an ink capsule with paper
    /// digits, matching the ink CTA pill (`99+` past the cap). Cleared on open,
    /// so it never shows on the row the user is currently viewing.
    private var unreadBadge: some View {
        Text(verbatim: row.unread > 99 ? "99+" : String(row.unread))
            .font(Theme.mono(11, weight: .medium))
            .foregroundStyle(Theme.paper)
            .padding(.horizontal, 6)
            .frame(minWidth: 18, minHeight: 18)
            .background(Theme.ink, in: Capsule())
            .accessibilityLabel(Text(verbatim: "\(row.unread)"))
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
