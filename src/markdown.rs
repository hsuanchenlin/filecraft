//! Pure Markdown and plain-text rendering for the built-in reader.
//!
//! Text goes in, styled display rows come out. Everything here is
//! terminal-free: line classification, inline emphasis, width-aware
//! wrapping (a wide CJK character owns two columns and is never split),
//! search, and match highlighting are all testable without a TTY.
//! [`crate::ui`] only turns a [`Span`] into a ratatui span.
//!
//! The markers stay visible on purpose: `#`, list bullets, quote bars and
//! the backticks around inline code are textual duals of the styling, so
//! a `NO_COLOR` or ASCII screen still shows the structure.

use std::path::Path;

use crate::bearings::{char_width, display_width, pad_to_width_with, Glyphs};

/// Columns a tab expands to, before any width arithmetic runs.
const TAB_STOP: usize = 4;
/// Deepest list nesting that still buys indentation.
const MAX_LIST_DEPTH: usize = 6;
/// Deepest blockquote nesting that still buys a bar.
const MAX_QUOTE_DEPTH: usize = 4;

/// Inline styling of one run of characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Plain,
    Strong,
    Emph,
    Code,
    /// The leading marker of a line (`##`, a bullet, a quote bar).
    Marker,
    /// Rules, fences, and reader notices.
    Meta,
    /// Part of the active search query.
    Match,
}

/// A run of characters that share one [`Ink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub ink: Ink,
}

impl Span {
    pub fn new(text: impl Into<String>, ink: Ink) -> Self {
        Span {
            text: text.into(),
            ink,
        }
    }
}

/// What a source line is, which is what the reader styles it by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Body,
    Heading(u8),
    Bullet,
    Quote,
    /// A line inside a fenced code block.
    Code,
    /// The ``` line itself, drawn as a labelled rule.
    Fence,
    /// A thematic break (`---`).
    Rule,
    /// A reader notice such as the truncation footer.
    Meta,
}

/// The leading decoration of a line, kept abstract so a document parsed
/// once still draws correctly if the character set changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    None,
    /// `##` and its space.
    Hash(u8),
    /// An unordered list item at `depth`.
    Bullet {
        depth: usize,
    },
    /// An ordered list item, keeping the file's own `1.` or `2)`.
    Ordered {
        depth: usize,
        label: String,
    },
    /// `depth` blockquote bars.
    Quote {
        depth: usize,
    },
    /// A line inside a fenced code block.
    Code,
}

impl Marker {
    /// The marker as drawn, in the character set in force.
    pub fn render(&self, glyphs: &Glyphs) -> String {
        match self {
            Marker::None => String::new(),
            Marker::Hash(level) => format!("{} ", "#".repeat(*level as usize)),
            Marker::Bullet { depth } => format!("{}{} ", "  ".repeat(*depth), glyphs.bullet),
            Marker::Ordered { depth, label } => format!("{}{label} ", "  ".repeat(*depth)),
            Marker::Quote { depth } => format!("{} ", glyphs.quote_bar).repeat(*depth),
            Marker::Code => "  ".to_string(),
        }
    }
}

/// One source line, classified and split into styled runs.
///
/// Wrapped continuation rows are indented to exactly the marker's width,
/// so a wrapped bullet stays under its own text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLine {
    pub kind: Kind,
    pub marker: Marker,
    pub spans: Vec<Span>,
}

impl DocLine {
    fn new(kind: Kind, marker: Marker, spans: Vec<Span>) -> Self {
        DocLine {
            kind,
            marker,
            spans,
        }
    }

    /// A plain body line.
    ///
    /// The text is [`clean`]ed here rather than at the call site, so
    /// every line the reader holds - parsed from a file or written by the
    /// app itself - satisfies the same invariant: no tabs, no control
    /// characters, and a display width the screen will actually spend.
    pub fn body(text: impl Into<String>) -> Self {
        DocLine::new(
            Kind::Body,
            Marker::None,
            vec![Span::new(clean(&text.into()), Ink::Plain)],
        )
    }

