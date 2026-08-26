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
/// card genuinely does differently: the unblock toggle, the REST approval
/// card, and a send that posts a comment and then — in that order — lifts a
/// block.
struct IssueDock: View {
    @ObservedObject var store: IssueStore
    @ObservedObject var staging: ComposerStaging
    @ObservedObject var attach: AttachMenu
    @ObservedObject private var lang = Lang.shared

    @State private var photoPicks: [PhotosPickerItem] = []
    @State private var sending = false
    @FocusState private var focused: Bool
    /// Where the caret was after the last edit, as a UTF-16 offset — `nil`
    /// while an IME composition is open, and until the field has been typed
    /// into at all.
    ///
    /// A `TextField` reports no selection, so this is read off the focused
    /// UIKit document on each edit. What that cannot see is the caret MOVING
    /// with no edit behind it (a tap into the middle of a draft), and the
    /// strip is stale for exactly that long — harmlessly: the offset it holds
    /// still points at the `@` it was measured from, so completing from a
    /// stale strip still lands in the right place.
    @State private var caret: Int?
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

    /// The gutter every row above the pill sits in. The PILL's own gutters are
    /// `ComposerPill`'s — wider at rest, and animated — so this cannot be one
    /// padding around the whole stack the way it was until 2026-08-26.
    private static let rowGutter: CGFloat = 14

    var body: some View {
        VStack(spacing: 8) {
            if let notice = store.notice {
                noticeRow(notice)
                    .padding(.horizontal, Self.rowGutter)
            }
            if let prompt = store.pendingApprovals.first {
                approvalCard(prompt)
                    .padding(.horizontal, Self.rowGutter)
                    // Re-key on the prompt so answering the head SWAPS in the
                    // next card (a fresh one-shot latch) instead of reusing
                    // this one, whose buttons have already fired.
                    .id(prompt.callId)
            }
            answerRow
                .padding(.horizontal, Self.rowGutter)
            if !staging.staged.isEmpty {
                StagedStrip(
                    items: staging.staged,
                    onRemove: { staging.remove($0) },
                    onRetry: { staging.retry($0) }
                )
                .padding(.horizontal, Self.rowGutter)
            }
            mentionRow
            composer
        }
        .padding(.top, 8)
        .padding(.bottom, dockBottomPadding)
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

    /// The one row above the pill, and only when an agent is waiting on an
    /// answer: who it goes to, and whether sending also lifts the block.
    ///
    /// **This is a control, not a caption.** The composer HINT that used to
    /// live here — the sentence saying what sending would do — came out on
    /// 2026-08-26: two lines of full-width mono over every card, mostly
    /// repeating what the state band above it already said (`WORKING @dev-1`
    /// and then "@dev-1 is mid-run"), and never localized. The toggle stays
    /// because it changes what the send DOES.
    @ViewBuilder private var answerRow: some View {
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
        }
    }

    // MARK: - Mentions

    /// The mention being typed, if the field has one open.
    private var mention: IssueMentionQuery? {
        guard focused, let caret else { return nil }
        return IssueMention.query(in: staging.text, caret: caret)
    }

    /// Who a half-typed `@` could mean, on the board's own roster.
    private var mentionCandidates: [TeamMemberInfo] {
        guard let mention else { return [] }
        return IssueMention.candidates(
            in: store.team, prefix: mention.prefix, assignee: store.issue?.assignee)
    }

