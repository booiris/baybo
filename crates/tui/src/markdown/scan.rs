//! Splits a streaming answer into complete markdown blocks.
//!
//! Scrollback is append-only: `Terminal::insert_before` can only add rows above
//! the live viewport, so a committed row can never be corrected. A block may
//! therefore be emitted only once appending more source cannot change how it
//! renders. That is per-block for everything with cross-line semantics, and
//! per-line for a fenced code body — nothing later reinterprets an earlier code
//! row, which is what keeps a 300-line code dump streaming instead of stalling.
//!
//! The scanner decides boundaries only. Everything inside a block is handed to
//! `pulldown-cmark`, so CommonMark semantics are not reimplemented here.

/// A unit the scanner has proven complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Chunk {
    /// Complete block source, to be parsed by the renderer. `list` is set when
    /// the source is one list item's content, whose marker the scanner owns
    /// because an item flushed on its own would otherwise renumber to `1.`.
    Block {
        source: String,
        list: Option<ListContext>,
    },
    /// A fenced code block opened, carrying its info string's language.
    CodeStart { lang: Option<String> },
    /// One line of an open fenced code block.
    Code { text: String },
    /// An open fence ended, or the answer finished while it was still open.
    CodeEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListContext {
    pub(crate) depth: usize,
    pub(crate) marker: Marker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    Bullet,
    Ordered(u64),
    Task(bool),
}

/// Spaces of source indent per nesting level.
const INDENT_PER_LEVEL: usize = 2;
/// Shortest run of backticks or tildes that opens a fence.
const MIN_FENCE: usize = 3;
/// Longest `#` run that is still a heading.
const MAX_HEADING_LEVEL: usize = 6;
/// Shortest run that forms a thematic break.
const MIN_THEMATIC_BREAK: usize = 3;
/// Guard against a pathological ordered-list marker.
const MAX_ORDINAL_DIGITS: usize = 9;

/// An open fenced code block. `indent` is the opening fence's own indentation,
/// which CommonMark strips from every body line.
#[derive(Debug, Clone)]
struct Fence {
    tick: char,
    len: usize,
    indent: usize,
}

impl Fence {
    /// A fence opener that re-opens this fence for a transcript record that
    /// continues it. Deliberately carries **no** info string: the language label
    /// row was already drawn by the record that opened the fence, so repeating it
    /// would draw it twice on replay. The indent is preserved so the body lines
    /// dedent by the same amount they did live.
    fn continuation_opener(&self) -> String {
        format!(
            "{}{}",
            " ".repeat(self.indent),
            self.tick.to_string().repeat(self.len)
        )
    }
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Idle,
    Fence(Fence),
    Paragraph(Vec<String>),
    List {
        depth: usize,
        marker: Marker,
        /// Columns of the item's indent plus its marker, stripped from
        /// continuation lines so indentation *inside* the item survives.
        content_indent: usize,
        lines: Vec<String>,
        /// A fence opened inside this item. While set, nothing terminates the
        /// item — a blank line or a `- `-shaped line inside a code block is
        /// content, not structure.
        fence: Option<Fence>,
        /// Blank lines seen since the last content line. They belong to the item
        /// only if a sufficiently-indented line follows; otherwise they are the
        /// separator that ended it.
        pending_blanks: usize,
    },
    Quote(Vec<String>),
    /// A `|` row whose delimiter row has not arrived yet, so it is not yet
    /// known to be a table.
    MaybeTable(String),
    Table(Vec<String>),
}

#[derive(Default)]
pub(crate) struct Scanner {
    state: State,
    /// Live ordinal per nesting depth, so incremental flushing still renders
    /// `1. 2. 3.` for the `1. 1. 1.` an LLM often emits.
    ordinals: Vec<u64>,
}

