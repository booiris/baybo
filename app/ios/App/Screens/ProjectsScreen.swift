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

    private var live: [ProjectInfo] { projects.projects.filter { $0.archivedAtMs == nil } }
    private var archived: [ProjectInfo] { projects.projects.filter { $0.archivedAtMs != nil } }
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
                newProjectCard
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

    private var newProjectCard: some View {
        Button {
            Haptics.tap()
            appStore.openNewProject()
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "plus")
                    .font(.system(size: 14, weight: .medium))
                Text(verbatim: lang.t("projects.new"))
                    .font(Theme.mono(13))
            }
            .foregroundStyle(Theme.ink)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 18)
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .strokeBorder(
                        Theme.inkSoft.opacity(0.3),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 4]))
            )
            .contentShape(RoundedRectangle(cornerRadius: Theme.radius, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("project-new")
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

    /// Approvals, failures and unread together — every one of them an event a
    /// press can discharge. Runs the ceiling is holding are deliberately not
    /// here: a hold is a standing condition, and painting it in the same red
    /// makes a mark that cannot be cleared by acting.
    private var waiting: Int {
        guard let attention else { return 0 }
        return Int(attention.approvals + attention.failed + attention.unread)
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
                .strokeBorder(Theme.line, lineWidth: 1)
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
                    .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
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
                .foregroundStyle(Theme.line)
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
                                .strokeBorder(Theme.line, lineWidth: 1)
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
private struct TeamFaces: View {
    let team: [TeamMemberInfo]
    let runs: [IssueRunInfo]

    private var working: Set<String> {
        Set(runs.filter { $0.status == .running && $0.settledAtMs == nil }.map(\.agentId))
    }

    /// Monograms, made unique ACROSS the faces actually drawn.
    ///
    /// The obvious rule — first letter of each dash-segment — collides on real
    /// handles (`dev-1` and `docs-1` both give `D1`), and two identical faces
    /// standing for different agents is worse than a longer one: the whole
    /// point of the row is "who".
    ///
    /// When one pair collides the WHOLE row widens, not just the pair. A row
    /// reading `DE1 D2 DO1` makes the odd one out look like a different kind of
    /// thing, when all it means is that its neighbours happened to clash.
    private var monograms: [String: String] {
        let shown = Array(team.prefix(5))
        var out: [String: String] = [:]
        // Three glyphs is the ceiling a 22pt circle can carry; past that,
        // duplicates are simply what the row shows.
        for width in 1...3 {
            out = Dictionary(
                uniqueKeysWithValues: shown.map {
                    ($0.id, Self.monogram($0.handle, leading: width))
                })
            if Set(out.values).count == shown.count { break }
        }
        return out
    }

    private static func monogram(_ handle: String, leading: Int) -> String {
        let parts = handle.split(separator: "-")
        guard let first = parts.first else { return handle.prefix(2).uppercased() }
        let head = first.prefix(leading).uppercased()
        guard parts.count >= 2, let tail = parts[1].first else {
            return String(first.prefix(max(2, leading))).uppercased()
        }
        return head + String(tail).uppercased()
    }

    var body: some View {
        let monograms = self.monograms
        // Set apart rather than stacked. The overlapping-avatars idiom saves
        // room this card does not need, and it costs the thing the row is FOR:
        // a working member's ring lands on its neighbour's edge, and four
        // agents read as a tangle of arcs instead of as who is busy.
        return HStack(spacing: 4) {
            ForEach(team.prefix(5), id: \.id) { member in
                AgentFace(
                    handle: member.handle,
                    monogram: monograms[member.id],
                    lead: member.lead,
                    working: working.contains(member.id))
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
    /// Precomputed by `TeamFaces` so the monogram can be made unique across the
    /// row; a face drawn on its own falls back to the plain rule.
    var monogram: String? = nil
    var lead: Bool = false
    var working: Bool = false
    var size: CGFloat = AgentFace.defaultSize

    static let defaultSize: CGFloat = 22

    private var initials: String {
        if let monogram { return monogram }
        let parts = handle.split(separator: "-")
        if parts.count >= 2, let a = parts[0].first, let b = parts[1].first {
            return "\(a)\(b)".uppercased()
        }
        return String(handle.prefix(2)).uppercased()
    }

    var body: some View {
        Text(verbatim: initials)
            // Three glyphs only happen on a collision, and they have to fit the
            // same circle — the row's rhythm is the point, not the type size.
            .font(Theme.mono(size * (initials.count > 2 ? 0.30 : 0.36)))
            .foregroundStyle(Theme.ink)
            .frame(width: size, height: size)
            .background(Theme.paper, in: Circle())
            .overlay(
                Circle().strokeBorder(lead ? Theme.ink : Theme.line, lineWidth: lead ? 1.5 : 1)
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
    }
}
