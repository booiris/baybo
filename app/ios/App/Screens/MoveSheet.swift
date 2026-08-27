import SwiftUI

/// Where to put a card, and what that will do.
///
/// **The consequence is the sheet.** A desktop board makes moving obvious by
/// making it a drag; a phone has no drag here, so every row says out loud what
/// the tap does — above all the two an operator gets wrong: that moving a card
/// OUT of In Progress does not stop its run, and that moving one IN starts
/// one. The sentences come from `MoveConsequence`, which is where they can be
/// tested without a screen.
struct MoveSheet: View {
    let issue: IssueInfo
    let liveRun: IssueRunInfo?
    let assigneeHandle: String?
    let overCeiling: Bool
    let heldCeiling: MoveConsequence.HeldCeiling
    /// Nil on a row that needs an assignee first — the caller opens the picker
    /// and moves afterwards.
    let onPick: (MoveConsequence.Row) -> Void

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    private var rows: [MoveConsequence.Row] {
        MoveConsequence.rows(
            for: issue, liveRun: liveRun, assigneeHandle: assigneeHandle,
            overCeiling: overCeiling, heldCeiling: heldCeiling)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: lang.t("board.moveTitle", "#\(issue.number)"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .padding(.horizontal, 20)
                .padding(.top, 22)
                .padding(.bottom, 12)

            ForEach(rows, id: \.status) { row in
                Button {
                    guard !row.isCurrent else { return }
                    Haptics.tap()
                    dismiss()
                    onPick(row)
                } label: {
                    rowBody(row)
                }
                .buttonStyle(.plain)
                .disabled(row.isCurrent)
                .accessibilityIdentifier("move-\(Self.identifier(row.status))")
            }

            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.paper)
        .presentationDetents([.height(430)])
        .presentationDragIndicator(.visible)
    }

    private func rowBody(_ row: MoveConsequence.Row) -> some View {
        HStack(alignment: .top, spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    // The bolt is the one mark that means "this press runs
                    // something", and it appears nowhere else on the board.
                    if row.startsRun {
                        Image(systemName: "bolt.fill")
                            .font(.system(size: 11, weight: .medium))
                            .foregroundStyle(Theme.ink)
                    }
                    Text(verbatim: MoveConsequence.label(row.status))
                        .font(Theme.sys(15, weight: row.isCurrent ? .regular : .medium))
                        .foregroundStyle(row.isCurrent ? Theme.inkSoft : Theme.ink)
                }
                if let note = row.note {
                    Text(verbatim: note)
                        .font(Theme.sys(12))
                        .foregroundStyle(Theme.inkSoft)
                        .multilineTextAlignment(.leading)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 6)
            if row.isCurrent {
                Image(systemName: "checkmark")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
            } else if row.needsAssignee {
                Image(systemName: "person.badge.plus")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .frame(minHeight: 56)
        .contentShape(Rectangle())
        .opacity(row.isCurrent ? 0.6 : 1)
    }

    static func identifier(_ status: IssueStatus) -> String {
        switch status {
        case .backlog: "backlog"
        case .todo: "todo"
        case .inProgress: "in-progress"
        case .review: "review"
        case .done: "done"
        case .unknown: "unknown"
        }
    }
}

/// How urgent this card is.
///
/// The board has never had one: priority is set when a card is filed and then
/// only ever read, so the one place that can change it is the card itself.
/// `AssigneePicker`'s grammar rather than `MoveSheet`'s — a priority has no
/// consequence to spell out, it orders no column and starts no run, so a row
/// is a mark, a word and a tick.
struct PriorityPicker: View {
    let current: IssuePriority
    let onPick: (IssuePriority) -> Void

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    /// Loudest first, the way the board's own filter and the New issue sheet
    /// order them — `none` last, because it is the absence of an answer rather
    /// than the quietest one.
    private static let levels: [IssuePriority] = [.urgent, .high, .medium, .low, .none]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: lang.t("board.priorityTitle"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .padding(.horizontal, 20)
                .padding(.top, 22)
                .padding(.bottom, 12)

            ForEach(Self.levels, id: \.self) { level in
                Button {
                    guard level != current else { return }
                    Haptics.tap()
                    dismiss()
                    onPick(level)
                } label: {
                    row(level)
                }
                .buttonStyle(.plain)
                .disabled(level == current)
                .accessibilityIdentifier(
                    "priority-\(IssueCardRow.priorityWord(level).lowercased().replacingOccurrences(of: " ", with: "-"))"
                )
            }

            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.paper)
        // Header + five 52pt rows. Its own number rather than the Move sheet's
        // 430: those rows are taller and each carries a sentence, and a detent
        // copied from them leaves this one standing in a hand's width of paper.
        .presentationDetents([.height(330)])
        .presentationDragIndicator(.visible)
    }

