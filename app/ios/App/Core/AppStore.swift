import SwiftUI

/// The root state machine: unbound (landing / direct login / scan / pair
/// confirm) vs bound (home). Home is the chat list — sessions are entered by
/// pushing onto `chatPath` (a tap, a compose, a push-notification route) and
/// left with the back chevron or the edge-swipe pop.
@MainActor
final class AppStore: ObservableObject {
    /// The app's one live store, reachable from UIKit delegate callbacks (the
    /// notification-tap router) that sit outside the SwiftUI environment.
    private(set) static weak var shared: AppStore?

    enum Route: Equatable {
        case launching
        case landing
        case home
    }

    enum LandingView {
        case menu
        case direct
    }

    /// The home shell's bottom-menu sections. `deck` (the board of
    /// agent-authored live cards — docs/modules/deck.md), `chats`, and
    /// `settings` have real screens; `projects` is a placeholder. Compose and
    /// push-tap routing force `chats` so backing out of a conversation lands on
    /// the list, not whatever section was showing.
    enum HomeTab: CaseIterable {
        case deck
        case projects
        case chats
        case settings
    }

    /// One entry on the outer NavigationStack over the home shell: a pushed
    /// conversation, the archived list, or one scheduled job's fires.
    enum ChatRoute: Hashable {
        case session(String)
        case archived
        /// A **cron group** (`docs/cron-groups.md`): every fire of one cron job,
        /// keyed by the job id. A pushed screen rather than an inline expansion —
        /// `Section(isExpanded:)`'s chevron only renders under `.listStyle(.sidebar)`,
        /// and `DisclosureGroup` nests rows inside one cell, which kills per-row
        /// `.swipeActions`. The Archived screen already established this shape.
        case cronGroup(String)
        /// The owner's live scheduled JOBS — the schedules, not the fires. A
        /// sibling of `archived`: same ☰ menu, same push, no argument (the list
        /// fetches itself).
        case cronJobs
    }

    /// A scheduled job's delete, waiting on the confirm dialog. Carries the name
    /// so the dialog can say WHICH job it is about to stop — the list is the only
    /// place these are told apart, and the dialog covers it.
    struct PendingCronJobDelete: Equatable {
        let jobId: String
        let name: String
    }

    /// A cron group's delete, waiting on the confirm dialog.
    ///
    /// The members are **snapshotted when the dialog opens**, and the confirm
    /// deletes exactly those: the dialog names a count ("all 12 execution
    /// records"), and a group is a live view over its fires — the job can fire
    /// again while the user reads. Re-deriving the members at the tap would
    /// delete a fire the user was never shown, which is the one thing a
    /// destructive confirm must not do.
    struct PendingCronGroupDelete: Equatable {
        let jobId: String
        /// The group's **visible** members. An escapee (pinned, archived) is its
        /// own row elsewhere and is deleted from where it lives.
        let memberIds: [String]
    }

    @Published var route: Route = .launching
    @Published var landingView: LandingView = .menu
    /// The selected home section (the bottom menu bar's current tab).
    @Published var homeTab: HomeTab = .chats
    /// The NavigationStack path over the chat list. A session opened from the
    /// list/compose/push resets it to `[.session(id)]`; one opened from the
    /// archived screen appends, so the pop chain runs chat → archived → list.
    ///
    /// Popping the last conversation off the stack stops chat audio: a track
    /// playing over the list — with no visible card to control it — reads as
    /// a bug, not a feature. The path only changes on navigation, so
    /// lock-screen/background playback while parked IN a chat is untouched.
    /// (`ChatScreen.onDisappear` can't be this hook: it also fires under
    /// fullScreenCovers like the image viewer.)
    @Published var chatPath: [ChatRoute] = [] {
        didSet {
            if Self.hasSession(oldValue) && !Self.hasSession(chatPath) {
                AudioPlayerCenter.shared.stop()
            }
        }
    }

    private static func hasSession(_ path: [ChatRoute]) -> Bool {
        path.contains { route in
            if case .session = route { return true }
            return false
        }
    }
    /// Landing / pairing status line (localized, already resolved).
    @Published var status: String?
    @Published var busy = false
    /// Non-nil while the pair-confirm screen is up.
    @Published var challenge: PairChallenge?
    @Published var scanPresented = false
    /// The hand-rolled logout confirm (`ConfirmDialog` in `RootView`). App-shell
    /// state, not screen state, so the overlay can dim the ENTIRE shell — tab
    /// bar included — and tab switches can't orphan a system presentation (the
    /// stock `.confirmationDialog` left `isPresented` latched true after a
    /// scrim dismiss, deadening the logout button).
    @Published var confirmLogout = false
    /// The session a swipe-delete is asking to confirm — hosted in `RootView`
    /// exactly like the logout confirm, and for the same latch/coverage reasons.
    @Published var confirmDeleteSession: String?
    /// The cron group a swipe-delete is asking to confirm, same host and reasons.
    @Published var confirmDeleteCronGroup: PendingCronGroupDelete?
    /// Job id → name, learned from the scheduled-jobs list. See
    /// `rememberCronJobTitles`.
    @Published private(set) var cronJobTitles: [String: String] = [:]
    /// The scheduled job a swipe-delete is asking to confirm.
    @Published var confirmDeleteCronJob: PendingCronJobDelete?
    /// The scheduled-jobs list `CronJobsScreen` renders.
    ///
    /// Held here rather than in the screen's `@State` for the reason the chat
    /// list's rows are in `SessionIndex`: the delete confirm mounts at the app
    /// root (it must dim the whole shell), so the rows it removes have to be
    /// reachable from outside the screen. `nil` = never loaded; `[]` = proven
    /// empty — only the latter may say "no scheduled jobs".
    ///
    /// In memory only. It is re-fetched on every appear, so what survives here is
    /// one screen's worth of rows to paint while that lands — not a cache that
    /// can outlive the app and disagree with the gateway about what is scheduled.
    @Published private(set) var cronJobs: [CronJobSummary]?
    /// A cron mutation that failed, rendered by the jobs screen. Its own line, not
    /// `sessionNotice`: that belongs to the chat list's mutations, and would
    /// surface this on a screen the user already left.
    @Published var cronJobNotice: String?
    /// Transient archive/delete failure line, rendered by the list headers the
    /// way compose failures are. Cleared when the next mutation starts.
    @Published var sessionNotice: String?
    /// Whether the active binding is direct (drives push re-registration).
    @Published private(set) var directBound = false

