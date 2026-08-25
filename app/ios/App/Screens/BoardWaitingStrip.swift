import SwiftUI

/// The board's "Waiting on you" strip: the tool calls parked on an approval
/// gate, and nothing else.
///
/// **Only approvals**, and the phrase is the reason. Something is *waiting on
/// you* when it has stopped and cannot go on until a person answers. On a
/// board that is true of a parked prompt and of nothing else:
///
/// - a **failed run** is over, not waiting — nothing is blocked on an answer,
///   and the card wears `✕ Run failed`;
/// - an **unread card** is news — nobody is stopped, and the card wears a red
///   count while its segment wears a dot;
/// - an agent's **question** does park a run, but it is answered by writing a
///   sentence, and no sentence fits in a strip row — so the card wears
///   `⊘ Blocked` and the answering happens where the writing happens.
///
/// Each of those already says itself on the card row. A strip that repeated
/// them was a third place for the same fact, and it filled up with rows whose
/// only affordance was "go and look" — which is what the list underneath it
/// already is.
///
/// The strip carries the current board only. A cross-board inbox was the other
/// candidate and was rejected: it would be a second place a card can be acted
/// on, with its own idea of what is urgent, and the cards root already carries
/// the per-board count.
struct BoardWaitingStrip: View {
    let items: [Item]
    let onApprove: (Int64, String, IssueApprovalDecision) -> Void
    let onOpen: (Int64) -> Void

    @ObservedObject private var lang = Lang.shared

    /// One parked prompt.
    ///
    /// A struct rather than an enum now that there is one kind: an enum with a
    /// single case is a switch nobody will ever add a branch to, and the four
    /// it used to have made three dead shapes look like live ones.
    struct Item: Identifiable, Equatable {
        let number: Int64
        let title: String
        let prompt: IssueApprovalPrompt

        var id: String { "approval-\(number)-\(prompt.callId)" }
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
            .background(
                Theme.surface, in: RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .strokeBorder(Theme.line, lineWidth: 1)
            )
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("waiting-strip")
    }

    private func row(_ item: Item) -> some View {
        HStack(alignment: .center, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(
                    verbatim: lang.t(
                        "board.waitingApproval",
                        item.prompt.askedBy.map { "@\($0)" } ?? lang.t("board.anAgent"),
                        "#\(item.number)", item.title)
                )
                .font(Theme.sys(12.5, weight: .medium))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                Text(verbatim: item.prompt.summary ?? item.prompt.tool)
                    .font(Theme.sys(11.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            // The text column opens the card. It must NOT answer — the two
            // buttons are the only thing that answers.
            .contentShape(Rectangle())
            .onTapGesture { onOpen(item.number) }

            // Both buttons are `.plain`-isolated on purpose: inside a row that
            // is itself tappable, a default-styled button hands its press to
            // the row, and Approve would open the card instead of approving.
            HStack(spacing: 6) {
                Button(lang.t("board.deny")) {
                    onApprove(item.number, item.prompt.callId, .deny)
                }
                .buttonStyle(CompactPillButtonStyle(fill: nil, color: Theme.err, expands: false))
                .accessibilityIdentifier("waiting-deny-\(item.number)")
                Button(lang.t("board.approve")) {
                    onApprove(item.number, item.prompt.callId, .approve)
                }
                .buttonStyle(
                    CompactPillButtonStyle(fill: Theme.ink, color: Theme.paper, expands: false))
                .accessibilityIdentifier("waiting-approve-\(item.number)")
            }
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
}

/// Building the strip from a board.
///
/// Kept apart from the view so what it contains is testable without a screen.
enum BoardWaiting {
    static func items(
        issues: [IssueInfo],
        prompts: [Int64: [IssueApprovalPrompt]]
    ) -> [BoardWaitingStrip.Item] {
        // A cancelled card waits for nothing: it is terminal, and the run that
        // was on it does not come back on its own.
        issues
            .filter { $0.cancelledAtMs == nil }
            .flatMap { issue in
                // Several prompts on one card are several rows: each is
                // answered by its own `call_id`, and collapsing them would
                // leave one unanswerable.
                (prompts[issue.number] ?? []).map {
                    BoardWaitingStrip.Item(number: issue.number, title: issue.title, prompt: $0)
                }
            }
    }
}