    private func row(_ level: IssuePriority) -> some View {
        HStack(spacing: 10) {
            // The board's own mark, so a level is the same shape wherever it
            // is read — a card row, the New issue sheet, here.
            Text(verbatim: IssueCardRow.priorityMark(level))
                .font(Theme.mono(12))
                .foregroundStyle(level == current ? Theme.inkSoft : Theme.ink)
                .frame(width: 26, alignment: .leading)
            Text(verbatim: IssueCardRow.priorityWord(level))
                .font(Theme.sys(15, weight: level == current ? .regular : .medium))
                .foregroundStyle(level == current ? Theme.inkSoft : Theme.ink)
            Spacer(minLength: 6)
            if level == current {
                Image(systemName: "checkmark")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
        .padding(.horizontal, 20)
        .frame(minHeight: 52)
        .contentShape(Rectangle())
        .opacity(level == current ? 0.6 : 1)
    }
}

/// Who is on this card.
///
/// Reached two ways, and the second is why it exists: from the ⋯ menu, and
/// from the Move sheet's In Progress row when nobody is assigned. A disabled
/// "needs an assignee" row would leave the operator to work out what to do
/// about it; this picks up where that row left off and finishes the move.
struct AssigneePicker: View {
    let team: [TeamMemberInfo]
    let current: String?
    let onPick: (String?) -> Void

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    private var monograms: [String: String] { AgentMonogram.map(for: team) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(verbatim: lang.t("board.assignTitle"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .padding(.horizontal, 20)
                .padding(.top, 22)
                .padding(.bottom, 12)

            ScrollView {
                VStack(spacing: 0) {
                    ForEach(team, id: \.id) { member in
                        Button {
                            Haptics.tap()
                            dismiss()
                            onPick(member.id)
                        } label: {
                            memberRow(member)
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("assign-\(member.handle)")
                    }
                    if current != nil {
                        Button {
                            Haptics.tap()
                            dismiss()
                            onPick(nil)
                        } label: {
                            Text(verbatim: lang.t("board.unassign"))
                                .font(Theme.sys(14))
                                .foregroundStyle(Theme.inkSoft)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(.horizontal, 20)
                                .frame(minHeight: 52)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("assign-nobody")
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.paper)
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
    }

    private func memberRow(_ member: TeamMemberInfo) -> some View {
        HStack(spacing: 10) {
            AgentFace(
                handle: member.handle, monogram: monograms[member.id],
                avatarBlobId: member.avatarBlobId, lead: member.lead)
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: "@\(member.handle)")
                    .font(Theme.sys(14, weight: .medium))
                    .foregroundStyle(Theme.ink)
                if !member.description.isEmpty {
                    Text(verbatim: member.description)
                        .font(Theme.sys(11.5))
                        .foregroundStyle(Theme.inkSoft)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 6)
            if member.id == current {
                Image(systemName: "checkmark")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.ink)
            }
        }
        .padding(.horizontal, 20)
        .frame(minHeight: 56)
        .contentShape(Rectangle())
    }
}
