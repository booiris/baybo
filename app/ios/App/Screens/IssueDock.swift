import SwiftUI

/// The card's bottom dock: what you can say, and what saying it will do.
///
/// Its own type rather than `ComposerView` reused, because that one is bound to
/// `ChatStore` from its initialiser down — a draft store, an attach menu, a
/// staging strip, an outbox. What a card needs is a text field, a sentence
/// above it, and the approval prompt when one is parked. Sharing the seam
/// (`CompactPillButtonStyle`, `ApprovalCardView`) rather than the surface.
struct IssueDock: View {
    @ObservedObject var store: IssueStore
    @ObservedObject private var lang = Lang.shared

    @State private var text = ""
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
                if let prompt = store.pendingApprovals.first {
                    approvalCard(prompt)
                }
                hint
                composer
            }
        }
        .padding(.horizontal, 14)
        .padding(.top, 8)
        .background(alignment: .bottom) { veil }
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

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField(lang.t("issue.commentPlaceholder"), text: $text, axis: .vertical)
                .font(Theme.sys(15))
                .foregroundStyle(Theme.ink)
                .lineLimit(1...5)
                .focused($focused)
                .padding(.horizontal, 14)
                .padding(.vertical, 11)
                .accessibilityIdentifier(Self.fieldIdentifier)
            Button {
                send()
            } label: {
                Image(systemName: "arrow.up")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(canSend ? Theme.paper : Theme.inkSoft)
                    .frame(width: 34, height: 34)
                    .background(canSend ? Theme.ink : Theme.surface, in: Circle())
            }
            .buttonStyle(.plain)
            .disabled(!canSend)
            .padding(.trailing, 6)
            .padding(.bottom, 6)
            .accessibilityIdentifier("issue-send")
            .accessibilityLabel(Text(verbatim: lang.t("issue.send")))
        }
        .background(
            RoundedRectangle(cornerRadius: 24, style: .continuous)
                .fill(Theme.paper)
                .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
        )
        .padding(.bottom, 8)
    }

    private var canSend: Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func send() {
        guard canSend else { return }
        Haptics.tap()
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        text = ""
        store.comment(body)
        // The comment first, THEN the unblock: the unblock door hands the
        // parked run back out, and its brief is built from what the card says
        // at that moment — lifting first would restart the agent without the
        // answer it stopped for.
        if question != nil, unblockAfterSend {
            store.unblock()
        }
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