    private var restoring = false
    private var relayPreconnectInFlight = false
    private var directPreconnectInFlight = false
    /// A push-notification tap that arrived before the launch restore resolved
    /// the route; consumed once home is up.
    private var pendingPushSession: String?
    private var chatStores: [String: ChatStore] = [:]
    /// Session ids in least→most recently used order, bounding the resident
    /// `chatStores` working set (`evictIdleStores`). Kept in sync with the map:
    /// `chatStore(for:)` promotes, eviction/reset removes.
    private var chatStoreLRU: [String] = []
    /// The resident `chatStores` cap. Beyond it, the least-recently-used idle
    /// (offscreen, not-pushed) stores are evicted so a long run that opens many
    /// conversations doesn't keep every store — and its buffer + timers —
    /// resident forever.
    static let maxResidentStores = 12
    private var transcriptHost: TranscriptHost?
    private var prewarmedDraftId: String?
    /// The Deck tab's engine + its kept-warm shell webview (the app's second
    /// webview; prewarmed once a binding reaches home, torn down with the binding).
    let deckStore = DeckStore()
    private var _deckHost: DeckHost?
    /// Sessions with an archive/hide request on the wire — the per-session
    /// serialization gate (`pumpSessionMutation`).
    private var sessionMutationsInFlight: Set<String> = []
    /// `-baybo-open-home` (DEBUG): no gateway is bound, so archive/delete
    /// mutations resolve locally instead of failing + rolling back — the
    /// headless UI tests assert on the optimistic flip staying put.
    private var demoHomeMode = false

    #if DEBUG
        /// The session `-baybo-open-chat` pushes. The demo frame feeders key on
        /// it so they can only ever write into this throwaway conversation — a
        /// demo turn pushed into a REAL session (`-baybo-open-session`) would land
        /// in that session's durable mirror and registry row.
        static let debugSessionId = "debug-session"
    #endif

    /// XCTest sets this in the host app's environment; the unit bundle is HOSTED
    /// (see project.yml), so `BayboApp` still constructs this store at launch.
    static let testEnvironmentKey = "XCTestConfigurationFilePath"

    /// DEBUG-only on purpose: this gate turns the whole app boot into a no-op,
    /// and the test bundle only ever builds Debug. A release binary must have no
    /// environment variable that disables it.
    static var runningUnderTest: Bool {
        #if DEBUG
            return ProcessInfo.processInfo.environment[testEnvironmentKey] != nil
        #else
            return false
        #endif
    }

