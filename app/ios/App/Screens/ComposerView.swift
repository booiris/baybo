import PhotosUI
import SwiftUI

/// The native composer dock: autosizing field, image staging via PhotosPicker,
/// in-field send. Interaction contract preserved from the web composer:
/// * staged picks count as a draft, so an attachment-only send works with an
///   empty field;
/// * send is gated while any staged item is uploading (`waitingUpload` notice);
/// * picks over 100 MiB are rejected up front (`tooLarge`), matching the
///   gateway's blob cap.
struct ComposerView: View {
    @ObservedObject var store: ChatStore
    @ObservedObject private var lang = Lang.shared
    @State private var text = ""
    @State private var staged: [StagedAttachment] = []
    @State private var pickerItem: PhotosPickerItem?
    @FocusState private var focused: Bool

    private static let inputHitSlop: CGFloat = 10

    private var hasDraft: Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !staged.isEmpty
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

            if !staged.isEmpty {
                stagedStrip
            }

            // One ChatGPT-style pill: inline plus on the left, in-field send
            // on the right — no satellite icon circles.
            HStack(alignment: .bottom, spacing: 4) {
                PhotosPicker(selection: $pickerItem, matching: .images) {
                    Image(systemName: "plus")
                        .font(.system(size: 22, weight: .light))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 46, height: 48)
                }
                .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.addImage")))

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
            .glassEffect(
                .regular.tint(Theme.paper.opacity(0.25)), in: .rect(cornerRadius: 24)
            )
            .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
            // At rest the pill holds a moderate width; focus stretches it out
            // toward the screen edges — a small gutter stays — on the
            // keyboard's beat. The notice/staged rows keep their own gutters.
            .padding(.horizontal, focused ? 14 : 40)
            .animation(.easeOut(duration: 0.25), value: focused)
        }
        .padding(.top, 0)
        .padding(.bottom, dockBottomPadding)
        .background { composerVeil }
        .onChange(of: pickerItem) { _, item in
            guard let item else { return }
            pickerItem = nil
            stage(item)
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

    private var stagedStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(staged) { item in
                    ZStack(alignment: .topTrailing) {
                        Image(uiImage: item.thumbnail)
                            .resizable()
                            .scaledToFill()
                            .frame(width: 64, height: 64)
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                            .overlay(
                                RoundedRectangle(cornerRadius: 10)
                                    .strokeBorder(
                                        item.state.isError ? Theme.err : Theme.line, lineWidth: 1)
                            )
                            .opacity(item.state.isUploading ? 0.5 : 1)

                        if item.state.isUploading {
                            ProgressView()
                                .frame(width: 64, height: 64)
                        }

                        Button {
                            staged.removeAll { $0.id == item.id }
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

    private func stage(_ item: PhotosPickerItem) {
        Task {
            let data: Data
            do {
                guard let loaded = try await item.loadTransferable(type: Data.self) else {
                    store.notice = Lang.shared.t("chat.attachFailed")
                    return
                }
                data = loaded
            } catch {
                store.notice = String(
                    format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error))
                return
            }
            guard data.count <= ChatStore.maxAttachmentBytes else {
                store.notice = Lang.shared.t("chat.tooLarge")
                return
            }
            guard let image = UIImage(data: data) else {
                store.notice = Lang.shared.t("chat.attachFailed")
                return
            }
            let stagedItem = StagedAttachment(thumbnail: image, byteCount: data.count)
            staged.append(stagedItem)
            do {
                let mime = Self.sniffMime(data)
                let blobId = try await Baybo.client.blobUploadBytes(bytes: data, mimeType: mime)
                update(stagedItem.id) { $0.state = .ready(blobId: blobId, mime: mime) }
            } catch {
                update(stagedItem.id) { $0.state = .error }
                store.notice = String(
                    format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error))
            }
        }
    }

    private func update(_ id: UUID, _ mutate: (inout StagedAttachment) -> Void) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        mutate(&staged[idx])
    }

    private func send() {
        if staged.contains(where: { $0.state.isUploading }) {
            store.notice = Lang.shared.t("chat.waitingUpload")
            return
        }
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let refs: [AttachmentRef] = staged.compactMap { item in
            guard case .ready(let blobId, let mime) = item.state else { return nil }
            return AttachmentRef(
                kind: .image, blobId: blobId, mimeType: mime,
                size: UInt32(clamping: item.byteCount), filename: nil)
        }
        guard !body.isEmpty || !refs.isEmpty else { return }
        // Guard the write: an unconditional `store.notice = nil` publishes an
        // objectWillChange every send even when already nil, forcing a needless
        // ComposerView recompute in the same beat as the field reset.
        if store.notice != nil {
            store.notice = nil
        }
        store.send(text: body, attachments: refs)
        staged.removeAll()
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

    private static func sniffMime(_ data: Data) -> String {
        if data.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if data.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if data.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        if data.count > 11, data[8...11] == Data([0x57, 0x45, 0x42, 0x50]) {
            return "image/webp"
        }
        // HEIC picks land here; the gateway stores whatever mime is declared.
        return "application/octet-stream"
    }
}

struct StagedAttachment: Identifiable {
    enum State {
        case uploading
        case ready(blobId: String, mime: String)
        case error

        var isUploading: Bool {
            if case .uploading = self { return true }
            return false
        }

        var isError: Bool {
            if case .error = self { return true }
            return false
        }
    }

    let id = UUID()
    let thumbnail: UIImage
    let byteCount: Int
    var state: State = .uploading
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
