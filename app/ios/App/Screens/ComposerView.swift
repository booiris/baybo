import ImageIO
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// The native composer dock: autosizing field, attachment staging behind the
/// inline `+` menu (Photos / Files, plus Paste when the clipboard holds an
/// image), in-field send. The strip itself and every piece of work reading it
/// live in `ComposerStaging`; this view renders them.
/// Interaction contract preserved from the web composer:
/// * staged picks count as a draft, so an attachment-only send works with an
///   empty field;
/// * send is gated while any staged item is still on its way to the gateway
///   (`waitingUpload` notice) — a pick takes its slot in the strip the moment
///   it is admitted, so the gate sees one whose bytes are still loading too —
///   and blocked outright while one FAILED to upload
///   (`removeFailedAttachment`) — it carries no blob, and shipping the message
///   without it would drop the user's pick in silence. A failed tile is
///   individually retryable by tapping it: with multi-select, delete-and-repick
///   is not a cure.
/// * picks over 100 MiB are rejected up front (`tooLarge`), matching the
///   gateway's blob cap, and the strip holds at most
///   `ChatStore.maxStagedAttachments`.
struct ComposerView: View {
    @ObservedObject var store: ChatStore
    /// The SESSION's strip, not this frame's: the dock lives in `ChatScreen`'s
    /// `.safeAreaInset`, which every `fullScreenCover` over the chat (image
    /// viewer, video player) tears down and puts back.
    @ObservedObject private var staging: ComposerStaging
    @ObservedObject private var lang = Lang.shared
    /// The `+`'s panel, owned by `ChatScreen` — it floats over the TRANSCRIPT
    /// and over this dock's own rows, so the screen is what presents it (as an
    /// overlay on the dock). This side reports the anchor and answers the pick.
    @ObservedObject private var attach: AttachMenu
    @State private var text = ""
    @State private var photoPicks: [PhotosPickerItem] = []
    @FocusState private var focused: Bool

    init(store: ChatStore, attach: AttachMenu) {
        _store = ObservedObject(wrappedValue: store)
        _staging = ObservedObject(wrappedValue: store.staging)
        _attach = ObservedObject(wrappedValue: attach)
    }

    private static let inputHitSlop: CGFloat = 10
    private static let plusSize = CGSize(width: 46, height: 48)
    private static let tileSide: CGFloat = 64
    /// A file pill is the image thumbnail's height and wide enough for a
    /// middle-truncated name over its size line.
    private static let fileTileWidth: CGFloat = 176
    private static let tileCorner: CGFloat = 10

