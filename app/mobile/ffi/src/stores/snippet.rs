//! Excerpt + highlight for a search result card.
//!
//! `app/web/src/pages/chat/searchSnippet.ts` is the reference implementation and
//! this is the port, held to it byte for byte by the shared vectors in
//! `app/web/src/pages/chat/searchSnippetVectors.json` — the same file app/web's
//! own vitest suite asserts against. One fixture, several readers, which is the
//! arrangement the transcript already uses for its row DTO.
//!
//! It moved here from `app/ios/App/Core/SearchSnippet.swift` because Android
//! would otherwise have been the THIRD implementation of a piece of Unicode
//! index arithmetic whose failure mode is a highlight one cluster off — the kind
//! of bug that survives review on every side independently.
//!
//! Everything works in GRAPHEME CLUSTER space. Swift gets that for free
//! (`Character`), the JS side reaches for `Intl.Segmenter`, and Rust needs
//! `unicode-segmentation`. That is what keeps a window edge from splitting an
//! emoji ZWJ sequence or stranding a combining mark.

use unicode_segmentation::UnicodeSegmentation;

/// Clusters of prose kept BEFORE the match — front-loaded so the highlight
/// survives the result card's line clamp. Must equal the JS `LEAD_PAD`; the
/// shared vectors fail loudly if they drift.
const LEAD_PAD: usize = 12;
/// And AFTER it — the rest of the same budget.
const TRAIL_PAD: usize = 108;
/// Head shown when no term can be located.
const HEAD_LENGTH: usize = LEAD_PAD + TRAIL_PAD;

/// A run of the message, flagged when it is one of the query's terms.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SnippetSegment {
    pub text: String,
    pub is_match: bool,
}

/// Split a query the way the server does: the user's own whitespace is the AND
/// boundary, and a chunk with nothing alphanumeric contributes no tokens and is
/// dropped rather than searched for.
///
/// `is_alphabetic() || is_numeric()` are the `\p{L}` / `\p{N}` the JS regex uses
/// — note numeric rather than "is a digit", since `\p{N}` covers Nl and No too.
#[uniffi::export]
pub fn search_query_chunks(query: String) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|chunk| chunk.chars().any(|c| c.is_alphabetic() || c.is_numeric()))
        .map(str::to_string)
        .collect()
}

/// Case-fold cluster by cluster, never the string as a whole.
///
/// Folding a whole string can change its LENGTH (`İ` lowercases to two scalars),
/// which slides every subsequent index — so matching on a folded string and
/// slicing the original is only correct until someone types Turkish. Per-cluster
/// folding keeps the folded vector index-aligned with the original by
/// construction, whatever any one cluster does.
fn folded(clusters: &[&str]) -> Vec<String> {
    clusters.iter().map(|c| c.to_lowercase()).collect()
}

