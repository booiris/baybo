//! Width-aware wrapping for pre-laid-out scrollback rows.
//!
//! Answer rows are committed through `commit_lines_no_wrap`, so every row this
//! module returns becomes exactly one terminal row. Two things follow. Ratatui's
//! own `WordWrapper` is not usable here: it segments only on ASCII whitespace
//! (`ratatui-widgets` `reflow.rs`), so an unspaced Han sentence breaks wherever
//! the column budget happened to run out. And a row wider than the area would be
//! truncated with no marker, so [`wrap`] must never emit one.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display columns a wide (East Asian Wide / Fullwidth) grapheme occupies.
const WIDE_COLUMNS: usize = 2;

/// Glyphs that may not open a row: CJK closing brackets and trailing
/// punctuation. Breaking before one of these orphans it at the line head,
/// which is the most visible CJK typesetting error (kinsoku shori).
const NO_BREAK_BEFORE: &[char] = &[
    '，', '。', '、', '；', '：', '？', '！', '）', '】', '』', '」', '》', '〉', '〕', '｝', '”',
    '’', '…', '‥', '·', '・', '～', '％', ')', ']', '}', ',', '.', ';', ':', '?', '!', '%',
];

/// Glyphs that may not end a row: CJK opening brackets. Breaking after one
/// strands it at the line tail.
const NO_BREAK_AFTER: &[char] = &[
    '（', '【', '『', '「', '《', '〈', '〔', '｛', '“', '‘', '(', '[', '{',
];

/// Leading spans a wrapped block puts on its first row versus its
/// continuations. This is what gives a list item its hanging indent and a
/// blockquote its gutter on every row.
pub(crate) struct Gutter {
    first: Vec<Span<'static>>,
    rest: Vec<Span<'static>>,
}

impl Gutter {
    pub(crate) fn none() -> Self {
        Self {
            first: Vec::new(),
            rest: Vec::new(),
        }
    }

    /// The same leading spans on every row.
    pub(crate) fn uniform(spans: Vec<Span<'static>>) -> Self {
        Self {
            first: spans.clone(),
            rest: spans,
        }
    }

    /// A marker on the first row, a matching blank (or gutter) on the rest.
    pub(crate) fn hanging(first: Vec<Span<'static>>, rest: Vec<Span<'static>>) -> Self {
        Self { first, rest }
    }

    fn lead(&self, first_row: bool) -> &[Span<'static>] {
        if first_row { &self.first } else { &self.rest }
    }

    pub(crate) fn first_spans(&self) -> &[Span<'static>] {
        &self.first
    }

    pub(crate) fn rest_spans(&self) -> &[Span<'static>] {
        &self.rest
    }

    fn max_width(&self) -> usize {
        spans_width(&self.first).max(spans_width(&self.rest))
    }
}

/// Total display width of `spans`.
pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.as_ref().width()).sum()
}

/// Display width of one grapheme cluster. `UnicodeWidthStr` measures the whole
/// cluster, which is what a terminal advances the cursor by — summing per-char
/// widths would over-charge a ZWJ or skin-tone emoji.
fn grapheme_width(g: &str) -> usize {
    g.width()
}

fn first_char(g: &str) -> Option<char> {
    g.chars().next()
}

fn is_wide(g: &str) -> bool {
    grapheme_width(g) >= WIDE_COLUMNS
}

fn no_break_before(g: &str) -> bool {
    first_char(g).is_some_and(|c| NO_BREAK_BEFORE.contains(&c))
}

fn no_break_after(g: &str) -> bool {
    first_char(g).is_some_and(|c| NO_BREAK_AFTER.contains(&c))
}

/// One grapheme cluster carrying the style of the span it came from.
struct Atom<'a> {
    text: &'a str,
    width: usize,
    style: Style,
}

