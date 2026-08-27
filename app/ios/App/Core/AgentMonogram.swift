import Foundation

/// Monograms are resolved for the whole roster so collisions widen consistently
/// across board rows, pickers, and filters.
enum AgentMonogram {
    private static let maxLeading = 2

    static func map(for members: [TeamMemberInfo]) -> [String: String] {
        var out: [String: String] = [:]
        for width in 1...maxLeading {
            out = Dictionary(
                uniqueKeysWithValues: members.map { ($0.id, of($0.handle, leading: width)) })
            if Set(out.values).count == members.count { break }
        }
        return out
    }

    /// One handle's monogram at a given first-segment width. The fallback for
    /// a face drawn with no set around it.
    static func of(_ handle: String, leading: Int = 1) -> String {
        let parts = handle.split(separator: "-")
        guard let first = parts.first else { return String(handle.prefix(2)).uppercased() }
        guard parts.count >= 2, let tail = parts[1].first else {
            return String(first.prefix(max(2, leading))).uppercased()
        }
        return first.prefix(leading).uppercased() + String(tail).uppercased()
    }
}
