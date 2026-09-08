import Foundation

/// The gateway's configured LLM entries + the current `default-llm` name —
/// the chat header model picker's catalog and the Settings entry editor's
/// backing store (`GET /v1/llm/models` over the active leg). Global, not
/// per-session: one fetch serves every chat header.
///
/// Fetched lazily on chat open and cached for the app run — the entry set
/// changes when the operator edits the gateway config, not mid-conversation;
/// a failed fetch just retries on the next open. A `models.json` MIRROR (the
/// `deck.json` idiom) makes the catalog cold-start-offline durable: the pill
/// and panel paint from disk with zero network, and the next successful fetch
/// rewrites it. Each gateway has its own mirror namespace.
///
/// **Two kinds of refresh, and they are not interchangeable.**
/// [`refreshIfNeeded`] is the once-per-run latch every reader calls;
/// [`reload`] is the forced refetch a WRITE needs, because the row's
/// `effective*` columns are computed gateway-side by layering an override over
/// the OpenRouter snapshot — tables this app does not have — so no local patch
/// can produce the row that a save actually resulted in.
@MainActor
final class ModelCatalog: ObservableObject {
    static let shared = ModelCatalog()

    /// The `default-llm` entry name — what an unpinned session resolves
    /// against, and therefore the pill's label when `ChatStore.modelPin` is nil.
    @Published private(set) var defaultName: String?
    @Published private(set) var models: [LlmModelInfo] = []

    private let client: any BayboClientProtocol
    private var mirrorURL: URL
    private var fetchTask: Task<Void, Never>?
    /// Bumped on `unload` so a fetch that straddles a logout can't repopulate
    /// the next binding's catalog with the departed gateway's entries.
    private var epoch = 0
    /// One live fetch per app run — separate from `models.isEmpty`, which the
    /// mirror satisfies at launch: a mirror-painted catalog must still be
    /// reconciled against the gateway once, or a config edit made while the
    /// app was dead would stick until the next logout.
    private var fetchedThisRun = false

    init(
        client: any BayboClientProtocol = Baybo.client,
        directory: URL = SessionIndex.supportDirectory()
    ) {
        self.client = client
        mirrorURL = directory.appendingPathComponent("models.json")
        loadMirror()
    }

    /// Fetch once per app run, coalescing concurrent callers; a failure clears
    /// the in-flight latch so the next chat open retries. The mirror may have
    /// painted rows already — the fetch still runs and REPLACES them.
    func refreshIfNeeded() {
        #if DEBUG
            if seedDemoIfRequested() { return }
        #endif
        guard !fetchedThisRun, fetchTask == nil else { return }
        let epoch = epoch
        fetchTask = Task {
            do {
                let catalog = try await client.llmListModels()
                guard epoch == self.epoch else { return }
                defaultName = catalog.defaultName
                models = catalog.items
                fetchedThisRun = true
                saveMirror()
            } catch {
                NSLog("baybo: list models: %@", bayboErrorText(error))
                if epoch == self.epoch { fetchTask = nil }
            }
        }
    }

    /// Refetch NOW and await it — what every config write does on the way out.
    ///
    /// Deliberately not `refreshIfNeeded`: that latches permanently
    /// (`fetchedThisRun` is set on success and `fetchTask` is only cleared in
    /// the failure branch), so after the first success the only way to make it
    /// fetch again is `unload`, which empties `models` and would blank the chat
    /// pill mid-session.
    ///
    /// A failure leaves the CURRENT rows in place and rethrows. The write it
    /// follows has already landed on the gateway; showing an empty catalog
    /// because the read-back blipped would be a worse lie than showing values
    /// that are one edit stale.
    func reload() async throws {
        try await reload(expecting: epoch)
    }

    /// `expecting` is the epoch the CALLER started at, not the one this call
    /// finds. A write that began before a logout and lands after it would
    /// otherwise re-read on the new binding's epoch, pass its own guard, and
    /// repopulate the fresh catalog with the departed gateway's entries.
    private func reload(expecting epoch: Int) async throws {
        #if DEBUG
            if seedDemoIfRequested() { return }
        #endif
        guard epoch == self.epoch else { return }
        let catalog = try await client.llmListModels()
        guard epoch == self.epoch else { return }
        defaultName = catalog.defaultName
        models = catalog.items
        fetchedThisRun = true
        saveMirror()
    }

