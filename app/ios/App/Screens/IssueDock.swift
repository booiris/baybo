import PhotosUI
import SwiftUI

/// The card's bottom dock: what you can say and the files it will carry.
///
/// **Its own type, on shared parts.** It was its own type AND its own surface
/// until 2026-08-25, because `ComposerView` was bound to `ChatStore` from its
/// initialiser down — so a card got a plain rounded field while the chat got
/// the glass pill, and attachments were written up as deferred. What actually
/// bound them was two fields, now a `ComposerHost`; the pill, the `+`, the
/// strip and the pickers are shared views, and this file keeps only what a
/// card genuinely does differently: the REST approval card, and a send that
/// posts a comment and then — in that order — lifts an agent-authored block.
struct IssueDock: View {
    @ObservedObject var store: IssueStore
    @ObservedObject var staging: ComposerStaging
    @ObservedObject var attach: AttachMenu
    @ObservedObject private var lang = Lang.shared

    @State private var photoPicks: [PhotosPickerItem] = []
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
    /// The draft a completion just wrote. Compared by VALUE rather than held
    /// as a "just completed" flag, so it retires itself: the next keystroke
    /// makes the draft something else and the strip is live again.
    @State private var completedDraft: String?
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

    // MARK: - Mentions

    /// The mention being typed, if the field has one open.
    ///
    /// A draft this dock has just completed into offers nothing, whatever the
    /// caret says. The field's up-sync arrives a beat after the completion and
    /// re-reads the caret, and a caret UIKit parks in front of the trailing
    /// space is back INSIDE the finished handle — which reopens the strip on
    /// the handle just chosen, one tap away from writing it a second time.
    private var mention: IssueMentionQuery? {
        guard focused, let caret, staging.text != completedDraft else { return nil }
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
    ///
    /// **The whole completion is worked out BEFORE anything is written**, off
    /// one reading of the draft. Writing the document updates the binding
    /// under this function — that is what a text field does — so a draft read
    /// back afterwards already carries the handle, and applying the edit to it
    /// a second time is where `@dev-1 ev-1 ` came from. `IssueMentionCompletion`
    /// carries both halves so there is no second reading to get wrong.
    private func complete(_ handle: String) {
        guard let mention else { return }
        Haptics.tap()
        let completion = IssueMention.completion(
            for: mention, handle: handle, in: staging.text)
        // The DOCUMENT first, so a completion landing mid-draft leaves the
        // caret behind the handle instead of at the end of the comment.
        let wrote = FocusedTextInput.replace(completion.range, with: completion.text)
        // ...and the binding only if the field has not already reported the
        // same thing. A text field's own up-sync may land before this line or
        // after it, and the whole class of bug here is one writer acting on
        // what the other has already done — so the write is conditioned on
        // what the draft SAYS rather than on which order they ran in. It is
        // still unconditional when the document could not be reached at all:
        // an unwritten binding would send the half-typed handle.
        if !wrote || staging.text != completion.draft {
            staging.text = completion.draft
        }
        caret = completion.range.lowerBound + completion.text.utf16.count
        completedDraft = completion.draft
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
            .disabled(!canSend)
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
        guard let payload = staging.claimSend() else { return }
        Haptics.tap()
        let lifting = question != nil
        store.sendComment(
            payload.text,
            attachments: payload.picks.compactMap(\.attachmentRef),
            unblockAfterSend: lifting)
        clearField()
        staging.discardDraft()
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
