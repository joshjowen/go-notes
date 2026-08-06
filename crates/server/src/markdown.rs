//! Reading structure out of a markdown file: frontmatter, plain text for
//! search, `[[wikilinks]]`, `![[embeds]]`, inline links and `#tags`.
//!
//! Parsing happens in two passes for a specific reason. Pass one runs
//! pulldown-cmark to flatten the document to plain text and to pick up ordinary
//! `[text](target)` links. Pass two is a hand-written scanner for the syntax
//! CommonMark does not know about — wikilinks and tags.
//!
//! The second pass cannot simply run over the raw source, because a `[[link]]`
//! inside a code block is documentation, not a link, and indexing it would put
//! phantom edges in the graph. So pass one also records the byte ranges of every
//! code span, fenced block and raw HTML chunk, and pass two skips them. That
//! keeps CommonMark's notion of "what is code" as the single definition, rather
//! than re-deriving it with a second, subtly different, fence parser.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// `[[Note]]`
    Wikilink,
    /// `![[Note]]` or `![alt](image.png)`
    Embed,
    /// `[text](Note.md)`
    Markdown,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Wikilink => "wikilink",
            LinkKind::Embed => "embed",
            LinkKind::Markdown => "markdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// Target exactly as written, minus any `relation::`, `#anchor` or `|alias`.
    pub target_raw: String,
    /// The author's word for the relationship, from `[[relation::Note]]`.
    ///
    /// Only wikilinks can carry one: an inline `[text](note.md)` has nowhere to
    /// put it that would not also change what the link says when read as plain
    /// markdown by something else.
    pub relation: Option<String>,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    pub kind: LinkKind,
    /// A sentence or so around the link, for the backlinks pane.
    pub context: String,
    /// Byte range of the whole link in the source file, used when rewriting.
    pub span: Range<usize>,
}

impl ParsedLink {
    /// Normalised key used to match this link against a note.
    pub fn target_key(&self) -> String {
        normalize_target_key(&self.target_raw)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNote {
    pub title: String,
    pub stem: String,
    pub frontmatter: Value,
    pub body_text: String,
    pub links: Vec<ParsedLink>,
    pub tags: Vec<String>,
}

/// Splits a leading YAML frontmatter block off a document.
///
/// Returns `(yaml, body, body_offset)`. The offset lets link spans found in the
/// body be translated back into positions in the original file.
pub fn split_frontmatter(content: &str) -> (Option<&str>, &str, usize) {
    // The opening fence must be the very first thing in the file.
    let rest = match content.strip_prefix("---\n") {
        Some(rest) => rest,
        None => match content.strip_prefix("---\r\n") {
            Some(rest) => rest,
            None => return (None, content, 0),
        },
    };
    let open_len = content.len() - rest.len();

    // Find a closing fence that sits on its own line.
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            let yaml = &rest[..offset];
            let body_start = open_len + offset + line.len();
            return (Some(yaml), &content[body_start..], body_start);
        }
        offset += line.len();
    }

    // An unterminated fence is not frontmatter; treat the whole file as body so
    // the user still sees their text rather than losing it to a parse error.
    (None, content, 0)
}

fn parse_frontmatter(yaml: &str) -> Value {
    if yaml.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_yaml_ng::from_str::<Value>(yaml) {
        Ok(Value::Object(map)) => Value::Object(map),
        // Frontmatter that is valid YAML but not a mapping (a bare list, say)
        // is not something we can read keys out of; keep the file usable anyway.
        Ok(_) | Err(_) => Value::Object(Default::default()),
    }
}

/// Regions of the source that pass two must not scan.
struct CodeMask {
    ranges: Vec<Range<usize>>,
}

impl CodeMask {
    fn contains(&self, offset: usize) -> bool {
        self.ranges
            .iter()
            .any(|range| offset >= range.start && offset < range.end)
    }
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options
}

