import Foundation

/// The gateway's configured LLM entries + the current `default-llm` name —
/// the chat header model picker's catalog (`GET /v1/llm/models` over the
/// active leg). Global, not per-session: one fetch serves every chat header.
///
/// Fetched lazily on chat open and cached for the app run — the entry set
/// changes when the operator edits the gateway config, not mid-conversation;
/// a failed fetch just retries on the next open. A `models.json` MIRROR (the
/// `deck.json` idiom) makes the catalog cold-start-offline durable: the pill
/// and panel paint from disk with zero network, and the next successful fetch
/// rewrites it. Reset on logout/rebind — the catalog (and its mirror) belong
/// to the gateway that served them.
@MainActor
final class ModelCatalog: ObservableObject {
    static let shared = ModelCatalog()

    /// The `default-llm` entry name — what an unpinned session resolves
    /// against, and therefore the pill's label when `ChatStore.modelPin` is nil.
    @Published private(set) var defaultName: String?
    @Published private(set) var models: [LlmModelInfo] = []

    private let client: any BayboClientProtocol
    private let mirrorURL: URL
    private var fetchTask: Task<Void, Never>?
    /// Bumped on `reset` so a fetch that straddles a logout can't repopulate
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

    /// Logout/rebind: drop the departed gateway's entries, mirror included.
    func reset() {
        epoch += 1
        fetchTask?.cancel()
        fetchTask = nil
        fetchedThisRun = false
        defaultName = nil
        models = []
        try? FileManager.default.removeItem(at: mirrorURL)
    }

    /// The entry's reasoning-effort override; `nil` = provider default.
    func reasoningEffort(of name: String) -> String? {
        models.first { $0.name == name }?.reasoningEffort
    }

    /// The entry the session resolves against: `pin` when it names a cached
    /// entry, else the `default-llm` entry. `nil` while the catalog is empty
    /// or when the pin names an entry the catalog no longer has (the caller
    /// falls back to the raw pin string for display).
    func effectiveEntry(pin: String?) -> LlmModelInfo? {
        let name = pin ?? defaultName
        return models.first { $0.name == name }
    }

    /// The models an entry can be pinned to — its default `model` first, then
    /// each `model_candidates` id, de-duped. What the picker lists under the
    /// entry.
    func models(of entry: LlmModelInfo) -> [String] {
        var seen = Set<String>()
        return ([entry.model] + entry.modelCandidates).filter { seen.insert($0).inserted }
    }

    // MARK: - Mirror (`models.json` — a pure cache, never a source of truth)

    /// Codable twin of the uniffi records (which aren't Codable themselves).
    private struct Mirror: Codable {
        var defaultName: String?
        var models: [Entry]

        struct Entry: Codable {
            var name: String
            var provider: String
            var model: String
            var modelCandidates: [String]
            var reasoningEffort: String?
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
                modelCandidates: $0.modelCandidates, reasoningEffort: $0.reasoningEffort)
        }
    }

    private func saveMirror() {
        let mirror = Mirror(
            defaultName: defaultName,
            models: models.map {
                Mirror.Entry(
                    name: $0.name, provider: $0.provider, model: $0.model,
                    modelCandidates: $0.modelCandidates, reasoningEffort: $0.reasoningEffort)
            })
        guard let data = try? JSONEncoder().encode(mirror) else { return }
        try? data.write(to: mirrorURL, options: .atomic)
    }

    #if DEBUG
        /// `-baybo-demo-models`: seed a canned catalog so the header's model
        /// pill renders headlessly (no gateway to list from). Never persisted:
        /// a later plain launch on the same simulator must not inherit it.
        private func seedDemoIfRequested() -> Bool {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-models") else {
                return false
            }
            guard models.isEmpty else { return true }
            defaultName = "claude"
            // The `gpt` entry carries model_candidates so the entry→models
            // level is exercisable headlessly; efforts cover set and unset.
            models = [
                LlmModelInfo(
                    name: "claude", provider: "anthropic", model: "claude-sonnet-5",
                    modelCandidates: ["claude-opus-4-8"], reasoningEffort: nil),
                LlmModelInfo(
                    name: "gpt", provider: "openai", model: "gpt-5.5",
                    modelCandidates: ["gpt-5.5-mini", "o3"], reasoningEffort: "xhigh"),
            ]
            return true
        }
    #endif
}