    /// Explicit destructive reset used by isolated tests.
    func reset() {
        unload()
        try? FileManager.default.removeItem(at: mirrorURL)
    }

    func unload() {
        epoch += 1
        fetchTask?.cancel()
        fetchTask = nil
        fetchedThisRun = false
        defaultName = nil
        models = []
    }

    func activate(directory: URL) {
        unload()
        mirrorURL = directory.appendingPathComponent("models.json")
        loadMirror()
    }

    /// The entry's reasoning-effort override; `nil` = provider default.
    func reasoningEffort(of name: String) -> String? {
        models.first { $0.name == name }?.reasoningEffort
    }

    func entry(named name: String) -> LlmModelInfo? {
        models.first { $0.name == name }
    }

    /// The entry the session resolves against: `pin` when it names a cached
    /// entry, else the `default-llm` entry. `nil` while the catalog is empty
    /// or when the pin names an entry the catalog no longer has (the caller
    /// falls back to the raw pin string for display).
    func effectiveEntry(pin: String?) -> LlmModelInfo? {
        LlmPinOptions.effectiveEntry(in: models, defaultName: defaultName, pinned: pin)
    }

    /// The models an entry can be pinned to — its default `model` first, then
    /// each `model_candidates` id, de-duped. What the picker lists under the
    /// entry.
    func models(of entry: LlmModelInfo) -> [String] {
        LlmPinOptions.models(of: entry)
    }

    // MARK: - Writes (global gateway config, not a session pin)

    /// Change ONE field of one entry, then re-read the catalog.
    ///
    /// Not optimistic, and that is not caution — it is the only correct move.
    /// A row's `effectiveContextWindow` / `effectiveSupportsVision` are the
    /// result of layering an override over tables that live on the gateway, so
    /// clearing an override, or changing `model`, moves values this app cannot
    /// compute. Guessing them would paint a number the gateway disagrees with.
    ///
    /// The read-back is awaited so the caller's spinner covers it and the rows
    /// repaint once, from server truth.
    func apply(_ edit: LlmEntryEdit, to entry: String) async throws -> LlmMutateResult {
        let epoch = epoch
        let result = try await client.llmUpdateModel(name: entry, edit: edit)
        try await reload(expecting: epoch)
        return result
    }

    /// Move `default-llm` — which entry an UNPINNED session runs on, on every
    /// device. The reload is what republishes `defaultName` to the chat
    /// header's capsule and the picker's checkmark.
    @discardableResult
    func setDefault(_ name: String) async throws -> LlmMutateResult {
        let epoch = epoch
        let result = try await client.llmSetDefault(name: name)
        try await reload(expecting: epoch)
        return result
    }

    /// Replace the models an entry serves.
    ///
    /// The set is sent whole rather than as an add or a remove: model ids carry
    /// slashes, and a whole-set write is idempotent, so a replayed leg
    /// converges instead of double-adding. The gateway carries each surviving
    /// id's overrides across, so sending plain ids never destroys them.
    @discardableResult
    func setModels(_ models: [String], of entry: String) async throws -> LlmMutateResult {
        let epoch = epoch
        let result = try await client.llmSetModelList(name: entry, models: models)
        try await reload(expecting: epoch)
        return result
    }

    /// One real, billed completion against the provider — the only pre-flight
    /// in the system that touches the vendor. Reads nothing back: the probe
    /// changes no config.
    func test(entry: String) async throws -> LlmTestResult {
        try await client.llmTestModel(name: entry)
    }

    /// The provider's live catalog — what "add a model" picks from. Not cached
    /// and not mirrored: it is a read of the vendor's current offering, and a
    /// stale one would offer models the account may no longer have.
    func catalog(of entry: String) async throws -> [LlmCatalogModel] {
        try await client.llmCatalog(name: entry)
    }

    // MARK: - Mirror (`models.json` — a pure cache, never a source of truth)

    /// Codable twin of the uniffi records (which aren't Codable themselves).
    ///
    /// Every field added after the first shipped mirror is optional here, so a
    /// mirror written by an older build still decodes — it simply paints the
    /// pre-editor subset until the next live fetch fills the rest in. A cold-
    /// paint concession, not a source of truth.
    private struct Mirror: Codable {
        var defaultName: String?
        var models: [Entry]