/// The fence a line opens, plus its info string's language.
fn fence_open(line: &str) -> Option<(Fence, Option<String>)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    for tick in ['`', '~'] {
        let len = trimmed.chars().take_while(|c| *c == tick).count();
        if len < MIN_FENCE {
            continue;
        }
        let info = trimmed[len..].trim();
        if tick == '`' && info.contains('`') {
            return None;
        }
        let lang = info
            .split_whitespace()
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        return Some((Fence { tick, len, indent }, lang));
    }
    None
}

fn fence_close(line: &str, fence: &Fence) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|c| c == fence.tick)
        && trimmed.chars().count() >= fence.len
}

/// Remove up to `indent` leading spaces, keeping any deeper indentation.
fn dedent(line: &str, indent: usize) -> &str {
    let strippable = line.len() - line.trim_start_matches(' ').len();
    &line[strippable.min(indent)..]
}

fn atx_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=MAX_HEADING_LEVEL).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

fn thematic_break(line: &str) -> bool {
    let stripped: String = line.trim().chars().filter(|c| !c.is_whitespace()).collect();
    stripped.chars().count() >= MIN_THEMATIC_BREAK
        && ['-', '*', '_']
            .iter()
            .any(|m| stripped.chars().all(|c| c == *m))
}

/// A setext underline closes the paragraph above it into a heading. Checked
/// before [`thematic_break`] while a paragraph is open, because CommonMark
/// resolves `text` + `---` as a level-2 heading rather than a rule.
fn setext_underline(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && (trimmed.chars().all(|c| c == '=') || trimmed.chars().all(|c| c == '-'))
}

/// `(nesting depth, content indent, marker, content)` for a list-item line.
/// The content indent is where the item's own text begins, which is how much a
/// continuation line may be dedented without losing its own structure.
fn list_item(line: &str) -> Option<(usize, usize, Marker, String)> {
    let indent = line.chars().take_while(|c| *c == ' ').count();
    let rest = &line[indent..];
    let (marker, after) = if let Some(after) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        (Marker::Bullet, after)
    } else {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > MAX_ORDINAL_DIGITS {
            return None;
        }
        let tail = &rest[digits.len()..];
        let after = tail
            .strip_prefix(". ")
            .or_else(|| tail.strip_prefix(") "))?;
        (Marker::Ordered(digits.parse().unwrap_or(1)), after)
    };
    let depth = indent / INDENT_PER_LEVEL;
    let content_indent = line.len() - after.len();
    if let Some(content) = after.strip_prefix("[ ] ") {
        return Some((
            depth,
            content_indent,
            Marker::Task(false),
            content.to_string(),
        ));
    }
    for done in ["[x] ", "[X] "] {
        if let Some(content) = after.strip_prefix(done) {
            return Some((
                depth,
                content_indent,
                Marker::Task(true),
                content.to_string(),
            ));
        }
    }
    Some((depth, content_indent, marker, after.to_string()))
}

fn quote_line(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn table_row(line: &str) -> bool {
    line.contains('|') && !line.trim().is_empty()
}

fn table_delimiter(line: &str) -> bool {
    let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
    !trimmed.is_empty()
        && trimmed.split('|').all(|cell| {
            let cell = cell.trim();
            !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':')
        })
}

/// A line that ends an open paragraph because it opens a different block.
fn interrupts_paragraph(line: &str) -> bool {
    fence_open(line).is_some()
        || atx_heading(line)
        || list_item(line).is_some()
        || quote_line(line)
        || thematic_break(line)
}

impl Scanner {
    /// Whether a fenced code block is currently open. A fence buffers nothing,
    /// so it survives a tool-call boundary and keeps styling its tail as code.
    pub(crate) fn in_fence(&self) -> bool {
        matches!(self.state, State::Fence(_))
    }

    /// A fence opener that re-opens the fence currently in progress, if any. A
    /// tool block landing mid-fence ends the answer's transcript record, so the
    /// record that continues the answer has to re-open the fence or replay would
    /// re-parse committed code rows as markdown.
    pub(crate) fn fence_continuation(&self) -> Option<String> {
        match &self.state {
            State::Fence(fence) => Some(fence.continuation_opener()),
            _ => None,
        }
    }

