import Foundation

/// How many back-to-back WebContent deaths a host answers with a document
/// reload, and what happens past that — shared by the transcript, deck and
/// issue bridges, which each own exactly one webview that WebKit will NOT
/// reload on its own (implementing `webViewWebContentProcessDidTerminate` opts
/// a view out of WebKit's automatic recovery, visible or not).
///
/// The kill is memory pressure, and the reload rebuilds the same footprint, so
/// an uncapped handler flickers forever. The count re-arms on TIME ONLY — a
/// death landing more than `window` after the previous one resets it — never on
/// a paint: the white-flash loop painted on every reload and re-exploded within
/// ~1s.
///
/// Past the cap the host PARKS instead of giving up for good. A parked page is
/// dead and stays unloaded, but a user-driven edge (`revive`: opening the
/// screen, returning to the foreground) buys exactly one more reload — a
/// death within `window` of that reload parks again at once. Before parking
/// existed, the cap was permanent: the one shared webview stayed blank for
/// every screen it served until the process was killed.
struct CrashReloadBudget {
    enum Verdict: Equatable {
        case reload
        case park
    }

    static let maxConsecutiveDeaths = 3
    static let window: TimeInterval = 30

    private var consecutiveDeaths = 0
    private var lastDeathAt = Date.distantPast
    private(set) var parked = false

    mutating func recordDeath(at now: Date = Date()) -> Verdict {
        if now.timeIntervalSince(lastDeathAt) > Self.window {
            consecutiveDeaths = 0
        }
        lastDeathAt = now
        consecutiveDeaths += 1
        guard consecutiveDeaths <= Self.maxConsecutiveDeaths else {
            parked = true
            return .park
        }
        return .reload
    }

    /// `true` when the host was parked and must reload now. The revived page is
    /// on its last chance: the count stays at the cap and the window restarts,
    /// so only a reload that survives a full window earns a fresh budget.
    mutating func revive(at now: Date = Date()) -> Bool {
        guard parked else { return false }
        parked = false
        consecutiveDeaths = Self.maxConsecutiveDeaths
        lastDeathAt = now
        return true
    }
}
