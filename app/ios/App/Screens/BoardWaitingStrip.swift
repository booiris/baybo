import SwiftUI

struct BoardWaitingStrip: View {
    let items: [Item]
    let onApprove: (Int64, String, IssueApprovalDecision) -> Void
    let onOpen: (Int64) -> Void

    @ObservedObject private var lang = Lang.shared

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
                        Rectangle().fill(Theme.lineStrong).frame(height: 1)
                    }
                }
            }
            .background(
                Theme.surface, in: RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
            )
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .strokeBorder(Theme.lineStrong, lineWidth: 1)
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
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("waiting-row-\(item.number)")
    }
}

enum BoardWaiting {
    static func items(
        issues: [IssueInfo],
        prompts: [Int64: [IssueApprovalPrompt]]
    ) -> [BoardWaitingStrip.Item] {
        issues
            .filter { $0.cancelledAtMs == nil }
            .flatMap { issue in
                (prompts[issue.number] ?? []).map {
                    BoardWaitingStrip.Item(number: issue.number, title: issue.title, prompt: $0)
                }
            }
    }
}
