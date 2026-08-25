import PhotosUI
import SwiftUI

/// The card's bottom dock: what you can say, what saying it will do, and the
/// files it will carry.
///
/// **Its own type, on shared parts.** It was its own type AND its own surface
/// until 2026-08-25, because `ComposerView` was bound to `ChatStore` from its
/// initialiser down — so a card got a plain rounded field while the chat got
/// the glass pill, and attachments were written up as deferred. What actually
/// bound them was two fields, now a `ComposerHost`; the pill, the `+`, the
/// strip and the pickers are shared views, and this file keeps only what a
/// card genuinely does differently: the description editor's bar, the hint
/// line, the unblock toggle, the REST approval card, and a send that posts a
/// comment and then — in that order — lifts a block.
struct IssueDock: View {
    @ObservedObject var store: IssueStore
    @ObservedObject var staging: ComposerStaging
    @ObservedObject var attach: AttachMenu
    @ObservedObject private var lang = Lang.shared

    @State private var photoPicks: [PhotosPickerItem] = []
    @State private var sending = false
    @FocusState private var focused: Bool
    /// Send the comment AND lift the block. Checked by default when an agent
    /// is the one asking: answering a question and leaving the card parked is
    /// almost never what somebody meant, and the unblock is what hands the run
    /// back out carrying the answer.
    @State private var unblockAfterSend = true

    /// `IssueDockUITests` addresses the field by this.
    static let fieldIdentifier = "issue.field"

    private var question: IssueTimeline.PendingQuestion? {
        IssueTimeline.agentQuestion(
            blockedReason: store.issue?.blockedReason, events: store.events)
    }

    var body: some View {
        VStack(spacing: 8) {
            if store.editing {
                editingBar
            } else {
                if let notice = store.notice {
                    noticeRow(notice)
                }
                if let prompt = store.pendingApprovals.first {
                    approvalCard(prompt)
                        // Re-key on the prompt so answering the head SWAPS in
                        // the next card (a fresh one-shot latch) instead of
                        // reusing this one, whose buttons have already fired.
                        .id(prompt.callId)
                }
                hint
                if !staging.staged.isEmpty {
                    StagedStrip(
                        items: staging.staged,
                        onRemove: { staging.remove($0) },
                        onRetry: { staging.retry($0) })
                }
                composer
            }
        }
        .padding(.horizontal, 14)
        .padding(.top, 8)
        .background(alignment: .bottom) { veil }
        .attachmentPickers(attach: attach, staging: staging, photoPicks: $photoPicks)
    }