    fn next_marker(&mut self, depth: usize, marker: Marker) -> Marker {
        match marker {
            Marker::Ordered(explicit) => {
                if self.ordinals.len() <= depth {
                    self.ordinals.resize(depth + 1, 0);
                }
                self.ordinals[depth] = if self.ordinals[depth] == 0 {
                    explicit
                } else {
                    self.ordinals[depth] + 1
                };
                self.ordinals.truncate(depth + 1);
                Marker::Ordered(self.ordinals[depth])
            }
            other => {
                self.ordinals.truncate(depth);
                other
            }
        }
    }

    /// Feed one complete source line; returns whatever it completed.
    pub(crate) fn push_line(&mut self, line: &str) -> Vec<Chunk> {
        let mut out = Vec::new();
        self.consume(line, &mut out);
        out
    }

    fn consume(&mut self, line: &str, out: &mut Vec<Chunk>) {
        if let State::Fence(fence) = &self.state {
            if fence_close(line, fence) {
                self.state = State::Idle;
                out.push(Chunk::CodeEnd);
            } else {
                out.push(Chunk::Code {
                    text: dedent(line, fence.indent).to_string(),
                });
            }
            return;
        }

        let blank = line.trim().is_empty();

        match &mut self.state {
            State::MaybeTable(head) => {
                if table_delimiter(line) {
                    let head = std::mem::take(head);
                    self.state = State::Table(vec![head, line.to_string()]);
                    return;
                }
                // It was ordinary prose containing a pipe.
                let head = std::mem::take(head);
                self.state = State::Paragraph(vec![head]);
                self.consume(line, out);
                return;
            }
            State::Table(rows) => {
                if table_row(line) {
                    rows.push(line.to_string());
                    return;
                }
                out.extend(self.take_open());
                if blank {
                    return;
                }
            }
            State::Quote(lines) => {
                if quote_line(line) {
                    lines.push(line.to_string());
                    return;
                }
                out.extend(self.take_open());
                if blank {
                    return;
                }
            }
            State::List {
                content_indent,
                lines,
                fence,
                pending_blanks,
                ..
            } => {
                if let Some(open) = fence {
                    let closes = fence_close(line, open);
                    lines.push(dedent(line, *content_indent).to_string());
                    if closes {
                        *fence = None;
                    }
                    return;
                }
                // A blank line does not end an item on its own: a loose list, and
                // an indented block inside an item, both continue after one. Hold
                // it and let the next non-blank line decide.
                if blank {
                    *pending_blanks += 1;
                    return;
                }
                let indent = line.chars().take_while(|c| *c == ' ').count();
                if list_item(line).is_none() && indent >= *content_indent {
                    for _ in 0..std::mem::take(pending_blanks) {
                        lines.push(String::new());
                    }
                    let content = dedent(line, *content_indent);
                    if let Some((open, _)) = fence_open(content) {
                        *fence = Some(open);
                    }
                    lines.push(content.to_string());
                    return;
                }
                out.extend(self.take_open());
            }
            State::Paragraph(lines) => {
                if blank {
                    out.extend(self.take_open());
                    return;
                }
                if setext_underline(line) {
                    lines.push(line.to_string());
                    out.extend(self.take_open());
                    return;
                }
                if !interrupts_paragraph(line) {
                    lines.push(line.to_string());
                    return;
                }
                out.extend(self.take_open());
            }
            State::Idle | State::Fence(_) => {}
        }

        if blank {
            return;
        }
        if let Some((fence, lang)) = fence_open(line) {
            self.ordinals.clear();
            self.state = State::Fence(fence);
            out.push(Chunk::CodeStart { lang });
            return;
        }
        if atx_heading(line) || thematic_break(line) {
            self.ordinals.clear();
            out.push(Chunk::Block {
                source: line.to_string(),
                list: None,
            });
            return;
        }
        if let Some((depth, content_indent, marker, content)) = list_item(line) {
            let marker = self.next_marker(depth, marker);
            let fence = fence_open(&content).map(|(open, _)| open);
            self.state = State::List {
                depth,
                marker,
                content_indent,
                lines: vec![content],
                fence,
                pending_blanks: 0,
            };
            return;
        }
        if quote_line(line) {
            self.ordinals.clear();
            self.state = State::Quote(vec![line.to_string()]);
            return;
        }
        if table_row(line) {
            self.ordinals.clear();
            self.state = State::MaybeTable(line.to_string());
            return;
        }
        self.ordinals.clear();
        self.state = State::Paragraph(vec![line.to_string()]);
    }