/// Cluster indices where `needle` occurs in `haystack` (both already folded).
fn occurrences(haystack: &[String], needle: &[String]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at + needle.len() <= haystack.len() {
        if haystack[at..at + needle.len()] == *needle {
            out.push(at);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    out
}

/// Cut a window of `text` around what the query matched, flagging the terms.
///
/// Anchors on the EARLIEST term rather than the whole input: the server ANDs
/// each chunk independently, so `foo bar` legitimately matches a message reading
/// `bar … foo`. Searching for the literal input finds nothing there and falls
/// back to the head — a real hit rendered with an excerpt holding none of what
/// was searched for.
#[uniffi::export]
pub fn search_snippet(text: String, query: String) -> Vec<SnippetSegment> {
    let clusters: Vec<&str> = text.graphemes(true).collect();
    let lowered = folded(&clusters);

    let mut hits: Vec<(usize, usize)> = Vec::new();
    for chunk in search_query_chunks(query) {
        let needle = folded(&chunk.graphemes(true).collect::<Vec<_>>());
        for at in occurrences(&lowered, &needle) {
            hits.push((at, at + needle.len()));
        }
    }

    let cut = |from: usize, to: usize| -> String {
        if from >= to {
            return String::new();
        }
        clusters[from..to.min(clusters.len())].concat()
    };

    if hits.is_empty() {
        let head = cut(0, HEAD_LENGTH.min(clusters.len()));
        let ellipsis = if clusters.len() > HEAD_LENGTH {
            "…"
        } else {
            ""
        };
        return vec![SnippetSegment {
            text: format!("{head}{ellipsis}"),
            is_match: false,
        }];
    }

    // Stable: two terms matching at the same index keep the order the query
    // listed them, exactly as the JS comparator leaves them. `sort_by_key` is
    // stable in Rust, unlike Swift's `sort`, which is why the Swift port had to
    // sort on a pair and this does not.
    hits.sort_by_key(|(at, _)| *at);

    let (anchor_at, anchor_end) = hits[0];
    let from = anchor_at.saturating_sub(LEAD_PAD);
    let to = (anchor_end + TRAIL_PAD).min(clusters.len());

    let mut segments: Vec<SnippetSegment> = Vec::new();
    let push = |text: String, is_match: bool, out: &mut Vec<SnippetSegment>| {
        if !text.is_empty() {
            out.push(SnippetSegment { text, is_match });
        }
    };

    let mut cursor = from;
    for (at, end) in hits {
        // Highlight every term landing in the window, not just the anchor.
        // Overlapping hits (a term inside another) collapse into the first.
        if at < cursor || end > to {
            continue;
        }
        push(cut(cursor, at), false, &mut segments);
        push(cut(at, end), true, &mut segments);
        cursor = end;
    }
    push(cut(cursor, to), false, &mut segments);

    if from > 0 {
        segments.insert(
            0,
            SnippetSegment {
                text: "…".to_string(),
                is_match: false,
            },
        );
    }
    if to < clusters.len() {
        segments.push(SnippetSegment {
            text: "…".to_string(),
            is_match: false,
        });
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cross-end gate, over the SAME file app/web's vitest suite and
    /// `app/ios/Tests/SearchSnippetVectorTests.swift` read. Owned by app/web,
    /// the reference implementation; regenerate with
    /// `pnpm --filter baybo-web gen:snippet-vectors`.
    ///
    /// Read off disk rather than embedded with `include_str!`, deliberately: a
    /// second copy is a second thing to regenerate, i.e. a new drift surface
    /// inside the gate built to close one.
    #[test]
    fn every_vector_matches_the_reference_implementation() {
        #[derive(serde::Deserialize)]
        struct WireSegment {
            text: String,
            #[serde(rename = "match")]
            is_match: bool,
        }
        #[derive(serde::Deserialize)]
        struct Vector {
            name: String,
            text: String,
            query: String,
            segments: Vec<WireSegment>,
        }

        // ffi/ -> app/mobile/ -> app/ -> repo root
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../app/web/src/pages/chat/searchSnippetVectors.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "shared snippet vectors not found at {} ({e}); they are owned by \
                 app/web — regenerate with `pnpm --filter baybo-web gen:snippet-vectors`",
                path.display()
            )
        });
        let vectors: Vec<Vector> = serde_json::from_str(&raw).expect("vectors parse");
        assert!(!vectors.is_empty(), "the fixture must not be empty");

        for vector in &vectors {
            let got = search_snippet(vector.text.clone(), vector.query.clone());
            let want: Vec<SnippetSegment> = vector
                .segments
                .iter()
                .map(|s| SnippetSegment {
                    text: s.text.clone(),
                    is_match: s.is_match,
                })
                .collect();
            assert_eq!(got, want, "vector: {}", vector.name);
        }
    }

    /// The rule the shared vectors cannot state, because they only ever carry
    /// whole queries: a chunk with no letter or digit is dropped rather than
    /// searched for, so punctuation between terms never becomes a term.
    #[test]
    fn a_chunk_with_nothing_alphanumeric_is_not_a_term() {
        assert_eq!(
            search_query_chunks("  foo   --   bar ".to_string()),
            vec!["foo".to_string(), "bar".to_string()]
        );
        assert!(search_query_chunks("--- ???".to_string()).is_empty());
    }
}
