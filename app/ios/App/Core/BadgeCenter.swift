import Foundation
import UserNotifications

/// The app-icon badge number.
///
/// Two writers set it, and they agree by construction rather than by luck:
///
/// - **The gateway**, on every push, by sealing a `badge` into the encrypted
///   preview plaintext that the Notification Service Extension applies locally.
///   That is the only writer that can reach a phone whose app is not running.
/// - **This app**, from `SessionIndex`, whenever the chat list's own unread
///   state changes. This is the writer that keeps the icon honest while the
///   user is actually reading — opening a conversation drops its badge here
///   long before the read cursor round-trips to the gateway.
///
/// Both set an ABSOLUTE value, never an increment, so the number self-heals: a
/// dropped push, an NSE that ran out of execution budget, or a stale count is
/// corrected by the next push or the next time the app comes forward. The two
/// can disagree transiently — the app counts what the LIST counts (which the
/// live activity ping bumps for user sends too), while the gateway counts
/// unread assistant replies — and they converge on the next list merge, which
/// is the same reconciliation the row badges already go through.
@MainActor
enum BadgeCenter {
    /// Ceiling, mirroring the gateway's `BADGE_MAX`. iOS renders a large number
    /// as a wide pill that crowds the icon, and past a point the digit has
    /// stopped being information.
    static let badgeMax = 999

    /// Last value handed to the system, so repeated applies for an unchanged
    /// count cost nothing. `SessionIndex.save()` runs on essentially every list
    /// mutation — a merge, an activity ping, a badge clear — and most of them
    /// do not move this number.
    private static var lastApplied: Int?

    /// Total unread across the conversations the main list accounts for.
    ///
    /// Archived rows are excluded: the client buckets them onto their own
    /// screen, so counting them would put a number on the icon that nothing
    /// the user opens can explain. Pure and `nonisolated` so tests can assert
    /// it without touching the system badge.
    nonisolated static func total(_ rows: [SessionRow]) -> Int {
        rows.reduce(0) { $0 + ($1.archived ? 0 : $1.unread) }
    }

    /// Set the icon badge, clamped and coalesced.
    ///
    /// Failure is swallowed deliberately: the most common cause is that the
    /// install never got `.badge` authorization, which is a settled fact about
    /// the user's choice, not an error to surface on every list refresh. See
    /// `AppDelegate.registerForPush`.
    static func apply(_ count: Int) {
        let clamped = min(max(0, count), badgeMax)
        guard lastApplied != clamped else { return }
        lastApplied = clamped
        UNUserNotificationCenter.current().setBadgeCount(clamped)
    }

    /// Logout / rebind: the conversations belonged to the gateway we just left.
    static func clear() {
        apply(0)
    }

    #if DEBUG
        /// Tests share one process; drop the coalescing memo between cases.
        static func resetForTesting() {
            lastApplied = nil
        }
    #endif
}
