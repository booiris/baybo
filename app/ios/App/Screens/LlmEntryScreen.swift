import SwiftUI

/// One LLM entry's editor — the Settings half of the model story, where the
/// chat header's `ModelMenuPanel` is the per-session half. This writes GLOBAL
/// gateway config: every device sees it, and an unpinned session picks it up on
/// its next turn.
///
/// **One row, one field, one request — there is no Save button, and that is the
/// design, not an omission.** `PUT /v1/llm/models/{name}` assigns the entry's
/// `model` before it resolves the spec that `context_window` and
/// `supports_vision` land on, so a body carrying `model` beside either of those
/// moves the departing model's overrides onto its successor and answers 200. A
/// screen that commits one key at a time cannot express that request.
///
/// **Inherited vs pinned is the whole visual language.** A soft value is
/// whatever the provider's capability chain resolves to; an ink value is an
/// override pinned on this entry. One colour step, and each editor says which
/// it is in words, because a colour step alone is not self-explanatory.
///
/// Levels replace in place — `AgentProfileSheet`'s shape — so every editor
/// shares this screen's one `saving` gate and its scroll view's keyboard
/// behaviour, and no pick can overlap another write.
struct LlmEntryScreen: View {
    let entryName: String
    var client: any BayboClientProtocol = Baybo.client

    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var catalog = ModelCatalog.shared
    @Environment(\.dismiss) private var dismiss

    @State private var level: Level = .fields
    @State private var saving = false
    @State private var outcome: Outcome?
    /// Set once a write comes back staged. While true the entry on disk and the
    /// entry the pool is running have diverged, which is exactly the state where
    /// a probe reads the file and certifies settings nobody is serving.
    @State private var staged = false
    @State private var draft = ""
    @State private var probe: LlmTestResult?
    @State private var probing = false
    /// Whether the active binding would carry the key in clear text. Resolved
    /// when the key level OPENS rather than on appear: it costs a keychain read,
    /// and it is the only level that cares.
    @State private var cleartext = false
    @State private var catalogItems: [LlmCatalogModel] = []
    @State private var loadingCatalog = false
    @State private var catalogFailure: String?

    private enum Level: Equatable {
        case fields
        case model
        case addModel
        case effort
        case vision
        case baseUrl
        case contextWindow
        case apiKey
    }

    private enum Outcome: Equatable {
        case saved(String)
        case staged(String)
        case failed(String)
    }

    private var entry: LlmModelInfo? { catalog.entry(named: entryName) }

    var body: some View {
        ZStack(alignment: .top) {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    // Above the level content, not inside the fields level: a
                    // pick commits from a SUB-level, and a failure that only
                    // rendered on the fields level would leave the picker
                    // showing nothing at all — which reads as "the tap did
                    // nothing" rather than "the gateway refused it".
                    outcomeStrip
                    if let entry {
                        switch level {
                        case .fields: fieldsBody(entry)
                        case .model: modelLevel(entry)
                        case .addModel: addModelLevel(entry)
                        case .effort: effortLevel(entry)
                        case .vision: visionLevel(entry)
                        case .baseUrl: baseUrlLevel(entry)
                        case .contextWindow: contextLevel(entry)
                        case .apiKey: apiKeyLevel(entry)
                        }
                    } else {
                        // The entry vanished under us — a config edit elsewhere
                        // dropped it. Say so rather than rendering a blank form
                        // whose every write would 404.
                        Text(verbatim: lang.t("llm.gone"))
                            .font(Theme.sys(14))
                            .foregroundStyle(Theme.inkSoft)
                            .padding(.horizontal, 20)
                            .padding(.top, 20)
                    }
                    Spacer(minLength: 60)
                }
                .padding(.top, ChatHeaderView.barHeight + 16)
            }
            .scrollDismissesKeyboard(.interactively)
            .scrollContentBackground(.hidden)

