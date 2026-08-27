import Testing

@testable import Baybo

@MainActor
struct LlmPinOptionsTests {
    private func entry(
        _ name: String, model: String, candidates: [String] = [], efforts: [String] = []
    ) -> LlmModelInfo {
        LlmModelInfo(
            name: name, provider: "p", model: model, modelCandidates: candidates,
            reasoningEffort: nil, availableEfforts: efforts)
    }

    private var pool: [LlmModelInfo] {
        [
            entry("claude", model: "claude-sonnet-5", candidates: ["claude-opus-5"],
                efforts: ["low", "high"]),
            entry("local", model: "qwen-3", candidates: ["qwen-3", "qwen-3-coder"]),
        ]
    }

    @Test func theInheritRowNamesTodaysDefaultWithoutPinningIt() {
        let rows = LlmPinOptions.entryRows(entries: pool, defaultName: "claude", pinned: nil)
        #expect(rows.first?.value == nil)
        #expect(rows.first?.label.contains("claude") == true)
        #expect(rows.map(\.value) == [nil, "claude", "local"])
    }

    @Test func theDefaultEntryStillGetsARowOfItsOwn() {
        let rows = LlmPinOptions.entryRows(entries: pool, defaultName: "claude", pinned: nil)
        #expect(rows.contains { $0.value == "claude" })
    }

    @Test func modelsComeFromTheEntryThePinResolvesTo() {
        let rows = LlmPinOptions.modelRows(
            entries: pool, defaultName: "claude", entry: nil, pinned: nil)
        #expect(rows.map(\.value) == [nil, "claude-sonnet-5", "claude-opus-5"])
    }

    @Test func aModelListedTwiceIsOfferedOnce() {
        let rows = LlmPinOptions.modelRows(
            entries: pool, defaultName: "claude", entry: "local", pinned: nil)
        #expect(rows.map(\.value) == [nil, "qwen-3", "qwen-3-coder"])
    }

    @Test func aProviderWithNoRungsGetsNoLadderAtAll() {
        #expect(
            LlmPinOptions.effortRows(
                entries: pool, defaultName: "claude", entry: "local", pinned: nil
            ).isEmpty)
        #expect(
            !LlmPinOptions.effortRows(
                entries: pool, defaultName: "claude", entry: "claude", pinned: nil
            ).isEmpty)
    }

    @Test func theRungsAreTheEntrysOwn() {
        let rows = LlmPinOptions.effortRows(
            entries: pool, defaultName: "claude", entry: "claude", pinned: nil)
        #expect(rows.map(\.value) == [nil, "low", "high"])
    }

    @Test func aPinThePoolNoLongerKnowsStaysOnScreen() {
        let entries = LlmPinOptions.entryRows(
            entries: pool, defaultName: "claude", pinned: "retired")
        #expect(entries.last?.value == "retired")

        let models = LlmPinOptions.modelRows(
            entries: pool, defaultName: "claude", entry: "claude", pinned: "gone-4")
        #expect(models.last?.value == "gone-4")

        let efforts = LlmPinOptions.effortRows(
            entries: pool, defaultName: "claude", entry: "claude", pinned: "ultra")
        #expect(efforts.last?.value == "ultra")
    }

    @Test func anEntryThePoolDroppedOffersNoModels() {
        #expect(
            LlmPinOptions.modelRows(
                entries: pool, defaultName: "claude", entry: "retired", pinned: nil
            ).isEmpty)
    }

    @Test func anEmptyCatalogDrawsNoModelOrEffortRows() {
        #expect(
            LlmPinOptions.modelRows(entries: [], defaultName: nil, entry: nil, pinned: nil).isEmpty)
        #expect(
            LlmPinOptions.effortRows(entries: [], defaultName: nil, entry: nil, pinned: nil).isEmpty)
    }
}
