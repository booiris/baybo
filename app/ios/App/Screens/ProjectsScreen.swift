import SwiftUI

struct ProjectsScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var projects: ProjectsStore

    @State private var showsArchived = false

    init(store: ProjectsStore) {
        _projects = ObservedObject(wrappedValue: store)
        #if DEBUG
            _showsArchived = State(initialValue: AppStoreScreenshotData.requested)
        #endif
    }

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
        VStack {
            Spacer()
            CreationPrompt(
                symbol: AppStore.HomeTab.projects.icon,
                title: lang.t("home.tab.projects"),
                message: lang.t("projects.empty"),
                actionTitle: lang.t("projects.new"),
                actionIdentifier: "project-new"
            ) {
                Haptics.tap()
                appStore.openNewProject()
            }
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

struct TeamFaces: View {
    let team: [TeamMemberInfo]
    let runs: [IssueRunInfo]

    private var working: Set<String> {
        Set(runs.filter { $0.status == .running && $0.settledAtMs == nil }.map(\.agentId))
    }

    static let maxFaces = 6

    static func facesDrawn(of count: Int) -> Int {
        count <= maxFaces + 1 ? count : maxFaces
    }

    private var shown: [TeamMemberInfo] {
        Array(team.prefix(Self.facesDrawn(of: team.count)))
    }

    var body: some View {
        let monograms = AgentMonogram.map(for: team)
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

struct AgentFace: View {
    let handle: String
    var monogram: String? = nil
    var avatarBlobId: String? = nil
    var lead: Bool = false
    var working: Bool = false
    var size: CGFloat = AgentFace.defaultSize

    static let defaultSize: CGFloat = 22

    @ObservedObject private var avatars = AgentAvatars.shared

    private var initials: String { monogram ?? AgentMonogram.of(handle) }

    var body: some View {
        picture
            .overlay(
                Circle().strokeBorder(lead ? Theme.ink : Theme.lineStrong, lineWidth: lead ? 1.5 : 1)
            )
            .overlay(
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
                .aspectRatio(contentMode: .fill)
                .frame(width: size, height: size)
                .clipShape(Circle())
        } else {
            Text(verbatim: initials)
                .font(Theme.mono(size * (initials.count > 2 ? 0.30 : 0.36)))
                .foregroundStyle(Theme.ink)
                .frame(width: size, height: size)
                .background(Theme.paper, in: Circle())
        }
    }
}
