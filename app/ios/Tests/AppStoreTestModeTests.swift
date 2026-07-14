import Foundation
import Testing

@testable import Baybo

/// `AppStore.init` early-returns under test rather than booting the world (the
/// Rust client's tokio runtime + keychain, a gateway dial, and a rewrite of the
/// simulator's real `sessions.json`). The bundle is HOSTED, so `BayboApp` really
/// does construct an `AppStore` at launch — the probe below is the only thing
/// standing between these suites and the live app.
@Suite @MainActor
struct AppStoreTestModeTests {
    @Test func theTestProbeIsPresentInTheHostProcess() {
        #expect(ProcessInfo.processInfo.environment[AppStore.testEnvironmentKey] != nil)
        #expect(AppStore.runningUnderTest)
    }

    /// Hosted, not hostless: `Lang` resolves through `Bundle.main`, and if that
    /// were the xctest runner every `t(key)` would silently return the key and
    /// every string assertion in the suite would be vacuous.
    @Test func langResolvesAgainstTheHostAppBundle() {
        #expect(Lang.shared.t("chat.accessNetwork") != "chat.accessNetwork")
    }
}
