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
    /// pin flip never swaps the cell's background configuration — that swap is
    /// what blanked the incoming row and stalled the reorder.
    fileprivate static let pinnedRowTint = Color(red: 0.945, green: 0.945, blue: 0.945)
    /// Row content gutter. The pinned tint bleeds back out past it (negative
    /// padding in `SessionRowView`) so it fills edge-to-edge like the old ground.
    fileprivate static let rowHInset: CGFloat = 24
    /// How long the pin mutation waits after the swipe action is tapped, so the
    /// re-sort lands AFTER UIKit has torn down the `.swipeActions` panel.
    ///
    /// The row's arrival is not ours to schedule: `List` won't move the cell
    /// until UIKit releases it ~0.65s after the tap, and nothing on the SwiftUI
    /// side shortens that — spring vs linear vs no `.animation` at all vs an
    /// explicit `withAnimation` vs a synchronous mutation were all measured, and
    /// the last two are *worse* (0.9-1.1s). What we do schedule is when the
    /// destination slot opens, and it opens the moment `pinned` flips. Mutate
    /// early and the slot sits blank waiting for the row; mutate late and it
    /// does not. So this is a blank-gap knob, not a speed knob.
    ///
    /// Medians of 3 frame-by-frame runs (iPhone 17 Pro, iOS 26.5):
    ///
    ///     dismissal   blank gap   tap -> row arrives
    ///         0ms       0.637s        0.700s
    ///       320ms       0.330s        0.665s
    ///       480ms       0.137s        0.640s   <- knee: gap all but gone, free
    ///       640ms       0.098s        0.758s   <- past it, arrival slips
    ///
    /// Anything under ~300ms lands mid-teardown and is within noise of 0.
    private static let swipeDismissal: Duration = .milliseconds(660)

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
        // Popping back from a conversation to the list root: `.task` doesn't
        // re-run (the list stayed mounted under the pushed ChatScreen), so
        // refresh here to reconcile the row a just-visited chat may have
        // retitled / re-previewed while it was open.
        .onChange(of: appStore.chatPath) { old, new in
            if new.isEmpty && !old.isEmpty {
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
                        langCode: lang.current.lproj
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
        // No reorder animation: every reshuffle — pin, pull-refresh merge,
        // live-activity recency bump — snaps. A pin tapped from the swipe never
        // glided anyway (UIKit holds the cell; see `swipeDismissal`), so the
        // animation only ever dressed up the programmatic `-baybo-demo-pin` path.
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

    /// Toggle pin, once the swipe panel has closed (`swipeDismissal`). The row
    /// jumps to its new slot rather than gliding: `List` will not move the cell
    /// while the swipe container tears down, so the wait buys nothing but a
    /// destination slot that stops sitting blank. Haptic fires on the tap, not
    /// after the wait, so the press still feels immediate.
    private func togglePin(_ row: SessionRow) {
        Haptics.tap()
        Task { @MainActor in
            try? await Task.sleep(for: Self.swipeDismissal)
            appStore.requestPin(row.id, pinned: !row.pinned)
        }
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

/// One list row, Telegram-shaped: a bold headline (the conversation title, or a
/// short snippet of the user's last message until the title pass runs) over a
/// grey last-message preview on the left; the last-active time over an unread
/// badge on the right. Pinned rows are marked by a tinted background, not a glyph.
struct SessionRowView: View {
    let row: SessionRow
    /// The app language's locale identifier (drives the time formatter, so it
    /// can't diverge from the chrome language).
    let langCode: String

    /// The window (in days from today) over which the time column shows a
    /// weekday name rather than a clock time (today) or a calendar date (older).
    private static let weekdayWindow: Range<Int> = 1..<7

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            textColumn
                .frame(maxWidth: .infinity, alignment: .leading)
            metaColumn
        }
        // Taller blocks with room for the preview to wrap to two lines. minHeight
        // keeps short rows (a one-line preview, or a headline-only session with no
        // message yet) from collapsing shorter than the two-line rows, so the list
        // reads as one even block height.
        .frame(minHeight: 52)
        .padding(.vertical, 15)
        // The pinned ground, drawn in-content rather than as the cell's
        // background configuration; negative gutter bleeds it edge-to-edge.
        .background {
            ChatListScreen.pinnedRowTint
                .opacity(row.pinned ? 1 : 0)
                .padding(.horizontal, -ChatListScreen.rowHInset)
                .allowsHitTesting(false)
                // Snap with the row. There is no positional glide to ride
                // alongside, and a cross-fade here would land in the visible
                // window (the row now arrives the moment `pinned` flips). `nil`
                // also shields the tint from the archive/undo transactions.
                .animation(nil, value: row.pinned)
        }
        .contentShape(Rectangle())
    }

    /// Longest user-message snippet the bold headline shows before the title
    /// pass has run — a compact title stand-in, not the full message.
    private static let headlineMaxChars = 8

    /// Left column: a bold headline over the grey last-message preview. The
    /// headline is the conversation title, or — before the title pass runs — a
    /// short snippet of the user's most recent message; the preview line below
    /// is the latest message from either side.
    private var textColumn: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(verbatim: headline)
                .font(Theme.mono(15, weight: .bold))
                .foregroundStyle(hasHeadline ? Theme.ink : Theme.inkSoft)
                .lineLimit(1)
                .truncationMode(.tail)
            if let preview = row.preview, !preview.isEmpty {
                Text(verbatim: preview)
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
                    .truncationMode(.tail)
            }
        }
    }

    /// Bold first line: the title, else a short snippet of the user's last
    /// message, else a quiet placeholder for a session with no user turn yet.
    private var headline: String {
        if let title = row.title, !title.isEmpty { return title }
        if let userText = row.userText, !userText.isEmpty { return Self.snippet(userText) }
        return Lang.shared.t("list.previewPlaceholder")
    }

    private var hasHeadline: Bool {
        !(row.title ?? "").isEmpty || !(row.userText ?? "").isEmpty
    }

    /// The user-message fallback headline: whitespace collapsed and clipped to
    /// `headlineMaxChars` with a trailing ellipsis when it overflows.
    private static func snippet(_ text: String) -> String {
        let collapsed = text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return collapsed.count > headlineMaxChars
            ? String(collapsed.prefix(headlineMaxChars)) + "…"
            : collapsed
    }

    /// Right column: time pinned to the top (beside the headline), the unread
    /// badge pinned to the bottom (beside the preview).
    private var metaColumn: some View {
        VStack(alignment: .trailing, spacing: 6) {
            Text(verbatim: Self.timeLabel(row.lastActive, locale: langCode))
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            Spacer(minLength: 0)
            trailingMarker
        }
        .fixedSize(horizontal: true, vertical: false)
    }

    /// Bottom-right marker: the unread badge when there are unread replies, else
    /// nothing. A pinned row is signalled by its tinted background alone — no
    /// pin glyph.
    @ViewBuilder private var trailingMarker: some View {
        if row.unread > 0 {
            unreadBadge
        }
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

    /// Telegram-style time column: a clock time for today, the weekday name
    /// within the last week, then a calendar date (with year once it rolls
    /// over). Locale drives digit shape and weekday/date ordering.
    private static func timeLabel(
        _ date: Date, locale: String, relativeTo now: Date = Date()
    ) -> String {
        // A server-stamped `lastActive` can land slightly ahead of the device
        // clock (skew, or a just-sent row); clamp to now so a "tomorrow" value
        // reads as the current time, not a future calendar date.
        let date = min(date, now)
        let calendar = Calendar.current
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: locale)
        if calendar.isDateInToday(date) {
            formatter.setLocalizedDateFormatFromTemplate("jm")
            return formatter.string(from: date)
        }
        let days = calendar.dateComponents(
            [.day], from: calendar.startOfDay(for: date), to: calendar.startOfDay(for: now)
        ).day
        if let days, Self.weekdayWindow.contains(days) {
            formatter.setLocalizedDateFormatFromTemplate("EEE")
            return formatter.string(from: date)
        }
        let sameYear = calendar.isDate(date, equalTo: now, toGranularity: .year)
        formatter.setLocalizedDateFormatFromTemplate(sameYear ? "Md" : "yMd")
        return formatter.string(from: date)
    }
}