    /// A reader notice (`(empty file)`, the truncation footer).
    pub fn meta(text: impl Into<String>) -> Self {
        DocLine::new(
            Kind::Meta,
            Marker::None,
            vec![Span::new(clean(&text.into()), Ink::Meta)],
        )
    }

    /// The line's own text, without the decoration the reader adds. This
    /// is what search matches, so a query is about the file rather than
    /// about the bullets drawn beside it.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// One row as it is drawn: a slice of a [`DocLine`] that fits the width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Index of the source line this row came from, so the position
    /// footer can report a real line number even mid-wrap.
    pub line: usize,
    pub kind: Kind,
    pub spans: Vec<Span>,
}

impl Row {
    /// The row as plain text - the exact characters the screen shows.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// True for the extensions the reader renders as Markdown.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Tabs to spaces, control characters to `U+FFFD`: no file can inject an
/// escape sequence into the reader, and every width below is in real
/// cells.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut column = 0usize;
    for c in line.chars() {
        match c {
            '\t' => {
                let stop = TAB_STOP - (column % TAB_STOP);
                out.extend(std::iter::repeat_n(' ', stop));
                column += stop;
            }
            c if c.is_control() => {
                out.push('\u{FFFD}');
                column += 1;
            }
            c => {
                out.push(c);
                column += char_width(c);
            }
        }
    }
    out
}

/// Render `text` as plain text: one body line per source line, no
/// Markdown structure invented that the file does not have.
pub fn parse_plain(text: &str) -> Vec<DocLine> {
    text.lines().map(DocLine::body).collect()
}

/// Render `text` as Markdown in the given character set.
///
/// Line-based on purpose: a reader must show what is in the file, so an
/// unterminated fence or an exotic construct degrades to body text
/// instead of swallowing the rest of the document.
pub fn parse_markdown(text: &str) -> Vec<DocLine> {
    let mut out = Vec::new();
    let mut fence: Option<char> = None;
    for raw in text.lines() {
        let line = clean(raw);
        if let Some(open) = fence {
            if is_closing_fence(&line, open) {
                fence = None;
                out.push(DocLine::new(Kind::Fence, Marker::None, Vec::new()));
                continue;
            }
            out.push(DocLine::new(
                Kind::Code,
                Marker::Code,
                vec![Span::new(line, Ink::Code)],
            ));
            continue;
        }
        if let Some(open) = fence_char(&line) {
            fence = Some(open);
            let label = line.trim().trim_matches(open).trim().to_string();
            let spans = if label.is_empty() {
                Vec::new()
            } else {
                vec![Span::new(label, Ink::Meta)]
            };
            out.push(DocLine::new(Kind::Fence, Marker::None, spans));
            continue;
        }
        out.push(parse_block_line(&line));
    }
    out
}

/// The fence character of a ``` / ~~~ line, if this line opens or closes
/// a fenced block.
fn fence_char(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    ['`', '~'].into_iter().find(|&mark| {
        trimmed.starts_with(mark) && trimmed.chars().take_while(|c| *c == mark).count() >= 3
    })
}

fn is_closing_fence(line: &str, mark: char) -> bool {
    let trimmed = line.trim_start();
    let run = trimmed.chars().take_while(|c| *c == mark).count();
    run >= 3 && trimmed[run * mark.len_utf8()..].trim().is_empty()
}

fn parse_block_line(line: &str) -> DocLine {
    let trimmed = line.trim_end();
    if trimmed.trim().is_empty() {
        return DocLine::new(Kind::Body, Marker::None, Vec::new());
    }
    if is_rule(trimmed) {
        return DocLine::new(Kind::Rule, Marker::None, Vec::new());
    }
    if let Some(depth) = quote_depth(trimmed) {
        let rest = strip_quote(trimmed);
        return DocLine::new(Kind::Quote, Marker::Quote { depth }, inline(&rest));
    }
    if let Some((level, rest)) = heading(trimmed) {
        return DocLine::new(Kind::Heading(level), Marker::Hash(level), inline(rest));
    }
    if let Some((marker, rest)) = bullet(trimmed) {
        return DocLine::new(Kind::Bullet, marker, inline(&rest));
    }
    DocLine::new(Kind::Body, Marker::None, inline(trimmed))
}

