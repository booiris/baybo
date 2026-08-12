import XCTest

@testable import Baybo

/// The cross-end gate for the search excerpt.
///
/// `searchSnippet.ts` (app/web) is the reference implementation and
/// `SearchSnippet.swift` is the port; this suite holds the port to the reference
/// byte for byte, over the SAME file app/web's own vitest suite asserts against.
/// One fixture, two readers — the arrangement `restSentinel.ts` already
/// established for the transcript row DTO, and for the same reason: a second
/// copy is a second thing to regenerate, i.e. a new drift surface inside the
/// gate built to close one.
///
/// The fixture is read off disk rather than bundled. `#filePath` is the source
/// path of THIS file, so the walk up to the repo root holds wherever the
/// checkout lives, and there is no resource to remember to add to the target.
///
/// When it goes red after `pnpm --filter baybo-web gen:snippet-vectors`, that is
/// the gate working: the rules moved on the JS side and this port has not been
/// brought along.
final class SearchSnippetVectorTests: XCTestCase {
    private struct Vector: Decodable {
        let name: String
        let text: String
        let query: String
        let segments: [WireSegment]

        struct WireSegment: Decodable {
            let text: String
            let match: Bool
        }
    }

    private static var vectorsURL: URL {
        // Tests/ -> app/ios/ -> app/ -> repo root
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("app/web/src/pages/chat/searchSnippetVectors.json")
    }

    private func loadVectors() throws -> [Vector] {
        let url = Self.vectorsURL
        guard let data = try? Data(contentsOf: url) else {
            XCTFail(
                """
                Shared snippet vectors not found at \(url.path).
                They are owned by app/web (the reference implementation); regenerate with
                `pnpm --filter baybo-web gen:snippet-vectors`.
                """)
            return []
        }
        return try JSONDecoder().decode([Vector].self, from: data)
    }

    func testEveryVectorMatchesTheReferenceImplementation() throws {
        let vectors = try loadVectors()
        XCTAssertFalse(vectors.isEmpty, "the fixture must not be empty")

        for vector in vectors {
            let produced = SearchSnippet.snippet(vector.text, query: vector.query)
            let expected = vector.segments.map {
                SearchSnippet.Segment(text: $0.text, match: $0.match)
            }
            XCTAssertEqual(
                produced, expected,
                """
                vector "\(vector.name)" diverged from app/web.
                query: \(vector.query)
                produced: \(produced)
                expected: \(expected)
                """)
        }
    }

    /// The vectors only earn their keep if they still carry the cases a
    /// code-unit slice gets wrong. A regen that quietly dropped them would leave
    /// this suite green over a contract that no longer tests anything.
    func testTheFixtureStillCoversTheGraphemeCases() throws {
        let names = try loadVectors().map(\.name).joined(separator: "\n")
        for required in ["ZWJ emoji", "surrogate-pair", "decomposed combining mark"] {
            XCTAssertTrue(
                names.contains(required),
                "the shared fixture no longer covers \(required)")
        }
    }

    /// Swift cannot represent a lone surrogate in a `String` at all, so the
    /// failure this pins on the JS side shows up here as a mangled cluster
    /// instead: assert the excerpt is composed of whole clusters taken from the
    /// source, never a fragment of one.
    func testEveryExcerptIsBuiltFromWholeClusters() throws {
        for vector in try loadVectors() {
            let source = Set(vector.text.map(String.init))
            let produced = SearchSnippet.snippet(vector.text, query: vector.query)
            let joined = produced.map(\.text).joined()
            for cluster in joined where String(cluster) != "…" {
                XCTAssertTrue(
                    source.contains(String(cluster)),
                    """
                    vector "\(vector.name)" emitted \(String(reflecting: String(cluster))), \
                    which is not a whole cluster of the source text — a window edge split one.
                    """)
            }
        }
    }
}
