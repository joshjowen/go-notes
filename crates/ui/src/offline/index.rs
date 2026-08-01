//! A small index over the notes cached on this device.
//!
//! With the server unreachable there is no Postgres to ask, so search, the quick
//! switcher, tags and backlinks are answered from what the browser already
//! holds. This is deliberately an approximation of the real index and says so in
//! the interface: it matches ASCII-case-insensitively rather than by stemmed
//! full text, it has no trigram fallback, and it can only see notes that have
//! been opened on this device.
//!
//! What it must not do is be *wrong* in a way that misleads. A `[[link]]` inside
//! a code fence is documentation rather than a link, so the scanner skips code
//! for the same reason the server's parser does — a phantom backlink is worse
//! than a missing one.

use go_notes_shared::{
    paths, Backlink, QuickSwitchItem, SearchHit, TagCount, SNIPPET_CLOSE, SNIPPET_OPEN,
};

use super::CachedNote;

/// Longest snippet shown in a search result, in characters.
const SNIPPET_CHARS: usize = 180;
const SNIPPET_LEAD: usize = 60;

/// Full-text-ish search: every term must appear in the title or the body.
pub fn search(notes: &[CachedNote], query: &str) -> Vec<SearchHit> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.trim_matches('"').to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<SearchHit> = notes
        .iter()
        .filter_map(|note| {
            let haystack = format!("{}\n{}", note.title, note.markdown).to_ascii_lowercase();
            if !terms.iter().all(|term| haystack.contains(term)) {
                return None;
            }
            Some(SearchHit {
                path: note.path.clone(),
                title: note.title.clone(),
                snippet: snippet(&note.markdown, &terms),
                rank: rank(note, &terms),
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    hits
}

/// Titles for Ctrl+P and for wikilink autocomplete.
///
/// Ordered the way the server orders them — exact title, then prefix, then
/// shortest — so that going offline does not reshuffle the list under someone
/// who has learned where their notes sit in it.
pub fn quickswitch(notes: &[CachedNote], query: &str) -> Vec<QuickSwitchItem> {
    let query = query.trim();
    let needle = query.to_ascii_lowercase();

    let mut matches: Vec<&CachedNote> = notes
        .iter()
        .filter(|note| {
            needle.is_empty()
                || note.title.to_ascii_lowercase().contains(&needle)
                || note.path.to_ascii_lowercase().contains(&needle)
        })
        .collect();

    matches.sort_by_key(|note| {
        let title = note.title.to_ascii_lowercase();
        (
            title != needle,
            !title.starts_with(&needle),
            title.len(),
            note.title.clone(),
        )
    });

    let mut items: Vec<QuickSwitchItem> = matches
        .into_iter()
        .take(20)
        .map(|note| QuickSwitchItem {
            path: note.path.clone(),
            title: note.title.clone(),
            exists: true,
        })
        .collect();

    // The offer to create, exactly as the server makes it: a wikilink to a note
    // that does not exist yet is a normal thing to write, and following it
    // should still create the note while offline.
    let has_exact = items
        .iter()
        .any(|item| item.title.eq_ignore_ascii_case(query));
    if !query.is_empty() && !has_exact && paths::validate_component(query).is_ok() {
        items.push(QuickSwitchItem {
            path: format!("{query}.md"),
            title: query.to_string(),
            exists: false,
        });
    }

    items
}

/// Tags across the cached notes, most used first.
pub fn tags(notes: &[CachedNote]) -> Vec<TagCount> {
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for note in notes {
        // `tags_in` already de-duplicates, so a note using `#home` three times
        // counts once.
        for tag in tags_in(&note.markdown) {
            *counts.entry(tag).or_insert(0) += 1;
        }
    }

    let mut out: Vec<TagCount> = counts
        .into_iter()
        .map(|(name, count)| TagCount { name, count })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    out
}

pub fn notes_with_tag(notes: &[CachedNote], tag: &str) -> Vec<QuickSwitchItem> {
    let wanted = tag.trim_start_matches('#').to_ascii_lowercase();
    let mut items: Vec<QuickSwitchItem> = notes
        .iter()
        .filter(|note| {
            tags_in(&note.markdown)
                .iter()
                .any(|found| found.to_ascii_lowercase() == wanted)
        })
        .map(|note| QuickSwitchItem {
            path: note.path.clone(),
            title: note.title.clone(),
            exists: true,
        })
        .collect();
    items.sort_by(|a, b| a.title.cmp(&b.title));
    items
}

/// Notes that link to `path`, with the line the link sits on as context.
pub fn backlinks(notes: &[CachedNote], path: &str) -> Vec<Backlink> {
    let stem = paths::stem(path).to_ascii_lowercase();
    let without_extension = path
        .strip_suffix(".md")
        .unwrap_or(path)
        .to_ascii_lowercase();

    let mut found: Vec<Backlink> = Vec::new();
    for note in notes {
        if note.path == path {
            continue;
        }
        for (target, offset) in links_in(&note.markdown) {
            let key = normalize_target(&target);
            if key != stem && key != without_extension {
                continue;
            }
            found.push(Backlink {
                path: note.path.clone(),
                title: note.title.clone(),
                context: context_around(&note.markdown, offset),
            });
            break;
        }
    }
    found.sort_by(|a, b| a.title.cmp(&b.title));
    found
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// `[[wikilink]]` targets with their byte offsets, ignoring anything in code.
///
/// `![[embeds]]` count as links here, as they do on the server: an embed is a
/// reference to the note, and the graph treats it as one.
pub fn links_in(markdown: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let bytes = markdown.as_bytes();

    for (start, end) in prose_ranges(markdown) {
        let mut index = start;
        while index + 1 < end {
            if bytes[index] == b'[' && bytes[index + 1] == b'[' {
                let Some(close) = markdown[index + 2..end].find("]]") else {
                    break;
                };
                let inner = &markdown[index + 2..index + 2 + close];
                // `[[Target#heading|alias]]` — only the target names the note.
                let target = inner
                    .split('|')
                    .next()
                    .unwrap_or("")
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !target.is_empty() {
                    out.push((target.to_string(), index));
                }
                index += close + 4;
                continue;
            }
            index += 1;
        }
    }
    out
}

/// `#tags`, plus a `tags:` list in the frontmatter.
pub fn tags_in(markdown: &str) -> Vec<String> {
    let mut out = frontmatter_tags(markdown);

    for (start, end) in prose_ranges(markdown) {
        let segment = &markdown[start..end];
        // What precedes the segment matters: a `#` is only a tag at the start of
        // a word, so `C#` and a `#fragment` in a URL are not tags — and neither
        // is `# ` at the start of a line, which is a heading.
        let mut previous = markdown[..start].chars().next_back();

        let mut chars = segment.char_indices();
        while let Some((offset, character)) = chars.next() {
            if character != '#' {
                previous = Some(character);
                continue;
            }
            if previous.is_some_and(|before| !before.is_whitespace()) {
                previous = Some(character);
                continue;
            }

            let name: String = segment[offset + 1..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '/'))
                .collect();

            // Consume the name so its characters are not rescanned.
            for _ in name.chars() {
                chars.next();
            }
            previous = name.chars().next_back().or(Some(character));

            if name.is_empty() || name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            out.push(name);
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Byte ranges of the document that are prose rather than code or frontmatter.
///
/// Fenced blocks and inline code spans are excluded, because a `[[link]]` or a
/// `#tag` written inside them is being shown, not used.
fn prose_ranges(markdown: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    let mut in_fence = false;
    let body_start = frontmatter_end(markdown);

    for line in markdown.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if start < body_start {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        // Split the line around inline code spans.
        let mut cursor = 0usize;
        let mut open: Option<usize> = None;
        for (index, byte) in line.bytes().enumerate() {
            if byte != b'`' {
                continue;
            }
            match open {
                None => open = Some(index),
                Some(begin) => {
                    ranges.push((start + cursor, start + begin));
                    cursor = index + 1;
                    open = None;
                }
            }
        }
        // An unterminated backtick leaves the rest of the line as prose, which
        // is what a reader sees too.
        ranges.push((start + cursor, start + line.len()));
    }

    ranges.retain(|(start, end)| start < end);
    ranges
}

/// Byte offset where the body starts, skipping YAML frontmatter.
fn frontmatter_end(markdown: &str) -> usize {
    let Some(rest) = markdown
        .strip_prefix("---\n")
        .or_else(|| markdown.strip_prefix("---\r\n"))
    else {
        return 0;
    };
    let open_len = markdown.len() - rest.len();

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" || trimmed == "..." {
            return open_len + offset + line.len();
        }
        offset += line.len();
    }
    0
}

/// `tags: [a, b]` or a `tags:` block list in the frontmatter.
///
/// Deliberately not a YAML parser: this reads the two spellings people actually
/// write, and ignores anything else rather than guessing.
fn frontmatter_tags(markdown: &str) -> Vec<String> {
    let end = frontmatter_end(markdown);
    if end == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut in_block = false;
    for line in markdown[..end].lines() {
        let trimmed = line.trim_end();

        if in_block {
            if let Some(item) = trimmed.trim_start().strip_prefix("- ") {
                out.push(clean_tag(item));
                continue;
            }
            in_block = false;
        }

        let Some(value) = trimmed
            .strip_prefix("tags:")
            .or_else(|| trimmed.strip_prefix("tag:"))
        else {
            continue;
        };

        let value = value.trim();
        if value.is_empty() {
            in_block = true;
        } else {
            for item in value.trim_matches(['[', ']']).split(',') {
                let tag = clean_tag(item);
                if !tag.is_empty() {
                    out.push(tag);
                }
            }
        }
    }

    out.retain(|tag| !tag.is_empty());
    out
}

fn clean_tag(raw: &str) -> String {
    raw.trim()
        .trim_matches(['"', '\''])
        .trim_start_matches('#')
        .trim()
        .to_string()
}

/// The same normalisation the server applies before matching a link to a note:
/// case-folded, without the extension, and without any folder prefix when the
/// link only names a file.
fn normalize_target(target: &str) -> String {
    target
        .trim()
        .trim_end_matches(".md")
        .trim_matches('/')
        .to_ascii_lowercase()
}

fn rank(note: &CachedNote, terms: &[String]) -> f32 {
    let title = note.title.to_ascii_lowercase();
    let body = note.markdown.to_ascii_lowercase();

    let mut score = 0.0;
    for term in terms {
        if title == *term {
            score += 4.0;
        } else if title.starts_with(term.as_str()) {
            score += 2.0;
        } else if title.contains(term.as_str()) {
            score += 1.0;
        }
        // Occurrences count, with diminishing returns so one long note does not
        // bury a short, precisely-matching one.
        let occurrences = body.matches(term.as_str()).count() as f32;
        score += (1.0 + occurrences).ln();
    }
    score
}

/// An excerpt around the first match, with the matched terms delimited the way
/// the server delimits them so the frontend renders both identically.
fn snippet(markdown: &str, terms: &[String]) -> String {
    let folded = markdown.to_ascii_lowercase();
    let hit = terms
        .iter()
        .filter_map(|term| folded.find(term.as_str()))
        .min()
        .unwrap_or(0);

    let start = floor_boundary(markdown, hit.saturating_sub(SNIPPET_LEAD));
    let end = ceil_boundary(markdown, (start + SNIPPET_CHARS).min(markdown.len()));

    let mut window = markdown[start..end].replace('\n', " ");

    // Mark every term inside the window. Done on the extracted window so the
    // byte offsets stay valid as the markers lengthen it.
    for term in terms {
        window = mark_term(&window, term);
    }

    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(window.trim());
    if end < markdown.len() {
        out.push('…');
    }
    out
}

fn mark_term(haystack: &str, term: &str) -> String {
    let folded = haystack.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;

    while let Some(found) = folded[cursor..].find(term) {
        let at = cursor + found;
        out.push_str(&haystack[cursor..at]);
        out.push(SNIPPET_OPEN);
        out.push_str(&haystack[at..at + term.len()]);
        out.push(SNIPPET_CLOSE);
        cursor = at + term.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// The line a link sits on, for the backlinks pane.
fn context_around(markdown: &str, offset: usize) -> String {
    let start = markdown[..offset.min(markdown.len())]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = markdown[start..]
        .find('\n')
        .map(|index| start + index)
        .unwrap_or(markdown.len());

    let line = markdown[start..end].trim();
    if line.chars().count() <= 160 {
        return line.to_string();
    }
    let cut = ceil_boundary(line, 160);
    format!("{}…", &line[..cut])
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str, markdown: &str) -> CachedNote {
        CachedNote {
            path: path.to_string(),
            title: paths::stem(path).to_string(),
            markdown: markdown.to_string(),
            content_hash: "hash".into(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn search_requires_every_term() {
        let notes = vec![
            note("A.md", "kitchen renovation budget"),
            note("B.md", "kitchen only"),
        ];

        let hits = search(&notes, "kitchen budget");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "A.md");
    }

    #[test]
    fn search_marks_matches_the_way_the_server_does() {
        let notes = vec![note("A.md", "a line about budgets\n")];
        let hits = search(&notes, "budget");
        assert!(hits[0].snippet.contains(SNIPPET_OPEN));
        assert!(hits[0].snippet.contains(SNIPPET_CLOSE));
    }

    #[test]
    fn search_ranks_a_title_match_above_a_body_mention() {
        let notes = vec![
            note("Groceries.md", "milk\n"),
            note("Shopping.md", "groceries are cheap this week\n"),
        ];
        let hits = search(&notes, "groceries");
        assert_eq!(hits[0].path, "Groceries.md");
    }

    #[test]
    fn quickswitch_puts_an_exact_title_first() {
        let notes = vec![
            note("Budget Archive.md", ""),
            note("Budget.md", ""),
            note("Old Budget Notes.md", ""),
        ];
        let items = quickswitch(&notes, "Budget");
        assert_eq!(items[0].title, "Budget");
    }

    /// Following `[[Something New]]` has to keep working with no server, or
    /// linking while offline becomes a dead end.
    #[test]
    fn quickswitch_offers_to_create_what_does_not_exist() {
        let items = quickswitch(&[], "Something New");
        assert_eq!(items.len(), 1);
        assert!(!items[0].exists);
        assert_eq!(items[0].path, "Something New.md");
    }

    #[test]
    fn quickswitch_does_not_offer_to_create_an_illegal_name() {
        let items = quickswitch(&[], "../escape");
        assert!(items.is_empty());
    }

    #[test]
    fn backlinks_match_bare_and_full_link_forms() {
        let notes = vec![
            note("Index.md", "see [[Kitchen Reno]] for details\n"),
            note("Plan.md", "and [[Projects/Kitchen Reno|the reno]] too\n"),
            note("Other.md", "nothing here\n"),
        ];

        let links = backlinks(&notes, "Projects/Kitchen Reno.md");
        let paths: Vec<&str> = links.iter().map(|link| link.path.as_str()).collect();
        assert_eq!(paths, vec!["Index.md", "Plan.md"]);
        assert_eq!(links[0].context, "see [[Kitchen Reno]] for details");
    }

    /// The mistake that would put phantom edges in front of the user: a link
    /// written inside a code block is an example, not a link.
    #[test]
    fn links_inside_code_are_not_links() {
        let markdown = "real [[One]]\n\n```md\n[[Two]]\n```\n\nand `[[Three]]` inline\n";
        let found: Vec<String> = links_in(markdown)
            .into_iter()
            .map(|(target, _)| target)
            .collect();
        assert_eq!(found, vec!["One".to_string()]);
    }

    #[test]
    fn tags_come_from_the_body_and_the_frontmatter() {
        let markdown = "---\ntags: [home, budget]\n---\n\nSome #kitchen notes\n";
        assert_eq!(
            tags_in(markdown),
            vec![
                "budget".to_string(),
                "home".to_string(),
                "kitchen".to_string()
            ]
        );
    }

    #[test]
    fn tags_read_a_block_list_too() {
        let markdown = "---\ntags:\n  - home\n  - budget\n---\n\nbody\n";
        assert_eq!(tags_in(markdown), vec!["budget".to_string(), "home".to_string()]);
    }

    #[test]
    fn a_heading_is_not_a_tag_and_neither_is_a_url_fragment() {
        let markdown = "# Heading\n\nsee https://example.com/page#section and C# too\n";
        assert!(tags_in(markdown).is_empty());
    }

    #[test]
    fn tag_counts_are_ordered_by_use() {
        let notes = vec![
            note("A.md", "#home\n"),
            note("B.md", "#home #budget\n"),
            note("C.md", "#home\n"),
        ];
        let counted = tags(&notes);
        assert_eq!(counted[0].name, "home");
        assert_eq!(counted[0].count, 3);
        assert_eq!(counted[1].name, "budget");
    }

    #[test]
    fn notes_with_tag_ignores_case_and_a_leading_hash() {
        let notes = vec![note("A.md", "#Home\n"), note("B.md", "#away\n")];
        let items = notes_with_tag(&notes, "#home");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "A.md");
    }

    /// Snippets are cut on character boundaries; a multi-byte character across
    /// the cut would otherwise panic.
    #[test]
    fn snippets_survive_multibyte_text() {
        let body = "héllo wörld ".repeat(40);
        let notes = vec![note("A.md", &format!("{body}budget café\n"))];
        let hits = search(&notes, "café");
        assert!(hits[0].snippet.contains('«'));
    }
}
