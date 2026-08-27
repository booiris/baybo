import SwiftUI

/// One card, pushed over its board.
///
/// `ChatScreen`'s layering, applied to a different page: a native header, a
/// full-page webview, a native dock, and the dock's top edge streamed to the
/// web side as its bottom inset. The webview never resizes — that is the whole
/// reason the inset is streamed rather than the frame animated, and it is why
/// the keyboard can ride up without the page reflowing under it.
struct ProjectIssueScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @StateObject private var visit: IssueVisit
    @Environment(\.dismiss) private var dismiss

    let projectId: String
    let number: Int64

    private var store: IssueStore { visit.store }
    /// The `+`'s panel, owned HERE rather than by the dock: it floats over the
    /// dock's own rows — the hint line, the approval card, the strip — and a
    /// panel presented from inside them draws behind them and loses its taps.
    /// `ChatScreen` holds its own for the same reason.
    @StateObject private var attach = AttachMenu()
    @State private var confirmingStop = false
    /// Which chip was pressed. Typed rather than the bridge's raw string: a
    /// field this build has never heard of opens nothing instead of a sheet
    /// with no contents.
    @State private var picking: PickerField?
    /// The assignee picker, on its OWN presentation rather than a third case
    /// of `picking`. It has two callers — the chip, and a Move that needs
    /// somebody on the card first — and the second raises it from inside the
    /// Move sheet's dismissal, which is one presentation source replacing
    /// itself mid-flight. Two sources is what the board does, for this reason.
    @State private var assigning = false
    /// Set when the assignee picker was opened BY a move, so the move can
    /// finish once somebody is on the card.
    @State private var assignThenMoveTo: IssueStatus?
    /// The run whose transcript is open. Addressed by attempt, never by
    /// session id — an attempt that never started has no session and is
    /// still a row somebody wants to read.
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
        // The card page owns its own bottom padding from the streamed inset;
        // letting the shell ride the keyboard would move the webview's frame
        // underneath it and reflow the whole card.
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .safeAreaInset(edge: .bottom, spacing: 0) { dock }
        // Re-runs on every re-appear, which is what the refetch wants: a card
        // that was covered while its board moved on comes back current. The
        // pool keeps the renderer warm; this refresh only replaces data.
        .task { await store.refresh() }
        // The page's two requests, ANSWERED HERE and cleared. They arrive as
        // store state rather than as closures installed on the bridge, because
        // a closure written here captures this whole struct and a pool bridge
        // outlives many such bodies. See `IssueBridge`.
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
        .onChange(of: lang.current.lproj) { _, code in store.setLanguage(code) }
        .onAppear {
            // The paste row's source. A process-global slot with one occupant:
            // the chat screen and this one are never both on screen, and the
            // registrant is the only thing that can clear it, so the handover
            // survives an appear that lands before the other's disappear.
            ComposerPasteTarget.shared.attach(store.staging)
        }
        // ONLY reversible work belongs here: a push fires this too — tapping a
        // sub-issue covers this card without ending the visit — so anything
        // irreversible would destroy the page the reader is coming back to.
        // The paste target survives it because `detach` is identity-guarded and
        // `onAppear` puts it back; the visit/store destructors fire when the
        // navigation entry actually goes.
        .onDisappear {
            ComposerPasteTarget.shared.detach(store.staging)
        }
        .sheet(item: $picking) { field in
            picker(field)
        }
        .sheet(isPresented: $assigning) {
            // A picker closed WITHOUT an answer must not leave the move armed:
            // the next time somebody is assigned — from the chip, minutes
            // later — it would carry the card to a column nobody asked for.
            // `onPick` has already taken the target by the time this runs (the
            // sheet calls `dismiss()` first and this fires on the animation),
            // so clearing here only ever catches the swipe-away.
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
        .confirmationDialog(
            lang.t("issue.stopTitle"), isPresented: $confirmingStop, titleVisibility: .visible
        ) {
            Button(lang.t("issue.stopConfirm"), role: .destructive) { store.stopRun() }
            Button(lang.t("common.cancel"), role: .cancel) {}
        } message: {
            Text(verbatim: lang.t("issue.stopExplain"))
        }
    }

    private var page: some View {
        IssueWebView(lease: visit.lease)
            // The frame stays fixed while the keyboard moves, exactly like
            // the chat transcript. The dock/keyboard obstruction reaches the
            // page once, through `setComposerTop`; letting the keyboard safe
            // area shrink this view as well charges that height twice and a
            // jump exposes the duplicate as a keyboard-sized blank tail.
            .ignoresSafeArea(.all, edges: .bottom)
    }

    private var dock: some View {
        ComposerDock(collapsed: false) {
            IssueDock(store: store, staging: store.staging, attach: attach)
                // The DOCK's own top edge is the page's bottom obstruction —
                // measured on the dock itself, so the disc floating above it
                // does not inflate the inset and reflow the card under a
                // button that only appeared. `ChatScreen` measures its
                // composer for the same reason.
                .onGeometryChange(for: CGFloat.self) { proxy in
                    proxy.frame(in: .global).minY
                } action: { _, minY in
                    store.setComposerTop(minY)
                }
                // An overlay rather than a row above the dock: the attach
                // panel hangs off this content's top edge, and a disc in the
                // stack raised the panel by the disc's own height.
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
            Text(verbatim: "#\(number)")
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)

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

    /// Whether the ⋯ has anything in it.
    ///
    /// Both of its unconditional entries left on 2026-08-26 — the description
    /// editor lost its door, and Rebuild moved to the board row's long press,
    /// which is where you reach for it when the CARD is the thing that looks
    /// wrong. What is left is about runs and is conditional, so the button has
    /// to be too: a ⋯ that opens an empty sheet is worse than no ⋯.
    private var hasMenu: Bool {
        !store.runs.isEmpty || store.issue?.lastRunFailed == true
    }

    private var menu: some View {
        Menu {
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

    /// Every attempt this card has made, newest first — **the card page used to
    /// list them and no longer does.**
    ///
    /// A settled run is history: it is read once, when something went wrong,
    /// and it sat between the card's state and its comments on every open. The
    /// ⋯ is where the other things you can do to a card already are, and a
    /// submenu costs the one tap that history is worth.
    ///
    /// The rows come in the order the gateway sends (`ORDER BY attempt DESC`),
    /// so the newest attempt is the first thing the menu offers. An attempt
    /// that never started has no session and therefore no transcript — it is
    /// still a row somebody wants to SEE, so it is listed and disabled rather
    /// than hidden, which is the difference between "there was no third
    /// attempt" and "the third attempt never got a slot".
    @ViewBuilder private var runsMenu: some View {
        if !store.runs.isEmpty {
            Menu {
                ForEach(store.runs, id: \.self) { run in
                    Button {
                        Haptics.tap()
                        show(run)
                    } label: {
                        Text(verbatim: runLabel(run))
                        // A menu row's SECOND text is its subtitle. The
                        // server's sentence about why a run failed used to be
                        // on the page's run list, truncated to one line; this
                        // is the only place left that can carry it, and it is
                        // the one thing a settled attempt is opened for.
                        if let error = run.error, !error.isEmpty {
                            Text(verbatim: error)
                        }
                        Image(systemName: runIcon(run.status))
                    }
                    .disabled(run.sessionId == nil)
                    // The subtitle is DRAWN and nothing else: a menu row
                    // exposes only its title to accessibility, so without this
                    // the one sentence saying what went wrong is invisible to
                    // VoiceOver — and to any test that is not sampling pixels.
                    .accessibilityLabel(Text(verbatim: runReading(run)))
                }
            } label: {
                Label(lang.t("issue.runs"), systemImage: "clock.arrow.circlepath")
            }
            .accessibilityIdentifier("issue-runs")
        }
    }

    /// `#3 · Working · @dev-1` — the attempt, what became of it, and who ran
    /// it. One line: a menu row truncates, and of the three the handle is the
    /// one a reader can afford to lose.
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

    /// Open a run's transcript. The ONE door — the page's `Open run ›` and the
    /// ⋯ both come through here, so a run that cannot be opened is refused in
    /// one place rather than two.
    private func show(_ run: IssueRunInfo) {
        guard let sessionId = run.sessionId else { return }
        openRun = ProjectRunRoute(
            projectId: projectId, number: number, attempt: run.attempt,
            sessionId: sessionId, status: run.status)
    }

    // MARK: - The chips
    //
    // The three things a card page can change about the card itself. The web
    // half posts `pick`; everything from here down is native, and every write
    // goes through `ProjectsStore` rather than this card's own store — a move
    // sends the destination column's WHOLE order and a move into In Progress
    // starts a run, and neither rule may have a second implementation on the
    // one screen that is not the board.

    /// What a chip can ask for, as the PAGE spells it (`issue/bridge.ts`'s
    /// `pickField`). Wider than `PickerField` on purpose: the assignee has its
    /// own presentation, and `stage` is a field the page can name that this
    /// screen has no picker for — both have to be sayable here so the switch
    /// below is exhaustive and the compiler is the thing that notices when the
    /// page grows a fourth.
    private enum CardField: String {
        case status, priority, assignee, stage
    }

    /// Which picker is on screen, when it is one of the two that share a
    /// presentation.
    private enum PickerField: String, Identifiable {
        case status, priority
        var id: String { rawValue }
    }

    /// Raise the picker a chip asked for. A field this build has never heard
    /// of raises nothing — a sheet with no contents is worse than a chip that
    /// stays put.
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
                    // Which ceiling, not merely that there is one: an operator
                    // told "over its daily budget" on a token-limited board
                    // goes and raises a dollar figure that never stopped it.
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

    /// One write, then this card's own copy of the truth.
    ///
    /// The board store owns the rule and the optimistic edit; the card holds a
    /// SECOND copy of the same row (its own fetch, its own mirror), so without
    /// the refetch the chip the operator just changed keeps saying what it
    /// said. The board's refusal is already on `ProjectsStore.writeError`; the
    /// banner this screen draws reads the card's, so the sentence is carried
    /// over rather than dropped.
    @discardableResult
    private func commit(_ write: (ProjectsStore) async -> Bool) async -> Bool {
        let projects = appStore.projectsStore
        // A move sends the destination column's whole order, and the gateway
        // refuses a partial one — so a card opened without its board (a push
        // tap) fetches the board before it writes rather than being told off
        // by the server.
        if projects.boards[projectId] == nil { await projects.refreshBoard(projectId) }
        guard await write(projects) else {
            store.showWriteError(projects.writeError)
            return false
        }
        await store.refresh()
        return true
    }

    /// The server's own sentence — never paraphrased, for the reason the
    /// board's banner says it: the refusals name which ceiling, which block,
    /// which card holds the slot.
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
