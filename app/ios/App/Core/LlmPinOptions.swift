import Foundation

/// The rows an LLM pin is picked from: the `baybo.json` entry, the model
/// inside it, and how hard it thinks.
///
/// **A mirror of `app/web`'s `pages/projects/teamModel.ts`**, deliberately —
/// the two clients write the same three-field pin to the same endpoint, and
/// these are the rules that go wrong quietly when they are written twice: a
/// model is only pickable within an entry, changing the entry invalidates the
/// model, a provider baybo sends no effort to must be offered no ladder at
/// all, and a pin the pool has never heard of has to stay visible rather than
/// vanish. Pure and free of the catalog object so `LlmPinOptionsTests` can
/// state each rule against a fixture; `ModelCatalog` delegates to it, so the
/// chat header's picker and an agent's pin resolve an entry the same way.
@MainActor
enum LlmPinOptions {
    /// One row. `nil` is the inherit row at every level — what the wire sends
    /// as `null`, and what the server reads as "take the level above".
    struct Row: Equatable {
        let value: String?
        let label: String
    }

    /// Which entry a pin actually runs on: the one it names, or the
    /// `default-llm` entry when it names none.
    ///
    /// What the model and thinking rows are drawn from — they describe the
    /// entry the agent *will* use, not the one it spelled out. `nil` while the
    /// catalog is empty, or when the pin names an entry the catalog no longer
    /// has.
    static func effectiveEntry(
        in entries: [LlmModelInfo], defaultName: String?, pinned: String?
    ) -> LlmModelInfo? {
        let name = pinned ?? defaultName
        return entries.first { $0.name == name }
    }

    /// The models an entry can be pinned to — its own `model` first, then each
    /// `model_candidates` id, de-duped. An entry that lists its default among
    /// its candidates must not draw the same row twice.
    static func models(of entry: LlmModelInfo) -> [String] {
        var seen = Set<String>()
        return ([entry.model] + entry.modelCandidates).filter { seen.insert($0).inserted }
    }

    /// The entry rows: inherit first, then one per entry by name.
    ///
    /// The inherit row is labelled with the entry it resolves to today
    /// (`Default · deepseek`) but carries `nil`, so an agent that takes it
    /// follows `default-llm` wherever that moves.
    ///
    /// The entry which *is* default today still gets its own named row. It has
    /// to: a model can only be picked within a NAMED entry — the server
    /// refuses a model with no entry — so collapsing the two would make every
    /// model inside the deployment's most-used entry unreachable.
    static func entryRows(
        entries: [LlmModelInfo], defaultName: String?, pinned: String?
    ) -> [Row] {
        withStale(
            [Row(value: nil, label: inheritLabel(defaultName))]
                + entries.map { Row(value: $0.name, label: $0.name) },
            pinned: pinned)
    }

    /// The model rows for whichever entry the pin resolves to. The inherit row
    /// follows that entry's own default model, so an entry serving one model
    /// needs nothing set.
    static func modelRows(
        entries: [LlmModelInfo], defaultName: String?, entry: String?, pinned: String?
    ) -> [Row] {
        guard let resolved = effectiveEntry(in: entries, defaultName: defaultName, pinned: entry)
        else { return [] }
        let all = models(of: resolved)
        return withStale(
            [Row(value: nil, label: entryDefaultLabel(all.first))]
                + all.map { Row(value: $0, label: $0) },
            pinned: pinned)
    }

    /// The thinking rows — taken from the ENTRY, never a local ladder: each
    /// provider speaks its own effort vocabulary, and a rung its dialect
    /// cannot say is a pick the gateway refuses.
    ///
    /// **Empty is the meaningful answer**, not a missing one: an entry whose
    /// provider is sent no effort at all gets no rows, and the caller draws no
    /// field rather than a disabled one advertising a knob that does not exist.
    static func effortRows(
        entries: [LlmModelInfo], defaultName: String?, entry: String?, pinned: String?
    ) -> [Row] {
        guard let resolved = effectiveEntry(in: entries, defaultName: defaultName, pinned: entry),
            !resolved.availableEfforts.isEmpty
        else { return [] }
        return withStale(
            [Row(value: nil, label: Lang.shared.t("agent.entryDefault"))]
                + resolved.availableEfforts.map {
                    Row(value: $0, label: EffortLevel.named($0)?.label ?? $0)
                },
            pinned: pinned)
    }

    /// One home for the rule that a pin the pool has never heard of stays
    /// **visible and clearable** rather than vanishing: an entry dropped from
    /// `baybo.json`, a model taken off a `model_list`, a rung a provider
    /// stopped expressing. A row that disappears leaves the picker showing
    /// something else while the agent goes on failing on the old value; a row
    /// that says "(unavailable)" is the visible version of a pin that will not
    /// work.
    private static func withStale(_ rows: [Row], pinned: String?) -> [Row] {
        guard let pinned, !rows.contains(where: { $0.value == pinned }) else { return rows }
        return rows + [Row(value: pinned, label: Lang.shared.t("agent.unavailable", pinned))]
    }

    private static func inheritLabel(_ defaultName: String?) -> String {
        guard let defaultName else { return Lang.shared.t("agent.inherit") }
        return Lang.shared.t("agent.defaultEntry", defaultName)
    }

    private static func entryDefaultLabel(_ model: String?) -> String {
        Lang.shared.t("agent.entryDefaultModel", model ?? "default")
    }
}