/// Pass one: plain text, ordinary links, and the code mask.
fn walk_commonmark(body: &str, body_offset: usize) -> (String, Vec<ParsedLink>, CodeMask) {
    let mut text = String::with_capacity(body.len());
    let mut links = Vec::new();
    let mut masked = Vec::new();

    // Depth counter rather than a bool: a link can nest inside emphasis inside
    // a heading, and we only care that we are somewhere inside a link.
    let mut in_code_block = false;

    let parser = Parser::new_ext(body, markdown_options()).into_offset_iter();
    for (event, range) in parser {
        match event {
            Event::Text(chunk) => {
                if in_code_block {
                    masked.push(shift(&range, body_offset));
                } else {
                    text.push_str(&chunk);
                }
            }
            Event::Code(chunk) => {
                // Inline code contributes to search text but must not be
                // scanned for links.
                text.push_str(&chunk);
                masked.push(shift(&range, body_offset));
            }
            Event::Html(_) | Event::InlineHtml(_) => {
                masked.push(shift(&range, body_offset));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind {
                    // The info string is not prose; keep it out of search text.
                    let _ = lang;
                }
                masked.push(shift(&range, body_offset));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                if let Some(link) =
                    internal_markdown_link(&dest_url, LinkKind::Markdown, shift(&range, body_offset), body)
                {
                    links.push(link);
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                if let Some(link) =
                    internal_markdown_link(&dest_url, LinkKind::Embed, shift(&range, body_offset), body)
                {
                    links.push(link);
                }
            }
            Event::SoftBreak | Event::HardBreak | Event::Rule => text.push('\n'),
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Heading(_))
            | Event::End(TagEnd::Item)
            | Event::End(TagEnd::TableCell) => text.push('\n'),
            _ => {}
        }
    }

    (text, links, CodeMask { ranges: masked })
}

fn shift(range: &Range<usize>, offset: usize) -> Range<usize> {
    (range.start + offset)..(range.end + offset)
}

/// Keeps only links that point somewhere inside the vault.
///
/// External URLs are still perfectly good links for the reader, they just do not
/// belong in the graph, and storing them would mean every note that cites a web
/// page grows a permanently-broken edge.
fn internal_markdown_link(
    dest: &str,
    kind: LinkKind,
    span: Range<usize>,
    body: &str,
) -> Option<ParsedLink> {
    if dest.is_empty() || is_external(dest) {
        return None;
    }
    let decoded = percent_decode(dest);
    let (target, anchor) = split_anchor(&decoded);
    if target.is_empty() {
        // A bare `#anchor` is a jump within the same note, not a link out of it.
        return None;
    }
    Some(ParsedLink {
        target_raw: target.to_string(),
        // An inline markdown link has nowhere to put a relation that would not
        // also change what it says to a reader who is not using this editor.
        relation: None,
        anchor: anchor.map(str::to_string),
        alias: None,
        kind,
        context: context_around(body, span.start.saturating_sub(0)),
        span,
    })
}

fn is_external(dest: &str) -> bool {
    // A scheme, a protocol-relative URL, or an absolute path — none of which
    // name a note in this vault.
    dest.starts_with("//")
        || dest.starts_with('/')
        || dest
            .split_once(':')
            .is_some_and(|(scheme, _)| {
                !scheme.is_empty()
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            })
}

fn percent_decode(input: &str) -> String {
    percent_encoding::percent_decode_str(input)
        .decode_utf8()
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| input.to_string())
}

fn split_anchor(target: &str) -> (&str, Option<&str>) {
    match target.split_once('#') {
        Some((before, after)) => (before, Some(after)),
        None => (target, None),
    }
}

/// Pass two: wikilinks, embeds and tags, skipping anything the mask covers.
fn scan_wiki_syntax(
    source: &str,
    body_range: Range<usize>,
    mask: &CodeMask,
) -> (Vec<ParsedLink>, Vec<String>) {
    let mut links = Vec::new();
    let mut tags = Vec::new();
    let bytes = source.as_bytes();

    let mut i = body_range.start;
    while i < body_range.end {
        if mask.contains(i) {
            i += 1;
            continue;
        }

        if bytes[i] == b'[' && bytes.get(i + 1) == Some(&b'[') {
            // `![[...]]` is an embed; the `!` sits one byte before the `[[`.
            let is_embed = i > body_range.start && bytes[i - 1] == b'!';
            let start = if is_embed { i - 1 } else { i };

            if let Some(close) = find_wiki_close(bytes, i + 2, body_range.end) {
                let inner = &source[i + 2..close];
                if let Some(link) = parse_wikilink(
                    inner,
                    if is_embed {
                        LinkKind::Embed
                    } else {
                        LinkKind::Wikilink
                    },
                    start..close + 2,
                    source,
                ) {
                    links.push(link);
                }
                i = close + 2;
                continue;
            }
        }

        if bytes[i] == b'#' && is_tag_start(source, i) {
            if let Some((tag, end)) = read_tag(source, i + 1) {
                tags.push(tag);
                i = end;
                continue;
            }
        }

        i += 1;
    }

    tags.sort();
    tags.dedup();
    (links, tags)
}