    init() {
        AppStore.shared = self
        // Under test the store must not boot the world: constructing the client
        // spins the tokio runtime and reads the keychain, and `restoreOnLaunch`
        // dials the gateway and rewrites the simulator's real `sessions.json`
        // out from under the suites. The unit tests drive their own stores with
        // injected clients and temp support dirs.
        if Self.runningUnderTest { return }
        // The chat list's live unread/recency source: connection-global
        // `SessionActivity` pings land here for ANY session, subscribed or not.
        Baybo.client.setSessionListSink(sink: SessionActivityHandler())
        // Deck pushes are session-less by design; the connection-global sink
        // is the only way they reach a user parked on the Deck tab.
        Baybo.client.setDeckSink(sink: DeckEventsRelay(store: { AppStore.shared?.deckStore }))
        #if DEBUG
        // UI-verification hooks: land straight on interaction-gated screens so
        // they are screenshotable/log-verifiable headlessly on the simulator.
        // `-baybo-landing-direct` opens the direct-login form; `-baybo-open-chat`
        // opens the chat screen unbound (the dial fails offline — the transcript
        // webview + bridge still come up, which is what these runs verify); a
        // back-pop from it lands on the (empty) chat list.
        if ProcessInfo.processInfo.arguments.contains("-baybo-landing-direct") {
            landingView = .direct
            route = .landing
            return
        }
        if ProcessInfo.processInfo.arguments.contains("-baybo-open-chat") {
            route = .home
            chatPath = [.session(Self.debugSessionId)]
            return
        }
        // `-baybo-open-session <id>`: push a REAL (bound, registry-known) session
        // headlessly. The demo sessions above are local-only, so a list merge on a
        // bound sim drops their row — and a mirror dies with its row — so anything
        // that has to survive a relaunch (a restored transcript, the image sizes it
        // carries) can only be verified on an UNBOUND sim, or on a session the
        // gateway actually knows.
        let args = ProcessInfo.processInfo.arguments
        if let flag = args.firstIndex(of: "-baybo-open-session"), flag + 1 < args.count {
            route = .home
            chatPath = [.session(args[flag + 1])]
            restoreOnLaunch()
            return
        }
        if ProcessInfo.processInfo.arguments.contains("-baybo-demo-switch") {
            route = .home
            chatPath = [.session("demo-a")]
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(5))
                chatPath = [.session("demo-b")]
            }
            return
        }
        // `-baybo-open-home` lands on the tabbed home shell (chat list + bottom
        // menu bar) WITHOUT pushing a conversation, so the bar/header/sections
        // are screenshotable headlessly. Optional `-baybo-home-tab
        // <agents|projects|chats|settings>` preselects a section; a few demo rows
        // seed the list so content ghosts under the glass bar.
        if ProcessInfo.processInfo.arguments.contains("-baybo-open-home") {
            demoHomeMode = true
            let args = ProcessInfo.processInfo.arguments
            // `-baybo-demo-pin` records the pin reorder in isolation: the bottom
            // row (demo-1, oldest) springs to the top ~2s in. Start with nothing
            // pinned so it lands at the very top, not below demo-3.
            let demoPin = args.contains("-baybo-demo-pin")
            for i in 1...6 {
                SessionIndex.shared.recordUserSend(
                    sessionId: "demo-\(i)", text: "Demo conversation number \(i)")
            }
            // Normalize archive state so repeated headless runs start identical
            // (the container persists across suite runs, and a UI test
            // deliberately leaves rows archived): only demo-2 starts archived.
            for i in 1...6 {
                SessionIndex.shared.setArchivedFlag("demo-\(i)", archived: i == 2)
                SessionIndex.shared.setPinnedFlag("demo-\(i)", pinned: !demoPin && i == 3)
            }
            // Titles + a couple of unread badges so the Telegram-style row (bold
            // title over grey preview, time + count) is screenshotable; demo-6
            // stays untitled to exercise the single-line fallback. Past-dated
            // activity bumps unread WITHOUT reordering (`at` isn't newer than the
            // row's `lastActive`), so the pin-reorder demo's ordering is intact.
            let demoTitles = [
                "demo-1": "Ship the iOS chat list",
                "demo-2": "Weekend trip planning",
                "demo-3": "Refactor the sync loop",
                "demo-4": "Groceries and errands",
                "demo-5": "Design review notes",
            ]
            for (id, title) in demoTitles {
                SessionIndex.shared.applyTitle(sessionId: id, title: title)
            }
            // Reset unread first so repeated headless launches show a stable
            // count (it persists in sessions.json, so an unbalanced bump would
            // accumulate across runs), then seed exactly one on two rows.
            for i in 1...6 {
                SessionIndex.shared.clearUnread("demo-\(i)")
            }
            // Badge demo-1 / demo-5 (not the pinned demo-3) so the screenshot
            // shows a pinned-but-read row carrying only its tint, no glyph.
            for id in ["demo-1", "demo-5"] {
                SessionIndex.shared.noteActivity(sessionId: id, source: "assistant", atMillis: 0)
            }
            // Two fires of one scheduled job, so the CRON GROUP row (clock glyph,
            // newest fire's preview, summed unread) is screenshotable headlessly —
            // and so is the screen it pushes. They collapse into ONE row; that is
            // the whole feature (`docs/cron-groups.md`).
            for i in 1...2 {
                SessionIndex.shared.recordUserSend(
                    sessionId: "demo-cron-\(i)", text: "Overnight build is green; 3 PRs merged.")
                SessionIndex.shared.applyTitle(
                    sessionId: "demo-cron-\(i)", title: "Morning brief · 7/\(15 - i)")
                SessionIndex.shared.setCronGroupForDemo(
                    "demo-cron-\(i)", jobId: "demo-job", title: "Morning brief")
                SessionIndex.shared.clearUnread("demo-cron-\(i)")
                SessionIndex.shared.noteActivity(
                    sessionId: "demo-cron-\(i)", source: "assistant", atMillis: 0)
            }
            if demoPin {
                Task { @MainActor in
                    try? await Task.sleep(for: .seconds(2))
                    requestPin("demo-1", pinned: true)
                }
            }
            // `-baybo-demo-cron-pin`: pin the seeded cron GROUP ~2s in. The demo
            // fires are stamped at the epoch, so the group sits at the BOTTOM of
            // the list — the pin lifting it to the top is exactly the reorder the
            // feature exists for (a low-frequency job that would otherwise sink),
            // and this makes it screenshot-verifiable with no gateway.
            if args.contains("-baybo-demo-cron-pin") {
                Task { @MainActor in
                    try? await Task.sleep(for: .seconds(2))
                    requestCronPin(jobId: "demo-job", pinned: true)
                }
            }
            if let idx = args.firstIndex(of: "-baybo-home-tab"), idx + 1 < args.count {
                switch args[idx + 1] {
                case "deck": homeTab = .deck
                case "projects": homeTab = .projects
                case "settings": homeTab = .settings
                default: homeTab = .chats
                }
            }
            // `-baybo-demo-logout-confirm`: raise the logout confirm dialog on
            // arrival so the overlay is screenshotable headlessly.
            if args.contains("-baybo-demo-logout-confirm") {
                confirmLogout = true
            }
            route = .home
            return
        }
        #endif
        restoreOnLaunch()
    }

    /// Probe the durable binding and land on home (the chat list). No session
    /// is minted at launch — the compose button is the only session creator.
    private func restoreOnLaunch() {
        guard !restoring else { return }
        restoring = true
        Task {
            defer { restoring = false }
            let paired = Baybo.client.pairedDevice()
            let direct = (try? Baybo.client.directStatus()) ?? nil
            directBound = direct != nil
            guard paired != nil || direct != nil else {
                route = .landing
                return
            }
            if directBound {
                await registerPushBestEffort()
                preconnectDirectBestEffort()
            } else if paired != nil {
                preconnectRelayBestEffort()
            }
            route = .home
            consumePendingPushRoute()
            prewarmTranscriptHost()
            prewarmDeckHost()
            resumeStrandedSends()
        }
    }

    /// Sends that outlived the process. `ChatStore.send` enrols the outbox
    /// before it touches the network, so a kill in that window can leave a
    /// message for a session the gateway never heard of and the list never
    /// showed — unreachable from the UI and drained by nothing. Re-drive every
    /// session that still has one: the store ensures its remote row, lists it,
    /// and dials, and the dial's reconciliation gate looks each entry up before
    /// resending. Idempotent and cheap on the common path — logout wipes the
    /// outboxes, so an unbound install finds an empty directory.
    private func resumeStrandedSends() {
        for sessionId in OutboxStore.pendingSessionIds(in: SessionIndex.supportDirectory()) {
            chatStore(for: sessionId).resumePersistedSends()
        }
    }

    /// Foreground hook (scenePhase → .active): re-arm APNs registration while
    /// no token landed, refresh the direct push binding or warm the relay leg,
    /// and re-subscribe cached chat stores so catch-up does not wait for a
    /// screen to reappear.
    func didBecomeActive() {
        if !AppDelegate.hasToken {
            AppDelegate.registerForPush()
        }
        if directBound {
            Task { await registerPushBestEffort() }
            preconnectDirectBestEffort()
        } else if Baybo.client.pairedDevice() != nil {
            preconnectRelayBestEffort()
        }
        for store in chatStores.values {
            store.scheduleReconnect()
        }
        // Re-armed on every foreground, not just launch: an LRU eviction can
        // cancel a recovery's timers mid-flight, and a recovery that failed
        // offline deserves the next live leg.
        resumeStrandedSends()
        if route == .home {
            prewarmTranscriptHost()
            prewarmDeckHost()
        }
    }

    private func registerPushBestEffort() async {
        _ = try? await Baybo.client.registerPush()
    }

    private func preconnectRelayBestEffort() {
        guard !relayPreconnectInFlight else { return }
        relayPreconnectInFlight = true
        Task { @MainActor in
            defer { relayPreconnectInFlight = false }
            do {
                try await Baybo.client.relayPreconnect()
            } catch {
                NSLog("baybo: relay preconnect: %@", String(describing: error))
            }
        }
    }

    /// Warm the direct device leg so the list receives live `SessionActivity`
    /// while parked on it with no chat open (the direct analogue of relay's
    /// preconnect). Best-effort; the core coalesces a redundant dial.
    private func preconnectDirectBestEffort() {
        guard !directPreconnectInFlight else { return }
        directPreconnectInFlight = true
        Task { @MainActor in
            defer { directPreconnectInFlight = false }
            do {
                try await Baybo.client.directPreconnect()
            } catch {
                NSLog("baybo: direct preconnect: %@", String(describing: error))
            }
        }
    }

    // MARK: - Scan-to-pair

    func handleScan(payload: String) {
        scanPresented = false
        guard let target = parsePairQr(text: payload) else {
            status = Lang.shared.t("scan.notBaybo")
            return
        }
        Haptics.success()
        pairBegin(target: target)
    }

    private func pairBegin(target: PairTarget) {
        status = Lang.shared.t("pair.connecting")
        busy = true
        Task {
            defer { busy = false }
            do {
                let challenge = try await Baybo.client.pairBegin(
                    target: target,
                    onAbort: PairAbortHandler(store: self)
                )
                status = nil
                self.challenge = challenge
            } catch {
                status = Lang.shared.t("pair.failed", bayboErrorText(error))
            }
        }
    }

    func confirmPair(accepted: Bool) {
        guard let challenge else { return }
        status = Lang.shared.t(accepted ? "pair.confirming" : "pair.cancelling")
        busy = true
        Task {
            defer { busy = false }
            do {
                _ = try await Baybo.client.pairConfirm(
                    deviceId: challenge.deviceId, accepted: accepted)
                self.challenge = nil
                status = nil
                directBound = false
                await enterHomeFreshBinding()
            } catch {
                self.challenge = nil
                // The decline path errors by design ("pairing cancelled") —
                // render it as the neutral cancelled line, not a failure.
                status = accepted
                    ? Lang.shared.t("pair.failed", bayboErrorText(error))
                    : Lang.shared.t("pair.cancelled")
            }
        }
    }

    /// Gateway-side abort while the confirm screen is up: dismiss it.
    func pairAborted(reason: String) {
        challenge = nil
        status = Lang.shared.t("pair.cancelledReason", reason)
    }

    // MARK: - Direct login

    func directConnect(baseUrl: String, token: String) async -> String? {
        busy = true
        defer { busy = false }
        do {
            _ = try await Baybo.client.directLogin(baseUrl: baseUrl, token: token)
            directBound = true
            await registerPushBestEffort()
            await enterHomeFreshBinding()
            return nil
        } catch let error as BayboError {
            if case .InvalidToken = error {
                return Lang.shared.t("direct.invalidToken")
            }
            return Lang.shared.t("direct.failed", bayboErrorText(error))
        } catch {
            return Lang.shared.t("direct.failed", bayboErrorText(error))
        }
    }

    // MARK: - Chat navigation

    func chatStore(for sessionId: String) -> ChatStore {
        noteStoreUsed(sessionId)
        if let store = chatStores[sessionId] {
            return store
        }
        let store = ChatStore(sessionId: sessionId)
        chatStores[sessionId] = store
        return store
    }

    /// Promote a session to most-recently-used in the LRU ordering.
    private func noteStoreUsed(_ sessionId: String) {
        chatStoreLRU.removeAll { $0 == sessionId }
        chatStoreLRU.append(sessionId)
    }

    func transcriptHost(for sessionId: String) -> TranscriptHost {
        if let host = transcriptHost {
            return host
        }
        let host = TranscriptHost(store: chatStore(for: sessionId))
        transcriptHost = host
        return host
    }

    /// The deck shell webview, kept warm for the binding's lifetime and torn
    /// down in `resetChatStores`.
    func deckHost() -> DeckHost {
        if let host = _deckHost {
            return host
        }
        let host = DeckHost(store: deckStore)
        _deckHost = host
        return host
    }

    /// Open an existing session from the list.
    func openSession(_ sessionId: String) {
        Task {
            await activateSession(sessionId, ensureListed: true)
        }
    }

    /// Open a session from the archived screen: append, so the pop chain runs
    /// chat → archived → list.
    func openArchivedSession(_ sessionId: String) {
        Task {
            await activateSession(sessionId, ensureListed: true, appendToPath: true)
        }
    }

    /// The Chats header's ☰ menu entry: push the archived list. Guarded so a
    /// stray double-tap can't stack two copies.
    func openArchived() {
        guard !chatPath.contains(.archived) else { return }
        chatPath.append(.archived)
    }

    /// The ☰ menu's second entry: push the live scheduled jobs. Guarded like
    /// `openArchived`.
    func openCronJobs() {
        guard !chatPath.contains(.cronJobs) else { return }
        chatPath.append(.cronJobs)
    }

    /// Tapping a cron group row: push that job's fires. Guarded like `openArchived`.
    func openCronGroup(_ jobId: String) {
        guard !chatPath.contains(.cronGroup(jobId)) else { return }
        chatPath.append(.cronGroup(jobId))
    }

    /// Open a fire from inside its group: a normal push over the fire list, so it
    /// slides in like any conversation and backs out the archived-screen way —
    /// chat → fire list → main list.
    ///
    /// An earlier version dropped the fire list from the back stack once the push
    /// settled, so Back would skip straight to the main list (a fire is an
    /// ordinary conversation the group merely *collects*). But `NavigationStack`
    /// keys its path POSITIONALLY, so collapsing `[.cronGroup, .session]` →
    /// `[.session]` doesn't "remove the middle" — it rebuilds the `.session`
    /// destination at its new depth (0), tearing down and remounting the whole
    /// `ChatScreen`. That surfaced first as a pop animation, then — once the pop
    /// was suppressed — as a one-frame page flash the slide had been masking. No
    /// path trim can avoid it, so the drill-in stays a plain push.
    func openCronGroupSession(_ sessionId: String) {
        Task {
            await activateSession(sessionId, ensureListed: true, appendToPath: true)
        }
    }

    /// Fetch the owner's live scheduled jobs for `CronJobsScreen`.
    ///
    /// Deliberately a plain async read that throws, not an optimistic mutation
    /// through `SessionIndex`: there is nothing local to reconcile. The screen is
    /// the only reader, it owns the result, and it renders the failure itself —
    /// so this neither caches nor writes a `sessionNotice` (which belongs to the
    /// chat list's own mutations and would surface a cron error on a screen the
    /// user already left).
    @discardableResult
    func loadCronJobs() async throws -> [CronJobSummary] {
        #if DEBUG
            if demoHomeMode {
                let demo = Self.demoCronJobs()
                rememberCronJobTitles(demo)
                cronJobs = demo
                return demo
            }
        #endif
        let jobs = try await Baybo.client.chatListCronJobs()
        rememberCronJobTitles(jobs)
        cronJobs = jobs
        return jobs
    }

    /// Pause or resume a job: flip the row now, send, and take the list back from
    /// the gateway on success.
    ///
    /// That refetch is not belt-and-braces. Resuming recomputes the next trigger
    /// **server-side** (from now — a job paused over a weekend must not fire five
    /// times on Monday), and only the scheduler can say when that is. The
    /// optimistic row therefore carries no next trigger at all until the refetch
    /// lands, rather than a stale one it has no way to recompute.
    func requestCronPaused(_ jobId: String, paused: Bool) {
        cronJobNotice = nil
        let rollback = cronJobs
        applyCronPause(jobId, paused: paused)
        #if DEBUG
            if demoHomeMode { return }
        #endif
        Task { @MainActor in
            do {
                try await Baybo.client.chatSetCronPaused(jobId: jobId, paused: paused)
                // Already applied; a failure to REFETCH leaves the optimistic row
                // standing (it is right about the status) and the next appear
                // reconciles the trigger.
                try? await loadCronJobs()
            } catch {
                cronJobs = rollback
                cronJobNotice = Lang.shared.t(
                    paused ? "cronJobs.pauseFailed" : "cronJobs.resumeFailed")
            }
        }
    }

    /// Raise the delete confirm for a swiped job.
    func promptDeleteCronJob(_ jobId: String, name: String) {
        withAnimation(ConfirmDialog.enterMotion) {
            confirmDeleteCronJob = PendingCronJobDelete(jobId: jobId, name: name)
        }
    }

    /// Delete a scheduled job, after the confirm: drop the row and send.
    ///
    /// The job stops firing and goes to the gateway's recycle bin. Its execution
    /// records are untouched — they are ordinary conversations, and the chat list
    /// keeps showing them under the title each fire snapshotted. (The mirror
    /// gesture, deleting the GROUP, clears those records and leaves the job
    /// running: `requestDeleteGroup`.)
    func requestDeleteCronJob(_ jobId: String) {
        cronJobNotice = nil
        let rollback = cronJobs
        cronJobs?.removeAll { $0.id == jobId }
        #if DEBUG
            if demoHomeMode { return }
        #endif
        Task { @MainActor in
            do {
                try await Baybo.client.chatDeleteCronJob(jobId: jobId)
            } catch {
                cronJobs = rollback
                cronJobNotice = Lang.shared.t("cronJobs.deleteFailed")
            }
        }
    }

    /// Both directions drop the next trigger: pausing because the gateway does,
    /// and resuming because the new one is the scheduler's to compute.
    ///
    /// So a paused row reads "Paused" at once, while a resumed one shows an empty
    /// meta column for the round trip it takes the refetch to land. That blank is
    /// deliberate: the honest answer to "when does this next run" is not yet
    /// known here, and the alternatives are inventing a time or showing the stale
    /// one the job had before it was ever paused.
    private func applyCronPause(_ jobId: String, paused: Bool) {
        guard let index = cronJobs?.firstIndex(where: { $0.id == jobId }) else { return }
        cronJobs?[index] = cronJobs![index].with(
            status: paused ? .disabled : .enabled, nextTriggerAt: nil)
    }

    /// A job's name, kept so `CronGroupScreen` can title itself for a job whose
    /// fires cannot: a group is a view over its fires, so a job that has never
    /// fired has no members to read a title from, and the screen would otherwise
    /// head a specific job's page with the generic "Scheduled task".
    ///
    /// A cache with no staleness problem, because it cannot be consulted while
    /// wrong: a fire-less group is reachable ONLY from the jobs list — the chat
    /// list's group row exists only where fires do — so the one path that needs
    /// this always fills it moments earlier. Once fires exist they carry the
    /// live title themselves and win.
    private func rememberCronJobTitles(_ jobs: [CronJobSummary]) {
        for job in jobs where !job.title.isEmpty {
            cronJobTitles[job.id] = job.title
        }
    }

    #if DEBUG
        /// `-baybo-open-home`'s cron job fixture: a live job, a paused one, and a
        /// titleless one (the prompt fallback) — so the screen is screenshot- and
        /// XCUITest-verifiable with no gateway.
        ///
        /// All recurring, because the list is: a one-shot is dropped in
        /// `list_cron_jobs` and could never reach a real screen, so seeding one
        /// would be a fixture for a state production cannot produce.
        ///
        /// The live one is keyed to the seeded cron GROUP (`demo-job`), so tapping
        /// it opens that job's two demo fires: the list → job → its history path
        /// is the whole point of the tap, and this is what makes it drivable.
        private static func demoCronJobs() -> [CronJobSummary] {
            let iso = ISO8601DateFormatter()
            let now = Date()
            return [
                CronJobSummary(
                    id: "demo-job",
                    title: "Morning brief",
                    prompt: "Summarize overnight CI and the open PRs",
                    expr: "0 9 * * *",
                    timezone: "Asia/Shanghai",
                    status: .enabled,
                    nextTriggerAt: iso.string(from: now.addingTimeInterval(3 * 3600)),
                    lastTriggeredAt: iso.string(from: now.addingTimeInterval(-21 * 3600))),
                CronJobSummary(
                    id: "demo-job-paused",
                    title: "Weekly digest",
                    prompt: "Round up the week",
                    expr: "0 18 * * FRI",
                    timezone: "UTC",
                    status: .disabled,
                    nextTriggerAt: nil,
                    lastTriggeredAt: nil),
                CronJobSummary(
                    id: "demo-job-nameless",
                    title: "",
                    prompt: "Remind me to renew the TLS certificate",
                    expr: "*/30 * * * *",
                    timezone: "UTC",
                    status: .enabled,
                    nextTriggerAt: iso.string(from: now.addingTimeInterval(12 * 60)),
                    lastTriggeredAt: nil),
            ]
        }
    #endif

    /// Clear a cron group's badge in ONE round-trip: the gateway resolves each
    /// session's own tail ordinal, which the list does not have.
    ///
    /// Without this the feature is cosmetic — a `*/30` job becomes one row
    /// wearing a `48` badge that the user has no way to clear. `sessionIds` are
    /// the group's **visible** members; a fire that escaped (pinned, archived)
    /// clears itself where it lives.
    func requestMarkGroupRead(_ sessionIds: [String]) {
        guard !sessionIds.isEmpty else { return }
        SessionIndex.shared.clearUnread(sessionIds)
        Task {
            do {
                try await Baybo.client.chatMarkManyRead(sessionIds: sessionIds)
            } catch {
                // The optimistic clear stands: the next list merge reconciles the
                // badge to server truth either way, so a failure costs one stale
                // refresh, not a wrong number.
                NSLog("baybo: mark group read: %@", bayboErrorText(error))
            }
        }
    }

    /// Compose: mint or reuse a local draft id. The durable gateway row is
    /// created on first send, so abandoned drafts do not pollute the session list.
    func startNewChat() async -> String? {
        let sessionId = prewarmedDraftId ?? newChatSessionId()
        prewarmedDraftId = nil
        await activateSession(sessionId, ensureListed: false)
        return nil
    }

    private func prewarmTranscriptHost() {
        guard route == .home, transcriptHost == nil else { return }
        let sessionId = newChatSessionId()
        prewarmedDraftId = sessionId
        _ = transcriptHost(for: sessionId)
    }

    private func prewarmDeckHost() {
        guard route == .home, _deckHost == nil else { return }
        _ = deckHost()
    }

    /// A push-notification tap targeting `sessionId`: route straight into that
    /// conversation. Before the launch restore resolves the route, stash it —
    /// `restoreOnLaunch` consumes the stash once home is up.
    func routeToSession(_ sessionId: String) {
        guard route == .home else {
            pendingPushSession = sessionId
            return
        }
        Task {
            await activateSession(sessionId, ensureListed: true)
        }
    }

    private func consumePendingPushRoute() {
        if let sessionId = pendingPushSession {
            pendingPushSession = nil
            routeToSession(sessionId)
        }
    }

    /// Select the foreground session. The FFI transport keeps one global chat
    /// leg per binding; each opened session subscribes on that leg and keeps its
    /// sink registered for offscreen buffering. Push-tap routing deliberately
    /// keeps the reset path even for an archived session — backing out lands on
    /// the main list, and the row stays reachable via the ☰ menu.
    private func activateSession(
        _ sessionId: String, ensureListed: Bool, appendToPath: Bool = false
    ) async {
        if ensureListed {
            SessionIndex.shared.touch(sessionId: sessionId)
        }
        _ = transcriptHost(for: sessionId)
        // A conversation always belongs to the Chats section — backing out of it
        // must land on the list, whatever tab launched the compose/push.
        homeTab = .chats
        if appendToPath {
            if chatPath.last != .session(sessionId) {
                chatPath.append(.session(sessionId))
            }
        } else {
            chatPath = [.session(sessionId)]
        }
        await evictIdleStores()
    }

    // MARK: - Archive / delete (optimistic, serialized per session)

    /// Raise the delete confirm (`ConfirmDialog` in `RootView`) for a swiped row.
    func promptDeleteSession(_ sessionId: String) {
        withAnimation(ConfirmDialog.enterMotion) {
            confirmDeleteSession = sessionId
        }
    }

    /// Raise the delete confirm for a swiped **cron group**, snapshotting the
    /// members it will delete (see `PendingCronGroupDelete`). A group with no
    /// visible member cannot be swiped — it has no row — so the guard is only a
    /// belt against a racing merge emptying it mid-gesture.
    func promptDeleteCronGroup(jobId: String, memberIds: [String]) {
        guard !memberIds.isEmpty else { return }
        withAnimation(ConfirmDialog.enterMotion) {
            confirmDeleteCronGroup = PendingCronGroupDelete(jobId: jobId, memberIds: memberIds)
        }
    }

    /// Archive or unarchive, optimistically: the row moves lists at once and
    /// the PUT follows. Also the undo path (the toast's 撤销 re-sends `false`).
    func requestArchive(_ sessionId: String, archived: Bool) {
        sessionNotice = nil
        SessionIndex.shared.beginArchive(sessionId, archived: archived)
        pumpSessionMutation(sessionId)
    }

    /// Pin or unpin, optimistically: the row re-sorts to the pinned block at
    /// once and the PUT follows. The re-sort is not animated — see the list's
    /// `swipeDismissal` for why a glide was never reachable from the swipe.
    func requestPin(_ sessionId: String, pinned: Bool) {
        sessionNotice = nil
        SessionIndex.shared.beginPin(sessionId, pinned: pinned)
        pumpSessionMutation(sessionId)
    }

    /// Pin/unpin a **cron group** — every fire of one scheduled job, collapsed
    /// into a single list row. Keyed by the JOB: the group is a view over its
    /// fires, so the job is the only object that can hold the bit
    /// (`PUT /v1/cron/{id}/pin`, `chat_set_cron_pinned` over the active leg).
    ///
    /// Deliberately simpler than `pumpSessionMutation`: there is no
    /// archive/delete/pin interleaving to serialize on a job — pin is the ONLY
    /// mutation a cron job has — so one flip, one send, roll back on failure.
    func requestCronPin(jobId: String, pinned: Bool) {
        sessionNotice = nil
        SessionIndex.shared.beginCronPin(jobId: jobId, pinned: pinned)
        #if DEBUG
            // `-baybo-open-home` has no gateway to call; the demo asserts on the
            // optimistic flip staying put (mirrors the archive/delete demo mode).
            if demoHomeMode {
                SessionIndex.shared.finishCronPin(jobId: jobId)
                return
            }
        #endif
        Task {
            do {
                try await Baybo.client.chatSetCronPinned(jobId: jobId, pinned: pinned)
                SessionIndex.shared.finishCronPin(jobId: jobId)
            } catch {
                SessionIndex.shared.rollBackCronPin(jobId: jobId)
                sessionNotice = Lang.shared.t("list.pinFailed")
            }
        }
    }

    /// Delete (server-side soft-hide), after the confirm dialog: drop the row +
    /// mirror optimistically, evict any resident store so a dangling
    /// subscription doesn't keep buffering frames, and send the DELETE.
    func requestDelete(_ sessionId: String) {
        sessionNotice = nil
        if let store = chatStores[sessionId], isEvictable(sessionId, store) {
            Task { await evictStore(sessionId, store) }
        }
        SessionIndex.shared.beginHide(sessionId)
        pumpSessionMutation(sessionId)
    }

    /// Delete a **cron group**, after the confirm dialog: soft-hide every member
    /// the dialog counted, in one round-trip. That is the entire delete — a group
    /// is a view over its fires (`docs/cron-groups.md`), so clearing the
    /// execution records is the only object-less thing "delete the folder" can
    /// mean. The cron JOB is untouched and keeps firing; the group returns with
    /// its next fire, empty of the history just cleared.
    ///
    /// Deliberately outside `pumpSessionMutation`: that serializes ONE request
    /// per session, and this is one request for N. Nothing can re-pump the staged
    /// intents behind its back — every pump site is a swipe on a row, and these
    /// rows are gone the moment the batch is staged.
    func requestDeleteGroup(_ memberIds: [String]) {
        guard !memberIds.isEmpty else { return }
        sessionNotice = nil
        for sessionId in memberIds {
            guard let store = chatStores[sessionId], isEvictable(sessionId, store) else { continue }
            Task { await evictStore(sessionId, store) }
        }
        SessionIndex.shared.beginHideMany(memberIds)
        #if DEBUG
            if demoHomeMode {
                SessionIndex.shared.finishHideMany(memberIds)
                return
            }
        #endif
        Task { @MainActor in
            do {
                try await Baybo.client.chatHideMany(sessionIds: memberIds)
                SessionIndex.shared.finishHideMany(memberIds)
            } catch {
                SessionIndex.shared.rollBackHideMany(memberIds)
                sessionNotice = Lang.shared.t("list.deleteFailed")
            }
        }
    }

    /// One in-flight request per session; `SessionIndex.pendingMutation` holds
    /// the latest desired state. On a stale ack (the user flipped again while
    /// the request flew — archive→undo inside the 3s toast window is the common
    /// case) the newer intent is sent instead of resolving; on failure the
    /// still-current intent rolls back with a notice, a superseded one just
    /// yields to the newer send. Every user action gets at most one send — no
    /// retry loops.
    private func pumpSessionMutation(_ sessionId: String) {
        guard !sessionMutationsInFlight.contains(sessionId),
            let desired = SessionIndex.shared.pendingMutation(for: sessionId)
        else { return }
        #if DEBUG
            if demoHomeMode {
                SessionIndex.shared.finishMutation(sessionId)
                return
            }
        #endif
        sessionMutationsInFlight.insert(sessionId)
        Task { @MainActor in
            do {
                switch desired {
                case .archived(let archived):
                    try await Baybo.client.chatSetArchived(
                        sessionId: sessionId, archived: archived)
                case .pinned(let pinned):
                    try await Baybo.client.chatSetPinned(
                        sessionId: sessionId, pinned: pinned)
                case .hidden:
                    try await Baybo.client.chatHideSession(sessionId: sessionId)
                }
                sessionMutationsInFlight.remove(sessionId)
                if SessionIndex.shared.pendingMutation(for: sessionId) == desired {
                    SessionIndex.shared.finishMutation(sessionId)
                } else {
                    pumpSessionMutation(sessionId)
                }
            } catch {
                sessionMutationsInFlight.remove(sessionId)
                NSLog("baybo: session mutation: %@", bayboErrorText(error))
                guard SessionIndex.shared.pendingMutation(for: sessionId) == desired else {
                    pumpSessionMutation(sessionId)
                    return
                }
                switch desired {
                case .archived:
                    SessionIndex.shared.rollBackArchive(sessionId)
                    sessionNotice = Lang.shared.t("list.archiveFailed")
                case .pinned:
                    SessionIndex.shared.rollBackPin(sessionId)
                    sessionNotice = Lang.shared.t("list.pinFailed")
                case .hidden:
                    SessionIndex.shared.rollBackHide(sessionId)
                    sessionNotice = Lang.shared.t("list.deleteFailed")
                }
            }
        }
    }

    /// Bound the resident `chatStores` working set: evict the least-recently-used
    /// stores beyond `maxResidentStores` that are safe to drop — offscreen (no
    /// attached bridge) and not the pushed session. Runs after each activation,
    /// where the set has just grown.
    private func evictIdleStores() async {
        guard chatStores.count > Self.maxResidentStores else { return }
        var overflow = chatStores.count - Self.maxResidentStores
        // Least→most recently used: evict from the front (Array iterates a value
        // snapshot, so removing inside the loop is safe).
        for sessionId in chatStoreLRU where overflow > 0 {
            guard let store = chatStores[sessionId], isEvictable(sessionId, store) else { continue }
            await evictStore(sessionId, store)
            overflow -= 1
        }
    }

    /// Drop every idle store regardless of the working-set cap — the
    /// memory-warning response. Keeps only the pushed session.
    func evictAllIdleStores() async {
        for sessionId in chatStoreLRU {
            guard let store = chatStores[sessionId], isEvictable(sessionId, store) else { continue }
            await evictStore(sessionId, store)
        }
    }

    private func isEvictable(_ sessionId: String, _: ChatStore) -> Bool {
        chatPath.last != .session(sessionId)
    }

    private func evictStore(_ sessionId: String, _ store: ChatStore) async {
        chatStores.removeValue(forKey: sessionId)
        chatStoreLRU.removeAll { $0 == sessionId }
        await store.evict()
    }

    private func resetChatStores() async {
        let stores = Array(chatStores.values)
        chatStores.removeAll()
        chatStoreLRU.removeAll()
        // A track from the departing binding must not keep playing (or hold
        // the Now Playing entry) into the landing screen / next binding.
        AudioPlayerCenter.shared.stop()
        transcriptHost?.teardown()
        transcriptHost = nil
        prewarmedDraftId = nil
        // The deck belongs to the departing gateway too.
        _deckHost?.teardown()
        _deckHost = nil
        DeckStore.removeMirror()
        for store in stores {
            await store.disconnect()
        }
    }

    /// A fresh binding must not inherit the previous gateway's sessions: wipe
    /// the local registry + transcript mirrors, then land on the (empty) list.
    /// On direct the REST refresh repopulates it immediately.
    private func enterHomeFreshBinding() async {
        await resetChatStores()
        SessionIndex.shared.removeAll()
        chatPath = []
        route = .home
        if !directBound {
            preconnectRelayBestEffort()
        }
        prewarmDeckHost()
    }

    /// Log out: tear down the live leg, wipe both credential sets, drop the
    /// local session registry + transcripts, and return to landing.
    func logout() async {
        guard !busy else { return }
        busy = true
        chatPath = []
        directBound = false
        landingView = .menu
        status = nil
        route = .landing
        defer { busy = false }
        await resetChatStores()
        do {
            try await Baybo.client.logout()
        } catch {
            // Teardown is best-effort by design; surface nothing fatal.
            NSLog("baybo: logout: %@", bayboErrorText(error))
        }
        SessionIndex.shared.removeAll()
    }
}

/// Bridges the core's pairing-abort callback (arbitrary thread) onto the store.
/// `@unchecked Sendable`: the only mutable state is the auto-nilling weak ref.
private final class PairAbortHandler: PairAbortListener, @unchecked Sendable {
    private weak var store: AppStore?

    init(store: AppStore) {
        self.store = store
    }

    func onAbort(reason: String) {
        Task { @MainActor [store] in
            store?.pairAborted(reason: reason)
        }
    }
}

/// The user-facing text of a thrown core error: the stable variants map to
/// localized strings; prose rides through verbatim. Main-actor because it
/// resolves through the live language override.
@MainActor
func bayboErrorText(_ error: Error) -> String {
    switch error {
    case BayboError.InvalidToken:
        return Lang.shared.t("direct.invalidToken")
    case let BayboError.Other(message):
        return message
    case BayboError.NotBound:
        return Lang.shared.t("landing.subtitle")
    default:
        return error.localizedDescription
    }
}
