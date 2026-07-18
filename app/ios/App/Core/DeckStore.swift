import Foundation

/// The Deck tab's engine: transport (FFI over the active leg), the local
/// mirror for instant offline paint, live-push handling with the
/// accept-if-greater seq rule, and optimistic layout staging with baseline
/// rollback (the `SessionIndex` mutation idiom, deck-shaped).
///
/// Render state lives in the deck shell (webview); this store is the shell's
/// single data source over `DeckBridge`.
@MainActor
final class DeckStore: ObservableObject {
    struct Card: Codable, Equatable {
        let cardId: String
        let title: String
        var position: Int64
        var size: String
        let enabled: Bool
        let quarantined: Bool
        let specHash: String
        let lastSeq: Int64
        /// Ops declared replay-safe (`x-baybo-retryable: true`) — the per-op
        /// verdict passed into `deckCall` so the relay leg knows whether a
        /// silent-death call may be replayed. Optional so a pre-field mirror
        /// still decodes (treated as no-retry until the next refresh).
        let retryableOps: [String]?
    }

    struct Snapshot: Codable, Equatable {
        let cardId: String
        let seq: Int64
        let payload: String
        let error: String?
    }

    struct StatePayload: Codable, Equatable {
        var cards: [Card]
        var snapshots: [Snapshot]
    }

    struct InitPayload: Encodable {
        let cards: [Card]
        let snapshots: [Snapshot]
        let lang: String
        let editMode: Bool
        let setupInflight: Bool
    }

    /// One soft-deleted card in the recycle bin. Not persisted — the bin is
    /// fetched fresh on entry (it changes from any client, and staleness here
    /// costs nothing).
    struct RecycledCard: Equatable, Identifiable {
        let cardId: String
        let title: String
        let size: String
        let deletedAtMs: Int64?
        var id: String { cardId }
    }

    /// Mirrored from the shell's edit mode so the native header can show Done.
    @Published var editMode = false
    /// A delete waiting on the native confirm (destructive actions confirm
    /// natively; the shell only reports intent).
    @Published var pendingDelete: String?
    /// The recycle bin, most recently deleted first (server order).
    @Published private(set) var recycle: [RecycledCard] = []

    private(set) var state = StatePayload(cards: [], snapshots: [])
    /// The chat session of an in-flight empty-board `/card` creation (set by
    /// `AppStore.startCardDraft`, cleared when a card actually lands). While
    /// set, the empty-board CTA shows an in-flight state and a re-tap returns
    /// to this chat instead of starting a new one.
    private(set) var setupSessionId: String?
    weak var bridge: DeckBridge?

    /// Lazily resolved so constructing the store (an `AppStore` stored
    /// property) never boots the FFI client under test — the client only
    /// materializes on the first actual transport call.
    private lazy var client: any BayboClientProtocol = clientProvider()
    private let clientProvider: () -> any BayboClientProtocol
    /// Server-acked baseline for layout rollback (restore the BASELINE on
    /// failure, never the negation).
    private var layoutBaseline: [Card]?
    private var refreshTask: Task<Void, Never>?
    /// In-flight mutation tasks, held so tests can await completion.
    private(set) var layoutTask: Task<Void, Never>?
    private(set) var actionTask: Task<Void, Never>?

    private static var mirrorURL: URL {
        SessionIndex.supportDirectory().appendingPathComponent("deck.json")
    }

    init(clientProvider: @escaping () -> any BayboClientProtocol = { Baybo.client }) {
        self.clientProvider = clientProvider
        if let data = try? Data(contentsOf: Self.mirrorURL),
            let cached = try? JSONDecoder().decode(StatePayload.self, from: data)
        {
            state = cached
        }
    }

    // MARK: bridge lifecycle

    func bridgeBecameReady() {
        bridge?.deliverInit(
            InitPayload(
                cards: state.cards,
                snapshots: state.snapshots,
                lang: Lang.shared.code,
                editMode: editMode,
                setupInflight: setupSessionId != nil
            ))
        requestRefresh()
    }

    func setEditMode(_ active: Bool) {
        editMode = active
        bridge?.setEditMode(active)
    }

    /// Set (or clear, with nil) the in-flight card-setup session and reflect
    /// it on the empty-board CTA. Called by `AppStore.startCardDraft` on
    /// start; cleared here once a card lands (`refreshNow`).
    func setSetupSession(_ id: String?) {
        guard setupSessionId != id else { return }
        setupSessionId = id
        bridge?.setSetupInflight(id != nil)
    }