    private var hasDraft: Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !staging.staged.isEmpty
    }

    var body: some View {
        VStack(spacing: 8) {
            if let notice = store.notice {
                HStack {
                    Text(verbatim: notice)
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.err)
                    Spacer()
                    Button {
                        store.notice = nil
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.inkSoft)
                    }
                }
                .padding(.horizontal, 18)
            }

            // A blocked tool call outranks everything else in the dock: it is
            // the only thing the user can act on, and an unanswered gate denies
            // itself after 5 minutes.
            if let approval = store.pendingApprovals.first {
                ApprovalCardView(
                    approval: approval,
                    queued: store.pendingApprovals.count - 1
                ) { decision in
                    store.resolveApproval(approval, decision: decision)
                }
                // Re-key on the prompt so answering the head SWAPS in the next
                // card (fresh one-shot latch) instead of reusing this view.
                .id(approval.callId)
            }

            if !staging.staged.isEmpty {
                stagedStrip
            }

            // One ChatGPT-style pill: inline plus on the left, in-field send
            // on the right — no satellite icon circles.
            HStack(alignment: .bottom, spacing: 4) {
                plusButton

                // 13pt vertical padding makes the single-line field exactly
                // the row's 48pt (17pt body ≈ 22pt line), so the cursor sits
                // vertically centered despite the .bottom stack alignment;
                // extra lines still grow upward.
                TextField(lang.t("chat.placeholder"), text: $text, axis: .vertical)
                    .lineLimit(1...6)
                    .font(.system(size: 17))
                    .focused($focused)
                    .padding(.vertical, 13)
                    .background {
                        Color.clear
                            .padding(-Self.inputHitSlop)
                            .contentShape(Rectangle())
                            .onTapGesture {
                                focused = true
                            }
                    }

                // While a turn runs the button is a stop control (cancel via
                // `/stop`), independent of the field: typing can't turn it back
                // into a send button mid-turn (interjection is future work). Idle,
                // it's the send button, enabled only with a draft.
                Button {
                    if store.agentRunning {
                        store.stopAgent()
                    } else {
                        send()
                    }
                } label: {
                    Circle()
                        .fill(store.agentRunning || hasDraft ? Theme.ink : Theme.line)
                        .frame(width: 36, height: 36)
                        .overlay(
                            // A filled square is the "stop generating" affordance
                            // (ChatGPT-style); the up arrow is send. One Image so
                            // the glyph morphs between the two states.
                            Image(systemName: store.agentRunning ? "stop.fill" : "arrow.up")
                                .font(.system(size: store.agentRunning ? 13 : 15, weight: .semibold))
                                .foregroundStyle(Theme.paper)
                                .contentTransition(.symbolEffect(.replace))
                        )
                        .animation(.easeInOut(duration: 0.2), value: store.agentRunning)
                }
                .disabled(!store.agentRunning && !hasDraft)
                .accessibilityLabel(
                    Text(verbatim: Lang.shared.t(store.agentRunning ? "chat.stop" : "chat.send")))
                .padding(.trailing, 6)
                .padding(.bottom, 6)
            }
            .frame(minHeight: 48)
            // Borderless pill (ChatGPT-style): over the thread's blank white
            // at-rest strip the untinted glass is nearly invisible, so a soft
            // ambient shadow carries the boundary instead of a hairline.
            .glassSurface(tint: Theme.paper.opacity(0.25), in: .rect(cornerRadius: 24))
            .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
            // At rest the pill holds a moderate width; focus stretches it out
            // toward the screen edges — a small gutter stays — on the
            // keyboard's beat. The notice/staged rows keep their own gutters.
            .padding(.horizontal, focused ? 14 : 40)
            .animation(.easeOut(duration: 0.25), value: focused)
        }
        .padding(.top, 0)
        .padding(.bottom, dockBottomPadding)
        .animation(.easeOut(duration: 0.2), value: store.pendingApprovals.first?.callId)
        .background { composerVeil }
        .photosPicker(
            isPresented: pickerBinding(.photos),
            selection: $photoPicks,
            maxSelectionCount: max(1, ChatStore.maxStagedAttachments - staging.staged.count),
            matching: .images
        )
        // No type restriction: whatever the model can be handed, the user can
        // attach — the mime the extension implies is what decides server-side
        // whether it is readable at all.
        .fileImporter(
            isPresented: pickerBinding(.files),
            allowedContentTypes: [.data],
            allowsMultipleSelection: true
        ) { result in
            switch result {
            case .success(let urls):
                staging.stage(files: urls)
            case .failure:
                staging.notePickerFailed()
            }
        }
        .onChange(of: photoPicks) { _, picks in
            guard !picks.isEmpty else { return }
            photoPicks = []
            staging.stage(photos: picks)
        }
        // Paste is the one row with no picker to answer it, so `pickerBinding`
        // cannot serve it: that binding retires `attach.pick` on a sheet's
        // DISMISSAL, and a second tap on a row whose `pick` is already set
        // publishes no change at all — the row would work exactly once. Clearing
        // the request here, in the same turn, is what keeps it live.
        .onChange(of: attach.pick) { _, pick in
            guard pick == .paste else { return }
            attach.pick = nil
            staging.stagePasteboard()
        }
        .onAppear {
            // One-shot for the Deck "Quick setup": seed the /deck request and
            // send it immediately, so the user lands in the conversation
            // already working. Clear `initialDraft` FIRST so a re-appear can't
            // double-send.
            if let seed = store.initialDraft, text.isEmpty {
                store.initialDraft = nil
                text = seed
                send()
            }
        }
        #if DEBUG
            // `-baybo-demo-keyboard`: raise then drop the keyboard without a
            // tap, so the keyboard-tracking transcript slide is recordable
            // headlessly on a simulator (pair with -baybo-open-chat).
            .onAppear {
                guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-keyboard") else {
                    return
                }
                Task { @MainActor in
                    try? await Task.sleep(for: .seconds(2))
                    focused = true
                    try? await Task.sleep(for: .seconds(3))
                    focused = false
                }
            }
        #endif
    }

    /// Quadratic ease-out (`1 − (1−t)²` at t = 0, 0.2 … 1.0): the fade rises
    /// fast then levels off. It spans the dock itself: alpha 0 at the dock's
    /// top edge down to the peak at the PILL'S BOTTOM edge, so peak opacity
    /// is reached only under the pill and scrolled content ghosts past the
    /// pill's flanks; only the strip below the pill (bottom padding +
    /// home-indicator area) is solid.
    private static let veilPeakAlpha = 0.8
    private static let veilTailAlphas: [Double] = [0.0, 0.36, 0.64, 0.84, 0.96, 1.0]

    /// The dock's gap under the pill — also where the veil turns solid.
    private var dockBottomPadding: CGFloat { focused ? 12 : 0 }

    /// Bottom mirror of the header veil, replacing the old opaque paper dock.
    /// Nothing overhangs the dock, and the whole veil hit-tests: gutter taps
    /// must not fall through and scroll the webview. The solid tail finishes
    /// the home-indicator strip and masks the web-vs-native inset animation
    /// phase mismatch; the notice/staged rows sit in the fade, backed by the
    /// thread's blank at-rest strip.
    private var composerVeil: some View {
        GeometryReader { geo in
            VStack(spacing: 0) {
                LinearGradient(
                    stops: Self.veilTailAlphas.enumerated().map { idx, alpha in
                        .init(
                            color: Theme.paper.opacity(alpha * Self.veilPeakAlpha),
                            location: CGFloat(idx) / CGFloat(Self.veilTailAlphas.count - 1)
                        )
                    },
                    startPoint: .top,
                    endPoint: .bottom
                )
                .frame(height: geo.size.height - dockBottomPadding)
                Theme.paper.opacity(Self.veilPeakAlpha)
                    .frame(height: dockBottomPadding + geo.safeAreaInsets.bottom)
            }
        }
    }

    /// The inline `+`, at its own 46×48 frame. It toggles `AttachMenuPanel`
    /// (hand-rolled, overlaid on the dock by `ChatScreen`) rather than opening a
    /// stock `Menu`, which would dim the screen and lift this pill out of the
    /// dock.
    ///
    /// The button reports its own frame in the DOCK's coordinate space, which
    /// is the column the panel blooms from: the pill's horizontal padding
    /// animates between at-rest and focused, so there is no fixed coordinate to
    /// hardcode. Not `.global` — the panel is laid out in the dock's space, and
    /// a frame measured in another container is a rendering that disagrees with
    /// its own touch region.
    private var plusButton: some View {
        Button {
            Haptics.tap()
            withAnimation(AttachMenuPanel.fade) {
                attach.toggle(pasteReady: staging.pasteReady)
            }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 22, weight: .light))
                .foregroundStyle(Theme.ink)
                .frame(width: Self.plusSize.width, height: Self.plusSize.height)
                .contentShape(Rectangle())
        }
        .onGeometryChange(for: CGRect.self) { proxy in
            proxy.frame(in: .named(AttachMenuPanel.dockSpace))
        } action: { frame in
            attach.report(anchor: frame)
        }
        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.addImage")))
    }

    /// The panel's pick, as the picker that answers it sees it. The panel is
    /// presented by `ChatScreen` (it floats over the dock) while the
    /// pickers stay HERE — their selection cap reads the strip's free slots and
    /// their results feed `staging` — so only the row tap travels down, and
    /// each picker clears the request as it dismisses.
    private func pickerBinding(_ source: AttachSource) -> Binding<Bool> {
        Binding(
            get: { attach.pick == source },
            set: { presented in
                guard !presented, attach.pick == source else { return }
                attach.pick = nil
            })
    }

    private var stagedStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(staging.staged) { item in
                    ZStack(alignment: .topTrailing) {
                        stagedTile(item)

                        Button {
                            staging.remove(item.id)
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 16))
                                .foregroundStyle(Theme.ink)
                                .background(Circle().fill(Theme.paper))
                        }
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.remove")))
                        .offset(x: 6, y: -6)
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 6)
        }
    }

    @ViewBuilder
    private func stagedTile(_ item: StagedAttachment) -> some View {
        Group {
            switch item.preview {
            case .image(let image):
                // No thumbnail until the pick's bytes land: the tile is on
                // screen from the moment the pick is admitted, which is what
                // makes the send gate see it.
                Group {
                    if let image {
                        Image(uiImage: image)
                            .resizable()
                            .scaledToFill()
                    } else {
                        Theme.surface
                    }
                }
                .frame(width: Self.tileSide, height: Self.tileSide)
                .opacity(item.state.isPending ? 0.5 : 1)
                .overlay {
                    if item.state.isPending {
                        ProgressView()
                    } else if item.state.isError {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 18, weight: .light))
                            .foregroundStyle(Theme.err)
                    }
                }
            case .file(let name, let mime):
                fileTile(item, name: name, mime: mime)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: Self.tileCorner))
        .overlay(
            RoundedRectangle(cornerRadius: Self.tileCorner)
                .strokeBorder(item.state.isError ? Theme.err : Theme.line, lineWidth: 1)
        )
        .contentShape(RoundedRectangle(cornerRadius: Self.tileCorner))
        .onTapGesture {
            guard item.state.isError else { return }
            staging.retry(item.id)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(verbatim: tileLabel(item)))
        .accessibilityValue(Text(verbatim: metaText(item)))
    }

    /// A file pill at the image thumbnail's height: glyph, middle-truncated
    /// name (the extension has to stay visible — the mime it implies is what
    /// decides whether the model can read the file at all), size or live upload
    /// counter under it.
    private func fileTile(_ item: StagedAttachment, name: String, mime: String) -> some View {
        HStack(spacing: 8) {
            Group {
                if item.state.isPending {
                    ProgressView()
                } else {
                    Image(
                        systemName: item.state.isError
                            ? "arrow.clockwise" : StagedAttachment.glyph(forMime: mime)
                    )
                    .font(.system(size: 18, weight: .light))
                    .foregroundStyle(item.state.isError ? Theme.err : Theme.ink)
                }
            }
            .frame(width: 22)

            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: name)
                    .font(Theme.sys(13))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(verbatim: metaText(item))
                    .font(Theme.mono(11))
                    .foregroundStyle(item.state.isError ? Theme.err : Theme.inkSoft)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .frame(width: Self.fileTileWidth, height: Self.tileSide, alignment: .leading)
        .background(Theme.surface)
    }

    private func tileLabel(_ item: StagedAttachment) -> String {
        if case .file(let name, _) = item.preview { return name }
        return Lang.shared.t("chat.stagedImage")
    }

    /// The tile's second line: bytes uploaded of the total while it streams,
    /// the total once it lands, the retry affordance when it failed. Mirrors
    /// how a DOWNLOAD presents progress (indeterminate spinner, byte counter
    /// beside it as the real progress).
    private func metaText(_ item: StagedAttachment) -> String {
        switch item.state {
        case .queued, .uploading:
            let sent = StagedAttachment.byteText(item.state.sentBytes)
            let total = StagedAttachment.byteText(UInt64(item.byteCount))
            return "\(sent) / \(total)"
        case .ready:
            return StagedAttachment.byteText(UInt64(item.byteCount))
        case .error:
            return Lang.shared.t("chat.retryUpload")
        }
    }

    private func send() {
        // A pick whose upload failed has no blob to reference, so it can't ride
        // the send — and dropping it silently ships the message MINUS the file
        // the user attached, with nothing to say so. Block on it instead: the
        // tile retries on tap, and its ✕ is right there.
        if let blocker = StagedAttachment.blocker(staging.staged) {
            staging.noteBlocked(blocker)
            return
        }
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let refs = staging.staged.compactMap(\.attachmentRef)
        guard !body.isEmpty || !refs.isEmpty else { return }
        // Guard the write: an unconditional `store.notice = nil` publishes an
        // objectWillChange every send even when already nil, forcing a needless
        // ComposerView recompute in the same beat as the field reset.
        if store.notice != nil {
            store.notice = nil
        }
        store.send(text: body, attachments: refs)
        staging.clear()
        clearField()
    }

    /// Clear the field deterministically, INCLUDING a live CJK IME composition.
    /// The composing syllables (underlined marked text / inline candidates) live
    /// in the focused text view's marked range — the UIKit input session — NOT
    /// in the `text` binding, so a plain `text = ""` (sync or deferred) leaves
    /// them to re-commit on the next input turn and re-materialize after send
    /// (the intermittent "字没消失", worst under pinyin). Reach the focused input
    /// over the responder chain, `unmarkText()` to finalize+drop the composition
    /// FIRST (it commits, so ordering matters), then empty the document
    /// imperatively so the reset can't lose a race with the field's own edit
    /// up-sync; mirror `text` so `hasDraft`/send-gating stay in lockstep. No
    /// responder is resigned — the keyboard stays up.
    private func clearField() {
        if let input = Self.focusedTextInput() {
            input.unmarkText()
            if let range = input.textRange(
                from: input.beginningOfDocument, to: input.endOfDocument)
            {
                input.replace(range, withText: "")
            }
        }
        text = ""
    }

    /// The current first responder if it is a text input, found via the
    /// responder chain (`sendAction(to: nil)` targets the first responder).
    /// Keyed on the `UITextInput` PROTOCOL, never a concrete UITextView class,
    /// so it survives SwiftUI's private multiline-field backing across iOS
    /// versions.
    private static func focusedTextInput() -> UITextInput? {
        FirstResponderCapture.found = nil
        UIApplication.shared.sendAction(
            #selector(UIResponder.baybo_captureFirstResponder), to: nil, from: nil, for: nil)
        return FirstResponderCapture.found as? UITextInput
    }
}

