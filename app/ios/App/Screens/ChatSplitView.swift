import SwiftUI

/// When the Chats tab is a split view, and how its sidebar is drawn.
///
/// The web chat's shape (`app/web`'s session sidebar beside the thread) for a
/// window that is wide AND tall — iPhone Duo's inner display. Both size classes,
/// because width alone would take in a large phone in landscape (regular width,
/// compact height), and phones keep the push.
enum ChatSplit {
    /// The sidebar column — the web sidebar's 260px. Its lines are one title
    /// each, so it needs no more.
    static let sidebarWidth: CGFloat = 260
    /// A line's tappable height. Taller than the web's mouse-sized 32px row: this
    /// is a finger.
    static let lineHeight: CGFloat = 40
    /// The gutter between the sidebar's edges and its lines' rounded grounds
    /// (the web nav's `px-2`), and the content padding inside a ground.
    static let lineGutter: CGFloat = 8
    static let linePadding: CGFloat = 10
    /// How far a fire is indented under its open folder (the web's per-depth 14px).
    static let nestedIndent: CGFloat = 14
    static let rowRadius: CGFloat = 10
    static let folderToggle: Double = 0.18
    /// Ground for the docked conversation's line — the one wash
    /// `design-system.md` reserves for a selected cell. The web paints its active
    /// row coral; this palette has no hue for chrome.
    static let selectedRowTint = Color(red: 0.9, green: 0.9, blue: 0.9)

    static func engages(
        horizontal: UserInterfaceSizeClass?, vertical: UserInterfaceSizeClass?
    ) -> Bool {
        horizontal == .regular && vertical == .regular
    }
}

/// The Chats tab's content: the chat list, and — in the split view — the
/// docked conversation beside it (`AppStore.dockedSession`).
///
/// ONE tree for both shapes, so folding and unfolding resize the list rather
/// than rebuild it: its scroll position and unread walk survive the change.
struct ChatsTabView: View {
    let reselectEpoch: Int
    @EnvironmentObject private var store: AppStore
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        HStack(spacing: 0) {
            // Each pane resolves its own side rail: the conversation's trailing
            // edge is the window's, where the system column runs, and the list's
            // is not.
            ChatListScreen(
                reselectEpoch: reselectEpoch, sidebar: store.splitView,
                selected: store.dockedSession
            )
            .frame(width: store.splitView ? ChatSplit.sidebarWidth : nil)
            .resolvesSideRail()
            // The divider is drawn OVER the sidebar's edge, never laid out
            // beside it: a hairline taking its 1/3pt from the conversation left
            // the transcript webview a fractional width, which WebKit rounds its
            // page UP from — a page wider than its view, so it panned sideways.
            .overlay(alignment: .trailing) {
                if store.splitView {
                    Theme.line
                        .frame(width: 1 / displayScale)
                        .ignoresSafeArea()
                }
            }

            if store.splitView {
                detail
                    .frame(maxWidth: .infinity)
                    .resolvesSideRail()
            }
        }
    }

    @ViewBuilder private var detail: some View {
        if let sessionId = store.dockedSession {
            SessionChatScreen(sessionId: sessionId, docked: true)
        } else {
            Text(verbatim: Lang.shared.t("split.empty"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 24)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.paper)
        }
    }
}

extension View {
    /// A sidebar line's cell: no separator (the web sidebar has none — each line
    /// is its own ground), the gutter on both sides, a hairline of air between
    /// grounds.
    func sidebarLine() -> some View {
        listRowBackground(Theme.paper)
            .listRowSeparator(.hidden)
            .listRowInsets(
                EdgeInsets(
                    top: 1, leading: ChatSplit.lineGutter,
                    bottom: 1, trailing: ChatSplit.lineGutter))
    }
}

/// A block's label — the web sidebar's small uppercase mono divider.
struct SidebarSectionLabel: View {
    let title: String

    var body: some View {
        Text(verbatim: title)
            .font(Theme.mono(10, weight: .bold))
            .textCase(.uppercase)
            .kerning(1)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, ChatSplit.linePadding)
            .padding(.top, 14)
            .padding(.bottom, 4)
            .frame(maxWidth: .infinity, alignment: .leading)
            .accessibilityAddTraits(.isHeader)
    }
}

/// A conversation in the sidebar: its title alone, as on the web, plus the two
/// marks that are STATE — waiting on an approval, and unread. The preview and
/// the time stay on the phone row, where the list is the whole screen.
struct SidebarChatRow: View, Equatable {
    let row: SessionRow
    let nested: Bool
    let selected: Bool

    var body: some View {
        HStack(spacing: 8) {
            Text(verbatim: SessionRowView.headline(row))
                .font(Theme.sys(14, weight: selected ? .semibold : .regular))
                .foregroundStyle(SessionRowView.hasHeadline(row) ? Theme.ink : Theme.inkSoft)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)
            if row.approvalPending { ChatRowApprovalMark() }
            if row.unread > 0 && !selected { ChatRowUnreadBadge(count: row.unread) }
        }
        .padding(.leading, ChatSplit.linePadding + (nested ? ChatSplit.nestedIndent : 0))
        .padding(.trailing, ChatSplit.linePadding)
        .frame(minHeight: ChatSplit.lineHeight)
        .background {
            RoundedRectangle(cornerRadius: ChatSplit.rowRadius, style: .continuous)
                .fill(ChatSplit.selectedRowTint)
                .opacity(selected ? 1 : 0)
                .animation(nil, value: selected)
        }
        .contentShape(Rectangle())
    }
}

/// A cron group in the sidebar: a FOLDER, as on the web — a caret, the alarm
/// glyph the phone's group row wears (`CronGroupRowView`), and the job's name in
/// the web folder header's mono. Closed, it carries what it is hiding: the
/// summed unread and the approval mark; open, its fires carry their own.
struct SidebarFolderRow: View, Equatable {
    let group: CronGroup
    let expanded: Bool

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: expanded ? "chevron.down" : "chevron.right")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Theme.inkSoft)
                .frame(width: 14)
            Image(systemName: "alarm")
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(Theme.ink)
            Text(verbatim: group.title)
                .font(Theme.mono(12, weight: .bold))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)
            if group.pinned {
                Image(systemName: "pin.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.inkSoft)
                    .accessibilityLabel(Text(Lang.shared.t("split.pinned")))
            }
            if !expanded {
                if group.approvalPending { ChatRowApprovalMark() }
                if group.unread > 0 { ChatRowUnreadBadge(count: group.unread) }
            }
        }
        .padding(.leading, ChatSplit.linePadding)
        .padding(.trailing, ChatSplit.linePadding)
        .frame(minHeight: ChatSplit.lineHeight)
        .contentShape(Rectangle())
    }
}