/// Finds the `]]` closing a wikilink, refusing to run past the end of a line.
///
/// Bounding to a single line matters: without it, a stray `[[` swallows the rest
/// of the document looking for a close that may be paragraphs away, and a note
/// full of `[[` typos turns into one enormous bogus link.
fn find_wiki_close(bytes: &[u8], from: usize, limit: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < limit {
        match bytes[i] {
            b'\n' => return None,
            b']' if bytes[i + 1] == b']' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_wikilink(
    inner: &str,
    kind: LinkKind,
    span: Range<usize>,
    source: &str,
) -> Option<ParsedLink> {
    // `[[relation::target#anchor|alias]]`, unpicked in that order for a reason
    // each time. The alias runs to the end of the link, so it comes off first.
    // The anchor is next because a relation may not contain `#`, so by the time
    // the relation is split the `#` is already gone and cannot be mistaken for
    // part of a label.
    let (target_and_anchor, alias) = match inner.split_once('|') {
        Some((before, after)) => (before, Some(after.trim())),
        None => (inner, None),
    };
    let (target_and_relation, anchor) = split_anchor(target_and_anchor);
    let (relation, target) = go_notes_shared::links::split_relation(target_and_relation);
    let target = target.trim();

    if target.is_empty() {
        return None;
    }

    Some(ParsedLink {
        target_raw: target.to_string(),
        relation: relation.map(str::to_string),
        anchor: anchor.map(|a| a.trim().to_string()).filter(|a| !a.is_empty()),
        alias: alias.map(str::to_string).filter(|a| !a.is_empty()),
        kind,
        context: context_around(source, span.start),
        span,
    })
}

/// A `#` only starts a tag at the beginning of a line or after whitespace, and
/// only when something non-space follows. That single rule is what keeps
/// `# Heading` (space after) and `C#` (no preceding space) from being tags.
fn is_tag_start(source: &str, index: usize) -> bool {
    let bytes = source.as_bytes();
    let preceded_ok = index == 0
        || matches!(bytes[index - 1], b' ' | b'\n' | b'\t' | b'\r' | b'(' | b'[');
    if !preceded_ok {
        return false;
    }
    matches!(bytes.get(index + 1), Some(c) if is_tag_char(*c))
}

fn is_tag_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'/') || c >= 0x80
}

fn read_tag(source: &str, from: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut end = from;
    while end < bytes.len() && is_tag_char(bytes[end]) {
        end += 1;
    }
    if end == from {
        return None;
    }

    // Trailing punctuation belongs to the sentence, not the tag: `#done.`
    let mut tag = &source[from..end];
    while tag.ends_with(['-', '_', '/']) {
        tag = &tag[..tag.len() - 1];
    }
    if tag.is_empty() {
        return None;
    }

    // `#1` and `#2026` are issue numbers and years far more often than tags,
    // which is the rule Obsidian applies too.
    if tag.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    Some((tag.to_string(), end))
}

/// Grabs the text surrounding a byte offset, for display in the backlinks pane.
fn context_around(source: &str, offset: usize) -> String {
    const RADIUS: usize = 90;

    let line_start = source[..offset.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = source[offset.min(source.len())..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(source.len());

    let line = source[line_start..line_end].trim();
    if line.chars().count() <= RADIUS * 2 {
        return line.to_string();
    }

    let truncated: String = line.chars().take(RADIUS * 2).collect();
    format!("{truncated}…")
}

/// Normalises a link target into the key used to match it against a note.
///
/// Case is folded, a `.md` extension is dropped, and `./` prefixes are removed,
/// so `[[Kitchen Reno]]`, `[[kitchen reno.md]]` and `[[./Kitchen Reno]]` all
/// resolve to the same note.
pub fn normalize_target_key(target: &str) -> String {
    let mut key = target.trim();
    while let Some(rest) = key.strip_prefix("./") {
        key = rest;
    }
    let key = key.strip_suffix(".md").or_else(|| key.strip_suffix(".MD")).unwrap_or(key);
    key.trim().to_lowercase()
}

/// Extracts tags declared in frontmatter, accepting the several shapes people
/// write them in: a list, a comma-separated string, or a single string.
fn frontmatter_tags(frontmatter: &Value) -> Vec<String> {
    let raw = frontmatter
        .get("tags")
        .or_else(|| frontmatter.get("tag"))
        .or_else(|| frontmatter.get("keywords"));

    let mut out = Vec::new();
    match raw {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    out.push(s.trim().trim_start_matches('#').to_string());
                }
            }
        }
        Some(Value::String(s)) => {
            for part in s.split(&[',', ' '][..]) {
                let part = part.trim().trim_start_matches('#');
                if !part.is_empty() {
                    out.push(part.to_string());
                }
            }
        }
        _ => {}
    }
    out.retain(|t| !t.is_empty());
    out
}

