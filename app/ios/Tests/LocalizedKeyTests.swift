import Foundation
import Testing

@testable import Baybo

/// Every `lang.t("…")` key must exist in the catalog.
///
/// A miss is INVISIBLE. `Lang.t` falls back to echoing the key, so a screen
/// ships with a button labelled `chat.cancel` and every existing assertion
/// stays green — an `XCTAssert` on a label matches the key just as happily as
/// the word. This suite found exactly that on the card screen's Stop dialog,
/// where `chat.cancel` had never existed.
@Suite struct LocalizedKeyTests {
    /// Keys built at runtime rather than written as literals. `lang.label` is
    /// `Lang`'s own `"lang.\(code)"`.
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

    /// Both languages carry every key. One present in `en` and absent in
    /// `zh-Hans` renders as the raw key on that language's screen, and the
    /// suite runs pinned to English — so nothing else here would ever see it.
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
        // `#filePath` is `<app/ios>/Tests/LocalizedKeyTests.swift`.
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
