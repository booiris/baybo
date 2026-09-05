import Foundation

/// Excerpt + highlight for a search result card — a forwarder to the shared
/// core, kept as a type so the call sites read the same as they always did.
///
/// The rules live in `app/web/src/pages/chat/searchSnippet.ts`, the reference
/// implementation, and are held to byte for byte by the shared vectors in
/// `app/web/src/pages/chat/searchSnippetVectors.json`. What used to be here was
/// a hand port of that file into Swift; it now lives once in the core
/// (`app/mobile/ffi/src/stores/snippet.rs`), because a second phone shell would
/// have made it the third implementation of Unicode index arithmetic whose
/// failure mode is a highlight one cluster off — a bug that survives review on
/// every side independently.
///
/// `SearchSnippetVectorTests` still runs, now through this forwarder, over the
/// same fixture the core's own test and app/web's vitest suite read.
enum SearchSnippet {
    /// A run of the message, flagged when it is one of the query's terms.
    ///
    /// A local shape rather than the generated `SnippetSegment`: `match` reads
    /// as it always did at the call sites, and uniffi cannot emit a field with
    /// that name in Swift because `match` is a keyword there.
    struct Segment: Equatable {
        let text: String
        let match: Bool
    }

    /// Split a query the way the server does: the user's own whitespace is the
    /// AND boundary, and a chunk with nothing alphanumeric is dropped rather
    /// than searched for.
    static func queryChunks(_ query: String) -> [String] {
        searchQueryChunks(query: query)
    }

    /// Cut a window of `text` around what the query matched, flagging the terms.
    static func snippet(_ text: String, query: String) -> [Segment] {
        searchSnippet(text: text, query: query)
            .map { Segment(text: $0.text, match: $0.isMatch) }
    }
}