/// Parses a note. `stem` is the filename without its extension, used as the
/// title when frontmatter does not supply one.
pub fn parse(stem: &str, content: &str) -> ParsedNote {
    let (yaml, body, body_offset) = split_frontmatter(content);
    let frontmatter = yaml.map(parse_frontmatter).unwrap_or_else(|| Value::Object(Default::default()));

    let (body_text, mut links, mask) = walk_commonmark(body, body_offset);
    let (wiki_links, mut tags) = scan_wiki_syntax(content, body_offset..content.len(), &mask);
    links.extend(wiki_links);
    // Source order, so `ordinal` in the database means what it says.
    links.sort_by_key(|link| link.span.start);

    for tag in frontmatter_tags(&frontmatter) {
        tags.push(tag);
    }
    tags.sort();
    tags.dedup();

    let title = frontmatter
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(stem)
        .to_string();

    ParsedNote {
        title,
        stem: stem.to_string(),
        frontmatter,
        body_text,
        links,
        tags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(note: &ParsedNote) -> Vec<&str> {
        note.links.iter().map(|l| l.target_raw.as_str()).collect()
    }

    #[test]
    fn splits_frontmatter() {
        let (yaml, body, offset) = split_frontmatter("---\ntitle: Hi\n---\nBody here\n");
        assert_eq!(yaml, Some("title: Hi\n"));
        assert_eq!(body, "Body here\n");
        // "---\n" + "title: Hi\n" + "---\n" == 18 bytes of frontmatter.
        assert_eq!(offset, 18);
    }

    #[test]
    fn handles_documents_without_frontmatter() {
        let (yaml, body, offset) = split_frontmatter("Just a note\n");
        assert_eq!(yaml, None);
        assert_eq!(body, "Just a note\n");
        assert_eq!(offset, 0);
    }

    /// A horizontal rule at the top of a file is not an unterminated
    /// frontmatter block, and must not swallow the document.
    #[test]
    fn unterminated_frontmatter_is_treated_as_body() {
        let content = "---\nthis never closes\nmore text\n";
        let (yaml, body, _) = split_frontmatter(content);
        assert_eq!(yaml, None);
        assert_eq!(body, content);
    }

    #[test]
    fn reads_title_and_tags_from_frontmatter() {
        let note = parse(
            "filename",
            "---\ntitle: Real Title\ntags: [alpha, beta]\n---\nbody\n",
        );
        assert_eq!(note.title, "Real Title");
        assert_eq!(note.tags, vec!["alpha", "beta"]);
    }

    #[test]
    fn falls_back_to_the_filename_for_a_title() {
        let note = parse("Kitchen Reno", "no frontmatter here\n");
        assert_eq!(note.title, "Kitchen Reno");
    }

    #[test]
    fn accepts_the_several_shapes_of_frontmatter_tags() {
        let list = parse("n", "---\ntags:\n  - a\n  - b\n---\n");
        assert_eq!(list.tags, vec!["a", "b"]);

        let inline = parse("n", "---\ntags: a, b\n---\n");
        assert_eq!(inline.tags, vec!["a", "b"]);

        let hashed = parse("n", "---\ntags: [\"#a\", \"#b\"]\n---\n");
        assert_eq!(hashed.tags, vec!["a", "b"]);
    }

    #[test]
    fn malformed_frontmatter_does_not_lose_the_note() {
        let note = parse("n", "---\n  : : not yaml : :\n---\nThe body survives.\n");
        assert!(note.body_text.contains("The body survives."));
    }

    #[test]
    fn extracts_wikilinks_with_anchors_and_aliases() {
        let note = parse(
            "n",
            "See [[Kitchen Reno]] and [[Budget#Q3|the numbers]] and ![[diagram.png]].\n",
        );
        assert_eq!(targets(&note), vec!["Kitchen Reno", "Budget", "diagram.png"]);

        assert_eq!(note.links[0].kind, LinkKind::Wikilink);
        assert_eq!(note.links[1].anchor.as_deref(), Some("Q3"));
        assert_eq!(note.links[1].alias.as_deref(), Some("the numbers"));
        assert_eq!(note.links[2].kind, LinkKind::Embed);
    }

    /// The reason for the two-pass design: a link in a code sample is not a link.
    #[test]
    fn ignores_links_and_tags_inside_code() {
        let note = parse(
            "n",
            concat!(
                "Real [[Alpha]] and #realtag.\n\n",
                "Inline `[[Beta]]` and `#faketag` are examples.\n\n",
                "```\n[[Gamma]]\n#alsofake\n```\n\n",
                "    [[Delta]]\n",
            ),
        );
        assert_eq!(targets(&note), vec!["Alpha"]);
        assert_eq!(note.tags, vec!["realtag"]);
    }

    #[test]
    fn distinguishes_tags_from_headings_and_other_hashes() {
        let note = parse(
            "n",
            concat!(
                "# A Heading\n\n",
                "Tagged #project/alpha and #done here.\n",
                "Not a tag: C# or issue #123 or file.md#anchor\n",
                "Trailing punctuation #wrapped. stays clean.\n",
            ),
        );
        let mut tags = note.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["done", "project/alpha", "wrapped"]);
    }

    #[test]
    fn keeps_internal_markdown_links_and_drops_external_ones() {
        let note = parse(
            "n",
            concat!(
                "[internal](Other%20Note.md)\n",
                "[external](https://example.com)\n",
                "[protocol relative](//example.com)\n",
                "[mail](mailto:a@b.c)\n",
                "[absolute](/etc/passwd)\n",
                "[same doc](#section)\n",
            ),
        );
        assert_eq!(targets(&note), vec!["Other Note.md"]);
    }

    /// Percent-encoded targets are how most editors write a link to a file with
    /// a space in its name; they must resolve to the same note as the plain form.
    #[test]
    fn decodes_percent_encoded_targets() {
        let note = parse("n", "[a](Kitchen%20Reno.md)\n");
        assert_eq!(note.links[0].target_raw, "Kitchen Reno.md");
        assert_eq!(note.links[0].target_key(), "kitchen reno");
    }

    #[test]
    fn normalizes_target_keys() {
        for (input, expected) in [
            ("Kitchen Reno", "kitchen reno"),
            ("kitchen reno.md", "kitchen reno"),
            ("./Kitchen Reno", "kitchen reno"),
            ("././Kitchen Reno.md", "kitchen reno"),
            ("Projects/Kitchen Reno", "projects/kitchen reno"),
            ("  Spaced  ", "spaced"),
        ] {
            assert_eq!(normalize_target_key(input), expected, "for {input:?}");
        }
    }

    /// An unclosed `[[` must not consume the rest of the file.
    #[test]
    fn unterminated_wikilink_does_not_run_away() {
        let note = parse("n", "Broken [[ start\nNext line has [[Real]].\n");
        assert_eq!(targets(&note), vec!["Real"]);
    }

    #[test]
    fn empty_wikilinks_are_ignored() {
        let note = parse("n", "[[]] and [[  ]] and [[|alias]]\n");
        assert!(note.links.is_empty());
    }

    #[test]
    fn links_come_back_in_source_order() {
        let note = parse("n", "[[C]] then [b](b.md) then [[A]]\n");
        assert_eq!(targets(&note), vec!["C", "b.md", "A"]);
    }

    #[test]
    fn spans_point_at_the_whole_link() {
        let content = "x [[Alpha]] y ![[Beta]] z\n";
        let note = parse("n", content);
        assert_eq!(&content[note.links[0].span.clone()], "[[Alpha]]");
        assert_eq!(&content[note.links[1].span.clone()], "![[Beta]]");
    }

    /// Spans are file offsets, not body offsets — link rewriting depends on this.
    #[test]
    fn spans_account_for_frontmatter() {
        let content = "---\ntitle: T\n---\nBody with [[Alpha]].\n";
        let note = parse("n", content);
        assert_eq!(&content[note.links[0].span.clone()], "[[Alpha]]");
    }

    #[test]
    fn body_text_is_plain_prose_for_search() {
        let note = parse(
            "n",
            "# Heading\n\nSome **bold** and *italic* text with a [link](x.md).\n",
        );
        assert!(note.body_text.contains("Heading"));
        assert!(note.body_text.contains("Some bold and italic text"));
        assert!(!note.body_text.contains('*'));
        assert!(!note.body_text.contains("x.md"));
    }

    #[test]
    fn backlink_context_is_the_surrounding_line() {
        let note = parse("n", "First line.\nThe budget is in [[Budget]] as agreed.\n");
        assert_eq!(
            note.links[0].context,
            "The budget is in [[Budget]] as agreed."
        );
    }

    #[test]
    fn handles_multibyte_content_without_panicking() {
        let note = parse("n", "日本語の [[ノート]] と #タグ と émoji 🎉\n");
        assert_eq!(targets(&note), vec!["ノート"]);
        assert!(note.tags.contains(&"タグ".to_string()));
    }

    #[test]
    fn an_empty_file_parses() {
        let note = parse("Empty", "");
        assert_eq!(note.title, "Empty");
        assert!(note.links.is_empty());
        assert!(note.tags.is_empty());
        assert_eq!(note.body_text, "");
    }

    // ---------------------------------------------------------------- typed links

    #[test]
    fn a_typed_wikilink_keeps_its_relation_and_loses_it_from_the_target() {
        let note = parse("n", "This [[contradicts::Kitchen Reno]] entirely.\n");
        assert_eq!(targets(&note), vec!["Kitchen Reno"]);
        assert_eq!(note.links[0].relation.as_deref(), Some("contradicts"));
        // The relation must not leak into the key, or the link resolves to a
        // note nobody has: `contradicts::kitchen reno`.
        assert_eq!(note.links[0].target_key(), "kitchen reno");
    }

    #[test]
    fn an_ordinary_wikilink_has_no_relation() {
        let note = parse("n", "See [[Kitchen Reno]].\n");
        assert_eq!(note.links[0].relation, None);
    }

    /// The order the three separators come off in, exercised all at once —
    /// getting it wrong puts the anchor inside the relation or the relation
    /// inside the alias, and both read as a working link right up until it
    /// resolves to nothing.
    #[test]
    fn a_relation_survives_alongside_an_anchor_and_an_alias() {
        let note = parse("n", "[[supersedes::Projects/Budget#Q3|last year's]] plan\n");
        let link = &note.links[0];
        assert_eq!(link.target_raw, "Projects/Budget");
        assert_eq!(link.relation.as_deref(), Some("supersedes"));
        assert_eq!(link.anchor.as_deref(), Some("Q3"));
        assert_eq!(link.alias.as_deref(), Some("last year's"));
    }

    #[test]
    fn an_embed_can_be_typed_too() {
        let note = parse("n", "![[illustrates::Diagram]]\n");
        assert_eq!(note.links[0].kind, LinkKind::Embed);
        assert_eq!(note.links[0].relation.as_deref(), Some("illustrates"));
    }

    /// The guard, seen from the parser rather than from `shared::links`: a note
    /// that already contained a colon pair in a target must mean what it meant
    /// before typed links existed.
    #[test]
    fn a_target_that_merely_contains_colons_is_left_alone() {
        let note = parse("n", "See [[C++::Notes]].\n");
        assert_eq!(targets(&note), vec!["C++::Notes"]);
        assert_eq!(note.links[0].relation, None);
    }

    #[test]
    fn a_typed_link_inside_code_is_still_not_a_link() {
        let note = parse("n", "Write `[[relates::Note]]` or:\n\n```\n[[cites::Other]]\n```\n");
        assert!(note.links.is_empty());
    }

    /// An inline markdown link has nowhere to put a relation, and does not get
    /// one by accident either: `relation::` in that position reads as a URI
    /// scheme, so `is_external` has already dropped the link before any of this
    /// runs. Both halves are asserted because the second is what makes the
    /// first safe.
    #[test]
    fn inline_markdown_links_never_carry_a_relation() {
        let note = parse("n", "[text](Other.md)\n");
        assert_eq!(note.links[0].relation, None);
        assert_eq!(note.links[0].target_raw, "Other.md");

        let scheme_shaped = parse("n", "[text](cites::Other.md)\n");
        assert!(scheme_shaped.links.is_empty());
    }
}
