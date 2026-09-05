import SwiftUI

struct ProjectIssueScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @StateObject private var visit: IssueVisit
    @Environment(\.dismiss) private var dismiss

    let projectId: String
    let number: Int64

    private var store: IssueStore { visit.store }
    @StateObject private var attach = AttachMenu()
    @State private var confirmingStop = false
    @State private var picking: PickerField?
    @State private var assigning = false
    /// Set when the assignee picker was opened BY a move, so the move can
    /// finish once somebody is on the card.
    @State private var assignThenMoveTo: IssueStatus?
    @State private var openRun: ProjectRunRoute?

    init(visit: IssueVisit) {
        projectId = visit.store.projectId
        number = visit.store.number
        _visit = StateObject(wrappedValue: visit)
    }

    var body: some View {
        ZStack(alignment: .top) {
            page
            header
            IssueDidAppearReporter { visit.didAppear() }
                .frame(width: 0, height: 0)
            // Only the attach panel's SCRIM lands here — the panel itself rides
            // inside the dock's layer, above the dock's own rows.
            if attach.isOpen {
                AttachMenuScrim(isPresented: $attach.isOpen)
            }
            if let error = store.writeError { errorBanner(error) }
        }
        .background(Theme.paper)
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .safeAreaInset(edge: .bottom, spacing: 0) { dock }
        .task { await store.refresh() }
        .onChange(of: store.pickRequest) { _, field in
            guard let field else { return }
            store.pickRequest = nil
            raise(field)
        }
        .onChange(of: store.openRunRequest) { _, attempt in
            guard let attempt else { return }
            store.openRunRequest = nil
            guard let run = store.runs.first(where: { $0.attempt == attempt }) else { return }
            show(run)
        }
        .onAppear {
            ComposerPasteTarget.shared.attach(store.staging)
        }
        .onDisappear {
            ComposerPasteTarget.shared.detach(store.staging)
        }
        .sheet(item: $picking) { field in
            picker(field)
        }
        .sheet(isPresented: $assigning) {
            assignThenMoveTo = nil
        } content: {
            AssigneePicker(
                team: store.team, current: store.issue?.assignee,
                onPick: { agentId in assign(to: agentId) })
        }
        .sheet(item: $openRun) { route in
            ProjectRunSheet(route: route) { confirmingStop = true }
                .presentationDetents([.large])
                .presentationDragIndicator(.hidden)
                .presentationBackground(Theme.paper)
                .presentationCornerRadius(Theme.radiusModal)
        }
        .alert(lang.t("issue.stopTitle"), isPresented: $confirmingStop) {
            Button(lang.t("common.cancel"), role: .cancel) {}
            Button(lang.t("issue.stopConfirm"), role: .destructive) { store.stopRun() }
        } message: {
            Text(verbatim: lang.t("issue.stopExplain"))
        }
    }

    private var page: some View {
        IssueWebView(lease: visit.lease)
            .ignoresSafeArea(.all, edges: .bottom)
    }

    private var dock: some View {
        ComposerDock(collapsed: false) {
            IssueDock(store: store, staging: store.staging, attach: attach)
                .onGeometryChange(for: CGFloat.self) { proxy in
                    proxy.frame(in: .global).minY
                } action: { _, minY in
                    store.setComposerTop(minY)
                }
                .jumpToLatestDisc(
                    visible: !store.atBottom, label: lang.t("issue.jumpToLatest"),
                    identifier: "issue-jump"
                ) {
                    store.jumpToLatest()
                }
        } panel: {
            if attach.isOpen {
                AttachMenuPanel(
                    anchor: attach.anchor, sources: attach.sources,
                    isPresented: $attach.isOpen
                ) { source in
                    attach.pick = source
                }
            }
        }
    }

    // MARK: - Header

    private var header: some View {
        ZStack {
            if !store.atTop {
                Button {
                    Haptics.tap()
                    store.scrollToTop()
                } label: {
                    HStack(alignment: .center, spacing: 7) {
                        Text(verbatim: "#\(number)")
                            .font(Theme.mono(16))
                        Image(systemName: "arrow.up")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .foregroundStyle(Theme.ink)
                    .padding(.horizontal, 8)
                    .frame(height: 42)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("issue-scroll-top")
                .accessibilityLabel(Text(verbatim: lang.t("issue.jumpToTop", "#\(number)")))
            } else {
                Text(verbatim: "#\(number)")
                    .font(Theme.mono(16))
                    .foregroundStyle(Theme.ink)
            }

            HStack(spacing: 6) {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("board.back")))
                Spacer()
                if store.liveRun != nil {
                    Button {
                        Haptics.tap()
                        confirmingStop = true
                    } label: {
                        Image(systemName: "stop.fill")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassSurface(interactive: true, in: .circle)
                    .accessibilityIdentifier("issue-stop")
                    .accessibilityLabel(Text(verbatim: lang.t("issue.stop")))
                }
                if hasMenu { menu }
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) {
            LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .top)
                .allowsHitTesting(false)
        }
    }

    private var hasMenu: Bool {
        canMove || !store.runs.isEmpty || store.issue?.lastRunFailed == true
    }

    private var canMove: Bool {
        let archived = appStore.projectsStore.projects.first { $0.id == projectId }?.archivedAtMs
        return store.issue != nil && archived == nil && !appStore.projectsStore.isOffline
    }

    private var menu: some View {
        Menu {
            if canMove {
                Button {
                    Haptics.tap()
                    raise(CardField.status.rawValue)
                } label: {
                    Label(lang.t("issue.moveStatus"), systemImage: "arrow.left.arrow.right")
                }
            }
            if store.issue?.lastRunFailed == true {
                Button {
                    Haptics.tap()
                    store.retryRun()
                } label: {
                    Label(lang.t("board.runAgain"), systemImage: "arrow.clockwise")
                }
            }
            runsMenu
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.ink)
                .frame(width: 42, height: 42)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityIdentifier("issue-menu")
        .accessibilityLabel(Text(verbatim: lang.t("list.menu")))
    }

    @ViewBuilder private var runsMenu: some View {
        if !store.runs.isEmpty {
            Menu {
                ForEach(store.runs, id: \.self) { run in
                    Button {
                        Haptics.tap()
                        show(run)
                    } label: {
                        Text(verbatim: runLabel(run))
                        if let error = run.error, !error.isEmpty {
                            Text(verbatim: error)
                        }
                        Image(systemName: runIcon(run.status))
                    }
                    .disabled(run.sessionId == nil)
                    .accessibilityLabel(Text(verbatim: runReading(run)))
                }
            } label: {
                Label(lang.t("issue.runs"), systemImage: "clock.arrow.circlepath")
            }
            .accessibilityIdentifier("issue-runs")
        }
    }

    private func runLabel(_ run: IssueRunInfo) -> String {
        let status = lang.t("run.status.\(IssueWire.word(run.status))")
        return "#\(run.attempt) · \(status) · @\(store.handle(forAgent: run.agentId))"
    }

    /// The same row, spoken: the title plus the failure sentence the subtitle
    /// shows.
    private func runReading(_ run: IssueRunInfo) -> String {
        guard let error = run.error, !error.isEmpty else { return runLabel(run) }
        return "\(runLabel(run)) — \(error)"
    }

    private func runIcon(_ status: RunStatus) -> String {
        switch status {
        case .running: "play.circle"
        case .queued, .held: "clock"
        case .done: "checkmark.circle"
        case .failed: "exclamationmark.circle"
        case .cancelled, .unknown: "slash.circle"
        }
    }

    private func show(_ run: IssueRunInfo) {
        guard let sessionId = run.sessionId else { return }
        openRun = ProjectRunRoute(
            projectId: projectId, number: number, attempt: run.attempt,
            sessionId: sessionId, status: run.status)
    }

    // MARK: - The chips
    // Writes use ProjectsStore because moves serialize the destination order.

    private enum CardField: String {
        case status, priority, assignee, stage
    }

    /// Which picker is on screen, when it is one of the two that share a
    /// presentation.
    private enum PickerField: String, Identifiable {
        case status, priority
        var id: String { rawValue }
    }

    private func raise(_ field: String) {
        guard store.issue != nil, let field = CardField(rawValue: field) else { return }
        switch field {
        case .status: picking = .status
        case .priority: picking = .priority
        case .assignee: assigning = true
        // Parent + stage is the one picker this screen never grew; the page
        // does not offer it either, so nothing posts it today.
        case .stage: break
        }
    }

    @ViewBuilder private func picker(_ field: PickerField) -> some View {
        if let issue = store.issue {
            switch field {
            case .status:
                MoveSheet(
                    issue: issue,
                    liveRun: store.liveRun,
                    assigneeHandle: issue.assignee.map { store.handle(forAgent: $0) },
                    overCeiling: meter?.burn == .over,
                    heldCeiling: meter?.ceiling ?? .unknown,
                    onPick: { row in move(issue, row: row) })
            case .priority:
                PriorityPicker(current: issue.priority) { level in
                    Task {
                        await commit {
                            await $0.setPriority(board: projectId, issue: number, to: level)
                        }
                    }
                }
            }
        }
    }

    private var meter: BudgetMeter.Meter? { appStore.projectsStore.budgetMeter(board: projectId) }

    private func move(_ issue: IssueInfo, row: MoveConsequence.Row) {
        // In Progress with nobody on it starts nothing, so the picker comes
        // first and the move rides on its answer — the board's own chain.
        guard !row.needsAssignee else {
            assignThenMoveTo = row.status
            assigning = true
            return
        }
        Task { await commit { await $0.move(board: projectId, issue: number, to: row.status) } }
    }

    private func assign(to agentId: String?) {
        let target = assignThenMoveTo
        assignThenMoveTo = nil
        Task {
            guard await commit({ await $0.assign(board: projectId, issue: number, to: agentId) }),
                let target, agentId != nil
            else { return }
            await commit { await $0.move(board: projectId, issue: number, to: target) }
        }
    }

    @discardableResult
    private func commit(_ write: (ProjectsStore) async -> Bool) async -> Bool {
        let projects = appStore.projectsStore
        // Push deep links can open a card without its board, but move writes
        // require the destination column's full order.
        if projects.boards[projectId] == nil { await projects.refreshBoard(projectId) }
        guard await write(projects) else {
            store.showWriteError(projects.writeError)
            return false
        }
        await store.refresh()
        return true
    }

    private func errorBanner(_ message: String) -> some View {
        VStack {
            Spacer()
            HStack(spacing: 12) {
                Text(verbatim: message)
                    .font(Theme.sys(12.5))
                    .foregroundStyle(Theme.paper)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
                Button { store.clearWriteError() } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.paper)
                        .frame(width: 40, height: 40)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            .padding(.leading, 18)
            .padding(.trailing, 2)
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.err))
            .padding(.horizontal, 16)
            .padding(.bottom, 100)
            .accessibilityIdentifier("issue-write-error")
        }
    }
}
