//! Markdown rendering for assistant answers.
//!
//! [`MarkdownStream`] is fed the answer's source lines as they stream and
//! returns finished, already-wrapped rows. It emits a block only once appending
//! more source cannot change how that block renders, because scrollback is
//! append-only — see [`scan`] for the boundary rules.
//!
//! Rows come back without the `● `/`  ` answer leader; the caller in
//! [`crate::chat`] adds it, so the width passed here is the terminal width minus
//! that leader.

mod render;
mod scan;
mod wrap;

use ratatui::text::Line;

use scan::{Chunk, Marker, Scanner};

pub(crate) use wrap::{Gutter, pad, wrap};

/// What the stream emitted last, which decides whether the next block needs a
/// blank row above it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Emitted {
    Block,
    /// A list item, tagged with whether its list is numbered — switching
    /// between a bullet list and a numbered one starts a new list, which needs
    /// the separator that items within one list do not.
    ListItem {
        ordered: bool,
    },
    Code,
}

#[derive(Default)]
pub(crate) struct MarkdownStream {
    scanner: Scanner,
    last: Option<Emitted>,
    /// Container nesting level. A blockquote or list item renders its body by
    /// re-scanning it one level down; the depth bounds that recursion.
    depth: usize,
}

impl MarkdownStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one complete source line, returning any rows it completed.
    pub(crate) fn push_line(&mut self, line: &str, width: usize) -> Vec<Line<'static>> {
        let chunks = self.scanner.push_line(line);
        self.render(chunks, width)
    }

    /// Emit any buffered block so a tool or status block can commit below it.
    /// An open fence stays open: it buffers nothing, so its tail keeps rendering
    /// as code after the interruption.
    pub(crate) fn flush(&mut self, width: usize) -> Vec<Line<'static>> {
        let chunks = self.scanner.flush();
        self.render(chunks, width)
    }

    /// End the answer, closing anything still open.
    pub(crate) fn finish(&mut self, width: usize) -> Vec<Line<'static>> {
        let chunks = self.scanner.finish();
        self.render(chunks, width)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// The source line that opened the fence currently in progress. A tool block
    /// landing mid-fence ends the answer's transcript record, so the record that
    /// continues the answer has to re-open the fence or replay would re-parse
    /// committed code rows as markdown.
    pub(crate) fn fence_continuation(&self) -> Option<String> {
        self.scanner.fence_continuation()
    }

    fn nested(depth: usize) -> Self {
        Self {
            depth,
            ..Self::default()
        }
    }

    fn render(&mut self, chunks: Vec<Chunk>, width: usize) -> Vec<Line<'static>> {
        let mut rows = Vec::new();
        for chunk in chunks {
            let (kind, mut block_rows) = match chunk {
                Chunk::Block { source, list } => {
                    let kind = match &list {
                        Some(ctx) => Emitted::ListItem {
                            ordered: matches!(ctx.marker, Marker::Ordered(_)),
                        },
                        None => Emitted::Block,
                    };
                    (
                        kind,
                        render::block(&source, list.as_ref(), width, self.depth),
                    )
                }
                Chunk::CodeStart { lang } => {
                    (Emitted::Code, render::code_start(lang.as_deref(), width))
                }
                Chunk::Code { text } => (Emitted::Code, render::code_line(&text, width)),
                Chunk::CodeEnd => continue,
            };
            // A chunk that rendered nothing — a fence opener with no language,
            // an empty block — must not latch `last`. Latching it would let the
            // next chunk think it is continuing that kind and suppress its own
            // separator, and would emit a leading blank row that then collects
            // the answer's `● ` leader instead of the first row of real text.
            if block_rows.is_empty() {
                continue;
            }
            if self.needs_separator(kind) {
                rows.push(Line::default());
            }
            rows.append(&mut block_rows);
            self.last = Some(kind);
        }
        rows
    }

    /// Blocks are separated by one blank row, except within a tight list and
    /// within a code fence.
    fn needs_separator(&self, next: Emitted) -> bool {
        match (self.last, next) {
            (None, _) => false,
            (Some(Emitted::ListItem { ordered: was }), Emitted::ListItem { ordered: now }) => {
                was != now
            }
            (Some(Emitted::Code), Emitted::Code) => false,
            _ => true,
        }
    }
}