            header
        }
        .background(Theme.paper)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .onAppear { catalog.refreshIfNeeded() }
    }

    // MARK: - Chrome

    private var header: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(verbatim: entryName)
                    .font(Theme.mono(16))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                    .padding(.horizontal, 72)

                HStack {
                    Button {
                        // A sub-level's back chevron returns to the fields; only
                        // the fields level pops the screen. One affordance, two
                        // depths — the same contract the model panel's back row
                        // has.
                        switch level {
                        case .fields: dismiss()
                        // The catalog was opened FROM the model list, so back
                        // means back one step, not all the way out.
                        case .addModel:
                            Haptics.tap()
                            level = .model
                        default:
                            Haptics.tap()
                            level = .fields
                        }
                    } label: {
                        Image(systemName: "chevron.left")
                            .font(.system(size: 18, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassSurface(interactive: true, in: .circle)
                    .accessibilityIdentifier("llm-entry-back")
                    .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

                    Spacer()
                }
            }
            .padding(.horizontal, 24)
            .frame(height: ChatHeaderView.barHeight)
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    // MARK: - Level 1: the fields

    @ViewBuilder private func fieldsBody(_ entry: LlmModelInfo) -> some View {
        VStack(spacing: 0) {
            infoRow(label: lang.t("llm.provider"), value: entry.provider)
            fieldRow(
                label: lang.t("llm.model"), value: entry.model, pinned: true,
                identifier: "llm-field-model"
            ) { level = .model }
            fieldRow(
                label: lang.t("llm.baseUrl"), value: entry.baseUrl ?? "—",
                pinned: entry.baseUrl != nil, identifier: "llm-field-base-url"
            ) { open(.baseUrl, seed: entry.baseUrl ?? "") }
            fieldRow(
                label: lang.t("llm.apiKey"),
                value: lang.t(entry.apiKeyConfigured ? "llm.keySet" : "llm.keyUnset"),
                pinned: entry.apiKeyConfigured, identifier: "llm-field-api-key",
                a11yValue: lang.t(
                    entry.apiKeyConfigured ? "llm.a11yKeySet" : "llm.a11yKeyUnset")
            ) {
                cleartext = (try? client.activeBindingIsCleartext()) ?? false
                open(.apiKey, seed: "")
            }
            if !entry.availableEfforts.isEmpty {
                fieldRow(
                    label: lang.t("llm.thinking"),
                    value: entry.reasoningEffort.map { EffortLevel.named($0)?.label ?? $0 }
                        ?? lang.t("llm.providerDefault"),
                    pinned: entry.reasoningEffort != nil, identifier: "llm-field-effort"
                ) { level = .effort }
            }
            fieldRow(
                label: lang.t("llm.context"),
                value: numberText(entry.contextWindowOverride ?? entry.effectiveContextWindow),
                pinned: entry.contextWindowOverride != nil, identifier: "llm-field-context"
            ) {
                open(
                    .contextWindow,
                    seed: String(entry.contextWindowOverride ?? entry.effectiveContextWindow))
            }
            fieldRow(
                label: lang.t("llm.vision"),
                value: lang.t(
                    (entry.supportsVisionOverride ?? entry.effectiveSupportsVision)
                        ? "llm.on" : "llm.off"),
                pinned: entry.supportsVisionOverride != nil, identifier: "llm-field-vision"
            ) { level = .vision }
        }

        Text(verbatim: lang.t("llm.inheritedNote"))
            .font(Theme.mono(10.5))
            .foregroundStyle(Theme.inkSoft)
            .lineSpacing(2)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 20)
            .padding(.top, 18)

        actions(entry)
    }

    @ViewBuilder private func actions(_ entry: LlmModelInfo) -> some View {
        // A staged write means the file and the running pool disagree, and the
        // probe reads the FILE — so it would come back green for settings the
        // gateway is not serving. Hiding it beats captioning it: a button that
        // is present but lying is worse than one that is absent and explained.
        if staged {
            Text(verbatim: lang.t("llm.probeHiddenWhileStaged"))
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 20)
                .padding(.top, 22)
        } else {
            Button {
                Haptics.tap()
                runProbe()
            } label: {
                HStack(spacing: 8) {
                    if probing {
                        ProgressView()
                            .progressViewStyle(.circular)
                            .tint(Theme.ink)
                            .scaleEffect(0.7)
                            .frame(width: 13, height: 13)
                    }
                    Text(verbatim: lang.t("llm.test"))
                }
            }
            .buttonStyle(OutlinePillButtonStyle())
            .disabled(probing || saving)
            .opacity(probing || saving ? 0.5 : 1)
            .padding(.horizontal, 20)
            .padding(.top, 22)
            .accessibilityIdentifier("llm-test")

            if let probe {
                probeResult(probe)
            }
        }

        if entry.name != catalog.defaultName {
            Button {
                Haptics.tap()
                setAsDefault()
            } label: {
                Text(verbatim: lang.t("llm.setDefault"))
            }
            .buttonStyle(InkPillButtonStyle())
            .disabled(saving)
            .opacity(saving ? 0.5 : 1)
            .padding(.horizontal, 20)
            .padding(.top, 12)
            .accessibilityIdentifier("llm-set-default")

            Text(verbatim: lang.t("llm.setDefaultNote"))
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 20)
                .padding(.top, 8)
        } else {
            Text(verbatim: lang.t("llm.isDefaultNote"))
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.horizontal, 20)
                .padding(.top, 20)
        }
    }

    private func probeResult(_ result: LlmTestResult) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(
                verbatim: result.ok
                    ? lang.t("llm.probeOk", "\(result.latencyMs ?? 0)")
                    : lang.t("llm.probeFailed")
            )
            .font(Theme.mono(12))
            .foregroundStyle(result.ok ? Theme.ink : Theme.err)
            // The provider's own words — the one place on this surface where a
            // gateway-side failure arrives as prose instead of a status code.
            if let error = result.error {
                Text(verbatim: error)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .lineSpacing(2)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 20)
        .padding(.top, 10)
        .accessibilityIdentifier("llm-probe-result")
    }

    @ViewBuilder private var outcomeStrip: some View {
        if let outcome {
            let (text, tint): (String, Color) =
                switch outcome {
                case .saved(let field): (lang.t("llm.saved", field), Theme.inkSoft)
                case .staged(let field): (lang.t("llm.stagedSave", field), Theme.inkSoft)
                case .failed(let message): (message, Theme.err)
                }
            Text(verbatim: text)
                .font(Theme.mono(12))
                .foregroundStyle(tint)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 20)
                .padding(.top, 14)
                .accessibilityIdentifier("llm-outcome")
        }
    }

    // MARK: - Level 2: the pickers

    @ViewBuilder private func modelLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.model"))
        explain(lang.t("llm.modelExplain"))
        VStack(spacing: 0) {
            ForEach(catalog.models(of: entry), id: \.self) { model in
                optionRow(
                    title: model, selected: model == entry.model,
                    // Switching away from the model `lite_model` names can be
                    // rejected outright, and the 400 has no phone-side repair —
                    // so the warning sits on the row that would cause it.
                    note: entry.liteModel == entry.model && model != entry.model
                        ? lang.t("llm.liteModelWarning") : nil,
                    // The default cannot be dropped: the entry prepends it to
                    // its own list, so removing it would not remove it.
                    onRemove: model == entry.model ? nil : { removeModel(model, from: entry) },
                    identifier: "llm-option-\(model)"
                ) {
                    commit(.model(model: model), field: lang.t("llm.model"))
                }
            }
            fieldRow(
                label: lang.t("llm.addModel"), value: "", pinned: false,
                identifier: "llm-add-model", a11yValue: ""
            ) { openCatalog(entry) }
        }
    }

    /// The provider's live catalog. A PICK, never free text — nothing
    /// gateway-side checks a model id against the vendor, so a typo would
    /// build, list, validate, and only fail at the first real completion.
    @ViewBuilder private func addModelLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.addModel"))
        if loadingCatalog {
            HStack(spacing: 10) {
                ProgressView().progressViewStyle(.circular).tint(Theme.inkSoft).scaleEffect(0.8)
                Text(verbatim: lang.t("llm.catalogLoading"))
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
            }
            .padding(.horizontal, 20)
            .padding(.top, 8)
        } else if let failure = catalogFailure {
            // The provider's own words. This is one of the few places a
            // gateway-side failure reaches the phone as prose, and it is
            // usually the real diagnosis: a bad key, a bad base URL.
            warning(failure)
                .accessibilityIdentifier("llm-catalog-failure")
        } else if catalogItems.isEmpty {
            explain(lang.t("llm.catalogEmpty"))
        } else {
            explain(lang.t("llm.addModelExplain"))
            VStack(spacing: 0) {
                ForEach(catalogItems, id: \.id) { item in
                    optionRow(
                        title: item.id,
                        selected: item.configured,
                        note: catalogNote(item),
                        identifier: "llm-catalog-\(item.id)"
                    ) {
                        addModel(item.id, to: entry)
                    }
                }
            }
        }
    }

    /// The display name and window, when the provider supplies them — enough to
    /// tell two similar ids apart without turning the row into a table.
    private func catalogNote(_ item: LlmCatalogModel) -> String? {
        var parts: [String] = []
        if let name = item.displayName, name != item.id { parts.append(name) }
        if let window = item.contextWindow { parts.append(numberText(window)) }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    @ViewBuilder private func effortLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.thinking"))
        explain(lang.t("llm.effortExplain"))
        VStack(spacing: 0) {
            // The rungs come from the SERVER: each provider speaks its own
            // effort vocabulary, so a level its dialect cannot say would be a
            // pick that never reaches the wire.
            ForEach(entry.availableEfforts, id: \.self) { rung in
                optionRow(
                    title: EffortLevel.named(rung)?.label ?? rung,
                    selected: entry.reasoningEffort == rung,
                    identifier: "llm-option-\(rung)"
                ) {
                    commit(
                        .reasoningEffort(effort: rung), field: lang.t("llm.thinking"))
                }
            }
            optionRow(
                title: lang.t("llm.providerDefault"), selected: entry.reasoningEffort == nil,
                identifier: "llm-option-effort-default"
            ) {
                commit(.reasoningEffort(effort: nil), field: lang.t("llm.thinking"))
            }
        }
    }

    /// Three rows, never a `Toggle`. A two-state control cannot express *clear*,
    /// so it would silently convert an inherited `true` into a pinned `true`
    /// that stops tracking the provider's published capability forever.
    @ViewBuilder private func visionLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.vision"))
        explain(lang.t("llm.visionExplain"))
        VStack(spacing: 0) {
            optionRow(
                title: lang.t("llm.on"), selected: entry.supportsVisionOverride == true,
                identifier: "llm-option-vision-on"
            ) {
                commit(.supportsVision(on: true), field: lang.t("llm.vision"))
            }
            optionRow(
                title: lang.t("llm.off"), selected: entry.supportsVisionOverride == false,
                identifier: "llm-option-vision-off"
            ) {
                commit(.supportsVision(on: false), field: lang.t("llm.vision"))
            }
            optionRow(
                title: lang.t(
                    entry.effectiveSupportsVision
                        ? "llm.providerDefaultOn" : "llm.providerDefaultOff"),
                selected: entry.supportsVisionOverride == nil,
                identifier: "llm-option-vision-default"
            ) {
                commit(.supportsVision(on: nil), field: lang.t("llm.vision"))
            }
        }
    }

    // MARK: - Level 2: the typed values

    @ViewBuilder private func baseUrlLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.baseUrl"))
        explain(lang.t("llm.baseUrlExplain"))
        textField(placeholder: "https://…", keyboard: .URL, identifier: "llm-input-base-url")
        commitButton(
            title: lang.t("common.save"), enabled: baseUrlCommittable(entry),
            identifier: "llm-commit-base-url"
        ) {
            let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
            commit(
                .baseUrl(url: trimmed.isEmpty ? nil : trimmed),
                field: lang.t("llm.baseUrl"))
        }
        hint(lang.t("llm.baseUrlHint"))
    }

    @ViewBuilder private func contextLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.context"))
        explain(
            entry.contextWindowOverride == nil
                ? lang.t("llm.contextInherited", entry.model)
                : lang.t("llm.contextPinned"))
        textField(placeholder: "", keyboard: .numberPad, identifier: "llm-input-context")
        commitButton(
            title: lang.t("common.save"), enabled: contextCommittable(entry),
            identifier: "llm-commit-context"
        ) {
            guard let tokens = UInt32(draft.trimmingCharacters(in: .whitespaces)) else { return }
            commit(.contextWindow(tokens: tokens), field: lang.t("llm.context"))
        }
        if entry.contextWindowOverride != nil {
            secondaryAction(lang.t("llm.useProviderDefault"), identifier: "llm-clear-context") {
                commit(.contextWindow(tokens: nil), field: lang.t("llm.context"))
            }
        }
        hint(lang.t("llm.contextHint"))
    }

    @ViewBuilder private func apiKeyLevel(_ entry: LlmModelInfo) -> some View {
        levelTitle(lang.t("llm.apiKey"))
        Text(
            verbatim: entry.apiKeyEnv.map { lang.t("llm.keyViaEnv", $0) }
                ?? lang.t(entry.apiKeyConfigured ? "llm.keySet" : "llm.keyUnset")
        )
        .font(Theme.mono(12))
        .foregroundStyle(Theme.inkSoft)
        .padding(.horizontal, 20)
        .padding(.top, 2)

        if cleartext {
            // The key is the one payload whose exposure re-sending cannot undo,
            // and on this binding it would cross the network in clear text
            // beside the admin bearer. No field, no button — only the reason.
            warning(lang.t("llm.keyCleartext"))
        } else {
            SecureField("", text: $draft)
                .font(Theme.mono(14))
                .foregroundStyle(Theme.ink)
                .textContentType(.password)
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .padding(.horizontal, 14)
                .frame(minHeight: 46)
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                        .strokeBorder(Theme.lineStrong, lineWidth: 1)
                )
                .padding(.horizontal, 20)
                .padding(.top, 14)
                .accessibilityIdentifier("llm-input-api-key")

            // An env var OUTRANKS the vault, so a key stored here would be
            // accepted, reported saved, and never used. Say it before the tap,
            // not after.
            if entry.apiKeyEnv != nil {
                warning(lang.t("llm.keyShadowed", entry.apiKeyEnv ?? ""))
            }

            commitButton(
                title: lang.t("llm.storeKey"),
                enabled: !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
                identifier: "llm-commit-api-key"
            ) {
                // Trimmed because a pasted key drags a trailing newline far more
                // often than it carries meaningful whitespace.
                commit(
                    .apiKey(key: draft.trimmingCharacters(in: .whitespacesAndNewlines)),
                    field: lang.t("llm.apiKey"))
            }

            if let env = entry.apiKeyEnv {
                secondaryAction(lang.t("llm.clearEnv", env), identifier: "llm-clear-env") {
                    commit(.apiKeyEnv(env: nil), field: lang.t("llm.apiKeyEnv"))
                }
            }
        }

        // There is no clear/remove affordance because clearing is impossible:
        // an empty value does NOT delete the stored key — the gateway logs the
        // request and leaves the prior secret in place — and no HTTP route
        // deletes a vault key at all. A button that cannot do what it says is
        // worse than its absence.
        hint(lang.t("llm.keyHint"))
    }

    // MARK: - Row + control vocabulary

    private func infoRow(label: String, value: String) -> some View {
        HStack(spacing: 10) {
            Text(verbatim: label)
                .font(Theme.mono(12))
                .foregroundStyle(Theme.inkSoft)
            Spacer(minLength: 12)
            Text(verbatim: value)
                .font(Theme.sys(14))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
        }
        .padding(.horizontal, 20)
        .frame(minHeight: 52)
    }

    /// `pinned` drives the one colour step this screen encodes state with: ink
    /// for an override set on this entry, soft for a value inherited from the
    /// provider's capability chain.
    private func fieldRow(
        label: String, value: String, pinned: Bool, identifier: String,
        a11yValue: String? = nil, action: @escaping () -> Void
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
                    .foregroundStyle(pinned ? Theme.ink : Theme.inkSoft)
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
        .accessibilityLabel(Text(verbatim: label))
        .accessibilityValue(
            Text(
                verbatim: a11yValue
                    ?? (pinned
                        ? lang.t("llm.a11yPinned", value) : lang.t("llm.a11yInherited", value))))
    }

    /// `onRemove` puts a second, independent action on the row. It is a button
    /// beside the label rather than a swipe: these rows are in a plain VStack,
    /// not a `List`, so there is no `.swipeActions` to hang it on — and a
    /// destructive gesture with no visible affordance is the wrong default for
    /// config anyway.
    private func optionRow(
        title: String, selected: Bool, note: String? = nil,
        onRemove: (() -> Void)? = nil, identifier: String,
        action: @escaping () -> Void
    ) -> some View {
        Button {
            guard !saving else { return }
            Haptics.tap()
            if selected {
                level = .fields
            } else {
                action()
            }
        } label: {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 10) {
                    Text(verbatim: title)
                        .font(Theme.sys(14))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                    Spacer(minLength: 6)
                    if let onRemove {
                        Button {
                            guard !saving else { return }
                            Haptics.tap()
                            onRemove()
                        } label: {
                            Image(systemName: "minus.circle")
                                .font(.system(size: 15, weight: .regular))
                                .foregroundStyle(Theme.inkSoft)
                                .frame(width: 34, height: 34)
                                .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .accessibilityIdentifier("\(identifier)-remove")
                        .accessibilityLabel(Text(verbatim: lang.t("llm.removeModel", title)))
                    }
                    if selected {
                        Image(systemName: "checkmark")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                    }
                }
                if let note {
                    Text(verbatim: note)
                        .font(Theme.mono(10.5))
                        .foregroundStyle(Theme.inkSoft)
                        .lineSpacing(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 8)
            .frame(minHeight: 52)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(saving)
        .opacity(saving ? 0.5 : 1)
        .accessibilityIdentifier(identifier)
        .accessibilityLabel(Text(verbatim: title))
    }

    private func textField(placeholder: String, keyboard: UIKeyboardType, identifier: String)
        -> some View
    {
        TextField(placeholder, text: $draft)
            .font(Theme.mono(14))
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
            .padding(.horizontal, 20)
            .padding(.top, 14)
            .accessibilityIdentifier(identifier)
    }

    private func commitButton(
        title: String, enabled: Bool, identifier: String, action: @escaping () -> Void
    ) -> some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Text(verbatim: title)
        }
        .buttonStyle(InkPillButtonStyle())
        .disabled(!enabled || saving)
        .opacity(!enabled || saving ? 0.35 : 1)
        .padding(.horizontal, 20)
        .padding(.top, 18)
        .accessibilityIdentifier(identifier)
    }

    private func secondaryAction(_ title: String, identifier: String, action: @escaping () -> Void)
        -> some View
    {
        Button {
            Haptics.tap()
            action()
        } label: {
            Text(verbatim: title)
        }
        .buttonStyle(LinkButtonStyle())
        .disabled(saving)
        .opacity(saving ? 0.5 : 1)
        .padding(.horizontal, 12)
        .padding(.top, 4)
        .accessibilityIdentifier(identifier)
    }

    /// The top padding is clearance, not taste: the fields level opens with a
    /// 52pt row whose text sits centred, so a sub-level starting at the
    /// container's edge tucks its title under the glass back circle.
    private func levelTitle(_ text: String) -> some View {
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

    private func hint(_ text: String) -> some View {
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

    // MARK: - Rules

    private func numberText(_ value: UInt32) -> String {
        value == 0 ? "—" : String(value)
    }

    private func baseUrlCommittable(_ entry: LlmModelInfo) -> Bool {
        let trimmed = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed != (entry.baseUrl ?? "") else { return false }
        // Empty clears the override; anything else has to look like an endpoint.
        // Nothing gateway-side checks this — a wrong host builds a client fine
        // and fails at the entry's first real turn.
        return trimmed.isEmpty || trimmed.hasPrefix("https://") || trimmed.hasPrefix("http://")
    }

    /// The seeding trap, disarmed. When no override is set the field is seeded
    /// with the EFFECTIVE window, so committing it untouched would manufacture
    /// an override out of an inherited value — pinning the model to a number
    /// that used to track the provider snapshot. Equal-to-seed is therefore not
    /// committable, whichever state the row is in.
    private func contextCommittable(_ entry: LlmModelInfo) -> Bool {
        let trimmed = draft.trimmingCharacters(in: .whitespaces)
        guard trimmed != String(entry.contextWindowOverride ?? entry.effectiveContextWindow) else {
            return false
        }
        guard let value = UInt32(trimmed) else { return false }
        return value > 0
    }

    // MARK: - Actions

    private func open(_ next: Level, seed: String) {
        draft = seed
        level = next
    }

    /// Every write goes through here: one field, one request, then a catalog
    /// re-read so the rows repaint from server truth rather than a local guess.
    private func commit(_ edit: LlmEntryEdit, field: String) {
        saving = true
        outcome = nil
        Task {
            defer { saving = false }
            do {
                let result = try await catalog.apply(edit, to: entryName)
                staged = staged || result.requiresRestart
                outcome = result.requiresRestart ? .staged(field) : .saved(field)
                // A config change invalidates whatever the last probe proved.
                probe = nil
                level = .fields
            } catch {
                outcome = .failed(lang.t("llm.saveFailed", field))
                NSLog("baybo: llm update %@: %@", field, bayboErrorText(error))
            }
        }
    }

    private func setAsDefault() {
        saving = true
        outcome = nil
        Task {
            defer { saving = false }
            do {
                let result = try await catalog.setDefault(entryName)
                staged = staged || result.requiresRestart
                outcome =
                    result.requiresRestart
                    ? .staged(lang.t("llm.defaultField")) : .saved(lang.t("llm.defaultField"))
            } catch {
                outcome = .failed(lang.t("llm.defaultFailed"))
                NSLog("baybo: llm set default: %@", bayboErrorText(error))
            }
        }
    }

    /// Open the add-a-model level and fetch the provider's catalog for it. The
    /// fetch is a real call out to the vendor, so the level renders its own
    /// loading and failure states rather than blocking the transition.
    private func openCatalog(_ entry: LlmModelInfo) {
        catalogItems = []
        catalogFailure = nil
        loadingCatalog = true
        level = .addModel
        Task {
            defer { loadingCatalog = false }
            do {
                catalogItems = try await catalog.catalog(of: entryName)
            } catch {
                catalogFailure = bayboErrorText(error)
            }
        }
    }

    /// Both list edits send the whole SET, because that is the endpoint's shape:
    /// idempotent, and the gateway carries each surviving id's overrides across
    /// so plain ids never destroy them.
    private func addModel(_ model: String, to entry: LlmModelInfo) {
        let models = catalog.models(of: entry)
        guard !models.contains(model) else {
            level = .model
            return
        }
        writeModelList(models + [model], field: lang.t("llm.modelList"), returnTo: .model)
    }

    private func removeModel(_ model: String, from entry: LlmModelInfo) {
        let models = catalog.models(of: entry).filter { $0 != model }
        writeModelList(models, field: lang.t("llm.modelList"), returnTo: .model)
    }

    private func writeModelList(_ models: [String], field: String, returnTo: Level) {
        saving = true
        outcome = nil
        Task {
            defer { saving = false }
            do {
                let result = try await catalog.setModels(models, of: entryName)
                staged = staged || result.requiresRestart
                outcome = result.requiresRestart ? .staged(field) : .saved(field)
                probe = nil
                level = returnTo
            } catch {
                outcome = .failed(lang.t("llm.saveFailed", field))
                NSLog("baybo: llm model list: %@", bayboErrorText(error))
            }
        }
    }

    private func runProbe() {
        probing = true
        probe = nil
        Task {
            defer { probing = false }
            do {
                probe = try await catalog.test(entry: entryName)
            } catch {
                // The probe's own transport failure, not the provider's answer —
                // rendered in the same place so the button always says something.
                probe = LlmTestResult(
                    ok: false, error: bayboErrorText(error), latencyMs: nil,
                    inputTokens: nil, outputTokens: nil,
                    provider: entry?.provider ?? "", model: entry?.model ?? "")
            }
        }
    }
}
