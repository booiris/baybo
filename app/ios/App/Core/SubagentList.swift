import Foundation

/// Pure presentation logic for the subagent sheet's rows — no bridge, no FFI
/// call, no clock of its own, so it is unit-testable on its own (the
/// `MessageOutline` shape).
enum SubagentList {
    /// The status word beside a row. Deliberately not the raw enum name: the
    /// sheet speaks the reader's language, and `pending` in particular is a
    /// scheduling fact ("queued"), not a failure.
    static func statusKey(_ status: ChatSubagentStatus) -> String {
        switch status {
        case .pending: "subagent.pending"
        case .running: "subagent.running"
        case .completed: "subagent.completed"
        case .failed: "subagent.failed"
        case .cancelled: "subagent.cancelled"
        case .unknown: "subagent.unknown"
        }
    }

    /// How long the child has been working, from the timestamp pair the
    /// gateway sends rather than a duration it computed — so a running child's
    /// clock ticks locally between polls instead of freezing between them.
    ///
    /// `nil` when it has not started, which is the one case with nothing
    /// honest to show.
    static func elapsed(
        startedAt: Date?, endedAt: Date?, now: Date = Date()
    ) -> TimeInterval? {
        guard let startedAt else { return nil }
        let end = endedAt ?? now
        // A clock that ran backwards (device time moved, or the row was
        // written by a host whose clock differs) reads as zero rather than as
        // a negative age.
        return max(0, end.timeIntervalSince(startedAt))
    }

    /// `2m 12s` / `47s` / `1h 04m`. Coarse on purpose — this is a glance, and
    /// a subagent that ran for an hour does not need its seconds.
    static func durationLabel(_ seconds: TimeInterval) -> String {
        let total = Int(seconds.rounded())
        if total < 60 { return "\(total)s" }
        let minutes = total / 60
        if minutes < 60 { return "\(minutes)m \(String(format: "%02d", total % 60))s" }
        return "\(minutes / 60)h \(String(format: "%02d", minutes % 60))m"
    }

    /// What names the row. The errand the parent authored is the useful label;
    /// the profile is the fallback for a child spawned before the gateway
    /// stamped it, and the id is the last resort so a row is never blank.
    static func title(task: String?, subagentType: String?, sessionId: String) -> String {
        let trimmed = task?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmed, !trimmed.isEmpty { return trimmed }
        if let subagentType, !subagentType.isEmpty { return subagentType }
        return sessionId
    }

    /// The `explorer · claude` line under the title. The backend is named only
    /// when it is NOT the in-process one — every child is `baybo` unless the
    /// parent asked otherwise, so printing it always would be noise on the
    /// overwhelming majority of rows.
    static func subtitle(subagentType: String?, backend: String) -> String {
        let profile = subagentType ?? ""
        let external = backend != Self.inProcessBackend ? backend : ""
        return [profile, external].filter { !$0.isEmpty }.joined(separator: " · ")
    }

    /// Mirrors the gateway's `BAYBO_BACKEND_TAG`.
    static let inProcessBackend = "baybo"

    /// RFC 3339 with fractional seconds, which is what the gateway's
    /// `DateTime<Utc>` serialises to; the plain formatter rejects those.
    static func date(_ raw: String?) -> Date? {
        guard let raw, !raw.isEmpty else { return nil }
        return fractionalParser.date(from: raw) ?? plainParser.date(from: raw)
    }

    private static let fractionalParser: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    private static let plainParser = ISO8601DateFormatter()
}

/// `.sheet(item:)` needs an identity, and the child's session id IS one — it is
/// the argument every read of that child is keyed by. Declared here rather than
/// in the generated FFI file, which is regenerated from Rust.
extension ChatSubagentSummary: @retroactive Identifiable {
    public var id: String { sessionId }
}
