import SwiftUI

/// Narrowing the board.
///
/// Everything here applies to the CURRENT stage only, which is why there is no
/// status row: the segmented control above is the status filter, and a second
/// one in here would be two controls for one question.
struct BoardFilterSheet: View {
    @Binding var filter: BoardFilter
    let team: [TeamMemberInfo]

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    private var monograms: [String: String] { AgentMonogram.map(for: team) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(verbatim: lang.t("board.filter"))
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                Spacer()
                if filter.isActive {
                    Button(lang.t("board.clearFilter")) {
                        Haptics.tap()
                        filter.clear()
                    }
                    .buttonStyle(LinkButtonStyle())
                    .accessibilityIdentifier("filter-clear")
                }
            }
            .padding(.horizontal, 20)
            .padding(.top, 22)

            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    toggleRow(
                        lang.t("board.runningOnly"), isOn: filter.runningOnly,
                        identifier: "filter-running"
                    ) {
                        filter.runningOnly.toggle()
                    }
                    toggleRow(
                        lang.t("board.showCancelled"), isOn: filter.showsCancelled,
                        identifier: "filter-cancelled"
                    ) {
                        filter.showsCancelled.toggle()
                    }

                    sectionTitle(lang.t("board.filterPriority"))
                    HStack(spacing: 6) {
                        ForEach(
                            [IssuePriority.urgent, .high, .medium, .low], id: \.self
                        ) { priority in
                            chip(
                                IssueCardRow.priorityWord(priority),
                                selected: filter.priority == priority,
                                identifier: "filter-priority-\(IssueCardRow.priorityWord(priority).lowercased())"
                            ) {
                                filter.priority = filter.priority == priority ? nil : priority
                            }
                        }
                    }
                    .padding(.horizontal, 20)

                    if !team.isEmpty {
                        sectionTitle(lang.t("board.filterAssignee"))
                        VStack(spacing: 0) {
                            ForEach(team, id: \.id) { member in
                                Button {
                                    Haptics.tap()
                                    filter.assignee =
                                        filter.assignee == member.id ? nil : member.id
                                } label: {
                                    HStack(spacing: 10) {
                                        AgentFace(
                                            handle: member.handle,
                                            monogram: monograms[member.id],
                                            avatarBlobId: member.avatarBlobId,
                                            lead: member.lead)
                                        Text(verbatim: "@\(member.handle)")
                                            .font(Theme.sys(14))
                                            .foregroundStyle(Theme.ink)
                                        Spacer()
                                        if filter.assignee == member.id {
                                            Image(systemName: "checkmark")
                                                .font(.system(size: 12, weight: .semibold))
                                                .foregroundStyle(Theme.ink)
                                        }
                                    }
                                    .padding(.horizontal, 20)
                                    .frame(minHeight: 50)
                                    .contentShape(Rectangle())
                                }
                                .buttonStyle(.plain)
                                .accessibilityIdentifier("filter-assignee-\(member.handle)")
                            }
                        }
                    }
                }
                .padding(.bottom, 30)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.paper)
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(10.5))
            .textCase(.uppercase)
            .kerning(1.2)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 20)
            .padding(.top, 20)
            .padding(.bottom, 8)
    }

    private func toggleRow(
        _ title: String, isOn: Bool, identifier: String, action: @escaping () -> Void
    ) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            HStack {
                Text(verbatim: title)
                    .font(Theme.sys(14))
                    .foregroundStyle(Theme.ink)
                Spacer()
                Image(systemName: isOn ? "checkmark.circle.fill" : "circle")
                    .font(.system(size: 17))
                    .foregroundStyle(isOn ? Theme.ink : Theme.lineStrong)
            }
            .padding(.horizontal, 20)
            .frame(minHeight: 50)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier(identifier)
        .accessibilityValue(Text(verbatim: isOn ? "1" : "0"))
    }

    private func chip(
        _ title: String, selected: Bool, identifier: String, action: @escaping () -> Void
    ) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Text(verbatim: title)
                .font(Theme.mono(11))
                .foregroundStyle(selected ? Theme.paper : Theme.inkSoft)
                .padding(.horizontal, 10)
                .frame(minHeight: 32)
                .frame(maxWidth: .infinity)
                .background(selected ? Theme.ink : Color.clear, in: Capsule())
                .overlay(
                    Capsule().strokeBorder(selected ? Color.clear : Theme.lineStrong, lineWidth: 1))
                .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier(identifier)
    }
}