    /// The strip's own line — too large, still uploading, could not be read.
    /// Distinct from `writeError`'s banner at the top of the page: this one
    /// belongs to the row that raised it and is taken back by the same tile.
    private func noticeRow(_ notice: String) -> some View {
        HStack {
            Text(verbatim: notice)
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.err)
                .lineLimit(2)
            Spacer(minLength: 0)
        }
    }

    // MARK: - Editing

    /// Native owns the bar; the web owns the textarea. Done flips `editing`
    /// off, and the PAGE is what sends the text back — native never reads the
    /// editor's contents, so there is no second copy of the draft to disagree.
    private var editingBar: some View {
        HStack {
            Button(lang.t("common.cancel")) {
                Haptics.tap()
                store.editing = false
            }
            .buttonStyle(CompactPillButtonStyle(fill: nil, color: Theme.inkSoft, expands: false))
            .accessibilityIdentifier("issue-edit-cancel")
            Spacer()
            Text(verbatim: lang.t("issue.editingDescription"))
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
            Button(lang.t("issue.doneEdit")) {
                Haptics.tap()
                store.editing = false
            }
            .buttonStyle(
                CompactPillButtonStyle(fill: Theme.ink, color: Theme.paper, expands: false))
            .accessibilityIdentifier("issue-edit-done")
        }
        .padding(.bottom, 6)
    }

    // MARK: - Approval

    private func approvalCard(_ prompt: IssueApprovalPrompt) -> some View {
        ApprovalCardView(
            approval: PendingApproval(
                callId: prompt.callId,
                toolCallId: nil,
                tool: prompt.tool,
                description: prompt.summary,
                paramsPreview: "",
                accesses: []),
            queued: max(0, store.pendingApprovals.count - 1),
            onDecide: { decision in
                // Two answers, not the transcript's three: `approve_always`
                // widens a policy from a surface with no room to show what was
                // widened, so the board's REST door does not offer it at all.
                store.resolveApproval(
                    callId: prompt.callId,
                    decision: decision == .deny ? .deny : .approve)
            })
    }

    // MARK: - Composer

    /// The third mirror of `comments::comment_delivery` — what sending will do,
    /// said while the text is still being typed, which is why it cannot be a
    /// question for the server.
    @ViewBuilder private var hint: some View {
        if let question {
            HStack(spacing: 6) {
                Text(verbatim: lang.t("issue.answering", "@\(question.askedBy)"))
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                Spacer(minLength: 6)
                Button {
                    Haptics.tap()
                    unblockAfterSend.toggle()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: unblockAfterSend ? "checkmark.square" : "square")
                            .font(.system(size: 11))
                        Text(verbatim: lang.t("issue.unblockAfterSend"))
                            .font(Theme.mono(10.5))
                    }
                    .foregroundStyle(unblockAfterSend ? Theme.ink : Theme.inkSoft)
                    .frame(minHeight: 32)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("issue-unblock-toggle")
                .accessibilityValue(Text(verbatim: unblockAfterSend ? "1" : "0"))
            }
        } else if !store.commentHint.isEmpty {
            HStack {
                Text(verbatim: store.commentHint)
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
                Spacer(minLength: 0)
            }
        }
    }

    /// The chat's pill exactly — the glass, the 48pt floor, the 17pt field —
    /// with a `+` on the left and a plain send on the right. No focus-driven
    /// gutter animation: this dock streams its own top edge to the card page
    /// as a bottom inset on every layout settle, so an animating dock is a
    /// moving inset per tick.
    private var composer: some View {
        ComposerPill(
            text: $staging.text,
            placeholder: lang.t("issue.commentPlaceholder"),
            fieldIdentifier: Self.fieldIdentifier,
            lineLimit: 1...5,
            focused: $focused
        ) {
            AttachButton(attach: attach, pasteReady: staging.pasteReady)
        } trailing: {
            Button {
                send()
            } label: {
                ComposerSendCircle(systemName: "arrow.up", filled: canSend)
            }
            .buttonStyle(.plain)
            .disabled(!canSend || sending)
            .padding(.trailing, 6)
            .padding(.bottom, 6)
            .accessibilityIdentifier("issue-send")
            .accessibilityLabel(Text(verbatim: lang.t("issue.send")))
        }
        .padding(.bottom, 8)
    }

    private var canSend: Bool {
        !staging.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !staging.staged.isEmpty
    }

    private func send() {
        // The gate is the staging machine's — a pick still uploading or failed
        // holds the send, and says so on its own tile.
        guard !sending, let payload = staging.claimSend() else { return }
        Haptics.tap()
        let lifting = question != nil && unblockAfterSend
        sending = true
        Task {
            defer { sending = false }
            let landed = await store.comment(
                payload.text,
                attachments: payload.picks.compactMap { pick in
                    pick.blobId.map {
                        IssueAttachmentInput(blobId: $0, filename: pick.filename)
                    }
                })
            // **Only a landed comment discards.** There is no outbox here, and
            // the picks are uploaded blobs: clearing the strip on a failure
            // throws away files the operator cannot get back, with a banner as
            // the only trace. A failure leaves the text and the tiles exactly
            // where they are, to be sent again.
            guard landed else { return }
            clearField()
            staging.discardDraft()
            // The comment first, THEN the unblock: the unblock door hands the
            // parked run back out, and its brief is built from what the card
            // says at that moment — lifting first would restart the agent
            // without the answer it stopped for.
            if lifting {
                store.unblock()
            }
        }
    }

    /// Clear the field deterministically, INCLUDING a live CJK IME
    /// composition — the same reach the chat composer makes, and for the scar
    /// it was written for: the composing syllables live in the focused text
    /// view's marked range, not in the binding, so a plain `text = ""` leaves
    /// them to re-commit on the next input turn.
    private func clearField() {
        FocusedTextInput.clearDocument()
        staging.text = ""
    }

    /// The paper tail under the pill, so the page's content fades out behind
    /// the dock rather than sliding under a hard edge.
    private var veil: some View {
        LinearGradient(
            stops: [
                .init(color: Theme.paper.opacity(0), location: 0),
                .init(color: Theme.paper, location: 0.4),
                .init(color: Theme.paper, location: 1),
            ],
            startPoint: .top, endPoint: .bottom
        )
        .ignoresSafeArea(edges: .bottom)
        .allowsHitTesting(false)
    }
}
