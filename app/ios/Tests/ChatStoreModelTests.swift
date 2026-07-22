import Foundation
import Testing

@testable import Baybo

/// The header model picker's store half: seeding `(entry, model)` from the
/// session meta, the optimistic select + revert-on-failure contract, and the
/// draft path — a selection made before the session exists must land on the
/// gateway BETWEEN session creation and the first send, or the first turn runs
/// on the wrong model with nothing in the UI saying so.
@Suite @MainActor
struct ChatStoreModelTests {
    private static let sessionId = "s-model"

    private let temp = TempSupportDir()
    private let client = FakeBayboClient()

    private func makeStore(listed: Bool) -> ChatStore {
        let index = temp.makeIndex()
        if listed {
            index.touch(sessionId: Self.sessionId)
        }
        return ChatStore(
            sessionId: Self.sessionId, client: client, index: index,
            outbox: temp.makeOutbox(sessionId: Self.sessionId))
    }

    private func call(_ llm: String?, _ model: String?, _ effort: String? = nil)
        -> FakeBayboClient.SetModelCall
    {
        FakeBayboClient.SetModelCall(
            sessionId: Self.sessionId, llm: llm, model: model, effort: effort)
    }

    @Test func refreshSeedsThePinFromTheSessionMeta() async {
        client.answerSessionModel(llm: "gpt", model: "o3")
        let store = makeStore(listed: true)
        #expect(!store.modelPinResolved, "a listed session's pin is unknown until read")

        store.refreshModelPin()

        #expect(await waitUntil { store.modelPin == "gpt" })
        #expect(store.modelPinModel == "o3")
        #expect(store.modelPinResolved)
        #expect(client.modelReadSessions == [Self.sessionId])
    }

    /// The store stays cached resident and the gateway broadcasts no frame for
    /// a re-pin made on another client — every open must re-read, or the pill
    /// goes stale forever on exactly the most-used sessions.
    @Test func refreshRereadsOnEveryOpen() async {
        client.answerSessionModel(llm: "gpt", model: "o3")
        let store = makeStore(listed: true)

        store.refreshModelPin()
        #expect(await waitUntil { store.modelPinModel == "o3" })

        client.answerSessionModel(llm: "claude", model: "claude-opus-4-8")
        store.refreshModelPin()

        #expect(await waitUntil { store.modelPinModel == "claude-opus-4-8" })
        #expect(store.modelPin == "claude")
        #expect(client.modelReadSessions.count == 2)
    }

    /// A pin read still in flight when the user picks must not land on top of
    /// the pick — the read's answer is already stale.
    @Test func aFetchNeverClobbersAFresherSelection() async {
        client.answerSessionModel(llm: nil, model: nil)
        client.stallSessionModel(ms: 80)
        let store = makeStore(listed: true)

        store.refreshModelPin()
        store.selectModel(entry: "gpt", model: "gpt-5.5")

        try? await Task.sleep(for: .milliseconds(160))
        #expect(store.modelPin == "gpt")
        #expect(store.modelPinModel == "gpt-5.5")
    }

    @Test func refreshOnADraftIsANoOp() async {
        let store = makeStore(listed: false)

        store.refreshModelPin()

        try? await Task.sleep(for: .milliseconds(50))
        #expect(client.modelReadSessions.isEmpty)
        #expect(store.modelPin == nil)
    }

    @Test func selectPutsThePinAndKeepsTheOptimisticValue() async {
        let store = makeStore(listed: true)

        store.selectModel(entry: "gpt", model: "o3")

        #expect(store.modelPin == "gpt")
        #expect(store.modelPinModel == "o3")
        #expect(await waitUntil { self.client.setModelCalls == [self.call("gpt", "o3")] })
        #expect(store.notice == nil)
    }

    /// Once resolved, re-picking the identical (entry, model) is a no-op.
    /// (While unresolved the same pick deliberately sends — the display may
    /// not match the server, and the PUT is the user's way to force it.)
    @Test func reselectingTheResolvedPinSendsNothing() async {
        client.answerSessionModel(llm: "gpt", model: "o3")
        let store = makeStore(listed: true)
        store.refreshModelPin()
        #expect(await waitUntil { store.modelPinResolved })

        store.selectModel(entry: "gpt", model: "o3")

        try? await Task.sleep(for: .milliseconds(50))
        #expect(client.setModelCalls.isEmpty)
    }