fn atomise<'a>(spans: &'a [Span<'a>]) -> Vec<Atom<'a>> {
    let mut atoms = Vec::new();
    for span in spans {
        for g in span.content.as_ref().graphemes(true) {
            atoms.push(Atom {
                text: g,
                width: grapheme_width(g),
                style: span.style,
            });
        }
    }
    atoms
}

/// Wrap `spans` into rows at most `width` columns wide, prefixed by `gutter`.
///
/// Never drops an atom and never returns an over-wide row. When the gutter
/// itself would leave no room for content it is dropped for that block —
/// degrading the indent is always better than degrading the text.
pub(crate) fn wrap(spans: &[Span<'static>], width: usize, gutter: &Gutter) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    // Keep the gutter only while it still leaves room for the widest single
    // glyph. Below that, degrade the indent rather than the text: a row wider
    // than the area would be truncated by the terminal with no marker.
    let bare = Gutter::none();
    let (gutter, gutter_width) = match gutter.max_width() {
        used if width.saturating_sub(used) >= WIDE_COLUMNS => (gutter, used),
        _ => (&bare, 0),
    };
    let budget = (width - gutter_width).max(1);

    let atoms = atomise(spans);
    if atoms.is_empty() {
        let lead = gutter.lead(true);
        return if lead.is_empty() {
            vec![Line::default()]
        } else {
            vec![Line::from(owned(lead))]
        };
    }

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < atoms.len() {
        let end = row_end(&atoms, cursor, budget);
        let mut spans_out = owned(gutter.lead(rows.is_empty()));
        push_atoms(&mut spans_out, &atoms[cursor..end]);
        trim_trailing_spaces(&mut spans_out);
        rows.push(Line::from(spans_out));
        cursor = end;
        while cursor < atoms.len() && atoms[cursor].text == " " {
            cursor += 1;
        }
    }
    rows
}

/// Index one past the last atom that belongs on this row.
fn row_end(atoms: &[Atom<'_>], start: usize, budget: usize) -> usize {
    let mut used = 0usize;
    let mut end = start;
    while end < atoms.len() {
        let w = atoms[end].width;
        if used + w > budget {
            break;
        }
        used += w;
        end += 1;
    }
    if end == start {
        // A single atom wider than the whole budget still has to make progress.
        return start + 1;
    }
    if end >= atoms.len() {
        return end;
    }
    break_point(atoms, start, end).unwrap_or(end)
}

/// The last legal break in `start..=end`, or `None` to break mid-token.
fn break_point(atoms: &[Atom<'_>], start: usize, end: usize) -> Option<usize> {
    let mut i = end;
    while i > start {
        if atoms[i - 1].text == " " {
            return Some(i);
        }
        i -= 1;
    }
    let mut i = end;
    while i > start + 1 {
        let prev = atoms[i - 1].text;
        let next = atoms[i].text;
        if (is_wide(prev) || is_wide(next)) && !no_break_before(next) && !no_break_after(prev) {
            return Some(i);
        }
        i -= 1;
    }
    None
}

fn push_atoms(out: &mut Vec<Span<'static>>, atoms: &[Atom<'_>]) {
    let base = out.len();
    for atom in atoms {
        let extend = out.len() > base && out.last().is_some_and(|s| s.style == atom.style);
        match out.last_mut() {
            Some(last) if extend => last.content.to_mut().push_str(atom.text),
            _ => out.push(Span::styled(atom.text.to_string(), atom.style)),
        }
    }
}

/// A row broken on whitespace keeps the space it broke at; drop it so a styled
/// background does not extend past the text.
fn trim_trailing_spaces(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end_matches(' ');
        if trimmed.len() == last.content.len() {
            return;
        }
        if trimmed.is_empty() {
            spans.pop();
        } else {
            let keep = trimmed.to_string();
            last.content = keep.into();
            return;
        }
    }
}

fn owned(spans: &[Span<'static>]) -> Vec<Span<'static>> {
    spans.to_vec()
}

/// Clip `text` to `width` columns, appending `…` when anything was removed.
/// Used where content cannot reflow (table cells, the code-fence language tag).
pub(crate) fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let ellipsis_width = 1;
    let budget = width.saturating_sub(ellipsis_width);
    let mut used = 0usize;
    let mut out = String::new();
    for g in text.graphemes(true) {
        let w = grapheme_width(g);
        if used + w > budget {
            break;
        }
        used += w;
        out.push_str(g);
    }
    out.push('…');
    out
}

/// Right-pad `text` with spaces to exactly `width` columns, clipping first if
/// it is too wide. Table cells and the user bar both need exact column counts.
pub(crate) fn pad(text: &str, width: usize) -> String {
    let clipped = clip(text, width);
    let w = clipped.width();
    if w >= width {
        return clipped;
    }
    format!("{clipped}{}", " ".repeat(width - w))
}

/// Strip characters a terminal would act on rather than print. A stray ESC
/// desynchronises every following row, and `UnicodeWidthStr` charges `"\n"` one
/// column while `UnicodeWidthChar` charges it none — so control bytes break
/// width accounting as well as the screen.
///
/// Only control characters go. Gating on non-zero width instead would delete
/// combining marks, ZWJ, and variation selectors — silently rewriting a
/// decomposed `é` to `e` and splitting emoji sequences.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<Span<'static>> {
        vec![Span::raw(text.to_string())]
    }

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn row_width(line: &Line<'_>) -> usize {
        row_text(line).width()
    }

    const LATIN: &str = "the quick brown fox jumps over the lazy dog again and again";
    const HAN: &str = "这是一个很长的中文句子，用来测试终端里的换行逻辑是否正确处理宽字符。";
    const MIXED: &str = "中英mixed混排 text 测试wrapping行为 with ascii words";

    #[test]
    fn no_row_ever_exceeds_the_width() {
        for src in [
            LATIN,
            HAN,
            MIXED,
            "supercalifragilisticexpialidocious",
            "a",
            "",
        ] {
            for width in WIDE_COLUMNS..60 {
                for gutter in [
                    Gutter::none(),
                    Gutter::uniform(plain("│ ")),
                    Gutter::hanging(plain("• "), plain("  ")),
                ] {
                    let rows = wrap(&plain(src), width, &gutter);
                    for row in &rows {
                        assert!(
                            row_width(row) <= width,
                            "src {src:?} width {width} row {:?} is {} cols",
                            row_text(row),
                            row_width(row)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_glyph_too_wide_for_the_width_is_kept_not_dropped() {
        let rows = wrap(&plain("中"), 1, &Gutter::none());
        let joined: String = rows.iter().map(row_text).collect();
        assert_eq!(joined, "中", "a wide glyph must survive a 1-column width");
    }

    #[test]
    fn no_atom_is_ever_dropped() {
        for src in [LATIN, HAN, MIXED, "one", "词"] {
            for width in 1..60 {
                let rows = wrap(&plain(src), width, &Gutter::none());
                let got: String = rows
                    .iter()
                    .map(|r| row_text(r))
                    .collect::<String>()
                    .chars()
                    .filter(|c| *c != ' ')
                    .collect();
                let want: String = src.chars().filter(|c| *c != ' ').collect();
                assert_eq!(want, got, "src {src:?} at width {width}");
            }
        }
    }

    #[test]
    fn han_wraps_without_any_spaces_to_break_on() {
        let rows = wrap(
            &plain("中文没有空格所以必须能在字之间断行"),
            10,
            &Gutter::none(),
        );
        assert!(rows.len() > 1, "unspaced Han did not wrap: {rows:?}");
    }

    #[test]
    fn closing_punctuation_never_opens_a_row() {
        for width in 6..40 {
            let rows = wrap(&plain(HAN), width, &Gutter::none());
            for row in rows.iter().skip(1) {
                let text = row_text(row);
                if let Some(c) = text.chars().next() {
                    assert!(
                        !NO_BREAK_BEFORE.contains(&c),
                        "width {width}: row opens with {c:?}: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn opening_punctuation_never_ends_a_row() {
        let src = "参数（这里是一段很长的说明文字）后面还有内容";
        for width in 6..40 {
            let rows = wrap(&plain(src), width, &Gutter::none());
            for row in &rows {
                let text = row_text(row);
                if let Some(c) = text.chars().last() {
                    assert!(
                        !NO_BREAK_AFTER.contains(&c),
                        "width {width}: row ends with {c:?}: {text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_hanging_gutter_indents_continuations_only() {
        let rows = wrap(
            &plain(LATIN),
            20,
            &Gutter::hanging(plain("• "), plain("  ")),
        );
        assert!(rows.len() > 2);
        assert!(row_text(&rows[0]).starts_with("• "));
        for row in rows.iter().skip(1) {
            assert!(row_text(row).starts_with("  "), "{:?}", row_text(row));
        }
    }

    #[test]
    fn a_gutter_wider_than_the_terminal_is_dropped_not_the_text() {
        let rows = wrap(&plain("文字"), 3, &Gutter::uniform(plain("        ")));
        let joined: String = rows.iter().map(|r| row_text(r)).collect();
        assert!(
            joined.contains('文'),
            "text was sacrificed to the gutter: {joined:?}"
        );
        for row in &rows {
            assert!(row_width(row) <= 3);
        }
    }

    #[test]
    fn styles_survive_a_break_and_do_not_bleed() {
        let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
        let spans = vec![
            Span::raw("plain ".to_string()),
            Span::styled("emphasised words here".to_string(), bold),
            Span::raw(" tail".to_string()),
        ];
        let rows = wrap(&spans, 12, &Gutter::none());
        let mut styled = String::new();
        let mut unstyled = String::new();
        for row in &rows {
            for span in &row.spans {
                if span.style == bold {
                    styled.push_str(span.content.as_ref());
                } else {
                    unstyled.push_str(span.content.as_ref());
                }
            }
        }
        assert!(styled.replace(' ', "").contains("emphasised"), "{styled:?}");
        assert!(
            !styled.contains("plain"),
            "style bled backwards: {styled:?}"
        );
        assert!(!styled.contains("tail"), "style bled forwards: {styled:?}");
        assert!(unstyled.contains("plain"), "{unstyled:?}");
    }

    #[test]
    fn a_grapheme_cluster_is_never_split() {
        let src = "ok 👍🏽 done é\u{0301}x";
        for width in 2..20 {
            let rows = wrap(&plain(src), width, &Gutter::none());
            for row in &rows {
                let text = row_text(row);
                assert!(
                    !text.starts_with('\u{0301}') && !text.starts_with('\u{1F3FD}'),
                    "width {width}: row starts mid-cluster: {text:?}"
                );
            }
        }
    }

    #[test]
    fn an_empty_input_still_yields_one_row() {
        assert_eq!(wrap(&plain(""), 10, &Gutter::none()).len(), 1);
        assert_eq!(wrap(&[], 10, &Gutter::none()).len(), 1);
    }

    #[test]
    fn clip_and_pad_land_on_exact_widths() {
        assert_eq!(pad("ab", 5).width(), 5);
        assert_eq!(pad("中文", 5).width(), 5);
        assert_eq!(pad("中文字", 5).width(), 5);
        assert!(clip("中文字", 4).width() <= 4);
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("ab", 4), "ab");
    }

    #[test]
    fn sanitize_drops_terminal_control_bytes() {
        assert_eq!(sanitize("a\u{1b}[31mb"), "a[31mb");
        assert_eq!(sanitize("a\u{0}b"), "ab");
        assert_eq!(sanitize("a\rb"), "ab");
        assert_eq!(sanitize("a\u{7f}b"), "ab");
        assert_eq!(sanitize("a\tb"), "ab");
        assert_eq!(sanitize("keep\nnewline"), "keep\nnewline");
        assert_eq!(sanitize("中文 ok"), "中文 ok");
    }

    #[test]
    fn sanitize_keeps_zero_width_characters_that_carry_meaning() {
        // A width gate here would rewrite decomposed text and split emoji.
        assert_eq!(sanitize("e\u{301}clair"), "e\u{301}clair");
        assert_eq!(
            sanitize("\u{1f468}\u{200d}\u{1f469}"),
            "\u{1f468}\u{200d}\u{1f469}"
        );
        assert_eq!(sanitize("\u{2764}\u{fe0f}"), "\u{2764}\u{fe0f}");
    }
}
