import SwiftUI

/// A board's own settings.
///
/// **The PUT is a full replace**, so this form sends every field back on every
/// save — including the ones nobody touched. A partial body would clear a
/// budget by omitting it, which is the same shape of bug as a partial agent
/// pin and fails just as silently: nothing errors, the ceiling is simply gone.
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
        .confirmationDialog(
            lang.t(isArchived ? "settings.unarchiveTitle" : "settings.archiveTitle"),
            isPresented: $confirmingArchive, titleVisibility: .visible
        ) {
            Button(lang.t(isArchived ? "settings.unarchive" : "settings.archive")) {
                setArchived(!isArchived)
            }
            Button(lang.t("common.cancel"), role: .cancel) {}
        } message: {
            // Says the part an operator gets wrong: archiving does NOT stop
            // what is already running. It stops the board taking writes and
            // answering approvals — so a run mid-flight finishes, and its gate
            // self-denies.
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
                .font(Theme.sys(14))
                .foregroundStyle(isArchived ? Theme.ink : Theme.err)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 20)
                .frame(minHeight: 52)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.top, 22)
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
                // Every field, every time — see the type doc. An empty budget
                // is `nil`, which is what "no ceiling" means to the server; a
                // blank field is not the same as an unsent one.
                try await client.projectUpdate(
                    projectId: project.id,
                    settings: ProjectSettings(
                        name: name.trimmingCharacters(in: .whitespacesAndNewlines),
                        description: description,
                        dailyBudgetMicros: NewProjectScreen.micros(fromUsd: budgetUsd),
                        dailyBudgetTokens: Int64(budgetTokens.trimmingCharacters(in: .whitespaces)),
                        maxParallelIssueRuns: Int64(
                            parallelRuns.trimmingCharacters(in: .whitespaces))))
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
