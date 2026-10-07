import Foundation

enum LlmModelInput {
    static func parse(_ text: String) -> [String] {
        var seen = Set<String>()
        return text.components(
            separatedBy: .whitespacesAndNewlines.union(CharacterSet(charactersIn: ",，"))
        )
        .filter { !$0.isEmpty && seen.insert($0).inserted }
    }
}