/// `---`, `***`, `___`: three or more of one mark and nothing else.
fn is_rule(line: &str) -> bool {
    let body: String = line.trim().chars().filter(|c| !c.is_whitespace()).collect();
    body.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|mark| body.chars().all(|c| c == *mark))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    Some((hashes as u8, rest.trim_start()))
}

fn quote_depth(line: &str) -> Option<usize> {
    if !line.trim_start().starts_with('>') {
        return None;
    }
    let mut depth = 0;
    for c in line.chars() {
        match c {
            '>' => depth += 1,
            ' ' => {}
            _ => break,
        }
    }
    Some(depth.clamp(1, MAX_QUOTE_DEPTH))
}

fn strip_quote(line: &str) -> String {
    line.trim_start().trim_start_matches(['>', ' ']).to_string()
}

/// `- item`, `* item`, `+ item`, `1. item`, `2) item` - with the indent
/// that puts nested items under their parent.
fn bullet(line: &str) -> Option<(Marker, String)> {
    let indent_cols = line.len() - line.trim_start().len();
    let depth = (indent_cols / 2).min(MAX_LIST_DEPTH);
    let body = line.trim_start();
    let mut chars = body.chars();
    let first = chars.next()?;
    if matches!(first, '-' | '*' | '+') {
        let rest = &body[first.len_utf8()..];
        if !rest.starts_with(' ') {
            return None;
        }
        return Some((Marker::Bullet { depth }, rest.trim_start().to_string()));
    }
    let digits: String = body.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let rest = &body[digits.len()..];
    let mut rest_chars = rest.chars();
    let delimiter = rest_chars.next()?;
    if !matches!(delimiter, '.' | ')') {
        return None;
    }
    let tail = &rest[delimiter.len_utf8()..];
    if !tail.starts_with(' ') {
        return None;
    }
    Some((
        Marker::Ordered {
            depth,
            label: format!("{digits}{delimiter}"),
        },
        tail.trim_start().to_string(),
    ))
}

/// Inline runs: `` `code` `` keeps its backticks (a dual that survives
/// `NO_COLOR`), while `**strong**` and `*emph*` drop their marks because
/// bold and underline are themselves color-free.
pub fn inline(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                push_plain(&mut spans, &mut plain);
                let code: String = chars[i..=end].iter().collect();
                spans.push(Span::new(code, Ink::Code));
                i = end + 1;
                continue;
            }
        }
        if matches!(c, '*' | '_') {
            let run = chars[i..].iter().take_while(|d| **d == c).count().min(2);
            let mark: String = std::iter::repeat_n(c, run).collect();
            if let Some(end) = closing(&chars, i + run, &mark) {
                let body: String = chars[i + run..end].iter().collect();
                if !body.trim().is_empty() {
                    push_plain(&mut spans, &mut plain);
                    let ink = if run == 2 { Ink::Strong } else { Ink::Emph };
                    spans.extend(inline(&body).into_iter().map(|s| {
                        if s.ink == Ink::Plain {
                            Span::new(s.text, ink)
                        } else {
                            s
                        }
                    }));
                    i = end + run;
                    continue;
                }
            }
        }
        plain.push(c);
        i += 1;
    }
    push_plain(&mut spans, &mut plain);
    spans
}

fn push_plain(spans: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::new(std::mem::take(plain), Ink::Plain));
    }
}