    // MARK: refresh + live pushes

    func requestRefresh() {
        guard refreshTask == nil else { return }
        refreshTask = Task { [weak self] in
            await self?.refreshNow()
            self?.refreshTask = nil
        }
    }

    func refreshNow() async {
        do {
            let view = try await client.deckFetch()
            let cards = view.cards.map(Self.card(from:))
            let snapshots = view.snapshots.map(Self.snapshot(from:))
            state = StatePayload(cards: cards, snapshots: snapshots)
            persist()
            bridge?.deliverState(state)
            // A card landed → the empty-board setup finished; drop the
            // in-flight session so the CTA returns to its normal state.
            if !cards.isEmpty {
                setSetupSession(nil)
            }
        } catch {
            NSLog("deck: refresh failed: %@", String(describing: error))
        }
    }

    /// Live push: accept iff seq is strictly greater than the cached seq (the
    /// shell applies the same rule; the mirror must agree with what is shown).
    func handleCardData(cardId: String, seq: Int64, payload: String) {
        guard state.cards.contains(where: { $0.cardId == cardId }) else { return }
        if let cached = state.snapshots.first(where: { $0.cardId == cardId }),
            seq <= cached.seq
        {
            return
        }
        state.snapshots.removeAll { $0.cardId == cardId }
        state.snapshots.append(Snapshot(cardId: cardId, seq: seq, payload: payload, error: nil))
        persist()
        bridge?.deliverCardData(cardId: cardId, seq: seq, payload: payload)
    }

    func handleDeckChanged() {
        requestRefresh()
    }

    // MARK: shell requests

    func requestBundle(cardId: String) {
        Task { [weak self] in
            guard let self else { return }
            do {
                let html = try await self.client.deckFetchBundle(cardId: cardId)
                self.bridge?.deliverBundle(cardId: cardId, cardHtml: html)
            } catch {
                NSLog("deck: bundle fetch failed for %@: %@", cardId, String(describing: error))
            }
        }
    }

    func requestCall(id: String, cardId: String, op: String, params: Any?) {
        let paramsJSON: String
        if let params, JSONSerialization.isValidJSONObject(params),
            let data = try? JSONSerialization.data(withJSONObject: params),
            let text = String(data: data, encoding: .utf8)
        {
            paramsJSON = text
        } else {
            paramsJSON = "{}"
        }
        // The op's mandatory `x-baybo-retryable` verdict, off the deck view:
        // it decides whether the relay leg may replay a silent-death call.
        let retryable =
            state.cards.first { $0.cardId == cardId }?.retryableOps?.contains(op) ?? false
        Task { [weak self] in
            guard let self else { return }
            do {
                let value = try await self.client.deckCall(
                    cardId: cardId, op: op, paramsJson: paramsJSON, retryable: retryable)
                self.bridge?.deliverCallResult(id: id, ok: true, valueJSON: value, error: nil)
            } catch {
                self.bridge?.deliverCallResult(
                    id: id, ok: false, valueJSON: nil, error: String(describing: error))
            }
        }
    }

    /// Optimistic layout: apply locally at once, PUT, roll back to the
    /// server-acked baseline on failure.
    func requestLayout(entries: [[String: Any]]) {
        if layoutBaseline == nil {
            layoutBaseline = state.cards
        }
        var bySize: [String: (Int64, String)] = [:]
        var inputs: [DeckLayoutEntryInput] = []
        for entry in entries {
            guard let cardId = entry["cardId"] as? String,
                let position = entry["position"] as? Int,
                let size = entry["size"] as? String
            else { continue }
            bySize[cardId] = (Int64(position), size)
            inputs.append(
                DeckLayoutEntryInput(cardId: cardId, position: Int64(position), size: size))
        }
        state.cards = state.cards.map { card in
            var card = card
            if let (position, size) = bySize[card.cardId] {
                card.position = position
                card.size = size
            }
            return card
        }.sorted { $0.position < $1.position }
        persist()

        layoutTask = Task { [weak self] in
            guard let self else { return }
            do {
                try await self.client.deckSetLayout(entries: inputs)
                self.layoutBaseline = nil
            } catch {
                NSLog("deck: layout put failed, rolling back: %@", String(describing: error))
                if let baseline = self.layoutBaseline {
                    self.layoutBaseline = nil
                    self.state.cards = baseline
                    self.persist()
                    self.bridge?.deliverState(self.state)
                }
                self.requestRefresh()
            }
            self.layoutTask = nil
        }
    }

