import Testing

@testable import Baybo

@Suite
struct ApnsEnvironmentTests {
    @Test func signedDevelopmentValueUsesTheSandboxService() {
        #expect(Baybo.apnsEnvironment(for: "development") == .sandbox)
    }

    @Test func distributionValueUsesTheProductionService() {
        #expect(Baybo.apnsEnvironment(for: "production") == .production)
    }

    @Test func missingOrUnknownValueFailsClosedToSandbox() {
        #expect(Baybo.apnsEnvironment(for: nil) == .sandbox)
        #expect(Baybo.apnsEnvironment(for: "unexpected") == .sandbox)
    }
}
