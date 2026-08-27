import PhotosUI
import SwiftUI

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
    @State private var facePick: PhotosPickerItem?
    @State private var level: Level = .profile

    private enum Level: Equatable {
        case profile
        case llm
        case model
        case effort
    }

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
                if level == .profile {
                    profileBody
                } else {
                    pickerBody
                }
                Spacer(minLength: 40)
            }
            .padding(.top, 22)
        }
        .background(Theme.paper)
        .onAppear { catalog.refreshIfNeeded() }
        .onChange(of: facePick) { _, pick in
            guard let pick else { return }
            facePick = nil
            setFace(pick)
        }
    }

    private var head: some View {
        HStack(spacing: 12) {
            PhotosPicker(selection: $facePick, matching: .images) {
                AgentFace(
                    handle: member.handle, avatarBlobId: member.avatarBlobId,
                    lead: member.lead, size: 40)
                    .overlay(alignment: .bottomTrailing) {
                        Image(systemName: "pencil")
                            .font(.system(size: 8, weight: .semibold))
                            .foregroundStyle(Theme.paper)
                            .frame(width: 15, height: 15)
                            .background(Theme.ink, in: Circle())
                            .overlay(Circle().strokeBorder(Theme.paper, lineWidth: 1.5))
                            .offset(x: 3, y: 3)
                    }
            }
            .buttonStyle(.plain)
            .disabled(saving)
            .accessibilityIdentifier("agent-avatar-pick")
            .accessibilityLabel(Text(verbatim: lang.t("agent.changeFace")))
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

    @ViewBuilder private var profileBody: some View {
        section(lang.t("agent.runsOn"))
        pinRows
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
    }

    @ViewBuilder private var pinRows: some View {
        if catalog.models.isEmpty {
            Text(verbatim: lang.t("agent.noModels"))
                .font(Theme.sys(13))
                .foregroundStyle(Theme.inkSoft)
                .padding(.horizontal, 20)
        } else {
            VStack(spacing: 0) {
                fieldRow(
                    label: lang.t("agent.llm"), value: label(in: entryRows, for: member.llm),
                    identifier: "agent-field-llm"
                ) { level = .llm }
                fieldRow(
                    label: lang.t("agent.model"), value: label(in: modelRows, for: member.model),
                    identifier: "agent-field-model"
                ) { level = .model }
                if !effortRows.isEmpty {
                    fieldRow(
                        label: lang.t("agent.thinking"),
                        value: label(in: effortRows, for: member.reasoningEffort),
                        identifier: "agent-field-effort"
                    ) { level = .effort }
                }
            }
        }
    }

    /// The open field's rows, under a back row that names it.
    @ViewBuilder private var pickerBody: some View {
        backRow(title: levelTitle)
        VStack(spacing: 0) {
            ForEach(openRows, id: \.label) { row in
                optionRow(row)
            }
        }
    }

    private var entryRows: [LlmPinOptions.Row] {
        LlmPinOptions.entryRows(
            entries: catalog.models, defaultName: catalog.defaultName, pinned: member.llm)
    }

    private var modelRows: [LlmPinOptions.Row] {
        LlmPinOptions.modelRows(
            entries: catalog.models, defaultName: catalog.defaultName, entry: member.llm,
            pinned: member.model)
    }

    private var effortRows: [LlmPinOptions.Row] {
        LlmPinOptions.effortRows(
            entries: catalog.models, defaultName: catalog.defaultName, entry: member.llm,
            pinned: member.reasoningEffort)
    }

    private var openRows: [LlmPinOptions.Row] {
        switch level {
        case .profile: []
        case .llm: entryRows
        case .model: modelRows
        case .effort: effortRows
        }
    }

    private var levelTitle: String {
        switch level {
        case .profile: ""
        case .llm: lang.t("agent.llm")
        case .model: lang.t("agent.model")
        case .effort: lang.t("agent.thinking")
        }
    }

    private var pickedAtOpenLevel: String? {
        switch level {
        case .profile: nil
        case .llm: member.llm
        case .model: member.model
        case .effort: member.reasoningEffort
        }
    }

    private func label(in rows: [LlmPinOptions.Row], for value: String?) -> String {
        rows.first { $0.value == value }?.label ?? value ?? ""
    }

    /// Picking writes the whole triple and returns to the profile.
    private func pick(_ row: LlmPinOptions.Row) {
        switch level {
        case .profile:
            return
        case .llm:
            setPin(llm: row.value, model: nil, effort: member.reasoningEffort)
        case .model:
            setPin(llm: member.llm, model: row.value, effort: member.reasoningEffort)
        case .effort:
            setPin(llm: member.llm, model: member.model, effort: row.value)
        }
    }

    private func fieldRow(
        label: String, value: String, identifier: String, action: @escaping () -> Void
    ) -> some View {
        Button {
            guard !saving else { return }
            Haptics.tap()
            action()
        } label: {
            HStack(spacing: 10) {
                Text(verbatim: label)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
                Spacer(minLength: 12)
                Text(verbatim: value)
                    .font(Theme.sys(14))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
            }
            .padding(.horizontal, 20)
            .frame(minHeight: 52)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(saving)
        .opacity(saving ? 0.5 : 1)
        .accessibilityIdentifier(identifier)
        .accessibilityLabel(Text(verbatim: "\(label) \(value)"))
    }

    private func optionRow(_ row: LlmPinOptions.Row) -> some View {
        let selected = row.value == pickedAtOpenLevel
        return Button {
            guard !saving else { return }
            Haptics.tap()
            if selected {
                level = .profile
            } else {
                pick(row)
            }
        } label: {
            HStack(spacing: 10) {
                Text(verbatim: row.label)
                    .font(Theme.sys(14))
                    .foregroundStyle(Theme.ink)
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
        .accessibilityIdentifier("agent-option-\(row.value ?? "inherit")")
    }

    private func backRow(title: String) -> some View {
        Button {
            Haptics.tap()
            level = .profile
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "chevron.left")
                    .font(.system(size: 12, weight: .semibold))
                Text(verbatim: title)
                    .font(Theme.mono(12))
                    .textCase(.uppercase)
                    .kerning(1.2)
                Spacer(minLength: 0)
            }
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 20)
            .frame(minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.top, 14)
        .accessibilityIdentifier("agent-picker-back")
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

    private func setPin(llm: String?, model: String?, effort: String?) {
        saving = true
        error = nil
        Task {
            do {
                try await client.agentSetModel(
                    agentId: member.id, llm: llm, model: model, reasoningEffort: effort)
                onChanged()
                level = .profile
            } catch {
                self.error = ProjectsStore.message(from: error)
            }
            saving = false
        }
    }

    private func setFace(_ pick: PhotosPickerItem) {
        saving = true
        error = nil
        Task {
            defer { saving = false }
            do {
                guard let data = try await pick.loadTransferable(type: Data.self) else {
                    error = lang.t("agent.faceUnreadable")
                    return
                }
                let blobId = try await AgentFaceUpload.put(data, client: client)
                try await client.agentSetAvatar(agentId: member.id, blobId: blobId)
                onChanged()
                dismiss()
            } catch {
                self.error = ProjectsStore.message(from: error)
            }
        }
    }
}