    func requestCardAction(cardId: String, action: String) {
        switch action {
        case "delete":
            // Destructive: confirm natively before anything happens.
            pendingDelete = cardId
        case "enable", "disable":
            actionTask = Task { [weak self] in
                guard let self else { return }
                do {
                    try await self.client.deckSetEnabled(
                        cardId: cardId, enabled: action == "enable")
                } catch {
                    NSLog("deck: %@ failed: %@", action, String(describing: error))
                }
                await self.refreshNow()
                self.actionTask = nil
            }
        default:
            break
        }
    }

    func confirmPendingDelete() {
        guard let cardId = pendingDelete else { return }
        pendingDelete = nil
        actionTask = Task { [weak self] in
            guard let self else { return }
            do {
                try await self.client.deckDelete(cardId: cardId)
            } catch {
                NSLog("deck: delete failed: %@", String(describing: error))
            }
            await self.refreshNow()
            self.actionTask = nil
        }
    }

    // MARK: recycle bin

    func fetchRecycleNow() async {
        do {
            recycle = try await client.deckFetchRecycle().map {
                RecycledCard(
                    cardId: $0.cardId,
                    title: $0.title,
                    size: $0.size,
                    deletedAtMs: $0.deletedAtMs
                )
            }
        } catch {
            NSLog("deck: recycle fetch failed: %@", String(describing: error))
        }
    }

    /// Restore one card out of the bin. OPTIMISTIC: the row leaves the
    /// list at the tap — the gateway's restore is slow by design (it
    /// re-runs the dry-run admission gate: boot the service, invoke the
    /// refresh op once, vet the snapshot — a deleted card is never
    /// resurrected unvetted), and waiting on it left the row sitting
    /// there for seconds. On failure the row comes back (the mutation
    /// idiom: roll back to the baseline, never the negation).
    func restore(cardId: String) {
        guard let index = recycle.firstIndex(where: { $0.cardId == cardId }) else { return }
        let removed = recycle.remove(at: index)
        actionTask = Task { [weak self] in
            guard let self else { return }
            do {
                _ = try await self.client.deckRestore(cardId: cardId)
            } catch {
                NSLog("deck: restore failed: %@", String(describing: error))
                if !self.recycle.contains(where: { $0.cardId == cardId }) {
                    self.recycle.insert(removed, at: min(index, self.recycle.count))
                }
            }
            await self.refreshNow()
            self.actionTask = nil
        }
    }

    // MARK: persistence

    private func persist() {
        guard let data = try? JSONEncoder().encode(state) else { return }
        try? data.write(to: Self.mirrorURL, options: .atomic)
    }

    static func removeMirror() {
        try? FileManager.default.removeItem(at: mirrorURL)
    }

    // MARK: FFI mapping

    private static func card(from info: DeckCardInfo) -> Card {
        Card(
            cardId: info.cardId,
            title: info.title,
            position: info.position,
            size: info.size,
            enabled: info.enabled,
            quarantined: info.quarantined,
            specHash: info.specHash,
            lastSeq: info.lastSeq,
            retryableOps: info.retryableOps
        )
    }

    private static func snapshot(from info: DeckSnapshotInfo) -> Snapshot {
        Snapshot(
            cardId: info.cardId,
            seq: info.seq,
            payload: info.payload,
            error: info.error
        )
    }
}

/// Connection-global deck sink relay (the `SessionActivityHandler` idiom —
/// deliberately not named `DeckSinkImpl`, which UniFFI generates). Hops each
/// callback to the main actor and forwards to the app's `DeckStore`.
final class DeckEventsRelay: DeckSink {
    private let store: @MainActor () -> DeckStore?

    init(store: @escaping @MainActor () -> DeckStore?) {
        self.store = store
    }

    func onCardData(cardId: String, seq: UInt32, payload: String) {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                store()?.handleCardData(cardId: cardId, seq: Int64(seq), payload: payload)
            }
        }
    }

    func onDeckChanged() {
        let store = self.store
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                store()?.handleDeckChanged()
            }
        }
    }
}
