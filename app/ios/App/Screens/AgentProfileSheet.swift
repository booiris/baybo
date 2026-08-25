import SwiftUI

/// One teammate: who it is, and what it runs on.
///
/// The model pin is the only thing here that writes, and it is a **whole
/// pin** — entry, model and thinking level replaced together, because absent
/// means "inherit" at each level on the server. Setting a model without an
/// entry is a 400 there; this screen therefore never offers one without the
/// other, and Clear sends all three as null rather than one.
struct AgentProfileSheet: View {
    let member: TeamMemberInfo
    let projectId: String
    var client: any BayboClientProtocol = Baybo.client
    /// The team sheet refetches after a write — this screen holds a snapshot,
    /// and a stale one behind a dismissed sheet is worse than a refetch.
    let onChanged: () -> Void

    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var catalog = ModelCatalog.shared
    @Environment(\.dismiss) private var dismiss
    @State private var saving = false
    @State private var error: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                head
                if let error {
                    Text(verbatim: error)
                        .font(Theme.sys(12.5))
                        .foregroundStyle(Theme.err)
                        .padding(.horizontal, 20)
                        .padding(.top, 12)
                }
                section(lang.t("agent.runsOn"))
                modelRows
                if !member.description.isEmpty {
                    section(lang.t("agent.brief"))
                    Text(verbatim: member.description)
                        .font(Theme.sys(13.5))
                        .foregroundStyle(Theme.ink)
                        .lineSpacing(3)
                        .padding(.horizontal, 20)
                }
                if let hiredBy = member.hiredBy {
                    section(lang.t("agent.hiredBy"))
                    Text(verbatim: "@\(hiredBy.handle)")
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.inkSoft)
                        .padding(.horizontal, 20)
                }
                Spacer(minLength: 40)
            }
            .padding(.top, 22)
        }
        .background(Theme.paper)
        .onAppear { catalog.refreshIfNeeded() }
    }

    private var head: some View {
        HStack(spacing: 12) {
            AgentFace(
                handle: member.handle, avatarBlobId: member.avatarBlobId,
                lead: member.lead, size: 40)
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: "@\(member.handle)")
                    .font(Theme.sys(17, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                Text(verbatim: member.framework)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 20)
    }

    /// One row per LLM entry, plus Clear.
    ///
    /// Entries only — not the entry→model→effort ladder the chat header's
    /// picker walks. An agent's pin is a board-level decision made rarely, and
    /// three levels of menu on a sheet inside a sheet is a place to get lost;
    /// the entry's own default model and level are what the server uses, which
    /// is the honest thing for a screen this size to promise.
    @ViewBuilder private var modelRows: some View {
        if catalog.models.isEmpty {
            Text(verbatim: lang.t("agent.noModels"))
                .font(Theme.sys(13))
                .foregroundStyle(Theme.inkSoft)
                .padding(.horizontal, 20)
        } else {
            VStack(spacing: 0) {
                ForEach(catalog.models, id: \.name) { entry in
                    pinRow(
                        title: entry.name,
                        subtitle: entry.model,
                        selected: member.llm == entry.name,
                        identifier: "agent-llm-\(entry.name)"
                    ) {
                        setPin(llm: entry.name, model: nil)
                    }
                }
                pinRow(
                    title: lang.t("agent.inherit"),
                    subtitle: lang.t("agent.inheritNote"),
                    selected: member.llm == nil,
                    identifier: "agent-llm-inherit"
                ) {
                    setPin(llm: nil, model: nil)
                }
            }
        }
    }

    private func pinRow(
        title: String, subtitle: String?, selected: Bool, identifier: String,
        action: @escaping () -> Void
    ) -> some View {
        Button {
            guard !saving, !selected else { return }
            Haptics.tap()
            action()
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: title)
                        .font(Theme.sys(14))
                        .foregroundStyle(Theme.ink)
                    if let subtitle, !subtitle.isEmpty {
                        Text(verbatim: subtitle)
                            .font(Theme.mono(10.5))
                            .foregroundStyle(Theme.inkSoft)
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 6)
                if selected {
                    Image(systemName: "checkmark")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                }
            }
            .padding(.horizontal, 20)
            .frame(minHeight: 52)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(saving)
        .opacity(saving ? 0.5 : 1)
        .accessibilityIdentifier(identifier)
    }

    private func section(_ title: String) -> some View {
        Text(verbatim: title)
            .font(Theme.mono(10.5))
            .textCase(.uppercase)
            .kerning(1.2)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 20)
            .padding(.top, 22)
            .padding(.bottom, 8)
    }

    /// The pin is replaced WHOLE. Clearing means all three levels at once —
    /// anything less leaves two thirds of a pin pointing at an entry the agent
    /// no longer uses.
    private func setPin(llm: String?, model: String?) {
        saving = true
        error = nil
        Task {
            do {
                try await client.agentSetModel(
                    agentId: member.id, llm: llm, model: model, reasoningEffort: nil)
                onChanged()
                dismiss()
            } catch {
                self.error = ProjectsStore.message(from: error)
            }
            saving = false
        }
    }
}
