import Foundation
import Testing

@testable import Baybo

/// The crash-reload cap shared by the three warm webviews. Past the cap a page
/// used to stay dead for the rest of the process — for the transcript that is
/// every conversation, blank until the app was killed — so the cap now parks
/// and a user-driven edge revives it.
@Suite struct CrashReloadBudgetTests {
    private let t0 = Date(timeIntervalSinceReferenceDate: 0)

    private func at(_ seconds: TimeInterval) -> Date {
        t0.addingTimeInterval(seconds)
    }

    private func parkedBudget() -> CrashReloadBudget {
        var budget = CrashReloadBudget()
        for i in 0...CrashReloadBudget.maxConsecutiveDeaths {
            _ = budget.recordDeath(at: at(Double(i)))
        }
        return budget
    }

    @Test func reloadsUpToTheCapThenParks() {
        var budget = CrashReloadBudget()
        var verdicts: [CrashReloadBudget.Verdict] = []
        for i in 0...CrashReloadBudget.maxConsecutiveDeaths {
            verdicts.append(budget.recordDeath(at: at(Double(i))))
        }
        let expected =
            Array(repeating: CrashReloadBudget.Verdict.reload,
                  count: CrashReloadBudget.maxConsecutiveDeaths) + [.park]
        #expect(verdicts == expected)
        #expect(budget.parked)
    }

    @Test func deathsFurtherApartThanTheWindowNeverPark() {
        var budget = CrashReloadBudget()
        var verdicts: [CrashReloadBudget.Verdict] = []
        for i in 0..<10 {
            verdicts.append(budget.recordDeath(at: at(Double(i) * (CrashReloadBudget.window + 1))))
        }
        #expect(verdicts.allSatisfy { $0 == .reload })
        #expect(!budget.parked)
    }

    @Test func reviveIsANoOpUnlessParked() {
        var budget = CrashReloadBudget()
        let revived = budget.revive(at: at(0))
        #expect(!revived)
    }

    /// One user edge buys one reload: dying again inside the window parks at
    /// once instead of spending a fresh three.
    @Test func aRevivedPageThatDiesAgainSoonParksAtOnce() {
        var budget = parkedBudget()
        let revived = budget.revive(at: at(100))
        let verdict = budget.recordDeath(at: at(101))
        #expect(revived)
        #expect(verdict == .park)
    }

    /// A revived page that survives a whole window has proven itself.
    @Test func aRevivedPageThatSurvivesTheWindowEarnsAFreshBudget() {
        var budget = parkedBudget()
        let revived = budget.revive(at: at(100))
        let later = 100 + CrashReloadBudget.window + 1
        let first = budget.recordDeath(at: at(later))
        let second = budget.recordDeath(at: at(later + 1))
        #expect(revived)
        #expect(first == .reload)
        #expect(second == .reload)
    }
}
