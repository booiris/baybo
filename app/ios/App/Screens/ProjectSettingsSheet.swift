import SwiftUI

/// The settings PUT replaces the whole record, so every field must be sent on
/// every save; omission would silently clear an untouched value.
struct ProjectSettingsSheet: View {
    let project: ProjectInfo
    var client: any BayboClientProtocol = Baybo.client
    let onSaved: () -> Void

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    @State private var name: String
    @State private var description: String
    @State private var budgetUsd: String
    @State private var budgetTokens: String
    @State private var parallelRuns: String
    @State private var autoMerge: Bool
    @State private var saving = false
    @State private var error: String?
    @State private var confirmingArchive = false

    init(project: ProjectInfo, client: any BayboClientProtocol = Baybo.client, onSaved: @escaping () -> Void) {
        self.project = project
        self.client = client
        self.onSaved = onSaved
        _name = State(initialValue: project.name)
        _description = State(initialValue: project.description)
        _budgetUsd = State(
            initialValue: project.dailyBudgetMicros.map {
                String(format: "%.2f", Double($0) / 1_000_000)
            } ?? "")
        _budgetTokens = State(initialValue: project.dailyBudgetTokens.map(String.init) ?? "")
        _parallelRuns = State(initialValue: String(project.maxParallelIssueRuns))
        _autoMerge = State(initialValue: project.agentsMayMerge)
    }

    private var isArchived: Bool { project.archivedAtMs != nil }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                titleRow
                if let error {
                    Text(verbatim: error)
                        .font(Theme.sys(12.5))
                        .foregroundStyle(Theme.err)
                        .padding(.horizontal, 20)
                        .padding(.bottom, 8)
                }
                field(lang.t("newProject.name"), $name, "settings-name")
                field(lang.t("newProject.description"), $description, "settings-description")
                section(lang.t("newProject.ceilings"))
                field(
                    lang.t("newProject.budget"), $budgetUsd, "settings-budget",
                    keyboard: .decimalPad, hint: lang.t("newProject.budgetNote"))
                field(
                    lang.t("newProject.tokenBudget"), $budgetTokens, "settings-token-budget",
                    keyboard: .numberPad, hint: lang.t("newProject.tokenBudgetNote"))
                field(
                    lang.t("newProject.parallelRuns"), $parallelRuns, "settings-parallel",
                    keyboard: .numberPad, hint: lang.t("newProject.parallelRunsNote"))

                section(lang.t("settings.finishing"))
                autoMergeRow

                Button(lang.t("common.save")) { save() }
                    .buttonStyle(InkPillButtonStyle())
                    .disabled(saving || name.trimmingCharacters(in: .whitespaces).isEmpty)
                    .opacity(saving ? 0.5 : 1)
                    .padding(.horizontal, 20)
                    .padding(.top, 24)
                    .accessibilityIdentifier("settings-save")

                section(lang.t("settings.workdir"))
                Text(verbatim: project.workdir)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .padding(.horizontal, 20)

                archiveRow
                Spacer(minLength: 50)
            }
            .padding(.top, 6)
        }
        .background(Theme.paper)
        .alert(
            lang.t(isArchived ? "settings.unarchiveTitle" : "settings.archiveTitle"),
            isPresented: $confirmingArchive
        ) {
            Button(lang.t("common.cancel"), role: .cancel) {}
            if isArchived {
                Button(lang.t("settings.unarchive")) {
                    setArchived(false)
                }
            } else {
                Button(lang.t("settings.archive"), role: .destructive) {
                    setArchived(true)
                }
            }
        } message: {
            Text(verbatim: lang.t(isArchived ? "settings.unarchiveExplain" : "settings.archiveExplain"))
        }
    }

    private var titleRow: some View {
        ZStack {
            Text(verbatim: lang.t("settings.title"))
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
            HStack {
                Spacer()
                Button(lang.t("common.done")) { dismiss() }
                    .buttonStyle(LinkButtonStyle())
            }
        }
        .padding(.horizontal, 16)
        .frame(height: 52)
    }

    private var archiveRow: some View {
        Button {
            Haptics.tap()
            confirmingArchive = true
        } label: {
            Text(verbatim: lang.t(isArchived ? "settings.unarchive" : "settings.archive"))
        }
        .buttonStyle(OutlinePillButtonStyle(color: isArchived ? Theme.ink : Theme.err))
        .padding(.horizontal, 20)
        .padding(.top, 24)
        .accessibilityIdentifier("settings-archive")
    }

    private func section(_ title: String) -> some View {
        Text(verbatim: title)
            .font(Theme.mono(10.5))
            .textCase(.uppercase)
            .kerning(1.2)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 20)
            .padding(.top, 22)
            .padding(.bottom, 4)
    }

    private var autoMergeRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            Toggle(isOn: $autoMerge) {
                Text(verbatim: lang.t("settings.autoMerge"))
                    .font(Theme.sys(15))
                    .foregroundStyle(Theme.ink)
            }
            .tint(Theme.ink)
            .disabled(saving)
            .accessibilityIdentifier("settings-auto-merge")
            Text(verbatim: lang.t(autoMerge ? "settings.autoMergeOn" : "settings.autoMergeOff"))
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.horizontal, 20)
    }

    private func field(
        _ label: String, _ text: Binding<String>, _ identifier: String,
        keyboard: UIKeyboardType = .default, hint: String? = nil
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: label)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            TextField("", text: text)
                .font(Theme.sys(15))
                .foregroundStyle(Theme.ink)
                .keyboardType(keyboard)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .padding(.horizontal, 14)
                .frame(minHeight: 46)
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                        .strokeBorder(Theme.lineStrong, lineWidth: 1)
                )
                .accessibilityIdentifier(identifier)
            if let hint {
                Text(verbatim: hint)
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineSpacing(2)
            }
        }
        .padding(.horizontal, 20)
        .padding(.top, 12)
    }

    private func save() {
        saving = true
        error = nil
        Task {
            do {
                try await client.projectUpdate(
                    projectId: project.id,
                    settings: ProjectSettings(
                        name: name.trimmingCharacters(in: .whitespacesAndNewlines),
                        description: description,
                        dailyBudgetMicros: NewProjectScreen.micros(fromUsd: budgetUsd),
                        dailyBudgetTokens: Int64(budgetTokens.trimmingCharacters(in: .whitespaces)),
                        maxParallelIssueRuns: Int64(
                            parallelRuns.trimmingCharacters(in: .whitespaces)),
                        agentsMayMerge: autoMerge))
                onSaved()
                dismiss()
            } catch {
                self.error = ProjectsStore.message(from: error)
            }
            saving = false
        }
    }

    private func setArchived(_ archived: Bool) {
        Task {
            do {
                _ = try await client.projectSetArchived(
                    projectId: project.id, archived: archived)
                onSaved()
                dismiss()
            } catch {
                self.error = ProjectsStore.message(from: error)
            }
        }
    }
}
