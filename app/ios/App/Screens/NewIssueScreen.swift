import SwiftUI

/// File a card on a board.
///
/// A pushed route rather than a sheet, for the reason the new-board form is
/// one: the home shell opts out of keyboard avoidance wholesale
/// (`HomeTabView.ignoresSafeArea(.keyboard)`), and a pushed screen sits
/// outside that, so the title field can rise with the keyboard the ordinary
/// way.
///
/// **It opens in the column you were looking at**, which is the web's rule
/// (`CreateIssueModal`'s `initialStatus`) and the only one that does not
/// surprise: filing from the Todo tab and finding the card in Backlog is a
/// small betrayal every time.
struct NewIssueScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    let projectId: String
    /// The stage the board was showing. The card opens here.
    let initialStatus: IssueStatus

    @State private var title = ""
    @State private var description = ""
    @State private var status: IssueStatus
    @State private var priority: IssuePriority = .none
    @State private var assignee: String?
    @State private var showingAssignee = false
    @State private var creating = false
    @State private var failure: String?
    @FocusState private var titleFocused: Bool

    private static let stages: [IssueStatus] = [.backlog, .todo, .inProgress, .review, .done]
    private static let priorities: [IssuePriority] = [.urgent, .high, .medium, .low, .none]

    init(projectId: String, initialStatus: IssueStatus) {
        self.projectId = projectId
        self.initialStatus = initialStatus
        _status = State(initialValue: initialStatus)
    }

    private var board: ProjectsStore.Board? { appStore.projectsStore.boards[projectId] }
    private var team: [TeamMemberInfo] { board?.team ?? [] }
    private var assigneeHandle: String? {
        assignee.map { id in board?.handle(forAgent: id) ?? id }
    }

    /// The server refuses In Progress without somebody on it
    /// (`validate_staffing`), so the button does not offer to try.
    private var canCreate: Bool {
        !title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !creating
            && !MoveConsequence.refusesOpening(in: status, assignee: assignee)
    }

    var body: some View {
        ZStack(alignment: .top) {
            form
            header
        }
        .background(Theme.paper)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .sheet(isPresented: $showingAssignee) {
            AssigneePicker(team: team, current: assignee) { picked in
                assignee = picked
            }
        }
        .onAppear { titleFocused = true }
    }

    private var form: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                TextField(lang.t("newIssue.titlePlaceholder"), text: $title, axis: .vertical)
                    .font(Theme.sys(18, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1...3)
                    .focused($titleFocused)
                    .padding(.horizontal, ProjectsLayout.gutter)
                    .padding(.top, 8)
                    .accessibilityIdentifier("new-issue-title")

                TextField(
                    lang.t("newIssue.descriptionPlaceholder"), text: $description, axis: .vertical
                )
                .font(Theme.sys(14.5))
                .foregroundStyle(Theme.ink)
                .lineLimit(3...12)
                .padding(.horizontal, ProjectsLayout.gutter)
                .padding(.top, 14)
                .accessibilityIdentifier("new-issue-description")

                section(lang.t("newIssue.opensIn"))
                stageChips
                // Only In Progress has anything to say — the other four open a
                // card and do nothing, and a sentence about a run that never
                // existed would be worse than silence.
                if let note = MoveConsequence.openingNote(
                    in: status, assigneeHandle: assigneeHandle,
                    overCeiling: isOverCeiling, heldCeiling: heldCeiling)
                {
                    Text(verbatim: note)
                        .font(Theme.sys(12))
                        .foregroundStyle(Theme.inkSoft)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, ProjectsLayout.gutter)
                        .padding(.top, 8)
                        .accessibilityIdentifier("new-issue-consequence")
                }

                section(lang.t("newIssue.priority"))
                priorityChips

                section(lang.t("newIssue.assignee"))
                assigneeRow

                if let failure {
                    Text(verbatim: failure)
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.err)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, ProjectsLayout.gutter)
                        .padding(.top, 16)
                }

                Button(lang.t("newIssue.create")) { create() }
                    .buttonStyle(InkPillButtonStyle())
                    .disabled(!canCreate)
                    .opacity(canCreate ? 1 : 0.35)
                    .padding(.horizontal, ProjectsLayout.gutter)
                    .padding(.top, 26)
                    .padding(.bottom, 60)
                    .accessibilityIdentifier("new-issue-create")
            }
        }
        .contentMargins(.top, ProjectsLayout.topInset, for: .scrollContent)
        .scrollContentBackground(.hidden)
        .scrollDismissesKeyboard(.interactively)
    }

    private var stageChips: some View {
        HStack(spacing: 5) {
            ForEach(Self.stages, id: \.self) { stage in
                chip(
                    ProjectBoardScreen.shortLabel(stage),
                    selected: status == stage,
                    identifier: "new-issue-stage-\(MoveSheet.identifier(stage))"
                ) {
                    status = stage
                }
            }
        }
        .padding(.horizontal, ProjectsLayout.gutter)
    }

    private var priorityChips: some View {
        HStack(spacing: 5) {
            ForEach(Self.priorities, id: \.self) { level in
                chip(
                    IssueCardRow.priorityMark(level),
                    selected: priority == level,
                    identifier:
                        "new-issue-priority-\(IssueCardRow.priorityWord(level).lowercased())"
                ) {
                    priority = level
                }
                .accessibilityLabel(Text(verbatim: IssueCardRow.priorityWord(level)))
            }
        }
        .padding(.horizontal, ProjectsLayout.gutter)
    }

    private var assigneeRow: some View {
        Button {
            Haptics.tap()
            showingAssignee = true
        } label: {
            HStack(spacing: 10) {
                if let handle = assigneeHandle {
                    AgentFace(handle: handle, size: 22)
                    Text(verbatim: "@\(handle)")
                        .font(Theme.sys(14))
                        .foregroundStyle(Theme.ink)
                } else {
                    Text(verbatim: lang.t("board.unassign"))
                        .font(Theme.sys(14))
                        .foregroundStyle(Theme.inkSoft)
                }
                Spacer(minLength: 6)
                Image(systemName: "chevron.right")
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Theme.line)
            }
            .padding(.horizontal, ProjectsLayout.gutter)
            .frame(minHeight: 48)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("new-issue-assignee")
    }

    private func chip(
        _ text: String, selected: Bool, identifier: String, action: @escaping () -> Void
    ) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Text(verbatim: text)
                .font(Theme.mono(10))
                .kerning(0.4)
                .foregroundStyle(selected ? Theme.paper : Theme.inkSoft)
                .frame(maxWidth: .infinity)
                .frame(height: 34)
                .background(
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .fill(selected ? Theme.ink : Color.clear)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 9, style: .continuous)
                        .strokeBorder(selected ? Color.clear : Theme.line, lineWidth: 1)
                )
                .contentShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier(identifier)
        .accessibilityValue(Text(verbatim: selected ? "1" : "0"))
    }

    private func section(_ title: String) -> some View {
        Text(verbatim: title)
            .font(Theme.mono(10.5))
            .textCase(.uppercase)
            .kerning(1.2)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, ProjectsLayout.gutter)
            .padding(.top, 22)
            .padding(.bottom, 8)
    }

    private var header: some View {
        ZStack {
            Text(verbatim: lang.t("newIssue.title"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)
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

    private var meter: BudgetMeter.Meter? {
        guard let project = appStore.projectsStore.projects.first(where: { $0.id == projectId })
        else { return nil }
        let activity = appStore.projectsStore.activity[projectId]
        return BudgetMeter.meter(
            burnMicros: activity?.burnMicros ?? 0, burnTokens: activity?.burnTokens ?? 0,
            limitMicros: project.dailyBudgetMicros, limitTokens: project.dailyBudgetTokens)
    }
    private var isOverCeiling: Bool { meter?.burn == .over }
    private var heldCeiling: MoveConsequence.HeldCeiling { meter?.ceiling ?? .unknown }

    private func create() {
        guard canCreate else { return }
        creating = true
        failure = nil
        Haptics.tap()
        Task {
            do {
                let issue = try await Baybo.client.projectIssueCreate(
                    projectId: projectId,
                    new: NewIssue(
                        title: title.trimmingCharacters(in: .whitespacesAndNewlines),
                        description: description,
                        // Attachments want the composer's staging strip, which
                        // is bound to `ChatStore`; a card takes files from its
                        // own page instead.
                        attachments: [],
                        status: status,
                        priority: priority,
                        assignee: assignee,
                        parent: nil,
                        stage: nil))
                await appStore.projectsStore.refreshBoard(projectId)
                creating = false
                // Straight into the card that was just filed — it is what you
                // were making, and the board behind it is already refreshed.
                appStore.chatPath.removeAll {
                    if case let .newIssue(project, _) = $0 { return project == projectId }
                    return false
                }
                appStore.openProjectIssue(project: projectId, number: issue.number)
            } catch {
                creating = false
                failure = ProjectsStore.message(from: error)
            }
        }
    }
}