/// The composer's staging machine: the strip of picks between the picker and
/// `AttachmentRef`, and every piece of work still reading one.
///
/// Owned by the SESSION (`ChatStore.staging`) rather than by the composer's
/// frame: the work outlives the frame that started it — a photo's delivery, an
/// upload, a ✕ that lands in the middle of either — and that lifetime is what
/// the whole file is about. Three invariants carry it:
/// * **the strip is the truth.** Work that finishes writes state or raises a
///   notice only while its tile is still in `staged`; there is no parallel
///   bookkeeping that can drift from what the user can see. The one exception
///   is the in-flight upload count, which is what is on the WIRE — a tile the
///   user removed is off the strip and still on it.
/// * **a spool is owned, never deleted by hand.** The temp file belongs to a
///   `SpoolFile`, unlinked when the last holder lets go — so removing a tile
///   cannot pull the file out from under the upload that is reading it, and
///   the conversation going away cannot leave the bytes behind.
/// * **the strip ends with the CONVERSATION, not with the frame.** `ChatScreen`
///   docks the composer in a `.safeAreaInset`, and every `fullScreenCover` over
///   the chat — the image viewer, the video player — tears that inset down and
///   puts it straight back. Reclaiming on the composer's teardown therefore
///   cancelled three mid-flight uploads and emptied the strip because the user
///   tapped an image in the transcript. `AppStore.chatPath` is the signal that
///   means they left (`ChatStore.leaveChat`), exactly as it is for audio.
@MainActor
final class ComposerStaging: ObservableObject {
    @Published private(set) var staged: [StagedAttachment] = []

