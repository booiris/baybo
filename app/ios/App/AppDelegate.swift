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

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
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
