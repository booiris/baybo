import Foundation
import UserNotifications

/// The app-icon badge number.
///
/// Two processes write it:
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
///
/// The NSE is a separate process, so it can change the system badge without
/// changing this process's coalescing memo. Foreground/session-entry
/// reconciliation therefore forces a write even when the local count appears
/// unchanged.
@MainActor
enum BadgeCenter {
    /// Ceiling, mirroring the gateway's `BADGE_MAX`. iOS renders a large number
    /// as a wide pill that crowds the icon, and past a point the digit has
    /// stopped being information.
    static let badgeMax = 999

    private struct PendingWrite {
        let id: UInt64
        let count: Int
    }

    /// Last value the system confirmed, so repeated applies for an unchanged
    /// count cost nothing. Never advance this before the completion callback:
    /// a failed write must remain retryable.
    private static var lastApplied: Int?
    private static var pending: PendingWrite?
    private static var nextWriteId: UInt64 = 0

    /// Total unread across the conversations the main list accounts for.
    ///
    /// Archived rows are excluded: the client buckets them onto their own
    /// screen, so counting them would put a number on the icon that nothing
    /// the user opens can explain. Pure and `nonisolated` so tests can assert
    /// it without touching the system badge.
    nonisolated static func total(_ rows: [SessionRow]) -> Int {
        rows.reduce(0) { $0 + ($1.archived ? 0 : $1.unread) }
    }

    /// Set the icon badge, clamped and coalesced. `force` repairs an NSE write
    /// this process cannot observe.
    static func apply(_ count: Int, force: Bool = false) {
        let clamped = min(max(0, count), badgeMax)
        if !force {
            if pending?.count == clamped { return }
            if pending == nil && lastApplied == clamped { return }
        }
        nextWriteId &+= 1
        let write = PendingWrite(id: nextWriteId, count: clamped)
        pending = write
        setSystemBadge(clamped) { error in
            Task { @MainActor in
                guard pending?.id == write.id else { return }
                pending = nil
                if let error {
                    lastApplied = nil
                    NSLog(
                        "baybo: app icon badge update failed — count=%ld: %@",
                        clamped,
                        error.localizedDescription)
                    return
                }
                lastApplied = clamped
            }
        }
    }

    /// Logout / rebind: the conversations belonged to the gateway we just left.
    static func clear() {
        apply(0, force: true)
    }

    private static func setSystemBadge(
        _ count: Int,
        completion: @escaping @Sendable (Error?) -> Void
    ) {
        #if DEBUG
            if let writer = writerForTesting {
                writer(count, completion)
                return
            }
        #endif
        UNUserNotificationCenter.current().setBadgeCount(
            count,
            withCompletionHandler: completion)
    }

    #if DEBUG
        private static var writerForTesting:
            ((Int, @escaping @Sendable (Error?) -> Void) -> Void)?

        static func setWriterForTesting(
            _ writer: @escaping (Int, @escaping @Sendable (Error?) -> Void) -> Void
        ) {
            writerForTesting = writer
        }

        /// Tests share one process; drop the coalescing memo between cases.
        static func resetForTesting() {
            lastApplied = nil
            pending = nil
            nextWriteId = 0
            writerForTesting = nil
        }
    #endif
}