    /// Only ever written to (`notice`), and weakly: an upload can outlive the
    /// screen that started it, and the store belongs to the session registry.
    private weak var store: ChatStore?
    /// The Rust core. Injected like `ChatStore`'s, so the staging machine can
    /// be driven with no gateway behind it.
    private let client: any BayboClientProtocol
    /// The system clipboard. Injected for the same reason as the client, only
    /// more so: `UIPasteboard.general` is process-global, and swift-testing runs
    /// suites in PARALLEL — one suite's paste written to the real board would
    /// surface as another's logic bug.
    private let pasteboard: any PasteboardReading
    /// The dock's notice line while the STRIP is what put it there, with the
    /// tile it names when there is one — so it can be taken back when that tile
    /// leaves, and in either case when the conversation does. A pick REJECTED
    /// before it ever took a slot (strip full, over the byte cap, unreadable)
    /// has no tile to name: nothing on the strip can retract its line, so only
    /// leaving does.
    private var stripNotice: (owner: UUID?, text: String)?
    /// Uploads actually on the wire. Deliberately not derived from the strip:
    /// a removed tile's upload runs to completion regardless (the generated
    /// UniFFI async binding has no cancellation hook), and counting only what
    /// is on SCREEN let a completing upload start two more against a slot the
    /// zombie still held — three at once on a cap of two.
    private var uploadsInFlight = 0

    init(
        store: ChatStore,
        client: any BayboClientProtocol = Baybo.client,
        pasteboard: any PasteboardReading = Pasteboards.launch()
    ) {
        self.store = store
        self.client = client
        self.pasteboard = pasteboard
        #if DEBUG
            // `-baybo-demo-compose`: seed the staged strip (see `DemoFrames`).
            // Here rather than on the composer's appear, so it is one-shot per
            // conversation: a re-appear would otherwise refill a strip whose
            // emptiness is exactly what the smoke is checking.
            staged = StagedAttachment.demoStagedIfRequested()
        #endif
    }

    // MARK: - Admission

    func stage(photos picks: [PhotosPickerItem]) {
        admitThenLoad(picks) { id, pick in await self.loadPhoto(id: id, pick: pick) }
    }

    /// The drive every source whose bytes arrive LATER shares (a photo pick, a
    /// paste): take all the slots first, then fill them in one at a time. Both
    /// halves are load-bearing, and neither is the loader's business — which is
    /// why they live here and not once per source.
    ///
    /// **Every admitted pick takes its slot in the strip NOW, before a single
    /// byte is loaded.** `send()` reads the staged array to decide whether the
    /// message is ready, so a pick still inside its load would be invisible to
    /// it: the message shipped with whichever refs happened to be ready, the
    /// array was cleared, and the rest of the batch landed as ghost tiles on the
    /// NEXT message.
    ///
    /// **Sequential, and the handle lands before the body runs.** A load
    /// materialises the whole encoded pick, and ten at once is ten full-size
    /// `Data`s alive together. A task started on the main actor doesn't run until
    /// this turn ends, so `work` is on the tile before its own body can touch
    /// anything — a ✕ tapped mid-load always has something to cancel.
    private func admitThenLoad<Pick>(
        _ picks: [Pick], load: @escaping @MainActor (UUID, Pick) async -> Void
    ) {
        var admitted: [(id: UUID, pick: Pick)] = []
        for pick in picks {
            guard let id = admitPhoto() else { break }
            admitted.append((id, pick))
        }
        guard !admitted.isEmpty else { return }
        Task {
            for entry in admitted {
                let work = Task { await load(entry.id, entry.pick) }
                self.update(entry.id) { $0.work = work }
                await work.value
            }
        }
    }

    func stage(files urls: [URL]) {
        for url in urls {
            guard admitSlot() else { break }
            guard let item = makeFileItem(url) else { continue }
            staged.append(item)
        }
        pumpUploads()
    }

    /// Is there an image on the clipboard? Read on the `+`'s tap to decide
    /// whether the panel offers a Paste row at all — a presence probe over
    /// `types(forItemSet:)`, which is documented not to notify the user, so
    /// asking costs nothing and no "Allow Paste?" alert can come of it.
    var pasteReady: Bool { !pasteboard.imageItemIndices().isEmpty }

    /// The Paste row. The clipboard's shape is known up front — which items hold
    /// an image, without pulling a byte — so `admitThenLoad` can take the slots
    /// before any bytes are read, exactly as a photo batch does; then each image
    /// is pulled and handed to `acceptPhoto`, which is where a pasted image and a
    /// picked one become the same thing.
    ///
    /// Nothing dismisses this row (there is no picker behind it), so the caller
    /// clears `AttachMenu.pick` itself.
    ///
    /// `authorized` says the system has ALREADY attributed this read to the user
    /// — it came from iOS's own Paste command (`ComposerPasteTarget`) rather
    /// than from a button of ours — which is the whole difference between a
    /// silent read and an "Allow Paste?" alert, and it changes how the bytes are
    /// pulled below.
    func stagePasteboard(authorized: Bool = false) {
        let indices = pasteboard.imageItemIndices()
        guard !indices.isEmpty else {
            // The board can empty (or turn into plain text) between the panel
            // opening and the row being tapped. Nothing was admitted, so no tile
            // can own the line.
            publishUnownedNotice(Lang.shared.t("chat.pasteNoImage"))
            return
        }
        admitThenLoad(indices) { id, index in
            await self.loadPasted(id: id, index: index, authorized: authorized)
        }
    }

