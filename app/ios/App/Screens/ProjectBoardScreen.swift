import SwiftUI

/// One board, pushed over the cards root.
///
/// **One stage at a time**, which is the web's `ColumnPage` on a phone. A wall
/// of five columns needs horizontal room this screen does not have, and the
/// alternative — `TabView(.page)` — puts three horizontal gestures in the same
/// square inch: a row's swipe actions, the page paging, and the edge-back. The
/// bar strip at the top takes the horizontal swipe instead, so the card rows
/// keep their own.
///
/// There is no project switcher in this header. Changing board means backing
/// out to the cards root, which is what the back gesture already does.
struct ProjectBoardScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var projects: ProjectsStore
    @Environment(\.dismiss) private var dismiss

    let projectId: String

    @State private var stage: IssueStatus = .inProgress
    @State private var moving: IssueInfo?
    /// A move that needs somebody on the card first: the picker opens, and the
    /// move runs once it answers.
    @State private var assigning: IssueInfo?
    @State private var assignThenMoveTo: IssueStatus?
    @State private var showsFilter = false
    @State private var filter = BoardFilter()
    /// What the last move said, and — only when it can be taken back — how.
    @State private var toast: Toast?
    @State private var toastDismissTask: Task<Void, Never>?
    @State private var isRefreshing = false
    @State private var pullPeak: CGFloat = 0
    @State private var dragging = false
    /// Ticks so a running card's elapsed advances between refetches.
    @State private var now = Date()

    private static let toastWindow = Duration.seconds(3)
    private static let pullThreshold: CGFloat = 72
    private static let stages: [IssueStatus] = [.backlog, .todo, .inProgress, .review, .done]

    init(projectId: String, store: ProjectsStore) {
        self.projectId = projectId
        _projects = ObservedObject(wrappedValue: store)
    }

    /// A move's confirmation line.
    ///
    /// `reverse` is nil for a move that STARTED something. Undoing it would
    /// put the card back while the run it triggered kept going, so the toast
    /// would be offering to unwind something it cannot reach — and a "Queued
    /// for @dev-1" line with an Undo beside it is a lie the operator only
    /// finds out about after pressing.
    struct Toast: Equatable {
        let label: String
        let reverse: Reverse?

        struct Reverse: Equatable {
            let number: Int64
            let to: IssueStatus
        }
    }

    private var project: ProjectInfo? {
        projects.projects.first { $0.id == projectId }
    }
    private var board: ProjectsStore.Board? { projects.boards[projectId] }
    /// An archived board is a read-only record. Its runs are stopped and its
    /// gates self-deny, so every write here would be refused by the server —
    /// better to not offer them than to offer them and be told no.
    private var isArchived: Bool { project?.archivedAtMs != nil }
    private var isReadOnly: Bool { isArchived || projects.isOffline }

    private var waiting: [BoardWaitingStrip.Item] {
        guard let board else { return [] }
        return BoardWaiting.items(
            issues: board.issues, runs: board.runs,
            prompts: projects.approvalPrompts[projectId] ?? [:],
            blockedQuestions: projects.blockedQuestions[projectId] ?? [:])
    }

    var body: some View {
        ZStack(alignment: .top) {
            content
            header
            if let toast { toastBar(toast) }
            if let error = projects.writeError { errorBanner(error) }
        }
        .background(Theme.paper)
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task {
            await projects.refreshBoard(projectId)
            await projects.refreshWaitingDetails(board: projectId)
        }
        // A card's run word carries an elapsed, and a board left open with
        // nothing arriving would freeze it at whatever the last fetch said.
        .task {
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(10))
                now = Date()
            }
        }
        .onChange(of: board?.fetchedAtMs) { _, _ in
            Task { await projects.refreshWaitingDetails(board: projectId) }
        }
        .sheet(item: $moving) { issue in
            MoveSheet(
                issue: issue,
                liveRun: board?.liveRun(for: issue.number),
                assigneeHandle: issue.assignee.map { board?.handle(forAgent: $0) ?? $0 },
                overCeiling: isOverCeiling,
                heldCeiling: heldCeiling,
                onPick: { row in pick(row, for: issue) })
        }
        .sheet(item: $assigning) { issue in
            AssigneePicker(
                team: board?.team ?? [], current: issue.assignee,
                onPick: { agentId in assign(issue, to: agentId) })
        }
        .sheet(isPresented: $showsFilter) {
            BoardFilterSheet(filter: $filter, team: board?.team ?? [])
        }
    }

    // MARK: - Content

    @ViewBuilder private var content: some View {
        if board == nil {
            // A first open with no mirror: skeleton rows in the real rows'
            // geometry, so the list does not jump when the answer lands.
            boardSkeleton
        } else {
            list
        }
    }

    private var list: some View {
        List {
            Section {
                barStrip
                    .listRowInsets(EdgeInsets())
                    .listRowSeparator(.hidden)
                    .listRowBackground(Color.clear)
            }
            ForEach(bandSections, id: \.title) { section in
                Section {
                    ForEach(section.issues, id: \.number) { issue in
                        cardRow(issue)
                    }
                } header: {
                    if let title = section.title, bands.showsHeaders {
                        Text(verbatim: title)
                            .font(Theme.mono(10))
                            .textCase(.uppercase)
                            .kerning(1.2)
                            .foregroundStyle(Theme.inkSoft)
                    }
                }
            }
            if visibleIssues.isEmpty {
                emptyStage
                    .listRowSeparator(.hidden)
                    .listRowBackground(Color.clear)
            }
        }
        .listStyle(.plain)
        // The default section gap leaves a hole between the bar strip and
        // the first band that reads as "the board ended here".
        .listSectionSpacing(14)
        .environment(\.defaultMinListRowHeight, 0)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, ChatListScreen.topContentMargin, for: .scrollContent)
        .contentMargins(.bottom, 90, for: .scrollContent)
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

    @ViewBuilder private func cardRow(_ issue: IssueInfo) -> some View {
        Button {
            appStore.openProjectIssue(project: projectId, number: issue.number)
        } label: {
            IssueCardRow(
                issue: issue,
                run: board?.liveRun(for: issue.number),
                assigneeHandle: issue.assignee.map { board?.handle(forAgent: $0) ?? $0 },
                runnerHandle: runnerHandle(for: issue),
                langCode: lang.current.lproj,
                now: now)
        }
        .buttonStyle(.plain)
        .listRowInsets(EdgeInsets(top: 0, leading: 20, bottom: 0, trailing: 20))
        .listRowBackground(issue.pinned ? Theme.surface : Theme.paper)
        .swipeActions(edge: .leading, allowsFullSwipe: false) {
            if !isReadOnly {
                Button {
                    setPinned(issue, !issue.pinned)
                } label: {
                    Label(
                        lang.t(issue.pinned ? "board.unpin" : "board.pin"),
                        systemImage: issue.pinned ? "pin.slash" : "pin")
                }
                .tint(Theme.ink)
            }
        }
        // Full swipe deliberately off: the trailing action opens a sheet that
        // starts runs, and a flick must not be able to reach it.
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            if !isReadOnly {
                Button {
                    moving = issue
                } label: {
                    Label(lang.t("board.move"), systemImage: "arrow.left.arrow.right")
                }
                .tint(Theme.inkSoft)
            }
        }
        .contextMenu {
            Button {
                appStore.openProjectIssue(project: projectId, number: issue.number)
            } label: {
                Label(lang.t("board.open"), systemImage: "arrow.up.right")
            }
            if !isReadOnly {
                Button { moving = issue } label: {
                    Label(lang.t("board.move"), systemImage: "arrow.left.arrow.right")
                }
                Button { assigning = issue } label: {
                    Label(lang.t("board.assign"), systemImage: "person")
                }
                Button { setPinned(issue, !issue.pinned) } label: {
                    Label(
                        lang.t(issue.pinned ? "board.unpin" : "board.pin"),
                        systemImage: issue.pinned ? "pin.slash" : "pin")
                }
            }
        }
    }

    private var emptyStage: some View {
        Text(verbatim: lang.t("board.emptyStage", MoveConsequence.label(stage)))
            .font(Theme.sys(13))
            .foregroundStyle(Theme.inkSoft)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 46)
    }

    private var boardSkeleton: some View {
        VStack(spacing: 0) {
            ForEach(0..<5, id: \.self) { _ in
                VStack(alignment: .leading, spacing: 6) {
                    RoundedRectangle(cornerRadius: 3).fill(Theme.line).frame(width: 60, height: 9)
                    RoundedRectangle(cornerRadius: 3).fill(Theme.line)
                        .frame(maxWidth: .infinity).frame(height: 13)
                    RoundedRectangle(cornerRadius: 3).fill(Theme.line).frame(width: 120, height: 9)
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 15)
                .frame(minHeight: 78, alignment: .leading)
            }
            Spacer()
        }
        .padding(.top, ChatListScreen.topContentMargin)
        .accessibilityHidden(true)
    }

    // MARK: - The bar strip
    //
    // Segmented control, board row and Waiting strip together — and the ONLY
    // thing that takes a horizontal swipe on this screen. The card rows keep
    // theirs for the swipe actions.

    private var barStrip: some View {
        VStack(spacing: 10) {
            stageSegments
            boardRow
            if !waiting.isEmpty {
                BoardWaitingStrip(
                    items: waiting,
                    onApprove: { number, callId, decision in
                        answer(number: number, callId: callId, decision: decision)
                    },
                    onRetry: { number in retry(number) },
                    onOpen: { number in
                        appStore.openProjectIssue(project: projectId, number: number)
                    })
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 12)
        .contentShape(Rectangle())
        .gesture(
            DragGesture(minimumDistance: 24)
                .onEnded { value in
                    guard abs(value.translation.width) > abs(value.translation.height) else {
                        return
                    }
                    step(by: value.translation.width < 0 ? 1 : -1)
                }
        )
    }

    private var stageSegments: some View {
        HStack(spacing: 4) {
            ForEach(Self.stages, id: \.self) { candidate in
                let issues = board?.issues(in: candidate) ?? []
                Button {
                    guard candidate != stage else { return }
                    Haptics.tap()
                    withAnimation(.easeOut(duration: 0.14)) { stage = candidate }
                } label: {
                    VStack(spacing: 2) {
                        Text(verbatim: Self.shortLabel(candidate))
                            .font(Theme.mono(10))
                            .kerning(0.4)
                        Text(verbatim: "\(BoardOrder.liveCount(issues))")
                            .font(Theme.mono(12, weight: .medium))
                    }
                    .foregroundStyle(candidate == stage ? Theme.paper : Theme.inkSoft)
                    .frame(maxWidth: .infinity)
                    .frame(height: 40)
                    .background(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .fill(candidate == stage ? Theme.ink : Color.clear)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(candidate == stage ? Color.clear : Theme.line, lineWidth: 1)
                    )
                    // A dot rather than a number: pressing the segment cannot
                    // discharge it — opening the cards can.
                    .overlay(alignment: .topTrailing) {
                        if candidate != stage, BoardOrder.hasNews(inStage: issues) {
                            Circle().fill(Theme.err).frame(width: 6, height: 6)
                                .offset(x: -5, y: 5)
                        }
                    }
                    .contentShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("stage-\(MoveSheet.identifier(candidate))")
                .accessibilityLabel(
                    Text(
                        verbatim: "\(MoveConsequence.label(candidate)) \(BoardOrder.liveCount(issues))"
                    ))
            }
        }
    }

    private var boardRow: some View {
        HStack(spacing: 8) {
            TeamFaces(team: board?.team ?? [], runs: board?.runs ?? [])
            Spacer(minLength: 6)
            if let meter = budgetMeter, meter.burn == .over {
                Button {
                    // Settings lands in P7; until then the chip still says
                    // which ceiling stopped the board, which is the part an
                    // operator acts on.
                    Haptics.tap()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "pause.circle")
                            .font(.system(size: 10, weight: .medium))
                        Text(verbatim: "\(meter.spent) / \(meter.limit)")
                            .font(Theme.mono(10))
                    }
                    .foregroundStyle(Theme.ink)
                    .padding(.horizontal, 8)
                    .frame(height: 26)
                    .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("board-budget-chip")
            }
            filterChip
            boardMenu
        }
    }

    private var filterChip: some View {
        Button {
            Haptics.tap()
            showsFilter = true
        } label: {
            HStack(spacing: 4) {
                Image(systemName: "line.3.horizontal.decrease")
                    .font(.system(size: 10, weight: .medium))
                if filter.count > 0 {
                    Text(verbatim: "\(filter.count)")
                        .font(Theme.mono(10))
                }
            }
            .foregroundStyle(filter.isActive ? Theme.paper : Theme.inkSoft)
            .padding(.horizontal, 9)
            .frame(height: 26)
            .background(filter.isActive ? Theme.ink : Color.clear, in: Capsule())
            .overlay(
                Capsule().strokeBorder(filter.isActive ? Color.clear : Theme.line, lineWidth: 1))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("board-filter-chip")
        .accessibilityLabel(Text(verbatim: lang.t("board.filter")))
    }

    private var boardMenu: some View {
        Menu {
            Button {
                Haptics.tap()
                markAllRead()
            } label: {
                Label(lang.t("board.markAllRead", "\(unreadTotal)"), systemImage: "envelope.open")
            }
            .disabled(unreadTotal == 0 || isReadOnly)
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(Theme.inkSoft)
                .frame(width: 30, height: 26)
                .contentShape(Rectangle())
        }
        .accessibilityIdentifier("board-menu")
        .accessibilityLabel(Text(verbatim: lang.t("list.menu")))
    }

    // MARK: - Header

    private var header: some View {
        ZStack {
            Text(verbatim: project?.name ?? lang.t("home.tab.projects"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                .padding(.horizontal, 66)
                .overlay(alignment: .trailing) {
                    BoardRefreshRing(isRefreshing: isRefreshing).offset(x: 16)
                }

            HStack {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("board.back")))
                Spacer()
                if isArchived {
                    Text(verbatim: lang.t("projects.archivedChip"))
                        .font(Theme.mono(10))
                        .foregroundStyle(Theme.inkSoft)
                        .padding(.horizontal, 8)
                        .frame(height: 22)
                        .background(Theme.paper, in: Capsule())
                        .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
                }
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

    // MARK: - Toast and banner

    private func toastBar(_ toast: Toast) -> some View {
        VStack {
            Spacer()
            HStack(spacing: 18) {
                Text(verbatim: toast.label)
                    .font(Theme.sys(13))
                    .foregroundStyle(Theme.paper)
                    .lineLimit(1)
                if let reverse = toast.reverse {
                    Button {
                        Haptics.tap()
                        undoMove(reverse)
                    } label: {
                        Text(verbatim: lang.t("list.undo"))
                            .font(Theme.sys(13, weight: .bold))
                            .foregroundStyle(Theme.paper)
                            .frame(minHeight: 44)
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("board-undo")
                }
            }
            .padding(.horizontal, 22)
            .frame(height: 44)
            .background(Theme.ink, in: Capsule())
            .shadow(color: Theme.ink.opacity(0.18), radius: 16, y: 6)
            .padding(.bottom, 30)
        }
        .transition(.opacity)
    }

    /// The server's own sentence. Never paraphrased: the board's refusals name
    /// which ceiling, which block, which card holds the slot — and that is the
    /// only part an operator can act on.
    private func errorBanner(_ message: String) -> some View {
        VStack {
            Spacer()
            HStack(spacing: 12) {
                Text(verbatim: message)
                    .font(Theme.sys(12.5))
                    .foregroundStyle(Theme.paper)
                    .multilineTextAlignment(.leading)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
                Button {
                    projects.clearWriteError()
                } label: {
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
            .background(
                RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.err))
            .padding(.horizontal, 16)
            .padding(.bottom, 26)
            .accessibilityIdentifier("board-write-error")
        }
    }

    // MARK: - Derived

    private var visibleIssues: [IssueInfo] {
        filter.apply(board?.issues(in: stage) ?? [], runs: board?.runs ?? [])
    }
    private var bands: BoardOrder.Bands { BoardOrder.bands(visibleIssues) }

    private var bandSections: [(title: String?, issues: [IssueInfo])] {
        let bands = self.bands
        return [
            (lang.t("board.bandPinned"), bands.pinned),
            (lang.t("board.bandNew"), bands.new),
            (lang.t("board.bandQueue"), bands.queue),
        ].filter { !$0.1.isEmpty }
    }

    private var unreadTotal: Int {
        Int((board?.issues ?? []).filter { $0.cancelledAtMs == nil }.reduce(0) { $0 + $1.unread })
    }

    private var budgetMeter: BudgetMeter.Meter? {
        guard let project else { return nil }
        let activity = projects.activity[projectId]
        return BudgetMeter.meter(
            burnMicros: activity?.burnMicros ?? 0, burnTokens: activity?.burnTokens ?? 0,
            limitMicros: project.dailyBudgetMicros, limitTokens: project.dailyBudgetTokens)
    }
    private var isOverCeiling: Bool { budgetMeter?.burn == .over }
    /// Which ceiling the board is over. Taken from the meter rather than
    /// guessed: an operator told "over its daily budget" on a token-limited
    /// board goes and raises a dollar figure that was never what stopped it.
    private var heldCeiling: MoveConsequence.HeldCeiling {
        budgetMeter?.ceiling ?? .unknown
    }

    private func runnerHandle(for issue: IssueInfo) -> String? {
        guard let board, let run = board.liveRun(for: issue.number),
            RunLabels.runnerDiffersFromAssignee(run: run, assignee: issue.assignee)
        else { return nil }
        return board.handle(forAgent: run.agentId)
    }

    static func shortLabel(_ status: IssueStatus) -> String {
        switch status {
        case .backlog: "BACK"
        case .todo: "TODO"
        case .inProgress: "PROG"
        case .review: "REV"
        case .done: "DONE"
        case .unknown: "?"
        }
    }

    // MARK: - Actions

    private func step(by delta: Int) {
        guard let index = Self.stages.firstIndex(of: stage) else { return }
        let next = index + delta
        guard next >= 0, next < Self.stages.count else { return }
        Haptics.tap()
        withAnimation(.easeOut(duration: 0.16)) { stage = Self.stages[next] }
    }

    private func pick(_ row: MoveConsequence.Row, for issue: IssueInfo) {
        guard !row.needsAssignee else {
            assignThenMoveTo = row.status
            assigning = issue
            return
        }
        Task { await runMove(issue, row: row) }
    }

    private func runMove(_ issue: IssueInfo, row: MoveConsequence.Row) async {
        let from = issue.status
        let handle = issue.assignee.map { board?.handle(forAgent: $0) ?? $0 }
        guard await projects.move(board: projectId, issue: issue.number, to: row.status) else {
            return
        }
        // Every move says what it did; only one that started nothing offers to
        // take it back.
        show(
            Toast(
                label: MoveConsequence.toast(
                    afterMoving: issue.number, to: row, assigneeHandle: handle),
                reverse: MoveConsequence.isUndoable(row)
                    ? Toast.Reverse(number: issue.number, to: from) : nil))
    }

    private func undoMove(_ reverse: Toast.Reverse) {
        withAnimation(.easeIn(duration: 0.15)) { toast = nil }
        Task { await projects.move(board: projectId, issue: reverse.number, to: reverse.to) }
    }

    private func assign(_ issue: IssueInfo, to agentId: String?) {
        let target = assignThenMoveTo
        assignThenMoveTo = nil
        Task {
            guard await projects.assign(board: projectId, issue: issue.number, to: agentId) else {
                return
            }
            // The picker was opened BY a move, so finish it — and re-derive the
            // consequence from the card as it now is, rather than replaying the
            // row that was built when nobody was assigned.
            guard let target, let agentId,
                let fresh = projects.boards[projectId]?.issues.first(
                    where: { $0.number == issue.number })
            else { return }
            let handle = projects.boards[projectId]?.handle(forAgent: agentId) ?? agentId
            let rows = MoveConsequence.rows(
                for: fresh, liveRun: projects.boards[projectId]?.liveRun(for: fresh.number),
                assigneeHandle: handle, overCeiling: isOverCeiling, heldCeiling: heldCeiling)
            guard let row = rows.first(where: { $0.status == target }) else { return }
            await runMove(fresh, row: row)
        }
    }

    private func setPinned(_ issue: IssueInfo, _ pinned: Bool) {
        Haptics.tap()
        Task {
            // The swipe panel's teardown stalls List's reorder, so the row
            // would sit in a blank slot while UIKit finishes. Letting the
            // panel close first costs nothing the eye can see and buys a
            // destination slot that is not empty.
            try? await Task.sleep(for: .milliseconds(320))
            await projects.setPinned(board: projectId, issue: issue.number, pinned)
        }
    }

    private func answer(number: Int64, callId: String, decision: IssueApprovalDecision) {
        Haptics.tap()
        Task {
            await projects.resolveApproval(
                board: projectId, issue: number, callId: callId, decision: decision)
            await projects.refreshWaitingDetails(board: projectId)
        }
    }

    private func retry(_ number: Int64) {
        Haptics.tap()
        Task { await projects.retryRun(board: projectId, issue: number) }
    }

    private func markAllRead() {
        Task { await projects.markAllRead(board: projectId) }
    }

    private func triggerRefresh() {
        guard !isRefreshing else { return }
        isRefreshing = true
        Task {
            await projects.refreshBoard(projectId)
            await projects.refreshWaitingDetails(board: projectId)
            withAnimation(.easeOut(duration: 0.2)) { isRefreshing = false }
        }
    }

    private func show(_ next: Toast) {
        withAnimation(.easeOut(duration: 0.18)) { toast = next }
        toastDismissTask?.cancel()
        toastDismissTask = Task { @MainActor in
            try? await Task.sleep(for: Self.toastWindow)
            guard !Task.isCancelled else { return }
            withAnimation(.easeIn(duration: 0.18)) { toast = nil }
        }
    }
}

/// `IssueInfo` is a UniFFI record, so it carries no `Identifiable`; the sheet
/// bindings need one and a card's number is its identity within a board.
extension IssueInfo: @retroactive Identifiable {
    public var id: Int64 { number }
}

/// The board header's pull indicator. Same shape as the chat list's, which is
/// `private` to that file — one ring is not worth a shared module, but two
/// that drift apart would be a visible bug on two screens with one gesture.
private struct BoardRefreshRing: View {
    var isRefreshing: Bool
    @State private var angle: Double = 0

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.72)
            .stroke(Theme.inkSoft, style: StrokeStyle(lineWidth: 1, lineCap: .round))
            .frame(width: 10, height: 10)
            .rotationEffect(.degrees(angle))
            .opacity(isRefreshing ? 1 : 0)
            .animation(.easeOut(duration: 0.2), value: isRefreshing)
            .onChange(of: isRefreshing) { _, on in
                if on {
                    angle = 0
                    withAnimation(.linear(duration: 0.8).repeatForever(autoreverses: false)) {
                        angle = 360
                    }
                } else {
                    withAnimation(.easeOut(duration: 0.15)) { angle = 0 }
                }
            }
            .accessibilityHidden(true)
    }
}