        struct Entry: Codable {
            var name: String
            var provider: String
            var model: String
            var modelCandidates: [String]
            var reasoningEffort: String?
            /// Absent in mirrors written before the thinking ladder existed —
            /// decodes empty, which reads as "no Thinking row" until the next
            /// live fetch fills it in.
            var availableEfforts: [String]?
            var liteModel: String?
            var baseUrl: String?
            var apiKeyEnv: String?
            var apiKeyConfigured: Bool?
            var contextWindowOverride: UInt32?
            var effectiveContextWindow: UInt32?
            var supportsVisionOverride: Bool?
            var effectiveSupportsVision: Bool?
        }
    }

    private func loadMirror() {
        guard let data = try? Data(contentsOf: mirrorURL),
            let mirror = try? JSONDecoder().decode(Mirror.self, from: data)
        else { return }
        defaultName = mirror.defaultName
        models = mirror.models.map {
            LlmModelInfo(
                name: $0.name, provider: $0.provider, model: $0.model,
                modelCandidates: $0.modelCandidates, reasoningEffort: $0.reasoningEffort,
                availableEfforts: $0.availableEfforts ?? [],
                liteModel: $0.liteModel,
                baseUrl: $0.baseUrl,
                apiKeyEnv: $0.apiKeyEnv,
                apiKeyConfigured: $0.apiKeyConfigured ?? false,
                contextWindowOverride: $0.contextWindowOverride,
                effectiveContextWindow: $0.effectiveContextWindow ?? 0,
                supportsVisionOverride: $0.supportsVisionOverride,
                effectiveSupportsVision: $0.effectiveSupportsVision ?? false)
        }
    }

    private func saveMirror() {
        let mirror = Mirror(
            defaultName: defaultName,
            models: models.map {
                Mirror.Entry(
                    name: $0.name, provider: $0.provider, model: $0.model,
                    modelCandidates: $0.modelCandidates, reasoningEffort: $0.reasoningEffort,
                    availableEfforts: $0.availableEfforts,
                    liteModel: $0.liteModel,
                    baseUrl: $0.baseUrl,
                    apiKeyEnv: $0.apiKeyEnv,
                    apiKeyConfigured: $0.apiKeyConfigured,
                    contextWindowOverride: $0.contextWindowOverride,
                    effectiveContextWindow: $0.effectiveContextWindow,
                    supportsVisionOverride: $0.supportsVisionOverride,
                    effectiveSupportsVision: $0.effectiveSupportsVision)
            })
        guard let data = try? JSONEncoder().encode(mirror) else { return }
        try? data.write(to: mirrorURL, options: .atomic)
    }

    #if DEBUG
        /// `-baybo-demo-models`: seed a canned catalog so the header's model
        /// pill and the Settings entry editor render headlessly (no gateway to
        /// list from). Never persisted: a later plain launch on the same
        /// simulator must not inherit it.
        ///
        /// The two entries are deliberately opposite: `claude` inherits
        /// everything (no overrides, a resolvable key), `gpt` pins every
        /// override and carries the `apiKeyEnv` shadow — between them they
        /// cover both halves of the editor's inherited-vs-pinned language.
        private func seedDemoIfRequested() -> Bool {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-models") else {
                return false
            }
            guard models.isEmpty else { return true }
            defaultName = "claude"
            models = [
                LlmModelInfo(
                    name: "claude", provider: "anthropic", model: "claude-sonnet-5",
                    modelCandidates: ["claude-opus-4-8"], reasoningEffort: nil,
                    availableEfforts: ["low", "medium", "high", "xhigh", "max"],
                    liteModel: nil, baseUrl: nil, apiKeyEnv: nil, apiKeyConfigured: true,
                    contextWindowOverride: nil, effectiveContextWindow: 200_000,
                    supportsVisionOverride: nil, effectiveSupportsVision: true),
                LlmModelInfo(
                    name: "gpt", provider: "openai", model: "gpt-5.5",
                    modelCandidates: ["gpt-5.5-mini", "o3"], reasoningEffort: "xhigh",
                    availableEfforts: ["low", "medium", "high", "xhigh", "max"],
                    liteModel: "gpt-5.5-mini", baseUrl: "https://proxy.test/v1",
                    apiKeyEnv: "OPENAI_KEY", apiKeyConfigured: false,
                    contextWindowOverride: 400_000, effectiveContextWindow: 400_000,
                    supportsVisionOverride: false, effectiveSupportsVision: false),
            ]
            return true
        }
    #endif
}
