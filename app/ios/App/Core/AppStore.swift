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

    /// The home shell's bottom-menu sections. Only `chats` and `settings` have
    /// real screens today; `agents`/`projects` are placeholders. Compose and
    /// push-tap routing force `chats` so backing out of a conversation lands on
    /// the list, not whatever section was showing.
    enum HomeTab: CaseIterable {
        case agents
        case projects
        case chats
        case settings
    }

    @Published var route: Route = .launching
    @Published var landingView: LandingView = .menu
    /// The selected home section (the bottom menu bar's current tab).
    @Published var homeTab: HomeTab = .chats
    /// The NavigationStack path over the chat list: at most one pushed session.
    @Published var chatPath: [String] = []
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

    init() {
        AppStore.shared = self
        // The chat list's live unread/recency source: connection-global
        // `SessionActivity` pings land here for ANY session, subscribed or not.
        Baybo.client.setSessionListSink(sink: SessionActivityHandler())
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
            chatPath = ["debug-session"]
            return
        }
        // `-baybo-open-home` lands on the tabbed home shell (chat list + bottom
        // menu bar) WITHOUT pushing a conversation, so the bar/header/sections
        // are screenshotable headlessly. Optional `-baybo-home-tab
        // <agents|projects|chats|settings>` preselects a section; a few demo rows
        // seed the list so content ghosts under the glass bar.
        if ProcessInfo.processInfo.arguments.contains("-baybo-open-home") {
            for i in 1...6 {
                SessionIndex.shared.recordUserSend(
                    sessionId: "demo-\(i)", text: "Demo conversation number \(i)")
            }
            let args = ProcessInfo.processInfo.arguments
            if let idx = args.firstIndex(of: "-baybo-home-tab"), idx + 1 < args.count {
                switch args[idx + 1] {
                case "agents": homeTab = .agents
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

    /// Open an existing session from the list.
    func openSession(_ sessionId: String) {
        Task {
            await activateSession(sessionId, ensureListed: true)
        }
    }

    /// Compose: mint a local draft id and enter it. The durable gateway row is
    /// created on first send, so abandoned drafts do not pollute the session list.
    func startNewChat() async -> String? {
        let sessionId = newChatSessionId()
        await activateSession(sessionId, ensureListed: false)
        return nil
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
    /// sink registered for offscreen buffering.
    private func activateSession(_ sessionId: String, ensureListed: Bool) async {
        if ensureListed {
            SessionIndex.shared.touch(sessionId: sessionId)
        }
        _ = chatStore(for: sessionId)
        // A conversation always belongs to the Chats section — backing out of it
        // must land on the list, whatever tab launched the compose/push.
        homeTab = .chats
        chatPath = [sessionId]
        await evictIdleStores()
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
    /// memory-warning response. Keeps only the pushed session and any store
    /// still rendering on screen (an attached bridge).
    func evictAllIdleStores() async {
        for sessionId in chatStoreLRU {
            guard let store = chatStores[sessionId], isEvictable(sessionId, store) else { continue }
            await evictStore(sessionId, store)
        }
    }

    /// A store is safe to evict only when it is neither the pushed conversation
    /// nor rendering on screen.
    private func isEvictable(_ sessionId: String, _ store: ChatStore) -> Bool {
        sessionId != chatPath.last && !store.hasBridge
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