    @Test func aFailedPutRevertsThePillAndRaisesTheNotice() async {
        client.failSetModel(with: BayboError.Other(message: "boom"))
        let store = makeStore(listed: true)

        store.selectModel(entry: "gpt", model: "o3")
        #expect(store.modelPin == "gpt")

        #expect(await waitUntil { store.modelPin == nil })
        #expect(store.modelPinModel == nil)
        #expect(store.notice != nil)
    }

    /// Failures revert to the last GATEWAY-ACKNOWLEDGED selection, never to the
    /// previous display value: with two picks failing back to back, the pill
    /// rests on the server's actual state (no pin), not the first failed pick.
    @Test func failuresRevertToTheConfirmedPinNotThePreviousPick() async {
        client.failSetModel(with: BayboError.Other(message: "boom"))
        let store = makeStore(listed: true)

        store.selectModel(entry: "gpt", model: "o3")
        store.selectModel(entry: "claude", model: "claude-sonnet-5")

        #expect(await waitUntil { client.setModelCalls.count == 2 })
        #expect(await waitUntil { store.modelPin == nil })
        #expect(store.modelPinModel == nil)
        #expect(store.notice != nil)
    }

    /// The draft contract: pick before the first send, and the pin lands
    /// between `chatCreateSession` and the first transmission, so the first
    /// turn's actor already reads it.
    @Test func aDraftPinLandsBetweenCreationAndTheFirstSend() async {
        let store = makeStore(listed: false)

        store.selectModel(entry: "gpt", model: "o3")
        #expect(store.modelPin == "gpt")
        try? await Task.sleep(for: .milliseconds(50))
        #expect(client.setModelCalls.isEmpty, "a draft has no remote row to pin yet")

        store.send(text: "hello", attachments: [])

        #expect(
            await waitUntil {
                client.callTimeline.contains("sendAfterConnect")
                    || client.callTimeline.contains("send")
            })
        let timeline = client.callTimeline
        let create = timeline.firstIndex(of: "create")
        let setModel = timeline.firstIndex(of: "setModel")
        let send =
            timeline.firstIndex(of: "sendAfterConnect") ?? timeline.firstIndex(of: "send")
        #expect(create != nil && setModel != nil && send != nil)
        if let create, let setModel, let send {
            #expect(create < setModel && setModel < send, "timeline: \(timeline)")
        }
        #expect(client.setModelCalls == [call("gpt", "o3")])
    }

    /// Picking a model then clearing back to no-pin before the first send must
    /// create no pin — the session follows `default-llm`, and a PUT against a
    /// not-yet-created session would fail.
    @Test func aDraftChoiceRevertedToDefaultCreatesNoPin() async {
        let store = makeStore(listed: false)

        store.selectModel(entry: "gpt", model: "o3")
        store.selectModel(entry: nil, model: nil)
        store.send(text: "hello", attachments: [])

        #expect(
            await waitUntil {
                client.callTimeline.contains("sendAfterConnect")
                    || client.callTimeline.contains("send")
            })
        #expect(client.setModelCalls.isEmpty)
    }

    // MARK: - Reasoning effort

    private func makeCatalog() async -> ModelCatalog {
        client.answerModelCatalog(
            LlmModelCatalog(
                defaultName: "claude",
                items: [
                    LlmModelInfo(
                        name: "claude", provider: "anthropic", model: "claude-sonnet-5",
                        modelCandidates: ["claude-opus-4-8"], reasoningEffort: nil),
                    LlmModelInfo(
                        name: "gpt", provider: "openai", model: "gpt-5.5",
                        modelCandidates: ["o3"], reasoningEffort: "high"),
                ]))
        let catalog = ModelCatalog(client: client, directory: temp.url)
        catalog.refreshIfNeeded()
        _ = await waitUntil { !catalog.models.isEmpty }
        return catalog
    }

    /// The `models.json` mirror: a cold offline start paints the catalog from
    /// disk (pill + panel usable with zero network), including candidates.
    @Test func theCatalogMirrorPaintsAColdOfflineStart() async {
        _ = await makeCatalog()

        let offline = ModelCatalog(client: FakeBayboClient(), directory: temp.url)
        #expect(offline.defaultName == "claude")
        #expect(offline.models.count == 2)
        #expect(offline.models.first?.model == "claude-sonnet-5")
        #expect(offline.models.first?.modelCandidates == ["claude-opus-4-8"])
    }

    @Test func resetDropsTheMirrorWithTheCatalog() async {
        let catalog = await makeCatalog()

        catalog.reset()

        #expect(catalog.models.isEmpty)
        let offline = ModelCatalog(client: FakeBayboClient(), directory: temp.url)
        #expect(offline.models.isEmpty, "a rebind must not inherit the departed gateway's mirror")
    }

    /// Picking a thinking level writes the PER-SESSION pin (entry + model +
    /// effort), NOT a global entry edit — one `setModel` PUT carrying the
    /// effort, and the pill reflects it.
    @Test func selectEffortWritesThePerSessionPin() async {
        let catalog = await makeCatalog()  // default entry "claude", default model
        let store = makeStore(listed: true)

        store.selectEffort(
            entryName: "claude", model: "claude-sonnet-5", effort: "medium", catalog: catalog)

        #expect(store.modelPinEffort == "medium")
        #expect(store.modelPin == "claude")
        #expect(store.modelPinModel == "claude-sonnet-5")
        #expect(
            await waitUntil {
                self.client.setModelCalls == [self.call("claude", "claude-sonnet-5", "medium")]
            })
    }

    /// Picking a level under a NON-effective entry pins that entry+model and
    /// the effort together, in one PUT.
    @Test func selectEffortOnANonEffectiveEntryPinsEntryModelAndEffort() async {
        let catalog = await makeCatalog()
        let store = makeStore(listed: true)

        store.selectEffort(entryName: "gpt", model: "o3", effort: "low", catalog: catalog)

        #expect(store.modelPin == "gpt")
        #expect(store.modelPinModel == "o3")
        #expect(store.modelPinEffort == "low")
        #expect(await waitUntil { self.client.setModelCalls == [self.call("gpt", "o3", "low")] })
    }

    /// Selecting a model KEEPS the current effort (only entry+model change).
    @Test func selectModelPreservesTheEffortPin() async {
        let catalog = await makeCatalog()
        let store = makeStore(listed: true)
        store.selectEffort(entryName: "gpt", model: "o3", effort: "high", catalog: catalog)
        #expect(await waitUntil { !self.client.setModelCalls.isEmpty })

        store.selectModel(entry: "gpt", model: "gpt-5.5")

        #expect(store.modelPinEffort == "high")
        #expect(await waitUntil { self.client.setModelCalls.last == self.call("gpt", "gpt-5.5", "high") })
    }

    @Test func reselectingTheCurrentLevelSendsNothing() async {
        client.answerSessionModel(llm: "gpt", model: "o3", effort: "high")
        let store = makeStore(listed: true)
        store.refreshModelPin()
        #expect(await waitUntil { store.modelPinEffort == "high" })

        store.selectEffort(entryName: "gpt", model: "o3", effort: "high", catalog: await makeCatalog())

        try? await Task.sleep(for: .milliseconds(50))
        #expect(client.setModelCalls.isEmpty)
    }

    /// A failed effort PUT reverts the pill and says so.
    @Test func aFailedEffortPutRevertsAndRaisesTheNotice() async {
        client.failSetModel(with: BayboError.Other(message: "boom"))
        let catalog = await makeCatalog()
        let store = makeStore(listed: true)

        store.selectEffort(
            entryName: "claude", model: "claude-sonnet-5", effort: "xhigh", catalog: catalog)
        #expect(store.modelPinEffort == "xhigh")

        #expect(await waitUntil { store.modelPinEffort == nil })
        #expect(store.notice != nil)
    }

    /// A failed draft-pin PUT degrades to the default model — revert the pill,
    /// raise the notice, and NEVER block the send that triggered it.
    @Test func aFailedDraftPinRevertsAndStillSends() async {
        client.failSetModel(with: BayboError.Other(message: "boom"))
        let store = makeStore(listed: false)

        store.selectModel(entry: "gpt", model: "o3")
        store.send(text: "hello", attachments: [])

        #expect(
            await waitUntil {
                client.callTimeline.contains("sendAfterConnect")
                    || client.callTimeline.contains("send")
            })
        #expect(await waitUntil { store.modelPin == nil })
        #expect(store.modelPinModel == nil)
        #expect(store.notice != nil)
    }
}
