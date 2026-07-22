import SwiftUI

/// Hand-rolled replacement for the system `Menu` under the header's model
/// pill. The design wants trailing checkmarks, no submenu `›`, and a live
/// thinking-level subtitle — all impossible in a stock iOS 26 menu (selection
/// checkmarks and label icons render LEADING, and a nested `Menu` row always
/// draws its trailing chevron). Three levels replace in place: providers →
/// that provider's models + the thinking row → the levels; sub-levels carry a
/// back row. Presented by `ChatScreen` over the transcript; the scrim eats
/// every touch behind it and a tap closes.
struct ModelMenuPanel: View {
    @ObservedObject var store: ChatStore
    @ObservedObject var catalog: ModelCatalog
    @Binding var isPresented: Bool

    private enum Level: Equatable {
        case entries
        case models(entry: String)
        case effort(entry: String)
    }

    @State private var level: Level = .entries

    /// The one width knob. Content-independent on purpose: rows truncate
    /// rather than stretch the panel.
    private static let panelWidth: CGFloat = 232
    /// The pill's leading edge: bar padding 24 + back circle 42 + spacing 12 —
    /// the panel blooms from where the pill starts.
    private static let anchorLeading: CGFloat = 78

    var body: some View {
        ZStack(alignment: .topLeading) {
            // A whisper of dim doubles as the outside-tap dismiss surface; it
            // must swallow scrolls so the transcript can't move underneath.
            Color.black.opacity(0.06)
                .ignoresSafeArea()
                .onTapGesture { close() }
                .transition(.opacity)

            panel
                .padding(.leading, Self.anchorLeading)
                .padding(.top, ChatHeaderView.barHeight + 6)
                .transition(.scale(scale: 0.92, anchor: .topLeading).combined(with: .opacity))
        }
    }

    private var panel: some View {
        VStack(alignment: .leading, spacing: 0) {
            switch level {
            case .entries:
                entryRows
            case .models(let entry):
                modelRows(entry: entry)
            case .effort(let entry):
                effortRows(entry: entry)
            }
        }
        .frame(width: Self.panelWidth)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: Theme.radiusModal))
    }

    // MARK: - Levels

    /// L1: one row per LLM entry, by name (the "llm 名字").
    private var entryRows: some View {
        ForEach(catalog.models, id: \.name) { entry in
            row(
                title: entry.name,
                checked: entry.name == effectiveEntryName
            ) {
                push(.models(entry: entry.name))
            }
        }
    }

    /// L2: the entry's models (`model` + `model_candidates`), then the
    /// Thinking row (its subtitle the entry's current level) below a hairline.
    @ViewBuilder
    private func modelRows(entry entryName: String) -> some View {
        backRow(title: entryName) { push(.entries) }
        hairline
        if let entry = entryByName(entryName) {
            ForEach(catalog.models(of: entry), id: \.self) { model in
                row(
                    title: model,
                    checked: entryName == effectiveEntryName && model == effectiveModel
                ) {
                    Haptics.tap()
                    store.selectModel(entry: entryName, model: model)
                    close()
                }
            }
            hairline
            row(
                title: Lang.shared.t("chat.effort"),
                subtitle: effortFor(entry).map { EffortLevel(rawValue: $0)?.label ?? $0 },
                chevron: true
            ) {
                push(.effort(entry: entryName))
            }
        }
    }

    /// L3: the effort ladder. A pick sets the session's per-session effort AND
    /// pins it to (entry, its relevant model).
    @ViewBuilder
    private func effortRows(entry entryName: String) -> some View {
        backRow(title: Lang.shared.t("chat.effort")) { push(.models(entry: entryName)) }
        hairline
        let current = entryByName(entryName).map(effortFor) ?? nil
        ForEach(EffortLevel.allCases, id: \.rawValue) { effort in
            row(
                title: effort.label,
                checked: current == effort.rawValue
            ) {
                Haptics.tap()
                if let entry = entryByName(entryName) {
                    store.selectEffort(
                        entryName: entryName, model: relevantModel(for: entry),
                        effort: effort.rawValue, catalog: catalog)
                }
                close()
            }
        }
    }

    // MARK: - Rows

    /// One selectable row: title (+ optional small subtitle) leading, the
    /// checkmark — or a sub-level `›` — pinned to the TRAILING edge (trailing
    /// marks are the whole point of the panel). The vertical padding is what
    /// keeps a subtitled row's small text off the panel's bottom edge.
    private func row(
        title: String, subtitle: String? = nil, checked: Bool = false,
        chevron: Bool = false, action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: title)
                        .font(Theme.mono(14))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                    if let subtitle {
                        Text(verbatim: subtitle)
                            .font(Theme.mono(11))
                            .foregroundStyle(Theme.inkSoft)
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 8)
                if checked {
                    Image(systemName: "checkmark")
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                } else if chevron {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Theme.inkSoft)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .frame(minHeight: 46)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // Title-only label with the subtitle as VALUE: the concatenated
        // default would break by-label queries and read worse in VoiceOver.
        .accessibilityLabel(Text(verbatim: title))
        .accessibilityValue(Text(verbatim: subtitle ?? ""))
    }

    /// Sub-level header: a soft back affordance named after the parent.
    private func backRow(title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: "chevron.left")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.inkSoft)
                Text(verbatim: title)
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .frame(minHeight: 40)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(Text(verbatim: title))
    }

    private var hairline: some View {
        Rectangle()
            .fill(Theme.line.opacity(0.6))
            .frame(height: 1)
            .padding(.horizontal, 12)
    }

    // MARK: - Resolution

    private func entryByName(_ name: String) -> LlmModelInfo? {
        catalog.models.first { $0.name == name }
    }

    /// The entry the session resolves against: its pin, else `default-llm`.
    private var effectiveEntryName: String? {
        store.modelPin ?? catalog.defaultName
    }

    /// The model currently in effect: the pinned model, else the effective
    /// entry's default. What the L2 checkmark sits on.
    private var effectiveModel: String? {
        store.modelPinModel ?? catalog.effectiveEntry(pin: store.modelPin)?.model
    }

    /// The model a thinking-level pick under `entry` pins: the current model
    /// when the session already runs this entry, else the entry's default.
    private func relevantModel(for entry: LlmModelInfo) -> String {
        if entry.name == effectiveEntryName, let model = effectiveModel { return model }
        return entry.model
    }

    /// The effort shown/checked for an entry: the session's per-session pick
    /// (it applies whatever the entry), else the entry's configured default.
    private func effortFor(_ entry: LlmModelInfo) -> String? {
        store.modelPinEffort ?? entry.reasoningEffort
    }

    // MARK: - Actions

    private func push(_ next: Level) {
        withAnimation(.easeOut(duration: 0.12)) { level = next }
    }

    private func close() {
        withAnimation(.easeOut(duration: 0.15)) { isPresented = false }
    }
}

/// The gateway's reasoning-effort ladder — the values `baybo.json` accepts
/// for an entry's `reasoning_effort` (`crates/llm` registry contract). Raw
/// values go on the wire; labels are localized.
enum EffortLevel: String, CaseIterable {
    case none, minimal, low, medium, high, xhigh

    @MainActor var label: String { Lang.shared.t("chat.effort.\(rawValue)") }
}
