import UIKit
import UserNotifications

/// The app delegate: APNs registration and device-token delivery, memory-
/// pressure eviction, the notification-center callbacks for foreground
/// presentation and taps — and the tail of the responder chain, which is where
/// an image paste the composer field declined gets caught (see `paste(_:)`).
///
/// `UIResponder`, not `NSObject`, for that last reason alone: as an `NSObject`
/// the delegate is not in the chain at all, and there is no other ancestor to
/// hang the hook on — SwiftUI owns every view between the field and the window.
final class AppDelegate: UIResponder, UIApplicationDelegate {
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
        NSLog(
            "baybo: APNs device token received — env=%@ token_len=%ld",
            Baybo.apnsEnvironmentName,
            hex.count)
        Baybo.client.setApnsToken(tokenHex: hex)
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: Error
    ) {
        NSLog("baybo: APNs registration failed: %@", error.localizedDescription)
    }

    /// The composer field's Paste, for a clipboard it cannot use.
    ///
    /// UIKit asks the FIRST RESPONDER whether it can paste and, only if it says
    /// no, walks up the chain looking for someone who can. A SwiftUI `TextField`
    /// says no exactly when the clipboard holds no text — i.e. for the copied
    /// screenshot this exists to catch — and the walk then ends here.
    ///
    /// **Never delegate `paste:` to `super`.** `UIResponder`'s default answers
    /// YES for any action the class merely IMPLEMENTS, so a blanket `super` call
    /// would claim every paste in the app, including the plain text ones the
    /// field handles perfectly, and route them into an attachment strip.
    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        if action == #selector(paste(_:)) {
            return ComposerPasteTarget.shared.canPaste
        }
        return super.canPerformAction(action, withSender: sender)
    }

    override func paste(_ sender: Any?) {
        ComposerPasteTarget.shared.paste()
    }

    /// Under memory pressure, drop every idle (offscreen, not-pushed) chat store
    /// ahead of the normal LRU cap, freeing their buffers + gateway sinks.
    func applicationDidReceiveMemoryWarning(_ application: UIApplication) {
        Task { @MainActor in
            await AppStore.shared?.evictAllIdleStores()
        }
    }

    /// Notification authorization + remote-notification registration.
    /// Idempotent; re-run on foreground while no token is held.
    ///
    /// **Full authorization, not provisional.** Provisional is granted silently
    /// but delivers *quietly*: straight into Notification Center, with no
    /// sound, no lock-screen alert, and no icon badge. That was tolerable while
    /// a push only said "your agent replied"; it is not, now that a push also
    /// says "a tool call is blocked and will deny itself in five minutes unless
    /// you answer it". A notification the user cannot notice is not a
    /// notification. The cost is one system prompt on first launch.
    ///
    /// **iOS honours `options` only on the FIRST determination.** An install
    /// that already answered — including one that took provisional silently on
    /// an earlier build, which is every existing install — does not widen its
    /// granted set just because we asked for more. `logAuthorizationState`
    /// exists so that case is observable instead of presenting as "the badge
    /// mysteriously never appears"; those users have to enable it in Settings.
    static func registerForPush() {
        UNUserNotificationCenter.current().requestAuthorization(
            options: [.alert, .sound, .badge]
        ) { _, error in
            if let error {
                NSLog("baybo: notification authorization failed: %@", error.localizedDescription)
            }
            Self.logAuthorizationState()
            DispatchQueue.main.async {
                UIApplication.shared.registerForRemoteNotifications()
            }
        }
    }

    /// Record what the system actually granted. The badge and the alert are
    /// each load-bearing for a feature, and each degrades SILENTLY when
    /// unauthorized — `setBadgeCount` just fails, a quiet alert just does not
    /// interrupt — so without this line the only symptom is a feature that
    /// appears not to have been built.
    private static func logAuthorizationState() {
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            NSLog(
                "baybo: notification settings — status=%ld alert=%ld sound=%ld badge=%ld",
                settings.authorizationStatus.rawValue,
                settings.alertSetting.rawValue,
                settings.soundSetting.rawValue,
                settings.badgeSetting.rawValue)
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
