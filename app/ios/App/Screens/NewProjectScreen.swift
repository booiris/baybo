import SwiftUI

struct NewProjectScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var description = ""
    @State private var budgetUsd = ""
    @State private var budgetTokens = ""
    @State private var parallelRuns = ""
    @State private var creating = false
    @State private var failure: String?
    @FocusState private var nameFocused: Bool

    private var canCreate: Bool {
        !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !creating
    }

    var body: some View {
        ZStack(alignment: .top) {
            form
            header
        }
        .background(Theme.paper)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
    }

    private var form: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                field(
                    label: lang.t("newProject.name"),
                    text: $name,
                    placeholder: lang.t("newProject.namePlaceholder"),
                    hint: lang.t("newProject.workdirNote"),
                    identifier: "new-project-name"
                )
                .focused($nameFocused)

                field(
                    label: lang.t("newProject.description"),
                    text: $description,
                    placeholder: lang.t("newProject.descriptionPlaceholder"),
                    hint: nil,
                    identifier: "new-project-description"
                )

                sectionTitle(lang.t("newProject.ceilings"))

                field(
                    label: lang.t("newProject.budget"),
                    text: $budgetUsd,
                    placeholder: lang.t("newProject.budgetPlaceholder"),
                    hint: lang.t("newProject.budgetNote"),
                    identifier: "new-project-budget",
                    keyboard: .decimalPad
                )

                field(
                    label: lang.t("newProject.tokenBudget"),
                    text: $budgetTokens,
                    placeholder: lang.t("newProject.tokenBudgetPlaceholder"),
                    hint: lang.t("newProject.tokenBudgetNote"),
                    identifier: "new-project-token-budget",
                    keyboard: .numberPad
                )

                field(
                    label: lang.t("newProject.parallelRuns"),
                    text: $parallelRuns,
                    placeholder: "3",
                    hint: lang.t("newProject.parallelRunsNote"),
                    identifier: "new-project-parallel",
                    keyboard: .numberPad
                )

                if let failure {
                    Text(verbatim: failure)
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.err)
                        .padding(.top, 16)
                        .padding(.horizontal, ProjectsLayout.gutter)
                }

                Button(lang.t("newProject.create")) { create() }
                    .buttonStyle(InkPillButtonStyle())
                    .disabled(!canCreate)
                    .opacity(canCreate ? 1 : 0.35)
                    .padding(.horizontal, ProjectsLayout.gutter)
                    .padding(.top, 22)
                    .accessibilityIdentifier("new-project-create")

                Text(verbatim: lang.t("newProject.leadNote"))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .multilineTextAlignment(.center)
                    .lineSpacing(3)
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, 40)
                    .padding(.top, 10)
                    .padding(.bottom, 60)
            }
        }
        .contentMargins(.top, ProjectsLayout.topInset, for: .scrollContent)
        .scrollContentBackground(.hidden)
        .scrollDismissesKeyboard(.interactively)
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(11))
            .textCase(.uppercase)
            .kerning(1.5)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, ProjectsLayout.gutter)
            .padding(.top, 22)
    }

    private func field(
        label: String,
        text: Binding<String>,
        placeholder: String,
        hint: String?,
        identifier: String,
        keyboard: UIKeyboardType = .default
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: label)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            TextField(placeholder, text: text)
                .font(Theme.sys(15))
                .foregroundStyle(Theme.ink)
                .keyboardType(keyboard)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .padding(.horizontal, 14)
                .frame(minHeight: 48)
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
        .padding(.horizontal, ProjectsLayout.gutter)
        .padding(.top, 12)
    }

    private var header: some View {
        ZStack {
            Text(verbatim: lang.t("newProject.title"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)
            HStack {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("board.back")))
                Spacer()
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) {
            LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .top)
                .allowsHitTesting(false)
        }
    }

    private func create() {
        guard canCreate else { return }
        creating = true
        failure = nil
        Haptics.tap()
        Task {
            do {
                let project = try await Baybo.client.projectCreate(
                    new: NewProject(
                        name: name.trimmingCharacters(in: .whitespacesAndNewlines),
                        description: description,
                        workdir: nil,
                        dailyBudgetMicros: Self.micros(fromUsd: budgetUsd),
                        dailyBudgetTokens: Int64(budgetTokens.trimmingCharacters(in: .whitespaces)),
                        maxParallelIssueRuns: Int64(
                            parallelRuns.trimmingCharacters(in: .whitespaces))
                    ))
                await appStore.projectsStore.refreshRoot()
                creating = false
                // Straight into the new board — its lead is hired with it, so
                // there is something to look at.
                appStore.chatPath.removeAll { $0 == .newProject }
                appStore.openProjectBoard(project.id)
            } catch {
                creating = false
                failure = ProjectsStore.message(from: error)
            }
        }
    }

    /// Dollars in the field, micro-USD on the wire. Parsed leniently — a blank
    /// field means no ceiling, which is what absent means to the server.
    static func micros(fromUsd text: String) -> Int64? {
        let trimmed = text.trimmingCharacters(in: .whitespaces).replacingOccurrences(
            of: "$", with: "")
        guard !trimmed.isEmpty, let dollars = Double(trimmed), dollars >= 0 else { return nil }
        return Int64((dollars * 1_000_000).rounded())
    }
}