    /// Emit whatever buffered block is open, leaving an open fence alone. Used
    /// where a tool block must commit *below* the answer text so far: the
    /// ordering invariant [`crate::flush_answer_boundary`] protects.
    pub(crate) fn flush(&mut self) -> Vec<Chunk> {
        if self.in_fence() {
            return Vec::new();
        }
        self.take_open()
    }

    /// End the answer: emit any open block and close an open fence.
    pub(crate) fn finish(&mut self) -> Vec<Chunk> {
        self.ordinals.clear();
        self.take_open()
    }

    fn take_open(&mut self) -> Vec<Chunk> {
        match std::mem::take(&mut self.state) {
            State::Idle => Vec::new(),
            State::Fence(_) => vec![Chunk::CodeEnd],
            State::Paragraph(lines) | State::Quote(lines) | State::Table(lines) => {
                vec![Chunk::Block {
                    source: lines.join("\n"),
                    list: None,
                }]
            }
            State::MaybeTable(head) => vec![Chunk::Block {
                source: head,
                list: None,
            }],
            State::List {
                depth,
                marker,
                lines,
                ..
            } => vec![Chunk::Block {
                source: lines.join("\n"),
                list: Some(ListContext { depth, marker }),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(src: &str) -> Vec<Chunk> {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        for line in src.lines() {
            out.extend(scanner.push_line(line));
        }
        out.extend(scanner.finish());
        out
    }

    /// Feed the same source split into `size`-char pieces, re-deriving lines the
    /// way the event loop does, so chunk-boundary bugs surface.
    fn scan_chunked(src: &str, size: usize) -> Vec<Chunk> {
        let mut scanner = Scanner::default();
        let mut out = Vec::new();
        let mut buf = String::new();
        let chars: Vec<char> = src.chars().collect();
        for piece in chars.chunks(size) {
            buf.extend(piece);
            while let Some(idx) = buf.find('\n') {
                let line: String = buf.drain(..=idx).collect();
                out.extend(scanner.push_line(line.trim_end_matches('\n')));
            }
        }
        if !buf.is_empty() {
            out.extend(scanner.push_line(&buf));
        }
        out.extend(scanner.finish());
        out
    }

    fn sources(chunks: &[Chunk]) -> Vec<String> {
        chunks
            .iter()
            .map(|c| match c {
                Chunk::Block { source, .. } => source.clone(),
                Chunk::Code { text } => text.clone(),
                Chunk::CodeStart { .. } => "<start>".to_string(),
                Chunk::CodeEnd => "<end>".to_string(),
            })
            .collect()
    }

    fn markers(chunks: &[Chunk]) -> Vec<Marker> {
        chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Block {
                    list: Some(ctx), ..
                } => Some(ctx.marker.clone()),
                _ => None,
            })
            .collect()
    }

    const DOC: &str = "# Title\n\nSome **bold** prose.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n\nTail.\n";

    #[test]
    fn the_result_is_independent_of_chunk_size() {
        let want = scan_all(DOC);
        for size in 1..48 {
            assert_eq!(scan_chunked(DOC, size), want, "chunk size {size} diverged");
        }
    }

    #[test]
    fn a_fence_body_streams_line_by_line() {
        let mut scanner = Scanner::default();
        assert_eq!(
            scanner.push_line("```rust"),
            vec![Chunk::CodeStart {
                lang: Some("rust".into())
            }]
        );
        assert_eq!(
            scanner.push_line("let x = 1;"),
            vec![Chunk::Code {
                text: "let x = 1;".into()
            }]
        );
        assert_eq!(
            scanner.push_line("let y = 2;"),
            vec![Chunk::Code {
                text: "let y = 2;".into()
            }]
        );
        assert_eq!(scanner.push_line("```"), vec![Chunk::CodeEnd]);
    }

    #[test]
    fn a_fence_swallows_lines_that_look_like_other_blocks() {
        let chunks = scan_all("```md\n# not a heading\n- not a list\n| not | a table |\n```\n");
        assert_eq!(
            chunks
                .iter()
                .filter(|c| matches!(c, Chunk::Code { .. }))
                .count(),
            3
        );
        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, Chunk::Block { list: Some(_), .. }))
        );
    }

    #[test]
    fn an_unterminated_fence_is_closed_by_finish() {
        let mut scanner = Scanner::default();
        scanner.push_line("```python");
        scanner.push_line("print(1)");
        assert!(scanner.in_fence());
        assert_eq!(scanner.finish(), vec![Chunk::CodeEnd]);
    }

    #[test]
    fn a_tool_boundary_leaves_an_open_fence_open() {
        let mut scanner = Scanner::default();
        scanner.push_line("```rust");
        scanner.push_line("fn a() {}");
        assert!(scanner.flush().is_empty(), "a fence buffers nothing");
        assert!(scanner.in_fence(), "the fence must survive the boundary");
        assert_eq!(
            scanner.push_line("fn b() {}"),
            vec![Chunk::Code {
                text: "fn b() {}".into()
            }]
        );
    }

    #[test]
    fn a_tool_boundary_mid_paragraph_flushes_first() {
        let mut scanner = Scanner::default();
        assert!(scanner.push_line("let me check").is_empty());
        assert_eq!(
            scanner.flush(),
            vec![Chunk::Block {
                source: "let me check".into(),
                list: None
            }]
        );
        assert!(scanner.push_line("found it").is_empty());
        assert_eq!(sources(&scanner.finish()), vec!["found it"]);
    }

    #[test]
    fn a_hard_wrapped_paragraph_stays_one_block() {
        assert_eq!(
            sources(&scan_all("this was\nhard wrapped\n\nnext\n")),
            vec!["this was\nhard wrapped", "next"]
        );
    }

    #[test]
    fn a_new_block_interrupts_an_open_paragraph() {
        assert_eq!(
            sources(&scan_all("intro\n# Heading\nmore\n")),
            vec!["intro", "# Heading", "more"]
        );
    }

    #[test]
    fn list_items_flush_one_at_a_time() {
        let mut scanner = Scanner::default();
        assert!(scanner.push_line("- first").is_empty());
        assert_eq!(
            sources(&scanner.push_line("- second")),
            vec!["first"],
            "the second marker flushes the first item"
        );
    }

    #[test]
    fn ordered_numbering_survives_incremental_flushing() {
        assert_eq!(
            markers(&scan_all("1. a\n1. b\n1. c\n")),
            vec![Marker::Ordered(1), Marker::Ordered(2), Marker::Ordered(3)]
        );
    }

    #[test]
    fn a_loose_list_keeps_numbering_across_blank_lines() {
        // All-ones source: an explicitly numbered source would pass even if the
        // running ordinal were reset by the blank line.
        assert_eq!(
            markers(&scan_all("1. a\n\n1. b\n\n1. c\n")),
            vec![Marker::Ordered(1), Marker::Ordered(2), Marker::Ordered(3)]
        );
        assert_eq!(
            markers(&scan_all("1. a\n\n2. b\n\n3. c\n")),
            vec![Marker::Ordered(1), Marker::Ordered(2), Marker::Ordered(3)]
        );
    }

    #[test]
    fn a_fence_inside_a_list_item_absorbs_blank_and_list_shaped_lines() {
        // Without fence tracking in the item state, the blank line ends the item,
        // the item's closing ``` then opens a NEW fence, and the following prose
        // is swallowed as code.
        let src = "- run this:\n\n  ```sh\n  ls -la\n\n  - not a list\n  ```\n\nafter the list\n";
        let chunks = scan_all(src);
        let items: Vec<&Chunk> = chunks
            .iter()
            .filter(|c| matches!(c, Chunk::Block { list: Some(_), .. }))
            .collect();
        assert_eq!(
            items.len(),
            1,
            "the fence must not split the item: {chunks:?}"
        );
        let Some(Chunk::Block { source, .. }) = items.first().copied() else {
            unreachable!()
        };
        assert!(source.contains("ls -la"), "{source:?}");
        assert!(source.contains("- not a list"), "{source:?}");
        assert!(
            sources(&chunks).iter().any(|s| s == "after the list"),
            "trailing prose was swallowed: {chunks:?}"
        );
    }

    #[test]
    fn an_item_continuation_keeps_its_own_indentation() {
        // `trim_start` on continuation lines flattened every nested level. A line
        // at the content indent correctly dedents to column 0; anything deeper
        // must keep the difference.
        let src = "- outer text\n  at content indent\n      four deeper\n";
        let items: Vec<String> = scan_all(src)
            .iter()
            .filter_map(|c| match c {
                Chunk::Block {
                    source,
                    list: Some(_),
                } => Some(source.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec!["outer text\nat content indent\n    four deeper".to_string()],
            "relative indentation was lost"
        );
    }

    #[test]
    fn a_blank_line_inside_an_item_does_not_end_it() {
        let src = "- first para\n\n  second para of the same item\n\nplain prose\n";
        let chunks = scan_all(src);
        let items: Vec<String> = chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Block {
                    source,
                    list: Some(_),
                } => Some(source.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec!["first para\n\nsecond para of the same item".to_string()],
            "a loose item lost its second paragraph: {chunks:?}"
        );
        assert!(
            sources(&chunks).iter().any(|s| s == "plain prose"),
            "the under-indented line must end the item: {chunks:?}"
        );
    }

    #[test]
    fn trailing_blanks_after_an_item_are_not_kept() {
        let mut scanner = Scanner::default();
        scanner.push_line("- only item");
        scanner.push_line("");
        scanner.push_line("");
        assert_eq!(sources(&scanner.finish()), vec!["only item"]);
    }

    #[test]
    fn an_indented_fence_strips_its_own_indent_from_the_body() {
        let chunks = scan_all("  ```rust\n  fn main() {}\n      nested();\n  ```\n");
        let code: Vec<String> = chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Code { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            code,
            vec!["fn main() {}".to_string(), "    nested();".to_string()],
            "the opener's indent must be stripped, deeper indent kept"
        );
    }

    #[test]
    fn fence_continuation_reopens_without_repeating_the_language() {
        let mut scanner = Scanner::default();
        assert_eq!(scanner.fence_continuation(), None);
        scanner.push_line("  ```rust");
        assert_eq!(
            scanner.fence_continuation(),
            Some("  ```".to_string()),
            "the language label already rendered once; repeating it draws it twice"
        );
        scanner.push_line("x");
        assert_eq!(scanner.fence_continuation(), Some("  ```".to_string()));
        scanner.push_line("  ```");
        assert_eq!(scanner.fence_continuation(), None);
    }

    #[test]
    fn a_longer_fence_run_is_reproduced_by_its_continuation() {
        let mut scanner = Scanner::default();
        scanner.push_line("~~~~py");
        assert_eq!(scanner.fence_continuation(), Some("~~~~".to_string()));
    }

    #[test]
    fn an_explicit_start_ordinal_is_honoured() {
        assert_eq!(
            markers(&scan_all("3. c\n4. d\n")),
            vec![Marker::Ordered(3), Marker::Ordered(4)]
        );
    }

    #[test]
    fn task_markers_are_recognised() {
        assert_eq!(
            markers(&scan_all("- [ ] todo\n- [x] done\n- [X] also\n")),
            vec![Marker::Task(false), Marker::Task(true), Marker::Task(true)]
        );
    }

    #[test]
    fn nested_items_carry_their_depth() {
        let depths: Vec<usize> = scan_all("- outer\n  - inner\n- outer two\n")
            .iter()
            .filter_map(|c| match c {
                Chunk::Block {
                    list: Some(ctx), ..
                } => Some(ctx.depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, vec![0, 1, 0]);
    }

    #[test]
    fn an_item_absorbs_its_indented_continuation() {
        assert_eq!(
            sources(&scan_all("- first line\n  continued here\n- second\n")),
            vec!["first line\ncontinued here", "second"]
        );
    }

    #[test]
    fn a_table_buffers_until_it_ends() {
        let mut scanner = Scanner::default();
        assert!(scanner.push_line("| a | b |").is_empty());
        assert!(scanner.push_line("|---|---|").is_empty());
        assert!(scanner.push_line("| 1 | 2 |").is_empty());
        assert_eq!(
            sources(&scanner.push_line("")),
            vec!["| a | b |\n|---|---|\n| 1 | 2 |"]
        );
    }

    #[test]
    fn a_truncated_table_still_emits() {
        let mut scanner = Scanner::default();
        scanner.push_line("| a | b |");
        scanner.push_line("|---|---|");
        assert_eq!(sources(&scanner.finish()), vec!["| a | b |\n|---|---|"]);
    }

    #[test]
    fn prose_containing_a_pipe_is_not_a_table() {
        assert_eq!(
            sources(&scan_all("use a | b for pipes\nand more text\n")),
            vec!["use a | b for pipes\nand more text"]
        );
    }

    #[test]
    fn a_pipe_line_then_a_heading_is_prose_then_heading() {
        assert_eq!(sources(&scan_all("a | b\n# H\n")), vec!["a | b", "# H"]);
    }

    #[test]
    fn setext_underlines_stay_with_their_paragraph() {
        assert_eq!(
            sources(&scan_all("Title\n===\n\nbody\n")),
            vec!["Title\n===", "body"]
        );
        assert_eq!(
            sources(&scan_all("Title\n---\n\nbody\n")),
            vec!["Title\n---", "body"]
        );
    }

    #[test]
    fn a_rule_with_no_paragraph_above_is_a_rule() {
        assert_eq!(sources(&scan_all("a\n\n---\n\nb\n")), vec!["a", "---", "b"]);
    }

    #[test]
    fn a_quote_absorbs_its_own_lines() {
        assert_eq!(
            sources(&scan_all("> one\n> two\n\nafter\n")),
            vec!["> one\n> two", "after"]
        );
    }

    #[test]
    fn no_source_line_is_ever_lost() {
        let src = "# H\ntext one\n\n- li one\n  cont\n- li two\n\n> q\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```\ncode here\n```\n\n---\n\nlast\n";
        let emitted = sources(&scan_all(src)).join("\n");
        for line in src.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("```") {
                continue;
            }
            let needle = trimmed
                .trim_start_matches("- ")
                .trim_start_matches("> ")
                .trim();
            assert!(
                emitted.contains(needle),
                "line {line:?} (needle {needle:?}) vanished from:\n{emitted}"
            );
        }
    }

    #[test]
    fn a_blank_only_answer_emits_nothing() {
        assert!(scan_all("\n\n\n").is_empty());
        assert!(scan_all("").is_empty());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut scanner = Scanner::default();
        scanner.push_line("text");
        assert_eq!(scanner.finish().len(), 1);
        assert!(scanner.finish().is_empty());
    }
}
