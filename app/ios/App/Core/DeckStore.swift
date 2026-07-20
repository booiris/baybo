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
        /// The grid sizes the card implements; the shell's ⤢ cycle stays in
        /// this set and a single entry hides the control. Optional so a
        /// pre-field mirror still decodes — normalized to `[size]` on the web
        /// side when absent/empty.
        let sizes: [String]?
        /// Whether the card declares a maximized layout (the ⛶ affordance).
        /// Optional for the same pre-field-mirror reason (defaults false).
        let maximize: Bool?
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

    /// The blob ref returned to a card from `deck.pickBlob` — serialized as the
    /// `pick_result.ref` the card SDK resolves with.
    struct PickRef: Encodable {
        let blobId: String
        let contentType: String
        let size: Int64
        /// Synthesized (`photo.<ext>`) — PhotosPicker exposes no filename.
        let name: String?
    }

    /// Mirrored from the shell's edit mode so the native header can show Done.
    @Published var editMode = false
    /// A card is maximized full-screen in the shell (driven by the ⛶ tap the
    /// shell reports over the bridge). Native hides the wordmark header while
    /// set so the maximized card owns the whole surface; the tab bar stays.
    @Published var maximized = false
    /// A delete waiting on the native confirm (destructive actions confirm
    /// natively; the shell only reports intent).
    @Published var pendingDelete: String?
    /// The recycle bin, most recently deleted first (server order).
    @Published private(set) var recycle: [RecycledCard] = []

    /// Drives the native photo picker's presentation (`deck.pickBlob`). The
    /// picker is a single shared surface — one pick at a time; a concurrent
    /// request is rejected `busy` rather than queued (a picker popping up
    /// seconds later reads as a ghost dialog).
    @Published var pickerActive = false
    /// The in-flight pick (id + card). Non-nil ⇒ a pick is presenting OR
    /// uploading — the slot stays busy across the whole flow so a second
    /// request is rejected `busy`, not accepted mid-upload. Cleared only when
    /// the pick settles (`settlePick`), never conditioned on delivery.
    private var activePick: (id: String, cardId: String)?
    /// True once a photo was chosen (upload underway). Distinguishes a
    /// dismissal-after-selection (not a cancel) from a dismissal-with-no-choice
    /// (a cancel) in `pickerDismissed`.
    private var pickChosen = false
    /// The last pick outcome, for tests (the real result rides the bridge).
    private(set) var lastPickResult: (id: String, ok: Bool, error: String?)?

    /// A blob materialized on disk and awaiting the system share sheet
    /// (`deck.shareBlob`). Presented by `DeckScreen`; cleared when dismissed.
    @Published var shareItem: FilePreview?

    private(set) var state = StatePayload(cards: [], snapshots: [])
    /// The chat session of an in-flight empty-board `/deck` creation (set by
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
        // A fresh shell (first boot, or a rebind that rebuilt the webview) has
        // no maximized card and never sends the corrective postMaximize(false),
        // so a `maximized` left true from a prior binding would strand the
        // header hidden forever. Maximize is transient view state — not
        // restored through InitPayload — so reset it to match the new shell.
        maximized = false
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

    /// The shell entered/left a card's maximized layout. Purely reflective —
    /// the shell owns the animation and card state; native reflects it in the
    /// header (the edit pill becomes a ✕).
    func setMaximized(_ active: Bool) {
        maximized = active
    }

    /// Header ✕ tapped: ask the shell to collapse the maximized card. The
    /// shell drives the animation and echoes `maximize=false` back.
    func requestRestore() {
        bridge?.restoreMaximized()
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
            Self.pruneBundleCache(keeping: Set(cards.map { $0.cardId }))
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
        // `spec_hash` is the content hash of the card's bundle files, so a
        // cached card.html tagged with the current hash IS the current code —
        // deliver it straight from disk (renders fully offline). Only a new
        // card or a spec change misses and needs the leg.
        let specHash = state.cards.first { $0.cardId == cardId }?.specHash
        if let specHash, let html = Self.cachedBundle(cardId: cardId, specHash: specHash) {
            bridge?.deliverBundle(cardId: cardId, cardHtml: html)
            return
        }
        Task { [weak self] in
            guard let self else { return }
            do {
                let html = try await self.client.deckFetchBundle(cardId: cardId)
                self.bridge?.deliverBundle(cardId: cardId, cardHtml: html)
                if let specHash {
                    Self.cacheBundle(cardId: cardId, specHash: specHash, html: html)
                }
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

    // MARK: pick (deck.pickBlob)

    /// A card asked to pick a blob. Present the picker unless one is already up
    /// (then reject `busy`). Every request terminates in exactly one
    /// resolve/reject via `settlePick`.
    func requestPick(id: String, cardId: String, accept: String?) {
        guard activePick == nil else {
            settlePick(id: id, ok: false, refJSON: nil, error: "busy")
            return
        }
        _ = accept  // advisory in v1 (photo library); reserved for file types.
        activePick = (id, cardId)
        pickerActive = true
    }

    /// A photo was chosen: mark the pick as chosen (so the dismissal that
    /// follows is a no-op, not a spurious cancel) and return the pending id —
    /// but KEEP the slot busy so a second request during the upload is rejected
    /// `busy`, not accepted. The caller (the `DeckScreen` view, which owns the
    /// `PhotosPickerItem`) loads the bytes and hands them to `finishPick`; the
    /// engine stays free of the PhotosUI type. `nil` if no pick is active.
    func consumePick() -> String? {
        guard let pick = activePick else { return nil }
        pickChosen = true
        return pick.id
    }

    /// The picker dismissed. Only a dismissal with NOTHING chosen is a cancel;
    /// a dismissal AFTER a selection (`pickChosen`) leaves the upload running.
    /// Deferred one runloop turn so a selection landing in the same tick (which
    /// sets `pickChosen` first) wins the race.
    func pickerDismissed() {
        DispatchQueue.main.async { [weak self] in
            guard let self, let pick = self.activePick, !self.pickChosen else { return }
            self.settlePick(id: pick.id, ok: false, refJSON: nil, error: "cancelled")
        }
    }

    /// Upload the loaded photo bytes and resolve the card's promise with a ref.
    /// `data == nil` means the transfer failed. Size-checked BEFORE upload.
    func finishPick(id: String, data: Data?, mime: String) {
        guard let data else {
            settlePick(id: id, ok: false, refJSON: nil, error: "load failed")
            return
        }
        guard data.count <= ChatStore.maxAttachmentBytes else {
            settlePick(id: id, ok: false, refJSON: nil, error: "too large")
            return
        }
        Task { [weak self] in
            guard let self else { return }
            do {
                let blobId = try await self.client.blobUploadBytes(bytes: data, mimeType: mime)
                let ref = PickRef(
                    blobId: blobId, contentType: mime, size: Int64(data.count),
                    name: Self.synthesizedName(mime: mime))
                if let json = try? JSONEncoder().encode(ref),
                    let text = String(data: json, encoding: .utf8)
                {
                    self.settlePick(id: id, ok: true, refJSON: text, error: nil)
                } else {
                    self.settlePick(id: id, ok: false, refJSON: nil, error: "encode failed")
                }
            } catch {
                self.settlePick(id: id, ok: false, refJSON: nil, error: bayboErrorText(error))
            }
        }
    }

    private func settlePick(id: String, ok: Bool, refJSON: String?, error: String?) {
        // Free the slot only when the id that settled IS the in-flight pick — a
        // `busy` rejection carries a DIFFERENT (later) id and must not clear the
        // pick it was rejected against.
        if activePick?.id == id {
            activePick = nil
            pickChosen = false
        }
        lastPickResult = (id, ok, error)
        bridge?.deliverPickResult(id: id, ok: ok, refJSON: refJSON, error: error)
    }

    private static func synthesizedName(mime: String) -> String {
        let ext: String
        switch mime {
        case "image/png": ext = "png"
        case "image/gif": ext = "gif"
        case "image/heic", "image/heif": ext = "heic"
        case "image/webp": ext = "webp"
        default: ext = "jpg"
        }
        return "photo.\(ext)"
    }

    // MARK: share (deck.shareBlob)

    /// Fetch a blob (cache-first) and hand it to the system share sheet under a
    /// real filename, so Save-to-Photos / Files / AirDrop keep the encoding.
    /// Silently no-ops if the bytes can't be fetched (offline miss).
    func requestShare(blobId: String, filename: String?, contentType: String?) {
        Task { [weak self] in
            guard let self else { return }
            var data = await self.client.blobReadCached(blobId: blobId)
            if data == nil {
                data = try? await self.client.blobDownloadBytes(blobId: blobId, progress: nil)
            }
            guard let data else {
                NSLog("deck: share fetch failed for %@", blobId)
                return
            }
            let name = Self.shareFilename(filename: filename, contentType: contentType)
            guard let url = Self.writeShareFile(blobId: blobId, name: name, data: data) else {
                return
            }
            self.shareItem = FilePreview(url: url)
        }
    }

    private static func shareFilename(filename: String?, contentType: String?) -> String {
        if let filename, !filename.isEmpty {
            // A filename never contains a path separator on the wire; guard anyway.
            return filename.replacingOccurrences(of: "/", with: "_")
        }
        let ext = Self.shareExt(contentType)
        return ext.isEmpty ? "attachment" : "attachment.\(ext)"
    }

    private static func shareExt(_ mime: String?) -> String {
        switch mime {
        case "image/png": return "png"
        case "image/jpeg": return "jpg"
        case "image/gif": return "gif"
        case "image/webp": return "webp"
        case "image/heic", "image/heif": return "heic"
        case "application/pdf": return "pdf"
        case "text/csv": return "csv"
        case "text/plain": return "txt"
        case "application/json": return "json"
        case "video/mp4": return "mp4"
        case "audio/mpeg": return "mp3"
        default: return ""
        }
    }

    /// `<tmp>/baybo-deck-share/<blob digest>/<filename>` — the digest dir keeps
    /// two blobs sharing a filename apart and lets a re-share reuse the file.
    private static func writeShareFile(blobId: String, name: String, data: Data) -> URL? {
        let hex = blobId.drop(while: { $0 != ":" }).dropFirst().prefix(while: { $0 != "." })
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-deck-share", isDirectory: true)
            .appendingPathComponent(hex.isEmpty ? "blob" : String(hex), isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let url = dir.appendingPathComponent(name)
            if !FileManager.default.fileExists(atPath: url.path) {
                try data.write(to: url, options: .atomic)
            }
            return url
        } catch {
            NSLog("deck: share materialize failed: %@", String(describing: error))
            return nil
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
        // Bundles belong to the departing gateway too.
        try? FileManager.default.removeItem(at: bundleCacheDir)
    }

    // MARK: bundle cache
    //
    // Each card's `card.html` cached under its `spec_hash` so a card the device
    // has rendered before paints fully offline on a cold start (the snapshot
    // DATA is already in the `deck.json` mirror; this holds the rendering CODE).
    // spec_hash is the content hash of the bundle, so a hash match is a
    // byte-identical hit and a spec change is a clean miss. Bounded: pruned to
    // the live card set on refresh, wiped on logout/rebind.

    private struct CachedBundle: Codable {
        let specHash: String
        let html: String
    }

    private static var bundleCacheDir: URL {
        SessionIndex.supportDirectory().appendingPathComponent("deck-bundles", isDirectory: true)
    }

    private static func bundleFileURL(_ cardId: String) -> URL {
        bundleCacheDir.appendingPathComponent(cardId + ".json")
    }

    private static func cachedBundle(cardId: String, specHash: String) -> String? {
        guard let data = try? Data(contentsOf: bundleFileURL(cardId)),
            let entry = try? JSONDecoder().decode(CachedBundle.self, from: data),
            entry.specHash == specHash
        else { return nil }
        return entry.html
    }

    private static func cacheBundle(cardId: String, specHash: String, html: String) {
        try? FileManager.default.createDirectory(
            at: bundleCacheDir, withIntermediateDirectories: true)
        guard let data = try? JSONEncoder().encode(CachedBundle(specHash: specHash, html: html))
        else { return }
        try? data.write(to: bundleFileURL(cardId), options: .atomic)
    }

    /// Drop cached bundles for cards no longer live (deleted here or elsewhere).
    private static func pruneBundleCache(keeping ids: Set<String>) {
        guard
            let files = try? FileManager.default.contentsOfDirectory(
                at: bundleCacheDir, includingPropertiesForKeys: nil)
        else { return }
        for file in files where file.pathExtension == "json" {
            if !ids.contains(file.deletingPathExtension().lastPathComponent) {
                try? FileManager.default.removeItem(at: file)
            }
        }
    }

    // MARK: FFI mapping

    private static func card(from info: DeckCardInfo) -> Card {
        Card(
            cardId: info.cardId,
            title: info.title,
            position: info.position,
            size: info.size,
            sizes: info.sizes,
            maximize: info.maximize,
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
