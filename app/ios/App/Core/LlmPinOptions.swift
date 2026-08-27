import Foundation

@MainActor
enum LlmPinOptions {
    // Callers changing the entry must clear model and effort pins because both
    // option sets belong to the previously resolved entry.
    /// One row. `nil` is the inherit row at every level — what the wire sends
    /// as `null`, and what the server reads as "take the level above".
    struct Row: Equatable {
        let value: String?
        let label: String
    }

    static func effectiveEntry(
        in entries: [LlmModelInfo], defaultName: String?, pinned: String?
    ) -> LlmModelInfo? {
        let name = pinned ?? defaultName
        return entries.first { $0.name == name }
    }

    static func models(of entry: LlmModelInfo) -> [String] {
        var seen = Set<String>()
        return ([entry.model] + entry.modelCandidates).filter { seen.insert($0).inserted }
    }

    static func entryRows(
        entries: [LlmModelInfo], defaultName: String?, pinned: String?
    ) -> [Row] {
        withStale(
            [Row(value: nil, label: inheritLabel(defaultName))]
                + entries.map { Row(value: $0.name, label: $0.name) },
            pinned: pinned)
    }

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