    /// Pull ONE clipboard item's bytes and fill its tile in.
    ///
    /// The two ways to read, and why there are two. An UNAUTHORISED read — our
    /// own Paste row, which iOS does not count as user intent — can raise the
    /// system "Allow Paste?" alert, and the read BLOCKS the thread it is on
    /// until that is answered; on the main actor that is the whole composer,
    /// frozen mid-paste, so it goes to a detached task. An AUTHORISED one (iOS's
    /// own Paste command) is silent, and stays on the main actor deliberately:
    /// the permission belongs to the interaction, and hopping threads to read it
    /// later is how you land outside the window it granted.
    private func loadPasted(id: UUID, index: Int, authorized: Bool) async {
        guard holds(id) else { return }
        let reader = pasteboard
        let pasted =
            authorized
            ? reader.image(at: index)
            : await Task.detached(priority: .userInitiated) {
                reader.image(at: index)
            }.value
        guard let pasted else {
            drop(id, notice: Lang.shared.t("chat.attachFailed"))
            return
        }
        await acceptPhoto(id: id, data: pasted.data, declaredMime: pasted.mime)
    }

    /// A photo pick's slot in the strip, before PhotosUI has delivered a byte
    /// of it; `nil` once the strip is full.
    func admitPhoto() -> UUID? {
        guard admitSlot() else { return nil }
        let item = StagedAttachment.pendingPhoto()
        staged.append(item)
        return item.id
    }

    /// One free slot in the strip, else the over-cap notice. Checked against
    /// the live array between appends, so a multi-select batch admits exactly
    /// up to the cap.
    private func admitSlot() -> Bool {
        guard staged.count < ChatStore.maxStagedAttachments else {
            publishUnownedNotice(
                String(
                    format: Lang.shared.t("chat.tooManyAttachments"),
                    ChatStore.maxStagedAttachments))
            return false
        }
        return true
    }

    /// A `fileImporter` URL is security-scoped: without the access bracket an
    /// iCloud / Files-provider document reads as silently EMPTY. The size comes
    /// from the URL's resource values, before any byte is read — the payload is
    /// never materialised just to find out it is over the cap.
    private func makeFileItem(_ url: URL) -> StagedAttachment? {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        guard let bytes = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize else {
            publishUnownedNotice(Lang.shared.t("chat.attachFailed"))
            return nil
        }
        guard let byteCount = StagedAttachment.wireSize(bytes) else {
            publishUnownedNotice(Lang.shared.t("chat.tooLarge"))
            return nil
        }
        let name = url.lastPathComponent
        let mime = StagedAttachment.mimeType(forExtension: url.pathExtension)
        return StagedAttachment(
            preview: .file(name: name, mime: mime), source: .scoped(url), mime: mime,
            filename: name, byteCount: byteCount)
    }

    // MARK: - A photo's bytes

