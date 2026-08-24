import SwiftUI

/// The board's "Waiting on you" strip: everything on THIS board that has
/// stopped and is holding for a person.
///
/// Compact rows rather than whole cards, because the point is the count and
/// the answer, not the card — a strip that reprinted four full cards would
/// push the board itself off the screen. Each row's press is the answer,
/// except the unread one, which can only be discharged by opening the card.
///
/// The strip carries the current board only. A cross-board inbox was the other
/// candidate and was rejected: it would be a second place a card can be acted
/// on, with its own idea of what is urgent, and the cards root already carries
/// the per-board counts that say which board to enter.
struct BoardWaitingStrip: View {
    let items: [Item]
    let onApprove: (Int64, String, IssueApprovalDecision) -> Void
    let onRetry: (Int64) -> Void
    let onOpen: (Int64) -> Void

    @ObservedObject private var lang = Lang.shared

    /// One thing waiting, in the order the board wants them answered.
    ///
    /// The four kinds are deliberately not one "needs attention" row: they are
    /// answered by four different presses, and a strip that made them look
    /// alike would make three of them lie about what tapping does.
    enum Item: Identifiable, Equatable {
        /// A tool call parked on the gate. Answerable in place.
        case approval(number: Int64, title: String, prompt: IssueApprovalPrompt)
        /// The last attempt failed. One press starts another.
        case failed(number: Int64, title: String, error: String?)
        /// An AGENT wrote the block, so it is a question. An operator's own
        /// block is not — nothing should invite somebody to answer themselves.
        case question(number: Int64, title: String, askedBy: String, question: String)
        /// Something was said on the card while you were elsewhere. Opening it
        /// is the only thing that clears it.
        case unread(number: Int64, title: String, count: Int64)

        var id: String {
            switch self {
            case let .approval(number, _, prompt): "approval-\(number)-\(prompt.callId)"
            case let .failed(number, _, _): "failed-\(number)"
            case let .question(number, _, _, _): "question-\(number)"
            case let .unread(number, _, _): "unread-\(number)"
            }
        }

        var number: Int64 {
            switch self {
            case let .approval(number, _, _): number
            case let .failed(number, _, _): number
            case let .question(number, _, _, _): number
            case let .unread(number, _, _): number
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Text(verbatim: lang.t("board.waitingOnYou"))
                    .font(Theme.mono(10.5))
                    .textCase(.uppercase)
                    .kerning(1.2)
                Text(verbatim: "\(items.count)")
                    .font(Theme.mono(10.5))
                Spacer()
            }
            .foregroundStyle(Theme.inkSoft)
            .padding(.bottom, 6)

            VStack(spacing: 0) {
                ForEach(items) { item in
                    row(item)
                    if item.id != items.last?.id {
                        Rectangle().fill(Theme.line).frame(height: 1)
                    }
                }
            }
            .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.radius, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .strokeBorder(Theme.line, lineWidth: 1)
            )
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("waiting-strip")
    }

