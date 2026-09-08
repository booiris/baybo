import Foundation
import Testing

@testable import Baybo

/// The Settings entry editor's contracts, at the `ModelCatalog` seam — the one
/// door every write goes through.
///
/// What these are guarding is not "does the button call the function". It is the
/// three rules that are invisible at the call site and expensive when broken:
/// a write must re-read the catalog rather than guess, a failed write must not
/// leave the mirror claiming it landed, and a write straddling a logout must not
/// repopulate the next gateway's catalog.
@MainActor
struct LlmEntryEditorTests {
    private let temp = TempSupportDir()
    private let client = FakeBayboClient()

    private func makeCatalog(
        defaultName: String = "claude",
        entries: [LlmModelInfo]? = nil
    ) async -> ModelCatalog {
        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: defaultName,
                items: entries
                    ?? [
                        LlmFixtures.entry(
                            "claude", model: "claude-sonnet-5",
                            candidates: ["claude-opus-4-8"], efforts: ["low", "high"]),
                        LlmFixtures.entry(
                            "gpt", provider: "openai", model: "gpt-5.5", candidates: ["o3"],
                            apiKeyConfigured: false, contextWindowOverride: 400_000,
                            effectiveContextWindow: 400_000),
                    ]))
        let catalog = ModelCatalog(client: client, directory: temp.url)
        catalog.refreshIfNeeded()
        _ = await waitUntil { !catalog.models.isEmpty }
        return catalog
    }

    // MARK: - The write goes out as one key, and comes back re-read

    /// `refreshIfNeeded` latches permanently once it succeeds, so a write that
    /// relied on it would show the pre-write row forever. Every write ends in a
    /// forced `reload`, and this is the assertion that keeps it there.
    @Test func aWriteRefetchesTheCatalogRatherThanGuessing() async throws {
        let catalog = await makeCatalog()

        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: "claude",
                items: [
                    LlmFixtures.entry(
                        "claude", model: "claude-opus-4-8",
                        candidates: ["claude-opus-4-8"], efforts: ["low", "high"])
                ]))
        _ = try await catalog.apply(
            .model(model: "claude-opus-4-8"), to: "claude")

        #expect(catalog.entry(named: "claude")?.model == "claude-opus-4-8")
        #expect(
            client.llmEditCalls == [
                .init(entry: "claude", edit: .model(model: "claude-opus-4-8"))
            ])
    }

    /// The server computes `effectiveContextWindow` by layering an override over
    /// tables this app does not have. Clearing an override therefore MOVES a
    /// value the client cannot derive — so the post-write row has to come from
    /// the gateway, never from a local patch of the row we sent.
    @Test func clearingAnOverrideTakesTheServersEffectiveValue() async throws {
        let catalog = await makeCatalog()

        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: "claude",
                items: [
                    LlmFixtures.entry(
                        "gpt", provider: "openai", model: "gpt-5.5",
                        contextWindowOverride: nil, effectiveContextWindow: 272_000)
                ]))
        _ = try await catalog.apply(.contextWindow(tokens: nil), to: "gpt")

        let row = catalog.entry(named: "gpt")
        #expect(row?.contextWindowOverride == nil)
        #expect(row?.effectiveContextWindow == 272_000, "the effective value is the server's")
    }

    @Test func settingTheDefaultRepublishesItForTheChatHeader() async throws {
        let catalog = await makeCatalog(defaultName: "claude")

        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: "gpt",
                items: [
                    LlmFixtures.entry("gpt", provider: "openai", model: "gpt-5.5")
                ]))
        _ = try await catalog.setDefault("gpt")

        #expect(catalog.defaultName == "gpt")
        #expect(client.llmDefaultSetCalls == ["gpt"])
    }

    /// `requires_restart` is the difference between "saved" and "saved but not
    /// running", and it is also what withdraws the Test button — a probe reads
    /// the config FILE, so in this state it would certify settings the gateway
    /// is not serving.
    @Test func aStagedWriteIsReportedAsStaged() async throws {
        let catalog = await makeCatalog()
        client.answerLlmWritesStaged()

        let result = try await catalog.apply(.supportsVision(on: true), to: "claude")

        #expect(result.requiresRestart)
    }

    // MARK: - Failure

    /// A failed write must leave the on-disk mirror exactly as it was. Writing
    /// it optimistically would resurrect a refused edit on the next cold launch,
    /// with nothing to correct it until a successful fetch.
    @Test func aFailedWriteNeverReachesTheMirror() async throws {
        let catalog = await makeCatalog()
        client.failLlmWrite(with: BayboError.Other(message: "HTTP 400"))

        await #expect(throws: (any Error).self) {
            _ = try await catalog.apply(.baseUrl(url: "https://nope.test"), to: "claude")
        }

        let cold = ModelCatalog(client: FakeBayboClient(), directory: temp.url)
        #expect(cold.entry(named: "claude")?.baseUrl == nil)
        #expect(cold.models.count == 2, "the mirror must still hold the pre-write catalog")
    }

    /// The write landed but the read-back blipped. Showing an empty catalog
    /// because a refresh failed would be a worse lie than showing values that
    /// are one edit stale — and it would blank the chat header's pill too.
    @Test func aFailedReloadKeepsTheRowsItAlreadyHas() async throws {
        let catalog = await makeCatalog()
        client.failListModels(with: BayboError.Other(message: "offline"))

        await #expect(throws: (any Error).self) {
            _ = try await catalog.apply(.supportsVision(on: false), to: "claude")
        }

        #expect(catalog.models.count == 2, "a failed read-back must not empty the catalog")
        #expect(catalog.defaultName == "claude")
    }

    // MARK: - Straddling a logout

    /// The epoch guard. A write in flight when the user logs out must not write
    /// the departed gateway's rows into the next binding's catalog.
    ///
    /// The stall is what makes this a straddle rather than a sequence: without
    /// it the whole write runs after `unload`, captures the NEW epoch, and
    /// passes a guard that was never asked the real question.
    @Test func aWriteStraddlingALogoutDoesNotRepopulate() async throws {
        let catalog = await makeCatalog()
        client.stallLlmWrite(ms: 80)

        let write = Task { try? await catalog.apply(.supportsVision(on: true), to: "claude") }
        #expect(await waitUntil { !self.client.llmEditCalls.isEmpty }, "write must be in flight")
        catalog.unload()
        _ = await write.value

        #expect(catalog.models.isEmpty)
        #expect(catalog.defaultName == nil)
    }

    // MARK: - The mirror

    /// The editor's half of the row has to survive a cold offline start too —
    /// otherwise the screen opens claiming every entry is unconfigured.
    @Test func theMirrorCarriesTheEditorsFields() async {
        _ = await makeCatalog()

        let cold = ModelCatalog(client: FakeBayboClient(), directory: temp.url)
        let gpt = cold.entry(named: "gpt")
        #expect(gpt?.apiKeyConfigured == false)
        #expect(gpt?.contextWindowOverride == 400_000)
        #expect(gpt?.effectiveContextWindow == 400_000)
        #expect(cold.entry(named: "claude")?.apiKeyConfigured == true)
    }

    /// A mirror written before the editor existed carries only the six original
    /// fields. It must still decode — painting the pre-editor subset until the
    /// next live fetch — rather than throwing the whole catalog away.
    @Test func anOlderMirrorStillDecodes() throws {
        let legacy = """
            {"defaultName":"claude","models":[
              {"name":"claude","provider":"anthropic","model":"claude-sonnet-5",
               "modelCandidates":["claude-opus-4-8"],"availableEfforts":["low","high"]}
            ]}
            """
        try Data(legacy.utf8).write(to: temp.url.appendingPathComponent("models.json"))

        let cold = ModelCatalog(client: FakeBayboClient(), directory: temp.url)

        #expect(cold.defaultName == "claude")
        #expect(cold.models.count == 1)
        #expect(cold.entry(named: "claude")?.model == "claude-sonnet-5")
        // Absent in the old mirror, so they read as "nothing known yet" rather
        // than blocking the decode.
        #expect(cold.entry(named: "claude")?.apiKeyConfigured == false)
        #expect(cold.entry(named: "claude")?.baseUrl == nil)
    }

    // MARK: - The model list

    /// The endpoint REPLACES the set, so an "add" has to send the existing
    /// members alongside the newcomer. Sending only the newcomer would drop
    /// every other model the entry serves.
    @Test func addingAModelSendsTheWholeSet() async throws {
        let catalog = await makeCatalog()

        _ = try await catalog.setModels(["claude-sonnet-5", "claude-opus-4-8", "o3"], of: "claude")

        #expect(
            client.llmModelListCalls == [
                .init(entry: "claude", models: ["claude-sonnet-5", "claude-opus-4-8", "o3"])
            ])
    }

    /// A list write re-reads like every other write here: `model_list` feeds
    /// `models()`, which is what the pickers offer, and the gateway may have
    /// normalised what it stored.
    @Test func aListWriteRepaintsFromTheServer() async throws {
        let catalog = await makeCatalog()

        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: "claude",
                items: [
                    LlmFixtures.entry(
                        "claude", model: "claude-sonnet-5", candidates: ["o3"],
                        efforts: ["low", "high"])
                ]))
        _ = try await catalog.setModels(["claude-sonnet-5", "o3"], of: "claude")

        let entry = try #require(catalog.entry(named: "claude"))
        #expect(catalog.models(of: entry) == ["claude-sonnet-5", "o3"])
    }

    @Test func aFailedListWriteLeavesTheCatalogAlone() async throws {
        let catalog = await makeCatalog()
        client.failLlmWrite(with: BayboError.Other(message: "HTTP 400"))

        await #expect(throws: (any Error).self) {
            _ = try await catalog.setModels(["claude-sonnet-5"], of: "claude")
        }

        let entry = try #require(catalog.entry(named: "claude"))
        #expect(catalog.models(of: entry) == ["claude-sonnet-5", "claude-opus-4-8"])
    }

    /// The catalog is a LIVE read of the provider, deliberately uncached and
    /// unmirrored — a stale one would offer models the account may no longer
    /// have. So it must reach the core every time it is asked for.
    @Test func theProviderCatalogIsFetchedLive() async throws {
        let catalog = await makeCatalog()
        client.answerCatalog([
            LlmCatalogModel(
                id: "claude-sonnet-5", displayName: "Sonnet 5", contextWindow: 200_000,
                configured: true),
            LlmCatalogModel(
                id: "claude-haiku-4-5", displayName: nil, contextWindow: nil, configured: false),
        ])

        let first = try await catalog.catalog(of: "claude")
        _ = try await catalog.catalog(of: "claude")

        #expect(first.count == 2)
        #expect(first[0].configured, "an already-served model is marked, not offered as new")
        #expect(!first[1].configured)
        #expect(client.llmCatalogFetches == ["claude", "claude"], "never cached")
    }

    // MARK: - The probe

    @Test func theProbeReachesTheCoreAndCarriesTheProvidersProse() async throws {
        let catalog = await makeCatalog()
        client.answerProbe(LlmFixtures.probeFailed("401 invalid x-api-key"))

        let result = try await catalog.test(entry: "claude")

        #expect(client.llmProbeCalls == ["claude"])
        #expect(!result.ok)
        #expect(result.error == "401 invalid x-api-key")
        #expect(result.latencyMs == nil)
    }
}