    /// Fill an admitted photo tile in once PhotosUI delivers its bytes. The
    /// tile is `pending` throughout, so the send gate blocks on it from
    /// admission to blob.
    private func loadPhoto(id: UUID, pick: PhotosPickerItem) async {
        guard holds(id) else { return }
        let data: Data
        do {
            guard let loaded = try await pick.loadTransferable(type: Data.self) else {
                drop(id, notice: Lang.shared.t("chat.attachFailed"))
                return
            }
            data = loaded
        } catch {
            drop(
                id,
                notice: String(format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error)))
            return
        }
        await acceptPhoto(
            id: id, data: data,
            declaredMime: pick.supportedContentTypes.compactMap(\.preferredMIMEType).first)
    }

    /// What a DELIVERED photo goes through: the cap check, the mime sniff, the
    /// spool its upload will stream off, the thumbnail, then the upload queue.
    /// Split from the PhotosUI half above because every way this can end —
    /// including all the ways a pick the user has meanwhile removed has to end
    /// SILENTLY — lives on this side of the picker.
    func acceptPhoto(id: UUID, data: Data, declaredMime: String?) async {
        guard let byteCount = StagedAttachment.wireSize(data.count) else {
            drop(id, notice: Lang.shared.t("chat.tooLarge"))
            return
        }
        let mime = StagedAttachment.photoMime(declared: declaredMime, data: data)
        // Nothing to spool for a tile that is already gone — the ✕ during a
        // big pick's load is the common case, and it must not cost a 100 MiB
        // write.
        guard holds(id) else { return }
        guard let spool = await Self.spoolPhoto(data, id: id, mime: mime) else {
            drop(id, notice: Lang.shared.t("chat.attachFailed"))
            return
        }
        // Removed while its bytes were being written: the file goes with the
        // local reference, which is the only one that ever held it.
        guard holds(id) else { return }
        update(id) {
            $0.preview = .image(spool.image)
            $0.source = .spooled(spool.file)
            $0.mime = mime
            $0.byteCount = byteCount
        }
        pumpUploads()
    }

    /// The staging work a photo needs done OFF the main actor: spool the
    /// encoded bytes and decode the small thumbnail off the file. A ProRAW or
    /// panorama pick runs to tens of MiB, and neither the write nor the decode
    /// belongs inside a frame. `nil` means nothing could read the bytes as an
    /// image — the spool is unlinked as the returned-nothing frame drops it.
    private static func spoolPhoto(
        _ data: Data, id: UUID, mime: String
    ) async -> (file: SpoolFile, image: UIImage)? {
        await Task.detached(priority: .userInitiated) {
            guard let file = try? StagedAttachment.spool(data, id: id, mime: mime) else {
                return nil
            }
            guard let image = StagedAttachment.thumbnail(at: file.url) else { return nil }
            return (file, image)
        }.value
    }

    // MARK: - Leaving the strip

    /// A pick that never made it out of staging: its tile goes — freeing the
    /// slot for the rest of the batch — and the notice says why. The notice is
    /// raised BEFORE the removal on purpose: `publishNotice` reads the strip to
    /// decide whether the user can still see what it is about.
    private func drop(_ id: UUID, notice text: String) {
        publishNotice(text, for: id)
        discard(id)
    }

    /// The ✕ on a tile. Unlike `drop`, this is the USER taking a pick back, so
    /// the notice that named it goes with it: the red line is what offered the
    /// ✕ in the first place, and leaving it over a strip that no longer shows
    /// the failed pick is a dead end the user can only dismiss by hand.
    func remove(_ id: UUID) {
        retractNotice(for: id)
        discard(id)
    }

    /// Take a tile out of the strip and STOP whatever is still reading it.
    /// Nothing is unlinked here: a photo's spool belongs to a `SpoolFile` the
    /// running upload holds as well, so the file lives exactly as long as
    /// something is still reading it. Deleting it here instead landed either
    /// between the two opens the Rust side makes (the hash pass, then the body
    /// reader) — failing the upload with a read error the user never asked for
    /// — or after both, letting it silently succeed. A Files pick's URL is the
    /// user's own document and was never ours to delete.
    private func discard(_ id: UUID) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        let removed = staged.remove(at: idx)
        removed.work?.cancel()
    }

    /// The strip empties on send and when the user leaves the conversation
    /// (`ChatStore.leaveChat`). Work still in flight is cancelled — no message
    /// can reference its blob any more — and each spool is unlinked as its last
    /// holder lets go, so a 100 MiB photo never outlives the strip it was
    /// staged in. Whatever line the strip put on the dock goes too: it named a
    /// tile and there are no tiles left, or it named a pick that never became
    /// one and there is no longer anything for it to explain.
    func clear() {
        for item in staged {
            item.work?.cancel()
        }
        staged.removeAll()
        retractStripNotice()
    }

    /// Is this pick still on screen? Every terminal write and every failure
    /// notice is conditional on it. A tile the user removed mid-load or
    /// mid-upload has taken its slot back, and the failure that arrives
    /// afterwards — a timeout, a photo that came back over-cap — names nothing
    /// they can act on: "Send failed: …" over a strip that no longer shows it
    /// is a dead end.
    private func holds(_ id: UUID) -> Bool {
        staged.contains { $0.id == id }
    }

    private func publishNotice(_ text: String, for id: UUID) {
        guard holds(id) else { return }
        stripNotice = (id, text)
        store?.notice = text
    }

    /// A line about a pick that never became a tile — still worth saying (the
    /// pick simply not appearing is the part the user can't explain), but it
    /// names nothing they can act on, so `clear()` is the only thing that can
    /// take it back. It goes through here rather than straight to
    /// `store.notice` so the strip HAS it to take back: written directly, it
    /// outlived `leaveChat` and stood in red over the next visit's empty dock.
    private func publishUnownedNotice(_ text: String) {
        stripNotice = (nil, text)
        store?.notice = text
    }

    /// The Files browser handed back a failure instead of URLs: no pick, so no
    /// tile, so the line is the strip's own.
    func notePickerFailed() {
        publishUnownedNotice(Lang.shared.t("chat.attachFailed"))
    }

    /// Why the send didn't go, named on the tile that is holding it up — so the
    /// line dies with that tile the same way an upload failure's does. The gate
    /// itself is `StagedAttachment.blocker`; this is only how it speaks.
    func noteBlocked(_ blocker: StagedAttachment.Blocker) {
        let subject: StagedAttachment?
        let text: String
        switch blocker {
        case .waiting:
            subject = staged.first { $0.state.isPending }
            text = Lang.shared.t("chat.waitingUpload")
        case .failed:
            subject = staged.first { $0.state.isError }
            text = Lang.shared.t("chat.removeFailedAttachment")
        }
        guard let subject else { return }
        publishNotice(text, for: subject.id)
    }

    /// Take back the line this strip published about `id`, if it is still the
    /// one on the dock. The text check is what keeps a model or approval
    /// failure raised since then — the notice belongs to the whole dock, not to
    /// the composer — from being cleared by a ✕ it has nothing to do with.
    private func retractNotice(for id: UUID) {
        guard let current = stripNotice, current.owner == id else { return }
        retractStripNotice()
    }

    private func retractStripNotice() {
        guard let current = stripNotice else { return }
        stripNotice = nil
        guard store?.notice == current.text else { return }
        store?.notice = nil
    }

    // MARK: - Uploads

    /// Start as many queued uploads as there are free slots, oldest first.
    /// Queue order and whether a pick still exists come from the strip; the
    /// budget does NOT. `staged.filter(isUploading)` counted tiles, and a
    /// removed tile's upload is off the strip while still on the wire: with two
    /// of four picks removed mid-flight, the first completion saw an empty
    /// strip, released BOTH queued picks, and ran three uploads against a cap
    /// of two. The counter is decremented by the same task that incremented it,
    /// after the call returns — cancellation doesn't skip it, because the call
    /// is not cancellable in the first place.
    private func pumpUploads() {
        while uploadsInFlight < ChatStore.maxConcurrentUploads,
            let idx = staged.firstIndex(where: { $0.state.isQueued && $0.source != nil })
        {
            staged[idx].state = .uploading(sent: 0)
            let id = staged[idx].id
            uploadsInFlight += 1
            let task = Task {
                await self.upload(id)
                self.uploadsInFlight -= 1
                self.pumpUploads()
            }
            update(id) { $0.work = task }
        }
    }

    /// Every staged pick uploads off a PATH — a Files pick in place, a photo
    /// off the spool written at staging — so the encoded bytes never cross the
    /// FFI, a 100 MiB pick is never held whole in memory, and a retry re-reads
    /// the file instead of anything having retained it. `source` stays on this
    /// frame for the whole call: that reference is what keeps a spool on disk
    /// across both opens even if its tile is removed mid-flight.
    ///
    /// Removing a tile cancels this task, but the call itself still runs to
    /// completion — the generated UniFFI async binding has no cancellation
    /// hook — so the bytes of a removed pick DO become a blob on the gateway
    /// that no message will ever reference. Nothing sweeps chat blobs, so that
    /// one is permanent; what cancellation buys is that its result reaches
    /// neither the strip nor the notice line.
    private func upload(_ id: UUID) async {
        guard let item = staged.first(where: { $0.id == id }), let source = item.source else {
            return
        }
        let url = source.url
        let scoped = source.isScoped && url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        // ARC may release a local right after its LAST use, and `source`'s is
        // `url.path` below — before the call it has to outlive. `url` is a
        // plain value and retains nothing. Without this the strip's copy going
        // away mid-upload can leave this frame holding the only reference,
        // drop it early, and unlink the file the upload is still reading.
        defer { withExtendedLifetime(source) {} }
        do {
            let blobId = try await client.blobUploadFile(
                path: url.path, mimeType: item.mime,
                progress: BlobProgressForwarder { [weak self] sent, _ in
                    self?.update(id) {
                        guard $0.state.isUploading else { return }
                        $0.state = .uploading(sent: sent)
                    }
                })
            update(id) { $0.state = .ready(blobId: blobId) }
        } catch {
            update(id) { $0.state = .error }
            publishNotice(
                String(format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error)), for: id)
        }
    }

    /// A tap on a failed tile. The claim happens on the CURRENT array element
    /// and BEFORE any await, so a double-tap — both taps reading the same
    /// render snapshot, both seeing `.error` — starts exactly one upload:
    /// two would mint two blobs on the gateway and race over which one the
    /// message ends up referencing.
    func retry(_ id: UUID) {
        var claimed = false
        update(id) { claimed = $0.claimRetry() }
        guard claimed else { return }
        retractNotice(for: id)
        pumpUploads()
    }

    private func update(_ id: UUID, _ mutate: (inout StagedAttachment) -> Void) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        mutate(&staged[idx])
    }
}

