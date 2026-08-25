import SwiftUI

/// The Projects tab root: one card per board.
///
/// This is the tab's home, and there is deliberately **no project switcher
/// anywhere else** — a board is a pushed screen, and changing board means
/// backing out to here. That diverges from the web, whose rail entry restores
/// the last board directly and switches from a pill on its header; on a phone
/// a pushed board covers the tab bar, so a switcher in its header would be a
/// second way to do what the back gesture already does.
struct ProjectsScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var projects: ProjectsStore

    @State private var showsArchived = false

    init(store: ProjectsStore) {
        _projects = ObservedObject(wrappedValue: store)
    }

    /// **Most recently opened on this phone first**, then everything never
    /// opened here. The order is the store's answer, not this view's — the same
    /// list is sorted for the live and the archived block, and two call sites
    /// deciding it separately is how they drift.
    private var live: [ProjectInfo] {
        projects.inRecencyOrder(projects.projects.filter { $0.archivedAtMs == nil })
    }
    private var archived: [ProjectInfo] {
        projects.inRecencyOrder(projects.projects.filter { $0.archivedAtMs != nil })
    }
    private var visible: [ProjectInfo] { showsArchived ? live + archived : live }

    var body: some View {
        Group {
            if projects.projects.isEmpty {
                emptyState
            } else {
                cards
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.paper)
        .task { await projects.refreshRoot() }
        // The tab's pages stay alive, so `onAppear` re-fires on every return
        // from a pushed board and is useless as a "came back" signal. The tab
        // selection is the one that actually changes.
        .onChange(of: appStore.homeTab) { _, tab in
            guard tab == .projects else { return }
            Task { await projects.refreshRoot() }
        }
        // Returning from a board: the path emptying is the edge that means
        // "this screen is frontmost again".
        .onChange(of: appStore.chatPath.isEmpty) { _, empty in
            guard empty, appStore.homeTab == .projects else { return }
            Task { await projects.refreshRoot() }
        }
    }

    private var cards: some View {
        ScrollView {
            LazyVStack(spacing: 10) {
                header
                ForEach(visible, id: \.id) { project in
                    Button {
                        Haptics.tap()
                        appStore.openProjectBoard(project.id)
                    } label: {
                        ProjectCardView(
                            project: project,
                            board: projects.boards[project.id],
                            attention: projects.attention[project.id],
                            activity: projects.activity[project.id]
                        )
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("project-card")
                }
                archivedToggle
            }
            .padding(.horizontal, ProjectsLayout.gutter)
            .padding(.bottom, 120)
        }
        .contentMargins(.top, ProjectsLayout.topInset, for: .scrollContent)
        .scrollContentBackground(.hidden)
    }

    private var header: some View {
        HStack {
            Text(verbatim: lang.t("projects.count", "\(live.count)"))
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
            if projects.isOffline {
                Text(verbatim: lang.t("projects.offline"))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
        .padding(.bottom, 2)
    }

    @ViewBuilder private var archivedToggle: some View {
        if !archived.isEmpty {
            Button {
                withAnimation(.easeOut(duration: 0.15)) { showsArchived.toggle() }
            } label: {
                Text(
                    verbatim: showsArchived
                        ? lang.t("projects.hideArchived")
                        : lang.t("projects.showArchived", "\(archived.count)")
                )
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
                .frame(maxWidth: .infinity, minHeight: 44)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 14) {
            Spacer()
            Image(systemName: AppStore.HomeTab.projects.icon)
                .font(.system(size: 46, weight: .light))
                .foregroundStyle(Theme.inkSoft)
            Text(verbatim: lang.t("home.tab.projects"))
                .font(Theme.mono(17))
                .foregroundStyle(Theme.ink)
            // Says the one thing about this feature a phone user would
            // otherwise learn the hard way: nothing here is pushed.
            Text(verbatim: lang.t("projects.empty"))
                .font(Theme.sys(14))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .padding(.horizontal, 40)
            Button(lang.t("projects.new")) {
                Haptics.tap()
                appStore.openNewProject()
            }
            .buttonStyle(InkPillButtonStyle())
            .frame(maxWidth: 260)
            .padding(.top, 6)
            .accessibilityIdentifier("project-new")
            Spacer()
        }
        .padding(.bottom, 80)
    }
}

/// One board's card: what it is, what it is doing, and whether it wants you.
struct ProjectCardView: View {
    let project: ProjectInfo
    let board: ProjectsStore.Board?
    let attention: ProjectAttention?
    let activity: ProjectActivity?

    @ObservedObject private var lang = Lang.shared

    private var isArchived: Bool { project.archivedAtMs != nil }

    /// **Parked approvals, and nothing else** — the same set the board's
    /// Waiting strip shows, so the number you press and the rows you land on
    /// are the same number.
    ///
    /// The server's `/attention` also counts failed runs and unread cards, and
    /// this deliberately does not: neither is waiting on an answer. A failed
    /// run is over and an unread card is news, and a red count that cannot be
    /// discharged by answering anything is a mark you learn to ignore.
    private var waiting: Int {
        Int(attention?.approvals ?? 0)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            titleRow
            statusLine
            StageStrip(board: board)
                .padding(.top, 10)
                .padding(.bottom, 8)
            footRow
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .background(Theme.paper, in: RoundedRectangle(cornerRadius: Theme.radius, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                .strokeBorder(Theme.lineStrong, lineWidth: 1)
        )
        .opacity(isArchived ? 0.6 : 1)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(Text(verbatim: project.name))
        .accessibilityValue(Text(verbatim: statusText))
    }

    private var titleRow: some View {
        HStack(spacing: 8) {
            Text(verbatim: project.name)
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
            if isArchived {
                Text(verbatim: lang.t("projects.archivedChip"))
                    .font(Theme.mono(10))
                    .foregroundStyle(Theme.inkSoft)
                    .padding(.horizontal, 7)
                    .frame(height: 20)
                    .overlay(Capsule().strokeBorder(Theme.lineStrong, lineWidth: 1))
            }
            Spacer(minLength: 8)
            if waiting > 0 {
                Text(verbatim: waiting > 99 ? "99+" : "\(waiting)")
                    .font(Theme.sys(11, weight: .medium))
                    .foregroundStyle(Theme.paper)
                    .padding(.horizontal, 6)
                    .frame(minWidth: 18, minHeight: 18)
                    .background(Theme.err, in: Capsule())
                    .accessibilityLabel(Text(verbatim: lang.t("projects.waiting", "\(waiting)")))
            }
            Image(systemName: "chevron.right")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Theme.lineStrong)
        }
    }

    private var statusText: String {
        let working = Int(activity?.working ?? 0)
        let work =
            working > 0
            ? lang.t("projects.working", "\(working)") : lang.t("projects.idle")
        guard let meter else { return work }
        return "\(work) · \(meter.spent) / \(meter.limit)"
    }

    private var meter: BudgetMeter.Meter? {
        BudgetMeter.meter(
            burnMicros: activity?.burnMicros ?? 0,
            burnTokens: activity?.burnTokens ?? 0,
            limitMicros: project.dailyBudgetMicros,
            limitTokens: project.dailyBudgetTokens)
    }

    private var statusLine: some View {
        HStack(spacing: 4) {
            let working = Int(activity?.working ?? 0)
            if working > 0 {
                Circle().fill(Theme.ink).frame(width: 6, height: 6)
                Text(verbatim: lang.t("projects.working", "\(working)"))
            } else {
                Text(verbatim: lang.t("projects.idle"))
            }
            if let meter {
                Text(verbatim: "·")
                // Over the ceiling the figure is underlined rather than
                // reddened: red is the failure token, and a board that has
                // spent its budget has not failed at anything.
                Text(verbatim: "\(meter.spent) / \(meter.limit)")
                    .underline(meter.burn == .over)
            }
            Spacer()
        }
        .font(Theme.sys(12.5))
        .foregroundStyle(Theme.inkSoft)
        .padding(.top, 3)
    }

    private var footRow: some View {
        HStack(spacing: 8) {
            TeamFaces(team: board?.team ?? [], runs: board?.runs ?? [])
            Spacer(minLength: 8)
            if let line = board.flatMap(Self.headline(for:)) {
                Text(verbatim: line)
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
    }

    /// The card's one line of "what is going on here".
    ///
    /// Derived from the board it already holds rather than from the activity
    /// feed: the feed would be one more fetch per board on a screen that
    /// repaints on every tab entry, and this line only has to say enough to
    /// pick a board out of three.
    static func headline(for board: ProjectsStore.Board) -> String? {
        if let running = board.runs.first(where: { $0.status == .running }) {
            let who = board.handle(forAgent: running.agentId)
            return "@\(who) · #\(running.number)"
        }
        guard
            let newest = board.issues.filter({ $0.cancelledAtMs == nil })
                .max(by: { $0.updatedAtMs < $1.updatedAtMs })
        else { return nil }
        return "#\(newest.number) \(newest.title)"
    }
}

/// The five stages, small: each segment carries its live count, and a segment
/// holding something new wears a dot.
private struct StageStrip: View {
    let board: ProjectsStore.Board?

    private static let stages: [(IssueStatus, String)] = [
        (.backlog, "B"), (.todo, "T"), (.inProgress, "IP"), (.review, "R"), (.done, "D"),
    ]

    var body: some View {
        HStack(spacing: 3) {
            ForEach(Self.stages, id: \.0) { status, short in
                let issues = board?.issues(in: status) ?? []
                let count = BoardOrder.liveCount(issues)
                ZStack(alignment: .topTrailing) {
                    Text(verbatim: "\(short) \(count)")
                        .font(Theme.mono(9.5))
                        .foregroundStyle(count > 0 ? Theme.ink : Theme.inkSoft)
                        .frame(maxWidth: .infinity, minHeight: 22)
                        .overlay(
                            RoundedRectangle(cornerRadius: 6, style: .continuous)
                                .strokeBorder(Theme.lineStrong, lineWidth: 1)
                        )
                    if BoardOrder.hasNews(inStage: issues) {
                        Circle()
                            .fill(Theme.err)
                            .frame(width: 6, height: 6)
                            .offset(x: 3, y: -3)
                    }
                }
            }
        }
    }
}

/// Overlapping faces, the lead heavier and whoever is running ringed.
/// The row of faces a board shows for its team: who is on it, and who is
/// working right now. Shared by the cards root and the board's own bar strip —
/// the two say the same thing about the same team and must not drift.
struct TeamFaces: View {
    let team: [TeamMemberInfo]
    let runs: [IssueRunInfo]

    private var working: Set<String> {
        Set(runs.filter { $0.status == .running && $0.settledAtMs == nil }.map(\.agentId))
    }

    /// How many faces the row draws before it starts COUNTING instead.
    ///
    /// Six is what fits beside the board row's budget and filter chips on the
    /// narrowest phone this app supports; the cards root's row is wider. The
    /// cap used to be five and the remainder was simply dropped — a team of
    /// six drew as a team of five, with nothing on screen admitting it.
    static let maxFaces = 6

    /// How many of a team's faces get drawn.
    ///
    /// A `+1` would cost exactly the width of the face it replaced, so the
    /// counter only ever stands for two or more.
    static func facesDrawn(of count: Int) -> Int {
        count <= maxFaces + 1 ? count : maxFaces
    }

    private var shown: [TeamMemberInfo] {
        Array(team.prefix(Self.facesDrawn(of: team.count)))
    }

    var body: some View {
        // Over the WHOLE team, never the part that fits: one colliding pair
        // widens the entire set, so a monogram computed over a prefix is how
        // this row comes to print `D1` where the assignee picker prints `DE1`
        // for the same agent — the exact drift `AgentMonogram` exists to stop.
        let monograms = AgentMonogram.map(for: team)
        // Set apart rather than stacked. The overlapping-avatars idiom saves
        // room this card does not need, and it costs the thing the row is FOR:
        // a working member's ring lands on its neighbour's edge, and four
        // agents read as a tangle of arcs instead of as who is busy.
        return HStack(spacing: 4) {
            ForEach(shown, id: \.id) { member in
                AgentFace(
                    handle: member.handle,
                    monogram: monograms[member.id],
                    avatarBlobId: member.avatarBlobId,
                    lead: member.lead,
                    working: working.contains(member.id))
            }
            if shown.count < team.count {
                Text(verbatim: "+\(team.count - shown.count)")
                    .font(Theme.mono(10))
                    .foregroundStyle(Theme.inkSoft)
                    .frame(width: AgentFace.defaultSize, height: AgentFace.defaultSize)
                    .overlay(Circle().strokeBorder(Theme.lineStrong, lineWidth: 1))
            }
        }
        .accessibilityHidden(true)
    }
}

/// One agent, as initials in a hairline circle.
///
/// Monochrome by decree — the web draws generated Bottts robots on warm tints,
/// which is its palette, not this one. Initials off the handle keep the face
/// stable across a rename that cannot happen anyway (a handle is fixed at
/// hire).
struct AgentFace: View {
    let handle: String
    /// Precomputed by whoever draws the whole team, so the monogram can be
    /// made unique across it (`AgentMonogram.map`). A face drawn on its own
    /// falls back to the plain rule.
    var monogram: String? = nil
    /// The agent's uploaded picture, if it has one. Resolved through
    /// `AgentAvatars` rather than fetched here: a face knows its own blob and
    /// nothing about the others, so a face-driven fetch is one fetch per
    /// DRAWING — and a busy board draws the same teammate a dozen times.
    var avatarBlobId: String? = nil
    var lead: Bool = false
    var working: Bool = false
    var size: CGFloat = AgentFace.defaultSize

    static let defaultSize: CGFloat = 22

    @ObservedObject private var avatars = AgentAvatars.shared

    private var initials: String { monogram ?? AgentMonogram.of(handle) }

    /// **An uploaded avatar or a monogram, and nothing in between.**
    ///
    /// `app/web` fills that gap with a Bottts robot generated from the agent
    /// id, and this deliberately does not match it: DiceBear is not portable
    /// to Swift, and a *different* generated face on each device would be
    /// worse than none — two surfaces claiming to depict the same teammate
    /// with different pictures. The monogram is honestly "there is no
    /// picture", and it is derived from the handle printed beside it.
    var body: some View {
        picture
            .overlay(
                Circle().strokeBorder(lead ? Theme.ink : Theme.lineStrong, lineWidth: lead ? 1.5 : 1)
            )
            .overlay(
                // The working mark is a dot at the corner, matching the one the
                // card's own status line leads with. It was an open ring at the
                // face's edge, which failed twice on screen: outset it bled
                // across the neighbouring faces, and inset it vanished under
                // the LEAD's heavier border — leaving the one agent most likely
                // to be working as the one face that could never show it. There
                // is no room inside a 22pt circle for both a monogram and a
                // second ring, so the mark moved off the rim entirely.
                Group {
                    if working {
                        Circle()
                            .fill(Theme.ink)
                            .frame(width: 7, height: 7)
                            .overlay(Circle().strokeBorder(Theme.paper, lineWidth: 1.5))
                            // The frame is square; nudge the dot onto the
                            // circle's rim rather than leaving it in the corner.
                            .offset(x: -1.5, y: 1.5)
                    }
                },
                alignment: .topTrailing
            )
            .onAppear { avatars.load(blobId: avatarBlobId) }
    }

    @ViewBuilder private var picture: some View {
        if let uploaded = avatars.image(for: avatarBlobId) {
            uploaded
                .resizable()
                // `.fill`, not `.fit`: an avatar that is not square would
                // otherwise letterbox inside the circle and read as a broken
                // image rather than a cropped one.
                .aspectRatio(contentMode: .fill)
                .frame(width: size, height: size)
                .clipShape(Circle())
        } else {
            Text(verbatim: initials)
                // Three glyphs only happen on a collision, and they have to fit
                // the same circle — the row's rhythm is the point, not the type
                // size.
                .font(Theme.mono(size * (initials.count > 2 ? 0.30 : 0.36)))
                .foregroundStyle(Theme.ink)
                .frame(width: size, height: size)
                .background(Theme.paper, in: Circle())
        }
    }
}