/// Render a complete answer that never streamed (a cron delivery or a synthetic
/// message), so it lays out identically to the streamed path.
pub(crate) fn render_document(source: &str, width: usize) -> Vec<Line<'static>> {
    render_nested(source, width, 0)
}

/// Render a container's body at `depth`, used by [`render::block`] for a
/// blockquote's or list item's contents. Runs the same scan-and-render pass, so
/// nested content supports every block kind the top level does.
pub(crate) fn render_nested(source: &str, width: usize, depth: usize) -> Vec<Line<'static>> {
    let mut stream = MarkdownStream::nested(depth);
    let mut rows = Vec::new();
    for line in source.lines() {
        rows.extend(stream.push_line(line, width));
    }
    rows.extend(stream.finish(width));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Stream `source` in `size`-char chunks the way the event loop does.
    fn stream_chunked(source: &str, size: usize, width: usize) -> Vec<String> {
        let mut stream = MarkdownStream::new();
        let mut rows = Vec::new();
        let mut buf = String::new();
        let chars: Vec<char> = source.chars().collect();
        for piece in chars.chunks(size) {
            buf.extend(piece);
            while let Some(idx) = buf.find('\n') {
                let line: String = buf.drain(..=idx).collect();
                rows.extend(stream.push_line(line.trim_end_matches('\n'), width));
            }
        }
        if !buf.is_empty() {
            let line = std::mem::take(&mut buf);
            rows.extend(stream.push_line(&line, width));
        }
        rows.extend(stream.finish(width));
        texts(&rows)
    }

    const DOC: &str = r#"# Title

Some **bold** prose that is long enough to wrap at a narrow width.

- first item
- second item

```rust
fn main() {}
```

| lang | note |
|---|---|
| rust | fast |

> quoted text