/// A photo pick's temp spool, owned by everything holding it: the file is
/// unlinked when the LAST reference goes.
///
/// That ownership is the pick's whole lifetime. The tiles die with the
/// composer — the back button on a strip that was never sent — and an upload
/// still streaming one keeps its own file alive to the end instead of reading
/// a path the ✕ has just unlinked. Deliberately unlike the digest-keyed
/// `baybo-preview` / `baybo-deck-share` caches, which are bounded by the number
/// of distinct blobs and hit again on re-open: this name is a UUID, so anything
/// left behind is dead weight nothing can ever reach — up to 100 MiB of it per
/// pick, ten picks to a strip.
final class SpoolFile: Sendable {
    let url: URL

    init(url: URL) {
        self.url = url
    }

    deinit {
        try? FileManager.default.removeItem(at: url)
    }
}

/// One pick sitting in the composer's staged strip, from selection to blob.
struct StagedAttachment: Identifiable {
    /// What the strip renders. An enum, not an `isImage` flag: a file tile and
    /// an image thumbnail carry different payloads, and only one of them can
    /// ever be right for a given pick. The thumbnail is `nil` until the pick's
    /// bytes have been delivered and decoded.
    enum Preview {
        case image(UIImage?)
        case file(name: String, mime: String)
    }

    /// Where a (re)upload reads the bytes from — always a PATH, so the encoded
    /// pick never crosses the FFI and a retry re-reads the file instead of
    /// anything having retained it.
    enum Source {
        /// The composer's own temp spool (a photo pick) — ours, and unlinked
        /// when the last holder of the `SpoolFile` lets go.
        case spooled(SpoolFile)
        /// A Files pick read in place under its security scope — the user's
        /// own document, never ours to delete.
        case scoped(URL)

        var url: URL {
            switch self {
            case .spooled(let file): return file.url
            case .scoped(let url): return url
            }
        }

        var isScoped: Bool {
            if case .scoped = self { return true }
            return false
        }
    }

    enum State {
        /// Admitted to the strip with nothing on the wire yet: a photo whose
        /// bytes PhotosUI has not delivered, or a pick waiting for an upload
        /// slot. The send gate treats it exactly like `uploading` — either way
        /// it is a pick the message is still waiting on.
        case queued
        case uploading(sent: UInt64)
        case ready(blobId: String)
        case error

        var isQueued: Bool {
            if case .queued = self { return true }
            return false
        }

        var isUploading: Bool {
            if case .uploading = self { return true }
            return false
        }

        /// Not on the gateway yet, and not failed either.
        var isPending: Bool { isQueued || isUploading }

        var isError: Bool {
            if case .error = self { return true }
            return false
        }

        var sentBytes: UInt64 {
            if case .uploading(let sent) = self { return sent }
            return 0
        }
    }

    /// Why the staged strip can't ride a send yet, or `nil` when every pick has
    /// its blob.
    enum Blocker: Equatable {
        case waiting
        case failed
    }

    let id = UUID()
    var preview: Preview
    /// `nil` only on a photo tile PhotosUI has not delivered yet: the pick
    /// takes its slot in the strip before its bytes exist.
    var source: Source?
    var mime: String
    let filename: String?
    /// Validated at staging (`wireSize`), so the wire field is never a
    /// saturating conversion of a size the card's progress denominator reads.
    var byteCount: UInt32
    var state: State = .queued
    /// Whatever is currently reading this pick — the PhotosUI delivery, or its
    /// upload. Removing the tile cancels it rather than deleting the file out
    /// from under it.
    var work: Task<Void, Never>?

    /// The mime a file whose type nothing could name gets. The kind derivation
    /// reads it as a plain file, which is the honest answer.
    static let fallbackMime = "application/octet-stream"
    static let plainTextMime = "text/plain"

    /// A photo pick's tile the instant it is admitted, before PhotosUI has
    /// delivered a byte. It carries no thumbnail, mime or size because none of
    /// that is knowable yet; what it does carry is a SLOT in the strip, which
    /// is what `blocker` reads.
    static func pendingPhoto() -> StagedAttachment {
        StagedAttachment(
            preview: .image(nil), source: nil, mime: fallbackMime, filename: nil, byteCount: 0)
    }

    /// A pick still on its way to the gateway blocks the send — including one
    /// whose bytes PhotosUI has not delivered yet. Gating on "uploading" alone
    /// could not see a pick mid-`loadTransferable`: the message shipped with
    /// whichever refs were ready and the rest of the batch became ghost tiles
    /// on the next one.
    static func blocker(_ staged: [StagedAttachment]) -> Blocker? {
        if staged.contains(where: { $0.state.isPending }) { return .waiting }
        if staged.contains(where: { $0.state.isError }) { return .failed }
        return nil
    }

    /// Claim a failed tile for a retry, reporting whether THIS call is the one
    /// that owns it. Two taps in one frame both see `.error` in the render
    /// snapshot, so the flip off `.error` is what makes the second a no-op.
    mutating func claimRetry() -> Bool {
        guard state.isError else { return false }
        state = .queued
        return true
    }

    /// The wire reference for a landed pick; `nil` while it has no blob.
    var attachmentRef: AttachmentRef? {
        guard case .ready(let blobId) = state else { return nil }
        return AttachmentRef(
            kind: Self.kind(forMime: mime), blobId: blobId, mimeType: mime, size: byteCount,
            filename: filename)
    }

    /// The wire kind is derived from the MIME, never from which picker the file
    /// came through: an image picked in Files is still an image, and video has
    /// no kind of its own (it rides `file` + a `video/*` mime).
    static func kind(forMime mime: String) -> AttachmentKind {
        if mime.hasPrefix("image/") { return .image }
        if mime.hasPrefix("audio/") { return .audio }
        return .file
    }