/// Index of the closing `mark` at or after `from`, on the same line.
fn closing(chars: &[char], from: usize, mark: &str) -> Option<usize> {
    let mark: Vec<char> = mark.chars().collect();
    let mut i = from;
    while i + mark.len() <= chars.len() {
        if chars[i..i + mark.len()] == mark[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Lay a document out in `width` columns: the rows the reader scrolls.
///
/// Wrapping breaks at a space when there is one and at a character
/// boundary otherwise, which is what makes CJK - no spaces, two columns
/// per character - wrap without jitter instead of overflowing the frame.
pub fn layout(doc: &[DocLine], width: usize, glyphs: &Glyphs) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::with_capacity(doc.len());
    for (index, line) in doc.iter().enumerate() {
        match line.kind {
            Kind::Rule => rows.push(Row {
                line: index,
                kind: line.kind,
                spans: vec![Span::new(fill(glyphs.rule, width), Ink::Meta)],
            }),
            Kind::Fence => rows.push(Row {
                line: index,
                kind: line.kind,
                spans: vec![Span::new(fence_rule(line, width, glyphs), Ink::Meta)],
            }),
            _ => {
                let marker = line.marker.render(glyphs);
                let indent = display_width(&marker);
                let body_width = width.saturating_sub(indent).max(1);
                for (n, mut spans) in wrap(&line.spans, body_width).into_iter().enumerate() {
                    let lead = if n == 0 {
                        Span::new(marker.clone(), Ink::Marker)
                    } else {
                        Span::new(" ".repeat(indent), Ink::Plain)
                    };
                    if !lead.text.is_empty() {
                        spans.insert(0, lead);
                    }
                    rows.push(Row {
                        line: index,
                        kind: line.kind,
                        spans,
                    });
                }
            }
        }
    }
    rows
}

/// Repeat `glyph` until it fills exactly `width` columns.
fn fill(glyph: &str, width: usize) -> String {
    let step = display_width(glyph).max(1);
    let mut out = String::new();
    let mut used = 0;
    while used + step <= width {
        out.push_str(glyph);
        used += step;
    }
    out
}

/// A fence line as a labelled rule: `── rust ──────`.
fn fence_rule(line: &DocLine, width: usize, glyphs: &Glyphs) -> String {
    let label: String = line.spans.iter().map(|s| s.text.as_str()).collect();
    if label.is_empty() {
        return fill(glyphs.rule, width);
    }
    let head = fill(glyphs.rule, 2.min(width));
    let text = format!("{head} {label} ");
    let used = display_width(&text);
    if used >= width {
        // Columns, not characters: a wide label must not be cut mid-cell
        // and must never draw past the frame.
        return pad_to_width_with(&text, width, "");
    }
    format!("{text}{}", fill(glyphs.rule, width - used))
}

/// Greedy wrap of styled runs into rows of at most `width` columns.
/// Always yields at least one row, so a blank source line stays a line.
///
/// A space that lands on the boundary dissolves there rather than
/// pushing the word after it onto its own row, which is what keeps a
/// paragraph flush instead of ragged one word early.
fn wrap(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let cells: Vec<(char, Ink)> = spans
        .iter()
        .flat_map(|span| span.text.chars().map(|c| (c, span.ink)))
        .collect();
    let mut rows: Vec<Vec<(char, Ink)>> = Vec::new();
    let mut current: Vec<(char, Ink)> = Vec::new();
    let mut used = 0usize;
    let mut last_break: Option<usize> = None;
    let mut break_before_next = false;
    for (c, ink) in cells {
        let cell_width = char_width(c);
        if c == ' ' {
            // Leading spaces survive on the first row (a text file's own
            // indentation) but never open a continuation row.
            if current.is_empty() && !rows.is_empty() {
                continue;
            }
            if used + cell_width > width {
                break_before_next = true;
                continue;
            }
            if !current.is_empty() {
                last_break = Some(current.len() + 1);
            }
            current.push((c, ink));
            used += cell_width;
            continue;
        }
        if break_before_next {
            rows.push(std::mem::take(&mut current));
            used = 0;
            last_break = None;
            break_before_next = false;
        }
        if used + cell_width > width && !current.is_empty() {
            let cut = match last_break {
                Some(at) if at > 0 && at <= current.len() => at,
                _ => current.len(),
            };
            let rest: Vec<(char, Ink)> = current.split_off(cut);
            while current.last().is_some_and(|(c, _)| *c == ' ') {
                current.pop();
            }
            rows.push(std::mem::take(&mut current));
            current = rest;
            used = current.iter().map(|(c, _)| char_width(*c)).sum();
            last_break = None;
        }
        current.push((c, ink));
        used += cell_width;
    }
    while current.last().is_some_and(|(c, _)| *c == ' ') {
        current.pop();
    }
    rows.push(current);
    rows.into_iter().map(assemble).collect()
}

/// Rebuild spans by merging neighbouring cells that share an ink.
fn assemble(cells: Vec<(char, Ink)>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (c, ink) in cells {
        match spans.last_mut() {
            Some(last) if last.ink == ink => last.text.push(c),
            _ => spans.push(Span::new(c.to_string(), ink)),
        }
    }
    spans
}

/// Source-line indices whose text contains `query`, case-insensitively.
pub fn find_matches(doc: &[DocLine], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = fold(query);
    doc.iter()
        .enumerate()
        .filter(|(_, line)| fold(&line.text()).contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// Re-ink the characters of `spans` that belong to a `query` match.
pub fn highlight(spans: &[Span], query: &str) -> Vec<Span> {
    if query.is_empty() {
        return spans.to_vec();
    }
    let text: Vec<char> = spans.iter().flat_map(|s| s.text.chars()).collect();
    let folded = fold(&text.iter().collect::<String>());
    let needle = fold(query);
    let hay: Vec<char> = folded.chars().collect();
    let mark: Vec<char> = needle.chars().collect();
    if mark.is_empty() || hay.len() < mark.len() {
        return spans.to_vec();
    }
    let mut matched = vec![false; text.len()];
    let mut i = 0;
    while i + mark.len() <= hay.len() {
        if hay[i..i + mark.len()] == mark[..] {
            matched[i..i + mark.len()]
                .iter_mut()
                .for_each(|m| *m = true);
            i += mark.len();
        } else {
            i += 1;
        }
    }
    let mut cells: Vec<(char, Ink)> = Vec::with_capacity(text.len());
    let mut at = 0;
    for span in spans {
        for c in span.text.chars() {
            let ink = if matched[at] { Ink::Match } else { span.ink };
            cells.push((c, ink));
            at += 1;
        }
    }
    assemble(cells)
}

/// Case folding that keeps one character per character, so match indices
/// still line up with the text they came from.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(doc: &[DocLine]) -> Vec<Kind> {
        doc.iter().map(|l| l.kind).collect()
    }

    /// The lines as the screen draws them, marker included.
    fn drawn(doc: &[DocLine], glyphs: &Glyphs) -> Vec<String> {
        layout(doc, 60, glyphs).iter().map(Row::text).collect()
    }

    #[test]
    fn markdown_extensions_are_recognized() {
        assert!(is_markdown(Path::new("a.md")));
        assert!(is_markdown(Path::new("a.MD")));
        assert!(is_markdown(Path::new("a.markdown")));
        assert!(!is_markdown(Path::new("a.txt")));
        assert!(!is_markdown(Path::new("md")));
    }

    #[test]
    fn block_kinds_are_classified() {
        let doc = parse_markdown(
            "# Title\n\nbody text\n\n- one\n  - nested\n1. first\n\n> quoted\n\n---\n",
        );
        assert_eq!(
            kinds(&doc),
            vec![
                Kind::Heading(1),
                Kind::Body,
                Kind::Body,
                Kind::Body,
                Kind::Bullet,
                Kind::Bullet,
                Kind::Bullet,
                Kind::Body,
                Kind::Quote,
                Kind::Body,
                Kind::Rule,
            ]
        );
        let text = drawn(&doc, &Glyphs::UNICODE);
        assert_eq!(text[0], "# Title");
        assert_eq!(text[4], "• one");
        assert_eq!(text[5], "  • nested");
        assert_eq!(text[6], "1. first");
        assert_eq!(text[8], "│ quoted");
        // The same document, drawn in ASCII, keeps every marker legible.
        let ascii = drawn(&doc, &Glyphs::ASCII);
        assert_eq!(ascii[0], "# Title");
        assert_eq!(ascii[4], "* one");
        assert_eq!(ascii[8], "| quoted");
    }

    #[test]
    fn headings_need_a_space_and_stop_at_six() {
        assert_eq!(kinds(&parse_markdown("#no-space")), vec![Kind::Body]);
        assert_eq!(kinds(&parse_markdown("####### seven")), vec![Kind::Body]);
        assert_eq!(kinds(&parse_markdown("###### six")), vec![Kind::Heading(6)]);
    }

    #[test]
    fn fenced_code_is_never_reinterpreted() {
        let doc = parse_markdown("```rust\n# not a heading\n- not a bullet\n```\nafter\n");
        assert_eq!(
            kinds(&doc),
            vec![Kind::Fence, Kind::Code, Kind::Code, Kind::Fence, Kind::Body]
        );
        assert_eq!(drawn(&doc, &Glyphs::UNICODE)[1], "  # not a heading");
    }

    #[test]
    fn fenced_code_closes_only_with_a_bare_matching_fence() {
        let doc = parse_markdown("```rust\n```not-a-close\n~~~\n```   \nafter\n");
        assert_eq!(
            kinds(&doc),
            vec![Kind::Fence, Kind::Code, Kind::Code, Kind::Fence, Kind::Body]
        );
    }

    #[test]
    fn an_unterminated_fence_still_shows_the_rest_of_the_file() {
        let doc = parse_markdown("```\nstill here\n");
        assert_eq!(kinds(&doc), vec![Kind::Fence, Kind::Code]);
        assert!(drawn(&doc, &Glyphs::UNICODE)[1].contains("still here"));
    }

    #[test]
    fn inline_code_keeps_its_backticks_and_emphasis_drops_its_marks() {
        let spans = inline("run `cargo test` for **all** the *cases*");
        let rendered: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "run `cargo test` for all the cases");
        assert!(spans
            .iter()
            .any(|s| s.ink == Ink::Code && s.text == "`cargo test`"));
        assert!(spans
            .iter()
            .any(|s| s.ink == Ink::Strong && s.text == "all"));
        assert!(spans
            .iter()
            .any(|s| s.ink == Ink::Emph && s.text == "cases"));
    }

    #[test]
    fn unpaired_marks_stay_literal() {
        let rendered: String = inline("2 * 3 and `unclosed")
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(rendered, "2 * 3 and `unclosed");
    }

    #[test]
    fn ascii_mode_uses_only_printable_ascii_markers() {
        let doc = parse_markdown("- item\n> quote\n---\n```\ncode\n```\n");
        let rows = layout(&doc, 40, &Glyphs::ASCII);
        for row in &rows {
            for c in row.text().chars() {
                assert!(
                    (' '..='~').contains(&c),
                    "non-ascii {c:?} in {:?}",
                    row.text()
                );
            }
        }
        assert!(rows[0].text().starts_with("* item"));
        assert!(rows[1].text().starts_with("| quote"));
        assert_eq!(rows[2].text(), "-".repeat(40));
    }

    #[test]
    fn tabs_expand_and_control_characters_are_neutralized() {
        let doc = parse_plain("a\tb\x1b[31mred\x07\n");
        let text = doc[0].text();
        let text = &text;
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\t'));
        assert!(text.starts_with("a   b"));
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_hangs_under_the_marker() {
        let doc = parse_markdown("- alpha beta gamma delta");
        let rows = layout(&doc, 12, &Glyphs::UNICODE);
        let text: Vec<String> = rows.iter().map(Row::text).collect();
        assert_eq!(text, vec!["• alpha beta", "  gamma", "  delta"]);
        assert!(rows.iter().all(|r| r.line == 0));
    }

    #[test]
    fn wide_characters_never_split_or_overflow_the_width() {
        // No spaces at all: the break has to land on a character edge.
        let doc = parse_plain("檔案總管視窗介面設計");
        for width in 3..=21 {
            let rows = layout(&doc, width, &Glyphs::UNICODE);
            for row in &rows {
                let drawn = row.text();
                assert!(
                    display_width(&drawn) <= width,
                    "width {width} overflowed with {drawn:?}"
                );
                assert!(drawn.chars().all(|c| c != '\u{FFFD}'));
            }
            let joined: String = rows.iter().map(Row::text).collect();
            assert_eq!(
                joined, "檔案總管視窗介面設計",
                "width {width} lost characters"
            );
        }
    }

    #[test]
    fn a_word_longer_than_the_width_is_cut_not_dropped() {
        let doc = parse_plain("supercalifragilistic");
        let rows = layout(&doc, 8, &Glyphs::UNICODE);
        let joined: String = rows.iter().map(Row::text).collect();
        assert_eq!(joined, "supercalifragilistic");
        assert!(rows.iter().all(|r| display_width(&r.text()) <= 8));
    }

    #[test]
    fn every_source_line_keeps_at_least_one_row() {
        let doc = parse_plain("one\n\n\nfour");
        let rows = layout(&doc, 20, &Glyphs::UNICODE);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter().map(|r| r.line).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn rules_and_fences_fill_the_width() {
        let doc = parse_markdown("---\n```rust\ncode\n```\n");
        let rows = layout(&doc, 20, &Glyphs::UNICODE);
        assert_eq!(display_width(&rows[0].text()), 20);
        assert!(rows[1].text().contains("rust"));
        assert_eq!(display_width(&rows[1].text()), 20);
        assert_eq!(display_width(&rows[3].text()), 20);
    }

    #[test]
    fn a_wide_fence_label_is_cut_by_columns_not_by_characters() {
        // 40 CJK characters is 80 columns of label, far past any frame
        // the reader has: the cut has to land on a cell boundary.
        let doc = parse_markdown(&format!("```{}\ncode\n```\n", "檔".repeat(40)));
        for width in 1..=30 {
            let rows = layout(&doc, width, &Glyphs::UNICODE);
            let rule = rows[0].text();
            assert_eq!(
                display_width(&rule),
                width,
                "width {width} drew {rule:?} instead of filling the frame"
            );
            assert!(rule.chars().all(|c| c != '\u{FFFD}'));
        }
    }

    #[test]
    fn app_authored_lines_are_cleaned_like_parsed_ones() {
        // `DocLine::body` and `DocLine::meta` hold the invariant, so a
        // pane the app writes itself can never budget a width it will not
        // spend.
        assert_eq!(DocLine::body("a\tb\x1b[31m").text(), "a   b\u{FFFD}[31m");
        assert_eq!(DocLine::meta("note\x07").text(), "note\u{FFFD}");
    }

    #[test]
    fn search_is_case_insensitive_and_by_source_line() {
        let doc = parse_plain("Alpha\nbeta\nALPHABET\n");
        assert_eq!(find_matches(&doc, "alpha"), vec![0, 2]);
        assert_eq!(find_matches(&doc, "BETA"), vec![1]);
        assert!(find_matches(&doc, "gamma").is_empty());
        assert!(find_matches(&doc, "").is_empty());
    }

    #[test]
    fn highlight_marks_only_the_matched_characters() {
        let spans = vec![Span::new("find the FIND here", Ink::Plain)];
        let marked = highlight(&spans, "find");
        let matched: String = marked
            .iter()
            .filter(|s| s.ink == Ink::Match)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(matched, "findFIND");
        let rendered: String = marked.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "find the FIND here");
    }

    #[test]
    fn highlight_survives_a_match_that_spans_two_inks() {
        let spans = vec![Span::new("al", Ink::Plain), Span::new("pha", Ink::Strong)];
        let marked = highlight(&spans, "alpha");
        assert!(marked.iter().all(|s| s.ink == Ink::Match));
        assert_eq!(
            marked.iter().map(|s| s.text.as_str()).collect::<String>(),
            "alpha"
        );
    }
}