Tail paragraph.
"#;

    #[test]
    fn streaming_matches_a_whole_document_render_at_every_chunk_size() {
        for width in [24usize, 40, 80] {
            let want = texts(&render_document(DOC, width));
            for size in 1..40 {
                assert_eq!(
                    stream_chunked(DOC, size, width),
                    want,
                    "width {width} chunk {size} diverged"
                );
            }
        }
    }

    #[test]
    fn no_row_ever_exceeds_the_width() {
        for width in 4..90 {
            for row in &render_document(DOC, width) {
                assert!(
                    row.width() <= width,
                    "width {width}: {} cols: {:?}",
                    row.width(),
                    texts(std::slice::from_ref(row))
                );
            }
        }
    }

    #[test]
    fn switching_list_kind_starts_a_new_list() {
        let rows = texts(&render_document("- bullet\n1. numbered\n", 40));
        assert_eq!(rows, vec!["• bullet", "", "1. numbered"], "{rows:?}");
    }

    #[test]
    fn a_blank_row_separates_blocks_but_not_list_items() {
        let rows = texts(&render_document("para one\n\n- a\n- b\n\npara two\n", 40));
        assert_eq!(
            rows,
            vec!["para one", "", "• a", "• b", "", "para two"],
            "{rows:?}"
        );
    }

    #[test]
    fn code_rows_are_not_separated_from_each_other() {
        let rows = texts(&render_document("```\none\ntwo\n```\n", 40));
        assert_eq!(rows, vec!["│ one", "│ two"], "{rows:?}");
    }

    #[test]
    fn a_fence_with_no_language_does_not_steal_the_separator() {
        // `CodeStart { lang: None }` renders no rows. Latching it as "emitted
        // code" made the first real code row suppress its own separator.
        let rows = texts(&render_document("para\n\n```\ncode\n```\n", 40));
        assert_eq!(rows, vec!["para", "", "│ code"], "{rows:?}");
    }

    #[test]
    fn a_document_opening_with_a_bare_fence_has_no_leading_blank() {
        let rows = texts(&render_document("```\ncode\n```\n", 40));
        assert_eq!(rows, vec!["│ code"], "{rows:?}");
    }

    #[test]
    fn a_leading_blank_is_never_emitted_first() {
        for source in [
            "para",
            "# H",
            "- item",
            "```\nx\n```",
            "```rust\nx\n```",
            "> q",
        ] {
            let rows = render_document(source, 40);
            assert!(
                !rows.is_empty() && !texts(&rows)[0].trim().is_empty(),
                "source {source:?} led with a blank: {:?}",
                texts(&rows)
            );
        }
    }

    #[test]
    fn a_tool_boundary_mid_fence_keeps_styling_the_tail_as_code() {
        let mut stream = MarkdownStream::new();
        stream.push_line("```rust", 40);
        let before = texts(&stream.push_line("let a = 1;", 40));
        assert_eq!(before, vec!["│ let a = 1;"]);
        assert!(stream.flush(40).is_empty(), "a fence buffers nothing");
        let after = texts(&stream.push_line("let b = 2;", 40));
        assert_eq!(after, vec!["│ let b = 2;"], "the fence did not survive");
    }

    /// The rows committed live must equal the rows a replay produces from the
    /// recorded source. This mirrors the real sequence in `crate::lib`: stream
    /// some lines, a tool boundary forces a flush and closes the transcript run,
    /// then the answer continues into a second record that re-opens any fence.
    #[test]
    fn live_rows_equal_replay_rows_across_a_forced_flush() {
        const WIDTH: usize = 40;
        let cases: &[(&[&str], &[&str])] = &[
            (&["intro paragraph"], &["and the rest"]),
            (&["# Heading", "", "body text"], &["more body"]),
            (&["- one", "- two"], &["- three"]),
            (&["```rust", "let a = 1;"], &["let b = 2;", "```"]),
            (&["```", "bare fence body"], &["more code", "```"]),
            (&["  ```py", "  indented = 1"], &["  more = 2", "  ```"]),
            (&["| a | b |", "|---|---|"], &["| 1 | 2 |", ""]),
            (&["> quoted"], &["after the quote"]),
            (&["para", "", "```sh", "ls"], &["pwd", "```", "", "tail"]),
        ];
        for (before, after) in cases {
            let mut stream = MarkdownStream::new();
            let mut live = Vec::new();
            let mut first: Vec<String> = Vec::new();

            for line in *before {
                live.extend(stream.push_line(line, WIDTH));
                first.push((*line).to_string());
            }
            let closed = stream.flush(WIDTH);
            if !closed.is_empty() {
                first.push(String::new());
                live.extend(closed);
            }

            // A tool block commits here, which ends the answer's transcript run.
            let mut second: Vec<String> = stream.fence_continuation().into_iter().collect();
            let mut live_second = Vec::new();
            for line in *after {
                live_second.extend(stream.push_line(line, WIDTH));
                second.push((*line).to_string());
            }
            live_second.extend(stream.finish(WIDTH));

            // Each record's own rows must re-render identically. The blank row
            // between two records is the commit layer's (`begin_block`), not the
            // stream's, so a leading blank on the continuation is expected.
            assert_eq!(
                texts(&live),
                texts(&render_document(&first.join("\n"), WIDTH)),
                "first record diverged for {before:?}"
            );
            let mut live_tail = texts(&live_second);
            if live_tail.first().is_some_and(|row| row.trim().is_empty()) {
                live_tail.remove(0);
            }
            assert_eq!(
                live_tail,
                texts(&render_document(&second.join("\n"), WIDTH)),
                "continuation record diverged for {before:?} then {after:?}"
            );
        }
    }

    #[test]
    fn a_tool_boundary_mid_paragraph_emits_the_paragraph_first() {
        let mut stream = MarkdownStream::new();
        assert!(stream.push_line("checking that now", 40).is_empty());
        assert_eq!(texts(&stream.flush(40)), vec!["checking that now"]);
    }

    #[test]
    fn reset_forgets_an_open_block() {
        let mut stream = MarkdownStream::new();
        stream.push_line("half a paragraph", 40);
        stream.reset();
        assert!(stream.finish(40).is_empty());
    }

    #[test]
    fn plain_prose_renders_to_exactly_its_own_text() {
        assert_eq!(
            texts(&render_document("stub-reply for: hello there", 80)),
            vec!["stub-reply for: hello there"]
        );
    }

    #[test]
    fn han_prose_wraps_and_loses_nothing() {
        let source = "帮我把这个函数重构一下，谢谢你的帮助。";
        for width in 6..40 {
            let rows = texts(&render_document(source, width));
            let joined: String = rows.concat();
            assert_eq!(
                joined.chars().filter(|c| *c != ' ').collect::<String>(),
                source.chars().filter(|c| *c != ' ').collect::<String>(),
                "width {width} lost content: {rows:?}"
            );
        }
    }
}
