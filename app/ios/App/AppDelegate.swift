import UIKit
import UserNotifications

/// Real APNs delegate callbacks — this whole file replaces the Tauri shell's
/// `push_register.rs` (which had to inject these methods into wry's delegate at
/// runtime via `class_addMethod`).
final class AppDelegate: NSObject, UIApplicationDelegate {
    /// Whether iOS has delivered a device token this launch; foreground re-arms
    /// registration while it hasn't (APNs registration can fail transiently at
    /// launch and iOS never retries on its own).
    private(set) static var hasToken = false

    #if DEBUG
    /// Wipe the device-local stores (`Application Support/baybo`: the session
    /// registry, the transcript mirrors, the outboxes) before anything reads
    /// them, so a `-baybo-demo-*` launch is IDEMPOTENT.
    ///
    /// Without it the demo fixtures are not hermetic: their session ids are
    /// fixed, so every launch APPENDS its canned turn to the same persisted
    /// mirror. A UI suite shares one simulator across its cases, so by the
    /// fourth launch the attachment demo renders four video tiles and a
    /// by-label query that is unambiguous on a fresh install matches six
    /// elements and dies. That is a test-harness bug, not a product one — it
    /// just fails as if the product broke.
    static let resetStoreArg = "-baybo-reset-store"
    #endif

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        #if DEBUG
        // Earliest hook that still precedes `AppStore()` (a `@StateObject`,
        // built on the first body evaluation) and therefore every reader of the
        // support directory.
        if ProcessInfo.processInfo.arguments.contains(Self.resetStoreArg) {
            try? FileManager.default.removeItem(at: SessionIndex.supportDirectory())
        }
        #endif
        // Tap routing (didReceive below); foreground presentation stays silent.
        UNUserNotificationCenter.current().delegate = self
        Self.registerForPush()
        return true
    }

    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        // Lowercase hex — the token format the core, pairing protocol, and
        // gateway all expect.
        let hex = deviceToken.map { String(format: "%02x", $0) }.joined()
        Self.hasToken = true
        Baybo.client.setApnsToken(tokenHex: hex)
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        NSLog("baybo: APNs registration failed: %@", error.localizedDescription)
    }

    /// Under memory pressure, drop every idle (offscreen, not-pushed) chat store
    /// ahead of the normal LRU cap, freeing their buffers + gateway sinks.
    func applicationDidReceiveMemoryWarning(_ application: UIApplication) {
        Task { @MainActor in
            await AppStore.shared?.evictAllIdleStores()
        }
    }

    /// Provisional auth (granted silently, so the mutable-content push lands in
    /// Notification Center / lock screen) + remote-notification registration.
    /// Idempotent; re-run on foreground while no token is held.
    static func registerForPush() {
        UNUserNotificationCenter.current().requestAuthorization(
            options: [.provisional, .alert, .sound]
        ) { _, error in
            if let error {
                NSLog("baybo: notification authorization failed: %@", error.localizedDescription)
            }
            DispatchQueue.main.async {
                UIApplication.shared.registerForRemoteNotifications()
            }
        }
    }
}

extension AppDelegate: UNUserNotificationCenterDelegate {
    /// Foreground pushes present nothing (the pre-delegate behavior, kept on
    /// purpose): the chat list refreshes on foreground and the live session
    /// streams its own frames, so a banner would only duplicate them.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        []
    }

    /// A notification tap: route into the conversation the NSE decrypted the
    /// session id for. No id (legacy sender, failed decrypt, non-default
    /// action) → do nothing; the app lands on the chat list as usual.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        guard response.actionIdentifier == UNNotificationDefaultActionIdentifier,
            let sessionId = response.notification.request.content
                .userInfo[PushPayloadKeys.sessionId] as? String
        else { return }
        await MainActor.run {
            AppStore.shared?.routeToSession(sessionId)
        }
    }
}
