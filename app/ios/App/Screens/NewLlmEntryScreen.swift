import SwiftUI

/// Create an LLM entry — a provider plus the credential it needs.
///
/// **The provider is a PICK, from the gateway.** The registry is compiled into
/// the gateway binary, so this list is the only way the phone learns which ids
/// are legal, and a wrong one is not an error anywhere: the entry is written,
/// `prepare` warns and drops it from the pool, and `GET /v1/llm/models` keeps
/// listing a row that no longer exists. A free-text field here would be a
/// silent 200-OK deletion.
///
/// **This is the one screen in the editor that batches.** Everywhere else a row
/// commits on its own (see `LlmEntryScreen` — one key per PUT, because the
/// update route cannot say which model a per-model fact belongs to). Creation
/// has no such hazard and the opposite constraint: an entry cannot exist
/// half-made, so name + provider + model travel together in the one request
/// that brings it into being.
struct NewLlmEntryScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var catalog = ModelCatalog.shared
    @Environment(\.dismiss) private var dismiss

    var client: any BayboClientProtocol = Baybo.client

    @State private var providers: [LlmProviderInfo] = []
    @State private var loadingProviders = true
    @State private var providerFailure: String?
    @State private var picked: LlmProviderInfo?

    @State private var name = ""
    @State private var model = ""
    @State private var modelsDraft = ""
    @State private var liteModel = ""
    @State private var baseUrl = ""
    @State private var apiKey = ""

    @State private var creating = false
    @State private var failure: String?
    /// Resolved when the key field would be shown; an `http://` direct binding
    /// would put the key on the wire in clear text.
    @State private var cleartext = false

    @State private var level: Level = .form

    private enum Level: Equatable {
        case form
        case providerPicker
    }

    var body: some View {
        ZStack(alignment: .top) {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    switch level {
                    case .form: formBody
                    case .providerPicker: providerPicker
                    }
                    Spacer(minLength: 60)
                }
                .padding(.top, ChatHeaderView.barHeight + 16)
                // The container claims the width, or a level whose content is
                // all narrow gets centred by the ScrollView.
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .scrollDismissesKeyboard(.interactively)
            .scrollContentBackground(.hidden)

            header
        }
        .background(Theme.paper)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task { await loadProviders() }
    }

    // MARK: - Chrome

    private var header: some View {
        ZStack {
            Text(verbatim: lang.t("llm.newTitle"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                .padding(.horizontal, 72)

            HStack {
                Button {
                    if level == .form {
                        dismiss()
                    } else {
                        Haptics.tap()
                        level = .form
                    }
                } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityIdentifier("llm-new-back")
                .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

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

    // MARK: - The form

    @ViewBuilder private var formBody: some View {
        if let failure {
            Text(verbatim: failure)
                .font(Theme.mono(12))
                .foregroundStyle(Theme.err)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .accessibilityIdentifier("llm-new-error")
        }

        field(
            label: lang.t("llm.newName"), text: $name, placeholder: "anthropic",
            hint: lang.t("llm.newNameHint"), identifier: "llm-new-name")

        providerRow

        if let picked {
            field(
                label: lang.t("llm.model"), text: $model, placeholder: "claude-sonnet-5",
                hint: lang.t("llm.newModelHint"), identifier: "llm-new-model")

            LlmModelIdsField(text: $modelsDraft, identifier: "llm-new-models").disabled(creating)
            Picker(lang.t("llm.liteModel"), selection: $liteModel) {
                Text(verbatim: lang.t("llm.noLiteModel")).tag("")
                ForEach(LlmModelInput.parse(model + "\n" + modelsDraft), id: \.self) { id in
                    Text(verbatim: id).tag(id)
                }
            }
            .padding(.horizontal, 20)
            .disabled(creating)
            .accessibilityIdentifier("llm-new-lite-model")
            .onChange(of: model + "\n" + modelsDraft) { _, value in
                if !LlmModelInput.parse(value).contains(liteModel) { liteModel = "" }
            }

            field(
                label: lang.t("llm.baseUrl"), text: $baseUrl,
                placeholder: picked.defaultBaseUrl ?? "https://…",
                hint: lang.t("llm.newBaseUrlHint"), keyboard: .URL,
                identifier: "llm-new-base-url")

            keySection(picked)
        }

        Button {
            Haptics.tap()
            create()
        } label: {
            Text(verbatim: lang.t("llm.newCreate"))
        }
        .buttonStyle(InkPillButtonStyle())
        .disabled(!committable || creating)
        .opacity(!committable || creating ? 0.35 : 1)
        .padding(.horizontal, 20)
        .padding(.top, 24)
        .accessibilityIdentifier("llm-new-create")

        Text(verbatim: lang.t("llm.newFootnote"))
            .font(Theme.mono(10.5))
            .foregroundStyle(Theme.inkSoft)
            .lineSpacing(2)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 20)
            .padding(.top, 12)
    }

    private var providerRow: some View {
        Button {
            guard !creating else { return }
            Haptics.tap()
            level = .providerPicker
        } label: {
            HStack(spacing: 10) {
                Text(verbatim: lang.t("llm.provider"))
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
                Spacer(minLength: 12)
                Text(verbatim: picked?.name ?? lang.t("llm.newPickProvider"))
                    .font(Theme.sys(14))
                    .foregroundStyle(picked == nil ? Theme.inkSoft : Theme.ink)
                    .lineLimit(1)
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
            }
            .padding(.horizontal, 20)
            .frame(minHeight: 52)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(creating)
        .accessibilityIdentifier("llm-new-provider")
        .accessibilityLabel(Text(verbatim: lang.t("llm.provider")))
        .accessibilityValue(Text(verbatim: picked?.name ?? ""))
    }

    /// What the key field says depends on what the provider actually needs —
    /// which is why the picker carries `auth` rather than the app guessing from
    /// the provider's name.
    @ViewBuilder private func keySection(_ provider: LlmProviderInfo) -> some View {
        switch provider.auth {
        case .keyless:
            note(lang.t("llm.newKeyless", provider.name))
        case .oAuth:
            // Unreachable: the picker does not offer these. Kept so the switch
            // is total and a future reachable path says something true.
            warning(lang.t("llm.newOauth", provider.name))
        case .apiKey, .optionalApiKey, .unknown:
            if cleartext {
                warning(lang.t("llm.keyCleartext"))
            } else {
                field(
                    label: lang.t("llm.apiKey"), text: $apiKey, placeholder: "",
                    hint: provider.auth == .optionalApiKey
                        ? lang.t("llm.newKeyOptional") : lang.t("llm.newKeyRequired"),
                    secure: true, identifier: "llm-new-api-key")

                if let env = provider.defaultApiKeyEnv {
                    note(lang.t("llm.newKeyEnvNote", env))
                }
            }
        }
    }

    // MARK: - The provider picker

    @ViewBuilder private var providerPicker: some View {
        sectionTitle(lang.t("llm.provider"))
        if loadingProviders {
            HStack(spacing: 10) {
                ProgressView().progressViewStyle(.circular).tint(Theme.inkSoft).scaleEffect(0.8)
                Text(verbatim: lang.t("llm.newProvidersLoading"))
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
            }
            .padding(.horizontal, 20)
            .padding(.top, 8)
        } else if let providerFailure {
            warning(providerFailure)
                .accessibilityIdentifier("llm-new-providers-failure")
        } else {
            explain(lang.t("llm.newProvidersExplain"))
            VStack(spacing: 0) {
                ForEach(offerable, id: \.name) { provider in
                    providerOption(provider)
                }
            }
        }
    }

    /// OAuth providers are filtered OUT rather than shown disabled. Their login
    /// is a device-code flow the gateway only runs from its own shell, so the
    /// create route refuses them — offering a row that always 400s teaches the
    /// user nothing the footnote does not already say.
    private var offerable: [LlmProviderInfo] {
        providers.filter { $0.auth != .oAuth }
    }

    private func providerOption(_ provider: LlmProviderInfo) -> some View {
        Button {
            Haptics.tap()
            pick(provider)
        } label: {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 10) {
                    Text(verbatim: provider.name)
                        .font(Theme.sys(14))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    if provider.name == picked?.name {
                        Image(systemName: "checkmark")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                    }
                }
                if let note = authNote(provider) {
                    Text(verbatim: note)
                        .font(Theme.mono(10.5))
                        .foregroundStyle(Theme.inkSoft)
                }
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 8)
            .frame(minHeight: 52)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("llm-provider-\(provider.name)")
        .accessibilityLabel(Text(verbatim: provider.name))
    }

    private func authNote(_ provider: LlmProviderInfo) -> String? {
        switch provider.auth {
        case .keyless: lang.t("llm.newAuthKeyless")
        case .optionalApiKey: lang.t("llm.newAuthOptional")
        case .apiKey, .oAuth, .unknown: nil
        }
    }

    // MARK: - Rules

    /// A name that cannot ride a URL would make an entry nothing can address
    /// afterwards — the gateway refuses it, and so does this, before the round
    /// trip. The character set is the gateway's.
    private var nameIsAddressable: Bool {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty, trimmed.count <= 64 else { return false }
        guard trimmed.contains(where: { $0 != "." }) else { return false }
        return trimmed.allSatisfy { c in
            c.isASCII && (c.isLetter || c.isNumber || c == "-" || c == "_" || c == ".")
        }
    }

    private var committable: Bool {
        guard nameIsAddressable, picked != nil else { return false }
        guard !model.trimmingCharacters(in: .whitespaces).isEmpty else { return false }
        // The name is also this entry's vault-key suffix, so a collision is a
        // 409 — catch it here where the field is still on screen.
        guard catalog.entry(named: name.trimmingCharacters(in: .whitespaces)) == nil else {
            return false
        }
        return true
    }

    // MARK: - Actions

    private func pick(_ provider: LlmProviderInfo) {
        picked = provider
        if name.trimmingCharacters(in: .whitespaces).isEmpty {
            name = uniqueName(from: provider.name)
        }
        cleartext = (try? client.activeBindingIsCleartext()) ?? false
        level = .form
    }

    /// Mirrors the gateway wizard's own default: the provider id, numbered if
    /// taken. A name is required and this is the one the operator would type.
    private func uniqueName(from provider: String) -> String {
        guard catalog.entry(named: provider) != nil else { return provider }
        var suffix = 2
        while catalog.entry(named: "\(provider)\(suffix)") != nil { suffix += 1 }
        return "\(provider)\(suffix)"
    }

    private func loadProviders() async {
        defer { loadingProviders = false }
        do {
            providers = try await catalog.providers()
        } catch {
            providerFailure = bayboErrorText(error)
        }
    }

    private func create() {
        guard let picked else { return }
        creating = true
        failure = nil
        Task {
            defer { creating = false }
            let trimmed = { (s: String) -> String? in
                let v = s.trimmingCharacters(in: .whitespacesAndNewlines)
                return v.isEmpty ? nil : v
            }
            do {
                _ = try await catalog.create(
                    NewLlmEntry(
                        name: name.trimmingCharacters(in: .whitespaces),
                        provider: picked.name,
                        model: model.trimmingCharacters(in: .whitespaces),
                        models: LlmModelInput.parse(modelsDraft),
                        liteModel: trimmed(liteModel),
                        baseUrl: trimmed(baseUrl),
                        apiKeyEnv: nil,
                        apiKey: trimmed(apiKey)))
                Haptics.success()
                dismiss()
            } catch {
                failure = bayboErrorText(error)
            }
        }
    }

    // MARK: - Vocabulary

    private func field(
        label: String, text: Binding<String>, placeholder: String, hint: String?,
        keyboard: UIKeyboardType = .default, secure: Bool = false, identifier: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(verbatim: label)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            Group {
                if secure {
                    SecureField(placeholder, text: text).textContentType(.password)
                } else {
                    TextField(placeholder, text: text).keyboardType(keyboard)
                }
            }
            .font(Theme.sys(15))
            .foregroundStyle(Theme.ink)
            .autocorrectionDisabled()
            .textInputAutocapitalization(.never)
            .padding(.horizontal, 14)
            .frame(minHeight: 46)
            .overlay(
                RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                    .strokeBorder(Theme.lineStrong, lineWidth: 1)
            )
            .accessibilityIdentifier(identifier)
            .disabled(creating)
            if let hint {
                Text(verbatim: hint)
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineSpacing(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 20)
        .padding(.top, 14)
    }

    private func sectionTitle(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(10.5))
            .textCase(.uppercase)
            .kerning(1.2)
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 20)
            .padding(.top, 14)
            .padding(.bottom, 6)
    }

    private func explain(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.sys(13))
            .foregroundStyle(Theme.ink)
            .lineSpacing(3)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 20)
            .padding(.bottom, 6)
    }

    private func note(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(10.5))
            .foregroundStyle(Theme.inkSoft)
            .lineSpacing(2)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 20)
            .padding(.top, 14)
    }

    private func warning(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(11.5))
            .foregroundStyle(Theme.err)
            .lineSpacing(2)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 20)
            .padding(.top, 14)
    }
}
