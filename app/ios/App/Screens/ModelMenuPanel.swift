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
    /// The model pill's frame in `anchorSpace`, reported by `ChatHeaderView` —
    /// the panel's leading edge sits on the pill's.
    let anchor: CGRect
    @Binding var isPresented: Bool

    private enum Level: Equatable {
        case entries
        case models(entry: String)
        case effort(entry: String)
    }

    @State private var level: Level = .entries
    /// This layer's own frame in `anchorSpace`: the origin the anchor is read
    /// against, and the height the panel has to fit in — the stretch between the
    /// status bar and the dock's top edge, keyboard already taken out.
    @State private var bounds: CGRect = .zero
    /// Natural height of the current level's rows, measured to decide whether
    /// the panel has to become a scroller (`ApprovalCardView`'s idiom).
    @State private var rowsHeight: CGFloat = 0

    /// The one width knob. Content-independent on purpose: rows truncate
    /// rather than stretch the panel.
    private static let panelWidth: CGFloat = 232
    /// `ChatScreen`'s coordinate space, shared by the pill that reports its
    /// frame and this panel that lays itself out against it.
    static let anchorSpace = "baybo.chat.screen"
    /// The panel's top edge, just under the header bar the pill rides in.
    static let panelTop: CGFloat = ChatHeaderView.barHeight + 6
    /// Air between the panel's bottom edge and the dock's top edge.
    static let bottomClearance: CGFloat = 12
    /// The shortest a scrolling panel gets: one row, so it never collapses to a
    /// sliver on an absurdly short window.
    static let minScrollHeight: CGFloat = 46

    /// How tall the panel may draw, or `nil` when its rows fit as they are. A
    /// short window — a foldable's outer display in landscape, the inner one or
    /// any phone in landscape with the keyboard up — has less room under the
    /// header than a long model list needs, and an uncapped panel ran on behind
    /// the dock and the keyboard where its last rows could not be reached.
    /// Unmeasured (`available <= 0`) never scrolls.
    static func scrollHeight(rows: CGFloat, available: CGFloat) -> CGFloat? {
        guard available > 0, rows > available else { return nil }
        return max(minScrollHeight, available)
    }

    /// The panel's leading inset inside a layer whose frame is `bounds`: on the
    /// pill's leading edge, held inside the layer so a narrow window can't push
    /// the panel off its trailing side. Both frames are in `anchorSpace`, so a
    /// Split View half's asymmetric insets are already in them.
    static func leadingInset(anchor: CGRect, bounds: CGRect, panelWidth: CGFloat) -> CGFloat {
        max(0, min(anchor.minX - bounds.minX, bounds.width - panelWidth))
    }

    /// The panel's top inset inside the layer: just under the header bar, or —
    /// when the anchor sits lower, a circle in the side rail — level with it.
    /// A rail anchor lies past the layer's trailing edge, so `leadingInset`
    /// already clamps the panel flush against the column, beside the circle.
    static func topInset(anchor: CGRect, bounds: CGRect) -> CGFloat {
        max(panelTop, anchor.minY - bounds.minY)
    }

    /// Whether the panel opens beside a side-rail anchor rather than under a
    /// pill in the bar — which edge it blooms from.
    static func bloomsFromTrailing(anchor: CGRect, bounds: CGRect) -> Bool {
        anchor.minX >= bounds.maxX
    }

    var body: some View {
        ZStack(alignment: .topLeading) {
            // A whisper of dim doubles as the outside-tap dismiss surface; it
            // must swallow scrolls so the transcript can't move underneath.
            Theme.panelScrim
                .ignoresSafeArea()
                .onTapGesture { close() }
                .transition(.opacity)

            panel
                .padding(
                    .leading,
                    Self.leadingInset(anchor: anchor, bounds: bounds, panelWidth: Self.panelWidth)
                )
                .padding(.top, Self.topInset(anchor: anchor, bounds: bounds))
                .transition(
                    .scale(
                        scale: 0.92,
                        anchor: Self.bloomsFromTrailing(anchor: anchor, bounds: bounds)
                            ? .topTrailing : .topLeading
                    ).combined(with: .opacity))
        }
        .onGeometryChange(for: CGRect.self) { proxy in
            proxy.frame(in: .named(Self.anchorSpace))
        } action: { frame in
            bounds = frame
        }
    }

    @ViewBuilder private var panel: some View {
        let cap = Self.scrollHeight(
            rows: rowsHeight,
            available: bounds.height - Self.topInset(anchor: anchor, bounds: bounds)
                - Self.bottomClearance)
        Group {
            if let cap {
                ScrollView { rows }
                    .scrollBounceBehavior(.basedOnSize)
                    .frame(maxHeight: cap)
            } else {
                rows
            }
        }
        .frame(width: Self.panelWidth)
        .glassSurface(
            in: RoundedRectangle(cornerRadius: Theme.radiusModal),
            fallback: .regularMaterial, fallbackShadow: true)
    }

    private var rows: some View {
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
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { rowsHeight = $0 }
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
            if !entry.availableEfforts.isEmpty {
                hairline
                row(
                    title: Lang.shared.t("chat.effort"),
                    subtitle: effortFor(entry).map { EffortLevel.named($0)?.label ?? $0 },
                    chevron: true
                ) {
                    push(.effort(entry: entryName))
                }
            }
        }
    }

    /// L3: the thinking ladder. A pick sets the session's per-session effort
    /// AND pins it to (entry, its relevant model).
    ///
    /// The rungs come from the ENTRY, not a local list: each provider speaks
    /// its own effort vocabulary, and offering a rung its dialect cannot say
    /// would be a pick that never reaches the wire. The gateway sends the
    /// rungs it can actually translate for that provider.
    @ViewBuilder
    private func effortRows(entry entryName: String) -> some View {
        backRow(title: Lang.shared.t("chat.effort")) { push(.models(entry: entryName)) }
        hairline
        let entry = entryByName(entryName)
        let current = entry.map(effortFor) ?? nil
        ForEach(entry?.availableEfforts ?? [], id: \.self) { level in
            row(
                title: EffortLevel.named(level)?.label ?? level,
                checked: current == level
            ) {
                Haptics.tap()
                if let entry {
                    store.selectEffort(
                        entryName: entryName, model: relevantModel(for: entry),
                        effort: level, catalog: catalog)
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

/// Localized labels for baybo's thinking ladder (`baybo_llm::effort`). Which
/// rungs a given entry offers comes from the gateway (`availableEfforts`) —
/// this only names them, so a rung baybo learns later still renders (as its
/// raw value) instead of vanishing from the panel.
///
/// `none` is the pre-ladder spelling of `off`; the gateway canonicalises new
/// pins, but a session pinned before that still reads back the old string.
enum EffortLevel: String, CaseIterable {
    case off, minimal, low, medium, high, xhigh, max

    @MainActor var label: String { Lang.shared.t("chat.effort.\(rawValue)") }

    /// `none` is the pre-ladder spelling of `off`. The gateway canonicalises
    /// new pins, but a session pinned before that still reads back the old
    /// string — and a case literally named `none` would collide with
    /// `Optional.none` at every call site, so it stays a lookup instead.
    static func named(_ raw: String) -> EffortLevel? {
        EffortLevel(rawValue: raw) ?? (raw == "none" ? .off : nil)
    }
}