    @ViewBuilder private func row(_ item: Item) -> some View {
        HStack(alignment: .center, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: headline(item))
                    .font(Theme.sys(12.5, weight: .medium))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                if let detail = detail(item) {
                    Text(verbatim: detail)
                        .font(Theme.sys(11.5))
                        .foregroundStyle(Theme.inkSoft)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // The whole text column opens the card. Tapping it must NOT
            // discharge whatever the row is asking — the answer buttons are
            // the only thing that answers.
            .contentShape(Rectangle())
            .onTapGesture { onOpen(item.number) }

            answers(item)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        // A row is a CONTAINER, and saying so is load-bearing: it carries a tap
        // gesture and an identifier, and SwiftUI folds such a container into a
        // single element — which took its Deny and Approve buttons out of the
        // accessibility tree entirely. They still drew, and still worked under
        // a finger; they simply stopped existing for anything that reads the
        // tree, VoiceOver included.
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("waiting-row-\(item.number)")
    }

    /// Every button here is `.buttonStyle(.plain)`-isolated on purpose: inside
    /// a row that is itself tappable, a default-styled button hands its press
    /// to the row, and Approve would open the card instead of approving.
    @ViewBuilder private func answers(_ item: Item) -> some View {
        switch item {
        case let .approval(number, _, prompt):
            HStack(spacing: 6) {
                Button(lang.t("board.deny")) { onApprove(number, prompt.callId, .deny) }
                    .buttonStyle(
                        CompactPillButtonStyle(fill: nil, color: Theme.err, expands: false))
                    .accessibilityIdentifier("waiting-deny-\(number)")
                Button(lang.t("board.approve")) { onApprove(number, prompt.callId, .approve) }
                    .buttonStyle(
                        CompactPillButtonStyle(
                            fill: Theme.ink, color: Theme.paper, expands: false))
                    .accessibilityIdentifier("waiting-approve-\(number)")
            }
        case let .failed(number, _, _):
            Button(lang.t("board.runAgain")) { onRetry(number) }
                .buttonStyle(CompactPillButtonStyle(fill: nil, color: Theme.ink, expands: false))
                .accessibilityIdentifier("waiting-retry-\(number)")
        case let .question(number, _, _, _):
            Button(lang.t("board.answer")) { onOpen(number) }
                .buttonStyle(
                    CompactPillButtonStyle(fill: Theme.ink, color: Theme.paper, expands: false))
                .accessibilityIdentifier("waiting-answer-\(number)")
        case let .unread(_, _, count):
            // No button: opening the card is what clears an unread, and a
            // "Mark read" here would let the count go without the thing that
            // caused it ever being looked at.
            Text(verbatim: count > 99 ? "99+" : "\(count)")
                .font(Theme.sys(10, weight: .medium))
                .foregroundStyle(Theme.paper)
                .padding(.horizontal, 5)
                .frame(minWidth: 16, minHeight: 16)
                .background(Theme.err, in: Capsule())
        }
    }

    private func headline(_ item: Item) -> String {
        switch item {
        case let .approval(number, title, prompt):
            let who = prompt.askedBy.map { "@\($0)" } ?? lang.t("board.anAgent")
            return lang.t("board.waitingApproval", who, "#\(number)", title)
        case let .failed(number, title, _):
            return lang.t("board.waitingFailed", "#\(number)", title)
        case let .question(number, title, askedBy, _):
            return lang.t("board.waitingQuestion", "@\(askedBy)", "#\(number)", title)
        case let .unread(number, title, _):
            return lang.t("board.waitingUnread", "#\(number)", title)
        }
    }

    private func detail(_ item: Item) -> String? {
        switch item {
        case let .approval(_, _, prompt): prompt.summary ?? prompt.tool
        case let .failed(_, _, error): error
        case let .question(_, _, _, question): question
        case .unread: nil
        }
    }
}

/// Building the strip from a board.
///
/// Kept apart from the view so the ordering is testable without a screen: the
/// order IS the design — an approval blocks a running agent right now, a
/// failed run has already stopped, a question is waiting on a sentence, and an
/// unread is only news.
enum BoardWaiting {
    static func items(
        issues: [IssueInfo],
        runs: [IssueRunInfo],
        prompts: [Int64: [IssueApprovalPrompt]],
        blockedQuestions: [Int64: IssueTimeline.PendingQuestion]
    ) -> [BoardWaitingStrip.Item] {
        // A cancelled card waits for nothing: it is terminal, and the run that
        // was on it does not come back on its own.
        let live = issues.filter { $0.cancelledAtMs == nil }
        var out: [BoardWaitingStrip.Item] = []

        for issue in live {
            for prompt in prompts[issue.number] ?? [] {
                out.append(.approval(number: issue.number, title: issue.title, prompt: prompt))
            }
        }
        for issue in live where issue.lastRunFailed {
            let error = runs.first { $0.number == issue.number && $0.status == .failed }?.error
            out.append(.failed(number: issue.number, title: issue.title, error: error))
        }
        for issue in live {
            guard let question = blockedQuestions[issue.number] else { continue }
            out.append(
                .question(
                    number: issue.number, title: issue.title, askedBy: question.askedBy,
                    question: question.question))
        }
        for issue in live where issue.unread > 0 {
            // A card already in the strip for a reason that can be ANSWERED
            // does not also queue as news: the answer discharges the visit, and
            // the same card twice makes the count say two things are waiting.
            guard !out.contains(where: { $0.number == issue.number }) else { continue }
            out.append(.unread(number: issue.number, title: issue.title, count: issue.unread))
        }
        return out
    }
}
