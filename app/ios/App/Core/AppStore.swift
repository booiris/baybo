import SwiftUI

/// The root state machine: unbound (landing / direct login / scan / pair
/// confirm) vs bound (chat). Chat is home — a bound app auto-opens its chat on
/// launch, mirroring the Tauri app's launch-restore.
@MainActor
final class AppStore: ObservableObject {
    enum Route: Equatable {
        case launching
        case landing
        case chat(sessionId: String)
    }

    enum LandingView {
        case menu
        case direct
    }

    @Published var route: Route = .launching
    @Published var landingView: LandingView = .menu
    /// Landing / pairing status line (localized, already resolved).
    @Published var status: String?
    @Published var busy = false
    /// Non-nil while the pair-confirm screen is up.
    @Published var challenge: PairChallenge?
    @Published var scanPresented = false
    /// Whether the active binding is direct (drives push re-registration).
    @Published private(set) var directBound = false

    private var restoring = false

    init() {
        restoreOnLaunch()
    }

    /// Probe the durable binding and reopen the chat (restored session id when
    /// one is persisted, else a fresh session).
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
            }
            await openChat()
        }
    }

    /// Foreground hook (scenePhase → .active): re-arm APNs registration while
    /// no token landed, and refresh the direct push binding (a token can arrive
    /// seconds after launch; the binding needs re-asserting after relaunch).
    func didBecomeActive() {
        if !AppDelegate.hasToken {
            AppDelegate.registerForPush()
        }
        if directBound {
            Task { await registerPushBestEffort() }
        }
    }

    private func registerPushBestEffort() async {
        _ = try? await Baybo.client.registerPush()
    }

    // MARK: - Scan-to-pair

    func handleScan(payload: String) {
        scanPresented = false
        guard let target = parsePairQr(text: payload) else {
            status = String(localized: "scan.notBaybo")
            return
        }
        Haptics.success()
        pairBegin(target: target)
    }

    private func pairBegin(target: PairTarget) {
        status = String(localized: "pair.connecting")
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
                status = String(
                    format: String(localized: "pair.failed"), bayboErrorText(error))
            }
        }
    }

    func confirmPair(accepted: Bool) {
        guard let challenge else { return }
        status = String(localized: accepted ? "pair.confirming" : "pair.cancelling")
        busy = true
        Task {
            defer { busy = false }
            do {
                _ = try await Baybo.client.pairConfirm(
                    deviceId: challenge.deviceId, accepted: accepted)
                self.challenge = nil
                status = nil
                directBound = false
                await openChat(freshSession: true)
            } catch {
                self.challenge = nil
                // The decline path errors by design ("pairing cancelled") —
                // render it as the neutral cancelled line, not a failure.
                status = accepted
                    ? String(format: String(localized: "pair.failed"), bayboErrorText(error))
                    : String(localized: "pair.cancelled")
            }
        }
    }

    /// Gateway-side abort while the confirm screen is up: dismiss it.
    func pairAborted(reason: String) {
        challenge = nil
        status = String(format: String(localized: "pair.cancelledReason"), reason)
    }

    // MARK: - Direct login

    func directConnect(baseUrl: String, token: String) async -> String? {
        busy = true
        defer { busy = false }
        do {
            _ = try await Baybo.client.directLogin(baseUrl: baseUrl, token: token)
            directBound = true
            await registerPushBestEffort()
            await openChat(freshSession: true)
            return nil
        } catch let error as BayboError {
            if case .InvalidToken = error {
                return String(localized: "direct.invalidToken")
            }
            return String(format: String(localized: "direct.failed"), bayboErrorText(error))
        } catch {
            return String(format: String(localized: "direct.failed"), bayboErrorText(error))
        }
    }

    // MARK: - Chat lifecycle

    /// Enter the chat: reuse the persisted session id, else mint one on the
    /// active binding's leg. `freshSession` forces a new session (a new binding
    /// must not inherit the previous gateway's session pointer).
    func openChat(freshSession: Bool = false) async {
        if freshSession {
            clearPersistedChat()
        }
        if !freshSession,
            let existing = UserDefaults.standard.string(forKey: ChatDefaults.sessionId)
        {
            route = .chat(sessionId: existing)
            return
        }
        do {
            let sessionId = try await Baybo.client.chatCreateSession()
            UserDefaults.standard.set(sessionId, forKey: ChatDefaults.sessionId)
            route = .chat(sessionId: sessionId)
        } catch {
            route = .landing
            status = String(format: String(localized: "chat.startFailed"), bayboErrorText(error))
        }
    }

    /// Log out: tear down the live leg, wipe both credential sets, drop the
    /// persisted chat pointers, and return to landing.
    func logout() async {
        do {
            try await Baybo.client.logout()
        } catch {
            // Teardown is best-effort by design; surface nothing fatal.
            NSLog("baybo: logout: %@", bayboErrorText(error))
        }
        clearPersistedChat()
        directBound = false
        landingView = .menu
        status = nil
        route = .landing
    }

    private func clearPersistedChat() {
        let defaults = UserDefaults.standard
        defaults.removeObject(forKey: ChatDefaults.sessionId)
        defaults.removeObject(forKey: ChatDefaults.transcriptState)
        defaults.removeObject(forKey: ChatDefaults.lastOrdinal)
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
/// localized strings; prose rides through verbatim.
func bayboErrorText(_ error: Error) -> String {
    switch error {
    case BayboError.InvalidToken:
        return String(localized: "direct.invalidToken")
    case let BayboError.Other(message):
        return message
    case BayboError.NotBound:
        return String(localized: "landing.subtitle")
    default:
        return error.localizedDescription
    }
}
