import Foundation
import Testing

@testable import Baybo

/// Missing translations echo their key, so ordinary label assertions can stay
/// green while raw keys ship. Keep source references and both locales complete.
@Suite struct LocalizedKeyTests {
    private static let dynamicPrefixes = ["lang."]

    @Test func everyReferencedKeyIsInTheCatalog() throws {
        let catalog = try Self.catalogKeys()
        #expect(!catalog.isEmpty, "the string catalog should not be empty")

        var missing: [String] = []
        for (file, key) in try Self.referencedKeys() {
            if catalog.contains(key) { continue }
            if Self.dynamicPrefixes.contains(where: { key.hasPrefix($0) }) { continue }
            missing.append("\(file): \(key)")
        }
        #expect(missing.sorted() == [], "keys referenced but never defined")
    }

    @Test func bothLanguagesCarryEveryKey() throws {
        let root = try Self.catalog()
        var missing: [String] = []
        for (key, entry) in root {
            let locales = (entry["localizations"] as? [String: Any]) ?? [:]
            for language in ["en", "zh-Hans"] where locales[language] == nil {
                missing.append("\(key) [\(language)]")
            }
        }
        #expect(missing.sorted() == [])
    }

    // MARK: - Sources

    private static func repoRoot() -> URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
    }

    private static func catalog() throws -> [String: [String: Any]] {
        let url = repoRoot().appendingPathComponent("App/Resources/Localizable.xcstrings")
        let data = try Data(contentsOf: url)
        let root = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        return (root?["strings"] as? [String: [String: Any]]) ?? [:]
    }

    private static func catalogKeys() throws -> Set<String> {
        Set(try catalog().keys)
    }

    private static func referencedKeys() throws -> [(file: String, key: String)] {
        let appDir = repoRoot().appendingPathComponent("App")
        let pattern = try NSRegularExpression(
            pattern: #"(?:lang|Lang\.shared)\.t\("([A-Za-z][\w.]*)""#)
        var found: [(String, String)] = []
        guard
            let walker = FileManager.default.enumerator(
                at: appDir, includingPropertiesForKeys: nil)
        else { return [] }
        for case let url as URL in walker where url.pathExtension == "swift" {
            let text = try String(contentsOf: url, encoding: .utf8)
            let range = NSRange(text.startIndex..., in: text)
            for match in pattern.matches(in: text, range: range) {
                guard let keyRange = Range(match.range(at: 1), in: text) else { continue }
                found.append((url.lastPathComponent, String(text[keyRange])))
            }
        }
        return found
    }
}
