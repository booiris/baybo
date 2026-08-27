import PhotosUI
import SwiftUI

struct IssueDock: View {
    @ObservedObject var store: IssueStore
    @ObservedObject var staging: ComposerStaging
    @ObservedObject var attach: AttachMenu
    @ObservedObject private var lang = Lang.shared

    @State private var photoPicks: [PhotosPickerItem] = []
    @FocusState private var focused: Bool
    @State private var caret: Int?
    @State private var completedDraft: String?
    /// `IssueDockUITests` addresses the field by this.
    static let fieldIdentifier = "issue.field"

    private var question: IssueTimeline.PendingQuestion? {
        IssueTimeline.agentQuestion(
            blockedReason: store.issue?.blockedReason, events: store.events)
    }

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
                store.resolveApproval(
                    callId: prompt.callId,
                    decision: decision == .deny ? .deny : .approve)
            })
    }

    // MARK: - Composer

    // MARK: - Mentions

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
            .frame(height: Self.chipHeight)
        }
    }

    private static let chipHeight: CGFloat = 32

    private func complete(_ handle: String) {
        guard let mention else { return }
        Haptics.tap()
        let completion = IssueMention.completion(
            for: mention, handle: handle, in: staging.text)
        // The DOCUMENT first, so a completion landing mid-draft leaves the
        // caret behind the handle instead of at the end of the comment.
        let wrote = FocusedTextInput.replace(completion.range, with: completion.text)
        if !wrote || staging.text != completion.draft {
            staging.text = completion.draft
        }
        caret = completion.range.lowerBound + completion.text.utf16.count
        completedDraft = completion.draft
    }

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

    private func clearField() {
        FocusedTextInput.clearDocument()
        staging.text = ""
    }

    private var dockBottomPadding: CGFloat { focused ? 12 : 0 }

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
