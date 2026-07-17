import SwiftUI

/// One scheduled job's fires, pushed on the outer NavigationStack from its row
/// in the chat list — the same shape as `ArchivedScreen` (covers the tab bar,
/// pops with the edge swipe). Rows render with the main list's `SessionRowView`
/// and keep its swipe actions, so a fire archives / deletes / pins exactly as
/// any conversation does.
///
/// A **group is a view, not an object** (`docs/cron-groups.md`): everything here
/// is derived from `SessionRow.cronJobId`, and there is no folder row behind it —
/// so there is nothing to rename, and this screen offers no group-level actions.
/// Deleting a group is really deleting its execution records, one hide per fire;
/// that lives on the group's row in the chat list (`ChatListScreen.cronGroupRow`),
/// where the count it clears can be named before the user commits.
///
/// A pushed screen rather than an inline expansion, deliberately: the disclosure
/// chevron of `Section(isExpanded:)` only renders under `.listStyle(.sidebar)`,
/// and leaving `.plain` would fight the in-content pinned tint and the hand-rolled
/// pull-to-refresh; `DisclosureGroup` nests rows inside one cell, which kills
/// per-row `.swipeActions` outright.
struct CronGroupScreen: View {
    let jobId: String

    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var index = SessionIndex.shared
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    /// The group as the chat list computed it — same rule, one source. `nil` once
    /// its last visible fire leaves (all pinned, archived, or deleted), which is
    /// also when the row disappears from the list behind us.
    private var group: CronGroup? {
        ChatListBuckets.items(from: index.rows).lazy
            .compactMap { item -> CronGroup? in
                guard case .cronGroup(let group) = item, group.jobId == jobId else { return nil }
                return group
            }
            .first
    }

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if let group, !group.members.isEmpty {
                    fireList(group)
                } else {
                    emptyState
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

    /// Back chevron + the job's name, over the shared paper veil — `ArchivedScreen`'s
    /// header, with the group's own title.
    ///
    /// Three sources, narrowest first. The group's own title is the live one. A
    /// group with no members has none to read — it has never fired, and is
    /// reachable only from the jobs list, which just learned the name
    /// (`AppStore.cronJobTitles`); without that step this page would head a
    /// specific job with the generic fallback. The fallback survives for the
    /// group vanishing under us mid-pop.
    private var header: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(
                    verbatim: group?.title ?? appStore.cronJobTitles[jobId]
                        ?? lang.t("cronGroup.title"))
                    .font(Theme.mono(16))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .padding(.horizontal, 66)

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

    private func fireList(_ group: CronGroup) -> some View {
        List {
            ForEach(group.members) { row in
                Button {
                    appStore.openCronGroupSession(row.id)
                } label: {
                    SessionRowView(row: row, langCode: lang.current.lproj)
                }
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(EdgeInsets(top: 0, leading: 24, bottom: 0, trailing: 24))
                // A fire is an ordinary conversation — the main list's actions
                // carry over verbatim. Archiving or pinning one lifts it OUT of
                // the group (it renders in the archived screen / the pinned block
                // instead), which is what keeps every row in exactly one place.
                .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                    Button {
                        Haptics.tap()
                        appStore.requestArchive(row.id, archived: true)
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
                .swipeActions(edge: .leading, allowsFullSwipe: false) {
                    Button {
                        Haptics.tap()
                        appStore.requestPin(row.id, pinned: !row.pinned)
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
        .scrollContentBackground(.hidden)
        .contentMargins(.top, ChatListScreen.topContentMargin, for: .scrollContent)
        .scrollBounceBehavior(.always)
    }

    /// Reachable only transiently — the row that pushed this screen exists iff the
    /// group has a visible member — but a fire pinned or archived from *inside*
    /// here can empty it while the user is still looking.
    private var emptyState: some View {
        VStack {
            Spacer()
            Text(verbatim: lang.t("cronGroup.empty"))
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }
}
