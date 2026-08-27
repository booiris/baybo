import ImageIO
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// The native composer dock: autosizing field, attachment staging behind the
/// inline `+` menu (Photos / Files, plus Paste when the clipboard holds an
/// image), in-field send. The typed text, the strip and every piece of work
/// reading one live in `ComposerStaging`; this view renders them.
///
/// Nothing unsent is this view's `@State`. The field's text belongs to the
/// CONVERSATION, exactly as the strip does — see `ComposerStaging` — because
/// this frame is torn down and rebuilt for reasons that have nothing to do with
/// the user abandoning what they wrote: every `fullScreenCover` over the chat
/// takes the dock's `.safeAreaInset` with it, and backing out to the list must
/// give the draft back on the way in.
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
///   `ComposerStaging.maxStagedAttachments`.
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
    @State private var photoPicks: [PhotosPickerItem] = []
    @FocusState private var focused: Bool

    init(store: ChatStore, attach: AttachMenu) {
        _store = ObservedObject(wrappedValue: store)
        _staging = ObservedObject(wrappedValue: store.staging)
        _attach = ObservedObject(wrappedValue: attach)
    }

    /// `ComposerDraftUITests` addresses the field by this.
    static let fieldIdentifier = "composer.field"


    private var hasDraft: Bool {
        !staging.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !staging.staged.isEmpty
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
                StagedStrip(
                    items: staging.staged,
                    onRemove: { staging.remove($0) },
                    onRetry: { staging.retry($0) })
            }

            // One ChatGPT-style pill: inline plus on the left, in-field send
            // on the right — no satellite icon circles.
            ComposerPill(
                text: $staging.text,
                placeholder: lang.t("chat.placeholder"),
                accessibilityLabel: lang.t("chat.placeholder"),
                fieldIdentifier: Self.fieldIdentifier,
                lineLimit: 1...6,
                focused: $focused
            ) {
                AttachButton(attach: attach, pasteReady: staging.pasteReady)
            } trailing: {
                // While a turn runs the button is a stop control (cancel via
                // `/stop`), independent of the field: typing can't turn it back
                // into a send button mid-turn (interjection is future work).
                // Idle, it's the send button, enabled only with a draft.
                Button {
                    if store.agentRunning {
                        store.stopAgent()
                    } else {
                        send()
                    }
                } label: {
                    ComposerSendCircle(
                        systemName: store.agentRunning ? "stop.fill" : "arrow.up",
                        glyphSize: store.agentRunning ? 13 : 15,
                        filled: store.agentRunning || hasDraft
                    )
                    .animation(.easeInOut(duration: 0.2), value: store.agentRunning)
                }
                .disabled(!store.agentRunning && !hasDraft)
                .accessibilityLabel(
                    Text(verbatim: Lang.shared.t(store.agentRunning ? "chat.stop" : "chat.send")))
                .padding(.trailing, 6)
                .padding(.bottom, 6)
            }
        }
        .padding(.top, 0)
        .padding(.bottom, dockBottomPadding)
        .animation(.easeOut(duration: 0.2), value: store.pendingApprovals.first?.callId)
        .background { composerVeil }
        .attachmentPickers(attach: attach, staging: staging, photoPicks: $photoPicks)
        .onAppear {
            // One-shot for the Deck "Quick setup": seed the /deck request and
            // send it immediately, so the user lands in the conversation
            // already working. Clear `initialDraft` FIRST so a re-appear can't
            // double-send.
            //
            // The empty-field gate can only ever see an empty field: this seed
            // rides a session `startCardDraft` minted for it, and a fresh uuid
            // has no draft on disk. It stays because a seeded prompt landing
            // BEHIND something the user typed would be worse than not sending.
            if let seed = store.initialDraft, staging.text.isEmpty {
                store.initialDraft = nil
                staging.text = seed
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
    private func send() {
        // The gate — a pick still uploading or failed holds the send — lives in
        // the staging machine, so both docks pass through the same one.
        guard let payload = staging.claimSend() else { return }
        // Guard the write: an unconditional `store.notice = nil` publishes an
        // objectWillChange every send even when already nil, forcing a needless
        // ComposerView recompute in the same beat as the field reset.
        if store.notice != nil {
            store.notice = nil
        }
        store.send(text: payload.text, attachments: payload.picks.compactMap(\.attachmentRef))
        // The UIKit half FIRST: `discardDraft` empties the model, and a live
        // IME composition that is still in the input session would re-commit
        // into the freshly cleared field on the next input turn.
        clearField()
        staging.discardDraft()
    }

    /// Clear the field deterministically, INCLUDING a live CJK IME composition.
    /// The UIKit half is `FocusedTextInput.clearDocument()` — see it for the
    /// marked-text scar. This mirrors `staging.text` after, so `hasDraft` and
    /// the send gate stay in lockstep.
    private func clearField() {
        FocusedTextInput.clearDocument()
        staging.text = ""
    }
}
