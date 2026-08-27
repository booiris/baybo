import Testing

@testable import Baybo

/// The rows an agent's LLM pin is picked from.
///
/// Every rule here also lives in `app/web`'s `teamModel.ts`, because both
/// clients write the same triple to the same endpoint — and each one fails
/// SILENTLY when it is only half-implemented: a picker that hides a stale pin
/// leaves the agent failing on a value nobody can see to clear, and one that
/// carries a model across an entry change writes a pin the gateway refuses.
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
            // Lists its own default among its candidates, and its provider is
            // sent no effort at all.
            entry("local", model: "qwen-3", candidates: ["qwen-3", "qwen-3-coder"]),
        ]
    }

    /// The inherit row names what it resolves to today, and still carries the
    /// empty value — an agent that takes it follows `default-llm` wherever the
    /// operator moves it.
    @Test func theInheritRowNamesTodaysDefaultWithoutPinningIt() {
        let rows = LlmPinOptions.entryRows(entries: pool, defaultName: "claude", pinned: nil)
        #expect(rows.first?.value == nil)
        #expect(rows.first?.label.contains("claude") == true)
        #expect(rows.map(\.value) == [nil, "claude", "local"])
    }

    /// The default entry keeps its own NAMED row. Collapsing it into inherit
    /// makes every model inside the deployment's most-used entry unreachable,
    /// because the server refuses a model with no entry.
    @Test func theDefaultEntryStillGetsARowOfItsOwn() {
        let rows = LlmPinOptions.entryRows(entries: pool, defaultName: "claude", pinned: nil)
        #expect(rows.contains { $0.value == "claude" })
    }

    /// Models are drawn from the entry the pin RESOLVES to, so an unpinned
    /// agent can still be given a model out of the default entry.
    @Test func modelsComeFromTheEntryThePinResolvesTo() {
        let rows = LlmPinOptions.modelRows(
            entries: pool, defaultName: "claude", entry: nil, pinned: nil)
        #expect(rows.map(\.value) == [nil, "claude-sonnet-5", "claude-opus-5"])
    }

    /// An entry that lists its default among its candidates must not draw the
    /// same model twice.
    @Test func aModelListedTwiceIsOfferedOnce() {
        let rows = LlmPinOptions.modelRows(
            entries: pool, defaultName: "claude", entry: "local", pinned: nil)
        #expect(rows.map(\.value) == [nil, "qwen-3", "qwen-3-coder"])
    }

    /// No rungs means baybo sends this provider no effort at all — so there is
    /// nothing to offer, and the caller draws no field rather than a disabled
    /// one advertising a knob that does not exist.
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

    /// The rungs are the ENTRY's, never a local ladder: a rung this provider's
    /// dialect cannot say is a pick the gateway refuses.
    @Test func theRungsAreTheEntrysOwn() {
        let rows = LlmPinOptions.effortRows(
            entries: pool, defaultName: "claude", entry: "claude", pinned: nil)
        #expect(rows.map(\.value) == [nil, "low", "high"])
    }

    /// A pin the pool has never heard of stays visible and clearable at every
    /// level. A row that vanishes leaves the picker showing something else
    /// while the agent goes on failing on the value nobody can see.
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

    /// An entry the pool dropped resolves to nothing, so there is no model
    /// list to draw — offering the DEFAULT entry's models under a pin that
    /// names another entry would let one press write a model that entry does
    /// not serve.
    @Test func anEntryThePoolDroppedOffersNoModels() {
        #expect(
            LlmPinOptions.modelRows(
                entries: pool, defaultName: "claude", entry: "retired", pinned: nil
            ).isEmpty)
    }

    /// Nothing to resolve against: a catalog that has not loaded draws no rows
    /// rather than a list of one empty option.
    @Test func anEmptyCatalogDrawsNoModelOrEffortRows() {
        #expect(
            LlmPinOptions.modelRows(entries: [], defaultName: nil, entry: nil, pinned: nil).isEmpty)
        #expect(
            LlmPinOptions.effortRows(entries: [], defaultName: nil, entry: nil, pinned: nil).isEmpty)
    }
}