    /// Extensions the OS types as nothing at all (or as a UTI carrying no mime
    /// of its own) that are UTF-8 text by definition. Without them a `.rs` or a
    /// `docker-compose.yml` uploads as `application/octet-stream`, and the LLM
    /// layer only inlines a text-like mime — the user attaches a source file
    /// and the agent is handed a placeholder. Deliberately short: an extension
    /// that MIGHT be binary keeps the fallback, which degrades to that same
    /// placeholder rather than feeding the model garbage.
    private static let plainTextExtensions: Set<String> = [
        "rs", "toml", "yml", "yaml", "log", "env", "ini", "cfg", "conf",
        "sql", "kt", "go", "tsx", "jsx",
    ]

    static func mimeType(forExtension ext: String) -> String {
        let declared = UTType(filenameExtension: ext)
        if let mime = declared?.preferredMIMEType { return mime }
        if declared?.conforms(to: .text) == true { return plainTextMime }
        return plainTextExtensions.contains(ext.lowercased()) ? plainTextMime : fallbackMime
    }

    /// A photo's mime, sniffed from the BYTES first. `supportedContentTypes`
    /// lists what the item CAN be delivered as and `loadTransferable` promises
    /// nothing about returning the first entry — when PhotosUI transcodes for
    /// compatibility the declared type and the actual bytes disagree, and the
    /// mime is what the gateway stores and what the provider is handed. The
    /// declared type fills in only for bytes the sniff doesn't recognise.
    static func photoMime(declared: String?, data: Data) -> String {
        let sniffed = sniffMime(data)
        if sniffed != fallbackMime { return sniffed }
        return declared ?? fallbackMime
    }

    private static let heifBrands = ["heic", "heix", "hevc", "mif1", "msf1"]

    static func sniffMime(_ data: Data) -> String {
        let head = [UInt8](data.prefix(12))
        if head.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if head.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if head.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        guard head.count == 12 else { return fallbackMime }
        let box = String(decoding: head[4...7], as: UTF8.self)
        let brand = String(decoding: head[8...11], as: UTF8.self)
        if brand == "WEBP" { return "image/webp" }
        // HEIC is an ISO-BMFF brand, and a picked photo is very often one: a
        // mime the kind derivation can't read as an image would ship the user's
        // photo as a plain file card.
        if box == "ftyp", heifBrands.contains(brand) { return "image/heic" }
        return fallbackMime
    }

    static func glyph(forMime mime: String) -> String {
        if mime.hasPrefix("image/") { return "photo" }
        if mime.hasPrefix("video/") { return "film" }
        if mime.hasPrefix("audio/") { return "waveform" }
        if mime.hasPrefix("text/") { return "doc.text" }
        return "doc"
    }

    /// The size as the wire carries it, or `nil` for a pick that must be
    /// rejected up front — over the gateway's blob cap, or (unreachably, since
    /// the cap is well under 4 GiB) too large for the field.
    static func wireSize(_ bytes: Int) -> UInt32? {
        guard bytes >= 0, bytes <= ChatStore.maxAttachmentBytes else { return nil }
        return UInt32(exactly: bytes)
    }

    static func byteText(_ bytes: UInt64) -> String {
        byteFormatter.string(fromByteCount: Int64(bytes))
    }

    /// `<tmp>/baybo-compose/<run id>/<pick id>.<ext>`: a photo's bytes land
    /// here and the upload streams off the PATH. Nothing retains the encoded
    /// pick — ten ProRAW picks held as `Data` for their tiles' lifetime is most
    /// of a gigabyte, and a foreground jetsam. The pick id keeps two picks
    /// apart; the extension only makes the path legible.
    static func spool(_ data: Data, id: UUID, mime: String) throws -> SpoolFile {
        try FileManager.default.createDirectory(
            at: spoolDirectory, withIntermediateDirectories: true)
        let ext = UTType(mimeType: mime)?.preferredFilenameExtension
        let name = ext.map { "\(id.uuidString).\($0)" } ?? id.uuidString
        let url = spoolDirectory.appendingPathComponent(name)
        try data.write(to: url, options: .atomic)
        return SpoolFile(url: url)
    }

    static let spoolRoot = FileManager.default.temporaryDirectory
        .appendingPathComponent(spoolRootName, isDirectory: true)
    /// THIS run's spools, and nothing else's — which is what lets the sweep
    /// below reclaim by directory instead of having to know which individual
    /// files are still being read.
    static let spoolDirectory = spoolRoot.appendingPathComponent(runId, isDirectory: true)

    /// Reclaim the spools of runs that are OVER. A `SpoolFile` unlinks its own
    /// file when the last holder lets go, which a kill, a crash or a jetsam
    /// never gets to do: those bytes are then unreachable forever, so an
    /// abandoned ten-pick strip would sit in the temp dir at up to a gigabyte
    /// until iOS itself reclaims it. Run at LAUNCH, and structurally unable to
    /// touch a pick this run is still uploading — everything it spools is under
    /// `spoolDirectory`, the one child left alone.
    static func sweepAbandonedSpools() async {
        let manager = FileManager.default
        guard
            let runs = try? manager.contentsOfDirectory(
                at: spoolRoot, includingPropertiesForKeys: nil)
        else { return }
        for run in runs where run.lastPathComponent != spoolDirectory.lastPathComponent {
            try? manager.removeItem(at: run)
        }
    }

    /// A small decoded thumbnail off the spooled file. ImageIO downsamples
    /// DURING the decode, so the tile never holds a full-resolution backing
    /// store — and, unlike `UIImage(data:)`, never keeps the encoded pick alive
    /// behind it. `nil` for bytes no decoder recognises.
    static func thumbnail(at url: URL) -> UIImage? {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: thumbnailMaxPixels,
        ]
        guard
            let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary)
        else { return nil }
        return UIImage(cgImage: image)
    }

    private static let spoolRootName = "baybo-compose"
    private static let runId = UUID().uuidString
    /// The tile draws at 64pt; 256px covers it at every screen scale.
    private static let thumbnailMaxPixels = 256

    private static let byteFormatter: ByteCountFormatter = {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        formatter.allowsNonnumericFormatting = false
        return formatter
    }()
}

/// One-shot sink for the responder-chain first-responder probe below.
private enum FirstResponderCapture {
    static weak var found: UIResponder?
}

extension UIResponder {
    /// Action target for `sendAction(to: nil)`: only the current first
    /// responder receives it, so it records itself for `focusedTextInput()`.
    @objc fileprivate func baybo_captureFirstResponder() {
        FirstResponderCapture.found = self
    }
}