    /// The handles an open `@` could become, directly above the field.
    ///
    /// **A strip, not a popup at the caret.** The web dashboard opens its list
    /// where the caret is, which it can do because a `<textarea>`'s caret can
    /// be measured (`projects/caret.ts` lays the draft out a second time in a
    /// hidden mirror to find it). A SwiftUI `TextField` exposes neither its
    /// caret's position nor its offset, and a phone has the QuickType bar's
    /// answer anyway: one row between the words and the keyboard, close enough
    /// to what is being typed that choosing does not mean looking away.
    ///
    /// It scrolls rather than wrapping, so a board with a large team costs the
    /// page the same height as a board with three agents — this row's height
    /// is reported to the card as its bottom obstruction, and a row that grows
    /// to two lines reflows the whole page mid-word.
    @ViewBuilder private var mentionRow: some View {
        let candidates = mentionCandidates
        if !candidates.isEmpty {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(candidates, id: \.id) { member in
                        Button {
                            complete(member.handle)
                        } label: {
                            Text(verbatim: "@\(member.handle)")
                                .font(Theme.mono(12))
                                .foregroundStyle(Theme.ink)
                                .padding(.horizontal, 10)
                                .frame(minHeight: Self.chipHeight)
                                .background(Capsule().fill(Theme.paper))
                                .overlay(Capsule().strokeBorder(Theme.lineStrong, lineWidth: 1))
                                .contentShape(Capsule())
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("issue-mention.\(member.handle)")
                    }
                }
                .padding(.horizontal, Self.rowGutter)
            }
            // The scroller is pinned to the chips it holds: left to size
            // itself it takes the height it is offered, and the dock's height
            // is what the card is told to keep clear.
            .frame(height: Self.chipHeight)
        }
    }

    /// One chip, and therefore the strip. Under the 44pt hit floor
    /// deliberately — this is the QuickType bar's row rather than a control
    /// with consequence, and every point of it is charged to the card as
    /// obstruction.
    private static let chipHeight: CGFloat = 32

    /// Write `@handle ` over what has been typed of it.
    private func complete(_ handle: String) {
        guard let mention else { return }
        Haptics.tap()
        let edit = IssueMention.edit(for: mention, handle: handle, in: staging.text)
        // The DOCUMENT first, so a completion landing mid-draft leaves the
        // caret behind the handle instead of at the end of the comment. The
        // binding write after it is the fallback for a probe that found no
        // responder, and an equal write — discarded by `text`'s own guard —
        // when the document took it.
        FocusedTextInput.replace(edit.range, with: edit.text)
        staging.text = IssueMention.applying(edit, to: staging.text)
        caret = edit.range.lowerBound + edit.text.utf16.count
    }

    /// The chat's pill exactly — the glass, the 48pt floor, the 17pt field, and
    /// since 2026-08-26 its WIDTH and the beat it changes it on. The two docks
    /// drawing the same control at two widths, one push apart, was the whole
    /// reason to move the gutters into `ComposerPill`.
    ///
    /// **No prompt text.** A card's field is the only thing on the dock that
    /// takes typing, and what a comment does is already said above it by the
    /// hint line — so the grey sentence inside the pill was a third voice
    /// saying the obvious. The words stay as the field's accessibility name,
    /// which is what a `TextField` would otherwise have taken from them.
    private var composer: some View {
        ComposerPill(
            text: $staging.text,
            placeholder: "",
            accessibilityLabel: lang.t("issue.commentField"),
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
        // The caret is read HERE rather than tracked by the field, which
        // reports no selection at all. By the time the binding has changed the
        // UIKit document behind it already holds the edit, so this is the
        // caret as it stands after the keystroke.
        //
        // Nothing is offered against an OPEN composition: a pinyin keyboard
        // mirrors its uncommitted syllables into the binding, so `@ceshi` on
        // the way to `@测试` would otherwise be read as a handle prefix and
        // answered with a list.
        .onChange(of: staging.text) { _, text in
            caret =
                FocusedTextInput.isComposing
                ? nil : (FocusedTextInput.caretOffset ?? text.utf16.count)
        }
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

    /// The gap under the pill — `ComposerView.dockBottomPadding`, to the pixel.
    /// The card's pill used to sit a flat 8pt off the bottom while the chat's
    /// sat flush and lifted 12 on focus, so pushing a card off a conversation
    /// moved the thing you type into. Unlike the gutters this one is VERTICAL,
    /// and this dock's height is what native reports to the page as its bottom
    /// obstruction — so the 12pt arrives as a re-pad, which is the same thing
    /// the keyboard riding up already does a beat later.
    private var dockBottomPadding: CGFloat { focused ? 12 : 0 }

    /// How far the paper takes to arrive at the dock's top edge. A card's
    /// entries slide under the dock as it scrolls, and a flat opaque band
    /// would cut the last one off mid-line.
    ///
    /// A FIXED band, not a fraction of the dock's height, which is what it was
    /// until the mention strip: at 40% of the height every row the dock grows
    /// pushed the fade further down, so the newest row — the one the operator
    /// is looking at — landed in the transparent part of it, with the card's
    /// own text running between the chips. At rest the two are within a few
    /// points of each other.
    private static let veilFade: CGFloat = 28

    /// The paper tail under the pill, so the page's content fades out behind
    /// the dock rather than sliding under a hard edge.
    private var veil: some View {
        VStack(spacing: 0) {
            LinearGradient(
                colors: [Theme.paper.opacity(0), Theme.paper],
                startPoint: .top, endPoint: .bottom
            )
            .frame(height: Self.veilFade)
            Theme.paper
        }
        .ignoresSafeArea(edges: .bottom)
        .allowsHitTesting(false)
    }
}
